//! Internal interface table ported from Windows-MCP vdm/core.py (MIT).
//! No fallback to public MoveWindowToDesktop: that rejects other-process windows.
use serde_json::{json, Value};
use std::{ffi::c_void, ptr::null_mut};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::HWND,
        System::{Com::*, SystemInformation::OSVERSIONINFOW},
    },
};
fn guid(s: &str) -> GUID {
    let g = windows::core::GUID::try_from(s.trim_matches(['{', '}'])).expect("constant GUID");
    GUID {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    }
}
fn guid_text(g: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}
fn check(hr: i32) -> Result<(), String> {
    if hr < 0 {
        Err(format!(
            "UNSUPPORTED: virtual desktop COM call failed (0x{:08X})",
            hr as u32
        ))
    } else {
        Ok(())
    }
}
struct Com(*mut c_void);
impl Com {
    unsafe fn slot(&self, n: usize) -> *const c_void {
        *(*(self.0 as *const *const *const c_void)).add(n)
    }
    unsafe fn create(class: &str, iid: &str) -> Result<Self, String> {
        let mut p = null_mut();
        check(CoCreateInstance(
            &guid(class),
            null_mut(),
            CLSCTX_LOCAL_SERVER | CLSCTX_INPROC_SERVER,
            &guid(iid),
            &mut p,
        ))?;
        if p.is_null() {
            return Err("UNSUPPORTED: null COM interface".into());
        }
        Ok(Self(p))
    }
    unsafe fn service(&self, sid: &str, iid: &str) -> Result<Self, String> {
        let mut p = null_mut();
        let f: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            *const GUID,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(self.slot(3));
        check(f(self.0, &guid(sid), &guid(iid), &mut p))?;
        if p.is_null() {
            return Err("UNSUPPORTED: null COM service".into());
        }
        Ok(Self(p))
    }
    unsafe fn out(&self, slot: usize) -> Result<Self, String> {
        let mut p = null_mut();
        let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 =
            std::mem::transmute(self.slot(slot));
        check(f(self.0, &mut p))?;
        if p.is_null() {
            return Err("UNSUPPORTED: null desktop interface".into());
        }
        Ok(Self(p))
    }
    unsafe fn id(&self) -> Result<GUID, String> {
        let mut g: GUID = std::mem::zeroed();
        let f: unsafe extern "system" fn(*mut c_void, *mut GUID) -> i32 =
            std::mem::transmute(self.slot(4));
        check(f(self.0, &mut g))?;
        Ok(g)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(self.slot(2));
            release(self.0);
        }
    }
}
#[link(name = "ntdll")]
extern "system" {
    fn RtlGetVersion(version: *mut OSVERSIONINFOW) -> i32;
}
pub fn execute(mode: &str, hwnd: Option<HWND>, requested: Option<&str>) -> Result<Value, String> {
    unsafe {
        let mut version: OSVERSIONINFOW = std::mem::zeroed();
        version.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        check(RtlGetVersion(&mut version))?;
        let (internal_iid, desktop_iid) = match version.dwBuildNumber {
            26100.. => (
                "{53F5CA0B-158F-4124-900C-057158060B27}",
                "{3F07F4BE-B107-441A-AF0F-39D82529072C}",
            ),
            22621..=26099 => (
                "{A3175F2D-239C-4BD2-8AA0-EEBA8B0B138E}",
                "{3F07F4BE-B107-441A-AF0F-39D82529072C}",
            ),
            19041..=21999 => (
                "{F31574D6-B682-4CDC-BD56-1827860ABEC6}",
                "{FF72FFDD-BE7E-43FC-9C03-AD81681E88E4}",
            ),
            _ => {
                return Err(format!(
                    "UNSUPPORTED: virtual desktop Windows build {}",
                    version.dwBuildNumber
                ))
            }
        };
        let shell = Com::create(
            "{C2F03A33-21F5-47FA-B4BB-156362A2F239}",
            "{6D5140C1-7436-11CE-8034-00AA006009FA}",
        )?;
        let internal = shell.service("{C5E0CDCA-7B6E-41B2-9FC4-D93975CC467B}", internal_iid)?;
        let desktops = internal.out(7)?;
        let mut count = 0;
        let get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 =
            std::mem::transmute(desktops.slot(3));
        check(get_count(desktops.0, &mut count))?;
        let mut choices = Vec::new();
        let mut target = None;
        for index in 0..count {
            let mut p = null_mut();
            let get: unsafe extern "system" fn(
                *mut c_void,
                u32,
                *const GUID,
                *mut *mut c_void,
            ) -> i32 = std::mem::transmute(desktops.slot(4));
            check(get(desktops.0, index, &guid(desktop_iid), &mut p))?;
            if p.is_null() {
                return Err("UNSUPPORTED: null enumerated desktop".into());
            }
            let desktop = Com(p);
            let id = guid_text(&desktop.id()?);
            let name = desktop_name(&id).unwrap_or_else(|| format!("Desktop {}", index + 1));
            choices.push(json!({"id":id,"name":name}));
            if requested.is_some_and(|r| r.eq_ignore_ascii_case(&id)) {
                target = Some(desktop);
            }
        }
        if mode == "desktops" {
            return Ok(json!({"desktops":choices,"currentId":guid_text(&internal.out(6)?.id()?)}));
        }
        let target = target.ok_or("Unknown virtual desktop GUID")?;
        if mode == "switch_desktop" {
            let switch: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32 =
                std::mem::transmute(internal.slot(9));
            check(switch(internal.0, target.0))?;
            let actual = guid_text(&internal.out(6)?.id()?);
            if !requested.is_some_and(|r| r.eq_ignore_ascii_case(&actual)) {
                return Err("Virtual desktop switch was not confirmed".into());
            }
            return Ok(json!({"currentId":actual}));
        }
        let hwnd = hwnd.ok_or("hwnd required")?;
        let views = shell.service(
            "{1841C6D7-4F9D-42C0-AF41-8747538F10E5}",
            "{1841C6D7-4F9D-42C0-AF41-8747538F10E5}",
        )?;
        let mut view = null_mut();
        let get_view: unsafe extern "system" fn(*mut c_void, HWND, *mut *mut c_void) -> i32 =
            std::mem::transmute(views.slot(6));
        check(get_view(views.0, hwnd, &mut view))?;
        if view.is_null() {
            return Err("UNSUPPORTED: no application view for window".into());
        }
        let view = Com(view);
        let public = Com::create(
            "{AA509086-5CA9-4C25-8F95-589D3C07B48A}",
            "{A5CD92FF-29BE-454C-8D04-D82879FB3F1B}",
        )?;
        let move_view: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
            std::mem::transmute(internal.slot(4));
        check(move_view(internal.0, view.0, target.0))
            .map_err(|e| format!("OUTCOME_UNKNOWN: {e}; do not replay"))?;
        let mut actual: GUID = std::mem::zeroed();
        let get_id: unsafe extern "system" fn(*mut c_void, HWND, *mut GUID) -> i32 =
            std::mem::transmute(public.slot(4));
        check(get_id(public.0, hwnd, &mut actual)).map_err(|e| {
            format!("OUTCOME_UNKNOWN: {e}; desktop move may have occurred; do not replay")
        })?;
        let actual = guid_text(&actual);
        if !requested.is_some_and(|r| r.eq_ignore_ascii_case(&actual)) {
            return Err("Window desktop move was not confirmed".into());
        }
        Ok(json!({"desktopId":actual}))
    }
}
unsafe fn desktop_name(id: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::*;
    let path: Vec<u16> = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VirtualDesktops\\Desktops\\{id}\0"
    )
    .encode_utf16()
    .collect();
    let name: Vec<u16> = "Name\0".encode_utf16().collect();
    let mut buffer = [0u16; 256];
    let mut size = (buffer.len() * 2) as u32;
    if RegGetValueW(
        HKEY_CURRENT_USER,
        path.as_ptr(),
        name.as_ptr(),
        RRF_RT_REG_SZ,
        std::ptr::null_mut(),
        buffer.as_mut_ptr() as *mut _,
        &mut size,
    ) == 0
    {
        Some(String::from_utf16_lossy(
            &buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())],
        ))
    } else {
        None
    }
}
