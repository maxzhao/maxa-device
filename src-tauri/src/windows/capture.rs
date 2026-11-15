//! Native visible-crop capture and rendering. Ported from Windows-MCP (MIT).
use super::{
    control::Coordinator,
    tree::{Element, Tree},
};
use image::{
    imageops::{resize, FilterType},
    RgbaImage,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*, Graphics::Gdi::*, System::SystemInformation::GetLocalTime,
    UI::WindowsAndMessaging::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
impl Rect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }
    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
    pub fn center(self) -> [i32; 2] {
        [self.left + self.width() / 2, self.top + self.height() / 2]
    }
    pub fn intersect(self, b: Self) -> Option<Self> {
        let r = Self {
            left: self.left.max(b.left),
            top: self.top.max(b.top),
            right: self.right.min(b.right),
            bottom: self.bottom.min(b.bottom),
        };
        if r.right > r.left && r.bottom > r.top {
            Some(r)
        } else {
            None
        }
    }
    pub fn contains(self, p: [i32; 2]) -> bool {
        p[0] >= self.left && p[0] < self.right && p[1] >= self.top && p[1] < self.bottom
    }
    pub fn xyxy(self) -> String {
        format!(
            "({},{},{},{})",
            self.left, self.top, self.right, self.bottom
        )
    }
}
impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}
pub struct Display {
    pub index: usize,
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}
pub fn displays() -> Result<Vec<Display>, String> {
    unsafe {
        unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, p: LPARAM) -> BOOL {
            let mut info: MONITORINFOEXW = std::mem::zeroed();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO) != 0 {
                let list = &mut *(p as *mut Vec<Display>);
                let len = info
                    .szDevice
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(info.szDevice.len());
                list.push(Display {
                    index: list.len(),
                    name: String::from_utf16_lossy(&info.szDevice[..len]),
                    rect: info.monitorInfo.rcMonitor.into(),
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
            }
            1
        }
        let mut list = vec![];
        if EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(collect),
            &mut list as *mut _ as isize,
        ) == 0
            || list.is_empty()
        {
            return Err("No displays detected".into());
        }
        Ok(list)
    }
}
fn array(v: &serde_json::Value) -> Result<Vec<i32>, String> {
    let parsed;
    if let Some(s) = v.as_str() {
        parsed = serde_json::from_str(s).map_err(|_| "Expected a JSON array")?;
    } else {
        parsed = v.clone();
    }
    parsed
        .as_array()
        .ok_or("Expected a JSON array")?
        .iter()
        .map(|v| {
            v.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("Array must contain integers".into())
        })
        .collect()
}
pub struct Geometry {
    pub rect: Rect,
    pub selected: Vec<usize>,
    pub filtered: bool,
}
pub fn geometry(a: &serde_json::Value, displays: &[Display]) -> Result<Geometry, String> {
    let full = Rect {
        left: displays.iter().map(|d| d.rect.left).min().unwrap(),
        top: displays.iter().map(|d| d.rect.top).min().unwrap(),
        right: displays.iter().map(|d| d.rect.right).max().unwrap(),
        bottom: displays.iter().map(|d| d.rect.bottom).max().unwrap(),
    };
    let selected = if a["display"].is_null() {
        vec![]
    } else {
        let mut out = vec![];
        for i in array(&a["display"])? {
            if i < 0 {
                return Err("display must contain zero-based active display indices".into());
            }
            if !out.contains(&(i as usize)) {
                out.push(i as usize);
            }
        }
        out
    };
    // Region takes precedence, including over an invalid display selection.
    let rect = if !a["region"].is_null() {
        let r = array(&a["region"])?;
        if r.len() != 4 {
            return Err("region must be [left,top,right,bottom]".into());
        }
        let r = Rect {
            left: r[0],
            top: r[1],
            right: r[2],
            bottom: r[3],
        };
        if r.right <= r.left || r.bottom <= r.top {
            return Err("region must satisfy right > left and bottom > top".into());
        }
        if r.intersect(full).is_none() {
            return Err("region does not overlap the virtual screen bounds".into());
        }
        r
    } else if !selected.is_empty() {
        let chosen: Vec<_> = selected
            .iter()
            .map(|i| {
                displays
                    .get(*i)
                    .ok_or_else(|| format!("Invalid display index {i}"))
            })
            .collect::<Result<_, _>>()?;
        Rect {
            left: chosen.iter().map(|d| d.rect.left).min().unwrap(),
            top: chosen.iter().map(|d| d.rect.top).min().unwrap(),
            right: chosen.iter().map(|d| d.rect.right).max().unwrap(),
            bottom: chosen.iter().map(|d| d.rect.bottom).max().unwrap(),
        }
    } else {
        full
    };
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    if width <= 0 || height <= 0 || width > 32768 || height > 32768 || width * height > 64_000_000 {
        return Err("Capture geometry is too large".into());
    }
    Ok(Geometry {
        rect,
        filtered: !a["region"].is_null() || !selected.is_empty(),
        selected,
    })
}
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}
struct Surface {
    screen: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}
