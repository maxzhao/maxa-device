use std::{ptr::null_mut, time::Duration};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*},
    UI::WindowsAndMessaging::*,
};
const UNICODE: u32 = 13;
struct Open(HWND);
impl Open {
    fn new() -> Result<Self, String> {
        unsafe {
            let class: Vec<u16> = "MaxaClipboardOwner\0".encode_utf16().collect();
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                std::ptr::null(),
            );
            if window.is_null() {
                return Err("Clipboard owner creation failed".into());
            }
            for _ in 0..20 {
                if OpenClipboard(window) != 0 {
                    return Ok(Self(window));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            DestroyWindow(window);
        }
        Err("Clipboard busy; no clipboard change made".into())
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
            DestroyWindow(self.0);
        }
    }
}
unsafe fn bytes(format: u32) -> Result<Option<Vec<u8>>, String> {
    let handle = GetClipboardData(format);
    if handle.is_null() {
        return Ok(None);
    }
    let size = GlobalSize(handle);
    if size == 0 {
        return Err(format!(
            "Clipboard format {format} is not an in-memory format; refusing lossy replacement"
        ));
    }
    let p = GlobalLock(handle);
    if p.is_null() {
        return Err("Clipboard memory is unavailable".into());
    }
    let result = std::slice::from_raw_parts(p as *const u8, size).to_vec();
    GlobalUnlock(handle);
    Ok(Some(result))
}
unsafe fn text_open() -> Result<Option<String>, String> {
    Ok(bytes(UNICODE)?.map(|b| {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|v| *v != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }))
}
unsafe fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if handle.is_null() {
        return Err("Clipboard allocation failed".into());
    }
    let p = GlobalLock(handle);
    if p.is_null() {
        GlobalFree(handle);
        return Err("Clipboard allocation lock failed".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    GlobalUnlock(handle);
    if SetClipboardData(format, handle).is_null() {
        GlobalFree(handle);
        return Err("Clipboard write failed".into());
    }
    Ok(())
}
fn encoded(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
pub fn get() -> Result<Option<String>, String> {
    let _open = Open::new()?;
    unsafe { text_open() }
}
pub fn set(text: &str) -> Result<(), String> {
    let _open = Open::new()?;
    unsafe {
        if EmptyClipboard() == 0 {
            return Err("Clipboard clear failed".into());
        }
        put(UNICODE, &encoded(text))
    }
}
pub struct Snapshot(Vec<(u32, Vec<u8>)>);
impl Snapshot {
    pub fn save_and_set(text: &str) -> Result<Self, String> {
        let _open = Open::new()?;
        unsafe {
            let mut formats = vec![];
            let mut id = 0;
            loop {
                id = EnumClipboardFormats(id);
                if id == 0 {
                    break;
                }
                let data = bytes(id)?.ok_or("Clipboard format disappeared")?;
                formats.push((id, data));
            }
            if EmptyClipboard() == 0 {
                return Err("Clipboard clear failed".into());
            }
            if let Err(error) = put(UNICODE, &encoded(text)) {
                for (f, b) in &formats {
                    let _ = put(*f, b);
                }
                return Err(error);
            }
            Ok(Self(formats))
        }
    }
    pub fn restore_if_unchanged(self, text: &str) -> Result<(), String> {
        let _open = Open::new()?;
        unsafe {
            if text_open()?.as_deref() != Some(text) {
                return Ok(());
            }
            if EmptyClipboard() == 0 {
                return Err("OUTCOME_UNKNOWN: clipboard restoration failed".into());
            }
            for (f, b) in self.0 {
                put(f, &b)
                    .map_err(|e| format!("OUTCOME_UNKNOWN: clipboard restoration failed: {e}"))?;
            }
            Ok(())
        }
    }
}
