use crate::model::Result;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{broadcast, oneshot, Mutex, Notify},
};

type Pending = std::collections::HashMap<u64, oneshot::Sender<Result<Value>>>;
pub struct Client {
    pub instance: String,
    input: Mutex<ChildStdin>,
    pending: Mutex<Pending>,
    serial: Mutex<()>,
    next: AtomicU64,
    epoch: AtomicU64,
    generation: AtomicU64,
    alive: AtomicBool,
    ready: AtomicBool,
    changed: Notify,
    events: broadcast::Sender<Value>,
}
impl Client {
    pub fn attach(
        input: ChildStdin,
        output: ChildStdout,
        events: broadcast::Sender<Value>,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            instance: crate::model::id(),
            input: Mutex::new(input),
            pending: Mutex::new(Pending::new()),
            serial: Mutex::new(()),
            next: AtomicU64::new(1),
            epoch: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            ready: AtomicBool::new(false),
            changed: Notify::new(),
            events,
        });
        let c = client.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                match v["type"].as_str() {
                    Some("ready") if v["protocol"] == 1 => {
                        c.ready.store(true, Ordering::SeqCst);
                        c.changed.notify_waiters();
                    }
                    Some("result") => {
                        let mut pending = c.pending.lock().await;
                        if let Some(tx) = v["id"].as_u64().and_then(|id| pending.remove(&id)) {
                            let _ = tx.send(if let Some(e) = v["error"].as_str() {
                                Err(e.into())
                            } else {
                                Ok(v["result"].clone())
                            });
                        }
                        c.changed.notify_waiters();
                    }
                    Some("event") => {
                        let mut v = v;
                        v["instance"] = json!(c.instance);
                        let _ = c.events.send(v);
                    }
                    _ => {}
                }
            }
            c.alive.store(false, Ordering::SeqCst);
            for (_, tx) in c.pending.lock().await.drain() {
                let _ = tx.send(Err(
                    "OUTCOME_UNKNOWN: Windows worker exited; do not replay".into()
                ));
            }
            c.changed.notify_waiters();
            let _ = c
                .events
                .send(json!({"type":"event","event":"worker-exit","instance":c.instance,"epoch":c.epoch.load(Ordering::SeqCst),"generation":c.generation.load(Ordering::SeqCst)}));
        });
        client
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    pub fn event_current(&self, event: &Value) -> bool {
        event["instance"].as_str() == Some(self.instance.as_str())
            && event["generation"]
                .as_u64()
                .is_some_and(|g| g <= self.generation.load(Ordering::SeqCst))
    }
    // The input mutex is also the admission lock: check, registration and write
    // are indivisible relative to cancel. Never wait for a result while holding it.
    async fn write(input: &mut ChildStdin, v: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        input
            .write_all(&bytes)
            .await
            .map_err(|e| format!("OUTCOME_UNKNOWN: Windows pipe write failed: {e}; do not replay"))
    }
    pub async fn wait_ready(&self) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let changed = self.changed.notified();
                if !self.alive() {
                    return Err("Windows worker exited during startup".into());
                }
                if self.ready.load(Ordering::SeqCst) {
                    return Ok(());
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| "Windows worker startup timed out".to_string())?
    }
    async fn cancel_generation(&self, generation: u64) -> Result<()> {
        let mut input = self.input.lock().await;
        Self::write(&mut input, json!({"type":"cancel","generation":generation})).await
    }
    /// Cancel only work admitted before this stop, draining it for at most five seconds.
    pub async fn cancel(&self) -> bool {
        if !self.alive() {
            return true;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let generation = {
                let mut input = self.input.lock().await;
                let generation = self.generation.load(Ordering::SeqCst);
                Self::write(&mut input, json!({"type":"cancel","generation":generation})).await?;
                generation
            };
            loop {
                let changed = self.changed.notified();
                if !self.pending.lock().await.keys().any(|id| *id <= generation) {
                    break;
                }
                changed.await;
            }
            Ok::<_, String>(())
        })
        .await
        .is_ok_and(|r| r.is_ok())
    }
    pub async fn call(
        &self,
        message: &Value,
        timeout: f64,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Value> {
        if !timeout.is_finite() {
            return Err("requestTimeout must be finite".into());
        }
        let _serial = if message["params"]["name"] == "ControlStatus" {
            None
        } else {
            Some(
                self.serial
                    .try_lock()
                    .map_err(|_| "CONTROL_BUSY: Windows worker is busy")?,
            )
        };
        if !self.alive() {
            return Err("Windows worker is unavailable".into());
        }
        let status = message["params"]["name"] == "ControlStatus";
        let (id, mut rx) = {
            let mut input = self.input.lock().await;
            if cancelled.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err("CONTROL_STOPPED before Windows pipe admission".into());
            }
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            if !status {
                self.epoch.store(
                    message["_maxaEpoch"].as_u64().unwrap_or(0),
                    Ordering::SeqCst,
                );
                self.generation.store(id, Ordering::SeqCst);
            }
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(id, tx);
            if let Err(e) = Self::write(&mut input, json!({"type":"call","id":id,"generation":id,"tool":message["params"]["name"],"arguments":message["params"]["arguments"],"owner":message["_maxaOwner"],"epoch":message["_maxaEpoch"],"operation":message["_maxaOperation"]})).await {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }
            (id, rx)
        };
        let start = std::time::Instant::now();
        loop {
            tokio::select! {
                result=&mut rx => {return result.map_err(|_|"OUTCOME_UNKNOWN: Windows response channel closed".to_string())?.map(|result|json!({"jsonrpc":"2.0","id":message["id"],"result":result}));},
                _=tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    if cancelled.is_some_and(|c|c.load(Ordering::SeqCst)) || (timeout>0.0 && start.elapsed().as_secs_f64()>=timeout) {
                        if status {
                            self.pending.lock().await.remove(&id);
                            return Err("CONTROL_STOPPED: ControlStatus response interrupted".into());
                        }
                        return tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            self.cancel_generation(id).await?;
                            rx.await.map_err(|_|"OUTCOME_UNKNOWN: Windows response channel closed".to_string())?
                        }).await
                            .map_err(|_|"WINDOWS_WORKER_STUCK: OUTCOME_UNKNOWN after cancellation; do not replay".to_string())?
                            .map(|_|json!({"jsonrpc":"2.0","id":message["id"],"result":{"isError":true,"content":[{"type":"text","text":"CONTROL_STOPPED: operation interrupted; partial effects may already have occurred; do not replay"}]}}));
                    }
                }
            }
        }
    }
}
