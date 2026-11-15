use super::{control::Coordinator, logic};
use std::{sync::Arc, time::Duration};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
pub const MARKER: usize = 0x4d415841;
fn send(inputs: &[INPUT]) -> Result<(), String> {
    let n = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if n != inputs.len() as u32 {
        Err(
            "OUTCOME_UNKNOWN: SendInput was blocked or partially accepted (UIPI); do not replay"
                .into(),
        )
    } else {
        Ok(())
    }
}
pub fn send_button(key: u16, mouse: bool, down: bool) -> Result<(), String> {
    let input = if mouse {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: match (key, down) {
                        (1, true) => MOUSEEVENTF_LEFTDOWN,
                        (1, false) => MOUSEEVENTF_LEFTUP,
                        (2, true) => MOUSEEVENTF_RIGHTDOWN,
                        (2, false) => MOUSEEVENTF_RIGHTUP,
                        (3, true) => MOUSEEVENTF_MIDDLEDOWN,
                        _ => MOUSEEVENTF_MIDDLEUP,
                    },
                    time: 0,
                    dwExtraInfo: MARKER,
                },
            },
        }
    } else {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: 0,
                    dwFlags: (if down { 0 } else { KEYEVENTF_KEYUP })
                        | (if matches!(key, 0x21..=0x28 | 0x2d | 0x2e | 0x5b | 0x5c) {
                            KEYEVENTF_EXTENDEDKEY
                        } else {
                            0
                        }),
                    time: 0,
                    dwExtraInfo: MARKER,
                },
            },
        }
    };
    send(&[input])
}
pub fn validate(p: [i32; 2]) -> Result<(), String> {
    if unsafe { MonitorFromPoint(POINT { x: p[0], y: p[1] }, MONITOR_DEFAULTTONULL) }.is_null() {
        Err("Point is outside the physical desktop".into())
    } else {
        Ok(())
    }
}
pub fn cursor() -> Result<[i32; 2], String> {
    let mut p: POINT = unsafe { std::mem::zeroed() };
    if unsafe { GetCursorPos(&mut p) } == 0 {
        Err("GetCursorPos failed".into())
    } else {
        Ok([p.x, p.y])
    }
}
pub struct Input(pub Arc<Coordinator>);
impl Input {
    pub fn key(&self, key: u16) -> Result<(), String> {
        self.0.hold(key, false, true)?;
        self.0.hold(key, false, false)
    }
    pub fn chord(&self, keys: &[u16]) -> Result<(), String> {
        let result = (|| {
            for k in keys {
                self.0.hold(*k, false, true)?;
            }
            Ok(())
        })();
        for k in keys.iter().rev() {
            let _ = self.0.hold(*k, false, false);
        }
        result
    }
    pub fn shortcut(&self, text: &str) -> Result<(), String> {
        self.chord(&logic::shortcut(text)?)?;
        self.0.sleep(Duration::from_millis(50))
    }
    pub fn unicode(&self, ch: char) -> Result<(), String> {
        if ch == '\n' {
            return self.key(13);
        }
        if ch == '\t' {
            return self.key(9);
        }
        self.0.checkpoint()?;
        let mut utf16 = [0; 2];
        let units = ch.encode_utf16(&mut utf16);
        let mut events = Vec::new();
        for unit in units {
            for up in [false, true] {
                events.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: 0,
                            wScan: *unit,
                            dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
                            time: 0,
                            dwExtraInfo: MARKER,
                        },
                    },
                });
            }
        }
        send(&events)
    }
    pub fn move_to(&self, p: [i32; 2]) -> Result<(), String> {
        validate(p)?;
        self.0.checkpoint()?;
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            send(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: ((p[0] - x) as i64 * 65535 / (w - 1).max(1) as i64) as i32,
                        dy: ((p[1] - y) as i64 * 65535 / (h - 1).max(1) as i64) as i32,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: MARKER,
                    },
                },
            }])
        }
    }
    pub fn click(&self, p: [i32; 2], button: &str, count: i64) -> Result<(), String> {
        if count < 0 {
            return Err("clicks must be 0 or more".into());
        }
        let k = match button {
            "left" => 1,
            "right" => 2,
            "middle" => 3,
            _ => return Err("Unsupported mouse button".into()),
        };
        self.move_to(p)?;
        for _ in 0..count {
            self.0.hold(k, true, true)?;
            self.0.hold(k, true, false)?;
            self.0.sleep(Duration::from_millis(50))?;
        }
        Ok(())
    }
    pub fn drag(
        &self,
        to: [i32; 2],
        from: Option<[i32; 2]>,
        duration: Option<f64>,
    ) -> Result<[i32; 2], String> {
        validate(to)?;
        if let Some(p) = from {
            validate(p)?;
            self.move_to(p)?;
        }
        let start = cursor()?;
        self.0.hold(1, true, true)?;
        let r = (|| {
            let steps = duration
                .map(|d| (d / 0.01).ceil() as i32)
                .unwrap_or(
                    ((to[0] - start[0]).abs().max((to[1] - start[1]).abs()) / 20).clamp(1, 80),
                )
                .clamp(1, 200);
            for i in 1..=steps {
                self.move_to([
                    start[0] + ((to[0] - start[0]) as f64 * i as f64 / steps as f64).round() as i32,
                    start[1] + ((to[1] - start[1]) as f64 * i as f64 / steps as f64).round() as i32,
                ])?;
                self.0.sleep(Duration::from_secs_f64(
                    duration.map(|d| d / steps as f64).unwrap_or(0.01),
                ))?;
            }
            Ok(start)
        })();
        let _ = self.0.hold(1, true, false);
        r
    }
    pub fn scroll(&self, horizontal: bool, direction: &str, n: i64) -> Result<(), String> {
        let sign = match (horizontal, direction) {
            (false, "up") | (true, "right") => 1,
            (false, "down") | (true, "left") => -1,
            _ => return Err("direction does not match scroll type".into()),
        };
        if n < 0 {
            return Err("wheel_times must be nonnegative".into());
        }
        for _ in 0..n {
            self.0.checkpoint()?;
            send(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: (sign * 120) as u32,
                        dwFlags: if horizontal {
                            MOUSEEVENTF_HWHEEL
                        } else {
                            MOUSEEVENTF_WHEEL
                        },
                        time: 0,
                        dwExtraInfo: MARKER,
                    },
                },
            }])?;
        }
        Ok(())
    }
}
