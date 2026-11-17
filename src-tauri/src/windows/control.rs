use super::{
    indicator::Indicator,
    logic::{control_state, takeover_chord, Admission, MousePath},
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{Input::*, WindowsAndMessaging::*},
};
const COMMAND: u32 = WM_APP + 1;
static CURRENT: OnceLock<Arc<Coordinator>> = OnceLock::new();
#[derive(Default)]
struct Physical {
    keys: HashSet<u32>,
    path: MousePath,
    position: Option<(i32, i32)>,
    raw_moved: bool,
}
struct State {
    owner: String,
    control_owner: String,
    app_epoch: u64,
    active: bool,
    generation: u64,
    taken: bool,
    last_input: u64,
    admission: Admission,
    operation_generation: u64,
    operation: String,
    input_session: Option<u32>,
}
enum Command {
    Language(bool),
    Begin(String, u64, mpsc::Sender<Result<(), String>>),
    End(u64),
    Takeover,
    CaptureBegin(mpsc::Sender<Result<bool, String>>),
    CaptureEnd(mpsc::Sender<Result<(), String>>),
}
pub struct Coordinator {
    // Prevent a late old stop/end from releasing a newly admitted operation's input.
    transition: Mutex<()>,
    state: Mutex<State>,
    held: Mutex<Vec<(u16, bool)>>,
    physical: Mutex<Physical>,
    cancelled: AtomicBool,
    stopped: AtomicBool,
    takeover_pending: AtomicBool,
    heartbeat: AtomicU64,
    failure: Mutex<Option<String>>,
    busy: AtomicBool,
    started: AtomicU64,
    completed: AtomicU64,
    deadline: AtomicU64,
    physical_sequence: AtomicU64,
    thread: AtomicU32,
    commands: mpsc::Sender<Command>,
    emit: Arc<dyn Fn(Value) + Send + Sync>,
}
impl Coordinator {
    pub fn start(emit: Arc<dyn Fn(Value) + Send + Sync>) -> Result<Arc<Self>, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let c = Arc::new(Self {
            transition: Mutex::new(()),
            state: Mutex::new(State {
                owner: String::new(),
                control_owner: String::new(),
                app_epoch: 0,
                active: false,
                generation: 0,
                taken: false,
                last_input: 0,
                admission: Admission::default(),
                operation_generation: 0,
                operation: String::new(),
                input_session: None,
            }),
            physical: Mutex::new(Physical::default()),
            held: Mutex::new(vec![]),
            cancelled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            takeover_pending: AtomicBool::new(false),
            heartbeat: AtomicU64::new(0),
            failure: Mutex::new(None),
            busy: AtomicBool::new(false),
            started: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            physical_sequence: AtomicU64::new(0),
            thread: AtomicU32::new(0),
            commands: tx,
            emit,
        });
        CURRENT
            .set(c.clone())
            .map_err(|_| "coordinator already exists")?;
        let copy = c.clone();
        std::thread::spawn(move || unsafe { copy.message_loop(rx, ready_tx) });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "control hook startup timed out")??;
        Ok(c)
    }
    /// W2 captures must use this guard, not call a screen backend directly.
    pub fn capture<T>(&self, capture: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let (tx, rx) = mpsc::channel();
        self.post(Command::CaptureBegin(tx))?;
        let hidden = rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Indicator capture pause timed out".to_string())??;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(capture));
        if hidden {
            let (tx, rx) = mpsc::channel();
            self.post(Command::CaptureEnd(tx))?;
            rx.recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Indicator capture resume timed out".to_string())??;
        }
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
    pub fn set_language(&self, chinese: bool) -> Result<(), String> {
        self.post(Command::Language(chinese))
    }
    pub fn operation_started(&self) {
        self.busy.store(true, Ordering::SeqCst);
        self.started.store(now(), Ordering::SeqCst);
    }
    pub fn operation_finished(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.completed.store(now(), Ordering::SeqCst);
    }
    pub fn admit(&self, request: &Value) -> Result<(), String> {
        let _transition = self.transition.lock().unwrap();
        let mut s = self.state.lock().unwrap();
        if self.takeover_pending.load(Ordering::SeqCst)
            || !s
                .admission
                .admit(request["generation"].as_u64().unwrap_or(0))
        {
            return Err(
                "CONTROL_STOPPED: cancelled or stale admission; submit only a new explicit request"
                    .into(),
            );
        }
        s.app_epoch = request["epoch"].as_u64().unwrap_or(0);
        s.operation_generation = request["generation"].as_u64().unwrap();
        s.operation = request["operation"].as_str().unwrap_or("").into();
        s.owner = request["owner"].as_str().unwrap_or("pipe").into();
        self.cancelled.store(false, Ordering::SeqCst);
        self.stopped.store(false, Ordering::SeqCst);
        Ok(())
    }
    pub fn cancel_generation(&self, generation: u64) {
        let _transition = self.transition.lock().unwrap();
        let current = {
            let mut s = self.state.lock().unwrap();
            s.admission.cancel(generation)
        };
        // App stop already invalidates the appropriate resource/client epoch.
        // Do not turn a client-only stop into a global desktop invalidation.
        if current {
            self.stop_locked(false, false);
        }
    }
    fn event(&self, mut value: Value) {
        let s = self.state.lock().unwrap();
        value["epoch"] = json!(s.app_epoch);
        value["generation"] = json!(s.operation_generation);
        value["operation"] = json!(s.operation);
        value["owner"] = json!(s.owner);
        // Keep the operation identity bound through emission, not a later caller's epoch.
        (self.emit)(value);
    }
    fn healthy(&self) -> bool {
        self.thread.load(Ordering::SeqCst) != 0
            && now().saturating_sub(self.heartbeat.load(Ordering::SeqCst)) <= 1000
    }
    fn fail(&self, reason: String) {
        *self.failure.lock().unwrap() = Some(reason);
        self.cancel(false);
    }
    pub fn checkpoint(&self) -> Result<(), String> {
        if !self.healthy() {
            self.fail("Control coordinator heartbeat lost; input released".into());
        }
        if self.failure.lock().unwrap().is_some() {
            return Err(
                "CONTROL_STOPPED: control coordinator unavailable; no input permitted".into(),
            );
        }
        let session = self.state.lock().unwrap().input_session;
        if let Some(session) = session {
            if let Err(e) = super::desktop::verify(session) {
                self.cancel(false);
                return Err(format!(
                    "CONTROL_STOPPED: {e}; partial effects may have occurred; do not replay"
                ));
            }
        }
        if self.cancelled.load(Ordering::SeqCst) {
            Err("CONTROL_STOPPED: partial effects may already have occurred; do not replay".into())
        } else {
            Ok(())
        }
    }
    pub fn sleep(&self, d: Duration) -> Result<(), String> {
        // Wait does not begin an AI period, but it still belongs to the admitted
        // operation. A later admission must never revive this operation's sleep.
        let generation = self.state.lock().unwrap().operation_generation;
        let start = std::time::Instant::now();
        loop {
            self.checkpoint()?;
            if !self.state.lock().unwrap().admission.active(generation) {
                return Err(
                    "CONTROL_STOPPED: waiting operation was cancelled or superseded; do not replay"
                        .into(),
                );
            }
            let remaining = d.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    fn post(&self, cmd: Command) -> Result<(), String> {
        self.commands
            .send(cmd)
            .map_err(|_| "control thread unavailable")?;
        if unsafe { PostThreadMessageW(self.thread.load(Ordering::SeqCst), COMMAND, 0, 0) } == 0 {
            return Err("control thread post failed".into());
        }
        Ok(())
    }
    pub fn begin(&self, owner: &str) -> Result<(), String> {
        self.checkpoint()?;
        let (tx, rx) = mpsc::channel();
        let session = super::desktop::current()?;
        let generation = {
            let mut s = self.state.lock().unwrap();
            s.input_session = Some(session);
            s.operation_generation
        };
        self.post(Command::Begin(owner.into(), generation, tx))?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| "AI indicator/control thread not responding; no input sent".to_string())?
    }
    pub fn end(&self) {
        let _transition = self.transition.lock().unwrap();
        self.end_locked();
    }
    fn end_locked(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.deadline.store(0, Ordering::SeqCst);
        self.release_held();
        {
            let mut s = self.state.lock().unwrap();
            s.active = false;
            s.input_session = None;
            s.generation += 1;
        }
        self.event(json!({"type":"event","event":"control-state","active":false}));
        let generation = self.state.lock().unwrap().operation_generation;
        let _ = self.post(Command::End(generation));
    }
    pub fn cancel(&self, takeover: bool) {
        self.stop(takeover, true);
    }
    fn stop(&self, takeover: bool, notify: bool) {
        let _transition = self.transition.lock().unwrap();
        self.stop_locked(takeover, notify);
    }
    fn stop_locked(&self, takeover: bool, notify: bool) {
        self.cancelled.store(true, Ordering::SeqCst);
        if takeover {
            let mut s = self.state.lock().unwrap();
            s.taken = true;
            s.last_input = now();
        }
        self.deadline.store(0, Ordering::SeqCst);
        self.release_held();
        if !self.stopped.swap(true, Ordering::SeqCst) {
            if notify {
                self.event(json!({"type":"event","event":if takeover{"takeover"}else{"control-stopped"},"active":false}));
            }
            self.end_locked();
        }
    }
    pub fn status(&self, owner: &str, release: bool) -> String {
        let owns_control = {
            let s = self.state.lock().unwrap();
            s.control_owner == owner && (!self.busy.load(Ordering::SeqCst) || s.owner == owner)
        };
        if release && owns_control {
            if self.busy.load(Ordering::SeqCst) {
                self.stop(false, false);
            } else {
                self.end();
            }
        }
        if !self.healthy() {
            self.fail("Control coordinator heartbeat lost; input released".into());
        }
        let failure = self.failure.lock().unwrap().clone();
        let s = self.state.lock().unwrap();
        let state = control_state(
            true,
            failure.is_some(),
            s.taken,
            self.takeover_pending.load(Ordering::SeqCst),
            s.active,
        );
        let wait = if state == "user" {
            10000u64.saturating_sub(now().saturating_sub(s.last_input)) as f64 / 1000.0
        } else {
            0.0
        };
        let mut status = json!({"state":state,"generation":s.generation,"wait_seconds":wait,"enabled":true,"policy":"relaxed","idle_timeout_seconds":10.0,"ui_busy":self.busy.load(Ordering::SeqCst),"uia_worker":{"busy":self.busy.load(Ordering::SeqCst),"busy_seconds":if self.busy.load(Ordering::SeqCst){now().saturating_sub(self.started.load(Ordering::SeqCst)) as f64/1000.0}else{0.0},"abandoned_calls":0,"queued_calls":0,"last_completed_age_seconds":if self.completed.load(Ordering::SeqCst)==0{Value::Null}else{json!(now().saturating_sub(self.completed.load(Ordering::SeqCst)) as f64/1000.0)}}});
        if let Some(reason) = failure {
            status["reason"] = json!(reason);
        }
        status.to_string()
    }
    pub fn hold(&self, key: u16, mouse: bool, down: bool) -> Result<(), String> {
        if down {
            self.checkpoint()?;
        }
        let mut held = self.held.lock().unwrap();
        if down && self.cancelled.load(Ordering::SeqCst) {
            return Err("CONTROL_STOPPED before input".into());
        }
        super::input::send_button(key, mouse, down)?;
        if down {
            held.push((key, mouse));
        } else if let Some(i) = held.iter().rposition(|x| *x == (key, mouse)) {
            held.remove(i);
        }
        Ok(())
    }
    pub fn release_held(&self) {
        let mut held = self.held.lock().unwrap();
        for (k, m) in held.drain(..).rev() {
            let _ = super::input::send_button(k, m, false);
        }
    }
    fn physical_input(&self) {
        self.physical_sequence.fetch_add(1, Ordering::SeqCst);
        let mut s = self.state.lock().unwrap();
        if s.taken {
            s.last_input = now();
        }
    }
    fn suppress(&self) -> bool {
        self.deadline.load(Ordering::SeqCst) > now()
    }
    unsafe fn message_loop(
        self: Arc<Self>,
        rx: mpsc::Receiver<Command>,
        ready: mpsc::Sender<Result<(), String>>,
    ) {
        self.thread.store(GetCurrentThreadId(), Ordering::SeqCst);
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        let keyboard = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(key_hook),
            GetModuleHandleW(std::ptr::null()),
            0,
        );
        let mouse = SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook),
            GetModuleHandleW(std::ptr::null()),
            0,
        );
        let raw = raw_window();
        if keyboard.is_null() || mouse.is_null() || raw.is_null() {
            if !keyboard.is_null() {
                UnhookWindowsHookEx(keyboard);
            }
            if !mouse.is_null() {
                UnhookWindowsHookEx(mouse);
            }
            let _ = ready.send(Err(
                "Windows input hooks/raw input unavailable; no suppression".into(),
            ));
            return;
        }
        if SetTimer(std::ptr::null_mut(), 1, 100, None) == 0 {
            self.thread.store(0, Ordering::SeqCst);
            self.fail("Control coordinator timer unavailable".into());
            UnhookWindowsHookEx(keyboard);
            UnhookWindowsHookEx(mouse);
            DestroyWindow(raw);
            let _ = ready.send(Err("Control coordinator timer unavailable".into()));
            return;
        }
        self.heartbeat.store(now(), Ordering::SeqCst);
        let _ = ready.send(Ok(()));
        let mut chinese = crate::preferences::Language::System.chinese();
        let mut locale_changed = false;
        let mut indicator: Option<Indicator> = None;
        let mut capture_at: Option<u64> = None;
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == COMMAND {
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Command::Language(value) => {
                            if chinese != value {
                                chinese = value;
                                locale_changed = true;
                                if indicator.is_some() && capture_at.is_none() {
                                    match Indicator::show(chinese) {
                                        Ok(i) => {
                                            indicator = Some(i);
                                            locale_changed = false;
                                        }
                                        Err(e) => {
                                            self.fail(e);
                                            self.cancel(false);
                                            indicator = None;
                                        }
                                    }
                                }
                            }
                        }
                        Command::CaptureBegin(reply) => {
                            if let Some(i) = indicator.as_ref().filter(|i| !i.excluded) {
                                self.deadline.store(0, Ordering::SeqCst);
                                capture_at = Some(self.physical_sequence.load(Ordering::SeqCst));
                                i.hide();
                                if i.visible() {
                                    self.cancel(false);
                                    let _ = reply.send(Err(
                                        "Indicator could not be hidden for capture".into(),
                                    ));
                                } else {
                                    let _ = reply.send(Ok(true));
                                }
                            } else {
                                let _ = reply.send(Ok(false));
                            }
                        }
                        Command::CaptureEnd(reply) => {
                            let capture_sequence = capture_at.take();
                            let unchanged = capture_sequence.is_some_and(|at| {
                                at == self.physical_sequence.load(Ordering::SeqCst)
                            });
                            if unchanged && self.state.lock().unwrap().active {
                                if locale_changed {
                                    match Indicator::show(chinese) {
                                        Ok(i) => {
                                            indicator = Some(i);
                                            locale_changed = false;
                                        }
                                        Err(e) => {
                                            self.fail(e.clone());
                                            self.cancel(false);
                                            indicator = None;
                                            let _ = reply.send(Err(e));
                                            continue;
                                        }
                                    }
                                }
                                if let Some(i) = indicator.as_ref() {
                                    i.reveal();
                                }
                                if indicator.as_ref().is_some_and(|i| i.visible())
                                    && capture_sequence
                                        == Some(self.physical_sequence.load(Ordering::SeqCst))
                                {
                                    self.deadline.store(now() + 1000, Ordering::SeqCst);
                                    if capture_sequence
                                        != Some(self.physical_sequence.load(Ordering::SeqCst))
                                    {
                                        self.cancel(false);
                                        let _ = reply.send(Err("CONTROL_STOPPED: physical input while rearming capture; control was released".into()));
                                    } else {
                                        let _ = reply.send(Ok(()));
                                    }
                                } else {
                                    self.cancel(false);
                                    let _=reply.send(Err("CONTROL_STOPPED: indicator failed to reappear after capture".into()));
                                }
                            } else {
                                self.cancel(false);
                                indicator = None;
                                let _=reply.send(Err("CONTROL_STOPPED: physical input during capture; control was not rearmed".into()));
                            }
                        }
                        Command::Takeover => {
                            self.cancel(true);
                            self.takeover_pending.store(false, Ordering::SeqCst);
                            indicator = None;
                        }
                        Command::End(generation) => {
                            if self.state.lock().unwrap().operation_generation == generation {
                                capture_at = None;
                                indicator = None;
                            }
                        }
                        Command::Begin(owner, generation, answer) => {
                            if self.state.lock().unwrap().operation_generation != generation {
                                let _ = answer.send(Err("CONTROL_STOPPED: stale begin".into()));
                                continue;
                            }
                            if let Err(e) = self
                                .checkpoint()
                                .and_then(|_| super::desktop::current().map(|_| ()))
                            {
                                let _ = answer.send(Err(e));
                                continue;
                            }
                            let wait = {
                                let s = self.state.lock().unwrap();
                                if s.taken {
                                    10000u64.saturating_sub(now().saturating_sub(s.last_input))
                                } else {
                                    0
                                }
                            };
                            if wait > 0 {
                                let _ = answer.send(Err(format!(
                                    "USER_CONTROL: wait_seconds={:.3}",
                                    wait as f64 / 1000.0
                                )));
                                continue;
                            }
                            if self.cancelled.load(Ordering::SeqCst) {
                                let _ = answer.send(Err("CONTROL_STOPPED before indicator".into()));
                                continue;
                            }
                            if indicator.is_none() {
                                match Indicator::show(chinese) {
                                    Ok(i) => indicator = Some(i),
                                    Err(e) => {
                                        self.end();
                                        let _ = answer.send(Err(e));
                                        continue;
                                    }
                                }
                            }
                            if !indicator.as_ref().is_some_and(|i| i.visible()) {
                                self.end();
                                let _ = answer.send(Err(
                                    "AI control indicator is not visible; no input sent".into(),
                                ));
                                continue;
                            }
                            if let Err(e) = self.checkpoint() {
                                let _ = answer.send(Err(e));
                                continue;
                            }
                            // Only the short state commit shares admission/stop ordering.
                            // Never hold this lock while showing/hiding UI or waiting on COM.
                            let _transition = self.transition.lock().unwrap();
                            if self.state.lock().unwrap().operation_generation != generation
                                || self.cancelled.load(Ordering::SeqCst)
                            {
                                let _ =
                                    answer.send(Err("CONTROL_STOPPED before arming input".into()));
                                continue;
                            }
                            if !self.state.lock().unwrap().active {
                                self.physical.lock().unwrap().path.reset();
                            }
                            {
                                let mut s = self.state.lock().unwrap();
                                s.control_owner = owner;
                                s.active = true;
                                s.taken = false;
                                s.generation += 1;
                            }
                            self.deadline.store(now() + 1000, Ordering::SeqCst);
                            self.event(
                                json!({"type":"event","event":"control-state","active":true}),
                            );
                            let _ = answer.send(Ok(()));
                        }
                    }
                }
            } else if msg.message == WM_TIMER {
                // Only the timer proves the message pump is servicing deadlines.
                if !self.healthy() {
                    self.fail("Control coordinator missed its heartbeat deadline".into());
                    indicator = None;
                }
                self.heartbeat.store(now(), Ordering::SeqCst);
                let active = self.state.lock().unwrap().active;
                if active && self.checkpoint().is_err() {
                    indicator = None;
                }
                if capture_at.is_none() && self.state.lock().unwrap().active {
                    if self.deadline.load(Ordering::SeqCst) > now()
                        && indicator.as_ref().is_some_and(|i| i.visible())
                    {
                        self.deadline.store(now() + 1000, Ordering::SeqCst);
                    } else {
                        self.cancel(false);
                        indicator = None;
                    }
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        self.thread.store(0, Ordering::SeqCst);
        self.fail("Control coordinator exited; input released".into());
        drop(indicator);
        UnhookWindowsHookEx(keyboard);
        UnhookWindowsHookEx(mouse);
        DestroyWindow(raw);
    }
}
fn now() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}
unsafe extern "system" fn key_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(c) = CURRENT.get() {
            let k = &*(l as *const KBDLLHOOKSTRUCT);
            if k.flags & LLKHF_INJECTED == 0 {
                c.physical_input();
                let down = w as u32 == WM_KEYDOWN || w as u32 == WM_SYSKEYDOWN;
                let takeover = {
                    let mut p = c.physical.lock().unwrap();
                    if down {
                        p.keys.insert(k.vkCode);
                    } else {
                        p.keys.remove(&k.vkCode);
                    }
                    takeover_chord(
                        k.vkCode,
                        down,
                        p.keys.contains(&0x11) || p.keys.contains(&0xa2) || p.keys.contains(&0xa3),
                        p.keys.contains(&0x12) || p.keys.contains(&0xa4) || p.keys.contains(&0xa5),
                        p.keys.contains(&0x10) || p.keys.contains(&0xa0) || p.keys.contains(&0xa1),
                    )
                };
                if c.suppress() {
                    if takeover {
                        c.takeover_pending.store(true, Ordering::SeqCst);
                        c.cancelled.store(true, Ordering::SeqCst);
                        c.deadline.store(0, Ordering::SeqCst);
                        let _ = c.post(Command::Takeover);
                    }
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(c) = CURRENT.get() {
            let m = &*(l as *const MSLLHOOKSTRUCT);
            if m.flags & LLMHF_INJECTED != 0 {
                c.physical.lock().unwrap().position = Some((m.pt.x, m.pt.y));
            } else {
                if w as u32 != WM_MOUSEMOVE {
                    c.physical_input();
                }
                if w as u32 == WM_MOUSEMOVE {
                    let mut p = c.physical.lock().unwrap();
                    let pos = (m.pt.x, m.pt.y);
                    p.raw_moved = p.position.is_none_or(|previous| previous != pos);
                    p.position = Some(pos);
                    let moved = p.raw_moved;
                    drop(p);
                    if moved {
                        c.physical_input();
                    }
                }
                if c.suppress() {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
unsafe fn raw_window() -> HWND {
    let name: Vec<u16> = "MaxaNativeRawInput\0".encode_utf16().collect();
    let instance = GetModuleHandleW(std::ptr::null());
    let wc = WNDCLASSW {
        lpfnWndProc: Some(raw_proc),
        hInstance: instance,
        lpszClassName: name.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        0,
        name.as_ptr(),
        name.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return hwnd;
    }
    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    if RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
        DestroyWindow(hwnd);
        return std::ptr::null_mut();
    }
    hwnd
}
unsafe extern "system" fn raw_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        if let Some(c) = CURRENT.get() {
            let mut input: RAWINPUT = std::mem::zeroed();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            if GetRawInputData(
                l as HRAWINPUT,
                RID_INPUT,
                &mut input as *mut _ as *mut _,
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            ) != u32::MAX
                && input.header.dwType == RIM_TYPEMOUSE
            {
                let m = input.data.mouse;
                let take = {
                    let mut p = c.physical.lock().unwrap();
                    let moved = p.raw_moved;
                    p.raw_moved = false;
                    p.path.update(
                        input.header.hDevice as usize,
                        now(),
                        m.lLastX,
                        m.lLastY,
                        moved,
                    )
                };
                if take && c.suppress() {
                    c.takeover_pending.store(true, Ordering::SeqCst);
                    c.cancelled.store(true, Ordering::SeqCst);
                    c.deadline.store(0, Ordering::SeqCst);
                    let _ = c.post(Command::Takeover);
                }
            }
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
