//! Successful Snapshot provenance, local to one worker and caller.
use std::collections::HashMap;
#[derive(Clone)]
pub struct Label {
    pub point: [i32; 2],
    pub window: usize,
    pub process: u32,
    pub window_rect: [i32; 4],
    pub bounds: [i32; 4],
    pub child: usize,
    pub child_rect: Option<[i32; 4]>,
    #[cfg(windows)]
    pub element: Option<windows::Win32::UI::Accessibility::IUIAutomationElement>,
    pub element_rect: Option<[i32; 4]>,
}
/// Pure freshness decision; OS/UIA collection remains at the Windows boundary.
pub fn fresh(
    label: &Label,
    window_rect: Option<[i32; 4]>,
    hit_window: usize,
    element_unchanged: bool,
) -> bool {
    window_rect == Some(label.window_rect)
        && hit_window == label.window
        && label.bounds[0] <= label.point[0]
        && label.point[0] < label.bounds[2]
        && label.bounds[1] <= label.point[1]
        && label.point[1] < label.bounds[3]
        && element_unchanged
}
#[derive(Default)]
pub struct Labels {
    owner: String,
    points: Option<HashMap<i64, Label>>,
}
impl Labels {
    pub fn select_owner(&mut self, owner: &str) {
        if self.owner != owner {
            self.owner = owner.into();
            self.points = None;
        }
    }
    pub fn clear(&mut self) {
        self.points = None;
    }
    pub fn replace(&mut self, points: HashMap<i64, Label>) {
        self.points = Some(points);
    }
    pub fn get(&self, label: i64) -> Result<&Label, String> {
        self.points
            .as_ref()
            .ok_or("Desktop state is empty. Please call Snapshot first.")?
            .get(&label)
            .ok_or_else(|| {
                format!("Failed to find element with label {label}; take a Snapshot first.")
            })
    }
    #[cfg(windows)]
    pub fn resolve(&self, label: i64) -> Result<[i32; 2], String> {
        use windows_sys::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
        unsafe fn rect(hwnd: HWND) -> Option<[i32; 4]> {
            let mut r: RECT = std::mem::zeroed();
            if IsWindow(hwnd) == 0
                || IsWindowVisible(hwnd) == 0
                || IsIconic(hwnd) != 0
                || GetWindowRect(hwnd, &mut r) == 0
            {
                None
            } else {
                Some([r.left, r.top, r.right, r.bottom])
            }
        }
        let target = self.get(label)?;
        unsafe {
            let hwnd = target.window as HWND;
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let hit = WindowFromPoint(POINT {
                x: target.point[0],
                y: target.point[1],
            });
            let child_ok = target.child_rect.is_none_or(|r| {
                hit as usize == target.child && rect(target.child as HWND) == Some(r)
            });
            // One property read from the retained element is cheaper and safer than a new tree walk.
            let element_ok = match (&target.element, target.element_rect) {
                (Some(e), Some(r)) => {
                    e.CurrentBoundingRectangle()
                        .is_ok_and(|b| [b.left, b.top, b.right, b.bottom] == r)
                        && e.CurrentIsOffscreen().is_ok_and(|b| !b.as_bool())
                }
                _ => true, // MSAA/word labels still have window + native-child checks.
            };
            if pid != target.process
                || !fresh(
                    target,
                    rect(hwnd),
                    GetAncestor(hit, GA_ROOT) as usize,
                    child_ok && element_ok,
                )
            {
                return Err(format!("STALE_LABEL: label {label} moved, disappeared or is occluded; take a new Snapshot before input."));
            }
        }
        Ok(target.point)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn label_requires_unchanged_window_hit_and_element() {
        let l = Label {
            point: [20, 30],
            window: 7,
            process: 1,
            window_rect: [0, 0, 100, 100],
            bounds: [10, 20, 30, 40],
            child: 0,
            child_rect: None,
            #[cfg(windows)]
            element: None,
            element_rect: None,
        };
        assert!(fresh(&l, Some(l.window_rect), 7, true));
        assert!(!fresh(&l, None, 7, true));
        assert!(!fresh(&l, Some([1, 0, 101, 100]), 7, true));
        assert!(!fresh(&l, Some(l.window_rect), 8, true));
        assert!(!fresh(&l, Some(l.window_rect), 7, false));
    }
}
