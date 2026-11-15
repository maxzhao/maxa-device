//! UIA semantic tree. Ported from Windows-MCP tree/{service,views,budget}.py (MIT).
use super::{capture::Rect, control::Coordinator};
use std::collections::{HashMap, HashSet};
use windows::Win32::{
    Foundation::HWND,
    System::{
        Com::SAFEARRAY,
        Ole::{
            SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElemsize,
            SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
        },
        Variant::{VariantClear, VariantToInt32},
    },
    UI::Accessibility::*,
};

pub const LIMIT: usize = 500;
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(*c as u32,0xe000..=0xf8ff|0xf0000..=0xffffd|0x100000..=0x10fffd))
        .collect()
}
fn shown(text: &str, n: usize) -> String {
    let text = clean(text);
    let mut out = text.chars().take(n).collect::<String>();
    if text.chars().count() > n {
        out.push('…');
    }
    out.replace('\r', "\\r").replace('\n', "\\n")
}
#[derive(Clone)]
pub struct Element {
    pub parent: Option<usize>,
    pub kind: String,
    pub name: String,
    pub bounds: Option<Rect>,
    pub interactive: bool,
    pub scrollable: bool,
    pub label: Option<i64>,
    pub meta: Vec<String>,
    pub structural: bool,
    pub element: Option<IUIAutomationElement>,
    pub source_bounds: Option<Rect>,
}
impl Element {
    pub fn new(parent: Option<usize>, kind: &str, name: &str, bounds: Option<Rect>) -> Self {
        Self {
            parent,
            kind: kind.into(),
            name: clean(name),
            bounds,
            interactive: false,
            scrollable: false,
            label: None,
            meta: vec![],
            structural: false,
            element: None,
            source_bounds: None,
        }
    }
}
#[derive(Default)]
pub struct Tree {
    pub elements: Vec<Element>,
    pub truncated: bool,
    pub notes: Vec<String>,
    emitted: usize,
    visited: usize,
    window: usize,
}
impl Tree {
    pub fn add(&mut self, e: Element) -> Option<usize> {
        if self.window != 0
            && e.bounds
                .is_some_and(|r| !super::visibility::point(self.window as _, r.center()))
        {
            return None;
        }
        if (e.interactive || e.scrollable || e.kind == "text") && self.emitted >= LIMIT {
            self.truncated = true;
            return None;
        }
        if e.interactive || e.scrollable || e.kind == "text" {
            self.emitted += 1;
        }
        let i = self.elements.len();
        self.elements.push(e);
        Some(i)
    }
    pub fn assign_labels(&mut self) -> HashMap<i64, [i32; 2]> {
        let mut labels = HashMap::new();
        let mut label = 0;
        for scroll in [false, true] {
            for e in &mut self.elements {
                if (if scroll {
                    e.scrollable && !e.interactive
                } else {
                    e.interactive
                }) && e.bounds.is_some()
                {
                    e.label = Some(label);
                    labels.insert(label, e.bounds.unwrap().center());
                    label += 1;
                }
            }
        }
        labels
    }
    pub fn render(&self) -> String {
        let mut children = vec![vec![]; self.elements.len()];
        let mut roots = vec![];
        for (i, e) in self.elements.iter().enumerate() {
            if let Some(p) = e.parent {
                if p < children.len() {
                    children[p].push(i);
                }
            } else {
                roots.push(i);
            }
        }
        fn keep(i: usize, t: &Tree, children: &[Vec<usize>]) -> bool {
            let e = &t.elements[i];
            !e.structural || children[i].iter().any(|i| keep(*i, t, children))
        }
        fn render(
            i: usize,
            t: &Tree,
            children: &[Vec<usize>],
            prefix: &str,
            last: bool,
            out: &mut Vec<String>,
        ) {
            let e = &t.elements[i];
            let mut description = if e.kind == "window" {
                format!("window {:?}", e.name)
            } else if e.interactive || e.scrollable {
                let p = e.bounds.map(|r| r.center()).unwrap_or([0, 0]);
                let action = match e.kind.as_str() {
                    "edit" => "fill",
                    "check box" | "checkbox" => "toggle",
                    "combo box" | "combobox" | "radio button" => "select",
                    "slider" => "slide",
                    "document" => "scroll",
                    _ => "click",
                };
                format!(
                    "({},{}) {} {:?}  [action: {action}]",
                    p[0], p[1], e.kind, e.name
                )
            } else {
                format!("{} {:?}", e.kind, e.name)
            };
            if let Some(label) = e.label {
                description.push_str(&format!("  [label:{label}]"));
            }
            for meta in &e.meta {
                description.push_str(&format!("  [{meta}]"));
            }
            out.push(format!(
                "{prefix}{}{description}",
                if last { "└── " } else { "├── " }
            ));
            let next = format!("{prefix}{}", if last { "    " } else { "│   " });
            let kept: Vec<_> = children[i]
                .iter()
                .copied()
                .filter(|i| keep(*i, t, children))
                .collect();
            for (n, child) in kept.iter().enumerate() {
                render(*child, t, children, &next, n + 1 == kept.len(), out);
            }
        }
        let mut out = vec!["desktop".into()];
        for (n, root) in roots.iter().enumerate() {
            render(*root, self, &children, "", n + 1 == roots.len(), &mut out);
        }
        if self.truncated {
            out.push(format!("... [truncated: reached the {LIMIT}-element capture limit or traversal safety bound; narrow region/display or use_dom for a browser page.]"));
        }
        for note in &self.notes {
            out.push(note.clone());
        }
        out.join("\n")
    }
    pub unsafe fn window(
        &mut self,
        uia: &IUIAutomation,
        c: &Coordinator,
        hwnd: HWND,
        title: &str,
        browser: bool,
        firefox: bool,
        use_dom: bool,
        clip: Rect,
    ) -> Result<(), String> {
        if use_dom && !browser {
            return Ok(());
        }
        if self.emitted >= LIMIT {
            self.truncated = true;
            return Ok(());
        }
        c.checkpoint()?;
        if !super::visibility::window(hwnd.0 as _) {
            return Ok(());
        }
        self.window = hwnd.0 as usize;
        let root = self.add(Element::new(None, "window", title, None)).unwrap();
        let cache = uia.CreateCacheRequest().map_err(|e| e.to_string())?;
        cache
            .SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0))
            .map_err(|e| e.to_string())?;
        for property in [
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_LocalizedControlTypePropertyId,
            UIA_BoundingRectanglePropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_IsEnabledPropertyId,
            UIA_IsControlElementPropertyId,
            UIA_IsKeyboardFocusablePropertyId,
            UIA_HasKeyboardFocusPropertyId,
            UIA_IsPasswordPropertyId,
            UIA_AutomationIdPropertyId,
            UIA_AcceleratorKeyPropertyId,
            UIA_ClassNamePropertyId,
            UIA_LegacyIAccessibleRolePropertyId,
            UIA_SelectionItemIsSelectedPropertyId,
            UIA_ToggleToggleStatePropertyId,
            UIA_ExpandCollapseExpandCollapseStatePropertyId,
        ] {
            let _ = cache.AddProperty(property);
        }
        for pattern in [
            UIA_ValuePatternId,
            UIA_RangeValuePatternId,
            UIA_ScrollPatternId,
            UIA_LegacyIAccessiblePatternId,
        ] {
            let _ = cache.AddPattern(pattern);
        }
        let element = match uia.ElementFromHandle(hwnd) {
            Ok(e) => e,
            Err(e) => {
                self.notes
                    .push(format!("UIA unavailable for {title:?}: {e}"));
                if firefox && use_dom {
                    super::ia2::collect(self, c, hwnd, root, clip)?;
                }
                return Ok(());
            }
        };
        let mut stack = vec![(element, root, 0usize, false, clip)];
        let mut seen = HashSet::new();
        let mut found_dom = false;
        while let Some((element, parent, depth, in_dom, dom_clip)) = stack.pop() {
            c.checkpoint()?;
            self.visited += 1;
            if self.emitted >= LIMIT || self.visited > 30000 || depth > 500 {
                self.truncated = true;
                break;
            }
            let Ok(e) = element.BuildUpdatedCache(&cache) else {
                continue;
            };
            let runtime = e
                .GetRuntimeId()
                .ok()
                .map(|a| safe_array::<i32>(a))
                .unwrap_or_default();
            if !runtime.is_empty() && !seen.insert(runtime) {
                continue;
            }
            let kind = e.CachedControlType().map(|id| id.0).unwrap_or(0);
            let raw_box = e.CachedBoundingRectangle().ok().map(|r| Rect {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
            let offscreen = e.CachedIsOffscreen().map(|v| v.as_bool()).unwrap_or(true);
            let enabled = e.CachedIsEnabled().is_ok_and(|v| v.as_bool());
            let control = e.CachedIsControlElement().is_ok_and(|v| v.as_bool());
            let name = e
                .CachedName()
                .map(|v| v.to_string().trim().to_string())
                .unwrap_or_default();
            let localized = e
                .CachedLocalizedControlType()
                .map(|v| v.to_string().to_lowercase())
                .unwrap_or_default();
            let automation = e
                .CachedAutomationId()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let root_web = browser
                && (automation == "RootWebArea"
                    || (!firefox && kind == 50030 && depth > 0 && !offscreen));
            let is_dom = in_dom || root_web;
            let dom_clip = if root_web {
                found_dom = true;
                raw_box.and_then(|r| r.intersect(clip)).unwrap_or(dom_clip)
            } else {
                dom_clip
            };
            let bounds = raw_box.and_then(|r| r.intersect(if is_dom { dom_clip } else { clip }));
            // Offscreen edits/list items are not exempt: providers can retain hidden
            // dialogs and virtualized children with real-looking cached rectangles.
            if offscreen {
                continue;
            }
            let can_emit = (!use_dom || is_dom)
                && bounds.is_some_and(|r| super::visibility::point(hwnd.0 as _, r.center()))
                && control
                && enabled;
            let role = cached_int(&e, UIA_LegacyIAccessibleRolePropertyId).unwrap_or(-1);
            let focusable = e.CachedIsKeyboardFocusable().is_ok_and(|v| v.as_bool())
                || matches!(kind, 50000 | 50002 | 50004 | 50007 | 50013 | 50019);
            let actionable = kind == 50030
                || ((interactive_kind(kind) || kind == 50026)
                    && ((interactive_role(role) && focusable)
                        || matches!(
                            kind,
                            50000
                                | 50002
                                | 50003
                                | 50004
                                | 50005
                                | 50007
                                | 50011
                                | 50014
                                | 50013
                                | 50015
                                | 50016
                                | 50019
                                | 50024
                                | 50029
                                | 50031
                        )))
                || (!browser && kind == 50006 && focusable);
            let mut semantic_parent = parent;
            if can_emit && actionable && (!name.is_empty() || is_dom) {
                let kind_name = if localized.is_empty() {
                    control_name(kind).into()
                } else {
                    localized.clone()
                };
                let mut node = Element::new(Some(parent), &kind_name, &name, bounds);
                node.interactive = true;
                node.element = Some(e.clone());
                node.source_bounds = raw_box;
                node.meta = metadata(&e, kind);
                if let Some(index) = self.add(node) {
                    semantic_parent = index;
                    if matches!(kind, 50004 | 50030)
                        && !is_dom
                        && !e.CachedIsPassword().is_ok_and(|b| b.as_bool())
                    {
                        self.words(c, &e, index, bounds.unwrap())?;
                    }
                }
            } else if can_emit
                && is_dom
                && matches!(kind, 50020 | 50006 | 50017)
                && !name.is_empty()
            {
                let node = Element::new(Some(parent), "text", &name, None);
                self.add(node);
            } else if (!use_dom || is_dom)
                && !offscreen
                && matches!(kind, 50021 | 50022 | 50023 | 50025 | 50026 | 50032 | 50033)
                && !name.is_empty()
            {
                let mut node = Element::new(Some(parent), control_name(kind), &name, None);
                node.structural = true;
                if let Some(i) = self.add(node) {
                    semantic_parent = i;
                }
            }
            if can_emit && !interactive_kind(kind) {
                if let Ok(scroll) =
                    e.GetCachedPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                {
                    let v = scroll
                        .CachedVerticallyScrollable()
                        .is_ok_and(|v| v.as_bool());
                    let h = scroll
                        .CachedHorizontallyScrollable()
                        .is_ok_and(|v| v.as_bool());
                    if v || h {
                        let mut node = Element::new(
                            Some(semantic_parent),
                            if localized.is_empty() {
                                control_name(kind)
                            } else {
                                &localized
                            },
                            if name.is_empty() { "DOM" } else { &name },
                            bounds,
                        );
                        node.scrollable = true;
                        node.element = Some(e.clone());
                        node.source_bounds = raw_box;
                        if v {
                            node.meta.push(format!(
                                "v:{:.2}%",
                                scroll.CachedVerticalScrollPercent().unwrap_or(0.0)
                            ));
                        }
                        if h {
                            node.meta.push(format!(
                                "h:{:.2}%",
                                scroll.CachedHorizontalScrollPercent().unwrap_or(0.0)
                            ));
                        }
                        self.add(node);
                    }
                }
            }
            if let Ok(children) = e.GetCachedChildren() {
                let len = children.Length().unwrap_or(0).min(30000);
                for index in (0..len).rev() {
                    if let Ok(child) = children.GetElement(index) {
                        stack.push((child, semantic_parent, depth + 1, is_dom, dom_clip));
                    }
                }
            }
        }
        if browser && use_dom && !found_dom {
            let before = self.elements.len();
            super::ia2::collect(self, c, hwnd, root, clip)?;
            let content = self.elements[before..]
                .iter()
                .filter(|e| e.interactive || e.kind == "text")
                .count();
            if content == 0 {
                self.notes.push(format!("Browser DOM unavailable for {title:?}: neither UIA document nor MSAA/IAccessible2 content was exposed."));
            } else {
                self.notes.push(format!(
                    "Browser DOM backend: MSAA/IAccessible2 fallback; {} captured elements.",
                    content
                ));
            }
        }
        Ok(())
    }
    unsafe fn words(
        &mut self,
        c: &Coordinator,
        e: &IUIAutomationElement,
        parent: usize,
        clip: Rect,
    ) -> Result<(), String> {
        let Ok(pattern) = e.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        else {
            return Ok(());
        };
        let Ok(visible) = pattern.GetVisibleRanges() else {
            return Ok(());
        };
        for index in 0..visible.Length().unwrap_or(0).min(LIMIT as i32) {
            let Ok(range) = visible.GetElement(index) else {
                continue;
            };
            let Ok(word) = range.Clone() else { continue };
            if word.ExpandToEnclosingUnit(TextUnit_Word).is_err() {
                continue;
            }
            for _ in 0..LIMIT {
                c.checkpoint()?;
                if self.emitted >= LIMIT {
                    self.truncated = true;
                    return Ok(());
                }
                if word
                    .CompareEndpoints(
                        TextPatternRangeEndpoint_Start,
                        &range,
                        TextPatternRangeEndpoint_End,
                    )
                    .unwrap_or(0)
                    >= 0
                {
                    break;
                }
                let text = word
                    .GetText(201)
                    .map(|v| v.to_string().trim().to_string())
                    .unwrap_or_default();
                if !text.is_empty() {
                    if let Ok(array) = word.GetBoundingRectangles() {
                        let rects = safe_array::<f64>(array);
                        for r in rects.chunks_exact(4) {
                            let rect = Rect {
                                left: r[0] as i32,
                                top: r[1] as i32,
                                right: (r[0] + r[2]) as i32,
                                bottom: (r[1] + r[3]) as i32,
                            };
                            if let Some(rect) = rect.intersect(clip) {
                                let mut node =
                                    Element::new(Some(parent), "word", &text, Some(rect));
                                node.interactive = true;
                                self.add(node);
                            }
                        }
                    }
                }
                if word.Move(TextUnit_Word, 1).unwrap_or(0) == 0 {
                    break;
                }
            }
        }
        Ok(())
    }
}
pub unsafe fn safe_array<T: Copy>(array: *mut SAFEARRAY) -> Vec<T> {
    if array.is_null() {
        return vec![];
    }
    if SafeArrayGetDim(array) != 1
        || SafeArrayGetElemsize(array) as usize != std::mem::size_of::<T>()
    {
        let _ = SafeArrayDestroy(array);
        return vec![];
    }
    let mut data = std::ptr::null_mut();
    let result = (|| {
        let low = SafeArrayGetLBound(array, 1).ok()?;
        let high = SafeArrayGetUBound(array, 1).ok()?;
        let n = i64::from(high) - i64::from(low) + 1;
        if !(0..=1_000_000).contains(&n) {
            return None;
        }
        SafeArrayAccessData(array, &mut data).ok()?;
        if data.is_null() {
            let _ = SafeArrayUnaccessData(array);
            return None;
        }
        let out = std::slice::from_raw_parts(data as *const T, n as usize).to_vec();
        let _ = SafeArrayUnaccessData(array);
        Some(out)
    })();
    let _ = SafeArrayDestroy(array);
    result.unwrap_or_default()
}
unsafe fn cached_int(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<i32> {
    let mut v = e.GetCachedPropertyValue(id).ok()?;
    let result = VariantToInt32(&v).ok();
    let _ = VariantClear(&mut v);
    result
}
fn interactive_kind(kind: i32) -> bool {
    matches!(
        kind,
        50000
            | 50002
            | 50003
            | 50004
            | 50005
            | 50007
            | 50008
            | 50011
            | 50014
            | 50013
            | 50015
            | 50016
            | 50019
            | 50024
            | 50029
            | 50031
            | 50035
    )
}
fn interactive_role(role: i32) -> bool {
    matches!(role,0x1e|0x21..=0x25|0x2a..=0x2f|0x33|0x34|0x38|0x3e)
}
fn control_name(kind: i32) -> &'static str {
    match kind {
        50000 => "button",
        50002 => "check box",
        50003 => "combo box",
        50004 => "edit",
        50005 => "link",
        50006 => "image",
        50007 => "list item",
        50008 => "list",
        50009 => "menu",
        50010 => "menu bar",
        50011 => "menu item",
        50012 => "progress bar",
        50013 => "radio button",
        50014 => "scroll bar",
        50015 => "slider",
        50016 => "spinner",
        50017 => "status bar",
        50018 => "tab",
        50019 => "tab item",
        50020 => "text",
        50021 => "tool bar",
        50022 => "tool tip",
        50023 => "tree",
        50024 => "tree item",
        50025 => "custom",
        50026 => "group",
        50028 => "data grid",
        50029 => "data item",
        50030 => "document",
        50032 => "window",
        50033 => "pane",
        50035 => "header item",
        _ => "control",
    }
}
unsafe fn metadata(e: &IUIAutomationElement, kind: i32) -> Vec<String> {
    let mut out = vec![];
    let password = e.CachedIsPassword().is_ok_and(|v| v.as_bool());
    if e.CachedHasKeyboardFocus().is_ok_and(|v| v.as_bool()) {
        out.push("focused".into());
    }
    if password {
        out.push("password".into());
    }
    if !password {
        let value = e
            .GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .and_then(|p| p.CachedValue())
            .or_else(|_| {
                e.GetCachedPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                    UIA_LegacyIAccessiblePatternId,
                )
                .and_then(|p| p.CachedValue())
            })
            .ok()
            .map(|v| v.to_string());
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.push(format!("value:{:?}", shown(&v, 200)));
        }
    }
    if let Ok(p) = e.GetCachedPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId) {
        if let (Ok(value), Ok(min), Ok(max)) =
            (p.CachedValue(), p.CachedMinimum(), p.CachedMaximum())
        {
            out.push(format!("value:{value:.2}"));
            out.push(format!("range:{min:.2}-{max:.2}"));
        }
    }
    if cached_int(e, UIA_SelectionItemIsSelectedPropertyId) == Some(-1) {
        out.push("selected".into());
    }
    if matches!(kind, 50000 | 50002) {
        if let Some(toggle) = cached_int(e, UIA_ToggleToggleStatePropertyId) {
            if toggle == 0 || toggle == 1 {
                out.push(format!("toggle:{}", if toggle == 1 { "on" } else { "off" }));
            }
        }
    }
    if let Some(state) = cached_int(e, UIA_ExpandCollapseExpandCollapseStatePropertyId) {
        if let Some(value) = match state {
            0 => Some("collapsed"),
            1 => Some("expanded"),
            2 => Some("partially expanded"),
            _ => None,
        } {
            out.push(format!("state:{value}"));
        }
    }
    if let Ok(key) = e.CachedAcceleratorKey() {
        let key = key.to_string();
        if !key.is_empty() {
            out.push(format!("shortcut:{key}"));
        }
    }
    if !password && matches!(kind, 50004 | 50030) {
        if let Ok(pattern) = e.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
            if let Ok(selection) = pattern.GetSelection() {
                if selection.Length().unwrap_or(0) > 0 {
                    if let Ok(range) = selection.GetElement(0) {
                        if let Ok(text) = range.GetText(101) {
                            let text = text.to_string();
                            if !text.is_empty() {
                                let all = pattern.DocumentRange().is_ok_and(|doc| {
                                    range
                                        .CompareEndpoints(
                                            TextPatternRangeEndpoint_Start,
                                            &doc,
                                            TextPatternRangeEndpoint_Start,
                                        )
                                        .unwrap_or(-1)
                                        == 0
                                        && range
                                            .CompareEndpoints(
                                                TextPatternRangeEndpoint_End,
                                                &doc,
                                                TextPatternRangeEndpoint_End,
                                            )
                                            .unwrap_or(-1)
                                            == 0
                                });
                                out.push(if all {
                                    "selected:all".into()
                                } else {
                                    format!("selected:{:?}", shown(&text, 100))
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out
}
