//! System copy: metadata-only full preflight, owned bounded byte streams, per-file commit.
use crate::{
    files::{Files, Guarded},
    model::*,
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::UNIX_EPOCH,
};

pub const MAX_FILE_BYTES: u64 = 100_000_000_000;
pub const BUFFER_BYTES: usize = 1024 * 1024;
const LIFETIME: u64 = 24 * 3600 * 1000;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub directory: bool,
    pub bytes: u64,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub directory: bool,
    pub entries: Vec<Entry>,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_windows: Option<bool>,
}
#[derive(Clone)]
struct Staged {
    path: PathBuf,
    sha256: String,
    version: String,
    _file: Arc<File>,
}
struct Progress {
    started: bool,
    busy: bool,
    done: HashMap<usize, String>,
    links: HashSet<usize>,
    staged: HashMap<usize, Staged>,
    temps: HashMap<usize, PathBuf>,
    bytes: u64,
    error: Option<String>,
}
pub struct Session {
    pub id: String,
    client: String,
    chat: String,
    root: PathBuf,
    pub manifest: Manifest,
    source: bool,
    overwrite: bool,
    versions: HashMap<String, String>,
    link_targets: HashMap<usize, usize>,
    root_guard: Mutex<Option<Guarded>>,
    pub cancelled: AtomicBool,
    expires: u64,
    progress: Mutex<Progress>,
}
#[derive(Default)]
pub struct Transfers {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn text<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v[name].as_str().ok_or_else(|| format!("{name} required"))
}
fn linked(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        m.file_type().is_symlink()
    }
}
pub fn version(path: &Path) -> Result<String> {
    version_for(path, false)
}
fn version_for(path: &Path, allow_link: bool) -> Result<String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok("absent".into()),
        Err(e) => Err(err(e)),
        Ok(m) => {
            let symlink = m.file_type().is_symlink();
            if linked(&m) && !(allow_link && symlink) {
                return Err("REPARSE_POINT_DENIED".into());
            }
            if !m.is_file() && !m.is_dir() && !symlink {
                return Err("SPECIAL_FILE_DENIED".into());
            }
            let modified = m
                .modified()
                .map_err(err)?
                .duration_since(UNIX_EPOCH)
                .map_err(err)?
                .as_nanos();
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                format!("{}:{}:{}:{}", m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
            };
            #[cfg(windows)]
            let identity = {
                use std::os::windows::fs::MetadataExt;
                format!("{}:{}", m.creation_time(), m.file_attributes())
            };
            #[cfg(not(any(unix, windows)))]
            let identity = String::new();
            let link = if symlink {
                fs::read_link(path)
                    .map_err(err)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| "NON_UTF8_COPY_LINK")?
            } else {
                String::new()
            };
            Ok(format!(
                "{}:{}:{modified}:{identity}:{symlink}:{link}",
                m.is_dir(),
                m.len()
            ))
        }
    }
}
fn relative(root: &Path, path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Ok(root.into());
    }
    if path.contains('\\')
        || path.contains('\0')
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err("INVALID_COPY_RELATIVE_PATH".into());
    }
    let mut out = root.to_path_buf();
    for part in path.split('/') {
        #[cfg(windows)]
        {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            if part.ends_with(['.', ' '])
                || part.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c))
                || ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
            {
                return Err("INVALID_WINDOWS_COPY_NAME".into());
            }
        }
        out.push(part);
    }
    Ok(out)
}
fn normalized(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        value.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}
fn overlap(source: &Path, destination: &Path, directory: bool) -> bool {
    let a = normalized(source);
    let b = normalized(destination);
    a == b || (directory && b.starts_with(&(a.trim_end_matches('/').to_owned() + "/")))
}
fn entry_version(root: &Path, entry: &Entry) -> Result<String> {
    version_for(&relative(root, &entry.path)?, entry.link_target.is_some())
}
fn check_entry(files: &Files, path: &Path, entry: &Entry, write: bool) -> Result<Guarded> {
    if entry.link_target.is_some() {
        files.copy_link(path, write)
    } else {
        files.check(path, write)?;
        Files::pin(path, write)
    }
}

