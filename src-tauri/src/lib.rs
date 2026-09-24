mod auth;
mod config;
mod logger;
mod model;
mod paths;
mod portal;
mod probe;
mod runtime;
mod secure_store;
mod startup;

use model::{AppConfig, CaptureResult, ConfigView, Credentials, StatusView};
use runtime::RuntimeHandle;
use serde::Serialize;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

#[derive(Default)]
struct CaptureLifecycle(tauri::async_runtime::Mutex<Option<CaptureSession>>);

struct CaptureSession {
    closing: Arc<AtomicBool>,
    closed: tauri::async_runtime::Receiver<()>,
}

static MAIN_WINDOW_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[tauri::command]
fn get_status(runtime: State<'_, RuntimeHandle>) -> StatusView {
    runtime.status_view()
}

#[tauri::command]
fn get_config() -> Result<ConfigView, String> {
    config::read_view()
}

#[tauri::command]
fn save_setup(
    app: AppHandle,
    runtime: State<'_, RuntimeHandle>,
    config_value: AppConfig,
    credentials: Credentials,
) -> Result<ConfigView, String> {
    if config_value.apartment_id.trim().is_empty() || config_value.room_id.trim().is_empty() {
        return Err("请填写楼栋/公寓 ID 和房间 ID，也可以先使用自动获取".into());
    }
    config::save_config(&config_value)?;
    config::save_credentials(&credentials)?;
    runtime.set_configured();
    let _ = app.emit("status-changed", runtime.status_view());
    config::read_view()
}

/// Save non-sensitive settings without requiring the user to re-enter the
/// credentials. This is used when an existing user only changes the carrier
/// selection (or another account configuration field).
#[tauri::command]
fn save_config(
    app: AppHandle,
    runtime: State<'_, RuntimeHandle>,
    config_value: AppConfig,
) -> Result<ConfigView, String> {
    if config_value.apartment_id.trim().is_empty() || config_value.room_id.trim().is_empty() {
        return Err("请填写楼栋/公寓 ID 和房间 ID，也可以先使用自动获取".into());
    }
    config::save_config(&config_value)?;
    runtime.set_configured();
    let _ = app.emit("status-changed", runtime.status_view());
    config::read_view()
}

#[tauri::command]
fn set_paused(
    app: AppHandle,
    runtime: State<'_, RuntimeHandle>,
    paused: bool,
) -> Result<(), String> {
    runtime.set_paused(paused);
    let _ = app.emit("status-changed", runtime.status_view());
    Ok(())
}

#[tauri::command]
fn login_now(app: AppHandle, runtime: State<'_, RuntimeHandle>) -> Result<(), String> {
    runtime.login_now()?;
    let _ = app.emit("status-changed", runtime.status_view());
    Ok(())
}

#[tauri::command]
fn logs_path() -> String {
    paths::log_dir().to_string_lossy().into_owned()
}

#[tauri::command]
fn set_startup_enabled(runtime: State<'_, RuntimeHandle>, enabled: bool) -> Result<(), String> {
    startup::set_enabled(enabled)?;
    runtime
        .logger
        .event("INFO", "auto_start_changed", &format!("enabled={enabled}"));
    Ok(())
}

#[derive(Serialize)]
struct LogEntry {
    time: String,
    kind: String,
    label: String,
    message: String,
}

#[tauri::command]
fn get_logs() -> Vec<LogEntry> {
    let mut entries = Vec::new();
    let mut paths_to_read = vec![paths::log_dir().join("current.log")];
    if let Ok(read_dir) = fs::read_dir(paths::log_dir()) {
        for item in read_dir.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with("archive-") && name.ends_with(".jsonl") {
                paths_to_read.push(item.path());
            }
        }
    }
    paths_to_read.sort();
    for path in paths_to_read {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let (time, level, event, message) =
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    (
                        value
                            .get("ts")
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string(),
                        value
                            .get("level")
                            .and_then(|v| v.as_str())
                            .unwrap_or("INFO")
                            .to_string(),
                        value
                            .get("event")
                            .and_then(|v| v.as_str())
                            .unwrap_or("event")
                            .to_string(),
                        value
                            .get("detail")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    )
                } else {
                    let mut parts = line.splitn(4, ' ');
                    (
                        parts
                            .next()
                            .unwrap_or_default()
                            .strip_prefix("ts=")
                            .unwrap_or_default()
                            .to_string(),
                        parts
                            .next()
                            .unwrap_or_default()
                            .strip_prefix("level=")
                            .unwrap_or("INFO")
                            .to_string(),
                        parts
                            .next()
                            .unwrap_or_default()
                            .strip_prefix("event=")
                            .unwrap_or("event")
                            .to_string(),
                        parts.next().unwrap_or_default().to_string(),
                    )
                };
            let kind = if level == "ERROR" {
                "error"
            } else if event.starts_with("auth") {
                "auth"
            } else {
                "network"
            };
            entries.push(LogEntry {
                time,
                kind: kind.into(),
                label: event,
                message,
            });
        }
    }
    entries
}

