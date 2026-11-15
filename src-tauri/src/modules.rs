use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    endpoint: Option<String>,
    token: String,
    #[cfg(windows)]
    trees: Vec<crate::jobs::ProcessTree>,
    _output_root: Option<crate::files::Guarded>,
}
fn spawn_owned(c: &mut Command, r: &mut Running) -> Result<()> {
    #[cfg(windows)]
    c.creation_flags(0x08000000 | 0x00000004);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let owned = (|| {
            let tree = crate::jobs::own_tree(&child)?;
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
            endpoint: None,
            token: String::new(),
            _output_root: None,
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
    owned_phones: Mutex<HashMap<String, String>>,
    phone_running: Mutex<HashMap<String, Running>>,
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
            owned_phones: Mutex::new(HashMap::new()),
            phone_running: Mutex::new(HashMap::new()),
            adb_port: free_port()?,
        }))
    }
    #[cfg(test)]
    pub(crate) async fn ensure_windows_worker_for_control_test(&self) -> Result<()> {
        self.ensure_windows_worker(None).await
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
    pub fn mobilecli(&self) -> Command {
        let root = self.module_root("mobile");
        let mut c = command(root.join(
            "server/node_modules/@mobilenext/mobilecli-windows-amd64/mobilecli-windows-amd64.exe",
        ));
        self.mobile_env(&mut c);
        c
    }
    fn mobile_env(&self, c: &mut Command) {
        let root = self.module_root("mobile");
        c.env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
        c.env("ANDROID_HOME",root.join("sdk")).env("ADB_SERVER_SOCKET",format!("tcp:127.0.0.1:{}",self.adb_port)).env("ANDROID_ADB_SERVER_PORT",self.adb_port.to_string()).env("ANDROID_USER_HOME",self.root.join("android-user")).env("MOBILECLI_HOME",self.root.join("mcli")).env_remove("ADB_VENDOR_KEYS").env_remove("ANDROID_SERIAL").env("MOBILEMCP_DISABLE_TELEMETRY","1").env_remove("MOBILEMCP_LEGACY_ROBOT").env("MOBILECLI_PATH",root.join("server/node_modules/@mobilenext/mobilecli-windows-amd64/mobilecli-windows-amd64.exe"));
    }
    fn install(&self, id: &str) -> Result<()> {
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
        if id == "windows" && dest.exists() {
            remove_python_caches(&dest)?;
        }
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
                if id == "windows" && is_python_cache(&name) {
                    continue;
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
            self.stop_backend("windows").await;
        } else if module == "mobile" {
            self.stop_phone(Some(physical)).await;
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
        if self.running.lock().await.contains_key("windows") {
            return Ok(());
        }
        if let Err(e) = self.start("windows").await {
            let mut states = self.states.lock().await;
            let state = states.get_mut("windows").unwrap();
            state.phase = "failed".into();
            state.error = Some(e.clone());
            return Err(e);
        }
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
            if id == "mobile" {
                self.stop_phone(None).await;
                self.owned_phones.lock().await.clear();
            }
            self.stop_backend(id).await;
            let mut states = self.states.lock().await;
            let s = states.get_mut(id).unwrap();
            s.error = None;
            return Ok(());
        }
        if states[id].phase == "ready" {
            return Ok(());
        }
        states.get_mut(id).unwrap().phase = "starting".into();
        drop(states);
        let result = self.start(id).await;
        let mut states = self.states.lock().await;
        let s = states.get_mut(id).unwrap();
        match &result {
            Ok(_) => {
                s.phase = "ready".into();
                s.version = match id {
                    "windows" => "0.8.9",
                    "mobile" => "1.0.17",
                    _ => "1",
                }
                .into();
                s.error = None
            }
            Err(e) => {
                s.phase = "failed".into();
                s.error = Some(e.clone())
            }
        }
        result
    }
    async fn start(&self, id: &str) -> Result<()> {
        self.install(id)?;
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
        let root = self.module_root(id);
        let port = free_port()?;
        let token = format!("{}{}", crate::model::id(), crate::model::id());
        let mut running = Running::empty();
        let mut c = if id == "mobile" {
            fs::create_dir_all(self.root.join("android-user")).map_err(|e| e.to_string())?;
            spawn_owned(
                self.adb_server()
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
                &mut running,
            )?;
            let mut c = command(root.join("node/node.exe"));
            c.env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
            c.arg(root.join("server/lib/index.js"))
                .args(["--listen", &format!("127.0.0.1:{port}")]);
            self.mobile_env(&mut c);
            c.env("MOBILEMCP_AUTH", &token);
            c
        } else {
            let mut c = command(root.join("python/python.exe"));
            let working_directory = self.root.join("control/windows");
            fs::create_dir_all(&working_directory)
                .map_err(|e| format!("Windows control working directory: {e}"))?;
            c.current_dir(&working_directory).args(["-m","windows_mcp","serve","--transport","streamable-http","--host","127.0.0.1","--port",&port.to_string(),"--stateless-http"]).env("PYTHONPATH",root.join("server/src")).env("PYTHONIOENCODING","utf-8").env("PYTHONDONTWRITEBYTECODE","1").env("PYTHONNOUSERSITE","1").env_remove("PYTHONHOME").env("ANONYMIZED_TELEMETRY","false").env("WINDOWS_MCP_AUTH_KEY",&token).env("WINDOWS_MCP_TOOLS","Snapshot,Screenshot,Click,Type,Shortcut,Scroll,Drag,Move,Wait,App,Clipboard,ControlStatus,MultiSelect,MultiEdit,Window");
            c
        };
        c.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_owned(&mut c, &mut running)?;
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let client = reqwest::Client::new();
        let mut ready = false;
        for _ in 0..100 {
            if running
                .children
                .iter_mut()
                .any(|p| p.try_wait().ok().flatten().is_some())
            {
                break;
            }
            if let Ok(r)=client.post(&endpoint).bearer_auth(&token).header("accept","application/json, text/event-stream").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"maxa-device","version":"0.1.0"}}})).send().await{if r.status().is_success(){ready=true;break}}
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if !ready {
            for c in &mut running.children {
                let _ = c.kill().await;
            }
            return Err("module failed startup/MCP initialization; check private dependency versions and desktop access".into());
        }
        running.endpoint = Some(endpoint);
        running.token = token;
        self.running.lock().await.insert(id.into(), running);
        Ok(())
    }
    fn phone_home(&self, physical: &str) -> PathBuf {
        self.root
            .join("mcli")
            .join(&digest(physical.as_bytes())[..16])
    }
    fn mobile_output_environment(&self, command: &mut Command) -> Result<crate::files::Guarded> {
        let staging = self.root.join("staging");
        let _data = crate::files::Files::pin(&self.root, false)?;
        fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let pinned = crate::files::Files::pin(&staging, false)?;
        // Upstream permits cwd and os.tmpdir(): both must resolve to this one private root.
        // Keep its non-reparse ancestors pinned for the entire worker/daemon lifetime.
        command
            .current_dir(&pinned.path)
            .env("TEMP", &pinned.path)
            .env("TMP", &pinned.path)
            .env("TMPDIR", &pinned.path);
        Ok(pinned)
    }
    fn phone_cli(&self, physical: &str) -> Command {
        let mut c = self.mobilecli();
        c.env("MOBILECLI_HOME", self.phone_home(physical));
        c
    }
    pub async fn stop_phone(&self, physical: Option<&str>) {
        let mut workers = self.phone_running.lock().await;
        let ids: Vec<_> = workers
            .keys()
            .filter(|id| physical.map(|p| p == id.as_str()).unwrap_or(true))
            .cloned()
            .collect();
        for id in ids {
            // Each daemon has only ever received this physical phone's calls.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                self.phone_cli(&id).args(["daemon", "stop"]).output(),
            )
            .await;
            if let Some(mut worker) = workers.remove(&id) {
                for child in &mut worker.children {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
            self.owned_phones.lock().await.remove(&id);
        }
    }
    pub async fn rpc_phone(
        &self,
        physical: &str,
        message: &Value,
        timeout: f64,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        self.ready("mobile").await?;
        let (endpoint, token) = {
            let mut workers = self.phone_running.lock().await;
            self.ready("mobile").await?;
            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("CONTROL_STOPPED before phone worker startup".into());
            }
            if !workers.contains_key(physical) {
                let mut worker = Running::empty();
                let port = free_port()?;
                let token = format!("{}{}", id(), id());
                fs::create_dir_all(self.phone_home(physical)).map_err(|e| e.to_string())?;
                let mut daemon = self.phone_cli(physical);
                worker._output_root = Some(self.mobile_output_environment(&mut daemon)?);
                spawn_owned(
                    daemon
                        .args(["daemon", "start", "--idle-timeout", "0"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null()),
                    &mut worker,
                )?;
                let root = self.module_root("mobile");
                let mut node = command(root.join("node/node.exe"));
                self.mobile_env(&mut node);
                // A second guard also covers construction failure before the worker is registered.
                let node_root = self.mobile_output_environment(&mut node)?;
                node.env("MOBILECLI_HOME", self.phone_home(physical))
                    .env("MAXA_DEVICE_MANAGED", "1")
                    .env("MOBILEMCP_AUTH", &token)
                    .arg(root.join("server/lib/index.js"))
                    .args(["--listen", &format!("127.0.0.1:{port}")])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                spawn_owned(&mut node, &mut worker)?;
                drop(node_root); // worker._output_root retains the same pinned ancestors.
                worker.endpoint = Some(format!("http://127.0.0.1:{port}/mcp"));
                worker.token = token;
                let mut ready = false;
                for _ in 0..100 {
                    if worker
                        .children
                        .iter_mut()
                        .any(|p| !matches!(p.try_wait(), Ok(None)))
                    {
                        break;
                    }
                    if self.rpc_endpoint(worker.endpoint.as_ref().unwrap(),&worker.token,&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"maxa-device","version":"0.1.0"}}}),3.0).await.is_ok(){ready=true;break;}
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if !ready {
                    return Err("private phone backend failed startup".into());
                }
                workers.insert(physical.into(), worker);
            }
            let worker = &workers[physical];
            (worker.endpoint.clone().unwrap(), worker.token.clone())
        };
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("CONTROL_STOPPED before upstream send".into());
        }
        self.rpc_endpoint(&endpoint, &token, message, timeout).await
    }
    pub async fn rpc(&self, module: &str, message: &Value) -> Result<Value> {
        self.rpc_timeout(module, message, 300.0).await
    }
    pub async fn rpc_timeout(&self, module: &str, message: &Value, timeout: f64) -> Result<Value> {
        self.rpc_controlled(module, message, timeout, None).await
    }
    pub async fn rpc_controlled(
        &self,
        module: &str,
        message: &Value,
        timeout: f64,
        cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Value> {
        if module == "windows" {
            self.ensure_windows_worker(cancelled).await?;
        } else {
            self.ready(module).await?;
        }
        if !timeout.is_finite() {
            return Err("MCP timeout must be finite".into());
        }
        let (endpoint, token) = {
            let running = self.running.lock().await;
            let r = running.get(module).ok_or("module stopped")?;
            (
                r.endpoint.clone().ok_or("module endpoint missing")?,
                r.token.clone(),
            )
        };
        if cancelled
            .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false)
        {
            return Err("CONTROL_STOPPED before upstream send".into());
        }
        self.rpc_endpoint(&endpoint, &token, message, timeout).await
    }
    async fn rpc_endpoint(
        &self,
        endpoint: &str,
        token: &str,
        message: &Value,
        timeout: f64,
    ) -> Result<Value> {
        if !timeout.is_finite() {
            return Err("MCP timeout must be finite".into());
        }
        let mut request = reqwest::Client::new()
            .post(endpoint)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(message);
        if timeout > 0.0 {
            request = request.timeout(std::time::Duration::from_secs_f64(timeout));
        }
        let result = request
            .send()
            .await
            .map_err(|e| format!("OUTCOME_UNKNOWN: upstream request interrupted: {e}"))?;
        if !result.status().is_success() {
            return Err(format!("upstream HTTP {}", result.status()));
        }
        let body = result.text().await.map_err(|e| e.to_string())?;
        serde_json::from_str(&body)
            .or_else(|_| {
                body.lines()
                    .filter_map(|l| l.strip_prefix("data: "))
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .find(|v: &Value| v.get("id") == message.get("id"))
                    .ok_or_else(|| serde::de::Error::custom("MCP response not found"))
            })
            .map_err(|e: serde_json::Error| e.to_string())
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
        let out = self
            .adb()
            .args(["connect", address])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() || String::from_utf8_lossy(&out.stdout).contains("failed") {
            return Err("Android connection failed".into());
        }
        Ok(())
    }
    pub async fn phone_preflight(&self, physical: &str, transport: &str) -> Result<()> {
        self.ready("mobile").await?;
        let state = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            self.adb().args(["-s", transport, "get-state"]).output(),
        )
        .await
        .map_err(|_| "Android readiness timed out")?
        .map_err(|e| e.to_string())?;
        if !state.status.success() || String::from_utf8_lossy(&state.stdout).trim() != "device" {
            return Err(
                "Android offline/unauthorized; confirm device debugging authorization".into(),
            );
        }
        let shell = self
            .adb()
            .args(["-s", transport, "shell", "id", "-u"])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !shell.status.success() || String::from_utf8_lossy(&shell.stdout).trim() != "2000" {
            return Err(
                "Authorized Android shell UID 2000 required; no root/permission fallback".into(),
            );
        }
        let serial = self
            .adb()
            .args(["-s", transport, "shell", "getprop", "ro.serialno"])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        let identity = String::from_utf8_lossy(&serial.stdout).trim().to_string();
        if !serial.status.success()
            || identity.is_empty()
            || identity == "unknown"
            || format!("android:{}", digest(identity.as_bytes())) != physical
        {
            return Err(
                "PHONE_IDENTITY_CHANGED: refresh physical phone/transport mapping; no input sent"
                    .into(),
            );
        }
        // Readiness is not proof of OEM input permission. Actual UI/screenshot/input errors remain authoritative.
        Ok(())
    }
    pub async fn mobile_handover_check(&self, physical: &str, transport: &str) -> Result<()> {
        let out = self
            .adb()
            .args(["-s", transport, "shell", "ps", "-A", "-o", "PID,ARGS"])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("Cannot inspect Android automation ownership; control refused".into());
        }
        let processes = String::from_utf8_lossy(&out.stdout);
        let signature = automation_signature(&processes);
        let remembered = self.owned_phones.lock().await.get(physical).cloned();
        let live = {
            let mut workers = self.phone_running.lock().await;
            workers
                .get_mut(physical)
                .map(|w| {
                    w.children
                        .iter_mut()
                        .all(|c| matches!(c.try_wait(), Ok(None)))
                })
                .unwrap_or(false)
        };
        let own = live && !signature.is_empty() && remembered.as_deref() == Some(&signature);
        if processes.lines().any(|line| {
            line.contains("UiDumpServer")
                || line.contains("uiautomator")
                || line.contains("com.mobilenext.devicekit")
                || line.contains("DeviceServer") && !own
        }) {
            return Err("ANDROID_CONTROL_BUSY: existing automation requires explicit human handover; ADB ports do not isolate UiAutomation".into());
        }
        Ok(())
    }
    pub async fn mark_mobile_owned(&self, physical: &str, transport: &str) {
        if let Ok(out) = self
            .adb()
            .args(["-s", transport, "shell", "ps", "-A", "-o", "PID,ARGS"])
            .output()
            .await
        {
            if out.status.success() {
                let signature = automation_signature(&String::from_utf8_lossy(&out.stdout));
                if !signature.is_empty() {
                    self.owned_phones
                        .lock()
                        .await
                        .insert(physical.into(), signature);
                }
            }
        }
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
        let services = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.adb().args(["mdns", "services"]).output(),
        )
        .await
        {
            Ok(Ok(out)) if out.status.success() => {
                mdns_services(&String::from_utf8_lossy(&out.stdout))
            }
            _ => HashMap::new(),
        };
        // A discovery candidate is never a new pairing. Only remembered service+physical identity may reconnect.
        for (service, route) in &mut routes {
            if removed.contains(&route.physical) {
                continue;
            }
            let Some(address) = services.get(service) else {
                continue;
            };
            if let Ok(Ok(out)) = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                self.adb().args(["connect", address]).output(),
            )
            .await
            {
                if out.status.success() {
                    if let Ok(serial) = self
                        .adb()
                        .args(["-s", address, "shell", "getprop", "ro.serialno"])
                        .output()
                        .await
                    {
                        let ok = serial.status.success();
                        let serial = String::from_utf8_lossy(&serial.stdout).trim().to_string();
                        if ok
                            && !serial.is_empty()
                            && format!("android:{}", digest(serial.as_bytes())) == route.physical
                        {
                            route.address = address.clone();
                            continue;
                        }
                    }
                    let _ = self.adb().args(["disconnect", address]).output().await;
                }
            }
        }
        let out = self
            .adb()
            .arg("devices")
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("ADB enumeration failed".into());
        }
        let mut by_id: HashMap<String, PhoneInfo> = HashMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
            let mut parts = line.split_whitespace();
            let Some(transport) = parts.next() else {
                continue;
            };
            let status = parts.next().unwrap_or("unknown");
            let mut info = PhoneInfo {
                removed: false,
                id: format!("unverified:{}", digest(transport.as_bytes())),
                name: "Android".into(),
                transports: vec![transport.into()],
                active_transport: None,
                state: status.into(),
                reason: Some("physical identity has not been verified".into()),
            };
            if status == "device" {
                let serial = self
                    .adb()
                    .args(["-s", transport, "shell", "getprop", "ro.serialno"])
                    .output()
                    .await
                    .map_err(|e| e.to_string())?;
                let serial = String::from_utf8_lossy(&serial.stdout).trim().to_string();
                if !serial.is_empty() && serial != "unknown" {
                    info.id = format!("android:{}", digest(serial.as_bytes()));
                    info.active_transport = Some(transport.into());
                    info.reason = None;
                    let model = self
                        .adb()
                        .args(["-s", transport, "shell", "getprop", "ro.product.model"])
                        .output()
                        .await
                        .map_err(|e| e.to_string())?;
                    info.name = String::from_utf8_lossy(&model.stdout).trim().into();
                }
            }
            if let Some(previous) = by_id.get_mut(&info.id) {
                previous.transports.extend(info.transports)
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
            let _ = self.set(&id, false).await;
            if let Some(s) = self.states.lock().await.get_mut(&id) {
                s.phase = "failed".into();
                s.error =
                    Some("Owned module process exited; no global fallback was started".into());
            }
        }
    }
    pub async fn shutdown(&self) {
        for id in ["windows", "mobile", "system"] {
            let _ = self.set(id, false).await;
        }
    }
}
fn is_python_cache(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("pyc" | "pyo")
    )
}
fn remove_python_caches(root: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(root)
        .map_err(|e| format!("component cache metadata {}: {e}", root.display()))?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = meta.file_type().is_symlink();
    if reparse {
        return Err(format!(
            "REPARSE_POINT_DENIED: component cache {}",
            root.display()
        ));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(root)
            .map_err(|e| format!("component cache scan {}: {e}", root.display()))?
        {
            remove_python_caches(&entry.map_err(|e| e.to_string())?.path())?;
        }
    } else if is_python_cache(root) {
        fs::remove_file(root)
            .map_err(|e| format!("component cache cleanup {}: {e}", root.display()))?;
    }
    Ok(())
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
        // Relocatable Python bytecode is mutable and is purged before enabling Windows.
        // Source/native files remain strictly verified; existing pinned pack archives stay unchanged.
        if p.module == "windows" && is_python_cache(relative) {
            continue;
        }
        let bytes =
            fs::read(root.join(relative)).map_err(|e| format!("component missing: {name}: {e}"))?;
        if &digest(&bytes) != expected {
            return Err(format!("component file digest mismatch: {name}"));
        }
    }
    Ok(())
}
fn automation_signature(processes: &str) -> String {
    processes
        .lines()
        .filter(|line| line.contains("com.mobilenext.mobilecli.DeviceServer"))
        .collect::<Vec<_>>()
        .join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_repair_preserves_a_launched_process_working_directory() {
        use std::io::Write;
        use tokio::io::AsyncBufReadExt;
        let root = std::env::temp_dir().join(format!("maxa-windows-repair-{}", id()));
        let resources = root.join("resources");
        fs::create_dir_all(resources.join("packs")).unwrap();
        let file = "python/Lib/__pycache__/fixture.cpython-314.pyc";
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(file, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"verified pack bytes").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        fs::write(resources.join("packs/windows.zip"), &bytes).unwrap();
        let pack = Pack {
            module: "windows".into(),
            version: "0.8.9".into(),
            archive: "windows.zip".into(),
            sha256: digest(&bytes),
            files: HashMap::from([(file.into(), digest(b"verified pack bytes"))]),
        };
        fs::write(
            resources.join("packs/manifest.json"),
            serde_json::to_vec(&Packs {
                format: 1,
                packs: vec![pack],
            })
            .unwrap(),
        )
        .unwrap();
        let legacy = root.join("modules/windows");
        fs::create_dir_all(legacy.join("server")).unwrap();
        fs::create_dir_all(legacy.join("python/Lib/__pycache__")).unwrap();
        fs::write(legacy.join(file), b"runtime regenerated cache").unwrap();
        let modules = Modules::new(root.clone(), resources).unwrap();
        let cwd = legacy.join("server");
        let script=format!("[Environment]::CurrentDirectory='{}';[Console]::WriteLine('READY');Start-Sleep -Seconds 60",cwd.to_string_lossy().replace("'","''"));
        let mut child_command = command("powershell.exe");
        child_command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut owned = Running::empty();
        spawn_owned(&mut child_command, &mut owned).unwrap();
        let mut output = tokio::io::BufReader::new(owned.children[0].stdout.take().unwrap());
        let mut ready = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            output.read_line(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(ready.trim(), "READY");
        let denial = fs::rename(&legacy, root.join("would-be-backup")).unwrap_err();
        eprintln!(
            "LOCKED_COMPONENT_RENAME raw_os_error={:?}",
            denial.raw_os_error()
        );
        assert_eq!(denial.raw_os_error(), Some(5));
        let installed = modules.install("windows");
        assert!(owned.children[0].try_wait().unwrap().is_none());
        for child in &mut owned.children {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        drop(owned);
        let installed = installed
            .expect("repair must not rename a component tree held by a launched application's cwd");
        assert!(
            !legacy.join(file).exists(),
            "mutable bytecode must not survive the next enable"
        );
        // Resolve again: the verified repaired generation must be reused, not extracted on every enable.
        let again = modules.install("windows").unwrap();
        assert_eq!(installed, again);
        assert!(legacy.join("server").is_dir());
        drop(modules);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn windows_cache_cleanup_preserves_source_verification() {
        let root = std::env::temp_dir().join(id());
        fs::create_dir_all(root.join("python/Lib/__pycache__")).unwrap();
        let source = "python/Lib/component.py";
        let cache = "python/Lib/__pycache__/component.cpython-314.pyc";
        fs::write(root.join(source), b"immutable source").unwrap();
        fs::write(root.join(cache), b"relocated runtime bytecode").unwrap();
        let pack = Pack {
            module: "windows".into(),
            version: "0.8.9".into(),
            archive: "unused.zip".into(),
            sha256: String::new(),
            files: HashMap::from([
                (source.into(), digest(b"immutable source")),
                (cache.into(), digest(b"original cache bytes")),
            ]),
        };
        remove_python_caches(&root).unwrap();
        assert!(!root.join(cache).exists());
        assert!(verify(&root, &pack).is_ok());
        fs::write(root.join(source), b"changed source").unwrap();
        assert!(verify(&root, &pack)
            .unwrap_err()
            .contains("digest mismatch"));
        fs::remove_dir_all(root).unwrap();
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
    fn mobile_worker_uses_one_private_output_root() {
        let root = std::env::temp_dir().join(id());
        fs::create_dir(&root).unwrap();
        let modules = Modules::new(root.clone(), root.join("resources")).unwrap();
        let mut node = command("not-spawned");
        let pin = modules.mobile_output_environment(&mut node).unwrap();
        let expected = root.join("staging");
        assert_eq!(pin.path, expected);
        let config = node.as_std();
        assert_eq!(config.get_current_dir(), Some(expected.as_path()));
        let env: HashMap<_, _> = config.get_envs().collect();
        for key in ["TEMP", "TMP", "TMPDIR"] {
            assert_eq!(env[std::ffi::OsStr::new(key)], Some(expected.as_os_str()));
        }
        #[cfg(windows)]
        assert!(
            fs::rename(&expected, root.join("replaced")).is_err(),
            "worker lifetime must pin output root"
        );
        drop(pin);
        drop(modules);
        fs::remove_dir_all(root).unwrap();
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