// Resolve against metadata only: no symlink target is opened during preflight.
struct LinkPlan {
    order: Vec<usize>,
    targets: HashMap<usize, usize>,
}
fn link_plan(manifest: &Manifest, enabled: bool, windows: bool) -> Result<LinkPlan> {
    fn key(value: &str, windows: bool) -> String {
        if windows {
            value.to_lowercase()
        } else {
            value.into()
        }
    }
    fn visit(
        index: usize,
        m: &Manifest,
        names: &HashMap<String, usize>,
        windows: bool,
        states: &mut [u8],
        resolved: &mut [String],
        plan: &mut LinkPlan,
    ) -> Result<String> {
        if states[index] == 1 {
            return Err("COPY_SYMLINK_CYCLE".into());
        }
        if states[index] == 2 {
            return Ok(resolved[index].clone());
        }
        states[index] = 1;
        let entry = &m.entries[index];
        let text = entry
            .link_target
            .as_deref()
            .ok_or("COPY_EXPECTED_SYMLINK")?;
        if text.is_empty()
            || text.contains('\0')
            || text.starts_with(['/', '\\'])
            || (text.as_bytes().get(1) == Some(&b':') && text.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err("COPY_SYMLINK_TARGET_DENIED".into());
        }
        let text = if windows {
            text.replace('\\', "/")
        } else {
            text.into()
        };
        let parent = entry.path.rsplit_once('/').map(|v| v.0).unwrap_or("");
        let mut parts: Vec<String> = if parent.is_empty() {
            vec![]
        } else {
            parent.split('/').map(String::from).collect()
        };
        for part in text.split('/') {
            let current = *names
                .get(&key(&parts.join("/"), windows))
                .ok_or("COPY_SYMLINK_DANGLING")?;
            if !m.entries[current].directory {
                return Err("COPY_SYMLINK_NOT_DIRECTORY".into());
            }
            match part {
                "" | "." => (),
                ".." => {
                    parts.pop().ok_or("COPY_SYMLINK_ESCAPE")?;
                }
                _ => {
                    parts.push(part.into());
                    let next = *names
                        .get(&key(&parts.join("/"), windows))
                        .ok_or("COPY_SYMLINK_DANGLING")?;
                    if m.entries[next].link_target.is_some() {
                        let name = visit(next, m, names, windows, states, resolved, plan)?;
                        parts = if name.is_empty() {
                            vec![]
                        } else {
                            name.split('/').map(String::from).collect()
                        };
                    }
                }
            }
        }
        let target = *names
            .get(&key(&parts.join("/"), windows))
            .ok_or("COPY_SYMLINK_DANGLING")?;
        let name = m.entries[target].path.clone();
        plan.targets.insert(index, target);
        plan.order.push(index);
        resolved[index] = name.clone();
        states[index] = 2;
        Ok(name)
    }
    let mut names = HashMap::new();
    for (index, entry) in manifest.entries.iter().enumerate() {
        if names.insert(key(&entry.path, windows), index).is_some() {
            return Err("INVALID_COPY_MANIFEST".into());
        }
        if entry.link_target.is_some()
            && (!enabled || entry.directory || entry.bytes != 0 || entry.path.is_empty())
        {
            return Err("COPY_SYMLINK_DENIED".into());
        }
    }
    let mut plan = LinkPlan {
        order: vec![],
        targets: HashMap::new(),
    };
    let mut states = vec![0; manifest.entries.len()];
    let mut resolved = vec![String::new(); manifest.entries.len()];
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.link_target.is_some() {
            visit(
                index,
                manifest,
                &names,
                windows,
                &mut states,
                &mut resolved,
                &mut plan,
            )?;
        }
    }
    Ok(plan)
}
#[cfg(test)]
fn source_manifest(files: &Files, root: &Path, recursive: bool) -> Result<Manifest> {
    source_manifest_with_links(files, root, recursive, true)
}
fn source_manifest_with_links(
    files: &Files,
    root: &Path,
    recursive: bool,
    symlinks: bool,
) -> Result<Manifest> {
    fn walk(
        files: &Files,
        root: &Path,
        path: &Path,
        out: &mut Vec<Entry>,
        total: &mut u64,
        symlinks: bool,
    ) -> Result<()> {
        let meta = fs::symlink_metadata(path).map_err(err)?;
        let is_link = meta.file_type().is_symlink();
        if is_link && symlinks && path != root {
            files.copy_link(path, false)?;
        } else {
            files.check(path, false)?;
        }
        let version = version_for(path, symlinks && path != root)?;
        let relative = path
            .strip_prefix(root)
            .map_err(err)?
            .to_str()
            .ok_or("NON_UTF8_COPY_NAME")?
            .replace('\\', "/");
        if !meta.is_dir() && meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "FILE_TOO_LARGE: {relative}; maxBytes={MAX_FILE_BYTES}"
            ));
        }
        let bytes = if !is_link && meta.is_file() {
            meta.len()
        } else {
            0
        };
        *total = total.checked_add(bytes).ok_or("COPY_SIZE_OVERFLOW")?;
        out.push(Entry {
            path: relative,
            directory: !is_link && meta.is_dir(),
            bytes,
            version,
            link_target: if is_link {
                Some(
                    fs::read_link(path)
                        .map_err(err)?
                        .into_os_string()
                        .into_string()
                        .map_err(|_| "NON_UTF8_COPY_LINK")?,
                )
            } else {
                None
            },
        });
        if !is_link && meta.is_dir() {
            let mut children = fs::read_dir(path)
                .map_err(err)?
                .map(|v| v.map(|v| v.path()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(err)?;
            children.sort();
            for child in children {
                walk(files, root, &child, out, total, symlinks)?;
            }
        }
        Ok(())
    }
    files.check(root, false)?;
    let m = fs::symlink_metadata(root).map_err(err)?;
    if m.is_dir() && !recursive {
        return Err("RECURSIVE_REQUIRED".into());
    }
    let mut manifest = Manifest {
        directory: m.is_dir(),
        entries: vec![],
        bytes: 0,
        link_windows: None,
    };
    walk(
        files,
        root,
        root,
        &mut manifest.entries,
        &mut manifest.bytes,
        symlinks,
    )?;
    if !link_plan(&manifest, symlinks, cfg!(windows))?
        .order
        .is_empty()
    {
        manifest.link_windows = Some(cfg!(windows));
    }
    Ok(manifest)
}
fn validate_manifest(manifest: &Manifest) -> Result<()> {
    let mut names = HashSet::new();
    let mut dirs = HashSet::new();
    let mut total = 0u64;
    if manifest.entries.is_empty()
        || manifest.entries[0].path != ""
        || manifest.entries[0].directory != manifest.directory
        || (!manifest.directory && manifest.entries.len() != 1)
    {
        return Err("INVALID_COPY_MANIFEST".into());
    }
    for entry in &manifest.entries {
        relative(Path::new("/"), &entry.path)?;
        let key = normalized(Path::new(&entry.path));
        if !names.insert(key)
            || entry.bytes > MAX_FILE_BYTES
            || (entry.directory && entry.bytes != 0)
        {
            return Err("INVALID_COPY_MANIFEST".into());
        }
        if !entry.path.is_empty() {
            let parent = entry.path.rsplit_once('/').map(|v| v.0).unwrap_or("");
            if !dirs.contains(parent) {
                return Err("INVALID_COPY_MANIFEST_PARENT".into());
            }
        }
        if entry.directory {
            dirs.insert(entry.path.as_str());
        }
        total = total.checked_add(entry.bytes).ok_or("COPY_SIZE_OVERFLOW")?;
    }
    if total != manifest.bytes {
        return Err("INVALID_COPY_MANIFEST_SIZE".into());
    }
    link_plan(manifest, true, manifest.link_windows.unwrap_or(false))?;
    Ok(())
}
fn writable(path: &Path, directory: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let access = if directory {
            0x0002 | 0x0004
        } else {
            0x40000000 | 0x00010000
        };
        OpenOptions::new()
            .access_mode(access)
            .custom_flags(0x02000000 | 0x00200000)
            .share_mode(1 | 2 | 4)
            .open(path)
            .map_err(err)?;
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let p = CString::new(path.as_os_str().as_bytes()).map_err(err)?;
        if unsafe {
            libc::access(
                p.as_ptr(),
                libc::W_OK | if directory { libc::X_OK } else { 0 },
            )
        } != 0
        {
            return Err(err(io::Error::last_os_error()));
        }
    }
    Ok(())
}
fn free_bytes(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let p: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0;
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                p.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(err(io::Error::last_os_error()));
        }
        Ok(free)
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let p = CString::new(path.as_os_str().as_bytes()).map_err(err)?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(p.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(err(io::Error::last_os_error()));
        }
        let stat = unsafe { stat.assume_init() };
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
}
impl Transfers {
    pub fn in_progress(&self) -> bool {
        self.sessions.lock().unwrap().values().any(|s|s.progress.lock().unwrap().busy)
    }
    pub fn quiesce_for_update(&self) -> Result<()> {
        let sessions=self.sessions.lock().unwrap();
        let mut progress=Vec::new();
        for s in sessions.values() {progress.push(s.progress.lock().unwrap());}
        if progress.iter().any(|p|p.busy) {return Err("BUSY: copy stream in progress".into());}
        for s in sessions.values() {s.cancelled.store(true,Ordering::SeqCst);}
        Ok(())
    }

