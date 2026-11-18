#![cfg_attr(all(windows, feature = "desktop", not(debug_assertions)), windows_subsystem = "windows")]
#[cfg(all(windows, feature = "desktop"))]
mod desktop {
    use maxa_device::{model::*, state::AppState};
    use std::sync::Arc;
    use tauri::{Emitter, Manager};
    // Kernel objects are scoped to this user's App data path and Windows session.
    // The worker returns before entering desktop::run and never claims this GUI instance.
    struct Instance {
        wake: std::os::windows::io::OwnedHandle,
        _mutex: std::os::windows::io::OwnedHandle,
    }
    impl Instance {
        fn claim(data: &std::path::Path) -> std::io::Result<Option<Self>> {
            use std::os::windows::io::{AsRawHandle, FromRawHandle};
            use windows_sys::Win32::{Foundation::*, System::Threading::*};
            let name = format!(
                "Local\\MaxaDevice-{}",
                digest(data.to_string_lossy().as_bytes())
            );
            let wide = |s: String| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            unsafe {
                let event = CreateEventW(
                    std::ptr::null(),
                    0,
                    0,
                    wide(format!("{name}-open")).as_ptr(),
                );
                if event.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let wake = std::os::windows::io::OwnedHandle::from_raw_handle(event);
                let mutex = CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr());
                if mutex.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let existing = GetLastError() == ERROR_ALREADY_EXISTS;
                let mutex = std::os::windows::io::OwnedHandle::from_raw_handle(mutex);
                if existing {
                    if SetEvent(wake.as_raw_handle()) == 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    return Ok(None);
                }
                Ok(Some(Self {
                    wake,
                    _mutex: mutex,
                }))
            }
        }
        fn watch(self, app: tauri::AppHandle) {
            use std::os::windows::io::AsRawHandle;
            std::thread::spawn(move || {
                let instance = self;
                while unsafe {
                    windows_sys::Win32::System::Threading::WaitForSingleObject(
                        instance.wake.as_raw_handle(),
                        u32::MAX,
                    )
                } == 0
                {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || open_main(&handle, false));
                }
            });
        }
    }
    struct DesktopState {
        app: Arc<AppState>,
        shutdown: axum_server::Handle,
        tray_ready: std::sync::atomic::AtomicBool,
    }
    struct NativeMenus {
        show: tauri::menu::MenuItem<tauri::Wry>,
        settings: tauri::menu::MenuItem<tauri::Wry>,
        quit: tauri::menu::MenuItem<tauri::Wry>,
    }
    fn open_main(app: &tauri::AppHandle, settings: bool) {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
            if settings {
                let _ = app.emit("maxa-device:open-settings", ());
            }
        }
    }
    async fn apply_appearance(
        app: &tauri::AppHandle,
        value: &maxa_device::preferences::Appearance,
    ) -> Result<()> {
        use maxa_device::preferences::{native_text, Theme};
        if let Some(w) = app.get_webview_window("main") {
            w.set_theme(match value.theme {
                Theme::System => None,
                Theme::Light => Some(tauri::Theme::Light),
                Theme::Dark => Some(tauri::Theme::Dark),
            })
            .map_err(|e| e.to_string())?;
        }
        let chinese = value.language.chinese();
        if let Some(menus) = app.try_state::<NativeMenus>() {
            menus
                .show
                .set_text(native_text(chinese, "打开 Maxa Device"))
                .map_err(|e| e.to_string())?;
            menus
                .settings
                .set_text(native_text(chinese, "设置"))
                .map_err(|e| e.to_string())?;
            menus
                .quit
                .set_text(native_text(chinese, "退出并停止受管工作"))
                .map_err(|e| e.to_string())?;
        }
        app.state::<DesktopState>()
            .app
            .modules
            .set_language(chinese)
            .await?;
        Ok(())
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Settings {
        appearance: maxa_device::preferences::Appearance,
        autostart: bool,
        copy_symlinks: bool,
        copy_symlinks_error: Option<String>,
    }
    async fn settings_value(state: &DesktopState) -> Result<Settings> {
        let (copy_symlinks, copy_symlinks_error) = state.app.copy_symlinks_status().await;
        Ok(Settings {
            appearance: state.app.preferences.lock().await.appearance.clone(),
            autostart: maxa_device::preferences::autostart::enabled()?,
            copy_symlinks,
            copy_symlinks_error,
        })
    }
    #[tauri::command]
    async fn device_settings(state: tauri::State<'_, DesktopState>) -> Result<Settings> {
        settings_value(&state).await
    }
    #[tauri::command]
    async fn device_settings_save(
        app: tauri::AppHandle,
        state: tauri::State<'_, DesktopState>,
        appearance: maxa_device::preferences::Appearance,
    ) -> Result<Settings> {
        state.app.set_appearance(appearance.clone()).await?;
        apply_appearance(&app, &appearance).await?;
        settings_value(&state).await
    }
    #[tauri::command]
    async fn device_autostart(
        state: tauri::State<'_, DesktopState>,
        enabled: bool,
    ) -> Result<Settings> {
        maxa_device::preferences::autostart::set(enabled)?;
        settings_value(&state).await
    }
    #[tauri::command]
    async fn device_copy_symlinks(
        state: tauri::State<'_, DesktopState>,
        enabled: bool,
    ) -> Result<Settings> {
        state.app.set_copy_symlinks(enabled).await?;
        settings_value(&state).await
    }
    #[tauri::command]
    fn device_developer_settings() -> Result<()> {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let result = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                wide("ms-settings:developers").as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if result as isize <= 32 {
            return Err("Cannot open Windows developer settings".into());
        }
        Ok(())
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
                let Some(instance) = Instance::claim(&data)? else {
                    std::process::exit(0)
                };
                let resources = app.path().resource_dir()?;
                let backend = AppState::open(data, resources).map_err(std::io::Error::other)?;
                let appearance = backend
                    .preferences
                    .try_lock()
                    .map_err(std::io::Error::other)?
                    .appearance
                    .clone();
                backend.modules.chinese.store(
                    appearance.language.chinese(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                let restore = backend.clone();
                tauri::async_runtime::spawn(async move {
                    restore.restore_modules().await;
                });
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
                    tray_ready: std::sync::atomic::AtomicBool::new(false),
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
                    maxa_device::preferences::native_text(
                        appearance.language.chinese(),
                        "打开 Maxa Device",
                    ),
                    true,
                    None::<&str>,
                )?;
                let quit = tauri::menu::MenuItem::with_id(
                    app,
                    "quit",
                    maxa_device::preferences::native_text(
                        appearance.language.chinese(),
                        "退出并停止受管工作",
                    ),
                    true,
                    None::<&str>,
                )?;
                let settings = tauri::menu::MenuItem::with_id(
                    app,
                    "settings",
                    maxa_device::preferences::native_text(appearance.language.chinese(), "设置"),
                    true,
                    None::<&str>,
                )?;
                let menu = tauri::menu::Menu::with_items(app, &[&show, &settings, &quit])?;
                app.manage(NativeMenus {
                    show,
                    settings,
                    quit,
                });
                let mut tray = tauri::tray::TrayIconBuilder::new()
                    .tooltip("Maxa Device")
                    .menu(&menu)
                    .on_menu_event(|app, event| {
                        if event.id().as_ref() == "show" {
                            open_main(app, false);
                        }
                        if event.id().as_ref() == "settings" {
                            open_main(app, true);
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
                instance.watch(app.handle().clone());
                let tray_ready = tray.build(app).is_ok();
                app.state::<DesktopState>()
                    .tray_ready
                    .store(tray_ready, std::sync::atomic::Ordering::SeqCst);
                if let Some(w) = app.get_webview_window("main") {
                    use maxa_device::preferences::Theme;
                    w.set_theme(match appearance.theme {
                        Theme::System => None,
                        Theme::Light => Some(tauri::Theme::Light),
                        Theme::Dark => Some(tauri::Theme::Dark),
                    })?;
                    if !appearance.start_minimized || !tray_ready {
                        w.show()?;
                    }
                }
                Ok(())
            })
            .on_window_event(|window, event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if window
                        .app_handle()
                        .state::<DesktopState>()
                        .tray_ready
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        let _ = window.hide();
                    }
                }
            })
            .invoke_handler(tauri::generate_handler![
                device_settings,
                device_settings_save,
                device_autostart,
                device_copy_symlinks,
                device_developer_settings,
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
    #[cfg(feature = "desktop")]
    desktop::run()
}
#[cfg(unix)]
#[tokio::main]
async fn main() {
    if let Err(error) = maxa_device::headless::run().await {
        if std::env::args().nth(1).as_deref() == Some("android") && std::env::args().nth(2).as_deref() == Some("api") {
            let (code, message) = error.split_once(": ").unwrap_or(("ERROR", error.as_str()));
            eprintln!("{}", serde_json::json!({"ok":false,"error":{"code":code,"message":message}}));
        } else {
            eprintln!("{error}");
        }
        std::process::exit(1);
    }
}
