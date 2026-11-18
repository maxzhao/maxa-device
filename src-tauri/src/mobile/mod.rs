//! App-owned Android automation. No ambient ADB, Node, MCP listener or action retry.
pub(crate) mod hierarchy;
mod location;
mod orientation;
pub(crate) mod tools;
pub(crate) mod transport;
use crate::model::Result;
use adbutils::AdbDevice;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Mutex;
use transport::{cmd, shell, OwnedProcess};

pub struct Mobile {
    phones: Mutex<HashMap<String, Arc<Phone>>>,
}
struct Phone {
    dev: std::sync::Mutex<AdbDevice>,
    cancelled: AtomicBool,
    termination_error: std::sync::Mutex<Option<String>>,
    operation: Mutex<()>,
    helper: Mutex<Option<OwnedProcess>>,
    elements: Mutex<Vec<hierarchy::Element>>,
    generation: std::sync::atomic::AtomicU64,
    recording: Mutex<Option<Recording>>,
    location: Mutex<Option<location::Override>>,
    orientation: Mutex<Option<orientation::Override>>,
}
struct Recording {
    process: OwnedProcess,
    remote: String,
    output: PathBuf,
    started: std::time::Instant,
}
impl Default for Mobile {
    fn default() -> Self {
        Self {
            phones: Mutex::new(HashMap::new()),
        }
    }
}
impl Mobile {
    pub async fn call(
        &self,
        port: u16,
        jar: PathBuf,
        physical: &str,
        message: &Value,
        timeout: f64,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        if !timeout.is_finite() {
            return Err("request timeout must be finite".into());
        }
        let serial = message["params"]["arguments"]["device"]
            .as_str()
            .ok_or("device required")?;
        let phone = {
            let mut phones = self.phones.lock().await;
            phones
                .entry(physical.into())
                .or_insert_with(|| {
                    Arc::new(Phone {
                        dev: std::sync::Mutex::new(transport::client(port).device(serial)),
                        cancelled: AtomicBool::new(false),
                        termination_error: std::sync::Mutex::new(None),
                        operation: Mutex::new(()),
                        helper: Mutex::new(None),
                        elements: Mutex::new(Vec::new()),
                        generation: std::sync::atomic::AtomicU64::new(
                            u64::from_str_radix(
                                &crate::model::digest(crate::model::id().as_bytes())[..12],
                                16,
                            )
                            .unwrap(),
                        ),
                        recording: Mutex::new(None),
                        location: Mutex::new(None),
                        orientation: Mutex::new(None),
                    })
                })
                .clone()
        };
        let ctx = Context {
            phone: &phone,
            dev: transport::client(port).device(serial),
            jar,
            cancelled,
            helper_used: AtomicBool::new(false),
            deadline: if timeout > 0. {
                Some(tokio::time::Instant::now() + Duration::from_secs_f64(timeout))
            } else {
                None
            },
        };
        let _operation = ctx.run(phone.operation.lock()).await?;
        ctx.check()?;
        *phone.dev.lock().unwrap() = ctx.dev.clone();
        let output = tools::dispatch(
            &ctx,
            message["params"]["name"]
                .as_str()
                .ok_or("tool name required")?,
            &message["params"]["arguments"],
        )
        .await;
        match output {
            Ok(content) => Ok(
                json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":content,"isError":false}}),
            ),
            Err(e) if e.starts_with("OUTCOME_UNKNOWN") || e.starts_with("CONTROL_STOPPED") => {
                Err(e)
            }
            Err(e) => Ok(
                json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":[{"type":"text","text":e}],"isError":true}}),
            ),
        }
    }
    pub async fn handover(&self, port: u16, physical: &str, serial: &str) -> Result<()> {
        let dev = transport::client(port).device(serial);
        let processes = shell(&dev, "ps -A -o PID,ARGS".into()).await?;
        let phone = self.phones.lock().await.get(physical).cloned();
        let own = if let Some(p) = phone {
            // state.rs verified physical identity before handover. A changed wireless port
            // must also be usable for cleanup of a previously owned helper/recording.
            *p.dev.lock().unwrap() = dev.clone();
            let h = p.helper.lock().await;
            if let Some(h) = h.as_ref() {
                if h.matches(&dev).await {
                    Some(h.pid)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        for line in processes.lines() {
            let signature = [
                "UiDumpServer",
                "uiautomator",
                "com.mobilenext.devicekit",
                "DeviceServer",
                "atx-agent",
                transport::MAIN_CLASS,
            ]
            .iter()
            .any(|s| line.contains(s));
            if signature
                && line
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    != own
            {
                return Err("ANDROID_CONTROL_BUSY: external or unowned UiAutomation; explicit human handover required".into());
            }
        }
        Ok(())
    }
    pub async fn termination_error(&self) -> Option<String> {
        let errors: Vec<_> = self
            .phones
            .lock()
            .await
            .values()
            .filter_map(|p| p.termination_error.lock().unwrap().clone())
            .collect();
        if errors.is_empty() {
            None
        } else {
            Some(format!("TERMINATION_UNCONFIRMED: {}", errors.join("; ")))
        }
    }
    pub async fn stop(&self, physical: Option<&str>) -> Result<()> {
        let phones = {
            let map = self.phones.lock().await;
            map.iter()
                .filter(|(id, _)| physical.map(|p| p == id.as_str()).unwrap_or(true))
                .map(|(id, p)| {
                    p.cancelled.store(true, Ordering::SeqCst);
                    (id.clone(), p.clone())
                })
                .collect::<Vec<_>>()
        };
        for (id, p) in phones {
            let dev = p.dev.lock().unwrap().clone();
            let mut errors = vec![];
            if let Err(e) = location::clear(&p, &dev).await {
                errors.push(format!("phone {id} owned location override: {e}"));
            }
            {
                let mut slot = p.recording.lock().await;
                if let Some(r) = slot.as_ref() {
                    match r.process.terminate(&dev, 2).await {
                        Ok(()) => {
                            let _ = shell(&dev, cmd(&["rm", "-f", &r.remote])).await;
                            slot.take();
                        }
                        Err(e) => {
                            errors.push(format!("phone {id} owned recording: {e}"));
                        }
                    }
                }
            }
            {
                let mut slot = p.helper.lock().await;
                if let Some(h) = slot.as_ref() {
                    match h.terminate(&dev, 9).await {
                        Ok(()) => {
                            slot.take();
                        }
                        Err(e) => {
                            errors.push(format!("phone {id} owned helper: {e}"));
                        }
                    }
                }
            }
            if let Err(e) = orientation::clear(&p, &dev).await {
                errors.push(format!("phone {id} owned orientation override: {e}"));
            }
            *p.termination_error.lock().unwrap() = if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            };
            if errors.is_empty() {
                let mut map = self.phones.lock().await;
                if map.get(&id).map(|v| Arc::ptr_eq(v, &p)).unwrap_or(false) {
                    map.remove(&id);
                }
            }
            // An unconfirmed remote process keeps a cancelled tombstone. Never start a second helper.
        }
        match self.termination_error().await {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
struct Context<'a> {
    phone: &'a Arc<Phone>,
    dev: AdbDevice,
    jar: PathBuf,
    cancelled: &'a AtomicBool,
    deadline: Option<tokio::time::Instant>,
    helper_used: AtomicBool,
}
impl Context<'_> {
    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) || self.phone.cancelled.load(Ordering::SeqCst) {
            return Err("CONTROL_STOPPED: partial effects may have occurred; do not replay".into());
        }
        if self
            .deadline
            .map(|d| tokio::time::Instant::now() >= d)
            .unwrap_or(false)
        {
            return Err("OUTCOME_UNKNOWN: request timeout; do not replay".into());
        }
        Ok(())
    }
    async fn run<T>(&self, f: impl std::future::Future<Output = T>) -> Result<T> {
        self.check()?;
        tokio::pin!(f);
        loop {
            tokio::select! {
                result=&mut f=>return Ok(result),
                _=tokio::time::sleep(Duration::from_millis(25))=>self.check()?,
            }
        }
    }
    async fn shell(&self, args: &[&str]) -> Result<String> {
        self.run(shell(&self.dev, cmd(args))).await?
    }
    async fn ensure_helper(&self) -> Result<()> {
        self.check()?;
        let used = self.helper_used.swap(true, Ordering::SeqCst);
        {
            let mut helper = self.phone.helper.lock().await;
            if helper
                .as_ref()
                .map(|h| h.done.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                let old = helper.as_ref().unwrap();
                old.terminate(&self.dev, 9).await?;
                helper.take();
                if used {
                    return Err(
                        "OUTCOME_UNKNOWN: helper exited; no restart/replay of this request".into(),
                    );
                }
                // A later independent request may prepare a new helper; never replay an RPC.
            }
            if helper.is_none() {
                // Startup observes phone cancellation; register before the final external-cancel check.
                *helper = Some(
                    transport::launch_helper(&self.dev, &self.jar, &self.phone.cancelled).await?,
                );
            }
        }
        self.check()
    }
    async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        self.ensure_helper().await?;
        self.run(transport::rpc(&self.dev, method, args)).await?
    }
    async fn package_catalog(&self, package: Option<&str>) -> Result<Value> {
        self.ensure_helper().await?;
        let path = match package {
            Some(p) => format!("/maxa/packages?package={p}"),
            None => "/maxa/packages".into(),
        };
        let body = self
            .run(transport::http(&self.dev, "GET", &path, None))
            .await??;
        let v: Value =
            serde_json::from_slice(&body).map_err(|e| format!("invalid package catalog: {e}"))?;
        if let Some(e) = v.get("error") {
            return Err(format!("ANDROID_PACKAGE_ERROR: {e}"));
        }
        Ok(v)
    }
    async fn action(&self, method: &str, args: Value) -> Result<()> {
        if self.rpc(method, args).await? == false {
            return Err(format!(
                "UNSUPPORTED: Android {method} rejected input; inspect debugging/input permission"
            ));
        }
        Ok(())
    }
    async fn hierarchy(&self, publish: bool) -> Result<(Vec<hierarchy::Element>, Value)> {
        if publish {
            self.phone.elements.lock().await.clear();
        }
        let xml = self.rpc("dumpWindowHierarchy", json!([false, 50])).await?;
        let generation = self.phone.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (elements, raw) = hierarchy::parse(
            xml.as_str().ok_or("helper returned no hierarchy")?,
            generation,
        )?;
        if publish {
            *self.phone.elements.lock().await = elements.clone();
        }
        Ok((elements, raw))
    }
}
