//! Visibility is not just WS_VISIBLE: virtual-desktop cloaking and occlusion
//! must not produce actionable Snapshot labels over another window's pixels.
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
    UI::WindowsAndMessaging::*,
};
pub fn window(hwnd: HWND) -> bool {
    unsafe {
        let mut cloaked = 0u32;
        let mut rect: RECT = std::mem::zeroed();
        IsWindow(hwnd) != 0
            && IsWindowVisible(hwnd) != 0
            && IsIconic(hwnd) == 0
            && GetWindowRect(hwnd, &mut rect) != 0
            && rect.right > rect.left
            && rect.bottom > rect.top
            && DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            ) >= 0
            && cloaked == 0
    }
}
pub fn point(hwnd: HWND, point: [i32; 2]) -> bool {
    window(hwnd)
        && unsafe {
            GetAncestor(
                WindowFromPoint(POINT {
                    x: point[0],
                    y: point[1],
                }),
                GA_ROOT,
            ) == hwnd
        }
}