#[tauri::command]
fn export_logs() -> Result<String, String> {
    let dir = paths::local_data_dir().join("exports");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let output = dir.join(format!("diagnostics-{stamp}.log"));
    let mut content = String::new();
    for path in [
        paths::log_dir().join("previous.log"),
        paths::log_dir().join("current.log"),
    ] {
        if let Ok(text) = fs::read_to_string(path) {
            content.push_str(&text);
            content.push('\n');
        }
    }
    fs::write(&output, content).map_err(|e| e.to_string())?;
    Ok(output.to_string_lossy().into_owned())
}

// Windows WebView2 initialization re-enters the UI event loop. Creating a
// WebView in a synchronous IPC command deadlocks that loop; keep this async.
#[tauri::command]
async fn open_capture_window(
    app: AppHandle,
    runtime: State<'_, RuntimeHandle>,
    lifecycle: State<'_, CaptureLifecycle>,
) -> Result<(), String> {
    // Serialize open/cancel without ever blocking the native event loop. A
    // cancel arriving during initialization will close the resulting window.
    let mut session = lifecycle.0.lock().await;
    let label = "capture";
    if let Some(current) = session.as_mut() {
        if current.closing.load(Ordering::Acquire) || app.get_webview_window(label).is_none() {
            // destroy() only queues native destruction. Wait until its handler
            // has finished cleaning up before reusing the window label.
            let _ = current.closed.recv().await;
            *session = None;
        }
    }
    if let Some(window) = app.get_webview_window(label) {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let initial_url = capture_start_url()?;
    runtime.set_capture_active(true);
    runtime
        .logger
        .event("INFO", "capture_window_opening", "source=user_action");
    let app_for_navigation = app.clone();
    let runtime_for_navigation = runtime.inner().inner.clone();
    let runtime_capture = runtime.inner().clone();
    let runtime_for_close = runtime.inner().clone();
    let app_for_close = app.clone();
    let capture_host = config::load_config().ok().map(|value| {
        if value.portal_host.trim().is_empty() {
            url::Url::parse(&value.gateway)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default()
        } else {
            value.portal_host
        }
    });
    let logger = runtime.logger.clone();
    let builder = WebviewWindowBuilder::new(&app, label, WebviewUrl::External(initial_url));
    #[cfg(debug_assertions)]
    let builder = capture_test_browser_args(builder, true);
    let window_result = builder
        .title("获取校园网认证信息")
        .inner_size(980.0, 720.0)
        .resizable(true)
        // The capture page is an authentication entry point.  Keep it in a
        // non-persistent WebView2 profile so stale portal cookies and cached
        // error pages cannot affect the next capture attempt.
        .incognito(true)
        .general_autofill_enabled(false)
        // A private profile is shared by WebViews in the same data directory.
        // Isolate capture from the long-lived dashboard so closing it ends
        // the authentication session even while the main window stays open.
        .data_directory(paths::local_data_dir().join("WebView").join("capture"))
        // Install lifecycle handling before the user can close the window.
        .visible(false)
        .on_navigation(move |url| {
            let binding = capture_host
                .as_deref()
                .and_then(|host| portal::extract_room_binding_for_host(url.as_str(), host))
                .or_else(|| {
                    if capture_host.is_none() {
                        portal::extract_room_binding(url.as_str())
                    } else {
                        None
                    }
                });
            if let Some((apartment_id, room_id)) = binding {
                let mut config_value = match config::load_config() {
                    Ok(value) => value,
                    Err(error) => {
                        logger.event("ERROR", "capture_config_read_failed", &error);
                        return true;
                    }
                };
                config_value.apartment_id = apartment_id.clone();
                config_value.room_id = room_id.clone();
                if let Err(error) = config::save_config(&config_value) {
                    logger.event("ERROR", "capture_config_save_failed", &error);
                } else {
                    logger.event(
                        "INFO",
                        "capture_success",
                        "apartment_id_present=true room_id_present=true",
                    );
                    let _ = app_for_navigation.emit(
                        "capture-result",
                        CaptureResult {
                            apartment_id,
                            room_id,
                        },
                    );
                    runtime_capture.set_capture_active(false);
                    if let Ok(mut state) = runtime_for_navigation.lock() {
                        state.status = model::AppStatus::Checking;
                        state.detail = "认证信息已获取，正在验证网络".into();
                        state.checking = false;
                    }
                }
            }
            true
        })
        .build();
    let window = match window_result {
        Ok(window) => window,
        Err(error) => {
            runtime.set_capture_active(false);
            runtime.logger.event(
                "ERROR",
                "capture_window_create_failed",
                "monitor_resumed=true",
            );
            let _ = app.emit_to(
                "main",
                "capture-window-closed",
                serde_json::json!({ "reason": "create_failed" }),
            );
            return Err(format!("无法创建认证窗口：{error}"));
        }
    };
    runtime
        .logger
        .event("INFO", "capture_window_ready", "native_window_created=true");
    let closing = Arc::new(AtomicBool::new(false));
    let closing_for_event = closing.clone();
    let (closed_tx, closed_rx) = tauri::async_runtime::channel(1);
    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                // Explicit cancellation must not wait for the external
                // page to finish navigating or approve closing.
                api.prevent_close();
                if !closing_for_event.swap(true, Ordering::AcqRel) {
                    if let Some(window) = app_for_close.get_webview_window("capture") {
                        if window.destroy().is_err() {
                            closing_for_event.store(false, Ordering::Release);
                            runtime_for_close.logger.event(
                                "ERROR",
                                "capture_window_close_failed",
                                "source=native_close",
                            );
                        }
                    }
                }
            }
            tauri::WindowEvent::Destroyed => {
                closing_for_event.store(true, Ordering::Release);
                runtime_for_close.set_capture_active(false);
                runtime_for_close.logger.event(
                    "INFO",
                    "capture_window_closed",
                    "monitor_resumed=true",
                );
                let _ = app_for_close.emit_to(
                    "main",
                    "capture-window-closed",
                    serde_json::json!({ "reason": "closed" }),
                );
                // Never take the lifecycle mutex here: the async close/open
                // command may hold it while waiting for this notification.
                let _ = closed_tx.try_send(());
            }
            _ => {}
        }
    });
    *session = Some(CaptureSession {
        closing,
        closed: closed_rx,
    });
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

