//! Termux-only management surface. Socket permissions plus peer UID, never remote approval.
use crate::{model::*, state::AppState};
use serde_json::{json, Value};
use std::{
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::PathBuf,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
fn data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| "HOME missing")?;
    #[cfg(target_os = "android")]
    if !matches!(
        home.as_str(),
        "/data/data/com.termux/files/home" | "/data/user/0/com.termux/files/home"
    ) {
        return Err("Refusing credentials outside Termux private home".into());
    }
    if !home.starts_with('/') {
        return Err("HOME must be absolute".into());
    }
    let root = PathBuf::from(home).join(".local/share/maxa-device");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut p = root.as_path();
    loop {
        if std::fs::symlink_metadata(p)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Private data path must not contain symlinks".into());
        }
        let Some(parent) = p.parent() else { break };
        p = parent;
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    Ok(root)
}
pub async fn run() -> Result<()> {
    // New SQLite WAL/log/socket files are private even during creation.
    unsafe {
        libc::umask(0o077);
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("serve") {
        #[cfg(not(target_os = "android"))]
        return Err("Headless host distribution supports Android Termux only".into());
        #[cfg(target_os = "android")]
        return serve(data_dir()?).await;
    }
    let req = command(&args)?;
    #[cfg(not(target_os = "android"))]
    if req["op"] == "api" {
        return Err("UNSUPPORTED: embedded API requires an Android Termux host".into());
    }
    let root = data_dir()?;
    let socket_running = UnixStream::connect(root.join("control.sock")).await.is_ok();
    if !socket_running && matches!(req["op"].as_str(), Some("link" | "status" | "rename")) {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("agent.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Agent starting; retry the explicit link command after startup".into());
        }
        if req["op"] == "status" {
            stdout_line(format_args!(
                "{}",
                json!({"running":false,"busy":false,"version":env!("CARGO_PKG_VERSION"),"credentialBoundary":crate::android::TRUST_NOTICE})
            ))?;
            return Ok(());
        }
        if req["op"] == "rename" {
            #[cfg(not(target_os = "android"))]
            return Err("Host rename is supported by the Android Termux host only".into());
            #[cfg(target_os = "android")]
            {
                let name = if req["reset"] == true {
                    None
                } else {
                    Some(req["name"].as_str().ok_or("name required")?)
                };
                crate::identity::save_display_name(&root, name)?;
                let companion = crate::android::Mobile::new(root.clone())
                    .status()
                    .await
                    .ok();
                stdout_line(format_args!(
                    "{}",
                    json!({"displayName":crate::identity::android_display_name(&root, companion.as_ref().and_then(|s|s["deviceName"].as_str()))?,"running":false})
                ))?;
                return Ok(());
            }
        }
        if let Ok(meta) = std::fs::symlink_metadata(root.join("control.sock")) {
            if !meta.file_type().is_socket() {
                return Err("Refusing non-socket control path".into());
            }
            std::fs::remove_file(root.join("control.sock")).map_err(|e| e.to_string())?;
        }
        let link = crate::companion::Link {
            port: req["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or("Invalid port")?,
            secret: req["secret"].as_str().ok_or("secret required")?.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        crate::companion::Client::new(link.clone())?
            .status()
            .await?;
        crate::identity::private_write(
            &root.join("companion.json"),
            &serde_json::to_vec(&link).unwrap(),
        )?;
        stdout_line(format_args!(
            "{}",
            json!({"linked":true,"credentialBoundary":crate::android::TRUST_NOTICE})
        ))?;
        return Ok(());
    }
    if req["op"] == "pair.approve" {
        let list = request(&root, &json!({"op":"status"})).await?;
        let selected = list["snapshot"]["pairing"]
            .as_array()
            .and_then(|v| v.iter().find(|p| p["id"] == req["id"]))
            .ok_or("Pair request not found or expired")?;
        stdout_line(format_args!(
            "Host: {}\nPair request: {}\n{}",
            list["snapshot"]["identity"],
            selected,
            crate::android::TRUST_NOTICE
        ))?;
        eprint!("Compare both endpoints. Type the verificationCode to approve (anything else cancels): ");
        use std::io::Write;
        std::io::stderr().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        if selected["verificationCode"].as_str() != Some(answer.trim()) {
            return Err("Pair approval cancelled".into());
        }
        let mut req = req;
        req["verificationCode"] = selected["verificationCode"].clone();
        stdout_line(format_args!("{}", request(&root, &req).await?))?;
        return Ok(());
    }
    stdout_line(format_args!("{}", request(&root, &req).await?))?;
    Ok(())
}
fn stdout_line(args: std::fmt::Arguments<'_>) -> Result<()> {
    use std::io::Write;
    match writeln!(std::io::stdout().lock(), "{args}") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(error) => Err(error.to_string()),
    }
}

fn command(a: &[String]) -> Result<Value> {
    let s = a.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(match s.as_slice() {
        ["status"]=>json!({"op":"status"}),
        ["rename","--reset"]=>json!({"op":"rename","reset":true}),
        ["rename",name]=>json!({"op":"rename","name":name}),
        ["pair","list"]=>json!({"op":"pair.list"}),
        ["pair","approve",id]=>json!({"op":"pair.approve","id":id}),
        ["pair","reject",id]=>json!({"op":"pair.reject","id":id}),
        ["clients","list"]=>json!({"op":"clients.list"}),
        ["clients","revoke",id]=>json!({"op":"clients.revoke","id":id}),
        ["stop"]=>json!({"op":"stop"}),
        ["module",action,module] if matches!(*action,"enable"|"disable") && matches!(*module,"system"|"mobile")=>json!({"op":"module","module":module,"enabled":*action=="enable"}),
        ["link","--port",port,"--secret",secret]=>json!({"op":"link","port":port.parse::<u16>().map_err(|_|"Invalid port")?,"secret":secret}),
        ["unlink"]=>json!({"op":"unlink"}),
        ["android","api",name]=>api_command(name, json!({}))?,
        ["android","api",name,"--json",args]=>api_command(name, serde_json::from_str(args).map_err(|_|"INVALID_ARGUMENT: --json must be a JSON object")?)?,
        ["android","api",..]=>return Err("INVALID_ARGUMENT: Usage: maxa-device android api <name> [--json <object>]".into()),
        ["android","usage"]=>json!({"op":"usage","sinceMs":crate::model::now().saturating_sub(86_400_000),"untilMs":crate::model::now()}),
        ["android","usage","--since",dur]=>json!({"op":"usage","sinceMs":crate::model::now().saturating_sub(duration(dur)?),"untilMs":crate::model::now()}),
        ["shutdown"]=>json!({"op":"shutdown"}),
        _=>return Err("Usage: maxa-device serve | status | rename NAME|--reset | pair list|approve ID|reject ID | clients list|revoke ID | stop | module enable|disable system|mobile | link --port P --secret S | unlink | android usage [--since 24h] | android api <name> [--json <object>] | shutdown".into()),
    })
}
fn api_command(name: &str, args: Value) -> Result<Value> {
    crate::android::validate_api(name, &args)?;
    Ok(json!({"op":"api","name":name,"args":args}))
}
fn duration(s: &str) -> Result<u64> {
    let n = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &s[n.len()..];
    let mul = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err("Duration unit: ms,s,m,h,d".into()),
    };
    n.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or("Invalid duration".into())
}
async fn request(root: &std::path::Path, req: &Value) -> Result<Value> {
    let mut socket = UnixStream::connect(root.join("control.sock"))
        .await
        .map_err(|_| "Agent not running; start maxa-device serve first")?;
    let mut bytes = serde_json::to_vec(req).unwrap();
    bytes.push(b'\n');
    socket.write_all(&bytes).await.map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    BufReader::new(socket)
        .take(1_048_577)
        .read_until(b'\n', &mut body)
        .await
        .map_err(|e| e.to_string())?;
    if body.len() > 1_048_576 {
        return Err("Control response exceeds limit".into());
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| "Invalid control response")?;
    if v["ok"] != true {
        return Err(v["error"].as_str().unwrap_or("Local command failed").into());
    }
    Ok(v["result"].clone())
}
fn busy(v: &crate::model::DeviceSnapshot) -> bool {
    v.jobs
        .iter()
        .any(|j| matches!(j.state.as_str(), "queued" | "running" | "cancel_requested"))
        || !v.controls.is_empty()
}
async fn manage(
    app: &Arc<AppState>,
    v: &Value,
    shutdown: &tokio::sync::watch::Sender<bool>,
) -> Result<Value> {
    match v["op"].as_str().ok_or("op required")? {
        "status" => {
            let snapshot = app.snapshot().await;
            let in_progress = busy(&snapshot)
                || app.transfers.in_progress()
                || app.modules.companion.recording_active().await;
            let companion = match app.modules.companion.status().await {
                Ok(v) => v,
                Err(e) => json!({"ready":false,"missing":e}),
            };
            Ok(
                json!({"running":true,"version":env!("CARGO_PKG_VERSION"),"busy":in_progress,"snapshot":snapshot,"environment":crate::environment::context(),"companion":companion,"credentialBoundary":crate::android::TRUST_NOTICE}),
            )
        }
        "rename" => {
            #[cfg(not(any(target_os = "android", test)))]
            return Err("Host rename is supported by the Android Termux host only".into());
            #[cfg(any(target_os = "android", test))]
            {
                let name = if v["reset"] == true {
                    None
                } else {
                    Some(v["name"].as_str().ok_or("name required")?)
                };
                Ok(json!(app.rename(name).await?))
            }
        }
        "pair.list" => {
            Ok(json!({"identity":app.public_identity(),"requests":app.pairing.requests()}))
        }
        "pair.approve" | "pair.reject" => {
            let id = v["id"].as_str().ok_or("id required")?;
            let accept = v["op"] == "pair.approve";
            if accept {
                let p = app
                    .pairing
                    .requests()
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or("Pair request expired")?;
                if v["verificationCode"] != p.verification_code {
                    return Err("Verification mismatch; no approval".into());
                }
            }
            app.decide(id, accept).await?;
            Ok(json!({"approved":accept}))
        }
        "clients.list" => Ok(json!(app.store.clients())),
        "clients.revoke" => {
            app.revoke(v["id"].as_str().ok_or("id required")?).await?;
            Ok(json!({"revoked":true}))
        }
        "stop" => {
            app.jobs.stop_client(None).await;
            app.transfers.cancel(None, &app.store);
            app.stop_control(None, None).await?;
            Ok(json!({"state":"stop_requested"}))
        }
        "module" => {
            let m = v["module"]
                .as_str()
                .filter(|m| matches!(*m, "system" | "mobile"))
                .ok_or("Invalid module")?;
            app.set_module(m, v["enabled"].as_bool().ok_or("enabled required")?)
                .await?;
            Ok(json!({"module":m,"enabled":v["enabled"]}))
        }
        "link" => {
            if busy(&app.snapshot().await) {
                return Err("BUSY: stop current operations before relinking".into());
            }
            let link = crate::companion::Link {
                port: v["port"]
                    .as_u64()
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or("Invalid port")?,
                secret: v["secret"].as_str().ok_or("secret required")?.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            };
            crate::companion::Client::new(link.clone())?
                .status()
                .await?;
            crate::identity::private_write(
                &app.data.join("companion.json"),
                &serde_json::to_vec(&link).unwrap(),
            )?;
            app.modules.companion.reset().await;
            if app.preferences.lock().await.modules["mobile"] {
                app.modules.set("mobile", false).await?;
                app.modules.set("mobile", true).await?;
            }
            Ok(
                json!({"linked":true,"version":link.version,"credentialBoundary":crate::android::TRUST_NOTICE}),
            )
        }
        "unlink" => {
            let _ = app.stop_control(None, None).await;
            let _ = app.modules.set("mobile", false).await;
            let revoked = app.modules.companion.revoke_link().await;
            match std::fs::remove_file(app.data.join("companion.json")) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.to_string()),
            }
            app.modules.companion.reset().await;
            Ok(
                json!({"linked":false,"revoked":revoked.is_ok(),"warning":revoked.err().map(|_|"APK revoke failed: unlink or regenerate in the companion wizard to invalidate its secret and sessions")}),
            )
        }
        "api" => {
            let name = v["name"]
                .as_str()
                .ok_or("INVALID_ARGUMENT: API name required")?;
            let args = v.get("args").ok_or("INVALID_ARGUMENT: API args required")?;
            crate::android::validate_api(name, args)?;
            app.android_api(name, args.clone()).await
        }
        "usage" => {
            app.modules
                .companion
                .usage(json!({"sinceMs":v["sinceMs"],"untilMs":v["untilMs"]}))
                .await
        }
        "shutdown" => {
            app.prepare_update_shutdown().await?;
            let _ = shutdown.send(true);
            Ok(json!({"shutdown":true}))
        }
        _ => Err("Unknown local command".into()),
    }
}
pub async fn serve(root: PathBuf) -> Result<()> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("agent.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Agent already running".into());
    }
    let path = root.join("control.sock");
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?
        }
        Ok(_) => return Err("Refusing non-socket control path".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let resources = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("Executable parent missing")?
        .to_path_buf();
    let app = AppState::open(root, resources)?;
    #[cfg(target_os = "android")]
    app.refresh_android_display_name().await?;
    app.restore_modules().await;
    let handle = axum_server::Handle::new();
    let addr = std::env::var("MAXA_DEVICE_LISTEN")
        .unwrap_or("0.0.0.0:19427".into())
        .parse()
        .map_err(|_| "Invalid MAXA_DEVICE_LISTEN")?;
    let server_app = app.clone();
    let server_handle = handle.clone();
    let mut server =
        tokio::spawn(async move { crate::http::serve(server_app, addr, server_handle).await });
    let (shutdown, mut stopped) = tokio::sync::watch::channel(false);
    let health = app.clone();
    let maintenance = tokio::spawn(async move {
        loop {
            health.cleanup_pairing();
            #[cfg(target_os = "android")]
            if let Err(error) = health.refresh_android_display_name().await {
                eprintln!("Host display name refresh: {error}");
            }
            health.modules.health().await;
            let selected = health.preferences.lock().await.modules.clone();
            for (module, enabled) in selected {
                if enabled && module != "windows" && health.modules.ready(&module).await.is_err() {
                    let _ = health.set_module(&module, true).await;
                }
            }
            if health.modules.ready("mobile").await.is_ok() {
                let _ = health.refresh_phones().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
    let result = loop {
        tokio::select! {
            result=&mut server=>break result.map_err(|e|e.to_string()).and_then(|v|v),
            _=tokio::signal::ctrl_c()=>break Ok(()),
            _=stopped.changed()=>break Ok(()),
            accepted=listener.accept()=>{
                let (mut socket,_)=accepted.map_err(|e|e.to_string())?;
                if socket.peer_cred().map_err(|e|e.to_string())?.uid()!=unsafe {libc::getuid()} {continue;}
                let state=app.clone();let shutdown=shutdown.clone();
                tokio::spawn(async move {
                    let call=async {
                        let mut body=Vec::new();BufReader::new(&mut socket).take(1_048_577).read_until(b'\n',&mut body).await.map_err(|e|e.to_string())?;
                        if body.len()>1_048_576 {return Err("Control request exceeds limit".into());}
                        let v=serde_json::from_slice(&body).map_err(|_|"Invalid local JSON")?;
                        manage(&state,&v,&shutdown).await
                    };
                    let response=match call.await {Ok(v)=>json!({"ok":true,"result":v}),Err(e)=>json!({"ok":false,"error":e})};
                    let mut bytes=serde_json::to_vec(&response).unwrap();bytes.push(b'\n');let _=socket.write_all(&bytes).await;
                });
            }
        }
    };
    maintenance.abort();
    app.shutdown().await;
    handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    if !server.is_finished() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(6), &mut server).await;
    }
    let _ = std::fs::remove_file(path);
    drop(lock);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const REMOVED_API_NAMES: [&str; 15] = [
        "wifi-enable",
        "wifi-scaninfo",
        "keystore",
        "telephony-deviceinfo",
        "media-scan",
        "download",
        "saf-managedir",
        "saf-dirs",
        "saf-ls",
        "saf-stat",
        "saf-read",
        "saf-create",
        "saf-write",
        "saf-mkdir",
        "saf-rm",
    ];
    #[test]
    fn embedded_api_cli_accepts_only_37_names_and_object_json() {
        let names = crate::android::API_NAMES;
        let expected = [
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
        assert_eq!(
            names, expected,
            "API names must exactly match the finalized specification"
        );
        assert_eq!(
            names
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            37
        );
        for name in names {
            let args = vec!["android".into(), "api".into(), name.into()];
            assert_eq!(
                command(&args).unwrap(),
                json!({"op":"api","name":name,"args":{}})
            );
            let args = vec![
                "android".into(),
                "api".into(),
                name.into(),
                "--json".into(),
                r#"{"text":"中文","nested":{"x":1}}"#.into(),
            ];
            assert_eq!(
                command(&args).unwrap()["args"],
                json!({"text":"中文","nested":{"x":1}})
            );
        }
        for name in REMOVED_API_NAMES.into_iter().chain([
            "termux-battery-status",
            "sms-inbox",
            "clipboard-get",
            "intent",
            "sh",
            "battery-status;id",
            "",
        ]) {
            assert!(command(&["android".into(), "api".into(), name.into()])
                .unwrap_err()
                .starts_with("INVALID_ARGUMENT:"));
        }
        for args in ["[]", "null", "true", "42", r#""text""#, "{", "{} {}"] {
            assert!(command(&[
                "android".into(),
                "api".into(),
                "battery-status".into(),
                "--json".into(),
                args.into()
            ])
            .unwrap_err()
            .starts_with("INVALID_ARGUMENT:"));
        }
        for args in [
            vec!["android", "api"],
            vec!["android", "api", "battery-status", "--json"],
            vec!["android", "api", "battery-status", "--shell", "id"],
            vec!["android", "api", "battery-status", "--json", "{}", "extra"],
        ] {
            assert!(
                command(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                    .unwrap_err()
                    .starts_with("INVALID_ARGUMENT:")
            );
        }
        assert_eq!(
            command(&["android".into(), "usage".into()]).unwrap()["op"],
            "usage"
        );
        assert_eq!(
            command(&[
                "android".into(),
                "usage".into(),
                "--since".into(),
                "1h".into()
            ])
            .unwrap()["op"],
            "usage"
        );
    }
    #[tokio::test]
    async fn embedded_api_rechecks_local_socket_input_and_shutdown_barrier() {
        let (root, app, _, _) = crate::lifecycle_tests::fixture().await;
        let (shutdown, _) = tokio::sync::watch::channel(false);
        for request in [
            json!({"op":"api","name":"intent","args":{}}),
            json!({"op":"api","name":"battery-status","args":[]}),
            json!({"op":"api","name":"battery-status"}),
        ] {
            assert!(manage(&app, &request, &shutdown)
                .await
                .unwrap_err()
                .starts_with("INVALID_ARGUMENT:"));
        }
        for name in REMOVED_API_NAMES {
            assert!(
                manage(&app, &json!({"op":"api","name":name,"args":{}}), &shutdown)
                    .await
                    .unwrap_err()
                    .starts_with("INVALID_ARGUMENT:"),
                "removed API {name} must be rejected before companion access"
            );
        }
        app.prepare_update_shutdown().await.unwrap();
        assert_eq!(
            manage(
                &app,
                &json!({"op":"api","name":"battery-status","args":{}}),
                &shutdown
            )
            .await
            .unwrap_err(),
            "HOST_SHUTTING_DOWN"
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn rename_cli_and_pair_list_use_current_display_name() {
        let args = vec!["rename".into(), "Local tablet".into()];
        let rename = command(&args).unwrap();
        let (root, app, _, _) = crate::lifecycle_tests::fixture().await;
        let (shutdown, _) = tokio::sync::watch::channel(false);
        assert_eq!(
            manage(&app, &rename, &shutdown).await.unwrap()["displayName"],
            "Local tablet"
        );
        assert_eq!(
            manage(&app, &json!({"op":"pair.list"}), &shutdown)
                .await
                .unwrap()["identity"]["displayName"],
            "Local tablet"
        );
        let reset = command(&["rename".into(), "--reset".into()]).unwrap();
        assert_eq!(reset["reset"], true);
        assert!(manage(&app, &reset, &shutdown).await.is_ok());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
