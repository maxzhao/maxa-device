//! Snapshot/Screenshot orchestration and MCP content; no HTTP/MCP forwarding.
use super::{
    capture, input,
    tools::{boolean, windows, Backend},
    tree::Tree,
    virtual_desktop,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use windows::Win32::Foundation::HWND as ComHwnd;
use windows_sys::Win32::{Foundation::*, System::Threading::*, UI::WindowsAndMessaging::*};
impl Backend {
    pub(super) fn observe(&mut self, tool: &str, a: &Value) -> Result<Value, String> {
        let screenshot = tool == "Screenshot";
        let ui_tree = !screenshot && boolean(a, "use_ui_tree", true)?;
        let dom = !screenshot && boolean(a, "use_dom", false)?;
        if dom && !ui_tree {
            return Err("use_dom=True requires use_ui_tree=True".into());
        }
        let saved = a["save_path"].as_str().filter(|s| !s.is_empty());
        let vision = screenshot || boolean(a, "use_vision", false)? || saved.is_some();
        let annotation = boolean(a, "use_annotation", !screenshot)?;
        let displays = capture::displays()?;
        let geometry = capture::geometry(a, &displays)?;
        let rect = geometry.rect;
        let mut tree = Tree::default();
        let mut provenance = vec![];
        let mut listed = vec![];
        let foreground = unsafe { GetForegroundWindow() };
        if !screenshot {
            self.labels.clear();
        }
        if ui_tree {
            listed = unsafe { windows() }
                .into_iter()
                .filter(|w| {
                    let hwnd = w["hwnd"].as_u64().unwrap_or(0) as usize as HWND;
                    class(hwnd) != "MaxaAIControlIndicator" && super::visibility::window(hwnd)
                })
                .collect();
            if geometry.filtered {
                listed.retain(|w| window_rect(w).is_some_and(|r| r.intersect(rect).is_some()));
            }
            let uia = self
                .uia
                .as_ref()
                .ok_or("UI Automation is unavailable on this interactive desktop")?;
            let mut walk: Vec<_> = listed
                .iter()
                .filter(|w| {
                    let hwnd = w["hwnd"].as_u64().unwrap_or(0) as usize as HWND;
                    let class = class(hwnd);
                    geometry.filtered
                        || hwnd == foreground
                        || class == "Shell_TrayWnd"
                        || class == "Shell_SecondaryTrayWnd"
                        || class == "Progman"
                        || unsafe { GetWindow(hwnd, GW_OWNER) } == foreground
                })
                .collect();
            walk.sort_by_key(|w| w["hwnd"].as_u64() != Some(foreground as usize as u64));
            for w in walk {
                self.c.checkpoint()?;
                let hwnd = w["hwnd"].as_u64().unwrap_or(0) as usize as HWND;
                let name = w["title"].as_str().unwrap_or("");
                let exe = process_name(w["processId"].as_u64().unwrap_or(0) as u32);
                let browser = matches!(exe.as_str(), "chrome.exe" | "msedge.exe" | "firefox.exe");
                let Some(clip) = window_rect(w).and_then(|r| r.intersect(rect)) else {
                    continue;
                };
                let first = tree.elements.len();
                unsafe {
                    tree.window(
                        uia,
                        &self.c,
                        ComHwnd(hwnd),
                        name,
                        browser,
                        exe == "firefox.exe",
                        dom,
                        clip,
                    )?;
                }
                if let Some(r) = window_rect(w) {
                    provenance.push((
                        first..tree.elements.len(),
                        hwnd,
                        r,
                        w["processId"].as_u64().unwrap_or(0) as u32,
                    ));
                }
            }
        }
        tree.assign_labels();
        let mut points = std::collections::HashMap::new();
        for (range, hwnd, r, process) in provenance {
            for e in &tree.elements[range] {
                let (Some(label), Some(bounds)) = (e.label, e.bounds) else {
                    continue;
                };
                let point = bounds.center();
                let hit = unsafe {
                    WindowFromPoint(POINT {
                        x: point[0],
                        y: point[1],
                    })
                };
                let child = if unsafe { GetAncestor(hit, GA_ROOT) } == hwnd && hit != hwnd {
                    hit
                } else {
                    std::ptr::null_mut()
                };
                let mut child_rect: RECT = unsafe { std::mem::zeroed() };
                let child_rect =
                    if !child.is_null() && unsafe { GetWindowRect(child, &mut child_rect) } != 0 {
                        Some([
                            child_rect.left,
                            child_rect.top,
                            child_rect.right,
                            child_rect.bottom,
                        ])
                    } else {
                        None
                    };
                points.insert(
                    label,
                    super::labels::Label {
                        point,
                        window: hwnd as usize,
                        process,
                        window_rect: [r.left, r.top, r.right, r.bottom],
                        bounds: [bounds.left, bounds.top, bounds.right, bounds.bottom],
                        child: child as usize,
                        child_rect,
                        element: e.element.clone(),
                        element_rect: e.source_bounds.map(|r| [r.left, r.top, r.right, r.bottom]),
                    },
                );
            }
        }
        let cursor = input::cursor().ok().filter(|p| rect.contains(*p));
        let grid = match (
            a["width_reference_line"].as_i64(),
            a["height_reference_line"].as_i64(),
        ) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                Some([w.min(u32::MAX as i64) as u32, h.min(u32::MAX as i64) as u32])
            }
            _ => None,
        };
        let image = if vision {
            Some(capture::capture(
                &self.c, rect, &tree, annotation, grid, cursor,
            )?)
        } else {
            None
        };
        let saved = match (saved, image.as_ref()) {
            (Some(path), Some(image)) => Some(capture::save(&image.bytes, path)?),
            _ => None,
        };
        self.c.checkpoint()?;
        // Commit labels only after a successful observation; failed captures leave none.
        if !screenshot {
            self.labels.replace(points);
        }
        let snapshot_id = crate::model::id();
        let cursor_text = cursor
            .map(|p| format!("({}, {})", p[0], p[1]))
            .unwrap_or("None".into());
        let mut metadata = format!("Cursor Position: {cursor_text}\nSnapshot ID: {snapshot_id}\n");
        if let Some(image) = image.as_ref() {
            metadata.push_str(&format!("Capture Mode: visible-crop\nCapture Rect: ({},{},{},{})\nOriginal Image Size: ({},{})\nReturned Image Size: ({},{})\nCoordinate Mapping: x={}+image_x*{}/{}; y={}+image_y*{}/{}; round independently\n",rect.left,rect.top,rect.width(),rect.height(),rect.width(),rect.height(),image.width,image.height,rect.left,rect.width(),image.width,rect.top,rect.height(),image.height));
        }
        metadata.push_str("Visible Displays: ");
        metadata.push_str(
            &displays
                .iter()
                .map(|d| {
                    format!(
                        "{}:{} {}{}",
                        d.index,
                        d.name,
                        d.rect.xyxy(),
                        if d.primary { " primary" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
        metadata.push('\n');
        if !geometry.selected.is_empty() {
            metadata.push_str(&format!(
                "Selected Displays: {}\n",
                geometry
                    .selected
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if geometry.filtered {
            metadata.push_str(&format!(
                "Screenshot Region: {}\nCoordinate Space: Virtual desktop coordinates\n",
                rect.xyxy()
            ));
            if ui_tree {
                metadata.push_str("UI Tree Scope: Region-filtered. Windows and UI elements outside this region are omitted. Omit region and display to inspect the full desktop.\n");
            }
        }
        if image.is_some() {
            metadata.push_str("Screenshot Backend: native-gdi\n");
        }
        if let Some(path) = saved {
            metadata.push_str(&format!("Screenshot Saved To: {path}\n"));
        }
        let (desktop, desktops) = match virtual_desktop::execute("desktops", None, None) {
            Ok(v) => {
                let all = v["desktops"].as_array().cloned().unwrap_or_default();
                let current = all
                    .iter()
                    .find(|d| d["id"] == v["currentId"])
                    .map(|d| d["name"].as_str().unwrap_or(""))
                    .unwrap_or("Unknown")
                    .to_string();
                (
                    name_table(&[current]),
                    name_table(
                        &all.iter()
                            .map(|d| d["name"].as_str().unwrap_or("").to_string())
                            .collect::<Vec<_>>(),
                    ),
                )
            }
            Err(e) => (format!("Unavailable: {e}"), format!("Unavailable: {e}")),
        };
        let (active, opened) = if ui_tree {
            let current = listed
                .iter()
                .find(|w| w["hwnd"].as_u64() == Some(foreground as usize as u64));
            let active = current
                .map(|w| window_table(&[w]))
                .unwrap_or("No active window found".into());
            let rest = listed
                .iter()
                .filter(|w| w["hwnd"].as_u64() != Some(foreground as usize as u64))
                .collect::<Vec<_>>();
            (
                active,
                if rest.is_empty() {
                    "No windows found".into()
                } else {
                    window_table(&rest)
                },
            )
        } else {
            let note =
                "Not listed: Screenshot skips window enumeration. Call Snapshot for windows.";
            (note.into(), note.into())
        };
        if screenshot {
            metadata.push_str("UI Tree: Skipped for fast screenshot-only capture. Call Snapshot when you need interactive or scrollable elements.\n");
        }
        let mut text=format!("{metadata}\nActive Desktop:\n{desktop}\n\nAll Desktops:\n{desktops}\n\nFocused Window:\n{active}\n\nOpened Windows:\n{opened}");
        if !screenshot {
            text.push_str(&format!(
                "\n\nUI Tree:\n{}",
                if ui_tree {
                    tree.render()
                } else {
                    "No elements".into()
                }
            ));
            if ui_tree {
                text.push_str("\nLabels refer only to this successful Snapshot; label coordinates are physical virtual-desktop pixels, not returned-image pixels.");
            }
        }
        let mut content = vec![json!({"type":"text","text":text})];
        if let Some(image) = image {
            content.push(
                json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(image.bytes)}),
            );
        }
        Ok(json!({"content":content}))
    }
}
fn window_rect(w: &Value) -> Option<capture::Rect> {
    let a = w["rect"].as_array()?;
    Some(capture::Rect {
        left: a.first()?.as_i64()? as i32,
        top: a.get(1)?.as_i64()? as i32,
        right: a.get(2)?.as_i64()? as i32,
        bottom: a.get(3)?.as_i64()? as i32,
    })
}
fn class(hwnd: HWND) -> String {
    unsafe {
        let mut buffer = [0u16; 256];
        let n = GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..n.max(0) as usize])
    }
}
fn process_name(pid: u32) -> String {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return String::new();
        }
        let mut buffer = [0u16; 32768];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        if ok == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
            .rsplit('\\')
            .next()
            .unwrap_or("")
            .to_lowercase()
    }
}
fn name_table(names: &[String]) -> String {
    format!("Name\n--------------------\n{}", names.join("\n"))
}
fn window_table(windows: &[&Value]) -> String {
    let mut out = vec![
        "Name | Depth | Status | Width | Height | Handle".into(),
        "-----|-------|--------|-------|--------|-------".into(),
    ];
    for (depth, w) in windows.iter().enumerate() {
        let r = window_rect(w);
        out.push(format!(
            "{} | {} | {} | {} | {} | {}",
            super::tree::clean(w["title"].as_str().unwrap_or("")),
            depth,
            if w["minimized"] == true {
                "Minimized"
            } else if unsafe { IsZoomed(w["hwnd"].as_u64().unwrap_or(0) as usize as HWND) } != 0 {
                "Maximized"
            } else {
                "Normal"
            },
            r.map(|r| r.width()).unwrap_or(0),
            r.map(|r| r.height()).unwrap_or(0),
            w["hwnd"]
        ));
    }
    out.join("\n")
}
