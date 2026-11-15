//! Same-executable Windows worker. stdout is exclusively JSONL; no listener.
pub mod logic;
mod pipe;
pub use pipe::Client;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod control;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod ia2;
#[cfg(windows)]
mod indicator;
#[cfg(windows)]
mod input;
pub mod labels;
#[cfg(windows)]
mod snapshot;
#[cfg(windows)]
mod tools;
#[cfg(windows)]
mod tree;
#[cfg(windows)]
mod virtual_desktop;
#[cfg(windows)]
mod visibility;

#[cfg(windows)]
pub fn worker_main() -> Result<(), String> {
    unsafe {
        use windows_sys::Win32::UI::{HiDpi::*, WindowsAndMessaging::SetProcessDPIAware};
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
    }
    use serde_json::{json, Value};
    use std::{
        io::{BufRead, Write},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let emit: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |v| {
        let mut out = output.lock().unwrap();
        if serde_json::to_writer(&mut *out, &v).is_ok() {
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    });
    let coordinator = control::Coordinator::start(emit.clone())?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Value>(0);
    let busy = Arc::new(AtomicBool::new(false));
    let op_busy = busy.clone();
    let c = coordinator.clone();
    let send = emit.clone();
    std::thread::spawn(move || {
        let mut backend = tools::Backend::new(c.clone());
        for request in rx {
            let id = request["id"].clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                backend.call(
                    request["tool"].as_str().unwrap_or(""),
                    &request["arguments"],
                    request["owner"].as_str().unwrap_or("pipe"),
                )
            }));
            if !matches!(&result, Ok(Ok(_))) {
                c.end();
            }
            c.release_held();
            let response = match result {
                Ok(Ok(result)) => {
                    json!({"type":"result","id":id,"result":result})
                }
                Ok(Err(e)) => {
                    json!({"type":"result","id":id,"result":{"isError":true,"content":[{"type":"text","text":e}]}})
                }
                Err(_) => {
                    c.cancel(false);
                    json!({"type":"result","id":id,"error":"OUTCOME_UNKNOWN: Windows operation panicked; do not replay"})
                }
            };
            c.operation_finished();
            op_busy.store(false, Ordering::SeqCst);
            send(response);
        }
        c.cancel(false);
    });
    emit(json!({"type":"ready","protocol":1}));
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                emit(json!({"type":"error","error":format!("Invalid JSON: {e}")}));
                continue;
            }
        };
        match v["type"].as_str() {
            Some("cancel") => {
                if let Some(generation) = v["generation"].as_u64() {
                    coordinator.cancel_generation(generation);
                    emit(
                        json!({"type":"cancelled","generation":generation,"busy":busy.load(Ordering::SeqCst)}),
                    );
                }
            }
            Some("call") if v["tool"] == "ControlStatus" => {
                let args = if v["arguments"].is_null() {
                    json!({})
                } else {
                    v["arguments"].clone()
                };
                let result = if args
                    .as_object()
                    .is_some_and(|a| a.keys().all(|k| k == "release"))
                    && (args["release"].is_null() || args["release"].is_boolean())
                {
                    json!({"content":[{"type":"text","text":coordinator.status(v["owner"].as_str().unwrap_or("pipe"),args["release"]==true)}]})
                } else {
                    json!({"isError":true,"content":[{"type":"text","text":"Invalid ControlStatus arguments"}]})
                };
                emit(json!({"type":"result","id":v["id"],"result":result}));
            }
            Some("call") => {
                if busy.swap(true, Ordering::SeqCst) {
                    emit(
                        json!({"type":"result","id":v["id"],"error":"CONTROL_BUSY: Windows worker is busy"}),
                    );
                    continue;
                }
                // No worker-side queue. A second call is rejected, never retained for replay.
                if let Err(error) = coordinator.admit(&v) {
                    busy.store(false, Ordering::SeqCst);
                    emit(json!({"type":"result","id":v["id"],"error":error}));
                    continue;
                }
                coordinator.operation_started();
                if tx.send(v).is_err() {
                    break;
                }
            }
            Some("shutdown") => break,
            _ => emit(json!({"type":"error","error":"Unknown worker message type"})),
        }
    }
    coordinator.cancel(false);
    // EOF is an ownership boundary even if the COM thread cannot return.
    std::process::exit(0)
}
