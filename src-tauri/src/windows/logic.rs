//! Platform-independent policy. No hooks or desktop effects in this module.
use serde_json::Value;

pub fn direct(tool: &str, args: &Value) -> bool {
    match tool {
        "Click" | "Type" | "Shortcut" | "Scroll" | "Move" | "MultiSelect" | "MultiEdit" => true,
        "Window" => matches!(
            args["mode"].as_str(),
            Some("focus" | "move" | "click" | "switch_desktop" | "move_desktop")
        ),
        "App" => matches!(args["mode"].as_str(), Some("resize" | "switch")),
        _ => false,
    }
}
/// Pipe-local admission identity, independent of App resource/client stop epochs.
#[derive(Default)]
pub struct Admission {
    latest: u64,
    cancelled_through: u64,
}
impl Admission {
    pub fn cancel(&mut self, generation: u64) -> bool {
        self.cancelled_through = self.cancelled_through.max(generation);
        self.latest != 0 && self.latest <= generation
    }
    pub fn active(&self, generation: u64) -> bool {
        generation != 0 && generation == self.latest && generation > self.cancelled_through
    }
    pub fn admit(&mut self, generation: u64) -> bool {
        if generation == 0 || generation <= self.latest || generation <= self.cancelled_through {
            return false;
        }
        self.latest = generation;
        true
    }
}
pub fn control_state(
    enabled: bool,
    unavailable: bool,
    taken: bool,
    pending: bool,
    active: bool,
) -> &'static str {
    if !enabled {
        "disabled"
    } else if unavailable {
        "unavailable"
    } else if taken {
        "user"
    } else if pending && active {
        "takeover_pending"
    } else if active {
        "ai"
    } else {
        "ready"
    }
}
pub fn paste_path(text: &str) -> bool {
    text.chars().count() >= 20 && !text.chars().any(|c| matches!(c, '\n' | '\t' | '{' | '}'))
}
pub fn takeover_chord(key: u32, down: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
    key == 8 && down && ctrl && alt && shift
}
#[derive(Default)]
pub struct MousePath {
    device: usize,
    at_ms: u64,
    total: f64,
}
impl MousePath {
    pub fn update(&mut self, device: usize, at_ms: u64, dx: i32, dy: i32, moved: bool) -> bool {
        if !moved {
            return false;
        }
        if device != self.device || at_ms.saturating_sub(self.at_ms) >= 500 {
            self.total = 0.0;
        }
        self.device = device;
        self.at_ms = at_ms;
        self.total += (dx as f64).hypot(dy as f64);
        self.total >= 500.0
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
pub fn shortcut(text: &str) -> Result<Vec<u16>, String> {
    let mut keys = Vec::new();
    for token in text.split('+') {
        let t = token.trim().to_lowercase();
        let k = match t.as_str() {
            "ctrl" | "control" => 0x11,
            "alt" | "option" => 0x12,
            "shift" => 0x10,
            "win" | "windows" | "super" | "command" => 0x5b,
            "enter" | "return" => 0x0d,
            "tab" => 9,
            "esc" | "escape" => 0x1b,
            "back" | "backspace" => 8,
            "delete" | "del" => 0x2e,
            "home" => 0x24,
            "end" => 0x23,
            "left" => 0x25,
            "up" => 0x26,
            "right" => 0x27,
            "down" => 0x28,
            "pageup" | "pgup" => 0x21,
            "pagedown" | "pgdn" => 0x22,
            "space" => 0x20,
            "insert" | "ins" => 0x2d,
            "capslock" | "capital" => 0x14,
            "scrolllock" | "scroll" => 0x91,
            "numlock" => 0x90,
            "pause" | "break" => 0x13,
            "comma" | "," => 0xbc,
            "period" | "dot" | "." => 0xbe,
            "minus" | "dash" | "hyphen" | "-" => 0xbd,
            "plus" | "equals" | "equal" | "=" => 0xbb,
            "semicolon" | ";" => 0xba,
            "slash" | "/" => 0xbf,
            "backslash" | "\\" => 0xdc,
            "grave" | "backtick" | "`" => 0xc0,
            "bracketleft" | "lbracket" | "[" => 0xdb,
            "bracketright" | "rbracket" | "]" => 0xdd,
            "apostrophe" | "quote" | "'" => 0xde,
            "printscreen" | "prtsc" => 0x2c,
            _ if t.starts_with('f')
                && t[1..].parse::<u16>().is_ok_and(|v| (1..=24).contains(&v)) =>
            {
                0x6f + t[1..].parse::<u16>().unwrap()
            }
            _ if t.len() == 1 && t.as_bytes()[0].is_ascii_alphanumeric() => {
                t.as_bytes()[0].to_ascii_uppercase() as u16
            }
            _ => return Err(format!("Unsupported shortcut key: {token}")),
        };
        if keys.contains(&k) {
            return Err("Duplicate shortcut key".into());
        }
        keys.push(k);
    }
    if keys.is_empty() {
        return Err("shortcut required".into());
    }
    Ok(keys)
}
fn processed(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 100.0;
    }
    let mut row = vec![0; b.len() + 1];
    for c in &a {
        let mut prev = 0;
        for (j, d) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = if c == d {
                prev + 1
            } else {
                row[j + 1].max(row[j])
            };
            prev = old;
        }
    }
    200.0 * row[b.len()] as f64 / (a.len() + b.len()) as f64
}
fn partial(a: &str, b: &str) -> f64 {
    let (a, b) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    let chars: Vec<_> = b.chars().collect();
    let n = a.chars().count();
    if n == 0 {
        return 0.0;
    }
    chars
        .windows(n)
        .map(|v| ratio(a, &v.iter().collect::<String>()))
        .fold(0.0, f64::max)
}
/// FuzzyWuzzy's default WRatio family: normalized ratio, partial and token scores.
pub fn fuzzy(a: &str, b: &str) -> f64 {
    let a = processed(a);
    let b = processed(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let len_ratio = a.chars().count().max(b.chars().count()) as f64
        / a.chars().count().min(b.chars().count()) as f64;
    let sort = |s: &str| {
        let mut t: Vec<_> = s.split_whitespace().collect();
        t.sort();
        t.join(" ")
    };
    let mut aw: Vec<_> = a.split_whitespace().collect();
    aw.sort();
    aw.dedup();
    let mut bw: Vec<_> = b.split_whitespace().collect();
    bw.sort();
    bw.dedup();
    let common = aw
        .iter()
        .filter(|w| bw.contains(w))
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let left = aw.join(" ");
    let right = bw.join(" ");
    let score = if len_ratio >= 1.5 {
        let scale = if len_ratio > 8.0 { 0.6 } else { 0.9 };
        let mut s = ratio(&a, &b)
            .max(partial(&a, &b) * scale)
            .max(partial(&sort(&a), &sort(&b)) * 0.95 * scale);
        if !common.is_empty() {
            s = s
                .max(partial(&common, &left) * 0.95 * scale)
                .max(partial(&common, &right) * 0.95 * scale);
        }
        s
    } else {
        let mut s = ratio(&a, &b).max(ratio(&sort(&a), &sort(&b)) * 0.95);
        if !common.is_empty() {
            s = s
                .max(ratio(&common, &left) * 0.95)
                .max(ratio(&common, &right) * 0.95);
        }
        s
    };
    score.round()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_stop_order_and_new_independent_calls() {
        let mut a = Admission::default();
        assert!(!a.cancel(1)); // stop reaches the worker before the old call
        assert!(!a.admit(1));
        assert!(a.admit(2));
        assert!(a.active(2));
        assert!(a.cancel(2)); // call reaches the worker first
        assert!(!a.active(2));
        assert!(!a.admit(2));
        assert!(a.admit(3));
        assert!(!a.active(2)); // an old sleeper cannot resume after admission reset
        assert!(a.active(3));
        assert!(!a.cancel(1)); // delayed old cancellation cannot cancel generation 3
        assert!(a.active(3));
        assert!(!a.admit(0));
    }
    #[test]
    fn baseline_control_status_precedence() {
        assert_eq!(control_state(false, true, true, true, true), "disabled");
        assert_eq!(control_state(true, true, true, true, true), "unavailable");
        assert_eq!(control_state(true, false, true, true, true), "user");
        assert_eq!(
            control_state(true, false, false, true, true),
            "takeover_pending"
        );
        assert_eq!(control_state(true, false, false, false, true), "ai");
        assert_eq!(control_state(true, false, false, false, false), "ready");
    }
    #[test]
    fn mouse_takeover_resets_device_and_pause() {
        let mut p = MousePath::default();
        assert!(!p.update(1, 1, 300, 0, true));
        assert!(!p.update(2, 2, 300, 0, true));
        assert!(!p.update(2, 502, 300, 0, true));
        assert!(!p.update(2, 503, 900, 0, false));
        assert!(p.update(2, 504, 200, 0, true));
    }
    #[test]
    fn chord_requires_all_modifiers() {
        assert!(takeover_chord(8, true, true, true, true));
        assert!(!takeover_chord(8, true, true, false, true));
        assert!(!takeover_chord(8, false, true, true, true));
    }
    #[test]
    fn shortcuts_are_explicit() {
        assert_eq!(shortcut("Ctrl+Shift+End").unwrap(), [17, 16, 35]);
        assert_eq!(shortcut("f24").unwrap(), [135]);
        assert!(shortcut("ctrl++a").is_err());
    }
    #[test]
    fn type_path_counts_characters() {
        assert!(!paste_path("Rust中文验证✓"));
        assert!(paste_path(&"中".repeat(20)));
        assert!(!paste_path(&format!("{}\n", "x".repeat(20))));
    }
    #[test]
    fn fuzzy_cutoff() {
        assert!(fuzzy("Notepad", "Notepad") >= 100.0);
        assert!(fuzzy("unique.txt", "*unique.txt - Notepad") >= 70.0);
        assert!(fuzzy("Notepad", "Calculator") < 70.0);
        assert!(fuzzy("Visual Studio Code", "Code Visual Studio") >= 95.0);
    }
}
