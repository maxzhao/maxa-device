//! Termux native Mobile backend; no ADB, shell input, clipboard typing or action replay.
use crate::{
    companion::{Client, Link},
    mobile::{hierarchy, tools},
    model::*,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
pub const TRUST_NOTICE: &str = "All software installed or run in Termux shares its UID and is trusted with host identity, pairing credentials and enabled companion capabilities. Keep ~/.local/share/maxa-device private; never copy credentials into shared storage.";
pub fn property(name: &str) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        let name = std::ffi::CString::new(name).ok()?;
        let mut bytes = [0 as libc::c_char; 92];
        unsafe extern "C" {
            fn __system_property_get(
                name: *const libc::c_char,
                value: *mut libc::c_char,
            ) -> libc::c_int;
        }
        let n = unsafe { __system_property_get(name.as_ptr(), bytes.as_mut_ptr()) };
        if n > 0 {
            return Some(
                unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
                    .to_string_lossy()
                    .into(),
            );
        }
    }
    let _ = name;
    None
}
// Exact embedded API surface. Never accept arbitrary intents, shell commands,
// service names or legacy termux-* dispatch through this operation.
pub(crate) const API_NAMES: [&str; 37] = [
    "battery-status",
    "brightness",
    "volume",
    "audio-info",
    "vibrate",
    "torch",
    "wallpaper",
    "sensor",
    "location",
    "wifi-connectioninfo",
    "notification",
    "notification-channel",
    "notification-remove",
    "notification-list",
    "toast",
    "dialog",
    "fingerprint",
    "tts-engines",
    "tts-speak",
    "speech-to-text",
    "camera-info",
    "camera-photo",
    "microphone-record",
    "media-player",
    "share",
    "storage-get",
    "contact-list",
    "call-log",
    "sms-list",
    "sms-send",
    "telephony-call",
    "telephony-cellinfo",
    "infrared-frequencies",
    "infrared-transmit",
    "nfc",
    "usb",
    "job-scheduler",
];
pub(crate) fn validate_api(name: &str, args: &Value) -> Result<()> {
    if !API_NAMES.contains(&name) {
        return Err("INVALID_ARGUMENT: unknown embedded API name".into());
    }
    if !args.is_object() {
        return Err("INVALID_ARGUMENT: --json must be a JSON object".into());
    }
    Ok(())
}
pub fn shell() -> Result<String> {
    let prefix = std::env::var("PREFIX").map_err(|_| "Termux PREFIX missing; start from Termux")?;
    if !prefix.starts_with('/') {
        return Err("Termux PREFIX must be absolute".into());
    }
    let path = Path::new(&prefix).join("bin/sh");
    if !path.is_file() {
        return Err("Termux shell missing; pkg install dash".into());
    }
    Ok(path.to_string_lossy().into())
}
pub fn dependencies() -> Result<()> {
    shell()?;
    let found = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let path = dir.join("rg");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
    });
    if !found {
        return Err(
            "MODULE_UNAVAILABLE: ripgrep missing from Termux PATH; pkg install ripgrep".into(),
        );
    }
    Ok(())
}
pub fn path_arg(value: &str) -> Result<()> {
    if !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('\0')
        || value.contains("://")
        || Path::new(value)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("INVALID_ARGUMENT: absolute Android POSIX path required".into());
    }
    Ok(())
}
pub fn validate_tool(name: &str, args: &Value) -> Result<()> {
    if !crate::native_catalog::android_tool(name) {
        return Err(format!("UNSUPPORTED: Android tool {name}"));
    }
    if name != "mobile_list_available_devices" && args["device"] != "local" {
        return Err("INVALID_ARGUMENT: Android host device must be local".into());
    }
    if name == "mobile_launch_app" && args.get("locale").is_some() {
        return Err("UNSUPPORTED: launch locale; no launch sent".into());
    }
    if name == "mobile_press_button"
        && !matches!(
            args["button"].as_str(),
            Some(
                "HOME"
                    | "BACK"
                    | "APP_SWITCH"
                    | "ENTER"
                    | "BACKSPACE"
                    | "VOLUME_UP"
                    | "VOLUME_DOWN"
            )
        )
    {
        return Err("UNSUPPORTED: Android button; no input sent".into());
    }
    if matches!(name, "mobile_save_screenshot" | "mobile_take_screenshot") {
        if let Some(path) = args["saveTo"].as_str() {
            let ext = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
                return Err(
                    "UNSUPPORTED: screenshot output extension; no screenshot requested".into(),
                );
            }
        }
    }
    for key in ["saveTo", "path", "output"] {
        if let Some(s) = args[key].as_str() {
            path_arg(s)?;
        }
    }
    Ok(())
}
struct Recording {
    id: String,
    output: String,
}
pub struct Mobile {
    root: PathBuf,
    client: Mutex<Option<Arc<Client>>>,
    status_client: Mutex<Option<Arc<Client>>>,
    operation: Mutex<()>,
    elements: Mutex<Vec<hierarchy::Element>>,
    generation: AtomicU64,
    recording: Mutex<Option<Recording>>,
    pending: std::sync::Mutex<Option<(Arc<Client>, String)>>,
}
impl Mobile {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            client: Mutex::new(None),
            status_client: Mutex::new(None),
            operation: Mutex::new(()),
            elements: Mutex::new(Vec::new()),
            generation: AtomicU64::new(crate::model::now()),
            recording: Mutex::new(None),
            pending: std::sync::Mutex::new(None),
        }
    }
    async fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if slot.is_none() {
            *slot = Some(Arc::new(Client::new(Link::load(&self.root)?)?));
        }
        Ok(slot.as_ref().unwrap().clone())
    }
    pub async fn recording_active(&self) -> bool {
        self.recording.lock().await.is_some()
    }
    pub async fn reset(&self) {
        self.client.lock().await.take();
        self.status_client.lock().await.take();
        self.elements.lock().await.clear();
    }
    pub async fn status(&self) -> Result<Value> {
        let c = {
            let mut slot = self.status_client.lock().await;
            if slot.is_none() {
                *slot = Some(Arc::new(Client::new(Link::load(&self.root)?)?));
            }
            slot.as_ref().unwrap().clone()
        };
        c.status().await
    }
    pub async fn revoke_link(&self) -> Result<()> {
        self.client().await?.call("link.revoke", json!({})).await?;
        Ok(())
    }
    pub async fn usage(&self, args: Value) -> Result<Value> {
        self.client().await?.call("usage.query", args).await
    }
    pub(crate) async fn api(&self, name: &str, args: Value) -> Result<Value> {
        validate_api(name, &args)?;
        let client = self.client().await?;
        client.status().await?; // Same protocol/version and mutual-authentication barrier.
                                // Capability toggles, grants and confirmations belong to the companion.
                                // Unknown results are errors, never replayed. Only the transport's
                                // explicitly no-effect 401 may renew authentication before one resend.
        client
            .call("api.call", json!({"name":name,"args":args}))
            .await
    }
    pub async fn stop(&self) -> Result<()> {
        let pending = self.pending.lock().unwrap().clone();
        if let Some((c, id)) = pending {
            c.cancel(&id).await?;
        }
        if let Some(r) = self.recording.lock().await.take() {
            let c = self.client().await?;
            let out = c.call("record.stop", json!({"recordingId":r.id})).await?;
            if let Some(blob) = out["blobId"].as_str() {
                c.call("blob.delete", json!({"blobId":blob})).await?;
            }
        }
        self.elements.lock().await.clear();
        Ok(())
    }
    pub async fn call(
        &self,
        message: &Value,
        timeout: f64,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        let name = message["params"]["name"]
            .as_str()
            .ok_or("tool name required")?;
        let args = &message["params"]["arguments"];
        validate_tool(name, args)?;
        if !timeout.is_finite() {
            return Err("Invalid timeout".into());
        }
        let _lock = self.operation.lock().await;
        if cancelled.load(Ordering::SeqCst) {
            return Err("CONTROL_STOPPED: no call sent".into());
        }
        let client = self.client().await?;
        client.status().await?;
        let request = crate::model::id();
        *self.pending.lock().unwrap() = Some((client.clone(), request.clone()));
        let work = self.dispatch(&client, &request, name, args);
        tokio::pin!(work);
        let deadline = if timeout > 0. {
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs_f64(timeout))
        } else {
            None
        };
        let result = loop {
            tokio::select! {
                result=&mut work=>break result,
                _=tokio::time::sleep(std::time::Duration::from_millis(25))=>{
                    if cancelled.load(Ordering::SeqCst) || deadline.is_some_and(|d|tokio::time::Instant::now()>=d) {
                        let _=client.cancel(&request).await;
                        break Err("OUTCOME_UNKNOWN: companion call cancelled/timed out; no replay".into());
                    }
                }
            }
        };
        self.pending.lock().unwrap().take();
        match result {
            Ok(content) => Ok(
                json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":content,"isError":false}}),
            ),
            Err(e) if e.starts_with("OUTCOME_UNKNOWN") || e.starts_with("AUTH_FAILED") => Err(e),
            Err(e) => Ok(
                json!({"jsonrpc":"2.0","id":message["id"],"result":{"content":[{"type":"text","text":e}],"isError":true}}),
            ),
        }
    }
    async fn dispatch(&self, c: &Client, id: &str, name: &str, args: &Value) -> Result<Vec<Value>> {
        let (op, a) = match name {
            "mobile_list_apps" => ("apps.list", json!({})),
            "mobile_get_foreground_app" => ("ui.foreground", json!({})),
            "mobile_launch_app" => ("apps.launch", json!({"packageName":args["packageName"]})),
            "mobile_open_url" => ("url.open", json!({"url":args["url"]})),
            "mobile_get_screen_size" | "mobile_get_orientation" => ("ui.screen", json!({})),
            "mobile_set_orientation" => (
                "orientation.set",
                json!({"orientation":args["orientation"]}),
            ),
            "mobile_press_button" => ("ui.button", json!({"button":args["button"]})),
            "mobile_type_keys" => (
                "ui.type",
                json!({"text":args["text"],"submit":args["submit"].as_bool().unwrap_or(false)}),
            ),
            "mobile_list_elements_on_screen" => {
                self.elements.lock().await.clear();
                let v = c.call_id(id, "ui.hierarchy", json!({})).await?;
                let (elements, raw) = hierarchy::parse(
                    v["xml"].as_str().ok_or("Invalid companion hierarchy")?,
                    self.generation.fetch_add(1, Ordering::SeqCst),
                )?;
                let mode = args["format"].as_str().unwrap_or("text");
                if mode == "raw" {
                    return Ok(text(raw.to_string()));
                }
                let out = hierarchy::format(&elements, mode);
                *self.elements.lock().await = elements;
                return Ok(text(out));
            }
            "mobile_click_on_screen_at_coordinates"
            | "mobile_double_tap_on_screen"
            | "mobile_long_press_on_screen_at_coordinates" => {
                let (x, y) = hierarchy::resolve(&self.elements.lock().await, args)?;
                let op = match name {
                    "mobile_double_tap_on_screen" => "ui.double_tap",
                    "mobile_long_press_on_screen_at_coordinates" => "ui.long_press",
                    _ => "ui.tap",
                };
                let mut a = json!({"x":x,"y":y});
                if op == "ui.long_press" {
                    a["durationMs"] =
                        json!(args["duration"].as_f64().unwrap_or(500.).round() as u64);
                }
                let output = if op == "ui.long_press" {
                    tools::long_pressed(x, y, args["duration"].as_f64().unwrap_or(500.).round())
                } else {
                    tools::tapped(op == "ui.double_tap", x, y)
                };
                c.call_id(id, op, a).await?;
                return Ok(text(output));
            }
            "mobile_swipe_on_screen" => {
                let screen = c.call("ui.screen", json!({})).await?;
                let w = screen["width"].as_f64().ok_or("Invalid screen size")?;
                let h = screen["height"].as_f64().ok_or("Invalid screen size")?;
                let explicit = !args["x"].is_null();
                if explicit != !args["y"].is_null() {
                    return Err("INVALID_ARGUMENT: both x and y required".into());
                }
                let d = if explicit {
                    args["distance"].as_f64().unwrap_or(400.)
                } else {
                    400.
                };
                let (dx, dy) = match args["direction"].as_str() {
                    Some("up") => (0., -d),
                    Some("down") => (0., d),
                    Some("left") => (-d, 0.),
                    Some("right") => (d, 0.),
                    _ => return Err("INVALID_ARGUMENT: swipe direction".into()),
                };
                let x = args["x"].as_f64().unwrap_or(w / 2. - dx / 2.);
                let y = args["y"].as_f64().unwrap_or(h / 2. - dy / 2.);
                (
                    "ui.swipe",
                    json!({"x1":x.round(),"y1":y.round(),"x2":(x+dx).round(),"y2":(y+dy).round(),"durationMs":275}),
                )
            }
            "mobile_take_screenshot" | "mobile_save_screenshot" => {
                let v = c.call_id(id, "ui.screenshot", json!({})).await?;
                let bytes = download_bytes(c, &v, 64 * 1024 * 1024).await?;
                return tools::screenshot_bytes(name, args, &bytes, || Ok(()));
            }
            "mobile_clipboard" => {
                let before = c.call("clipboard.get", json!({})).await;
                if let Some(value) = args["text"].as_str() {
                    match before.as_ref() {
                        Ok(v) if v["text"].is_string() => (),
                        Err(e) if !e.starts_with("UNSUPPORTED:") => return Err(e.clone()),
                        _ => {
                            return Err("UNSUPPORTED: clipboard not readable; no write sent".into())
                        }
                    }
                    c.call_id(id, "clipboard.set", json!({"text":value}))
                        .await?;
                    if c.call("clipboard.get", json!({})).await?["text"] != value {
                        return Err("OUTCOME_UNKNOWN: clipboard read-back mismatch".into());
                    }
                    return Ok(text("Clipboard updated"));
                }
                return Ok(text(
                    before?["text"]
                        .as_str()
                        .ok_or("UNSUPPORTED: clipboard not readable")?,
                ));
            }
            "mobile_install_app" => {
                let path = args["path"].as_str().ok_or("path required")?;
                let _pin = crate::files::Files::pin(Path::new(path), false)?;
                let mut f = tokio::fs::File::open(path)
                    .await
                    .map_err(|e| e.to_string())?;
                let size = f.metadata().await.map_err(|e| e.to_string())?.len();
                let v = c.call("blob.create", json!({"size":size})).await?;
                let blob = v["blobId"].as_str().ok_or("Invalid upload blob")?;
                let mut buf = vec![0; 1_048_576];
                let mut offset = 0u64;
                let upload=async {
                    loop {let n=f.read(&mut buf).await.map_err(|e|e.to_string())?; if n==0 {break;}
                        let meta=json!({"requestId":id,"op":"blob.write","args":{"blobId":blob,"offset":offset}}).to_string();
                        let reply=c.raw(&buf[..n],Some(&meta)).await?;
                        crate::companion::result(serde_json::from_slice(&reply.bytes).map_err(|_|"Invalid upload result")?)?;
                        offset+=n as u64;
                    }
                    c.call_id(id,"apps.install",json!({"blobId":blob})).await
                }.await;
                let _ = c.call("blob.delete", json!({"blobId":blob})).await;
                return installed(upload?, tools::installed(path));
            }
            "mobile_uninstall_app" => {
                return installed(
                    c.call_id(
                        id,
                        "apps.uninstall",
                        json!({"packageName":args["packageName"]}),
                    )
                    .await?,
                    tools::uninstalled(tools::string(args, "packageName")?),
                );
            }
            "mobile_start_screen_recording" => {
                let mut slot = self.recording.lock().await;
                if slot.is_some() {
                    return Err("BUSY: recording active".into());
                }
                let output = args["output"].as_str().ok_or("output required")?;
                let mut a = json!({});
                if let Some(v) = args.get("timeLimit") {
                    a["timeLimitSec"] = v.clone();
                }
                let v = c.call_id(id, "record.start", a).await?;
                *slot = Some(Recording {
                    id: v["recordingId"]
                        .as_str()
                        .ok_or("Invalid recording id")?
                        .into(),
                    output: output.into(),
                });
                return Ok(text(tools::recording_started(output)));
            }
            "mobile_stop_screen_recording" => {
                let mut slot = self.recording.lock().await;
                let r = slot.as_ref().ok_or("NOT_FOUND: no owned recording")?;
                let v = c
                    .call_id(id, "record.stop", json!({"recordingId":r.id}))
                    .await?;
                let file = Path::new(&r.output);
                let _pin = crate::files::Files::pin(file, true)?;
                let mut out = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(file)
                    .await
                    .map_err(|e| e.to_string())?;
                download(c, &v, &mut out).await?;
                out.sync_all().await.map_err(|e| e.to_string())?;
                let answer = tools::recording_stopped(
                    &r.output,
                    v["size"].as_u64().ok_or("Invalid recording size")?,
                    v["durationMs"]
                        .as_u64()
                        .ok_or("Invalid recording duration")?
                        / 1000,
                );
                slot.take();
                return Ok(text(answer));
            }
            _ => return Err(format!("UNSUPPORTED: Android tool {name}")),
        };
        let v = c.call_id(id, op, a).await?;
        let out = match name {
            "mobile_get_screen_size" => tools::screen_size_text(
                v["width"].as_u64().ok_or("Invalid screen width")?,
                v["height"].as_u64().ok_or("Invalid screen height")?,
            ),
            "mobile_get_orientation" => tools::orientation_text(screen_orientation(&v)?),
            "mobile_launch_app" => tools::launched(tools::string(args, "packageName")?),
            "mobile_open_url" => tools::opened(tools::string(args, "url")?),
            "mobile_press_button" => tools::pressed(tools::string(args, "button")?),
            "mobile_swipe_on_screen" => tools::swiped(tools::string(args, "direction")?),
            "mobile_type_keys" => tools::typed(tools::string(args, "text")?),
            "mobile_set_orientation" => {
                tools::orientation_changed(tools::string(args, "orientation")?)
            }
            "mobile_list_apps" => format!(
                "Found these apps on device: {}",
                v["apps"]
                    .as_array()
                    .ok_or("Invalid app list")?
                    .iter()
                    .map(|a| format!(
                        "{} ({})",
                        a["name"].as_str().unwrap_or(""),
                        a["packageName"].as_str().unwrap_or("")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "mobile_get_foreground_app" => format!(
                "Foreground app: {}, activity: {}",
                v["package"].as_str().unwrap_or(""),
                v["activity"].as_str().unwrap_or("")
            ),
            _ => return Err(format!("UNSUPPORTED: Android tool {name}")),
        };
        Ok(text(out))
    }
}
fn screen_orientation(screen: &Value) -> Result<&'static str> {
    let width = screen["width"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("Invalid screen width")?;
    let height = screen["height"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("Invalid screen height")?;
    Ok(if width > height {
        "landscape"
    } else {
        "portrait"
    })
}
#[cfg(test)]
#[test]
fn orientation_uses_current_dimensions_not_rotation_parity() {
    for rotation in 0..4 {
        assert_eq!(
            screen_orientation(&json!({"width":3200,"height":2136,"rotation":rotation})).unwrap(),
            "landscape"
        );
        assert_eq!(
            screen_orientation(&json!({"width":2136,"height":3200,"rotation":rotation})).unwrap(),
            "portrait"
        );
    }
    assert!(screen_orientation(&json!({"width":0,"height":3200,"rotation":0})).is_err());
}

fn text(s: impl Into<String>) -> Vec<Value> {
    vec![json!({"type":"text","text":s.into()})]
}
fn installed(v: Value, success: String) -> Result<Vec<Value>> {
    match v["status"].as_str() {
        Some("installed") => Ok(text(success)),
        Some("rejected") => Err(format!("PERMISSION_MISSING: {}", v["message"])),
        _ => Err(format!("OUTCOME_UNKNOWN: {}", v["message"])),
    }
}
async fn download_bytes(c: &Client, v: &Value, max: u64) -> Result<Vec<u8>> {
    let size = v["size"].as_u64().ok_or("Invalid blob size")?;
    if size > max {
        return Err("Blob exceeds limit".into());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    download(c, v, &mut bytes).await?;
    Ok(bytes)
}
async fn download(
    c: &Client,
    v: &Value,
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<()> {
    let blob = v["blobId"].as_str().ok_or("Invalid blob id")?;
    let size = v["size"]
        .as_u64()
        .filter(|s| *s > 0 && *s <= 512 * 1024 * 1024)
        .ok_or("Invalid blob size")?;
    let mut offset = 0;
    loop {
        let r=c.raw(&serde_json::to_vec(&json!({"requestId":crate::model::id(),"op":"blob.read","args":{"blobId":blob,"offset":offset,"length":1_048_576}})).unwrap(),None).await?;
        let meta = match r.meta {
            Some(meta) => meta,
            None => {
                crate::companion::result(
                    serde_json::from_slice(&r.bytes).map_err(|_| "Invalid blob response")?,
                )?;
                return Err("Invalid binary blob metadata".into());
            }
        };
        crate::companion::result(meta.clone())?;
        offset += r.bytes.len() as u64;
        if offset > size || r.bytes.is_empty() && offset < size {
            return Err("OUTCOME_UNKNOWN: incomplete blob".into());
        }
        out.write_all(&r.bytes).await.map_err(|e| e.to_string())?;
        if meta["result"]["eof"] == true {
            if offset != size {
                return Err("OUTCOME_UNKNOWN: blob size mismatch".into());
            }
            break;
        }
    }
    // EOF is authenticated. Explicit deletion is idempotent on a conforming companion.
    let _ = c.call("blob.delete", json!({"blobId":blob})).await;
    Ok(())
}