fn capture_start_url() -> Result<url::Url, String> {
    // The native-window regression test uses a local stalled/refused server.
    // Release builds always use the real portal-triggering HTTP address.
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("CAMPUS_CAPTURE_TEST_URL") {
        let url = url::Url::parse(&value).map_err(|_| "invalid capture fixture URL")?;
        if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") {
            return Ok(url);
        }
        return Err("capture fixture must use loopback HTTP".into());
    }
    url::Url::parse("http://baidu.com").map_err(|e| e.to_string())
}

// Native regression tests opt into a local CDP port. Neither the environment
// hook nor browser debugging flags are included in release builds.
#[cfg(debug_assertions)]
fn capture_test_browser_args(
    builder: WebviewWindowBuilder<'_, tauri::Wry, AppHandle>,
    capture: bool,
) -> WebviewWindowBuilder<'_, tauri::Wry, AppHandle> {
    let specific = if capture {
        std::env::var("CAMPUS_CAPTURE_TEST_CAPTURE_BROWSER_ARGS").ok()
    } else {
        None
    };
    match specific.or_else(|| std::env::var("CAMPUS_CAPTURE_TEST_BROWSER_ARGS").ok()) {
        Some(args) => builder.additional_browser_args(&args),
        None => builder,
    }
}

#[tauri::command]
async fn close_capture_window(
    app: AppHandle,
    lifecycle: State<'_, CaptureLifecycle>,
) -> Result<(), String> {
    let mut session = lifecycle.0.lock().await;
    if let Some(window) = app.get_webview_window("capture") {
        // `close()` can be intercepted by WebView2's close-request lifecycle;
        // this command is an explicit user cancellation, so destroy directly.
        if let Some(current) = session.as_mut() {
            if !current.closing.swap(true, Ordering::AcqRel) {
                if let Err(error) = window.destroy() {
                    current.closing.store(false, Ordering::Release);
                    return Err(error.to_string());
                }
            }
            let _ = current.closed.recv().await;
        } else {
            window.destroy().map_err(|e| e.to_string())?;
        }
    } else if let Some(current) = session.as_mut() {
        // The manager removes its window before invoking our Destroyed
        // listener; wait for that listener's state cleanup too.
        let _ = current.closed.recv().await;
    }
    *session = None;
    if let Some(runtime) = app.try_state::<RuntimeHandle>() {
        runtime.set_capture_active(false);
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) -> Result<(), String> {
    let lock = MAIN_WINDOW_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| "主窗口创建锁已损坏，请重启应用".to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        window.unminimize().map_err(|e| e.to_string())?;
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()));
    #[cfg(debug_assertions)]
    let builder = capture_test_browser_args(builder, false);
    builder
        .title("校园网自动登录")
        // The dashboard is designed around this 3:2 client area. 800x600
        // was narrower than the UI's 860px minimum and clipped the right
        // side of the overview on first launch.
        .inner_size(1080.0, 740.0)
        .resizable(false)
        // The dashboard accepts account credentials but must never expose
        // WebView2's saved-form suggestions.  Application-managed credentials
        // remain in the encrypted Windows store instead.
        .incognito(true)
        .general_autofill_enabled(false)
        .data_directory(paths::local_data_dir().join("WebView").join("main"))
        .visible(true)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn show_main_window_async(app: &AppHandle) {
    // WebView2 window creation can re-enter the Windows event loop. Keep it
    // off synchronous tray/single-instance callbacks so the UI stays live.
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = show_main_window(&app) {
            app.state::<RuntimeHandle>()
                .logger
                .event("ERROR", "main_window_create_failed", &error);
        }
    });
}

