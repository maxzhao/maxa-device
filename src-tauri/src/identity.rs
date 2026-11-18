use crate::model::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use rcgen::{CertificateParams, KeyPair};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Serialize, Deserialize)]
pub struct HostIdentity {
    pub public: Identity,
    pub certificate: String,
    pub private_key: String,
}
#[cfg(windows)]
fn protect(data: &[u8], decrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        if decrypt {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as _);
    };
    Ok(bytes)
}
// Termux isolates identity through the private directory and mode 0600.
#[cfg(not(windows))]
fn protect(data: &[u8], _: bool) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let tmp = path.with_extension(format!("{}.tmp", id()));
    let mut f = opts.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}
impl HostIdentity {
    pub fn load(dir: &Path) -> Result<Self> {
        let file = dir.join("identity.dpapi");
        if file.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = fs::symlink_metadata(&file).map_err(|e| e.to_string())?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err("Private identity must be a regular private file".into());
                }
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())?;
            }
            return serde_json::from_slice(&protect(
                &fs::read(file).map_err(|e| e.to_string())?,
                true,
            )?)
            .map_err(|e| e.to_string());
        }
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
            .map_err(|e| e.to_string())?;
        let certificate = params.self_signed(&key).map_err(|e| e.to_string())?;
        use sha2::Digest;
        let pin = format!(
            "sha256//{}",
            STANDARD.encode(sha2::Sha256::digest(key.public_key_der()))
        );
        let result = Self {
            public: Identity {
                host_id: id(),
                display_name: {
                    #[cfg(target_os = "android")]
                    {
                        android_display_name(dir, None)?
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Maxa Device".into())
                    }
                },
                public_key_pin: pin,
            },
            certificate: certificate.pem(),
            private_key: key.serialize_pem(),
        };
        private_write(
            &file,
            &protect(&serde_json::to_vec(&result).unwrap(), false)?,
        )?;
        Ok(result)
    }
}

#[cfg(any(target_os = "android", test))]
fn checked_name(name: &str) -> Option<&str> {
    let name = name.trim();
    // Leave room for the mDNS TXT key and preserve Unicode device names.
    (!name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)).then_some(name)
}
#[cfg(any(target_os = "android", test))]
fn select_display_name<'a>(
    override_name: Option<&'a str>,
    companion: Option<&'a str>,
    persisted: Option<&'a str>,
    market: Option<&'a str>,
    model: Option<&'a str>,
) -> &'a str {
    [override_name, companion, persisted, market, model]
        .into_iter()
        .flatten()
        .find_map(checked_name)
        .unwrap_or("Android")
}
#[cfg(any(target_os = "android", test))]
pub fn android_display_name(dir: &Path, companion: Option<&str>) -> Result<String> {
    let path = dir.join("display-name.json");
    let override_name: Option<String> = match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err("Display name override must be a regular private file".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())?;
            }
            Some(
                serde_json::from_slice::<String>(&fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|_| "Invalid display name override")?,
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let persisted = crate::android::property("persist.sys.device_name");
    let market = crate::android::property("ro.product.marketname");
    let model = crate::android::property("ro.product.model");
    Ok(select_display_name(
        override_name.as_deref(),
        companion,
        persisted.as_deref(),
        market.as_deref(),
        model.as_deref(),
    )
    .into())
}
#[cfg(any(target_os = "android", test))]
pub fn save_display_name(dir: &Path, name: Option<&str>) -> Result<()> {
    let path = dir.join("display-name.json");
    match name {
        Some(name) => {
            let name = checked_name(name).ok_or(
                "Name must be nonblank, at most 128 UTF-8 bytes and contain no control characters",
            )?;
            private_write(&path, &serde_json::to_vec(name).map_err(|e| e.to_string())?)
        }
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn android_display_name_priority_and_private_override() {
        assert_eq!(
            select_display_name(
                Some("Local"),
                Some("Companion"),
                Some("Persistent"),
                Some("Market"),
                Some("Model")
            ),
            "Local"
        );
        assert_eq!(
            select_display_name(
                None,
                Some("平板"),
                Some("Persistent"),
                Some("Market"),
                Some("Model")
            ),
            "平板"
        );
        assert_eq!(
            select_display_name(
                None,
                Some(" "),
                Some("Persistent"),
                Some("Market"),
                Some("Model")
            ),
            "Persistent"
        );
        assert_eq!(
            select_display_name(None, None, None, None, Some("Model")),
            "Model"
        );
        assert_eq!(
            select_display_name(None, None, Some(" "), Some("Market"), Some("Model")),
            "Market"
        );
        let dir = std::env::temp_dir().join(id());
        fs::create_dir(&dir).unwrap();
        save_display_name(&dir, Some("  我的平板  ")).unwrap();
        assert_eq!(
            android_display_name(&dir, Some("Companion")).unwrap(),
            "我的平板"
        );
        assert!(save_display_name(&dir, Some("bad\nname")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("display-name.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        save_display_name(&dir, None).unwrap();
        assert_eq!(
            android_display_name(&dir, Some("Companion")).unwrap(),
            "Companion"
        );
        assert!(!dir.join("display-name.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
