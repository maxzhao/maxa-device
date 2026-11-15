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
// Non-Windows builds validate the pure backend, not a supported host distribution.
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
                display_name: std::env::var("COMPUTERNAME")
                    .unwrap_or_else(|_| "Maxa Device".into()),
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
