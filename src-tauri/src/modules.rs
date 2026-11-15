use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Pack {
    pub module: String,
    pub version: String,
    pub archive: String,
    pub sha256: String,
    pub files: HashMap<String, String>,
}
#[derive(Serialize, Deserialize)]
pub struct Packs {
    pub format: u32,
    pub packs: Vec<Pack>,
}
#[derive(Clone, Serialize, Deserialize)]
struct PhoneRoute {
    physical: String,
    name: String,
    address: String,
}
fn mdns_services(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() >= 3
                && fields[1].trim_end_matches('.') == "_adb-tls-connect._tcp"
                && validate_address(fields[2]).is_ok()
            {
                Some((fields[0].into(), fields[2].into()))
            } else {
                None
            }
        })
        .collect()
}
struct Running {
    children: Vec<Child>,
    windows: Option<Arc<crate::windows::Client>>,
    #[cfg(windows)]
    trees: Vec<crate::jobs::ProcessTree>,
}
fn spawn_owned(c: &mut Command, r: &mut Running) -> Result<()> {
    #[cfg(windows)]
    c.creation_flags(0x08000000 | 0x00000004);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let owned = (|| {
            let tree = crate::jobs::own_tree(&child)?;
            if c.as_std().get_args().any(|a| a == "--windows-worker") {
                tree.allow_explicit_child_breakaway()?;
            }
            crate::jobs::resume_tree(&child)?;
            Ok::<_, String>(tree)
        })();
        match owned {
            Ok(tree) => r.trees.push(tree),
            Err(e) => {
                let _ = child.start_kill();
                return Err(e);
            }
        }
    }
    r.children.push(child);
    Ok(())
}
impl Running {
    fn empty() -> Self {
        Self {
            children: Vec::new(),
            windows: None,
            #[cfg(windows)]
            trees: Vec::new(),
        }
    }
}
pub struct Modules {
    pub root: PathBuf,
    pub resources: PathBuf,
    states: Mutex<HashMap<String, ModuleStatus>>,
    running: Mutex<HashMap<String, Running>>,
    transition: Mutex<()>,
    mobile: crate::mobile::Mobile,
    windows_events: tokio::sync::broadcast::Sender<Value>,
    pub adb_port: u16,
}
fn valid_module(id: &str) -> bool {
    matches!(id, "system" | "windows" | "mobile")
}
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    c.kill_on_drop(true);
    #[cfg(windows)]
    {
        c.creation_flags(0x08000000);
    }
    c
}
fn free_port() -> Result<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}
impl Modules {
    pub fn new(root: PathBuf, resources: PathBuf) -> Result<Arc<Self>> {
        let states = ["system", "windows", "mobile"]
            .into_iter()
            .map(|id| {
                (
                    id.into(),
                    ModuleStatus {
                        id: id.into(),
                        phase: "disabled".into(),
                        version: String::new(),
                        error: None,
                    },
                )
            })
            .collect();
        Ok(Arc::new(Self {
            root,
            resources,
            states: Mutex::new(states),
            running: Mutex::new(HashMap::new()),
            transition: Mutex::new(()),
            mobile: crate::mobile::Mobile::default(),
            windows_events: tokio::sync::broadcast::channel(64).0,
            adb_port: free_port()?,
        }))
    }
    #[cfg(test)]
    pub(crate) async fn ready_for_control_test(&self, module: &str) {
        self.states.lock().await.get_mut(module).unwrap().phase = "ready".into();
    }
    pub async fn statuses(&self) -> Vec<ModuleStatus> {
        let mut x: Vec<_> = self.states.lock().await.values().cloned().collect();
        x.sort_by(|a, b| a.id.cmp(&b.id));
        x
    }
    pub async fn ready(&self, id: &str) -> Result<()> {
        match self.states.lock().await.get(id) {
            Some(s) if s.phase == "ready" => Ok(()),
            _ => Err(format!("MODULE_UNAVAILABLE: {id}")),
        }
    }
    pub fn module_root(&self, id: &str) -> PathBuf {
        self.root.join("modules").join(id)
    }
    pub fn adb(&self) -> Command {
        let root = self.module_root("mobile");
        let mut c = command(root.join("sdk/platform-tools/adb.exe"));
        c.env(
            "ADB_SERVER_SOCKET",
            format!("tcp:127.0.0.1:{}", self.adb_port),
        )
        .env("ANDROID_ADB_SERVER_PORT", self.adb_port.to_string())
        .env("ANDROID_USER_HOME", self.root.join("android-user"))
        .env_remove("ADB_VENDOR_KEYS")
        .env_remove("ANDROID_SERIAL");
        c
    }
    fn adb_server(&self) -> Command {
        let mut server = self.adb();
        // Server -L accepts tcp:<port>, not tcp:host:<port>; without -a it binds loopback.
        server.args([
            "-L",
            &format!("tcp:{}", self.adb_port),
            "nodaemon",
            "server",
        ]);
        server
    }
    fn install(&self, id: &str) -> Result<()> {
        if !matches!(id, "system" | "mobile") {
            return Err("module has no component pack".into());
        }
        let catalog: Packs = serde_json::from_slice(
            &fs::read(self.resources.join("packs/manifest.json"))
                .map_err(|e| format!("component packs unavailable: {e}"))?,
        )
        .map_err(|e| e.to_string())?;
        if catalog.format != 1 {
            return Err("unsupported component manifest".into());
        }
        let p = catalog
            .packs
            .iter()
            .find(|p| p.module == id)
            .ok_or("component is not bundled")?;
        if p.archive.contains('/') || p.archive.contains('\\') {
            return Err("invalid pack archive".into());
        }
        let bytes =
            fs::read(self.resources.join("packs").join(&p.archive)).map_err(|e| e.to_string())?;
        if digest(&bytes) != p.sha256 {
            return Err("component digest mismatch".into());
        }
        let dest = self.module_root(id);
        if dest.exists() && verify(&dest, p).is_ok() {
            return Ok(());
        }
        let stage = self.root.join(format!("install-{}", crate::model::id()));
        fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let result = (|| {
            let mut zip =
                zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
            let mut total = 0u64;
            for n in 0..zip.len() {
                let mut f = zip.by_index(n).map_err(|e| e.to_string())?;
                let name = f.enclosed_name().ok_or("pack path escape")?;
                if f.unix_mode()
                    .map(|m| m & 0o170000 == 0o120000)
                    .unwrap_or(false)
                {
                    return Err("pack symlink rejected".into());
                }
                total = total.checked_add(f.size()).ok_or("pack size overflow")?;
                if total > 8 * 1024 * 1024 * 1024 {
                    return Err("pack exceeds 8 GiB".into());
                }
                let path = stage.join(name);
                if f.is_dir() {
                    fs::create_dir_all(path).map_err(|e| e.to_string())?
                } else {
                    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
                    let mut out = fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(path)
                        .map_err(|e| e.to_string())?;
                    std::io::copy(&mut f, &mut out).map_err(|e| e.to_string())?;
                    out.sync_all().map_err(|e| e.to_string())?;
                }
            }
            verify(&stage, p)?;
            fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
            let backup = self.root.join(format!("previous-{}", crate::model::id()));
            if dest.exists() {
                fs::rename(&dest, &backup).map_err(|e| {
                    format!(
                        "component backup {} -> {}: {e}",
                        dest.display(),
                        backup.display()
                    )
                })?;
            }
            if let Err(error) = fs::rename(&stage, &dest) {
                if backup.exists() {
                    let _ = fs::rename(&backup, &dest);
                }
                return Err(format!(
                    "component publish {} -> {}: {error}",
                    stage.display(),
                    dest.display()
                ));
            }
            if backup.exists() {
                let _ = fs::remove_dir_all(backup);
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(stage);
        result
    }
    async fn stop_backend(&self, id: &str) {
        if let Some(mut running) = self.running.lock().await.remove(id) {
            for child in &mut running.children {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }
    // Operation interruption is not a human module toggle: drop automation, retain enablement/trust.
    pub async fn release_automation(&self, module: &str, physical: &str) {
        let _transition = self.transition.lock().await;
        if module == "windows" {
            let worker = self
                .running
                .lock()
                .await
                .get("windows")
                .and_then(|r| r.windows.clone());
            if let Some(worker) = worker {
                if !worker.cancel().await {
                    self.stop_backend("windows").await;
                }
            }
        } else if module == "mobile" {
            // stop_phone publishes termination uncertainty even for best-effort release paths.
            let _ = self.stop_phone(Some(physical)).await;
        }
    }
    async fn ensure_windows_worker(
        &self,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let _transition = self.transition.lock().await;
        self.ready("windows").await?;
        if cancelled
            .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false)
        {
            return Err("CONTROL_STOPPED before worker startup".into());
        }
        let existing = self
            .running
            .lock()
            .await
            .get("windows")
            .and_then(|r| r.windows.clone());
        if existing.as_ref().is_some_and(|c| c.alive()) {
            return Ok(());
        }
        self.stop_backend("windows").await;
        self.spawn_windows().await?;
        Ok(())
    }
    pub async fn set(&self, id: &str, enabled: bool) -> Result<()> {
        if !valid_module(id) {
            return Err("unknown module".into());
        }
        let _transition = self.transition.lock().await;
        let mut states = self.states.lock().await;
        if !enabled {
            states.get_mut(id).unwrap().phase = "disabled".into();
            drop(states);
            let stopped = if id == "mobile" {
                self.stop_phone(None).await
            } else {
                Ok(())
            };
            if id == "windows" {
                let worker = self
                    .running
                    .lock()
                    .await
                    .get(id)
                    .and_then(|r| r.windows.clone());
                if let Some(worker) = worker {
                    let _ = worker.cancel().await;
                }
            }
            self.stop_backend(id).await;
            let mut states = self.states.lock().await;
            let s = states.get_mut(id).unwrap();
            s.error = stopped.as_ref().err().cloned();
            return stopped;
        }
        if states[id].phase == "ready" {
            return Ok(());
        }
        states.get_mut(id).unwrap().phase = "starting".into();
        drop(states);
        let result = self.start(id).await;
        let termination_error = if id == "mobile" {
            self.mobile.termination_error().await
        } else {
            None
        };
        let mut states = self.states.lock().await;
        let s = states.get_mut(id).unwrap();
        match &result {
            Ok(_) => {
                s.phase = "ready".into();
                s.version = match id {
                    "windows" => concat!(
                        env!("CARGO_PKG_NAME"),
                        "-",
                        env!("CARGO_PKG_VERSION"),
                        "-native"
                    ),
                    "mobile" => "1.0.17",
                    _ => "1",
                }
                .into();
                s.error = termination_error
            }
            Err(e) => {
                s.phase = "failed".into();
                s.error = Some(match termination_error {
                    Some(pending) => format!("{pending}; module startup failed: {e}"),
                    None => e.clone(),
                })
            }
        }
        result
    }
    fn remove_retired_windows(&self) -> Result<()> {
        // Fixed child only; pin every existing ancestor and reject reparse points.
        let parent = self.root.join("modules");
        let _parent = crate::files::Files::pin(&parent, true)?;
        let retired = parent.join("windows");
        match fs::symlink_metadata(&retired) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("retired Windows component metadata: {e}")),
            Ok(meta) if !meta.is_dir() && !meta.file_type().is_symlink() => {
                Err("retired Windows component path is not a directory".into())
            }
            Ok(_) => crate::files::remove_tree(&retired)
                .map_err(|e| format!("retired Windows component cleanup: {e}")),
        }
    }
    async fn start(&self, id: &str) -> Result<()> {
        self.remove_retired_windows()?;
        if id != "windows" {
            self.install(id)?;
        }
        if id == "system" {
            return Ok(());
        }
        #[cfg(windows)]
        if id == "windows" {
            use windows_sys::Win32::System::RemoteDesktop::{
                ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
            };
            let mut session = 0;
            let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
            if ok == 0 || session != unsafe { WTSGetActiveConsoleSessionId() } {
                return Err("active interactive desktop required".into());
            }
        }
        let mut running = Running::empty();
        if id == "mobile" {
            fs::create_dir_all(self.root.join("android-user")).map_err(|e| e.to_string())?;
            spawn_owned(
                self.adb_server()
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
                &mut running,
            )?;
            let mut ready = false;
            for _ in 0..100 {
                if running
                    .children
                    .iter_mut()
                    .any(|c| !matches!(c.try_wait(), Ok(None)))
                {
                    break;
                }
                if self.adb_client().server_version().await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if !ready {
                return Err("owned private ADB failed startup; no global fallback".into());
            }
            self.running.lock().await.insert(id.into(), running);
            return Ok(());
        }
        // Windows has no component pack or listener. Worker starts at first tools/call.
        #[cfg(not(windows))]
        return Err("UNSUPPORTED: active Windows interactive desktop required".into());
        #[cfg(windows)]
        Ok(())
    }
    async fn spawn_windows(&self) -> Result<()> {
        let mut running = Running::empty();
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut c = command(exe);
        c.arg("--windows-worker")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        spawn_owned(&mut c, &mut running)?;
        let child = running.children.last_mut().unwrap();
        let client = crate::windows::Client::attach(
            child.stdin.take().ok_or("worker stdin missing")?,
            child.stdout.take().ok_or("worker stdout missing")?,
            self.windows_events.clone(),
        );
        if let Err(e) = client.wait_ready().await {
            for child in &mut running.children {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            return Err(e);
        }
        running.windows = Some(client);
        self.running.lock().await.insert("windows".into(), running);
        Ok(())
    }
    pub fn windows_events(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.windows_events.subscribe()
    }
    pub async fn windows_event_current(&self, event: &Value) -> bool {
        self.running
            .lock()
            .await
            .get("windows")
            .and_then(|r| r.windows.as_ref())
            .is_some_and(|c| c.event_current(event))
    }
    fn adb_client(&self) -> adbutils::AdbClient {
        crate::mobile::transport::client(self.adb_port)
    }
    pub async fn stop_phone(&self, physical: Option<&str>) -> Result<()> {
        let result = self.mobile.stop(physical).await;
        let pending = self.mobile.termination_error().await;
        if let Some(s) = self.states.lock().await.get_mut("mobile") {
            if pending.is_some()
                || s.error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("TERMINATION_UNCONFIRMED:"))
            {
                s.error = pending;
            }
        }
        result
    }
    pub async fn dispatch_phone(
        &self,
        physical: &str,
        message: &Value,
        timeout: f64,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        self.ready("mobile").await?;
        self.mobile
            .call(
                self.adb_port,
                self.module_root("mobile")
                    .join("helper/u2-0.4.0-maxa.3.jar"),
                physical,
                message,
                timeout,
                cancelled,
            )
            .await
    }
    pub async fn dispatch(&self, module: &str, message: &Value) -> Result<Value> {
        self.dispatch_controlled(module, message, 300.0, None).await
    }
    pub async fn dispatch_controlled(
        &self,
        module: &str,
        message: &Value,
        timeout: f64,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Value> {
        self.ready(module).await?;
        if let Some(response) = crate::native_catalog::respond(module, message)? {
            return Ok(response);
        }
        if module != "windows" {
            return Err("native Mobile tools/call requires physical-phone dispatch".into());
        }
        self.ensure_windows_worker(cancelled).await?;
        let client = self
            .running
            .lock()
            .await
            .get("windows")
            .and_then(|r| r.windows.clone())
            .ok_or("Windows worker stopped")?;
        let result = client.call(message, timeout, cancelled).await;
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.contains("WINDOWS_WORKER_STUCK"))
        {
            self.stop_backend("windows").await;
        }
        result
    }
    pub async fn pair_phone(&self, address: &str, code: &str) -> Result<()> {
        self.ready("mobile").await?;
        validate_address(address)?;
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err("pair code must be six digits".into());
        }
        let out = self
            .adb()
            .args(["pair", address, code])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success()
            || !String::from_utf8_lossy(&out.stdout).contains("Successfully paired")
        {
            return Err(
                "Android pairing rejected; verify the on-device address, code and expiry".into(),
            );
        }
        Ok(())
    }
    pub async fn connect_phone(&self, address: &str) -> Result<()> {
        self.ready("mobile").await?;
        validate_address(address)?;
        let response = self
            .adb_client()
            .connect(address, Some(std::time::Duration::from_secs(8)))
            .await
            .map_err(|e| e.to_string())?;
        if !response.starts_with("connected to ") && !response.starts_with("already connected to ")
        {
            return Err(format!("Android connection failed: {response}"));
        }
        Ok(())
    }
    pub async fn phone_preflight(&self, physical: &str, transport: &str) -> Result<()> {
        self.ready("mobile").await?;
        let dev = self.adb_client().device(transport);
        let state = tokio::time::timeout(std::time::Duration::from_secs(8), dev.get_state())
            .await
            .map_err(|_| "Android readiness timed out")?
            .map_err(|e| e.to_string())?;
        if state != "device" {
            return Err("Android offline/unauthorized; confirm debugging authorization".into());
        }
        if crate::mobile::transport::shell(&dev, "id -u".into())
            .await?
            .trim()
            != "2000"
        {
            return Err("Authorized shell UID 2000 required; no permission fallback".into());
        }
        let identity = crate::mobile::transport::shell(&dev, "getprop ro.serialno".into()).await?;
        if identity.is_empty()
            || identity == "unknown"
            || format!("android:{}", digest(identity.as_bytes())) != physical
        {
            return Err(
                "PHONE_IDENTITY_CHANGED: refresh physical-phone mapping; no input sent".into(),
            );
        }
        Ok(())
    }
    pub async fn mobile_handover_check(&self, physical: &str, transport: &str) -> Result<()> {
        self.mobile
            .handover(self.adb_port, physical, transport)
            .await
    }
    async fn mdns(&self) -> Result<HashMap<String, String>> {
        let mut conn = self
            .adb_client()
            .make_connection(Some(std::time::Duration::from_secs(5)))
            .await
            .map_err(|e| e.to_string())?;
        conn.send_command("host:mdns:services")
            .await
            .map_err(|e| e.to_string())?;
        conn.check_okay().await.map_err(|e| e.to_string())?;
        Ok(mdns_services(
            &conn.read_string_block().await.map_err(|e| e.to_string())?,
        ))
    }
    pub async fn phones(
        &self,
        removed: &std::collections::HashSet<String>,
    ) -> Result<Vec<PhoneInfo>> {
        self.ready("mobile").await?;
        let routes_path = self.root.join("phone-routes.json");
        let mut routes: HashMap<String, PhoneRoute> = match fs::read(&routes_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "invalid private phone route store")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e.to_string()),
        };
        let services = self.mdns().await.unwrap_or_default();
        let adb = self.adb_client();
        // Reconnect only a previously verified service/physical identity, never a new pairing.
        for (service, route) in &mut routes {
            if removed.contains(&route.physical) {
                continue;
            }
            let Some(address) = services.get(service) else {
                continue;
            };
            if self.connect_phone(address).await.is_ok() {
                let dev = adb.device(address);
                if let Ok(serial) =
                    crate::mobile::transport::shell(&dev, "getprop ro.serialno".into()).await
                {
                    if !serial.is_empty()
                        && serial != "unknown"
                        && format!("android:{}", digest(serial.as_bytes())) == route.physical
                    {
                        route.address = address.clone();
                        continue;
                    }
                }
                let _ = adb.disconnect(address, false).await;
            }
        }
        let devices = adb.list(false).await.map_err(|e| e.to_string())?;
        let mut by_id: HashMap<String, PhoneInfo> = HashMap::new();
        for device in devices {
            let transport = device.serial;
            let mut info = PhoneInfo {
                removed: false,
                id: format!("unverified:{}", digest(transport.as_bytes())),
                name: "Android".into(),
                transports: vec![transport.clone()],
                active_transport: None,
                state: device.state.clone(),
                reason: Some("physical identity has not been verified".into()),
            };
            if device.state == "device" {
                let dev = adb.device(&transport);
                if let Ok(serial) =
                    crate::mobile::transport::shell(&dev, "getprop ro.serialno".into()).await
                {
                    if !serial.is_empty() && serial != "unknown" {
                        info.id = format!("android:{}", digest(serial.as_bytes()));
                        info.active_transport = Some(transport.clone());
                        info.reason = None;
                        info.name = crate::mobile::transport::shell(
                            &dev,
                            "getprop ro.product.model".into(),
                        )
                        .await
                        .unwrap_or_else(|_| "Android".into());
                    }
                }
            }
            if let Some(previous) = by_id.get_mut(&info.id) {
                previous.transports.extend(info.transports);
            } else {
                by_id.insert(info.id.clone(), info);
            }
        }
        for phone in by_id.values() {
            if phone.active_transport.is_none() {
                continue;
            }
            for (service, address) in &services {
                if phone.transports.contains(address) {
                    routes.insert(
                        service.clone(),
                        PhoneRoute {
                            physical: phone.id.clone(),
                            name: phone.name.clone(),
                            address: address.clone(),
                        },
                    );
                }
            }
        }
        for route in routes.values() {
            by_id.entry(route.physical.clone()).or_insert_with(||PhoneInfo{removed:false,id:route.physical.clone(),name:route.name.clone(),transports:vec![route.address.clone()],active_transport:None,state:"offline".into(),reason:Some("Previously verified phone is offline; only its remembered mDNS identity can auto-reconnect. Use human Connect if the service identity changed.".into())});
        }
        crate::identity::private_write(
            &routes_path,
            &serde_json::to_vec(&routes).map_err(|e| e.to_string())?,
        )?;
        Ok(by_id.into_values().collect())
    }
    pub async fn health(&self) {
        let broken = {
            let mut running = self.running.lock().await;
            running
                .iter_mut()
                .filter_map(|(id, r)| {
                    if r.children
                        .iter_mut()
                        .any(|p| !matches!(p.try_wait(), Ok(None)))
                    {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for id in broken {
            if id == "windows" {
                self.stop_backend("windows").await;
                continue;
            }
            let _ = self.set(&id, false).await;
            if let Some(s) = self.states.lock().await.get_mut(&id) {
                s.phase = "failed".into();
                let exit = "Owned module process exited; no global fallback was started";
                s.error = Some(match s.error.take() {
                    Some(e) => format!("{e}; {exit}"),
                    None => exit.into(),
                });
            }
        }
    }
    pub async fn shutdown(&self) {
        for id in ["windows", "mobile", "system"] {
            let _ = self.set(id, false).await;
        }
    }
}
fn verify(root: &Path, p: &Pack) -> Result<()> {
    for (name, expected) in &p.files {
        let relative = Path::new(name);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("invalid manifest path".into());
        }
        let bytes =
            fs::read(root.join(relative)).map_err(|e| format!("component missing: {name}: {e}"))?;
        if &digest(&bytes) != expected {
            return Err(format!("component file digest mismatch: {name}"));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn module_start_removes_only_retired_private_windows_tree() {
        let root = std::env::temp_dir().join(id());
        let retired = root.join("modules").join("windows");
        fs::create_dir_all(retired.join("server")).unwrap();
        fs::write(retired.join("server/retired-runtime.bin"), b"retired").unwrap();
        fs::create_dir_all(root.join("modules/mobile")).unwrap();
        fs::write(root.join("modules/mobile/keep.bin"), b"preserve").unwrap();
        let modules = Modules::new(root.clone(), root.join("resources")).unwrap();
        // Missing pack fails installation, but start must first remove the retired tree.
        assert!(modules.start("system").await.is_err());
        assert!(!retired.exists());
        assert_eq!(
            fs::read(root.join("modules/mobile/keep.bin")).unwrap(),
            b"preserve"
        );
        assert!(modules
            .install("windows")
            .unwrap_err()
            .contains("no component pack"));
        drop(modules);
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn retired_windows_cleanup_refuses_reparse_and_keeps_external_data() {
        let root = std::env::temp_dir().join(id());
        let external = std::env::temp_dir().join(id());
        fs::create_dir_all(root.join("modules/windows")).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("keep.bin"), b"preserve").unwrap();
        let link = root.join("modules/windows/escape");
        std::os::unix::fs::symlink(&external, &link).unwrap();
        let modules = Modules::new(root.clone(), root.join("resources")).unwrap();
        assert!(modules
            .remove_retired_windows()
            .unwrap_err()
            .contains("REPARSE_POINT_DENIED"));
        assert_eq!(fs::read(external.join("keep.bin")).unwrap(), b"preserve");
        fs::remove_file(&link).unwrap();
        fs::remove_dir(root.join("modules/windows")).unwrap();
        fs::remove_dir(root.join("modules")).unwrap();
        std::os::unix::fs::symlink(&external, root.join("modules")).unwrap();
        assert!(modules
            .remove_retired_windows()
            .unwrap_err()
            .contains("REPARSE_POINT_DENIED"));
        assert_eq!(fs::read(external.join("keep.bin")).unwrap(), b"preserve");
        fs::remove_file(root.join("modules")).unwrap();
        drop(modules);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(external).unwrap();
    }
    #[test]
    fn adb_server_listener_uses_port_only_syntax() {
        let root = std::env::temp_dir().join(id());
        let modules = Modules::new(root.clone(), root.join("resources")).unwrap();
        let server = modules.adb_server();
        let args: Vec<_> = server
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-L".to_string(),
                format!("tcp:{}", modules.adb_port),
                "nodaemon".into(),
                "server".into()
            ]
        );
        let env: HashMap<_, _> = server.as_std().get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("ADB_SERVER_SOCKET")].unwrap(),
            std::ffi::OsStr::new(&format!("tcp:127.0.0.1:{}", modules.adb_port))
        );
        assert!(!args.iter().any(|arg| arg == "-a"));
    }
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "Requires MAXA_DEVICE_MOBILE_PACK_ROOT pointing to the verified installed private Mobile pack; starts only an isolated owned ADB server, no device commands"]
    async fn owned_adb_foreground_server_remains_alive() {
        use tokio::io::AsyncReadExt;
        let source = PathBuf::from(
            std::env::var_os("MAXA_DEVICE_MOBILE_PACK_ROOT")
                .expect("verified private Mobile pack required"),
        );
        let root = std::env::temp_dir().join(format!("maxa-adb-owned-{}", id()));
        let target = root.join("modules/mobile/sdk/platform-tools");
        fs::create_dir_all(&target).unwrap();
        for file in ["adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll"] {
            fs::copy(
                source.join("sdk/platform-tools").join(file),
                target.join(file),
            )
            .unwrap();
        }
        fs::create_dir(root.join("android-user")).unwrap();
        let modules = Modules::new(root.clone(), root.join("resources")).unwrap();
        let mut server = modules.adb_server();
        let args: Vec<_> = server
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-L".to_string(),
                format!("tcp:{}", modules.adb_port),
                "nodaemon".into(),
                "server".into()
            ]
        );
        server
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut owned = Running::empty();
        spawn_owned(&mut server, &mut owned).unwrap();
        let mut child = owned.children.pop().unwrap();
        let pid = child.id().unwrap();
        let port = modules.adb_port;
        let stderr = child.stderr.take().unwrap();
        let captured = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.take(8192).read_to_end(&mut bytes).await;
            bytes
        });
        let mut exited = None;
        let mut listening = false;
        for _ in 0..20 {
            if let Some(status) = child.try_wait().unwrap() {
                exited = Some(status);
                break;
            }
            listening |=
                std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_ok();
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let sockets = command("netstat.exe")
            .args(["-ano", "-p", "tcp"])
            .output()
            .await
            .unwrap();
        let socket_text = String::from_utf8_lossy(&sockets.stdout);
        let endpoints: Vec<_> = socket_text
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() >= 5
                    && fields[0] == "TCP"
                    && fields[4] == pid.to_string()
                    && fields[1].ends_with(&format!(":{port}"))
                {
                    Some(fields[1].to_string())
                } else {
                    None
                }
            })
            .collect();
        let private_listener = !endpoints.is_empty()
            && endpoints
                .iter()
                .all(|address| address.starts_with("127.0.0.1:") || address.starts_with("[::1]:"));
        // Only this test-owned suspended/Job-Object process is stopped; no App/global ADB is touched.
        let _ = child.kill().await;
        let _ = child.wait().await;
        drop(owned);
        let bytes = captured.await.unwrap();
        let diagnostic = String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| {
                line.contains("F adb.exe")
                    || line.contains("could not")
                    || line.contains("failed")
                    || line.contains("error")
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("OWNED_ADB pid={pid} port={port} alive={} loopback_listener={listening} endpoints={endpoints:?} stderr_errors={diagnostic}",exited.is_none());
        drop(modules);
        fs::remove_dir_all(root).unwrap();
        assert!(
            exited.is_none(),
            "owned ADB exited: {exited:?}; {diagnostic}"
        );
        assert!(
            listening,
            "owned ADB never opened its loopback listener: {diagnostic}"
        );
        assert!(
            private_listener,
            "ADB must listen only on loopback: {endpoints:?}"
        );
        assert!(
            !diagnostic.contains("specified hostname currently unsupported"),
            "{diagnostic}"
        );
    }
    #[test]
    fn discovery_only_admits_explicit_android_tls_connect_services() {
        let services=mdns_services("List of discovered mdns services\nphone _adb-tls-connect._tcp. 192.0.2.1:43210\npair _adb-tls-pairing._tcp. 192.0.2.1:43211\nbad _adb-tls-connect._tcp. example.com:1234");
        assert_eq!(services.len(), 1);
        assert_eq!(services["phone"], "192.0.2.1:43210");
    }
}
fn validate_address(address: &str) -> Result<()> {
    let parsed: SocketAddr = address
        .parse()
        .map_err(|_| "Android address must be an explicit IP:port")?;
    if parsed.port() == 0 {
        return Err("invalid port".into());
    }
    Ok(())
}
