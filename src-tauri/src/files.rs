use crate::{identity::private_write, model::*};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

pub struct Files {
    serial: Mutex<()>,
    protected: Vec<PathBuf>,
}
impl Default for Files {
    fn default() -> Self {
        Self {
            serial: Mutex::new(()),
            protected: Vec::new(),
        }
    }
}
pub struct Guarded {
    pub path: PathBuf,
    _handles: Vec<File>,
}
pub struct Output {
    destination: Guarded,
    _parent: Guarded,
    pub stage: PathBuf,
    before: String,
}
impl Output {
    pub fn destination(&self) -> &Path {
        &self.destination.path
    }
}
fn clean_absolute(path: &Path) -> Result<()> {
    #[cfg(target_os="android")]
    crate::android::path_arg(path.to_str().ok_or("INVALID_ARGUMENT: UTF-8 Android path required")?)?;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("absolute normalized target path required".into());
    }
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if s.starts_with("\\\\") || s.starts_with("//") || s[2..].contains(':') {
            return Err(
                "UNC/device/alternate-stream paths are unsupported for file operations".into(),
            );
        }
    }
    Ok(())
}
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
// Pin existing ancestors against rename on Windows, reject every reparse/symlink component.
fn guarded(path: &Path, write: bool) -> Result<Guarded> {
    clean_absolute(path)?;
    let path = path.to_path_buf();
    let mut handles = Vec::new();
    let mut current = PathBuf::new();
    for c in path.components() {
        current.push(c);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if is_link(&meta) {
                    return Err("REPARSE_POINT_DENIED".into());
                }
                if meta.is_dir() {
                    // Linux/Android ancestors may be searchable but not readable (/data).
                    // Other Unix hosts do not need Windows' rename-prevention handles.
                    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
                    {
                        let mut opts = OpenOptions::new();
                        opts.read(true);
                        #[cfg(windows)]
                        {
                            use std::os::windows::fs::OpenOptionsExt;
                            opts.custom_flags(0x02000000 | 0x00200000).share_mode(1 | 2);
                        }
                        #[cfg(any(target_os = "linux", target_os = "android"))]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            opts.custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
                        }
                        let handle = opts.open(&current).map_err(|e| e.to_string())?;
                        if is_link(&handle.metadata().map_err(|e| e.to_string())?) {
                            return Err("REPARSE_POINT_DENIED".into());
                        }
                        handles.push(handle);
                    }
                } else if meta.is_file() && !write {
                    let mut opts = OpenOptions::new();
                    opts.read(true);
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::OpenOptionsExt;
                        opts.custom_flags(0x00200000).share_mode(1);
                    }
                    let handle = opts.open(&current).map_err(|e| e.to_string())?;
                    if is_link(&handle.metadata().map_err(|e| e.to_string())?) {
                        return Err("REPARSE_POINT_DENIED".into());
                    }
                    handles.push(handle);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(Guarded {
        path,
        _handles: handles,
    })
}
fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(0x00200000).share_mode(1);
    }
    let mut handle = opts.open(path)?;
    if is_link(&handle.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "REPARSE_POINT_DENIED",
        ));
    }
    let mut out = Vec::new();
    handle.read_to_end(&mut out)?;
    Ok(out)
}
fn version(path: &Path) -> Result<String> {
    match read_bytes(path) {
        Ok(b) => Ok(digest(&b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("absent".into()),
        Err(e) => Err(e.to_string()),
    }
}
#[cfg(windows)]
pub(crate) fn replace(tmp: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let a: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            a.as_ptr(),
            b.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
pub(crate) fn replace(tmp: &Path, path: &Path) -> Result<()> {
    fs::rename(tmp, path).map_err(|e| e.to_string())
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or(format!("missing string {key}"))
}
impl Files {
    pub fn new(protected: Vec<PathBuf>) -> Self {
        Self {
            serial: Mutex::new(()),
            protected: protected
                .into_iter()
                .map(|p| resolved(&p).unwrap_or(p))
                .collect(),
        }
    }
    pub fn pin(path: &Path, write: bool) -> Result<Guarded> {
        guarded(path, write)
    }
    fn guard(&self, path: &Path, write: bool) -> Result<Guarded> {
        let pinned = guarded(path, write)?;
        let actual = resolved(&pinned.path)?;
        if self
            .protected
            .iter()
            .any(|root| under(&actual, root) || (write && under(root, &actual)))
        {
            return Err("INTERNAL_PATH_PROTECTED".into());
        }
        Ok(pinned)
    }
    pub fn exclusions(&self, scope: &Path) -> Vec<String> {
        let scope = resolved(scope).unwrap_or_else(|_| scope.to_path_buf());
        self.protected
            .iter()
            .filter(|root| under(root, &scope))
            .map(|root| {
                let relative: PathBuf =
                    root.components().skip(scope.components().count()).collect();
                let literal = relative.to_string_lossy().replace('\\', "/");
                let escaped = literal
                    .chars()
                    .map(|c| match c {
                        '[' => "[[]".into(),
                        ']' => "[]]".into(),
                        '*' => "[*]".into(),
                        '?' => "[?]".into(),
                        '{' => "[{]".into(),
                        '}' => "[}]".into(),
                        c => c.to_string(),
                    })
                    .collect::<String>();
                format!("{escaped}/**")
            })
            .collect()
    }
    pub fn stat(&self, args: &Value) -> Result<Value> {
        let p = self.guard(Path::new(text(args, "path")?), false)?;
        let meta = fs::metadata(&p.path).map_err(|e| e.to_string())?;
        let directory = if meta.is_file() {
            p.path.parent().ok_or("missing parent")?.to_path_buf()
        } else {
            p.path.clone()
        };
        let target = if meta.is_file() {
            p.path.file_name().unwrap().to_string_lossy().to_string()
        } else {
            ".".into()
        };
        Ok(json!({"path":p.path,"directory":directory,"file":meta.is_file(),"target":target}))
    }
    pub fn check(&self, path: &Path, write: bool) -> Result<()> {
        self.guard(path, write).map(|_| ())
    }
    // Copy alone may inspect/replace a symlink leaf; never follow it or relax parent guards.
    pub(crate) fn copy_link(&self, path: &Path, write: bool) -> Result<Guarded> {
        clean_absolute(path)?;
        let parent = path.parent().ok_or("COPY_PARENT_MISSING")?;
        let pinned = self.guard(parent, false)?;
        let actual = resolved(parent)?.join(path.file_name().ok_or("COPY_LINK_NAME")?);
        if self.protected.iter().any(|root| under(&actual, root) || (write && under(root, &actual))) {
            return Err("INTERNAL_PATH_PROTECTED".into());
        }
        Ok(Guarded { path: path.into(), _handles: pinned._handles })
    }
    pub fn read(&self, args: &Value) -> Result<Value> {
        let _serial = self.serial.lock().unwrap();
        let p = self.guard(Path::new(text(args, "path")?), false)?;
        let bytes = match read_bytes(&p.path) {
            Ok(v) => v,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && args.get("allowAbsent") == Some(&Value::Bool(true)) =>
            {
                return Ok(
                    json!({"content":"","version":"absent","path":p.path,"temporary":p.path.starts_with(std::env::temp_dir())}),
                )
            }
            Err(e) => return Err(e.to_string()),
        };
        let content = String::from_utf8(bytes.clone()).map_err(|_| "file is not UTF-8")?;
        if content.contains('\0') {
            return Err("NUL-containing text is unsupported".into());
        }
        Ok(
            json!({"content":content,"version":digest(&bytes),"path":p.path,"temporary":p.path.starts_with(std::env::temp_dir())}),
        )
    }
    pub fn write(&self, args: &Value) -> Result<Value> {
        let _serial = self.serial.lock().unwrap();
        let p = self.guard(Path::new(text(args, "path")?), true)?;
        let before = version(&p.path)?;
        if let Some(expected) = args.get("version").and_then(Value::as_str) {
            if expected != before {
                return Err("VERSION_CONFLICT: no changes made".into());
            }
        }
        let parent = p.path.parent().ok_or("path has no parent")?;
        if !parent.is_dir() {
            let mut missing = Vec::new();
            let mut current = parent;
            while !current.exists() {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or("missing parent root")?;
            }
            for dir in missing.into_iter().rev() {
                let _pin = self.guard(&dir, true)?;
                fs::create_dir(&dir).map_err(|e| e.to_string())?;
            }
        }
        // The file is the mutation target; its parent is only pinned, not replaced.
        let _parent_pin = self.guard(parent, false)?;
        let tmp = parent.join(format!(".maxa-{}.tmp", id()));
        private_write(&tmp, text(args, "content")?.as_bytes())?;
        let result = (|| {
            let _again = self.guard(&p.path, true)?;
            if version(&p.path)? != before {
                return Err("VERSION_CONFLICT: no changes made".into());
            }
            replace(&tmp, &p.path)?;
            Ok(json!({"path":p.path,"version":version(&p.path)?}))
        })();
        let _ = fs::remove_file(tmp);
        result
    }
    pub fn mkdir(&self, args: &Value) -> Result<Value> {
        let _serial = self.serial.lock().unwrap();
        let p = self.guard(Path::new(text(args, "path")?), true)?;
        fs::create_dir(&p.path).map_err(|e| e.to_string())?;
        Ok(json!({"path":p.path}))
    }
    pub fn mov(&self, args: &Value) -> Result<Value> {
        let _serial = self.serial.lock().unwrap();
        let mut from = self.guard(Path::new(text(args, "path")?), true)?;
        let to = self.guard(Path::new(text(args, "new_path")?), true)?;
        if from.path.is_dir() {
            from._handles.pop();
        }
        if to.path.exists() {
            return Err("destination exists".into());
        }
        fs::rename(&from.path, &to.path).map_err(|e| e.to_string())?;
        Ok(json!({"path":from.path,"newPath":to.path}))
    }
    pub fn delete(&self, args: &Value) -> Result<Value> {
        let _serial = self.serial.lock().unwrap();
        let paths = args
            .get("paths")
            .and_then(Value::as_array)
            .ok_or("paths must be an array")?;
        // Validate every target before the first effect.
        let checked: Vec<Guarded> = paths
            .iter()
            .map(|p| self.guard(Path::new(p.as_str().ok_or("path must be string")?), true))
            .collect::<Result<_>>()?;
        let mut results = Vec::new();
        for mut p in checked {
            if p.path.is_dir() {
                p._handles.pop();
            }
            let result = if p.path.is_dir() {
                remove_tree(&p.path)
            } else {
                fs::remove_file(&p.path).map_err(|e| e.to_string())
            };
            results.push(json!({"path":p.path,"error":result.err()}));
        }
        Ok(json!({"items":results}))
    }
    // The upstream receives only stable private copies, never a caller-selected mutable path.
    pub fn stage_input(&self, path: &Path, stage: &Path) -> Result<PathBuf> {
        let _serial = self.serial.lock().unwrap();
        let input = self.guard(path, false)?;
        let bytes = read_bytes(&input.path).map_err(|e| e.to_string())?;
        private_write(stage, &bytes)?;
        Ok(stage.into())
    }
    pub fn stage_output(&self, path: &Path, stage: PathBuf) -> Result<Output> {
        let _serial = self.serial.lock().unwrap();
        let destination = self.guard(path, true)?;
        let parent = destination.path.parent().ok_or("output has no parent")?;
        // Existing parent is required so it remains pinned through the upstream effect.
        if !parent.is_dir() {
            return Err("output parent must already exist".into());
        }
        // As in write(), protect the final file and only pin/check its parent.
        // An ancestor of a protected root may contain unrelated output files.
        let parent = self.guard(parent, false)?;
        let before = version(&destination.path)?;
        Ok(Output {
            destination,
            _parent: parent,
            stage,
            before,
        })
    }
    pub fn commit_output(&self, output: &Output) -> Result<Vec<u8>> {
        let _serial = self.serial.lock().unwrap();
        let _current = self.guard(&output.destination.path, true)?;
        let bytes =
            read_bytes(&output.stage).map_err(|e| format!("upstream output unavailable: {e}"))?;
        let parent = output
            .destination
            .path
            .parent()
            .ok_or("missing output parent")?;
        let tmp = parent.join(format!(".maxa-{}.tmp", id()));
        private_write(&tmp, &bytes)?;
        let result = (|| {
            if version(&output.destination.path)? != output.before {
                return Err("VERSION_CONFLICT: upstream ran; destination not overwritten".into());
            }
            replace(&tmp, &output.destination.path)?;
            Ok(bytes)
        })();
        let _ = fs::remove_file(tmp);
        result
    }
    pub fn search_root(&self, path: &Path) -> Result<Guarded> {
        self.guard(path, false)
    }
}
fn resolved(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .ok_or("missing absolute path ancestor")?
                .to_os_string(),
        );
        existing = existing.parent().ok_or("missing absolute path ancestor")?;
    }
    let canonical = fs::canonicalize(existing).map_err(|e| e.to_string())?;
    let mut result = PathBuf::from(canonical.to_string_lossy().trim_start_matches("\\\\?\\"));
    for name in missing.into_iter().rev() {
        result.push(name);
    }
    Ok(result)
}
fn under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let p = path
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .replace('\\', "/")
            .to_lowercase();
        let r = root
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        p == r || p.starts_with(&(r + "/"))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}
pub(crate) fn remove_tree(p: &Path) -> Result<()> {
    // Never recurse through reparse points, even when a tree changed after initial validation.
    let m = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    if is_link(&m) {
        return Err("REPARSE_POINT_DENIED".into());
    }
    if m.is_dir() {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.custom_flags(0x02000000 | 0x00200000).share_mode(1 | 2);
        }
        let pin = opts.open(p).map_err(|e| e.to_string())?;
        if is_link(&pin.metadata().map_err(|e| e.to_string())?) {
            return Err("REPARSE_POINT_DENIED".into());
        }
        for child in fs::read_dir(p).map_err(|e| e.to_string())? {
            remove_tree(&child.map_err(|e| e.to_string())?.path())?
        }
        drop(pin);
        fs::remove_dir(p).map_err(|e| e.to_string())
    } else {
        fs::remove_file(p).map_err(|e| e.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absolute_paths_internal_protection_and_conflicts() {
        let dir = std::env::temp_dir().join(id());
        fs::create_dir_all(dir.join("private")).unwrap();
        let f = Files::new(vec![dir.join("private")]);
        let target = dir.join("workspace/file.txt");
        assert!(f
            .write(&json!({"path":target,"content":"a","version":"absent"}))
            .is_ok());
        assert!(f
            .write(&json!({"path":target,"content":"b","version":"absent"}))
            .unwrap_err()
            .starts_with("VERSION_CONFLICT"));
        assert!(f.read(&json!({"path":"relative.txt"})).is_err());
        assert!(f.read(&json!({"path":dir.join("../outside")})).is_err());
        assert_eq!(
            f.write(&json!({"path":dir.join("private/identity"),"content":"x"}))
                .unwrap_err(),
            "INTERNAL_PATH_PROTECTED"
        );
        assert_eq!(
            f.delete(&json!({"paths":[dir]})).unwrap_err(),
            "INTERNAL_PATH_PROTECTED"
        );
        assert_eq!(f.exclusions(&dir), vec!["private/**"]);
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn searchable_unreadable_ancestor_allows_file_access_but_not_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::set_permissions(self.0.join("walk"), fs::Permissions::from_mode(0o700));
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(id()));
        let parent = fixture.0.join("walk");
        let scope = parent.join("workspace");
        fs::create_dir_all(&scope).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o311)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(fs::read_dir(&parent).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
        let files = Files::new(vec![parent.join("private")]);
        let target = scope.join("hello.txt");
        files.write(&json!({"path":target,"content":"hello"})).unwrap();
        assert_eq!(files.read(&json!({"path":target})).unwrap()["content"], "hello");
        files.write(&json!({"path":target,"content":"updated"})).unwrap();
        assert_eq!(files.read(&json!({"path":target})).unwrap()["content"], "updated");
        files.write(&json!({"path":scope.join("nested/new.txt"),"content":"new"})).unwrap();
        assert_eq!(resolved(&target).unwrap(), target);
        assert_eq!(files.search_root(&scope).unwrap().path, scope);
        assert_eq!(files.search_root(&target).unwrap().path, target);
        files.stage_input(&target, &fixture.0.join("staged.txt")).unwrap();
        assert_eq!(fs::read(fixture.0.join("staged.txt")).unwrap(), b"updated");
        symlink(&target, scope.join("leaf-link")).unwrap();
        symlink(&scope, fixture.0.join("parent-link")).unwrap();
        for path in [scope.join("leaf-link"), fixture.0.join("parent-link/hello.txt")] {
            assert!(files.read(&json!({"path":path})).unwrap_err().contains("REPARSE_POINT_DENIED"));
            assert!(files.write(&json!({"path":path,"content":"blocked"})).unwrap_err().contains("REPARSE_POINT_DENIED"));
            assert!(files.search_root(&path).is_err());
        }
    }
    #[test]
    fn output_file_under_protected_data_ancestor_uses_file_level_guard() {
        let root = std::env::temp_dir().join(id());
        let home = root.join("home");
        let data = home.join(".local/share/maxa-device");
        let staging = data.join("staging");
        let resources = root.join("prefix/opt/maxa-device/0.1.1");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&resources).unwrap();
        let files = Files::new(vec![data.clone(), resources.clone()]);
        // Both Mobile output arguments share stage_output; their parent contains
        // the protected data root, but the final files do not overlap it.
        for name in ["maxa-shot.png", "maxa-recording.mp4"] {
            let target = home.join(name);
            let stage = staging.join(name);
            let output = files.stage_output(&target, stage.clone()).unwrap();
            fs::write(&stage, b"output bytes").unwrap();
            assert_eq!(files.commit_output(&output).unwrap(), b"output bytes");
            assert_eq!(fs::read(&target).unwrap(), b"output bytes");
        }
        let system_file = home.join("system.txt");
        files.write(&json!({"path":system_file,"content":"new"})).unwrap();
        files.write(&json!({"path":system_file,"content":"overwrite"})).unwrap();
        assert_eq!(fs::read(&system_file).unwrap(), b"overwrite");
        for protected in [&data, &data.join("blocked.png"), &resources] {
            assert_eq!(files.stage_output(protected, staging.join("blocked")).err().unwrap(), "INTERNAL_PATH_PROTECTED");
            assert_eq!(files.write(&json!({"path":protected,"content":"blocked"})).unwrap_err(), "INTERNAL_PATH_PROTECTED");
        }
        // Pinning an ancestor is allowed; deleting/replacing it is still refused.
        assert_eq!(files.delete(&json!({"paths":[home]})).unwrap_err(), "INTERNAL_PATH_PROTECTED");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn staged_input_and_guarded_output() {
        let dir = std::env::temp_dir().join(id());
        fs::create_dir_all(dir.join("public")).unwrap();
        fs::create_dir(dir.join("private")).unwrap();
        let f = Files::new(vec![dir.join("private")]);
        let input = dir.join("public/input.apk");
        fs::write(&input, b"old").unwrap();
        let copy = f
            .stage_input(&input, &dir.join("private/input.apk"))
            .unwrap();
        fs::write(&input, b"new").unwrap();
        assert_eq!(fs::read(copy).unwrap(), b"old");
        let target = dir.join("public/output.png");
        fs::write(&target, b"before").unwrap();
        let output = f
            .stage_output(&target, dir.join("private/output.png"))
            .unwrap();
        fs::write(&output.stage, b"image").unwrap();
        #[cfg(windows)]
        assert!(fs::rename(dir.join("public"), dir.join("moved")).is_err());
        fs::write(&target, b"outside change").unwrap();
        assert!(f
            .commit_output(&output)
            .unwrap_err()
            .starts_with("VERSION_CONFLICT"));
        assert_eq!(fs::read(target).unwrap(), b"outside change");
        drop(output);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn replaced_directory_does_not_redirect_access() {
        let dir = std::env::temp_dir().join(id());
        let root = dir.join("root");
        let outside = dir.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("data"), b"outside").unwrap();
        fs::remove_dir(&root).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            assert!(std::process::Command::new("cmd.exe")
                .creation_flags(0x08000000)
                .args(["/d", "/c", "mklink", "/J"])
                .arg(&root)
                .arg(&outside)
                .output()
                .unwrap()
                .status
                .success());
        }
        assert!(Files::default()
            .read(&json!({"path":root.join("data")}))
            .unwrap_err()
            .contains("REPARSE_POINT_DENIED"));
        #[cfg(unix)]
        fs::remove_file(root).unwrap();
        #[cfg(windows)]
        fs::remove_dir(root).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
