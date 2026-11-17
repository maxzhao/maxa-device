//! Local human preferences; runtime shutdown never changes module selections.
use crate::model::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    #[default]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    En,
}
impl Language {
    pub fn chinese(self) -> bool {
        match self {
            Self::ZhCn => true,
            Self::En => false,
            Self::System => {
                #[cfg(windows)]
                {
                    unsafe {
                        windows_sys::Win32::Globalization::GetUserDefaultUILanguage() & 0x3ff == 4
                    }
                }
                #[cfg(not(windows))]
                {
                    std::env::var("LANG").unwrap_or_default().starts_with("zh")
                }
            }
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Appearance {
    pub start_minimized: bool,
    pub theme: Theme,
    pub language: Language,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            start_minimized: true,
            theme: Theme::System,
            language: Language::System,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preferences {
    pub modules: BTreeMap<String, bool>,
    pub appearance: Appearance,
    #[serde(default)]
    pub copy_symlinks: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            modules: ["system", "windows", "mobile"]
                .into_iter()
                .map(|m| (m.into(), false))
                .collect(),
            appearance: Appearance::default(),
            copy_symlinks: false,
        }
    }
}
impl Preferences {
    pub fn validate(&self) -> Result<()> {
        if self.modules.len() != 3
            || !["system", "windows", "mobile"]
                .iter()
                .all(|m| self.modules.contains_key(*m))
        {
            return Err("Invalid saved module preferences".into());
        }
        Ok(())
    }
}

pub fn native_text(chinese: bool, key: &str) -> String {
    static EN: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../src/locales/en.json"))
            .expect("embedded English messages")
    });
    if chinese {
        key.into()
    } else {
        EN.get(key).and_then(|v| v.as_str()).unwrap_or(key).into()
    }
}

pub mod symlinks {
    use super::*;
    pub fn configured() -> Result<()> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Registry::*;
            let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let mut value = 0u32;
            let mut size = 4u32;
            let code = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    wide("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\AppModelUnlock").as_ptr(),
                    wide("AllowDevelopmentWithoutDevLicense").as_ptr(),
                    RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
                    std::ptr::null_mut(),
                    &mut value as *mut u32 as *mut _,
                    &mut size,
                )
            };
            if code != 0 && code != 2 {
                return Err(format!("Cannot read Windows developer mode: {code}"));
            }
            if code == 0 && value == 1 {
                return Ok(());
            }
            Err("请先在 Windows 设置中开启开发者模式，再重试。".into())
        }
        #[cfg(not(windows))]
        Err("Windows developer mode is unavailable on this host".into())
    }
    pub fn verify() -> Result<()> {
        configured()?;
        #[cfg(windows)]
        {
            let dir =
                std::env::temp_dir().join(format!("maxa-symlink-check-{}", crate::model::id()));
            std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
            let result = (|| {
                std::fs::write(dir.join("target"), b"").map_err(|e| e.to_string())?;
                std::os::windows::fs::symlink_file("target", dir.join("link"))
                    .map_err(|e| e.to_string())
            })();
            let cleanup = std::fs::remove_dir_all(&dir).map_err(|e| e.to_string());
            result.and(cleanup)
        }
        #[cfg(not(windows))]
        Ok(())
    }
}

#[cfg(windows)]
pub mod autostart {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::System::Registry::*;
    const RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const APPROVED: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
    const NAME: &str = "Maxa Device";
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn open(path: &str, access: u32) -> Result<Option<Key>> {
        let mut key = null_mut();
        let code =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(path).as_ptr(), 0, access, &mut key) };
        match code {
            0 => Ok(Some(Key(key))),
            2 => Ok(None),
            _ => Err(format!("Windows startup registry: {code}")),
        }
    }
    fn value(path: &str) -> Result<Option<(u32, Vec<u8>)>> {
        let Some(key) = open(path, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let mut kind = 0;
        let mut size = 0;
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                wide(NAME).as_ptr(),
                null(),
                &mut kind,
                null_mut(),
                &mut size,
            )
        };
        if code == 2 {
            return Ok(None);
        }
        if code != 0 {
            return Err(format!("Windows startup query: {code}"));
        }
        let mut bytes = vec![0; size as usize];
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                wide(NAME).as_ptr(),
                null(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut size,
            )
        };
        if code != 0 {
            return Err(format!("Windows startup query: {code}"));
        }
        bytes.truncate(size as usize);
        Ok(Some((kind, bytes)))
    }
    pub fn enabled() -> Result<bool> {
        let Some((kind, bytes)) = value(RUN)? else {
            return Ok(false);
        };
        let expected = wide(&format!(
            "\"{}\"",
            std::env::current_exe()
                .map_err(|e| e.to_string())?
                .display()
        ));
        let expected: Vec<u8> = expected.iter().flat_map(|c| c.to_le_bytes()).collect();
        if kind != REG_SZ || bytes != expected {
            return Ok(false);
        }
        // Task Manager may disable a retained Run entry. Never override it on launch.
        Ok(!value(APPROVED)?.is_some_and(|(_, b)| b.first().is_some_and(|v| v & 1 == 1)))
    }
    pub fn set(enabled: bool) -> Result<()> {
        if enabled {
            let mut key = null_mut();
            let code = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(RUN).as_ptr(),
                    0,
                    null(),
                    0,
                    KEY_SET_VALUE,
                    null(),
                    &mut key,
                    null_mut(),
                )
            };
            if code != 0 {
                return Err(format!("Windows startup registration: {code}"));
            }
            let key = Key(key);
            let text = wide(&format!(
                "\"{}\"",
                std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .display()
            ));
            let code = unsafe {
                RegSetValueExW(
                    key.0,
                    wide(NAME).as_ptr(),
                    0,
                    REG_SZ,
                    text.as_ptr() as *const u8,
                    (text.len() * 2) as u32,
                )
            };
            if code != 0 {
                return Err(format!("Windows startup registration: {code}"));
            }
        } else if let Some(key) = open(RUN, KEY_SET_VALUE)? {
            let code = unsafe { RegDeleteValueW(key.0, wide(NAME).as_ptr()) };
            if code != 0 && code != 2 {
                return Err(format!("Windows startup removal: {code}"));
            }
        }
        // Only an explicit local settings action clears this App's external disable marker.
        if let Some(key) = open(APPROVED, KEY_SET_VALUE)? {
            let code = unsafe { RegDeleteValueW(key.0, wide(NAME).as_ptr()) };
            if code != 0 && code != 2 {
                return Err(format!("Windows startup approval: {code}"));
            }
        }
        if self::enabled()? != enabled {
            return Err("Windows startup setting was not applied".into());
        }
        Ok(())
    }
}
