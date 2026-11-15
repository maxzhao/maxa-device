#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(windows)]
mod desktop {
    use maxa_device::{model::*, state::AppState};
    use std::sync::Arc;
    use tauri::{Emitter, Manager};
    struct DesktopState {
        app: Arc<AppState>,
        shutdown: axum_server::Handle,
    }
    async fn snapshot(s: &DesktopState) -> DeviceSnapshot {
        s.app.snapshot().await
    }
    #[tauri::command]
    async fn device_snapshot(state: tauri::State<'_, DesktopState>) -> Result<DeviceSnapshot> {
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_module(
        state: tauri::State<'_, DesktopState>,
        module: String,
        enabled: bool,
    ) -> Result<DeviceSnapshot> {
        state.app.set_module(&module, enabled).await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_pair_decide(
        state: tauri::State<'_, DesktopState>,
        request_id: String,
        accept: bool,
    ) -> Result<DeviceSnapshot> {
        state.app.decide(&request_id, accept).await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_client_revoke(
        state: tauri::State<'_, DesktopState>,
        client_id: String,
    ) -> Result<DeviceSnapshot> {
        state.app.revoke(&client_id).await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_stop_control(
        state: tauri::State<'_, DesktopState>,
        client_id: Option<String>,
        phone_id: Option<String>,
    ) -> Result<DeviceSnapshot> {
        state
            .app
            .stop_control(client_id.as_deref(), phone_id.as_deref())
            .await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_phone_remove(
        state: tauri::State<'_, DesktopState>,
        phone_id: String,
    ) -> Result<DeviceSnapshot> {
        state.app.remove_phone(&phone_id).await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_phone_add(
        state: tauri::State<'_, DesktopState>,
        phone_id: String,
    ) -> Result<DeviceSnapshot> {
        state.app.add_phone(&phone_id).await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_phone_refresh(state: tauri::State<'_, DesktopState>) -> Result<DeviceSnapshot> {
        state.app.refresh_phones().await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_phone_pair(
        state: tauri::State<'_, DesktopState>,
        address: String,
        pair_code: String,
    ) -> Result<DeviceSnapshot> {
        state.app.modules.pair_phone(&address, &pair_code).await?;
        state.app.refresh_phones().await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_phone_connect(
        state: tauri::State<'_, DesktopState>,
        address: String,
    ) -> Result<DeviceSnapshot> {
        state.app.connect_phone(&address).await?;
        state.app.refresh_phones().await?;
        Ok(snapshot(&state).await)
    }
    #[tauri::command]
    async fn device_exit(
        app: tauri::AppHandle,
        state: tauri::State<'_, DesktopState>,
    ) -> Result<()> {
        state.app.shutdown().await;
        state
            .shutdown
            .graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        app.exit(0);
        Ok(())
    }
    pub fn run() {
        tauri::Builder::default()
            .setup(|app| {
                let data = app.path().app_local_data_dir()?;
                let resources = app.path().resource_dir()?;
                let backend = AppState::open(data, resources).map_err(std::io::Error::other)?;
                let mut windows_events = backend.modules.windows_events();
                let windows_state = backend.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match windows_events.recv().await {
                            Ok(event) => windows_state.windows_worker_event(&event).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let _ = windows_state.stop_control(None, None).await;
                            }
                            Err(_) => break,
                        }
                    }
                });
                let shutdown = axum_server::Handle::new();
                let address = std::env::var("MAXA_DEVICE_LISTEN")
                    .unwrap_or("0.0.0.0:19427".into())
                    .parse()
                    .map_err(std::io::Error::other)?;
                app.manage(DesktopState {
                    app: backend.clone(),
                    shutdown: shutdown.clone(),
                });
                let server = backend.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) =
                        maxa_device::http::serve(server.clone(), address, shutdown).await
                    {
                        let mut s = server.service.lock().await;
                        s.state = "failed".into();
                        s.error = Some(e);
                    }
                });
                let phones = backend.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        if phones.modules.ready("mobile").await.is_ok() {
                            let _ = phones.refresh_phones().await;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                });
                let emitter = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut last = String::new();
                    loop {
                        backend.cleanup_pairing();
                        backend.modules.health().await;
                        let value = backend.snapshot().await;
                        let encoded = serde_json::to_string(&value).unwrap_or_default();
                        if encoded != last {
                            let _ = emitter.emit("maxa-device:snapshot", &value);
                            last = encoded;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    }
                });
                let show = tauri::menu::MenuItem::with_id(
                    app,
                    "show",
                    "打开 Maxa Device",
                    true,
                    None::<&str>,
                )?;
                let quit = tauri::menu::MenuItem::with_id(
                    app,
                    "quit",
                    "退出并停止受管工作",
                    true,
                    None::<&str>,
                )?;
                let menu = tauri::menu::Menu::with_items(app, &[&show, &quit])?;
                let mut tray = tauri::tray::TrayIconBuilder::new()
                    .tooltip("Maxa Device")
                    .menu(&menu)
                    .on_menu_event(|app, event| {
                        if event.id().as_ref() == "show" {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        if event.id().as_ref() == "quit" {
                            let app = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let state = app.state::<DesktopState>();
                                state.app.shutdown().await;
                                state
                                    .shutdown
                                    .graceful_shutdown(Some(std::time::Duration::from_secs(5)));
                                app.exit(0);
                            });
                        }
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray = tray.icon(icon.clone());
                }
                tray.build(app)?;
                Ok(())
            })
            .on_window_event(|window, event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            })
            .invoke_handler(tauri::generate_handler![
                device_snapshot,
                device_module,
                device_pair_decide,
                device_client_revoke,
                device_stop_control,
                device_phone_refresh,
                device_phone_remove,
                device_phone_add,
                device_phone_pair,
                device_phone_connect,
                device_exit
            ])
            .run(tauri::generate_context!())
            .expect("Maxa Device startup failed");
    }
}
#[cfg(windows)]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--windows-worker") {
        if let Err(e) = maxa_device::windows::worker_main() {
            eprintln!("Windows worker: {e}");
            std::process::exit(1);
        }
        return;
    }
    desktop::run()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Maxa Device first release supports Windows x64 hosts only");
    std::process::exit(1)
}
