//! Non-activating, input-transparent per-monitor indicator. GDI resources are thread-owned.
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Layer {
    window: HWND,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
pub struct Indicator {
    layers: Vec<Layer>,
    pub excluded: bool,
}
impl Indicator {
    pub unsafe fn show(chinese: bool) -> Result<Self, String> {
        let title = crate::preferences::native_text(chinese, "AI 控制中");
        let hint =
            crate::preferences::native_text(chinese, "移动鼠标或按 Ctrl+Alt+Shift+Backspace 接管");
        let class = wide("MaxaAIControlIndicator");
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let mut monitors: Vec<RECT> = vec![];
        unsafe extern "system" fn collect(
            _: HMONITOR,
            _: HDC,
            rect: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            (*(data as *mut Vec<RECT>)).push(*rect);
            1
        }
        if EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(collect),
            &mut monitors as *mut _ as isize,
        ) == 0
            || monitors.is_empty()
        {
            return Err("AI indicator monitor enumeration failed".into());
        }
        let mut result = Self {
            layers: vec![],
            excluded: true,
        };
        for rect in monitors {
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                wide(&title).as_ptr(),
                WS_POPUP,
                rect.left,
                rect.top,
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return Err("AI indicator window creation failed".into());
            }
            let dc = CreateCompatibleDC(null_mut());
            let mut bits: *mut std::ffi::c_void = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..std::mem::zeroed()
                },
                ..std::mem::zeroed()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if dc.is_null() || bitmap.is_null() || bits.is_null() {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                DestroyWindow(hwnd);
                return Err("AI indicator bitmap creation failed".into());
            }
            let old = SelectObject(dc, bitmap);
            result.layers.push(Layer {
                window: hwnd,
                dc,
                bitmap,
                previous: old,
            });
            let dpi = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
            let px = |v: f64| (v * dpi).round() as i32;
            let bold = CreateFontW(
                -px(13.0),
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Microsoft YaHei UI").as_ptr(),
            );
            let regular = CreateFontW(
                -px(13.0),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Microsoft YaHei UI").as_ptr(),
            );
            let first = wide(&title);
            let second = wide(&format!("  {hint}"));
            let mut a: SIZE = std::mem::zeroed();
            let mut b: SIZE = std::mem::zeroed();
            let old_font = SelectObject(dc, bold);
            GetTextExtentPoint32W(dc, first.as_ptr(), (first.len() - 1) as i32, &mut a);
            SelectObject(dc, regular);
            GetTextExtentPoint32W(dc, second.as_ptr(), (second.len() - 1) as i32, &mut b);
            let badge_w = a.cx + b.cx + px(28.0);
            let badge_h = px(30.0);
            let left = (width - badge_w) / 2;
            let top = px(12.0);
            let radius = px(8.0);
            let border = px(3.0).max(1);
            let outline = px(1.0).max(1);
            let pixels = std::slice::from_raw_parts_mut(
                bits as *mut u32,
                (width as usize) * (height as usize),
            );
            pixels.fill(0);
            for y in 0..height {
                for x in 0..width {
                    let mut color = if x < border
                        || x >= width - border
                        || y < border
                        || y >= height - border
                    {
                        0xe6ff7a1a
                    } else {
                        0
                    };
                    if rounded(x - left, y - top, badge_w, badge_h, radius) {
                        color = if rounded(
                            x - left - outline,
                            y - top - outline,
                            badge_w - 2 * outline,
                            badge_h - 2 * outline,
                            (radius - outline).max(0),
                        ) {
                            0xebff7a1a
                        } else {
                            0xebc2500a
                        };
                    }
                    pixels[(y * width + x) as usize] = color;
                }
            }
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00ffffff);
            SelectObject(dc, bold);
            TextOutW(
                dc,
                left + px(14.0),
                top + (badge_h - a.cy) / 2,
                first.as_ptr(),
                (first.len() - 1) as i32,
            );
            SelectObject(dc, regular);
            TextOutW(
                dc,
                left + px(14.0) + a.cx,
                top + (badge_h - b.cy) / 2,
                second.as_ptr(),
                (second.len() - 1) as i32,
            );
            SelectObject(dc, old_font);
            DeleteObject(bold);
            DeleteObject(regular);
            // GDI text has no alpha. Derive its coverage against the orange backing.
            for pixel in pixels.iter_mut() {
                let mut alpha = *pixel >> 24;
                if alpha == 0 && *pixel & 0xffffff != 0 {
                    alpha = 255;
                }
                let r = (*pixel >> 16) & 255;
                let g = (*pixel >> 8) & 255;
                let b = *pixel & 255;
                *pixel = (alpha << 24)
                    | ((r * alpha / 255) << 16)
                    | ((g * alpha / 255) << 8)
                    | (b * alpha / 255);
            }
            let dest = POINT {
                x: rect.left,
                y: rect.top,
            };
            let size = SIZE {
                cx: width,
                cy: height,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if UpdateLayeredWindow(
                hwnd,
                null_mut(),
                &dest,
                &size,
                dc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            ) == 0
            {
                return Err("AI indicator compositing failed".into());
            }
            if SetWindowDisplayAffinity(hwnd, 0x11) == 0 {
                result.excluded = false;
            }
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        if !result.visible() {
            return Err("AI indicator failed to become visible".into());
        }
        Ok(result)
    }
    pub unsafe fn visible(&self) -> bool {
        !self.layers.is_empty()
            && self
                .layers
                .iter()
                .all(|l| IsWindow(l.window) != 0 && IsWindowVisible(l.window) != 0)
    }
    pub unsafe fn hide(&self) {
        for l in &self.layers {
            ShowWindow(l.window, SW_HIDE);
        }
    }
    pub unsafe fn reveal(&self) {
        for l in &self.layers {
            ShowWindow(l.window, SW_SHOWNOACTIVATE);
        }
    }
}
fn rounded(x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
    if x < 0 || y < 0 || x >= w || y >= h {
        return false;
    }
    let cx = if x < r {
        r
    } else if x >= w - r {
        w - r - 1
    } else {
        x
    };
    let cy = if y < r {
        r
    } else if y >= h - r {
        h - r - 1
    } else {
        y
    };
    (x - cx).pow(2) + (y - cy).pow(2) <= r * r
}
impl Drop for Indicator {
    fn drop(&mut self) {
        unsafe {
            for l in &self.layers {
                ShowWindow(l.window, SW_HIDE);
                DestroyWindow(l.window);
                SelectObject(l.dc, l.previous);
                DeleteObject(l.bitmap);
                DeleteDC(l.dc);
            }
        }
    }
}
