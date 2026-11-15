//! No desktop switching or permission bypass: input is confined to the active console's Default desktop.
use windows_sys::Win32::System::{
    RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId},
    StationsAndDesktops::*,
    Threading::GetCurrentThreadId,
};

unsafe fn is_default(desktop: HDESK) -> bool {
    if desktop.is_null() {
        return false;
    }
    let mut name = [0u16; 256];
    let mut needed = 0;
    GetUserObjectInformationW(
        desktop,
        UOI_NAME,
        name.as_mut_ptr().cast(),
        std::mem::size_of_val(&name) as u32,
        &mut needed,
    ) != 0
        && String::from_utf16_lossy(
            &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
        )
        .eq_ignore_ascii_case("Default")
}
pub fn current() -> Result<u32, String> {
    unsafe {
        let mut session = 0;
        let console = WTSGetActiveConsoleSessionId();
        if console == u32::MAX
            || ProcessIdToSessionId(std::process::id(), &mut session) == 0
            || session != console
        {
            return Err(
                "INTERACTIVE_DESKTOP_REQUIRED: active console session changed or unavailable"
                    .into(),
            );
        }
        let input = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if input.is_null() {
            return Err("INTERACTIVE_DESKTOP_REQUIRED: input desktop unavailable (lock/UAC/secure desktop); no input permitted".into());
        }
        let default = is_default(input) && is_default(GetThreadDesktop(GetCurrentThreadId()));
        CloseDesktop(input);
        if !default {
            return Err("INTERACTIVE_DESKTOP_REQUIRED: input desktop is not Default (lock/UAC/secure desktop)".into());
        }
        Ok(session)
    }
}
pub fn verify(expected: u32) -> Result<(), String> {
    if current()? != expected {
        return Err(
            "INTERACTIVE_DESKTOP_REQUIRED: console session changed during operation".into(),
        );
    }
    Ok(())
}
