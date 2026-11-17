use crate::{
    files::Files, identity::HostIdentity, jobs::Jobs, model::*, modules::Modules, store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as SyncMutex,
    },
};
use tokio::sync::Mutex;
pub struct AppState {
    pub preferences: Mutex<crate::preferences::Preferences>,
    settings_change: Mutex<()>,
    shutting_down: AtomicBool,
    pub identity: HostIdentity,
    pub store: Arc<Store>,
    pub modules: Arc<Modules>,
    pub jobs: Arc<Jobs>,
    pub files: Files,
    pub transfers: crate::transfer::Transfers,
    pub artifacts: Arc<crate::artifacts::Artifacts>,
    pub data: PathBuf,
    pub service: Mutex<ServiceInfo>,
    pub pairing: crate::pairing::Pairing,
    pub activities: Mutex<Vec<ControlInfo>>,
    pub phones: Mutex<Vec<PhoneInfo>>,
    pub mutation: Mutex<()>,
    pub advertisement: Mutex<Option<crate::discovery::Advertisement>>,
    recordings: Mutex<HashMap<String, RecordingFiles>>,
    controls: SyncMutex<HashMap<String, Arc<AtomicBool>>>,
    control_epochs: SyncMutex<HashMap<String, u64>>,
    client_epochs: SyncMutex<HashMap<String, u64>>,
    phone_refresh: Mutex<()>,
    removed_phones: Mutex<HashMap<String, PhoneInfo>>,
    worker_owners: Mutex<HashMap<String, ControlInfo>>,
}
#[derive(Clone)]
struct ControlEpoch {
    operation_id: String,
    started: Arc<AtomicBool>,
    module: String,
    resource: String,
    value: u64,
    client: Option<(String, u64)>,
}
struct InvocationGuard {
    app: Arc<AppState>,
    client: String,
    chat: String,
    request: String,
    epoch: Option<ControlEpoch>,
    armed: bool,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if self.armed {
            let _=self.app.store.complete(&self.client,&self.request,&json!({"error":"OUTCOME_UNKNOWN: invocation connection interrupted; query state before retrying"}));
            if let Some(epoch) = self
                .epoch
                .clone()
                .filter(|e| e.started.load(Ordering::SeqCst))
            {
                self.app.mark_interrupted(&epoch.module, &epoch.resource);
                let app = self.app.clone();
                let client = self.client.clone();
                let chat = self.chat.clone();
                tokio::spawn(async move {
                    app.interrupt_operation(&client, &chat, &epoch).await;
                });
            }
        }
    }
}
struct RecordingFiles {
    files: Arc<McpFiles>,
    control: Arc<AtomicBool>,
    epoch: u64,
    client_epoch: Option<(String, u64)>,
    // Native retrieval completed; only artifact publication/commit remains.
    retrieved: bool,
}
impl RecordingFiles {
    fn valid(&self, epoch: &ControlEpoch) -> bool {
        !self.control.load(Ordering::SeqCst)
            && self.epoch == epoch.value
            && self.client_epoch == epoch.client
    }
}
struct McpFiles {
    dir: PathBuf,
    outputs: Vec<crate::files::Output>,
    private_outputs: Vec<PathBuf>,
    client: String,
    chat: String,
}
impl Drop for McpFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn replace_result_paths(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(text) => {
            for (from, to) in replacements {
                *text = text.replace(from, to);
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_result_paths(item, replacements)
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                replace_result_paths(item, replacements)
            }
        }
        _ => {}
    }
}
impl AppState {
    pub fn open(data: PathBuf, resources: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        let identity = HostIdentity::load(&data)?;
        let store = Arc::new(Store::open(&data.join("state.sqlite"))?);
        let preferences = store.preferences()?;
        let files = Files::new(vec![data.clone(), resources.clone()]);
        let pairing = crate::pairing::Pairing::new(identity.public.clone());
        let modules = Modules::new(data.clone(), resources)?;
        let artifacts = Arc::new(crate::artifacts::Artifacts::new(
            &data,
            &identity.public.host_id,
        ));
        let jobs = Jobs::new(store.clone(), data.clone(), artifacts.clone());
        // Retire pause-only state; removed phones, revoked clients and module switches are unchanged.
        match std::fs::remove_file(data.join("paused-phones.json")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(format!("Cannot retire paused phone state: {e}")),
        }
        Ok(Arc::new(Self {
            preferences: Mutex::new(preferences),
            settings_change: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            identity,
            store,
            modules,
            jobs,
            files,
            transfers: crate::transfer::Transfers::default(),
            artifacts,
            data: data.clone(),
            service: Mutex::new(ServiceInfo {
                state: "starting".into(),
                address: None,
                error: None,
            }),
            pairing,
            activities: Mutex::new(Vec::new()),
            phones: Mutex::new(Vec::new()),
            mutation: Mutex::new(()),
            advertisement: Mutex::new(None),
            recordings: Mutex::new(HashMap::new()),
            controls: SyncMutex::new(HashMap::new()),
            control_epochs: SyncMutex::new(HashMap::new()),
            client_epochs: SyncMutex::new(HashMap::new()),
            phone_refresh: Mutex::new(()),
            removed_phones: Mutex::new(match std::fs::read(data.join("removed-phones.json")) {
                Ok(v) => serde_json::from_slice(&v).map_err(|_| "invalid removed phone store")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
                Err(e) => return Err(e.to_string()),
            }),
            worker_owners: Mutex::new(HashMap::new()),
        }))
    }
    pub async fn windows_worker_event(&self, event: &Value) {
        if !self.modules.windows_event_current(event).await {
            return;
        }
        if !matches!(
            event["event"].as_str(),
            Some("takeover" | "control-stopped" | "worker-exit")
        ) {
            return;
        }
        let mut activities = self.activities.lock().await;
        if !self.modules.windows_event_current(event).await
            || event["epoch"].as_u64() != Some(self.control_epoch("windows", "desktop").await.value)
        {
            return;
        }
        self.invalidate_control("windows", "desktop").await;
        activities.retain(|a| a.module != "windows");
        self.worker_owners.lock().await.remove("windows:desktop");
        // Drain cooperative cancellation, then kill the owned tree after five seconds
        // if UIA/the operation thread is stuck. No replay and no module toggle.
        self.modules.release_automation("windows", "desktop").await;
    }
    async fn module_statuses(&self) -> Vec<ModuleStatus> {
        let selected = self.preferences.lock().await.modules.clone();
        let mut modules = self.modules.statuses().await;
        for module in &mut modules {
            module.enabled = selected.get(&module.id).copied().unwrap_or(false);
        }
        modules
    }
    pub async fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            protocol_version: 2,
            identity: self.identity.public.clone(),
            service: self.service.lock().await.clone(),
            modules: self.module_statuses().await,
            clients: self.store.clients(),
            pairing: self.pairing.requests(),
            phones: self.phone_status().await,
            controls: self.activities.lock().await.clone(),
            jobs: self.store.jobs(),
        }
    }
    pub async fn pair_request(&self, args: &Value) -> Result<Value> {
        self.pairing.request(args)
    }
    pub async fn decide(&self, request: &str, accept: bool) -> Result<()> {
        self.pairing.decide(request, accept, &self.store)
    }
    pub async fn poll_pair(&self, args: &Value) -> Result<Value> {
        self.pairing.poll(args)
    }
    pub async fn cancel_pair(&self, args: &Value) -> Result<()> {
        if let Some(client) = self.pairing.cancel(args)? {
            self.revoke(&client).await?;
        }
        Ok(())
    }
    pub async fn revoke(&self, client: &str) -> Result<()> {
        self.transfers.cancel(Some(client), &self.store);
        self.jobs.revoke(client).await?;
        self.stop(Some(client)).await;
        Ok(())
    }
    pub async fn phone_status(&self) -> Vec<PhoneInfo> {
        let mut phones = self.phones.lock().await.clone();
        let removed = self.removed_phones.lock().await;
        for phone in &mut phones {
            phone.removed = removed.contains_key(&phone.id);
            if phone.removed {
                phone.active_transport = None;
                phone.reason = Some("Removed by local human; add/pair/connect required".into());
            }
        }
        for (id, phone) in removed.iter() {
            if !phones.iter().any(|p| &p.id == id) {
                let mut p = phone.clone();
                p.removed = true;
                p.active_transport = None;
                phones.push(p);
            }
        }
        phones
    }
    pub async fn remove_phone(&self, physical: &str) -> Result<()> {
        let mut phone = self
            .phone_status()
            .await
            .into_iter()
            .find(|p| p.id == physical)
            .ok_or("unknown physical phone")?;
        phone.removed = true;
        phone.active_transport = None;
        let mut removed = self.removed_phones.lock().await;
        removed.insert(physical.into(), phone);
        let persisted = crate::identity::private_write(
            &self.data.join("removed-phones.json"),
            &serde_json::to_vec(&*removed).map_err(|e| e.to_string())?,
        );
        drop(removed);
        self.stop_control(None, Some(physical)).await?;
        persisted
    }
    pub async fn add_phone(&self, physical: &str) -> Result<()> {
        let removed_ids = self.removed_phones.lock().await.keys().cloned().collect();
        let phones = self.modules.phones(&removed_ids).await?;
        let phone = phones
            .iter()
            .find(|p| p.id == physical)
            .ok_or("physical phone is not currently verified")?;
        let transport = phone
            .active_transport
            .as_deref()
            .ok_or("phone offline/unauthorized; confirm USB or wireless OS authorization")?;
        self.modules.phone_preflight(physical, transport).await?;
        let mut removed = self.removed_phones.lock().await;
        let mut next = removed.clone();
        next.remove(physical);
        crate::identity::private_write(
            &self.data.join("removed-phones.json"),
            &serde_json::to_vec(&next).map_err(|e| e.to_string())?,
        )?;
        *removed = next;
        drop(removed);
        *self.phones.lock().await = phones;
        Ok(())
    }
    pub async fn connect_phone(&self, address: &str) -> Result<()> {
        self.modules.connect_phone(address).await?;
        let removed = self.removed_phones.lock().await.keys().cloned().collect();
        let phones = self.modules.phones(&removed).await?;
        let physical = phones
            .iter()
            .find(|p| p.transports.iter().any(|t| t == address) && p.active_transport.is_some())
            .map(|p| p.id.clone())
            .ok_or("connected endpoint did not verify a physical Android identity")?;
        *self.phones.lock().await = phones;
        self.add_phone(&physical).await
    }
    async fn control_epoch(&self, module: &str, resource: &str) -> ControlEpoch {
        let key = format!("{module}:{resource}");
        let value = *self.control_epochs.lock().unwrap().entry(key).or_default();
        ControlEpoch {
            operation_id: id(),
            started: Arc::new(AtomicBool::new(false)),
            module: module.into(),
            resource: resource.into(),
            value,
            client: None,
        }
    }
    fn mark_interrupted(&self, module: &str, resource: &str) {
        *self
            .control_epochs
            .lock()
            .unwrap()
            .entry(format!("{module}:{resource}"))
            .or_default() += 1;
        if let Some(cancelled) = self
            .controls
            .lock()
            .unwrap()
            .remove(&format!("{module}:{resource}"))
        {
            cancelled.store(true, Ordering::SeqCst);
        }
    }
    async fn invalidate_control(&self, module: &str, resource: &str) {
        self.mark_interrupted(module, resource);
    }
    async fn invalidate_module(&self, module: &str) {
        let prefix = format!("{module}:");
        for (key, value) in self.control_epochs.lock().unwrap().iter_mut() {
            if key.starts_with(&prefix) {
                *value += 1;
            }
        }
    }
    async fn queued_control(
        &self,
        client: &str,
        call: &Invocation,
    ) -> Result<Option<ControlEpoch>> {
        if call.operation != "mcp" || call.arguments["message"]["method"] != "tools/call" {
            return Ok(None);
        }
        let module = call.arguments["module"].as_str().ok_or("module required")?;
        if module == "windows" {
            let mut epoch = self.control_epoch("windows", "desktop").await;
            epoch.operation_id = call.request_id.clone();
            epoch.client = Some((
                client.into(),
                *self
                    .client_epochs
                    .lock()
                    .unwrap()
                    .entry(client.into())
                    .or_default(),
            ));
            return Ok(Some(epoch));
        }
        if module == "mobile"
            && call.arguments["message"]["params"]["name"] != "mobile_list_available_devices"
        {
            let transport = call.arguments["message"]["params"]["arguments"]["device"]
                .as_str()
                .ok_or("device required")?;
            let physical = self
                .phones
                .lock()
                .await
                .iter()
                .find(|p| p.active_transport.as_deref() == Some(transport))
                .map(|p| p.id.clone())
                .ok_or("phone transport unavailable")?;
            let mut epoch = self.control_epoch("mobile", &physical).await;
            epoch.operation_id = call.request_id.clone();
            epoch.client = Some((
                client.into(),
                *self
                    .client_epochs
                    .lock()
                    .unwrap()
                    .entry(client.into())
                    .or_default(),
            ));
            return Ok(Some(epoch));
        }
        Ok(None)
    }
    async fn check_control_epoch(&self, epoch: &ControlEpoch) -> Result<()> {
        let current = self.control_epoch(&epoch.module, &epoch.resource).await;
        let client_stopped = epoch
            .client
            .as_ref()
            .map(|(client, value)| {
                *self
                    .client_epochs
                    .lock()
                    .unwrap()
                    .entry(client.clone())
                    .or_default()
                    != *value
            })
            .unwrap_or(false);
        if current.value != epoch.value || client_stopped {
            return Err("CONTROL_STOPPED: old queued operation was invalidated; submit only a new explicit request".into());
        }
        Ok(())
    }
    pub async fn stop_control(&self, client: Option<&str>, phone: Option<&str>) -> Result<()> {
        if let Some(physical) = phone {
            if !self.phones.lock().await.iter().any(|p| p.id == physical) {
                return Err("unknown physical phone".into());
            }
            let mut activities = self.activities.lock().await;
            self.invalidate_control("mobile", physical).await;
            let removed = self
                .worker_owners
                .lock()
                .await
                .values()
                .filter(|l| l.module == "mobile" && l.resource == physical)
                .cloned()
                .collect();
            activities.retain(|l| !(l.module == "mobile" && l.resource == physical));
            self.release_controls(removed).await;
            let stopped = self.modules.stop_phone(Some(physical)).await;
            self.recordings.lock().await.remove(physical);
            drop(activities);
            stopped?;
        } else {
            self.stop(client).await;
        }
        Ok(())
    }
    pub async fn stop(&self, client: Option<&str>) {
        let mut activities = self.activities.lock().await;
        if let Some(client) = client {
            *self
                .client_epochs
                .lock()
                .unwrap()
                .entry(client.into())
                .or_default() += 1;
        } else {
            self.invalidate_module("windows").await;
            self.invalidate_module("mobile").await;
        }
        let removed: Vec<ControlInfo> = self
            .worker_owners
            .lock()
            .await
            .values()
            .filter(|l| client.map(|c| l.client_id == c).unwrap_or(true))
            .cloned()
            .collect();
        activities.retain(|l| !client.map(|c| l.client_id == c).unwrap_or(true));
        self.release_controls(removed).await;
        drop(activities);
    }
    async fn release_controls(&self, activities: Vec<ControlInfo>) {
        for activity in activities {
            if let Some(cancelled) = self
                .controls
                .lock()
                .unwrap()
                .remove(&format!("{}:{}", activity.module, activity.resource))
            {
                cancelled.store(true, Ordering::SeqCst);
            }
            self.worker_owners
                .lock()
                .await
                .remove(&format!("{}:{}", activity.module, activity.resource));
            self.modules
                .release_automation(&activity.module, &activity.resource)
                .await;
            if activity.module == "mobile" {
                self.recordings.lock().await.remove(&activity.resource);
            }
        }
    }
    async fn release_failed_control(&self, epoch: &ControlEpoch, token: &Arc<AtomicBool>) {
        let mut activities = self.activities.lock().await;
        let current = self
            .controls
            .lock()
            .unwrap()
            .get(&format!("{}:{}", epoch.module, epoch.resource))
            .cloned();
        if current
            .as_ref()
            .map(|c| Arc::ptr_eq(c, token))
            .unwrap_or(false)
        {
            let removed = activities
                .iter()
                .filter(|l| l.module == epoch.module && l.resource == epoch.resource)
                .cloned()
                .collect();
            activities.retain(|l| !(l.module == epoch.module && l.resource == epoch.resource));
            self.invalidate_control(&epoch.module, &epoch.resource)
                .await;
            self.release_controls(removed).await;
        }
    }
    async fn interrupt_operation(&self, client: &str, chat: &str, epoch: &ControlEpoch) {
        let mut active = self.activities.lock().await;
        let removed = active
            .iter()
            .filter(|a| a.id == epoch.operation_id && a.client_id == client && a.chat_id == chat)
            .cloned()
            .collect();
        active.retain(|a| {
            !(a.id == epoch.operation_id && a.client_id == client && a.chat_id == chat)
        });
        self.release_controls(removed).await;
    }
    pub async fn set_appearance(&self, appearance: crate::preferences::Appearance) -> Result<()> {
        let _change = self.settings_change.lock().await;
        let mut current = self.preferences.lock().await;
        let mut next = current.clone();
        next.appearance = appearance;
        self.store.save_preferences(&next)?;
        *current = next;
        Ok(())
    }
    pub async fn copy_symlinks_status(&self) -> (bool, Option<String>) {
        let preferences = self.preferences.lock().await;
        let error = crate::preferences::symlinks::configured().err().map(|message|
            crate::preferences::native_text(preferences.appearance.language.chinese(), &message));
        (preferences.copy_symlinks && error.is_none(), error)
    }
    pub async fn set_copy_symlinks(&self, enabled: bool) -> Result<()> {
        let _change = self.settings_change.lock().await;
        let mut current = self.preferences.lock().await;
        let verification = if enabled { crate::preferences::symlinks::verify() } else { Ok(()) };
        let mut next = current.clone();
        next.copy_symlinks = enabled && verification.is_ok();
        self.store.save_preferences(&next)?;
        *current = next;
        if !current.copy_symlinks { self.transfers.cancel_links(&self.store); }
        verification.map_err(|message| crate::preferences::native_text(current.appearance.language.chinese(), &message))
    }
    pub async fn restore_modules(&self) {
        let _change = self.settings_change.lock().await;
        let selected = self.preferences.lock().await.modules.clone();
        for (module, enabled) in selected {
            if self.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            if enabled {
                let _ = self.set_module_runtime(&module, true).await;
            }
        }
    }
    pub async fn set_module(&self, module: &str, enabled: bool) -> Result<()> {
        let _change = self.settings_change.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("App is shutting down".into());
        }
        let saved = {
            let mut current = self.preferences.lock().await;
            let mut next = current.clone();
            *next.modules.get_mut(module).ok_or("unknown module")? = enabled;
            let saved = self.store.save_preferences(&next);
            if enabled {
                saved.as_ref().map_err(Clone::clone)?;
            }
            // Fail closed when disabling, even if persistence failed; report the failure.
            *current = next;
            saved
        };
        let runtime = self.set_module_runtime(module, enabled).await;
        saved.and(runtime)
    }
    async fn set_module_runtime(&self, module: &str, enabled: bool) -> Result<()> {
        if module == "system" && !enabled { self.transfers.cancel(None, &self.store); }
        if module == "system" && !enabled {
            self.jobs.set_enabled(false).await;
        }
        if !enabled {
            let mut activities = self.activities.lock().await;
            self.invalidate_module(module).await;
            let removed = self
                .worker_owners
                .lock()
                .await
                .values()
                .filter(|l| l.module == module)
                .cloned()
                .collect();
            activities.retain(|l| l.module != module);
            self.release_controls(removed).await;
            let result = self.modules.set(module, false).await;
            drop(activities);
            return result;
        }
        let result = self.modules.set(module, enabled).await;
        if module == "system" && enabled && result.is_ok() {
            self.jobs.set_enabled(true).await;
        }
        result
    }
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.transfers.cancel(None, &self.store);
        let _change = self.settings_change.lock().await;
        self.jobs.set_enabled(false).await;
        self.stop(None).await;
        self.modules.shutdown().await;
    }
    pub fn cleanup_pairing(&self) {
        self.transfers.cleanup(&self.files, &self.store);
        self.pairing.cleanup();
    }
    pub async fn refresh_phones(&self) -> Result<()> {
        let _refresh = self.phone_refresh.lock().await;
        let removed = self.removed_phones.lock().await.keys().cloned().collect();
        let phones = self.modules.phones(&removed).await?;
        self.modules.ready("mobile").await?;
        *self.phones.lock().await = phones;
        Ok(())
    }
    async fn acquire(
        &self,
        client: &str,
        chat: &str,
        module: &str,
        resource: &str,
        epoch: &ControlEpoch,
    ) -> Result<Arc<AtomicBool>> {
        let mut activities = self.activities.lock().await;
        self.modules.ready(module).await?;
        self.store.active(client)?;
        if module == "mobile" && self.removed_phones.lock().await.contains_key(resource) {
            return Err("PHONE_REMOVED: use local human add/pair/connect".into());
        }
        if epoch.module != module || epoch.resource != resource {
            return Err("Control epoch target mismatch".into());
        }
        self.check_control_epoch(epoch).await?;
        if activities.iter().any(|l| {
            l.module == module
                && l.resource == resource
                && (l.client_id != client || l.chat_id != chat)
        }) {
            return Err("CONTROL_BUSY: human handover required".into());
        }
        activities.retain(|l| !(l.module == module && l.resource == resource));
        let key = format!("{module}:{resource}");
        let previous = self.worker_owners.lock().await.get(&key).cloned();
        if previous
            .as_ref()
            .map(|p| {
                p.client_id != client
                    || p.chat_id != chat
                    || !self.controls.lock().unwrap().contains_key(&key)
            })
            .unwrap_or(false)
        {
            self.modules.release_automation(module, resource).await;
        }
        let activity = ControlInfo {
            id: epoch.operation_id.clone(),
            module: module.into(),
            resource: resource.into(),
            client_id: client.into(),
            chat_id: chat.into(),
            operation: "tools/call".into(),
            started_at: now(),
        };
        epoch.started.store(true, Ordering::SeqCst);
        activities.push(activity.clone());
        self.worker_owners.lock().await.insert(key, activity);
        let key = format!("{module}:{resource}");
        let control = self
            .controls
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        Ok(control)
    }
    pub async fn invoke(self: &Arc<Self>, client: &ClientInfo, call: &Invocation) -> Result<Value> {
        self.store.active(&client.id)?;
        if call.request_id.len() < 8 || call.chat_id.is_empty() {
            return Err("requestId/chatId required".into());
        }
        if call.operation == "device.status" {
            let phones = self.phone_status().await.into_iter().collect::<Vec<_>>();
            let mut status = json!({"hostId":self.identity.public.host_id,"service":self.service.lock().await.clone(),"modules":self.module_statuses().await,"phones":phones,"trusted":true,"copyVersion":1,"copyInternalSymlinks":self.copy_symlinks_status().await.0,"maxFileBytes":crate::transfer::MAX_FILE_BYTES});
            status.as_object_mut().unwrap().extend(crate::environment::context().as_object().unwrap().clone());
            return Ok(status);
        }
        if call.operation == "copy.status" || call.operation == "copy.cancel" {
            return self.transfers.operation(&self.files,&self.store,&client.id,&call.chat_id,&call.operation,&call.arguments);
        }
        if call.operation == "client.revoke_self" {
            self.revoke(&client.id).await?;
            return Ok(json!({"revoked":true}));
        }
        if call.operation == "request.status" {
            return self.store.request(
                &client.id,
                call.arguments["requestId"]
                    .as_str()
                    .ok_or("requestId required")?,
                &call.chat_id,
            );
        }
        if call.operation == "job.status" {
            return self
                .jobs
                .status(
                    &client.id,
                    &call.chat_id,
                    call.arguments["jobId"].as_str().ok_or("jobId required")?,
                )
                .await;
        }
        if call.operation == "job.cancel" {
            return self
                .jobs
                .cancel(
                    &client.id,
                    &call.chat_id,
                    call.arguments["jobId"].as_str().ok_or("jobId required")?,
                )
                .await;
        }
        // Capture resource generation before waiting for serialized effects: old queues
        // must not become fresh operations after an interruption or human stop.
        let queued_epoch = self.queued_control(&client.id, call).await;
        // All mutations, permission checks and request registration have one owner.
        let _guard = self.mutation.lock().await;
        if self
            .store
            .clients()
            .iter()
            .all(|c| c.id != client.id || c.revoked)
        {
            return Err("UNAUTHORIZED".into());
        }
        if let Some(prior) = self.store.claim(&client.id, call)? {
            if let Some(error) = prior.get("error").and_then(Value::as_str) {
                return Err(error.into());
            }
            return Ok(prior.get("value").cloned().unwrap_or(prior));
        }
        let mut cancellation = InvocationGuard {
            app: self.clone(),
            client: client.id.clone(),
            chat: call.chat_id.clone(),
            request: call.request_id.clone(),
            epoch: queued_epoch.as_ref().ok().and_then(|e| e.clone()),
            armed: true,
        };
        let result = match &queued_epoch {
            Ok(epoch) => self.execute(client, call, epoch.as_ref()).await,
            Err(e) => Err(e.clone()),
        };
        if let Ok(Some(epoch)) = &queued_epoch {
            self.activities.lock().await.retain(|a| {
                !(a.module == epoch.module
                    && a.resource == epoch.resource
                    && a.client_id == client.id
                    && a.chat_id == call.chat_id)
            });
        }
        let saved = match &result {
            Ok(v) => json!({"value":v}),
            Err(e) => json!({"error":e}),
        };
        self.store.complete(&client.id, &call.request_id, &saved)?;
        cancellation.armed = false;
        result
    }
    async fn execute(
        self: &Arc<Self>,
        client: &ClientInfo,
        call: &Invocation,
        epoch: Option<&ControlEpoch>,
    ) -> Result<Value> {
        let a = &call.arguments;
        match call.operation.as_str() {
            operation if operation.starts_with("copy.") => {
                self.modules.ready("system").await?;
                let enabled = self.copy_symlinks_status().await.0;
                if !enabled {
                    self.transfers.cancel_links(&self.store);
                    if operation != "copy.prepare" {
                        let id = a["transferId"].as_str().ok_or("transferId required")?;
                        if self.transfers.get(&client.id, &call.chat_id, id)?.has_links() {
                            return Err("COPY_SYMLINKS_DISABLED".into());
                        }
                    }
                }
                let mut args = a.clone();
                if operation == "copy.prepare" { args["symlinks"] = json!(enabled && a["symlinks"] != false); }
                self.transfers.operation(&self.files,&self.store,&client.id,&call.chat_id,operation,&args)
            }
            "environment.wsl_host" => {
                self.modules.ready("system").await?;
                crate::environment::probe(a).await
            }
            "file.stat" => {
                self.modules.ready("system").await?;
                self.files.stat(a)
            }
            "file.read" => {
                self.modules.ready("system").await?;
                self.files.read(a)
            }
            "file.write" => {
                self.modules.ready("system").await?;
                self.files.write(a)
            }
            "file.move" => {
                self.modules.ready("system").await?;
                self.files.mov(a)
            }
            "file.delete" => {
                self.modules.ready("system").await?;
                self.files.delete(a)
            }
            "file.mkdir" => {
                self.modules.ready("system").await?;
                self.files.mkdir(a)
            }
            "process.start" => {
                self.modules.ready("system").await?;
                self.jobs.launch(client, call).await
            }
            "search" => self.search(a).await,
            "mcp" => self.mcp(client, call, epoch).await,
            _ => Err("unsupported operation".into()),
        }
    }
    async fn search(&self, a: &Value) -> Result<Value> {
        self.modules.ready("system").await?;
        let pinned = self.files.search_root(std::path::Path::new(
            a["root"].as_str().ok_or("root required")?,
        ))?;
        let path = &pinned.path;
        let is_file = path.is_file();
        let directory = if is_file {
            path.parent().ok_or("missing parent")?
        } else {
            path.as_path()
        };
        let args = a["argv"].as_array().ok_or("rg argv required")?;
        let mut cmd = crate::modules::command(self.modules.module_root("system").join("rg.exe"));
        cmd.env_remove("RIPGREP_CONFIG_PATH").current_dir(directory);
        // Search is not an arbitrary argv/shell endpoint. Only query options are admitted.
        for arg in args {
            let arg = arg.as_str().ok_or("rg arguments must be strings")?;
            if !(arg == "--json"
                || arg == "--files"
                || arg == "--files-with-matches"
                || arg == "--sort=path"
                || arg == "--color=never"
                || arg == "--hidden"
                || arg == "--no-ignore"
                || arg == "--ignore-case"
                || arg == "--case-sensitive"
                || arg == "--smart-case"
                || arg == "--fixed-strings"
                || arg == "--no-config"
                || arg == "--engine=default"
                || arg.starts_with("--regexp=")
                || arg.starts_with("--glob=!"))
            {
                return Err(format!("unsupported rg option: {arg}"));
            }
            cmd.arg(arg);
        }
        for exclusion in self.files.exclusions(directory) {
            #[cfg(windows)]
            cmd.arg(format!("--iglob=!{exclusion}"));
            #[cfg(not(windows))]
            cmd.arg(format!("--glob=!{exclusion}"));
        }
        // Never follow symlinks; caller-controlled executable/preprocessor options are disallowed.
        cmd.arg("--").arg(if is_file {
            path.file_name().ok_or("missing filename")?
        } else {
            std::ffi::OsStr::new(".")
        });
        let out = tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output())
            .await
            .map_err(|_| "rg timed out")?
            .map_err(|e| e.to_string())?;
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&out.stderr).into());
        }
        Ok(
            json!({"stdout":String::from_utf8_lossy(&out.stdout),"code":out.status.code(),"root":path}),
        )
    }
    fn stage_mcp(
        &self,
        client: &ClientInfo,
        chat: &str,
        name: &str,
        message: &mut Value,
    ) -> Result<McpFiles> {
        let dir = self.data.join("staging").join(id());
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut files = McpFiles {
            dir,
            outputs: Vec::new(),
            private_outputs: Vec::new(),
            client: client.id.clone(),
            chat: chat.into(),
        };
        let args = &mut message["params"]["arguments"];
        for key in ["saveTo", "output", "save_path"] {
            if let Some(path) = args[key].as_str().map(str::to_owned) {
                let ext = std::path::Path::new(&path)
                    .extension()
                    .and_then(|v| v.to_str())
                    .unwrap_or("bin");
                if !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return Err("invalid output extension".into());
                }
                let stage = files.dir.join(format!("{}.{}", id(), ext));
                let output = self
                    .files
                    .stage_output(std::path::Path::new(&path), stage.clone())?;
                args[key] = json!(stage);
                files.outputs.push(output);
            }
        }
        if name == "mobile_install_app" {
            let path = args["path"].as_str().ok_or("path required")?;
            let stage = files.dir.join("input.apk");
            self.files.stage_input(std::path::Path::new(path), &stage)?;
            args["path"] = json!(stage);
        }
        if name == "mobile_start_screen_recording"
            && args.get("output").filter(|v| !v.is_null()).is_none()
        {
            let stage = files.dir.join("recording.mp4");
            args["output"] = json!(stage);
            files.private_outputs.push(stage);
        }
        Ok(files)
    }
    fn finish_mcp_files(
        &self,
        files: &McpFiles,
        client: &ClientInfo,
        result: &mut Value,
        allow_commit: bool,
    ) {
        let mut refs = Vec::new();
        let mut errors = Vec::new();
        let mut replacements = Vec::new();
        for (path, output) in files
            .outputs
            .iter()
            .map(|o| (&o.stage, Some(o)))
            .chain(files.private_outputs.iter().map(|p| (p, None)))
        {
            let label = output
                .map(|o| format!("remote:{}", o.destination().display()))
                .unwrap_or("remote recording artifact".into());
            replacements.push((path.to_string_lossy().to_string(), label));
            if !path.exists() {
                errors.push("Upstream output is missing; no complete artifact was returned".into());
                continue;
            }
            let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "mp4" => "video/mp4",
                _ => "application/octet-stream",
            };
            match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| self.artifacts.put(&files.client, &files.chat, &bytes, mime))
            {
                Ok(reference) => refs.push(reference),
                Err(e) => errors.push(e),
            }
            if let Some(output) = output {
                let committed = if allow_commit {
                    self.store
                        .active(&client.id)
                        .and_then(|_| self.files.commit_output(output))
                } else {
                    Err(
                        "CONTROL_STOPPED: output retained as artifact, destination not changed"
                            .into(),
                    )
                };
                if let Err(e) = committed {
                    errors.push(e);
                }
            }
        }
        replace_result_paths(result, &replacements);
        result["artifacts"] = json!(refs);
        if !errors.is_empty() {
            result["result"]["isError"] = json!(true);
            if !result["result"]["content"].is_array() {
                result["result"]["content"] = json!([]);
            }
            result["result"]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":format!("Artifact/commit failed; upstream effects may have occurred: {}",errors.join("; "))}));
        }
    }
    async fn mcp(
        &self,
        client: &ClientInfo,
        call: &Invocation,
        epoch: Option<&ControlEpoch>,
    ) -> Result<Value> {
        self.store.active(&client.id)?;
        if let Some(epoch) = epoch {
            self.check_control_epoch(epoch).await?;
        }
        let module = call.arguments["module"].as_str().ok_or("module required")?;
        if !matches!(module, "windows" | "mobile") {
            return Err("invalid MCP module".into());
        }
        self.modules.ready(module).await?;
        let mut outgoing = call.arguments["message"].clone();
        let message = &call.arguments["message"];
        let method = message["method"].as_str().ok_or("MCP method required")?;
        if module == "mobile" && method == "tools/call" {
            // Reconnect only previously verified routes; never replay an earlier UI action.
            self.refresh_phones().await?;
        }
        let mut staged = None;
        let mut recording_key = None;
        let mut stopping_recording = false;
        let mut recording_retrieved = false;
        if !matches!(
            method,
            "initialize" | "notifications/initialized" | "tools/list" | "tools/call"
        ) {
            return Err("MCP method is not public".into());
        }
        if method == "notifications/initialized" {
            return Ok(json!({}));
        }
        let mut controlled_transport: Option<String> = None;
        let mut control = None;
        if method == "tools/call" {
            let live = self
                .modules
                .dispatch(
                    module,
                    &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                )
                .await?;
            let mut tools = live["result"]["tools"]
                .as_array()
                .ok_or("native catalog unavailable")?
                .clone();
            tools.retain(|t| {
                t["name"]
                    .as_str()
                    .map(|n| {
                        if module == "windows" {
                            windows_tool(n)
                        } else {
                            mobile_tool(n)
                        }
                    })
                    .unwrap_or(false)
            });
            let revision = digest(&serde_json::to_vec(&tools).unwrap());
            if call.arguments["schemaRevision"].as_str() != Some(&revision) {
                return Err(
                    "SCHEMA_CHANGED: reload this host-bound Ability catalog before execution"
                        .into(),
                );
            }
            let name = message["params"]["name"]
                .as_str()
                .ok_or("tool name required")?;
            let args = &message["params"]["arguments"];
            let tool = tools
                .iter()
                .find(|t| t["name"] == name)
                .ok_or("tool is not in the admitted live catalog")?;
            crate::schema::validate(&tool["inputSchema"], args)?;
            if name != "mobile_batch_commands" && name != "mobile_stop_screen_recording" {
                staged = Some(Arc::new(self.stage_mcp(
                    client,
                    &call.chat_id,
                    name,
                    &mut outgoing,
                )?));
            }
            if module == "windows" {
                if !windows_tool(name) {
                    return Err("tool not public".into());
                }
                if name != "ControlStatus" {
                    control = Some(
                        self.acquire(
                            &client.id,
                            &call.chat_id,
                            "windows",
                            "desktop",
                            epoch.ok_or("control epoch required")?,
                        )
                        .await?,
                    );
                }
            } else {
                if !mobile_tool(name) {
                    return Err("tool not public".into());
                }
                if name == "mobile_list_available_devices" {
                    let phones = self.phone_status().await;
                    let devices:Vec<_>=phones.iter().map(|p|json!({"id":p.active_transport,"name":p.name,"platform":"android","type":"real","state":p.state,"physicalId":p.id,"removed":p.removed})).collect();
                    return Ok(
                        json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":[{"type":"text","text":json!({"devices":devices}).to_string()}]}}),
                    );
                }
                let transport = args["device"].as_str().ok_or("device required")?;
                let phones = self.phone_status().await;
                let physical_id = phones
                    .iter()
                    .find(|p| {
                        p.active_transport.as_deref() == Some(transport) && !p.removed
                    })
                    .ok_or("PHONE_UNAVAILABLE: transport is offline or removed; refresh devices or use local phone management")?
                    .id
                    .clone();
                drop(phones);
                if name == "mobile_batch_commands" {
                    for step in args["steps"].as_array().ok_or("steps required")? {
                        let step_name = step["name"].as_str().ok_or("step name required")?;
                        if !mobile_tool(step_name)
                            || step_name == "mobile_batch_commands"
                            || step_name == "mobile_take_screenshot"
                            || step_name == "mobile_list_available_devices"
                        {
                            return Err("unsupported batch step".into());
                        }
                        let mut arguments = step["arguments"]
                            .as_object()
                            .ok_or("step arguments must be an object")?
                            .clone();
                        if let Some(selected) = arguments.get("device") {
                            if selected.as_str() != Some(transport) {
                                return Err(
                                    "cross-phone batch requires separate explicit calls".into()
                                );
                            }
                        }
                        arguments
                            .entry("device".to_string())
                            .or_insert(json!(transport));
                        let step_args = Value::Object(arguments);
                        let definition = tools
                            .iter()
                            .find(|t| t["name"] == step_name)
                            .ok_or("step not in admitted catalog")?;
                        crate::schema::validate(&definition["inputSchema"], &step_args)?;
                        for key in ["saveTo", "output"] {
                            if let Some(path) = step_args[key].as_str() {
                                self.files.check(std::path::Path::new(path), true)?;
                            }
                        }
                        if step_name == "mobile_install_app" {
                            if let Some(path) = step_args["path"].as_str() {
                                self.files.check(std::path::Path::new(path), false)?;
                            }
                        }
                    }
                }
                if name == "mobile_batch_commands" {
                    let mut content = Vec::new();
                    let mut artifacts = Vec::new();
                    let mut failed = false;
                    let mut steps = args["steps"].as_array().unwrap().clone();
                    if args["listElementsAtEnd"] == true {
                        steps.push(json!({"name":"mobile_list_elements_on_screen","arguments":{}}));
                    }
                    for (index, step) in steps.iter().enumerate() {
                        if let Some(epoch) = epoch {
                            if let Err(e) = self.check_control_epoch(epoch).await {
                                failed = true;
                                content.push(json!({"type":"text","text":format!("Batch cancelled before step {}: {e}; prior effects retained",index+1)}));
                                break;
                            }
                        }
                        let mut step_args = step["arguments"].as_object().unwrap().clone();
                        step_args.insert("device".into(), json!(transport));
                        let mut inner = call.clone();
                        inner.arguments["message"]["params"] =
                            json!({"name":step["name"],"arguments":step_args});
                        let result = Box::pin(self.mcp(client, &inner, epoch)).await;
                        match result {
                            Ok(v) => {
                                let error =
                                    v.get("error").is_some() || v["result"]["isError"] == true;
                                content.push(json!({"type":"text","text":format!("Step {} ({}): {}",index+1,step["name"].as_str().unwrap_or(""),v["result"]["content"])}));
                                if let Some(items) = v["artifacts"].as_array() {
                                    artifacts.extend(items.clone());
                                }
                                failed |= error;
                                if error && args["stopOnError"] != false {
                                    break;
                                }
                            }
                            Err(e) => {
                                failed = true;
                                content.push(json!({"type":"text","text":format!("Step {} failed: {e}; prior effects retained",index+1)}));
                                if args["stopOnError"] != false
                                    || epoch
                                        .map(|_| {
                                            e.starts_with("CONTROL_STOPPED")
                                                || e == "UNAUTHORIZED"
                                                || e.starts_with("MODULE_UNAVAILABLE")
                                                || e.starts_with("PHONE_REMOVED")
                                        })
                                        .unwrap_or(false)
                                {
                                    break;
                                }
                            }
                        }
                    }
                    return Ok(
                        json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":content,"isError":failed},"artifacts":artifacts}),
                    );
                }
                self.modules
                    .phone_preflight(&physical_id, transport)
                    .await?;
                self.modules
                    .mobile_handover_check(&physical_id, transport)
                    .await?;
                if matches!(
                    name,
                    "mobile_start_screen_recording" | "mobile_stop_screen_recording"
                ) {
                    let key = physical_id.clone();
                    let recordings = self.recordings.lock().await;
                    if name == "mobile_start_screen_recording" && recordings.contains_key(&key) {
                        return Err(
                            "phone recording already exists; stop it before restarting".into()
                        );
                    }
                    if name == "mobile_stop_screen_recording" {
                        if let Some(recording) = recordings.get(&key) {
                            if recording.files.client != client.id
                                || recording.files.chat != call.chat_id
                            {
                                return Err("PERMISSION_DENIED: recording owner".into());
                            }
                            if !recording.valid(epoch.ok_or("control epoch required")?) {
                                return Err("CONTROL_STOPPED: recording owner epoch expired".into());
                            }
                            // The registry keeps ownership until native retrieval AND
                            // artifact/commit succeed. HTTP drop cannot orphan the native slot.
                            staged = Some(recording.files.clone());
                            recording_retrieved = recording.retrieved;
                            recording_key = Some(key);
                            stopping_recording = true;
                        }
                    } else {
                        recording_key = Some(key);
                    }
                }
                // Reject a foreign recording owner before acquire can hand over
                // (and terminate) that owner's native automation.
                control = Some(
                    self.acquire(
                        &client.id,
                        &call.chat_id,
                        "mobile",
                        &physical_id,
                        epoch.ok_or("control epoch required")?,
                    )
                    .await?,
                );
                controlled_transport = Some(physical_id.clone());
            }
        }
        if let Some(epoch) = epoch {
            if let Some(activity) = self
                .activities
                .lock()
                .await
                .iter_mut()
                .find(|a| a.id == epoch.operation_id)
            {
                activity.operation = message["params"]["name"]
                    .as_str()
                    .unwrap_or("tools/call")
                    .into();
            }
        }
        let timeout = call
            .arguments
            .get("requestTimeout")
            .filter(|v| !v.is_null())
            .map(|v| v.as_f64().ok_or("requestTimeout must be numeric"))
            .transpose()?
            .unwrap_or(300.0);
        self.store.active(&client.id)?;
        if control
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            return Err("CONTROL_STOPPED".into());
        }
        if module == "windows" {
            outgoing["_maxaOwner"] = json!(format!("{}:{}", client.id, call.chat_id));
            outgoing["_maxaEpoch"] = json!(epoch.map(|e| e.value).unwrap_or(0));
            outgoing["_maxaOperation"] =
                json!(epoch.map(|e| e.operation_id.as_str()).unwrap_or(""));
        }
        let received = if recording_retrieved {
            // A new explicit stop can retry local publication, never the phone action.
            Ok(
                json!({"jsonrpc":"2.0","id":message["id"],"result":{"isError":false,"content":[{"type":"text","text":"Recording already retrieved; publishing retained staging."}]}}),
            )
        } else if let Some(physical) = controlled_transport.as_deref() {
            self.modules
                .dispatch_phone(physical, &outgoing, timeout, control.as_ref().unwrap())
                .await
        } else {
            self.modules
                .dispatch_controlled(module, &outgoing, timeout, control.as_deref())
                .await
        };
        let outcome_unknown = received.is_err();
        if outcome_unknown {
            if let (Some(epoch), Some(token)) = (epoch, control.as_ref()) {
                self.release_failed_control(epoch, token).await;
            }
        }
        let mut result=received.unwrap_or_else(|e|json!({"jsonrpc":"2.0","id":message["id"],"result":{"isError":true,"content":[{"type":"text","text":format!("{e}; operation outcome may be unknown; do not replay")}]}}));
        let active = !control
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(false)
            && self.store.active(&client.id).is_ok();
        if let Some(files) = staged {
            if let Some(key) = recording_key {
                if stopping_recording {
                    let activities = self.activities.lock().await;
                    let still_owned = activities.iter().any(|l| {
                        l.module == "mobile"
                            && l.resource == key
                            && l.client_id == client.id
                            && l.chat_id == call.chat_id
                    });
                    let mut recordings = self.recordings.lock().await;
                    let retained = recordings
                        .get_mut(&key)
                        .filter(|r| Arc::ptr_eq(&r.files, &files));
                    let mut owner_lost = true;
                    if let Some(recording) = retained {
                        if active && still_owned && recording.valid(epoch.unwrap()) {
                            owner_lost = false;
                            if result.get("error").is_none() && result["result"]["isError"] != true
                            {
                                recording.retrieved = true;
                                self.finish_mcp_files(&files, client, &mut result, true);
                                if result.get("error").is_none()
                                    && result["result"]["isError"] != true
                                {
                                    recordings.remove(&key);
                                }
                            }
                        } else {
                            // The stop/revoke/disconnect path owns native cleanup.
                            recordings.remove(&key);
                        }
                    }
                    drop(recordings);
                    drop(activities);
                    if owner_lost {
                        result["result"]["isError"] = json!(true);
                        if !result["result"]["content"].is_array() {
                            result["result"]["content"] = json!([]);
                        }
                        result["result"]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":"CONTROL_STOPPED: recording ownership expired; no output committed."}));
                    }
                } else {
                    let replacements = files
                        .outputs
                        .iter()
                        .map(|o| {
                            (
                                o.stage.to_string_lossy().to_string(),
                                "remote recording pending; download after stop".into(),
                            )
                        })
                        .chain(files.private_outputs.iter().map(|p| {
                            (
                                p.to_string_lossy().to_string(),
                                "remote recording pending; download after stop".into(),
                            )
                        }))
                        .collect::<Vec<_>>();
                    replace_result_paths(&mut result, &replacements);
                    let activities = self.activities.lock().await;
                    let still_owned = activities.iter().any(|l| {
                        l.module == "mobile"
                            && l.resource == key
                            && l.client_id == client.id
                            && l.chat_id == call.chat_id
                    });
                    if active
                        && still_owned
                        && !control.as_ref().unwrap().load(Ordering::SeqCst)
                        && (outcome_unknown
                            || result.get("error").is_none() && result["result"]["isError"] != true)
                    {
                        let epoch = epoch.unwrap();
                        self.recordings.lock().await.insert(
                            key,
                            RecordingFiles {
                                files,
                                control: control.as_ref().unwrap().clone(),
                                epoch: epoch.value,
                                client_epoch: epoch.client.clone(),
                                retrieved: false,
                            },
                        );
                    }
                    drop(activities);
                }
            } else {
                self.finish_mcp_files(&files, client, &mut result, active);
            }
        }
        if method == "tools/list" {
            if let Some(tools) = result["result"]["tools"].as_array_mut() {
                tools.retain(|t| {
                    t["name"]
                        .as_str()
                        .map(|n| {
                            if module == "windows" {
                                windows_tool(n)
                            } else {
                                mobile_tool(n)
                            }
                        })
                        .unwrap_or(false)
                });
            }
            result["schemaRevision"] = json!(digest(
                serde_json::to_vec(&result["result"]["tools"])
                    .unwrap()
                    .as_slice()
            ));
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> (PathBuf, Arc<AppState>, ClientInfo) {
        let (root, app, client, _) = crate::lifecycle_tests::fixture().await;
        app.modules.ready_for_control_test("windows").await;
        app.modules.ready_for_control_test("mobile").await;
        (root, app, client)
    }
    #[tokio::test]
    async fn recording_staging_survives_failed_publication_until_owner_cleanup() {
        let (root, app, client) = fixture().await;
        let epoch = app.control_epoch("mobile", "phone1").await;
        let token = app
            .acquire(&client.id, "owner", "mobile", "phone1", &epoch)
            .await
            .unwrap();
        let output_dir = root.with_extension("outputs");
        std::fs::create_dir(&output_dir).unwrap();
        let output = output_dir.join("recording.mp4");
        let mut message = json!({"params":{"arguments":{"output":output}}});
        let files = Arc::new(
            app.stage_mcp(
                &client,
                "owner",
                "mobile_start_screen_recording",
                &mut message,
            )
            .unwrap(),
        );
        let stage = files.outputs[0].stage.clone();
        app.recordings.lock().await.insert(
            "phone1".into(),
            RecordingFiles {
                files: files.clone(),
                control: token.clone(),
                epoch: epoch.value,
                client_epoch: epoch.client.clone(),
                retrieved: true,
            },
        );
        std::fs::write(&stage, b"owned recording bytes").unwrap();
        // A real destination conflict makes publication fail after native retrieval.
        std::fs::write(&output, b"user changed destination").unwrap();
        let mut result = json!({"result":{"content":[],"isError":false}});
        app.finish_mcp_files(&files, &client, &mut result, true);
        assert_eq!(result["result"]["isError"], true);
        drop(files);
        let recordings = app.recordings.lock().await;
        let retained = &recordings["phone1"];
        assert!(retained.retrieved && retained.valid(&epoch) && stage.exists());
        let mut stale = epoch.clone();
        stale.value += 1;
        assert!(!retained.valid(&stale));
        drop(recordings);
        app.stop(Some(&client.id)).await;
        assert!(token.load(Ordering::SeqCst));
        assert!(!stage.exists());
        drop(app);
        std::fs::remove_dir_all(output_dir).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trusted_stop_invalidates_old_queue_but_new_request_can_continue() {
        let (root, app, client) = fixture().await;
        let epoch = app.control_epoch("windows", "desktop").await;
        let token = app
            .acquire(&client.id, "owner", "windows", "desktop", &epoch)
            .await
            .unwrap();
        app.activities.lock().await[0].started_at = 0;
        app.cleanup_pairing();
        let snapshot = app.snapshot().await;
        assert_eq!(snapshot.controls.len(), 1);
        assert!(serde_json::to_value(&snapshot.controls[0])
            .unwrap()
            .get("expiresAt")
            .is_none());
        app.stop_control(None, None).await.unwrap();
        assert!(token.load(Ordering::SeqCst));
        app.modules.ready("windows").await.unwrap();
        assert!(app.check_control_epoch(&epoch).await.is_err());
        let fresh = app.control_epoch("windows", "desktop").await;
        assert!(!app
            .acquire(&client.id, "owner", "windows", "desktop", &fresh)
            .await
            .unwrap()
            .load(Ordering::SeqCst));
        assert!(app.store.active(&client.id).is_ok());
        app.set_module("windows", false).await.unwrap();
        let disabled = app.control_epoch("windows", "desktop").await;
        assert!(app
            .acquire(&client.id, "owner", "windows", "desktop", &disabled)
            .await
            .is_err());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trusted_phone_stop_allows_new_work_but_removal_persists() {
        let (root, app, client) = fixture().await;
        let e1 = app.control_epoch("mobile", "phone1").await;
        let e2 = app.control_epoch("mobile", "phone2").await;
        let t1 = app
            .acquire(&client.id, "one", "mobile", "phone1", &e1)
            .await
            .unwrap();
        let t2 = app
            .acquire(&client.id, "two", "mobile", "phone2", &e2)
            .await
            .unwrap();
        app.release_failed_control(&e1, &t1).await;
        assert!(t1.load(Ordering::SeqCst));
        assert!(!t2.load(Ordering::SeqCst));
        app.check_control_epoch(&e2).await.unwrap();
        app.modules.ready("mobile").await.unwrap();
        let next = app.control_epoch("mobile", "phone1").await;
        let latest = app
            .acquire(&client.id, "one", "mobile", "phone1", &next)
            .await
            .unwrap();
        app.release_failed_control(&e1, &t1).await;
        assert!(!latest.load(Ordering::SeqCst));
        app.phones.lock().await.push(PhoneInfo {
            id: "phone1".into(),
            name: "test".into(),
            transports: vec![],
            active_transport: None,
            state: "offline".into(),
            reason: None,
            removed: false,
        });
        app.stop_control(None, Some("phone1")).await.unwrap();
        assert!(latest.load(Ordering::SeqCst));
        assert!(!t2.load(Ordering::SeqCst));
        let after_stop = app.control_epoch("mobile", "phone1").await;
        assert!(app
            .acquire(&client.id, "one", "mobile", "phone1", &after_stop)
            .await
            .is_ok());
        app.remove_phone("phone1").await.unwrap();
        assert!(app.phone_status().await[0].removed);
        assert!(!t2.load(Ordering::SeqCst));
        let removed_epoch = app.control_epoch("mobile", "phone1").await;
        assert!(app
            .acquire(&client.id, "one", "mobile", "phone1", &removed_epoch)
            .await
            .unwrap_err()
            .contains("PHONE_REMOVED"));
        assert!(app.check_control_epoch(&next).await.is_err());
        app.stop(None).await;
        drop(app);
        std::fs::write(root.join("paused-phones.json"), b"[\"phone1\"]").unwrap();
        let reopened = AppState::open(root.clone(), root.join("resources")).unwrap();
        assert!(reopened.phone_status().await[0].removed);
        assert!(!root.join("paused-phones.json").exists());
        assert!(serde_json::to_value(&reopened.phone_status().await[0])
            .unwrap()
            .get("controlBlocked")
            .is_none());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trusted_disconnect_cancels_only_the_started_operation_generation() {
        let (root, app, client) = fixture().await;
        let owner = app.control_epoch("mobile", "phone1").await;
        let token = app
            .acquire(&client.id, "owner", "mobile", "phone1", &owner)
            .await
            .unwrap();
        let waiting = app.control_epoch("mobile", "phone1").await;
        drop(InvocationGuard {
            app: app.clone(),
            client: client.id.clone(),
            chat: "other".into(),
            request: id(),
            epoch: Some(waiting),
            armed: true,
        });
        assert!(!token.load(Ordering::SeqCst));
        app.check_control_epoch(&owner).await.unwrap();
        drop(InvocationGuard {
            app: app.clone(),
            client: client.id.clone(),
            chat: "owner".into(),
            request: id(),
            epoch: Some(owner.clone()),
            armed: true,
        });
        assert!(token.load(Ordering::SeqCst));
        assert!(app.check_control_epoch(&owner).await.is_err());
        tokio::task::yield_now().await;
        let next = app.control_epoch("mobile", "phone1").await;
        assert!(!app
            .acquire(&client.id, "owner", "mobile", "phone1", &next)
            .await
            .unwrap()
            .load(Ordering::SeqCst));
        app.stop(None).await;
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trusted_client_stop_cancels_its_queued_generation_only() {
        let (root, app, client) = fixture().await;
        let call = Invocation {
            request_id: id(),
            chat_id: "owner".into(),
            operation: "mcp".into(),
            arguments: json!({"module":"windows","message":{"method":"tools/call"}}),
        };
        let active = app.control_epoch("windows", "desktop").await;
        let token = app
            .acquire(&client.id, "owner", "windows", "desktop", &active)
            .await
            .unwrap();
        let queued = app
            .queued_control(&client.id, &call)
            .await
            .unwrap()
            .unwrap();
        let other = app
            .queued_control("other-client", &call)
            .await
            .unwrap()
            .unwrap();
        app.stop_control(Some(&client.id), None).await.unwrap();
        assert!(token.load(Ordering::SeqCst));
        assert!(app.check_control_epoch(&queued).await.is_err());
        app.check_control_epoch(&other).await.unwrap();
        let fresh = app
            .queued_control(&client.id, &call)
            .await
            .unwrap()
            .unwrap();
        app.check_control_epoch(&fresh).await.unwrap();
        app.modules.ready("windows").await.unwrap();
        app.modules.ready("mobile").await.unwrap();
        assert!(!root.join("paused-phones.json").exists());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trusted_revoked_client_and_old_queue_are_rejected() {
        let (root, app, client) = fixture().await;
        let epoch = app.control_epoch("windows", "desktop").await;
        let token = app
            .acquire(&client.id, "owner", "windows", "desktop", &epoch)
            .await
            .unwrap();
        let barrier = app.mutation.lock().await;
        let request = Invocation {
            request_id: id(),
            chat_id: "owner".into(),
            operation: "mcp".into(),
            arguments: json!({"module":"windows","message":{"method":"tools/call","params":{"name":"Shortcut","arguments":{}}}}),
        };
        let a = app.clone();
        let c = client.clone();
        let queued = tokio::spawn(async move { a.invoke(&c, &request).await });
        tokio::task::yield_now().await;
        app.revoke(&client.id).await.unwrap();
        drop(barrier);
        assert_eq!(queued.await.unwrap().unwrap_err(), "UNAUTHORIZED");
        assert!(token.load(Ordering::SeqCst));
        assert!(app.store.active(&client.id).is_err());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
fn windows_tool(n: &str) -> bool {
    matches!(
        n,
        "Snapshot"
            | "Screenshot"
            | "Click"
            | "Type"
            | "Shortcut"
            | "Scroll"
            | "Drag"
            | "Move"
            | "Wait"
            | "App"
            | "Clipboard"
            | "ControlStatus"
            | "MultiSelect"
            | "MultiEdit"
            | "Window"
    )
}
fn mobile_tool(n: &str) -> bool {
    n.starts_with("mobile_")
        && !matches!(
            n,
            "mobile_login_to_cloud_provider"
                | "mobile_list_remote_devices"
                | "mobile_allocate_remote_device"
                | "mobile_release_remote_device"
        )
}
