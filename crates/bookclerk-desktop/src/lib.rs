//! Tauri desktop shell: loads the shared React GUI and owns the system tray.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

use bookclerk_config::{operator_token_path, read_or_create_operator_token, Config};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State};

struct DaemonState {
    child: Mutex<Option<Child>>,
    base_url: String,
}

#[derive(Serialize)]
struct DesktopInfo {
    base_url: String,
    token: Option<String>,
}

#[tauri::command]
fn desktop_info(state: State<'_, DaemonState>, config: State<'_, Config>) -> DesktopInfo {
    let token = if config.daemon.auth.enabled {
        read_or_create_operator_token(&config)
            .ok()
            .map(|(token, _)| token)
    } else {
        None
    };
    DesktopInfo {
        base_url: state.base_url.clone(),
        token,
    }
}

#[tauri::command]
fn operator_token(config: State<'_, Config>) -> Option<String> {
    if !config.daemon.auth.enabled {
        return None;
    }
    read_or_create_operator_token(&config)
        .ok()
        .map(|(token, _)| token)
}

#[tauri::command]
fn trigger_scan(state: State<'_, DaemonState>, config: State<'_, Config>) -> Result<(), String> {
    post_daemon(
        &format!("{}/api/library/scan", state.base_url),
        "{}",
        &config,
    )
}

fn post_daemon(url: &str, body: &str, config: &Config) -> Result<(), String> {
    let mut req = ureq::post(url).set("Content-Type", "application/json");
    if config.daemon.auth.enabled {
        let (token, _) = read_or_create_operator_token(config).map_err(|e| e.to_string())?;
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    req.send_string(body).map_err(|e| e.to_string())?;
    Ok(())
}

fn daemon_reachable(base: &str) -> bool {
    ureq::get(&format!("{base}/health"))
        .timeout(Duration::from_secs(2))
        .call()
        .is_ok()
}

fn spawn_daemon(files_dir: &std::path::Path, listen: &str) -> anyhow::Result<Child> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let candidates = [
        dir.join("bookclerkd"),
        PathBuf::from("bookclerkd"),
        PathBuf::from("target/debug/bookclerkd"),
        PathBuf::from("target/release/bookclerkd"),
    ];
    let bin = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("bookclerkd binary not found beside desktop app"))?;
    let child = Command::new(bin)
        .env("BOOKCLERK_FILES_DIR", files_dir)
        .env("BOOKCLERK_DAEMON_LISTEN", listen)
        .spawn()?;
    Ok(child)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

#[cfg(not(debug_assertions))]
fn navigate_to_daemon_ui(app: &tauri::App, base_url: &str) {
    if !daemon_reachable(base_url) {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        if let Ok(url) = tauri::Url::parse(base_url) {
            let _ = window.navigate(url);
        }
    }
}

#[cfg(debug_assertions)]
fn navigate_to_daemon_ui(_app: &tauri::App, _base_url: &str) {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = Config::load(None, None).expect("load Bookclerk config");
    let files_dir = config.paths().files_dir.clone();
    let listen = config.daemon.listen.clone();
    let base_url = if listen.starts_with("http://") || listen.starts_with("https://") {
        listen.trim_end_matches('/').to_string()
    } else {
        format!("http://{listen}")
    };

    let mut child = None;
    if !daemon_reachable(&base_url) {
        match spawn_daemon(&files_dir, &listen) {
            Ok(c) => {
                // Wait briefly for listen.
                for _ in 0..40 {
                    if daemon_reachable(&base_url) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                child = Some(c);
            }
            Err(err) => {
                eprintln!("bookclerk-desktop: failed to spawn bookclerkd: {err}");
            }
        }
    }

    // Ensure operator token exists before UI loads.
    if config.daemon.auth.enabled {
        let _ = read_or_create_operator_token(&config);
        eprintln!(
            "bookclerk-desktop: operator token file {}",
            operator_token_path(&config).display()
        );
    }

    let daemon_state = DaemonState {
        child: Mutex::new(child),
        base_url: base_url.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(config)
        .manage(daemon_state)
        .invoke_handler(tauri::generate_handler![
            desktop_info,
            operator_token,
            trigger_scan
        ])
        .setup(move |app| {
            let show_i = MenuItem::with_id(app, "show", "Show Bookclerk", true, None::<&str>)?;
            let hide_i = MenuItem::with_id(app, "hide", "Hide window", true, None::<&str>)?;
            let scan_i = MenuItem::with_id(app, "scan", "Scan library", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &hide_i, &scan_i, &quit_i])?;

            let tray_icon = tauri::image::Image::from_path(
                app.path()
                    .resource_dir()
                    .ok()
                    .map(|p| p.join("icons/tray-icon.png"))
                    .filter(|p| p.is_file())
                    .unwrap_or_else(|| {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("icons/tray-icon.png")
                    }),
            )
            .unwrap_or_else(|_| {
                // 1x1 fallback — should not happen when icons are packaged.
                tauri::image::Image::new_owned(vec![0, 0, 0, 255], 1, 1)
            });

            let _tray = TrayIconBuilder::new()
                .icon(tray_icon)
                .menu(&menu)
                .tooltip("Bookclerk")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "hide" => hide_main_window(app),
                    "scan" => {
                        if let (Some(state), Some(config)) =
                            (app.try_state::<DaemonState>(), app.try_state::<Config>())
                        {
                            let _ = trigger_scan(state, config);
                        }
                    }
                    "quit" => {
                        if let Some(state) = app.try_state::<DaemonState>() {
                            if let Ok(mut guard) = state.child.lock() {
                                if let Some(child) = guard.as_mut() {
                                    let _ = child.kill();
                                }
                                *guard = None;
                            }
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Production: load daemon-hosted UI when available so API is same-origin.
            navigate_to_daemon_ui(app, &base_url);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Bookclerk desktop")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<DaemonState>() {
                    if let Ok(mut guard) = state.child.lock() {
                        if let Some(child) = guard.as_mut() {
                            let _ = child.kill();
                        }
                        *guard = None;
                    }
                }
            }
        });
}