impl Surface {
    unsafe fn new(r: Rect) -> Result<Self, String> {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err("Interactive screen DC unavailable".into());
        }
        let dc = CreateCompatibleDC(screen);
        if dc.is_null() {
            ReleaseDC(null_mut(), screen);
            return Err("Capture DC unavailable".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.width(),
                biHeight: -r.height(),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            DeleteDC(dc);
            ReleaseDC(null_mut(), screen);
            return Err("Capture bitmap allocation failed".into());
        }
        let old = SelectObject(dc, bitmap);
        let surface = Self {
            screen,
            dc,
            bitmap,
            old,
            bits: bits as *mut u8,
            width: r.width(),
            height: r.height(),
        };
        DwmFlush();
        if BitBlt(
            dc,
            0,
            0,
            r.width(),
            r.height(),
            screen,
            r.left,
            r.top,
            SRCCOPY | CAPTUREBLT,
        ) == 0
        {
            return Err("Visible-crop screen capture failed".into());
        }
        Ok(surface)
    }
    unsafe fn line(&self, a: [i32; 2], b: [i32; 2], color: u32, width: i32) {
        let pen = CreatePen(PS_SOLID, width, color);
        let old = SelectObject(self.dc, pen);
        MoveToEx(self.dc, a[0], a[1], null_mut());
        LineTo(self.dc, b[0], b[1]);
        SelectObject(self.dc, old);
        DeleteObject(pen);
    }
    unsafe fn label(&self, text: &str, x: i32, y: i32, color: u32) {
        let string: Vec<u16> = text.encode_utf16().collect();
        let mut size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(self.dc, string.as_ptr(), string.len() as i32, &mut size);
        let x = x.clamp(0, (self.width - size.cx - 4).max(0));
        let y = y.clamp(0, (self.height - size.cy - 4).max(0));
        let rect = RECT {
            left: x,
            top: y,
            right: x + size.cx + 4,
            bottom: y + size.cy + 4,
        };
        let brush = CreateSolidBrush(color);
        FillRect(self.dc, &rect, brush);
        DeleteObject(brush);
        SetBkMode(self.dc, TRANSPARENT as i32);
        SetTextColor(self.dc, 0xffffff);
        TextOutW(self.dc, x + 2, y + 2, string.as_ptr(), string.len() as i32);
    }
    unsafe fn annotation(&self, e: &Element, origin: Rect) {
        let Some(boxes) = e.bounds else { return };
        let Some(r) = boxes.intersect(origin) else {
            return;
        };
        let x = r.left - origin.left;
        let y = r.top - origin.top;
        let right = r.right - origin.left - 1;
        let bottom = r.bottom - origin.top - 1;
        let Some(label) = e.label else { return };
        let hash = (label as u32).wrapping_mul(0x9e3779b9);
        let color = ((hash & 0x7f) + 70)
            | ((((hash >> 8) & 0x7f) + 70) << 8)
            | ((((hash >> 16) & 0x7f) + 70) << 16);
        self.line([x, y], [right, y], color, 2);
        self.line([right, y], [right, bottom], color, 2);
        self.line([right, bottom], [x, bottom], color, 2);
        self.line([x, bottom], [x, y], color, 2);
        let ly = if y >= 18 { y - 18 } else { bottom + 2 };
        self.label(&label.to_string(), right - 18, ly, color);
    }
    unsafe fn finish(&self) -> Result<RgbaImage, String> {
        let raw =
            std::slice::from_raw_parts(self.bits, self.width as usize * self.height as usize * 4);
        let mut rgba = raw.to_vec();
        for p in rgba.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 255;
        }
        RgbaImage::from_raw(self.width as u32, self.height as u32, rgba)
            .ok_or("Invalid capture pixels".into())
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
            ReleaseDC(null_mut(), self.screen);
        }
    }
}
pub struct Image {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
pub fn capture(
    c: &Coordinator,
    r: Rect,
    tree: &Tree,
    annotation: bool,
    grid: Option<[u32; 2]>,
    cursor: Option<[i32; 2]>,
) -> Result<Image, String> {
    let raw = c.capture(|| unsafe { Surface::new(r)?.finish() })?;
    // Render after the guarded screen read; these pixels cannot contain the indicator.
    let mut image = if annotation || grid.is_some() {
        unsafe {
            let s = Surface::new_from_image(&raw)?;
            let font_name: Vec<u16> = "Arial\0".encode_utf16().collect();
            let font = CreateFontW(
                -12,
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
                font_name.as_ptr(),
            );
            let old_font = SelectObject(s.dc, font);
            if let Some([columns, rows]) = grid {
                for i in 1..columns.min(r.width() as u32) {
                    let x = (i as i64 * r.width() as i64 / columns as i64) as i32;
                    s.line([x, 0], [x, r.height()], 0xc8c8c8, 1);
                }
                for i in 1..rows.min(r.height() as u32) {
                    let y = (i as i64 * r.height() as i64 / rows as i64) as i32;
                    s.line([0, y], [r.width(), y], 0xc8c8c8, 1);
                }
            }
            if annotation {
                for e in &tree.elements {
                    if e.interactive {
                        s.annotation(e, r);
                    }
                }
                if let Some(p) = cursor.filter(|p| r.contains(*p)) {
                    let x = p[0] - r.left;
                    let y = p[1] - r.top;
                    let pen = CreatePen(PS_SOLID, 3, 0xff);
                    let old = SelectObject(s.dc, pen);
                    let brush = SelectObject(s.dc, GetStockObject(HOLLOW_BRUSH));
                    Ellipse(s.dc, x - 15, y - 15, x + 15, y + 15);
                    SelectObject(s.dc, brush);
                    SelectObject(s.dc, old);
                    DeleteObject(pen);
                    s.line([x - 15, y], [x + 15, y], 0xff, 2);
                    s.line([x, y - 15], [x, y + 15], 0xff, 2);
                    s.label("CURSOR", x + 15, y - 15, 0xff);
                }
            }
            SelectObject(s.dc, old_font);
            DeleteObject(font);
            s.finish()?
        }
    } else {
        raw
    };
    c.checkpoint()?;
    let scale = (1920.0 / image.width() as f64)
        .min(1080.0 / image.height() as f64)
        .min(1.0);
    if scale < 1.0 {
        image = resize(
            &image,
            ((image.width() as f64 * scale) as u32).max(1),
            ((image.height() as f64 * scale) as u32).max(1),
            FilterType::Lanczos3,
        );
    }
    let (width, height) = image.dimensions();
    let mut buffer = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Image {
        bytes: buffer.into_inner(),
        width,
        height,
    })
}
impl Surface {
    unsafe fn new_from_image(image: &RgbaImage) -> Result<Self, String> {
        // Allocate a DIB without another screen read (never recapture after the guard).
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let mut bits = null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: image.width() as i32,
                biHeight: -(image.height() as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if screen.is_null() || dc.is_null() || bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            if !screen.is_null() {
                ReleaseDC(null_mut(), screen);
            }
            return Err("Annotation bitmap allocation failed".into());
        }
        let old = SelectObject(dc, bitmap);
        let out = Self {
            screen,
            dc,
            bitmap,
            old,
            bits: bits as *mut u8,
            width: image.width() as i32,
            height: image.height() as i32,
        };
        let dest = std::slice::from_raw_parts_mut(out.bits, image.as_raw().len());
        for (s, d) in image.as_raw().chunks_exact(4).zip(dest.chunks_exact_mut(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
        Ok(out)
    }
}
fn expanded(raw: &str) -> String {
    let raw = if let Some(rest) = raw.strip_prefix('~') {
        format!(
            "{}{}",
            std::env::var("USERPROFILE").unwrap_or_default(),
            rest
        )
    } else {
        raw.into()
    };
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let name: String = chars.by_ref().take_while(|c| *c != '%').collect();
            result.push_str(&std::env::var(&name).unwrap_or(format!("%{name}%")));
        } else if c == '$' {
            let mut name = String::new();
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            while let Some(c) = chars.peek() {
                if c.is_alphanumeric() || *c == '_' {
                    name.push(*c);
                    chars.next();
                } else {
                    break;
                }
            }
            if braced && chars.peek() == Some(&'}') {
                chars.next();
            }
            result.push_str(&std::env::var(&name).unwrap_or(if braced {
                format!("${{{name}}}")
            } else {
                format!("${name}")
            }));
        } else {
            result.push(c);
        }
    }
    result
}
pub fn save(bytes: &[u8], raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("save_path must not be empty".into());
    }
    let mut path = PathBuf::from(expanded(raw));
    if !path.is_absolute() {
        return Err("save_path must be an absolute path".into());
    }
    if path.is_dir() || raw.ends_with(['\\', '/']) {
        let mut time: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut time) };
        path = path.join(format!(
            "screenshot-{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}-{}.png",
            time.wYear,
            time.wMonth,
            time.wDay,
            time.wHour,
            time.wMinute,
            time.wSecond,
            time.wMilliseconds,
            &crate::model::id()[..8]
        ));
    } else if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        let mut name = path
            .file_name()
            .ok_or("save_path must name a file")?
            .to_os_string();
        name.push(".png");
        path.set_file_name(name);
    }
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            format!("Error saving screenshot (existing files are not overwritten): {e}")
        })?;
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("Error saving screenshot: {e}"));
    }
    Ok(path.to_string_lossy().into_owned())
}
