use super::{
    clipboard,
    control::Coordinator,
    input::{self, Input},
    labels::Labels,
    logic, virtual_desktop,
};
use serde_json::{json, Value};
use std::{path::PathBuf, ptr::null_mut, sync::Arc, time::Duration};
use windows::Win32::{
    System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
    UI::Accessibility::*,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::ClientToScreen,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::WindowsAndMessaging::*,
};
fn text<'a>(a: &'a Value, k: &str, default: &'a str) -> &'a str {
    a[k].as_str().unwrap_or(default)
}
pub(super) fn boolean(a: &Value, k: &str, default: bool) -> Result<bool, String> {
    match &a[k] {
        Value::Null => Ok(default),
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(format!("{k} must be true or false")),
    }
}
fn list(v: &Value) -> Result<Value, String> {
    if let Some(s) = v.as_str() {
        serde_json::from_str(s).map_err(|_| "Expected JSON list".into())
    } else {
        Ok(v.clone())
    }
}
fn point(v: &Value) -> Result<[i32; 2], String> {
    let v = list(v)?;
    let a = v
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or("Point must contain exactly two integers")?;
    let number = |v: &Value| -> Result<i32, String> {
        let n = v
            .as_i64()
            .or_else(|| v.as_str()?.trim().parse().ok())
            .ok_or("Point must contain integers")?;
        i32::try_from(n).map_err(|_| "Coordinate out of range".into())
    };
    Ok([number(&a[0])?, number(&a[1])?])
}
fn titlecase(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub struct Backend {
    pub(super) c: Arc<Coordinator>,
    input: Input,
    pub labels: Labels,
    pub(super) uia: Option<IUIAutomation>,
}
impl Backend {
    pub fn new(c: Arc<Coordinator>) -> Self {
        let uia = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
        };
        Self {
            input: Input(c.clone()),
            c,
            labels: Labels::default(),
            uia,
        }
    }
    fn loc(&self, a: &Value) -> Result<[i32; 2], String> {
        if let Some(id) = a["label"].as_i64() {
            self.labels.resolve(id)
        } else {
            point(&a["loc"])
        }
    }
    pub fn call(&mut self, tool: &str, a: &Value, owner: &str) -> Result<Value, String> {
        self.labels.select_owner(owner);
        self.c.checkpoint()?;
        // Validate the shared schema even for direct pipe callers (the App validates too).
        let catalog =
            crate::native_catalog::respond("windows", &json!({"method":"tools/list"}))?.unwrap();
        let schema = &catalog["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == tool)
            .ok_or("Unknown Windows tool")?["inputSchema"];
        let args = if a.is_null() { json!({}) } else { a.clone() };
        crate::schema::validate(schema, &args)?;
        if matches!(tool, "Snapshot" | "Screenshot") {
            return self.observe(tool, &args);
        }
        self.call_text(tool, &args, owner)
            .map(|text| json!({"content":[{"type":"text","text":text}]}))
    }
    fn call_text(&mut self, tool: &str, a: &Value, owner: &str) -> Result<String, String> {
        // Resolve every label/point in batches before any input/control period begins.
        let p = if matches!(tool, "Click" | "Type" | "Move") {
            let p = self.loc(a)?;
            input::validate(p)?;
            Some(p)
        } else {
            None
        };
        let batch = if matches!(tool, "MultiSelect" | "MultiEdit") {
            Some(self.batch(a, tool == "MultiEdit")?)
        } else {
            None
        };
        if logic::direct(tool, a) {
            self.c.begin(owner)?;
            // Begin may wait for the indicator. Recheck provenance at the input boundary.
            self.validate_labels(a)?;
        }
        match tool {
            "ControlStatus" => Ok(self.c.status(owner, boolean(a, "release", false)?)),
            "Click" => {
                let p = p.unwrap();
                let clicks = a["clicks"].as_i64().unwrap_or(1);
                let button = text(a, "button", "left");
                self.input.click(p, button, clicks)?;
                let count = match clicks {
                    0 => "Hover".into(),
                    1 => "Single".into(),
                    2 => "Double".into(),
                    3 => "Triple".into(),
                    _ => format!("{clicks}x"),
                };
                Ok(format!("{count} {button} clicked at ({},{}).", p[0], p[1]))
            }
            "Type" => self.type_text(
                p.unwrap(),
                text(a, "text", ""),
                boolean(a, "clear", false)?,
                text(a, "caret_position", "idle"),
                boolean(a, "press_enter", false)?,
            ),
            "Shortcut" => {
                let s = text(a, "shortcut", "");
                self.input.shortcut(s)?;
                Ok(format!("Pressed {s}."))
            }
            "Move" => {
                let p = p.unwrap();
                let drag = boolean(a, "drag", false)?;
                if !drag && (!a["from_loc"].is_null() || !a["duration"].is_null()) {
                    return Err("from_loc and duration require drag=True".into());
                }
                if drag {
                    let from = if a["from_loc"].is_null() {
                        None
                    } else {
                        Some(point(&a["from_loc"])?)
                    };
                    let d = if a["duration"].is_null() {
                        None
                    } else {
                        let d = a["duration"]
                            .as_f64()
                            .or_else(|| a["duration"].as_str()?.parse().ok())
                            .ok_or("duration must be finite")?;
                        if !d.is_finite() || !(0.0..=10.0).contains(&d) {
                            return Err("duration must be between 0 and 10 seconds".into());
                        }
                        Some(d)
                    };
                    let start = self.input.drag(p, from, d)?;
                    Ok(format!(
                        "Dragged from ({},{}) to ({},{}){}.",
                        start[0],
                        start[1],
                        p[0],
                        p[1],
                        d.map(|d| format!(" over {d:.3} seconds"))
                            .unwrap_or_default()
                    ))
                } else {
                    self.input.move_to(p)?;
                    Ok(format!("Moved the mouse pointer to ({},{}).", p[0], p[1]))
                }
            }
            "Scroll" => {
                let p = if !a["label"].is_null() {
                    Some(
                        self.labels
                            .resolve(a["label"].as_i64().ok_or("label must be integer")?)?,
                    )
                } else if !a["loc"].is_null() {
                    Some(point(&a["loc"])?)
                } else {
                    None
                };
                if let Some(p) = p {
                    self.input.move_to(p)?;
                }
                let kind = text(a, "type", "vertical");
                let direction = text(a, "direction", "down");
                let n = a["wheel_times"].as_i64().unwrap_or(1);
                self.input.scroll(kind == "horizontal", direction, n)?;
                let at = p
                    .map(|p| format!("({},{})", p[0], p[1]))
                    .unwrap_or("the current mouse position".into());
                Ok(format!(
                    "Scrolled {kind} {direction} by {n} wheel times at {at}."
                ))
            }
            "Wait" => {
                let n = a["duration"].as_i64().ok_or("duration required")?;
                if n < 0 {
                    return Err("duration must be nonnegative".into());
                }
                self.c.sleep(Duration::from_secs(n as u64))?;
                Ok(format!("Waited for {n} seconds."))
            }
            "Clipboard" => match text(a, "mode", "") {
                "get" => Ok(clipboard::get()?
                    .map(|s| format!("Clipboard content:\n{s}"))
                    .unwrap_or("Clipboard is empty or contains non-text data.".into())),
                "set" => {
                    let s = a["text"].as_str().ok_or("text required")?;
                    clipboard::set(s)?;
                    Ok(format!("Clipboard set to: {s}"))
                }
                _ => Err("Unsupported clipboard mode".into()),
            },
            "MultiSelect" => {
                let points = batch.unwrap();
                let ctrl = boolean(a, "press_ctrl", true)?;
                if ctrl {
                    self.c.hold(0x11, false, true)?;
                }
                let r = (|| {
                    for (p, _) in &points {
                        self.validate_labels(a)?;
                        self.input.click(*p, "left", 1)?;
                        self.c.sleep(Duration::from_millis(100))?;
                    }
                    Ok(format!(
                        "Multi-selected elements at:\n{}",
                        points
                            .iter()
                            .map(|(p, _)| format!("({},{})", p[0], p[1]))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ))
                })();
                if ctrl {
                    let _ = self.c.hold(0x11, false, false);
                }
                r
            }
            "MultiEdit" => {
                let points = batch.unwrap();
                let mut warnings = Vec::new();
                for (p, t) in &points {
                    self.c.checkpoint()?;
                    self.validate_labels(a)?;
                    let note = self.type_text(*p, t, true, "idle", false)?;
                    if let Some(at) = note.find("Warning:") {
                        warnings.push(note[at..].to_string());
                    }
                }
                let result = format!(
                    "Multi-edited elements at: {}",
                    points
                        .iter()
                        .map(|(p, t)| format!("({},{}) with text '{t}'", p[0], p[1]))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                Ok(if warnings.is_empty() {
                    result
                } else {
                    format!("{result}\n{}", warnings.join("\n"))
                })
            }
            "Window" => self.window(a).map(|v| v.to_string()),
            "App" => self.app(a),
            _ => Err("Unknown Windows tool".into()),
        }
    }
    fn validate_labels(&self, a: &Value) -> Result<(), String> {
        if let Some(label) = a["label"].as_i64() {
            self.labels.resolve(label)?;
        }
        if !a["labels"].is_null() {
            let labels = list(&a["labels"])?;
            for row in labels.as_array().ok_or("labels must be a list")? {
                let id = if row.is_array() { &row[0] } else { row };
                self.labels
                    .resolve(id.as_i64().ok_or("label must be integer")?)?;
            }
        }
        self.c.checkpoint()
    }
    fn batch(&self, a: &Value, edit: bool) -> Result<Vec<([i32; 2], String)>, String> {
        if a["locs"].is_null() && a["labels"].is_null() {
            return Err("Either locs or labels must be provided.".into());
        }
        let mut out = vec![];
        if !a["locs"].is_null() {
            let v = list(&a["locs"])?;
            for row in v.as_array().ok_or("locs must be a list")? {
                if edit {
                    let r = row
                        .as_array()
                        .filter(|r| r.len() == 3)
                        .ok_or("Each loc must be [x,y,text]")?;
                    out.push((
                        point(&json!([r[0], r[1]]))?,
                        r[2].as_str().ok_or("text must be string")?.into(),
                    ));
                } else {
                    out.push((point(row)?, String::new()));
                }
            }
        }
        if !a["labels"].is_null() {
            let v = list(&a["labels"])?;
            for row in v.as_array().ok_or("labels must be a list")? {
                if edit {
                    let r = row
                        .as_array()
                        .filter(|r| r.len() == 2)
                        .ok_or("Each label must be [label,text]")?;
                    out.push((
                        self.labels
                            .resolve(r[0].as_i64().ok_or("label must be integer")?)?,
                        r[1].as_str().ok_or("text must be string")?.into(),
                    ));
                } else {
                    out.push((
                        self.labels
                            .resolve(row.as_i64().ok_or("label must be integer")?)?,
                        String::new(),
                    ));
                }
            }
        }
        for (p, _) in &out {
            input::validate(*p)?;
        }
        Ok(out)
    }
    fn focused(&self) -> Option<IUIAutomationElement> {
        unsafe { self.uia.as_ref()?.GetFocusedElement().ok() }
    }
    fn value(e: &IUIAutomationElement) -> Option<String> {
        unsafe {
            if e.CurrentIsPassword().ok()?.as_bool() {
                return None;
            }
            if let Ok(p) = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                return p.CurrentValue().ok().map(|s| s.to_string());
            }
            let hwnd = e.CurrentNativeWindowHandle().ok()?.0;
            let class = e.CurrentClassName().ok()?.to_string().to_lowercase();
            if hwnd.is_null() || !class.contains("edit") {
                return None;
            }
            // SendMessageTimeout avoids wedging on native edit WM_GETTEXT. Never inject arbitrary messages.
            let mut length = 0usize;
            if SendMessageTimeoutW(
                hwnd as HWND,
                WM_GETTEXTLENGTH,
                0,
                0,
                SMTO_ABORTIFHUNG,
                500,
                &mut length,
            ) == 0
            {
                return None;
            }
            let mut buffer = vec![0u16; length.min(1_000_000) + 1];
            let mut read = 0usize;
            if SendMessageTimeoutW(
                hwnd as HWND,
                WM_GETTEXT,
                buffer.len(),
                buffer.as_mut_ptr() as isize,
                SMTO_ABORTIFHUNG,
                500,
                &mut read,
            ) == 0
            {
                return None;
            }
            Some(String::from_utf16_lossy(&buffer[..read.min(buffer.len())]))
        }
    }
    fn multiline(e: &IUIAutomationElement) -> bool {
        unsafe {
            if e.CurrentControlType().ok() == Some(UIA_DocumentControlTypeId) {
                return true;
            }
            if let (Ok(h), Ok(class)) = (e.CurrentNativeWindowHandle(), e.CurrentClassName()) {
                return !h.0.is_null()
                    && class.to_string().to_lowercase().contains("edit")
                    && GetWindowLongW(h.0 as HWND, GWL_STYLE) & 4 != 0;
            }
            false
        }
    }
    fn editable(e: &IUIAutomationElement) -> bool {
        unsafe {
            matches!(
                e.CurrentControlType().ok(),
                Some(kind) if kind == UIA_EditControlTypeId || kind == UIA_DocumentControlTypeId
            ) || e
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .and_then(|p| p.CurrentIsReadOnly())
                .is_ok_and(|b| !b.as_bool())
        }
    }
    fn describe(e: &IUIAutomationElement) -> String {
        unsafe {
            let kind = if e.CurrentControlType().ok() == Some(UIA_DocumentControlTypeId) {
                "DocumentControl"
            } else if e.CurrentControlType().ok() == Some(UIA_EditControlTypeId) {
                "EditControl"
            } else {
                "Control"
            };
            let name = e.CurrentName().map(|s| s.to_string()).unwrap_or_default();
            format!("{kind} '{}'", name.chars().take(40).collect::<String>())
        }
    }
    fn type_text(
        &self,
        p: [i32; 2],
        text: &str,
        clear: bool,
        caret: &str,
        enter: bool,
    ) -> Result<String, String> {
        self.input.click(p, "left", 1)?;
        self.c.sleep(Duration::from_millis(100))?;
        let mut target = self.focused();
        let multi = target.as_ref().is_some_and(Self::multiline);
        let mut notes = vec![];
        if caret != "idle" {
            let clicked = unsafe {
                self.uia.as_ref().and_then(|u| {
                    u.ElementFromPoint(windows::Win32::Foundation::POINT { x: p[0], y: p[1] })
                        .ok()
                })
            };
            let same = match (&self.uia, &target, &clicked) {
                (Some(u), Some(t), Some(e)) => unsafe {
                    u.CompareElements(t, e).is_ok_and(|b| b.as_bool())
                },
                _ => false,
            };
            let key = if caret == "start" { 0x24 } else { 0x23 };
            if multi && same {
                self.input.chord(&[0x11, key])?;
            } else {
                self.input.key(key)?;
            }
        }
        if clear {
            self.c.sleep(Duration::from_millis(100))?;
            target = self.focused();
            if target.as_ref().is_some_and(|e| !Self::editable(e)) {
                notes.push(format!("Warning: did not clear: the focused element is {}, not a text field. The text was typed there; check where it landed.",Self::describe(target.as_ref().unwrap())));
            } else {
                let multi = target.as_ref().is_some_and(Self::multiline);
                if multi {
                    self.input.chord(&[0x11, 0x24])?;
                    self.input.chord(&[0x11, 0x10, 0x23])?;
                } else {
                    self.input.key(0x24)?;
                    self.input.chord(&[0x10, 0x23])?;
                }
                self.input.key(8)?;
                self.c.sleep(Duration::from_millis(60))?;
                let mut how = if multi {
                    "Ctrl+Home, Ctrl+Shift+End, Backspace".to_string()
                } else {
                    "Home, Shift+End, Backspace".to_string()
                };
                if let Some(e) = target.as_ref() {
                    if Self::value(e).is_some_and(|s| !s.is_empty()) {
                        unsafe {
                            if let Ok(pattern) = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(
                                UIA_ValuePatternId,
                            ) {
                                if pattern.CurrentIsReadOnly().is_ok_and(|b| !b.as_bool()) {
                                    self.c.checkpoint()?;
                                    if pattern.SetValue(&windows::core::BSTR::from("")).is_ok() {
                                        how.push_str(", then UIA ValuePattern");
                                    }
                                }
                            }
                        }
                    }
                    if Self::value(e).is_some_and(|s| !s.is_empty()) {
                        notes.push(format!(
                            "Warning: {} still held text after clearing ({how}).",
                            Self::describe(e)
                        ));
                    } else {
                        notes.push(format!("Cleared {} with {how}.", Self::describe(e)));
                    }
                } else {
                    notes.push(format!("Cleared with {how} (focused element unknown)."));
                }
            }
        }
        if logic::paste_path(text) {
            self.c.checkpoint()?;
            let prior = clipboard::Snapshot::save_and_set(text)?;
            let r = (|| {
                self.c.sleep(Duration::from_millis(50))?;
                self.input.chord(&[0x11, 0x56])?;
                self.c.sleep(Duration::from_millis(100))
            })();
            let restored = prior.restore_if_unchanged(text);
            restored?;
            r?;
        } else {
            for ch in text.chars() {
                self.input.unicode(ch)?;
                self.c.sleep(Duration::from_millis(40))?;
            }
        }
        let current = self.focused();
        let moved = match (&self.uia, &target, &current) {
            (Some(u), Some(t), Some(n)) => unsafe {
                u.CompareElements(t, n).is_ok_and(|b| !b.as_bool())
            },
            _ => false,
        };
        if moved {
            notes.push(format!(
                "Warning: focus moved to {} while typing; some keystrokes may have gone there.",
                Self::describe(current.as_ref().unwrap())
            ));
        } else if let Some(e) = target.as_ref().or(current.as_ref()) {
            if let Some(v) = Self::value(e) {
                let shown = v.chars().take(200).collect::<String>()
                    + if v.chars().count() > 200 { "…" } else { "" };
                if clear || !Self::multiline(e) {
                    if !text.contains(['\n', '\t', '{', '}'])
                        && ((clear && v != text) || (!clear && !v.contains(text)))
                    {
                        notes.push(format!(
                            "Warning: the field reads back '{shown}', not the typed text."
                        ));
                    } else {
                        notes.push(format!("Field reads back '{shown}'."));
                    }
                }
            }
        }
        if enter {
            self.input.key(13)?;
        }
        Ok(format!(
            "Typed {text} at ({},{}).{}",
            p[0],
            p[1],
            if notes.is_empty() {
                String::new()
            } else {
                format!(" {}", notes.join(" "))
            }
        ))
    }
    fn window(&self, a: &Value) -> Result<Value, String> {
        unsafe {
            let mode = text(a, "mode", "");
            if mode == "list" {
                return Ok(json!({"windows":windows()}));
            }
            if matches!(mode, "desktops" | "switch_desktop") {
                self.c.checkpoint()?;
                return virtual_desktop::execute(mode, None, a["desktop_id"].as_str());
            }
            let hwnd = a["hwnd"].as_u64().ok_or("hwnd required")? as usize as HWND;
            let info = window_info(hwnd)?;
            if mode == "query" {
                if !a["process_id"].is_null() && a["process_id"] != info["processId"] {
                    return Err("STALE_HWND: process ownership changed; query again".into());
                }
                return Ok(info);
            }
            if a["process_id"].as_u64() != info["processId"].as_u64() {
                return Err("STALE_HWND: process ownership changed; query again".into());
            }
            self.c.checkpoint()?;
            match mode {
                "focus" => {
                    focus(hwnd)?;
                    window_info(hwnd)
                }
                "move" => {
                    let r = list(&a["rect"])?;
                    let r = r
                        .as_array()
                        .filter(|r| r.len() == 4)
                        .ok_or("rect must be [left,top,right,bottom]")?;
                    let tl = point(&json!([r[0], r[1]]))?;
                    let br = point(&json!([r[2], r[3]]))?;
                    if br[0] <= tl[0] || br[1] <= tl[1] {
                        return Err("rect must have positive extent".into());
                    }
                    if SetWindowPos(
                        hwnd,
                        null_mut(),
                        tl[0],
                        tl[1],
                        br[0] - tl[0],
                        br[1] - tl[1],
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    ) == 0
                    {
                        return Err("Window move failed".into());
                    }
                    window_info(hwnd)
                }
                "click" => {
                    if IsWindowVisible(hwnd) == 0
                        || IsIconic(hwnd) != 0
                        || GetForegroundWindow() != hwnd
                    {
                        return Err("Window is not an active visible input target".into());
                    }
                    let p = point(&a["point"])?;
                    let mut rect: RECT = std::mem::zeroed();
                    if GetClientRect(hwnd, &mut rect) == 0
                        || p[0] < 0
                        || p[1] < 0
                        || p[0] >= rect.right
                        || p[1] >= rect.bottom
                    {
                        return Err("Point is outside the client area".into());
                    }
                    let mut screen = POINT { x: p[0], y: p[1] };
                    if ClientToScreen(hwnd, &mut screen) == 0 {
                        return Err("ClientToScreen failed".into());
                    }
                    if GetAncestor(WindowFromPoint(screen), GA_ROOT) != hwnd {
                        return Err("Window point is occluded; input refused".into());
                    }
                    self.input.click([screen.x, screen.y], "left", 1)?;
                    Ok(json!({"hwnd":hwnd as usize,"screenPoint":[screen.x,screen.y],"result":""}))
                }
                "move_desktop" => {
                    let d = virtual_desktop::execute(mode, Some(hwnd), a["desktop_id"].as_str())?;
                    let mut info = window_info(hwnd)?;
                    info["desktopId"] = d["desktopId"].clone();
                    Ok(info)
                }
                _ => Err("Unsupported Window mode".into()),
            }
        }
    }
    fn app(&self, a: &Value) -> Result<String, String> {
        let mode = text(a, "mode", "launch");
        if mode != "launch_executable"
            && ["executable", "args", "cwd"]
                .iter()
                .any(|k| !a[k].is_null())
        {
            return Err("executable, args, and cwd require mode=launch_executable".into());
        }
        if mode == "launch_executable" {
            if ["name", "window_loc", "window_size"]
                .iter()
                .any(|k| !a[k].is_null())
            {
                return Err(
                    "name, window_loc, window_size are not supported for launch_executable".into(),
                );
            }
            let exe = PathBuf::from(a["executable"].as_str().ok_or("executable required")?)
                .canonicalize()
                .map_err(|e| format!("Executable does not exist: {e}"))?;
            if !exe.is_file() {
                return Err("executable must be a file".into());
            }
            use std::os::windows::process::CommandExt;
            let mut c = std::process::Command::new(&exe);
            c.creation_flags(0x01000000); // Explicit user app: escape worker teardown.
            let args = if a["args"].is_null() {
                json!([])
            } else {
                list(&a["args"])?
            };
            let args: Vec<String> = args
                .as_array()
                .ok_or("args must be list of strings")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or("args must contain strings")
                })
                .collect::<Result<_, _>>()?;
            c.args(&args);
            let cwd = if let Some(s) = a["cwd"].as_str() {
                let p = PathBuf::from(s).canonicalize().map_err(|e| e.to_string())?;
                if !p.is_dir() {
                    return Err("cwd must be directory".into());
                }
                c.current_dir(&p);
                Some(p)
            } else {
                None
            };
            self.c.checkpoint()?;
            let child = c
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
            return Ok(
                json!({"pid":child.id(),"executable":exe,"args":args,"cwd":cwd}).to_string(),
            );
        }
        let name = a["name"].as_str().unwrap_or("");
        if mode == "launch" {
            return self.launch(name);
        }
        let choices = unsafe { windows() };
        let target = if name.is_empty() && mode == "resize" {
            let h = unsafe { GetForegroundWindow() };
            choices
                .iter()
                .find(|w| w["hwnd"].as_u64() == Some(h as usize as u64))
        } else {
            choices
                .iter()
                .map(|w| (w, logic::fuzzy(name, w["title"].as_str().unwrap_or(""))))
                .filter(|(_, s)| *s >= 70.0)
                .reduce(|best, item| if item.1 > best.1 { item } else { best })
                .map(|(w, _)| w)
        }
        .ok_or_else(|| format!("Application {} not found.", titlecase(name)))?;
        let hwnd = target["hwnd"].as_u64().unwrap() as usize as HWND;
        let title = target["title"].as_str().unwrap_or("");
        if mode == "switch" {
            self.c.checkpoint()?;
            let minimized = unsafe { IsIconic(hwnd) != 0 };
            unsafe {
                focus(hwnd)?;
            }
            return Ok(if minimized {
                format!(
                    "Restored {} from minimized and switched to it.",
                    titlecase(title)
                )
            } else {
                format!("Switched to {} window.", titlecase(title))
            });
        }
        if mode == "resize" {
            unsafe {
                if IsIconic(hwnd) != 0 {
                    return Ok(format!("{title} is minimized"));
                }
                if IsZoomed(hwnd) != 0 {
                    return Ok(format!("{title} is maximized"));
                }
            }
            let r = target["rect"].as_array().unwrap();
            let loc = if a["window_loc"].is_null() {
                [r[0].as_i64().unwrap() as i32, r[1].as_i64().unwrap() as i32]
            } else {
                point(&a["window_loc"])?
            };
            let size = if a["window_size"].is_null() {
                [
                    (r[2].as_i64().unwrap() - r[0].as_i64().unwrap()) as i32,
                    (r[3].as_i64().unwrap() - r[1].as_i64().unwrap()) as i32,
                ]
            } else {
                point(&a["window_size"])?
            };
            if size[0] <= 0 || size[1] <= 0 {
                return Err("window_size must be positive".into());
            }
            self.c.checkpoint()?;
            if unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    loc[0],
                    loc[1],
                    size[0],
                    size[1],
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            } == 0
            {
                return Err("Window resize failed".into());
            }
            return Ok(format!(
                "{title} resized to {}x{} at {},{}.",
                size[0], size[1], loc[0], loc[1]
            ));
        }
        Err("Unsupported App mode".into())
    }
    fn launch(&self, name: &str) -> Result<String, String> {
        let output =
            powershell("@(Get-StartApps | Select-Object Name,AppID) | ConvertTo-Json -Compress")
                .unwrap_or_default();
        let apps: Value = serde_json::from_str(output.trim()).unwrap_or(json!([]));
        let mut apps = apps.as_array().cloned().unwrap_or_default();
        // Same fallback order as the baseline: shortcuts only when StartApps is unavailable.
        if apps.is_empty() {
            for root in [
                std::env::var_os("APPDATA")
                    .map(PathBuf::from)
                    .map(|p| p.join("Microsoft/Windows/Start Menu/Programs")),
                std::env::var_os("ProgramData")
                    .map(PathBuf::from)
                    .map(|p| p.join("Microsoft/Windows/Start Menu/Programs")),
            ]
            .into_iter()
            .flatten()
            {
                start_links(&root, &mut apps);
            }
        }
        if let Ok(localized)=powershell("@((New-Object -ComObject Shell.Application).NameSpace('shell:::{4234d49b-0245-4df3-b780-3893943456e1}').Items() | Select-Object Name,@{n='AppID';e={$_.Path}}) | ConvertTo-Json -Compress") {
            if let Ok(Value::Array(localized))=serde_json::from_str(&localized) {for app in localized {if !apps.iter().any(|a|a["Name"].as_str().unwrap_or("").eq_ignore_ascii_case(app["Name"].as_str().unwrap_or(""))) {apps.push(app);}}}
        }
        let matched = apps
            .iter()
            .map(|a| (a, logic::fuzzy(name, a["Name"].as_str().unwrap_or(""))))
            .filter(|(_, s)| *s >= 70.0)
            .reduce(|best, item| if item.1 > best.1 { item } else { best })
            .map(|(a, _)| a);
        let Some(app) = matched else {
            return Ok(format!("{} not found in start menu.", titlecase(name)));
        };
        let id = app["AppID"]
            .as_str()
            .ok_or("Invalid Start Menu identifier")?;
        let path = if std::path::Path::new(id).exists() {
            id.to_string()
        } else if id.contains('\\') {
            resolve_known_folder(id)?
        } else {
            format!("shell:AppsFolder\\{id}")
        };
        self.c.checkpoint()?;
        powershell_launch(&format!(
            "Start-Process -FilePath '{}' -ErrorAction Stop",
            path.replace('\'', "''")
        ))?;
        for _ in 0..100 {
            self.c.checkpoint()?;
            if unsafe { windows() }.iter().any(|w| {
                w["title"]
                    .as_str()
                    .is_some_and(|s| logic::fuzzy(name, s) >= 70.0)
            }) {
                return Ok(format!("{} launched.", titlecase(name)));
            }
            self.c.sleep(Duration::from_millis(100))?;
        }
        Ok(format!(
            "Launching {} sent, but window not detected yet.",
            titlecase(name)
        ))
    }
}
fn resolve_known_folder(id: &str) -> Result<String, String> {
    use std::os::windows::ffi::OsStringExt;
    if !id.starts_with('{') {
        return Ok(id.into());
    }
    let end = id
        .find('}')
        .ok_or("Invalid known-folder application path")?;
    let g = windows::core::GUID::try_from(&id[1..end]).map_err(|e| e.to_string())?;
    let guid = windows_sys::core::GUID {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    };
    unsafe {
        let mut path = null_mut();
        let hr =
            windows_sys::Win32::UI::Shell::SHGetKnownFolderPath(&guid, 0, null_mut(), &mut path);
        if hr < 0 || path.is_null() {
            return Err(format!(
                "Known-folder application path unavailable (0x{:08X})",
                hr as u32
            ));
        }
        let mut len = 0;
        while *path.add(len) != 0 {
            len += 1;
        }
        let folder = std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, len));
        windows_sys::Win32::System::Com::CoTaskMemFree(path as *const _);
        Ok(PathBuf::from(folder)
            .join(id[end + 1..].trim_start_matches('\\'))
            .to_string_lossy()
            .into_owned())
    }
}
fn powershell(script: &str) -> Result<String, String> {
    powershell_flags(script, false)
}
fn powershell_launch(script: &str) -> Result<String, String> {
    powershell_flags(script, true)
}
fn powershell_flags(script: &str, user_launch: bool) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let script=format!("$ErrorActionPreference='Stop';[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false);{script}");
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000 | if user_launch { 0x01000000 } else { 0 })
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "PowerShell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
fn start_links(root: &std::path::Path, apps: &mut Vec<Value>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            let Ok(meta) = e.file_type() else { continue };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                start_links(&p, apps);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                apps.push(json!({"Name":p.file_stem().unwrap_or_default().to_string_lossy(),"AppID":p.to_string_lossy()}));
            }
        }
    }
}
unsafe fn window_info(hwnd: HWND) -> Result<Value, String> {
    if IsWindow(hwnd) == 0 {
        return Err("STALE_HWND".into());
    }
    let mut pid = 0;
    let thread = GetWindowThreadProcessId(hwnd, &mut pid);
    let n = GetWindowTextLengthW(hwnd);
    let mut title = vec![0u16; n.max(0) as usize + 1];
    let n = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
    let mut r: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut r) == 0 {
        return Err("Window bounds unavailable".into());
    }
    Ok(
        json!({"hwnd":hwnd as usize,"processId":pid,"threadId":thread,"title":String::from_utf16_lossy(&title[..n.max(0) as usize]),"rect":[r.left,r.top,r.right,r.bottom],"visible":IsWindowVisible(hwnd)!=0,"minimized":IsIconic(hwnd)!=0,"captureMode":"visible-crop; hidden/occluded pixels are not a window capture"}),
    )
}
pub(super) unsafe fn windows() -> Vec<Value> {
    unsafe extern "system" fn collect(hwnd: HWND, p: LPARAM) -> BOOL {
        if IsWindowVisible(hwnd) != 0 {
            if let Ok(w) = window_info(hwnd) {
                (*(p as *mut Vec<Value>)).push(w);
            }
        }
        1
    }
    let mut out = vec![];
    EnumWindows(Some(collect), &mut out as *mut _ as isize);
    out
}
unsafe fn focus(hwnd: HWND) -> Result<(), String> {
    if IsWindow(hwnd) == 0 {
        return Err("STALE_HWND".into());
    }
    if IsIconic(hwnd) != 0 {
        ShowWindow(hwnd, SW_RESTORE);
    }
    let current = GetCurrentThreadId();
    let foreground = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let target = GetWindowThreadProcessId(hwnd, null_mut());
    let mut attached = vec![];
    for thread in [foreground, target] {
        if thread != 0
            && thread != current
            && !attached.contains(&thread)
            && AttachThreadInput(current, thread, 1) != 0
        {
            attached.push(thread);
        }
    }
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    for thread in attached {
        AttachThreadInput(current, thread, 0);
    }
    for _ in 0..20 {
        if GetForegroundWindow() == hwnd {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("Windows denied focus".into())
}