fn tray_menu(app: &tauri::AppHandle) -> Result<tauri::menu::Menu<tauri::Wry>, tauri::Error> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    MenuBuilder::new(app)
        .items(&[
            &MenuItemBuilder::with_id("open", "打开控制面板").build(app)?,
            &MenuItemBuilder::with_id("login", "立即登录").build(app)?,
            &MenuItemBuilder::with_id("pause", "暂停自动登录").build(app)?,
            &MenuItemBuilder::with_id("quit", "退出").build(app)?,
        ])
        .build()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime = RuntimeHandle::new();
    let runtime_for_setup = runtime.clone();
    let runtime_for_tray = runtime.clone();
    tauri::Builder::default()
        .manage(runtime)
        .manage(CaptureLifecycle::default())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A second manual launch is a request to open the existing panel.
            // Automatic startup passes --background and must stay quiet.
            if args.iter().skip(1).any(|arg| arg == "--background") {
                return;
            }
            show_main_window_async(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let app_for_status = app.handle().clone();
            runtime_for_setup.set_status_notifier(move |status| {
                let _ = app_for_status.emit("status-changed", status);
            });
            runtime_for_setup.start_monitor();
            let app_handle = app.handle();
            let menu = tray_menu(&app_handle)?;
            let mut tray_builder = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                // A left click opens the panel. Keep the context menu for
                // right clicks, which is the standard tray interaction.
                .show_menu_on_left_click(false)
                .tooltip("校园网自动登录")
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_main_window_async(tray.app_handle());
                    }
                })
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => {
                        // Tray callbacks run on the UI thread too. Recreating
                        // a closed WebView here has the same Windows deadlock.
                        show_main_window_async(&app);
                    }
                    "login" => {
                        let _ = runtime_for_tray.login_now();
                    }
                    "pause" => {
                        let paused = matches!(
                            runtime_for_tray.status_view().status,
                            model::AppStatus::Paused
                        );
                        runtime_for_tray.set_paused(!paused);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            // Keep the tray icon aligned with the bundled Campus Link app icon.
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            tray_builder.build(app)?;
            if !std::env::args().skip(1).any(|arg| arg == "--background") {
                let _ = show_main_window(&app_handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            save_setup,
            save_config,
            set_paused,
            login_now,
            logs_path,
            set_startup_enabled,
            get_logs,
            export_logs,
            open_capture_window,
            close_capture_window
        ])
        .build(tauri::generate_context!())
        .expect("error while building campus auto login")
        .run(|_, event| {
            if let tauri::RunEvent::ExitRequested {
                api, code: None, ..
            } = event
            {
                // Closing windows leaves monitoring in the tray. Explicit
                // Quit uses app.exit(0), so has Some(0) and is not prevented.
                api.prevent_exit();
            }
        });
}