    pub fn prepare(&self, files: &Files, client: &str, chat: &str, args: &Value) -> Result<Value> {
        let root = PathBuf::from(text(args, "path")?);
        let source = args["source"] == true;
        let manifest = if source {
            source_manifest_with_links(
                files,
                &root,
                args["recursive"] == true,
                args["symlinks"] != false,
            )?
        } else {
            let m: Manifest = serde_json::from_value(args["manifest"].clone()).map_err(err)?;
            validate_manifest(&m)?;
            m
        };
        let plan = link_plan(&manifest, args["symlinks"] != false, cfg!(windows))?;
        let original = link_plan(
            &manifest,
            args["symlinks"] != false,
            manifest.link_windows.unwrap_or(false),
        )?;
        if plan.targets != original.targets {
            return Err("COPY_SYMLINK_NOT_PORTABLE".into());
        }
        let pin = Files::pin(&root, !source)?;
        files.check(&root, !source)?;
        let mut versions = HashMap::new();
        if !source {
            if let Some(source_id) = args["sourceTransfer"].as_str() {
                let src = self.get(client, chat, source_id)?;
                if !src.source || overlap(&src.root, &root, src.manifest.directory) {
                    return Err("COPY_PATH_OVERLAP".into());
                }
            }
            let mut ancestor = root.as_path();
            while !ancestor.exists() {
                ancestor = ancestor.parent().ok_or("COPY_PARENT_MISSING")?;
            }
            let volume = if ancestor.is_file() {
                ancestor.parent().ok_or("COPY_PARENT_MISSING")?
            } else {
                ancestor
            };
            writable(volume, true)?;
            if free_bytes(volume)? < manifest.bytes {
                return Err("INSUFFICIENT_COPY_SPACE".into());
            }
            for entry in &manifest.entries {
                let path = relative(&root, &entry.path)?;
                let _pin = check_entry(files, &path, entry, true)?;
                let before = version_for(&path, entry.link_target.is_some())?;
                if entry.link_target.is_some() {
                    let mut parent = path.parent().ok_or("COPY_PARENT_MISSING")?;
                    while !parent.exists() {
                        parent = parent.parent().ok_or("COPY_PARENT_MISSING")?;
                    }
                    writable(parent, true)?;
                }
                if before != "absent" {
                    let m = fs::symlink_metadata(&path).map_err(err)?;
                    if (!m.file_type().is_symlink() && m.is_dir()) != entry.directory
                        || m.file_type().is_symlink() != entry.link_target.is_some()
                    {
                        return Err(format!("COPY_TYPE_CONFLICT: {}", entry.path));
                    }
                    if !entry.directory {
                        if args["overwrite"] != true {
                            return Err(format!("COPY_EXISTS: {}", entry.path));
                        }
                        if entry.link_target.is_none() {
                            writable(&path, false)?;
                        }
                    } else {
                        writable(&path, true)?;
                    }
                }
                versions.insert(entry.path.clone(), before);
            }
        }
        let id = id();
        let output = json!({"transferId":id,"manifest":manifest,"maxFileBytes":MAX_FILE_BYTES,"expiresAt":now()+LIFETIME});
        self.sessions.lock().unwrap().insert(
            id.clone(),
            Arc::new(Session {
                id,
                client: client.into(),
                chat: chat.into(),
                root,
                manifest,
                source,
                overwrite: args["overwrite"] == true,
                versions,
                link_targets: plan.targets,
                root_guard: Mutex::new(Some(pin)),
                cancelled: AtomicBool::new(false),
                expires: now() + LIFETIME,
                progress: Mutex::new(Progress {
                    started: false,
                    busy: false,
                    done: HashMap::new(),
                    links: HashSet::new(),
                    staged: HashMap::new(),
                    temps: HashMap::new(),
                    bytes: 0,
                    error: None,
                }),
            }),
        );
        Ok(output)
    }
    pub fn get(&self, client: &str, chat: &str, id: &str) -> Result<Arc<Session>> {
        let s = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or("UNKNOWN_COPY_TRANSFER")?;
        if s.client != client || s.chat != chat {
            return Err("COPY_OWNER_MISMATCH".into());
        }
        if s.expires <= now() {
            s.cancelled.store(true, Ordering::SeqCst);
            return Err("COPY_EXPIRED".into());
        }
        Ok(s)
    }
    pub fn cancel(&self, client: Option<&str>, store: &Store) {
        for session in self.sessions.lock().unwrap().values() {
            if client.is_none_or(|id| session.client == id) {
                session.cancelled.store(true, Ordering::SeqCst);
                session.root_guard.lock().unwrap().take();
                session.cleanup_temps(store);
            }
        }
    }
    pub fn cancel_links(&self, store: &Store) {
        for session in self.sessions.lock().unwrap().values() {
            if session.has_links() {
                session.cancelled.store(true, Ordering::SeqCst);
                session.root_guard.lock().unwrap().take();
                session.cleanup_temps(store);
            }
        }
    }
    pub fn cleanup(&self, files: &Files, store: &Store) {
        self.cleanup_expired(files, Some(store));
    }
    fn cleanup_expired(&self, _files: &Files, store: Option<&Store>) {
        self.sessions.lock().unwrap().retain(|_, s| {
            if s.expires > now() {
                return true;
            }
            s.cancelled.store(true, Ordering::SeqCst);
            s.root_guard.lock().unwrap().take();
            if let Some(store) = store {
                s.cleanup_temps(store);
            }
            false
        });
    }
    pub fn operation(
        &self,
        files: &Files,
        store: &Store,
        client: &str,
        chat: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        if operation == "copy.prepare" {
            return self.prepare(files, client, chat, args);
        }
        let s = self.get(client, chat, text(args, "transferId")?)?;
        match operation {
            "copy.start" => {
                s.start(files)?;
                Ok(s.status())
            }
            "copy.status" => {
                let mut value = s.status();
                if let Some(index) = args["index"].as_u64() {
                    value["sha256"] = s
                        .progress
                        .lock()
                        .unwrap()
                        .done
                        .get(&(index as usize))
                        .cloned()
                        .map(Value::String)
                        .unwrap_or(Value::Null);
                }
                Ok(value)
            }
            "copy.commit" => s.commit(files, store, args),
            "copy.link" => s.link(files, store, args),
            "copy.finish" => {
                if s.source {
                    for entry in &s.manifest.entries {
                        if entry_version(&s.root, entry)? != entry.version {
                            return Err("COPY_SOURCE_CHANGED".into());
                        }
                    }
                }
                let status = s.status();
                if status["completedFiles"] != status["files"]
                    || status["completedSymlinks"] != status["symlinks"]
                {
                    return Err(format!("COPY_INCOMPLETE: {status}"));
                }
                self.sessions.lock().unwrap().remove(&s.id);
                Ok(status)
            }
            "copy.cancel" => {
                s.cancelled.store(true, Ordering::SeqCst);
                s.root_guard.lock().unwrap().take();
                s.cleanup_temps(store);
                Ok(s.status())
            }
            _ => Err("unsupported copy operation".into()),
        }
    }
}
impl Session {
    pub fn has_links(&self) -> bool {
        !self.link_targets.is_empty()
    }
    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) || self.expires <= now() {
            Err("COPY_CANCELLED_OR_EXPIRED".into())
        } else {
            Ok(())
        }
    }
    fn path(&self, index: usize) -> Result<PathBuf> {
        relative(
            &self.root,
            &self
                .manifest
                .entries
                .get(index)
                .ok_or("COPY_FILE_INDEX")?
                .path,
        )
    }
    pub fn start(&self, files: &Files) -> Result<()> {
        self.check()?;
        let mut p = self.progress.lock().unwrap();
        if p.started {
            return Ok(());
        }
        // Recheck every planned path before creating even an empty destination directory.
        for entry in &self.manifest.entries {
            let path = relative(&self.root, &entry.path)?;
            let _pin = check_entry(files, &path, entry, !self.source)?;
            if self.source {
                if entry_version(&self.root, entry)? != entry.version {
                    return Err("COPY_SOURCE_CHANGED".into());
                }
            } else if version_for(&path, entry.link_target.is_some())? != self.versions[&entry.path]
            {
                return Err("COPY_DESTINATION_CHANGED".into());
            }
        }
        if !self.source {
            for entry in &self.manifest.entries {
                let path = relative(&self.root, &entry.path)?;
                let directory = if entry.directory {
                    path.as_path()
                } else {
                    path.parent().ok_or("COPY_PARENT_MISSING")?
                };
                let mut missing = Vec::new();
                let mut current = directory;
                while !current.exists() {
                    missing.push(current.to_path_buf());
                    current = current.parent().ok_or("COPY_PARENT_MISSING")?;
                }
                for item in missing.into_iter().rev() {
                    files.check(&item, true)?;
                    let _pin = Files::pin(item.parent().ok_or("COPY_PARENT_MISSING")?, true)?;
                    fs::create_dir(&item).map_err(err)?;
                }
            }
        }
        p.started = true;
        Ok(())
    }
    pub fn status(&self) -> Value {
        let p = self.progress.lock().unwrap();
        json!({"transferId":self.id,"state":if self.cancelled.load(Ordering::SeqCst){"cancelled"}else if p.busy{"transferring"}else if !p.started{"prepared"}else{"ready"},"files":self.manifest.entries.iter().filter(|e|!e.directory && e.link_target.is_none()).count(),"directories":self.manifest.entries.iter().filter(|e|e.directory).count(),"symlinks":self.link_targets.len(),"completedFiles":p.done.len(),"completedSymlinks":p.links.len(),"bytes":p.bytes,"error":p.error})
    }
    fn acquire(self: &Arc<Self>, index: usize) -> Result<Lease> {
        self.check()?;
        let entry = self.manifest.entries.get(index).ok_or("COPY_FILE_INDEX")?;
        if entry.directory || entry.link_target.is_some() {
            return Err("COPY_EXPECTED_FILE".into());
        }
        let mut p = self.progress.lock().unwrap();
        if self.cancelled.load(Ordering::SeqCst) || !p.started || p.busy || p.done.contains_key(&index) || p.staged.contains_key(&index) {
            return Err("COPY_FILE_STATE_CONFLICT".into());
        }
        p.busy = true;
        Ok(Lease {
            session: self.clone(),
            complete: false,
        })
    }
    pub fn source(self: &Arc<Self>, files: &Files, index: usize) -> Result<Source> {
        if !self.source {
            return Err("COPY_DIRECTION_MISMATCH".into());
        }
        let lease = self.acquire(index)?;
        let path = self.path(index)?;
        files.check(&path, false)?;
        let pin = Files::pin(&path, false)?;
        if version(&path)? != self.manifest.entries[index].version {
            return Err("COPY_SOURCE_CHANGED".into());
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(1).custom_flags(0x00200000);
        }
        Ok(Source {
            file: opts.open(&path).map_err(err)?,
            _pin: pin,
            lease,
            index,
            path,
        })
    }
    pub fn destination(
        self: &Arc<Self>,
        files: &Files,
        store: &Store,
        index: usize,
    ) -> Result<Destination> {
        if self.source {
            return Err("COPY_DIRECTION_MISMATCH".into());
        }
        let lease = self.acquire(index)?;
        let path = self.path(index)?;
        files.check(&path, true)?;
        let pin = Files::pin(&path, true)?;
        if version(&path)? != self.versions[&self.manifest.entries[index].path] {
            return Err("COPY_DESTINATION_CHANGED".into());
        }
        let temp = path
            .parent()
            .ok_or("COPY_PARENT_MISSING")?
            .join(format!(".maxa-copy-{}.tmp", id()));
        store.copy_temp(&temp, true)?;
        self.progress
            .lock()
            .unwrap()
            .temps
            .insert(index, temp.clone());
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(1 | 4);
        }
        Ok(Destination {
            file: opts.open(&temp).map_err(err)?,
            _pin: pin,
            lease,
            index,
            temp,
        })
    }
    fn commit(&self, files: &Files, store: &Store, args: &Value) -> Result<Value> {
        self.check()?;
        let index = args["index"].as_u64().ok_or("COPY_FILE_INDEX")? as usize;
        let path = self.path(index)?;
        let mut p = self.progress.lock().unwrap();
        if let Some(hash) = p.done.get(&index) {
            return Ok(json!({"sha256":hash,"bytes":self.manifest.entries[index].bytes}));
        }
        let staged = p.staged.get(&index).cloned().ok_or("COPY_NOT_STAGED")?;
        if args["sha256"] != staged.sha256 || version(&staged.path)? != staged.version {
            return Err("COPY_INTEGRITY_FAILURE".into());
        }
        files.check(&path, true)?;
        let _pin = Files::pin(&path, true)?;
        if version(&path)? != self.versions[&self.manifest.entries[index].path] {
            return Err("COPY_DESTINATION_CHANGED".into());
        }
        if !self.overwrite {
            #[cfg(windows)]
            {
                move_new(&staged.path, &path)?;
            }
            #[cfg(not(windows))]
            {
                fs::hard_link(&staged.path, &path).map_err(err)?;
                fs::remove_file(&staged.path).map_err(err)?;
            }
        } else {
            crate::files::replace(&staged.path, &path)?;
        }
        p.staged.remove(&index);
        p.temps.remove(&index);
        p.done.insert(index, staged.sha256.clone());
        p.bytes += self.manifest.entries[index].bytes;
        store.copy_temp(&staged.path, false)?;
        Ok(json!({"sha256":staged.sha256,"bytes":self.manifest.entries[index].bytes}))
    }
    fn link(&self, files: &Files, store: &Store, args: &Value) -> Result<Value> {
        self.check()?;
        let index = args["index"].as_u64().ok_or("COPY_FILE_INDEX")? as usize;
        let entry = self.manifest.entries.get(index).ok_or("COPY_FILE_INDEX")?;
        let text = entry
            .link_target
            .as_deref()
            .ok_or("COPY_EXPECTED_SYMLINK")?;
        let path = self.path(index)?;
        let mut p = self.progress.lock().unwrap();
        if p.links.contains(&index) {
            return Ok(json!({"linkTarget":text}));
        }
        if !p.started
            || p.busy
            || p.done.len()
                != self
                    .manifest
                    .entries
                    .iter()
                    .filter(|e| !e.directory && e.link_target.is_none())
                    .count()
        {
            return Err("COPY_FILE_STATE_CONFLICT".into());
        }
        let _pin = files.copy_link(&path, !self.source)?;
        if self.source {
            if entry_version(&self.root, entry)? != entry.version {
                return Err("COPY_SOURCE_CHANGED".into());
            }
        } else {
            if version_for(&path, true)? != self.versions[&entry.path] {
                return Err("COPY_DESTINATION_CHANGED".into());
            }
            let directory = self.manifest.entries[self.link_targets[&index]].directory;
            let temp = path
                .parent()
                .ok_or("COPY_PARENT_MISSING")?
                .join(format!(".maxa-copy-{}.tmp", id()));
            store.copy_temp(&temp, true)?;
            p.temps.insert(index, temp.clone());
            create_symlink(text, &temp, directory)?;
            if fs::read_link(&temp).map_err(err)? != Path::new(text) {
                return Err("COPY_INTEGRITY_FAILURE".into());
            }
            files.copy_link(&path, true)?;
            if version_for(&path, true)? != self.versions[&entry.path] {
                return Err("COPY_DESTINATION_CHANGED".into());
            }
            self.check()?;
            if self.overwrite {
                crate::files::replace(&temp, &path)?;
            } else {
                create_symlink(text, &path, directory)?;
            }
            p.links.insert(index);
            if !self.overwrite {
                remove_temp(&temp).map_err(err)?;
            }
            p.temps.remove(&index);
            store.copy_temp(&temp, false)?;
        }
        if !fs::symlink_metadata(&path)
            .map_err(err)?
            .file_type()
            .is_symlink()
            || fs::read_link(&path).map_err(err)? != Path::new(text)
        {
            return Err("COPY_INTEGRITY_FAILURE".into());
        }
        p.links.insert(index);
        let target = &self.manifest.entries[self.link_targets[&index]];
        let target_path = relative(&self.root, &target.path)?;
        files.check(&target_path, false)?;
        if fs::symlink_metadata(&target_path).map_err(err)?.is_dir() != target.directory {
            return Err("COPY_INTEGRITY_FAILURE".into());
        }
        Ok(json!({"linkTarget":text}))
    }
    fn cleanup_temps(&self, store: &Store) {
        let mut p = self.progress.lock().unwrap();
        if p.busy {
            return;
        }
        for (_, path) in p.temps.drain() {
            match remove_temp(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(_) => continue,
            }
            let _ = store.copy_temp(&path, false);
        }
        p.staged.clear();
    }
}
fn create_symlink(text: &str, path: &Path, directory: bool) -> Result<()> {
    #[cfg(windows)]
    {
        if directory {
            std::os::windows::fs::symlink_dir(text, path)
        } else {
            std::os::windows::fs::symlink_file(text, path)
        }
        .map_err(err)
    }
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(text, path).map_err(err)
    }
}
pub(crate) fn remove_temp(path: &Path) -> io::Result<()> {
    let _pin = Files::pin(
        path.parent()
            .ok_or_else(|| io::Error::other("COPY_PARENT_MISSING"))?,
        true,
    )
    .map_err(io::Error::other)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if fs::symlink_metadata(path)?.file_type().is_symlink_dir() {
            return fs::remove_dir(path);
        }
    }
    fs::remove_file(path)
}
#[cfg(windows)]
fn move_new(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 8) }
        == 0
    {
        Err(err(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}
struct Lease {
    session: Arc<Session>,
    complete: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut p = self.session.progress.lock().unwrap();
        p.busy = false;
        if !self.complete {
            p.error = Some("COPY_INTERRUPTED: file was not committed".into());
        }
    }
}
pub struct Source {
    pub file: File,
    _pin: Guarded,
    lease: Lease,
    index: usize,
    path: PathBuf,
}
impl Source {
    pub fn check(&self) -> Result<()> {
        self.lease.session.check()
    }
    pub fn expected(&self) -> u64 {
        self.lease.session.manifest.entries[self.index].bytes
    }
    pub fn finish(mut self, bytes: u64, hash: String) -> Result<()> {
        self.check()?;
        if bytes != self.expected()
            || version(&self.path)? != self.lease.session.manifest.entries[self.index].version
        {
            return Err("COPY_SOURCE_CHANGED".into());
        }
        let mut p = self.lease.session.progress.lock().unwrap();
        p.done.insert(self.index, hash);
        p.bytes += bytes;
        drop(p);
        self.lease.complete = true;
        Ok(())
    }
}
pub struct Destination {
    pub file: File,
    _pin: Guarded,
    lease: Lease,
    index: usize,
    temp: PathBuf,
}
impl Destination {
    pub fn check(&self) -> Result<()> {
        self.lease.session.check()
    }
    pub fn expected(&self) -> u64 {
        self.lease.session.manifest.entries[self.index].bytes
    }
    pub fn finish(mut self, bytes: u64, hash: String) -> Result<Value> {
        self.check()?;
        if bytes != self.expected() {
            return Err("COPY_SIZE_MISMATCH".into());
        }
        self.file.sync_all().map_err(err)?;
        let mut p = self.lease.session.progress.lock().unwrap();
        p.staged.insert(
            self.index,
            Staged {
                path: self.temp.clone(),
                sha256: hash.clone(),
                version: version(&self.temp)?,
                _file: Arc::new(self.file.try_clone().map_err(err)?),
            },
        );
        drop(p);
        self.lease.complete = true;
        Ok(json!({"bytes":bytes,"sha256":hash}))
    }
}
impl Drop for Destination {
    fn drop(&mut self) {
        if !self.lease.complete {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    struct Fixture {
        root: PathBuf,
        files: Files,
        transfers: Transfers,
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("maxa-copy-{}", id()));
            fs::create_dir(&root).unwrap();
            let store = Store::open(&root.join("state.sqlite")).unwrap();
            Self {
                root,
                files: Files::default(),
                transfers: Transfers::default(),
                store,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.transfers.cancel(None, &self.store);
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn directory_preflight_rejects_conflict_before_creating_contents() {
        let f = Fixture::new();
        let src = f.root.join("source");
        let dst = f.root.join("destination");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::create_dir(src.join("new-directory")).unwrap();
        fs::write(src.join("new-directory/file"), b"new").unwrap();
        fs::write(src.join("z-existing"), b"source").unwrap();
        fs::write(dst.join("z-existing"), b"keep").unwrap();
        assert!(source_manifest(&f.files, &src, false)
            .unwrap_err()
            .contains("RECURSIVE_REQUIRED"));
        let manifest = source_manifest(&f.files, &src, true).unwrap();
        let result = f.transfers.prepare(
            &f.files,
            "client",
            "chat",
            &json!({"path":dst,"manifest":manifest}),
        );
        assert!(result.unwrap_err().contains("COPY_EXISTS"));
        assert!(!dst.join("new-directory").exists());
        assert_eq!(fs::read(dst.join("z-existing")).unwrap(), b"keep");
    }
    #[test]
    fn streamed_file_is_verified_and_only_then_committed() {
        let f = Fixture::new();
        let source = f.root.join("source.bin");
        let destination = f.root.join("target.bin");
        fs::write(&source, b"\0\xffbinary").unwrap();
        let manifest = source_manifest(&f.files, &source, false).unwrap();
        let prepared = f
            .transfers
            .prepare(
                &f.files,
                "client",
                "chat",
                &json!({"path":destination,"manifest":manifest}),
            )
            .unwrap();
        let transfer = prepared["transferId"].as_str().unwrap();
        assert!(!destination.exists());
        let session = f.transfers.get("client", "chat", transfer).unwrap();
        assert!(f.transfers.get("other", "chat", transfer).is_err());
        assert!(session.destination(&f.files, &f.store, 0).is_err());
        session.start(&f.files).unwrap();
        let mut output = session.destination(&f.files, &f.store, 0).unwrap();
        output.file.write_all(b"\0\xffbinary").unwrap();
        let hash = digest(b"\0\xffbinary");
        output.finish(8, hash.clone()).unwrap();
        assert!(!destination.exists());
        assert!(session
            .commit(&f.files, &f.store, &json!({"index":0,"sha256":"wrong"}))
            .is_err());
        assert!(!destination.exists());
        session
            .commit(&f.files, &f.store, &json!({"index":0,"sha256":hash}))
            .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"\0\xffbinary");
        assert_eq!(session.status()["completedFiles"], 1);
    }
    #[test]
    fn cancellation_removes_uncommitted_file_and_blocks_new_stream() {
        let f = Fixture::new();
        let source = f.root.join("source");
        fs::write(&source, b"a").unwrap();
        let destination = f.root.join("target");
        let manifest = source_manifest(&f.files, &source, false).unwrap();
        let prepared = f
            .transfers
            .prepare(
                &f.files,
                "client",
                "chat",
                &json!({"path":destination,"manifest":manifest}),
            )
            .unwrap();
        let session = f
            .transfers
            .get("client", "chat", prepared["transferId"].as_str().unwrap())
            .unwrap();
        session.start(&f.files).unwrap();
        let mut output = session.destination(&f.files, &f.store, 0).unwrap();
        output.file.write_all(b"a").unwrap();
        output.finish(1, digest(b"a")).unwrap();
        f.transfers.cancel(Some("client"), &f.store);
        assert!(session.destination(&f.files, &f.store, 0).is_err());
        assert!(!destination.exists());
        assert!(!fs::read_dir(&f.root).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".maxa-copy-")));
    }
    #[test]
    fn failed_temp_cleanup_keeps_restart_recovery_record() {
        let f = Fixture::new();
        let source = f.root.join("source");
        fs::write(&source, b"a").unwrap();
        let manifest = source_manifest(&f.files, &source, false).unwrap();
        let prepared = f
            .transfers
            .prepare(
                &f.files,
                "client",
                "chat",
                &json!({"path":f.root.join("target"),"manifest":manifest}),
            )
            .unwrap();
        let session = f
            .transfers
            .get("client", "chat", prepared["transferId"].as_str().unwrap())
            .unwrap();
        let temp = f.root.join(format!(".maxa-copy-{}.tmp", id()));
        // A directory cannot be removed as a temporary file on either supported test OS.
        fs::create_dir(&temp).unwrap();
        f.store.copy_temp(&temp, true).unwrap();
        session
            .progress
            .lock()
            .unwrap()
            .temps
            .insert(0, temp.clone());
        session.cleanup_temps(&f.store);
        let db = rusqlite::Connection::open(f.root.join("state.sqlite")).unwrap();
        let pending: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM copy_temps WHERE path=?1",
                [temp.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        drop(db);
        // Once deletion becomes possible, startup retries the retained record.
        fs::remove_dir(&temp).unwrap();
        fs::write(&temp, b"uncommitted bytes").unwrap();
        let _reopened = Store::open(&f.root.join("state.sqlite")).unwrap();
        assert!(!temp.exists());
    }
    #[cfg(unix)]
    #[test]
    fn internal_links_preserve_structure_and_replace_only_link_leaves() {
        use std::io::Read;
        let f = Fixture::new();
        let src = f.root.join("links");
        let dst = f.root.join("copied");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("file"), b"payload").unwrap();
        for (name, target) in [
            ("alias", "file"),
            ("chain", "alias"),
            ("dir", "sub"),
            ("sub/up", ".."),
        ] {
            std::os::unix::fs::symlink(target, src.join(name)).unwrap();
        }
        assert!(source_manifest_with_links(&f.files, &src, true, false).is_err());
        let prepared = f
            .transfers
            .prepare(
                &f.files,
                "client",
                "chat",
                &json!({"path":src,"source":true,"recursive":true}),
            )
            .unwrap();
        let input = f
            .transfers
            .get("client", "chat", prepared["transferId"].as_str().unwrap())
            .unwrap();
        let manifest = input.manifest.clone();
        let plan = link_plan(&manifest, true, false).unwrap();
        assert_eq!(plan.order.len(), 4);
        fs::create_dir(&dst).unwrap();
        let outside = f.root.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("alias")).unwrap();
        let args = json!({"path":dst,"manifest":manifest,"overwrite":true});
        assert!(f
            .transfers
            .prepare(
                &f.files,
                "client",
                "chat",
                &json!({"path":dst,"manifest":manifest,"overwrite":false})
            )
            .is_err());
        let prepared = f
            .transfers
            .prepare(&f.files, "client", "chat", &args)
            .unwrap();
        let output = f
            .transfers
            .get("client", "chat", prepared["transferId"].as_str().unwrap())
            .unwrap();
        input.start(&f.files).unwrap();
        output.start(&f.files).unwrap();
        for (index, entry) in manifest.entries.iter().enumerate() {
            if !entry.directory && entry.link_target.is_none() {
                let mut source = input.source(&f.files, index).unwrap();
                let mut bytes = vec![];
                source.file.read_to_end(&mut bytes).unwrap();
                let hash = digest(&bytes);
                source.finish(bytes.len() as u64, hash.clone()).unwrap();
                let mut destination = output.destination(&f.files, &f.store, index).unwrap();
                destination.file.write_all(&bytes).unwrap();
                destination
                    .finish(bytes.len() as u64, hash.clone())
                    .unwrap();
                output
                    .commit(&f.files, &f.store, &json!({"index":index,"sha256":hash}))
                    .unwrap();
            }
        }
        for index in plan.order {
            input
                .link(&f.files, &f.store, &json!({"index":index}))
                .unwrap();
            output
                .link(&f.files, &f.store, &json!({"index":index}))
                .unwrap();
        }
        assert_eq!(fs::read(dst.join("chain")).unwrap(), b"payload");
        assert_eq!(fs::read_link(dst.join("sub/up")).unwrap(), Path::new(".."));
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        assert_eq!(output.status()["files"], 1);
        assert_eq!(output.status()["completedSymlinks"], 4);
        assert_eq!(output.status()["bytes"], 7);
        assert!(f.files.check(&dst.join("alias"), false).is_err());
        assert_eq!(fs::read_link(dst.join("alias")).unwrap(), Path::new("file"));
    }
    #[cfg(unix)]
    #[test]
    fn invalid_links_fail_metadata_preflight() {
        let f = Fixture::new();
        let src = f.root.join("links");
        fs::create_dir(&src).unwrap();
        for target in ["../outside", "/tmp/outside", "missing", "bad"] {
            std::os::unix::fs::symlink(target, src.join("bad")).unwrap();
            assert!(source_manifest(&f.files, &src, true).is_err());
            fs::remove_file(src.join("bad")).unwrap();
        }
    }
    #[cfg(unix)]
    #[test]
    fn oversized_file_and_symlink_fail_metadata_preflight() {
        let f = Fixture::new();
        let path = f.root.join("sparse");
        File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(source_manifest(&f.files, &path, false)
            .unwrap_err()
            .contains("FILE_TOO_LARGE"));
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"a").unwrap();
        let link = f.root.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(source_manifest(&f.files, &link, false).is_err());
    }
}
