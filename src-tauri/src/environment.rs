//! Authenticated host context and a fixed, read-only WSL ownership challenge.
use crate::model::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn context() -> Value {
    let mut value = json!({"os":std::env::consts::OS});
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible},
            System::{
                Com::CoTaskMemFree,
                SystemInformation::{OSVERSIONINFOEXW, OSVERSIONINFOW},
            },
            UI::Shell::{FOLDERID_Profile, SHGetKnownFolderPath},
        };
        #[link(name = "ntdll")]
        extern "system" {
            fn RtlGetVersion(version: *mut OSVERSIONINFOW) -> i32;
        }
        let mut version: OSVERSIONINFOEXW = std::mem::zeroed();
        version.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOEXW>() as u32;
        if RtlGetVersion((&mut version as *mut OSVERSIONINFOEXW).cast()) >= 0 {
            value["os"] = json!(if version.wProductType != 1 {
                format!("Windows Server (build {})", version.dwBuildNumber)
            } else if version.dwMajorVersion == 10 {
                if version.dwBuildNumber >= 22000 {
                    "Windows 11".into()
                } else {
                    "Windows 10".into()
                }
            } else {
                format!(
                    "Windows {}.{}",
                    version.dwMajorVersion, version.dwMinorVersion
                )
            });
        }
        let mut name = vec![0u16; 512];
        let mut length = name.len() as u32;
        if GetUserNameExW(NameSamCompatible, name.as_mut_ptr(), &mut length) != 0 {
            value["user"] = json!(String::from_utf16_lossy(&name[..length as usize]));
        }
        let mut path = std::ptr::null_mut();
        // DONT_VERIFY reports the profile without accessing or creating it.
        if SHGetKnownFolderPath(&FOLDERID_Profile, 0x4000, std::ptr::null_mut(), &mut path) >= 0 {
            if !path.is_null() {
                let mut length = 0;
                while length < 32768 && *path.add(length) != 0 {
                    length += 1;
                }
                if length < 32768 {
                    value["home"] = json!(String::from_utf16_lossy(std::slice::from_raw_parts(
                        path, length
                    )));
                }
                CoTaskMemFree(path.cast());
            }
        }
    }
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") { value["home"] = json!(home); }
        value["user"] = json!(std::env::var("USER").unwrap_or_else(|_| format!("uid:{}", unsafe {libc::getuid()})));
    }
    #[cfg(target_os = "android")]
    { value["os"] = json!(format!("Android {}", crate::android::property("ro.build.version.release").unwrap_or_else(|| "unknown".into()))); }
    value["wslHostProbeVersion"] = json!(if cfg!(windows) { 1 } else { 0 });
    value
}

fn challenge_path(distro: &str, probe: &str) -> Result<PathBuf> {
    if probe.len() != 64
        || !probe.bytes().all(|c| c.is_ascii_hexdigit())
        || distro.is_empty()
        || distro.len() > 256
        || distro == "."
        || distro == ".."
        || distro
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
    {
        return Err("Invalid WSL host challenge".into());
    }
    Ok(PathBuf::from(format!(
        r"\\wsl.localhost\{distro}\tmp\maxa-device-host-{probe}"
    )))
}

