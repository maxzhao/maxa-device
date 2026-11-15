//! Firefox web-content fallback, ported from Windows-MCP tree/ia2.py (MIT).
//! Firefox's IAccessible2 providers expose the MSAA IAccessible traversal used here.
use super::{
    capture::Rect,
    control::Coordinator,
    tree::{clean, Element, Tree, LIMIT},
};
use std::mem::ManuallyDrop;
use windows::{
    core::{Interface, BSTR},
    Win32::{
        Foundation::HWND,
        System::Variant::{
            VariantClear, VariantToInt32, VariantToString, VARIANT, VARIANT_0, VARIANT_0_0,
            VARIANT_0_0_0, VT_I4,
        },
        UI::Accessibility::{AccessibleObjectFromWindow, IAccessible},
    },
};
unsafe fn child(id: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: id },
            }),
        },
    }
}
unsafe fn number(mut v: VARIANT) -> Option<i32> {
    let value = VariantToInt32(&v).ok();
    let _ = VariantClear(&mut v);
    value
}
unsafe fn role_name(role: i32, acc: &IAccessible, id: &VARIANT) -> String {
    let name = match role {
        0x0f => "document",
        0x14 => "grouping",
        0x16 => "toolbar",
        0x18 => "table",
        0x1e => "link",
        0x21 => "list",
        0x22 => "list item",
        0x23 => "outline",
        0x24 => "outline item",
        0x25 => "tab",
        0x28 => "graphic",
        0x29 | 0x2a => "text",
        0x2b | 0x38 | 0x3e => "button",
        0x2c => "check box",
        0x2d => "radio button",
        0x2e => "combo box",
        0x2f => "drop list",
        0x33 => "slider",
        0x3c => "tab list",
        _ => "",
    };
    if !name.is_empty() {
        return name.into();
    }
    if let Ok(mut role) = acc.get_accRole(id) {
        let mut buffer = [0u16; 256];
        let result = VariantToString(&role, &mut buffer).ok().map(|_| {
            String::from_utf16_lossy(
                &buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())],
            )
        });
        let _ = VariantClear(&mut role);
        if let Some(name) = result {
            return name.to_lowercase();
        }
    }
    "unknown".into()
}
pub unsafe fn collect(
    tree: &mut Tree,
    c: &Coordinator,
    hwnd: HWND,
    parent: usize,
    clip: Rect,
) -> Result<(), String> {
    let mut object = std::ptr::null_mut();
    if let Err(e) = AccessibleObjectFromWindow(hwnd, 0xffff_fffc, &IAccessible::IID, &mut object) {
        tree.notes
            .push(format!("MSAA/IAccessible2 acquisition unavailable: {e}"));
        return Ok(());
    }
    if object.is_null() {
        tree.notes
            .push("MSAA/IAccessible2 returned no client object".into());
        return Ok(());
    }
    let root = IAccessible::from_raw(object);
    let self_id = child(0);
    let mut stack = vec![(root, 0i32, parent, 0usize, false)];
    let mut visited = 0;
    while let Some((acc, simple_id, semantic_parent, depth, in_document)) = stack.pop() {
        c.checkpoint()?;
        visited += 1;
        if depth > 500
            || visited > 30000
            || tree
                .elements
                .iter()
                .filter(|e| e.interactive || e.scrollable || e.kind == "text")
                .count()
                >= LIMIT
        {
            tree.truncated = true;
            break;
        }
        let id = if simple_id == 0 {
            &self_id
        } else {
            &child(simple_id)
        };
        let role = acc
            .get_accRole(id)
            .ok()
            .and_then(|v| number(v))
            .unwrap_or(-1);
        let state = acc
            .get_accState(id)
            .ok()
            .and_then(|v| number(v))
            .unwrap_or(0);
        let mut left = 0;
        let mut top = 0;
        let mut width = 0;
        let mut height = 0;
        let located = acc
            .accLocation(&mut left, &mut top, &mut width, &mut height, id)
            .is_ok()
            && width > 0
            && height > 0;
        let bounds = if located {
            Some(Rect {
                left,
                top,
                right: left.saturating_add(width),
                bottom: top.saturating_add(height),
            })
        } else {
            None
        };
        let visible = state & (0x8000 | 0x10000) == 0 && located;
        if depth > 0 && !visible {
            continue;
        }
        let in_document = in_document || role == 0x0f;
        let mut next_parent = semantic_parent;
        if in_document && visible {
            let name = acc
                .get_accName(id)
                .unwrap_or_else(|_| BSTR::new())
                .to_string();
            let value = if state & 0x20000000 != 0 {
                String::new()
            } else {
                acc.get_accValue(id)
                    .unwrap_or_else(|_| BSTR::new())
                    .to_string()
            };
            let kind = role_name(role, &acc, id);
            if matches!(role, 0x29 | 0x2a | 0x28) && !name.trim().is_empty() {
                tree.add(Element::new(
                    Some(semantic_parent),
                    "text",
                    name.trim(),
                    None,
                ));
            } else if matches!(
                role,
                0x1e | 0x22 | 0x24 | 0x25 | 0x2b..=0x2f | 0x33 | 0x38 | 0x3e
            ) || state & 0x400000 != 0
            {
                if let Some(bounds) = bounds.and_then(|r| r.intersect(clip)) {
                    let mut e = Element::new(
                        Some(semantic_parent),
                        &kind,
                        if !name.trim().is_empty() {
                            name.trim()
                        } else if !value.is_empty() {
                            &value
                        } else {
                            &kind
                        },
                        Some(bounds),
                    );
                    e.interactive = true;
                    if state & 4 != 0 {
                        e.meta.push("focused".into());
                    }
                    if state & 0x20000000 != 0 {
                        e.meta.push("password".into());
                    } else if !value.is_empty() {
                        if role == 0x1e {
                            e.meta.push(format!("url:{:?}", clean(&value)));
                        } else if matches!(role, 0x2e | 0x2f | 0x33) {
                            e.meta.push(format!("value:{:?}", clean(&value)));
                        }
                    }
                    if let Some(i) = tree.add(e) {
                        next_parent = i;
                    }
                }
            } else if role == 0x0f {
                if let Some(bounds) = bounds.and_then(|r| r.intersect(clip)) {
                    let mut e = Element::new(
                        Some(semantic_parent),
                        "document",
                        if name.is_empty() { "DOM" } else { &name },
                        Some(bounds),
                    );
                    e.scrollable = true;
                    e.meta
                        .push("scroll state:unknown (MSAA/IAccessible2)".into());
                    if let Some(i) = tree.add(e) {
                        next_parent = i;
                    }
                }
            }
        }
        if simple_id == 0 {
            let count = acc.accChildCount().unwrap_or(0).clamp(0, 30000);
            for index in (1..=count).rev() {
                let id = child(index);
                match acc.get_accChild(&id) {
                    Ok(dispatch) => {
                        if let Ok(child) = dispatch.cast::<IAccessible>() {
                            stack.push((child, 0, next_parent, depth + 1, in_document));
                        }
                    }
                    Err(_) => {
                        stack.push((acc.clone(), index, next_parent, depth + 1, in_document));
                    }
                }
            }
        }
    }
    Ok(())
}