fn challenge_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "WSL challenge unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Invalid WSL challenge file".into());
    }
    let file = std::fs::File::open(path).map_err(|_| "WSL challenge unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "WSL challenge unavailable")?
        .is_file()
    {
        return Err("Invalid WSL challenge file".into());
    }
    let mut bytes = Vec::new();
    file.take(65)
        .read_to_end(&mut bytes)
        .map_err(|_| "WSL challenge unavailable")?;
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err("Invalid WSL challenge file".into());
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub async fn probe(args: &Value) -> Result<Value> {
    let probe = args["probe"].as_str().ok_or("probe required")?;
    challenge_path("validation", probe)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").ok_or("Windows system directory unavailable")?;
        let mut command = std::process::Command::new(PathBuf::from(root).join("System32\\wsl.exe"));
        // Enumerate only running distributions; never invoke a command inside a guest.
        command
            .args(["--list", "--running", "--quiet"])
            .creation_flags(0x08000000);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::from(command)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "WSL enumeration timed out")?
        .map_err(|_| "WSL enumeration unavailable")?;
        if !output.status.success() || output.stdout.len() > 32768 {
            return Err("WSL enumeration unavailable".into());
        }
        let names = if output.stdout.contains(&0) {
            let words = output
                .stdout
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect::<Vec<_>>();
            String::from_utf16_lossy(&words)
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let mut paths = Vec::new();
        for name in names.trim_start_matches('\u{feff}').lines().take(64) {
            if let Ok(path) = challenge_path(name.trim(), probe) {
                paths.push(path);
            }
        }
        return tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::task::spawn_blocking(move || {
                let digests = paths
                    .iter()
                    .filter_map(|path| challenge_digest(path).ok())
                    .collect::<Vec<_>>();
                json!({"digests":digests})
            }),
        )
        .await
        .map_err(|_| "WSL challenge timed out")?
        .map_err(|_| "WSL challenge unavailable".into());
    }
    #[cfg(not(windows))]
    Err("WSL host probing requires Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn environment_status_and_probe_keep_authentication_and_module_barriers() {
        let (root, app, client, mut call) = crate::lifecycle_tests::fixture().await;
        call.operation = "device.status".into();
        let status = app.invoke(&client, &call).await.unwrap();
        assert_eq!(
            status["wslHostProbeVersion"],
            if cfg!(windows) { 1 } else { 0 }
        );
        assert!(status.get("desktop").is_none());
        assert!(!status["os"].as_str().unwrap().is_empty());
        #[cfg(windows)]
        for key in ["user", "home", "os"] {
            assert!(!status[key].as_str().unwrap().is_empty());
        }
        call.request_id = crate::model::id();
        call.operation = "environment.wsl_host".into();
        call.arguments = json!({"probe":"a".repeat(64)});
        assert!(app
            .invoke(&client, &call)
            .await
            .unwrap_err()
            .starts_with("MODULE_UNAVAILABLE"));
        app.revoke(&client.id).await.unwrap();
        call.request_id = crate::model::id();
        assert_eq!(
            app.invoke(&client, &call).await.unwrap_err(),
            "UNAUTHORIZED"
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn wsl_host_challenge_is_restricted_and_bounded() {
        let probe = "a".repeat(64);
        assert!(challenge_path("Ubuntu", &probe).is_ok());
        assert!(challenge_path("..\\other", &probe).is_err());
        assert!(challenge_path("Ubuntu", "../file").is_err());
        let path = std::env::temp_dir().join(crate::model::id());
        std::fs::write(&path, b"a".repeat(64)).unwrap();
        assert_eq!(
            challenge_digest(&path).unwrap(),
            crate::model::digest(&b"a".repeat(64))
        );
        std::fs::write(&path, b"a".repeat(65)).unwrap();
        assert!(challenge_digest(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}

/// Only publish a bundled file matching this binary's build-time companion manifest.
pub fn companion_apk(resources: &Path) -> Option<Value> {
    let manifest: Value=serde_json::from_str(option_env!("MAXA_COMPANION_MANIFEST")?).ok()?;
    verified_companion(resources,&manifest)
}
fn verified_companion(resources: &Path, manifest: &Value) -> Option<Value> {
    use sha2::{Digest,Sha256};
    use std::io::Read;
    let name=manifest["file"].as_str()?;
    let version=manifest["version"].as_str()?;
    let hash=manifest["sha256"].as_str()?;
    if version!=env!("CARGO_PKG_VERSION") || name!=format!("maxa-device-companion-{version}.apk") || hash.len()!=64 {return None;}
    let path=resources.join("android").join(name);
    if !path.is_absolute() {return None;}
    let meta=std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {return None;}
    let mut f=std::fs::File::open(&path).ok()?;let mut h=Sha256::new();let mut buf=[0;65536];
    loop {let n=f.read(&mut buf).ok()?;if n==0 {break;}h.update(&buf[..n]);}
    if format!("{:x}",h.finalize())!=hash {return None;}
    Some(json!({"path":path,"version":version,"sha256":hash}))
}
#[cfg(test)]
#[test]
fn companion_apk_reporting_requires_matching_bundled_bytes() {
    let root=std::env::temp_dir().join(crate::model::id());
    std::fs::create_dir_all(root.join("android")).unwrap();
    let version=env!("CARGO_PKG_VERSION");let file=format!("maxa-device-companion-{version}.apk");
    let manifest=json!({"file":file,"version":version,"sha256":crate::model::digest(b"test apk")});
    assert!(verified_companion(&root,&manifest).is_none());
    std::fs::write(root.join("android").join(&file),b"test apk").unwrap();
    let report=verified_companion(&root,&manifest).unwrap();assert_eq!(report["version"],version);assert_eq!(report["sha256"],manifest["sha256"]);assert!(Path::new(report["path"].as_str().unwrap()).is_absolute());
    std::fs::write(root.join("android").join(&file),b"tampered").unwrap();assert!(verified_companion(&root,&manifest).is_none());
    std::fs::remove_dir_all(root).unwrap();
}
