//! Deskoryn tray UI — a thin Tauri client for the `deskorynd` daemon.
//!
//! The app holds no state and does no networking of its own. Every Tauri
//! command here forwards to the daemon's local control socket (see [`ipc`]) and
//! returns the daemon's `UiEvent`s to the webview as JSON. The daemon is the
//! single source of truth.

mod daemon;
mod ipc;

use ipc::{Feature, UiRequest};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

/// Forward a request to the daemon and return its events as a JSON array.
async fn forward(req: UiRequest) -> Result<serde_json::Value, String> {
    let events = ipc::request(&req)
        .await
        .map_err(|e| format!("daemon control socket: {e}"))?;
    serde_json::to_value(events).map_err(|e| e.to_string())
}

#[tauri::command]
async fn daemon_status() -> Result<serde_json::Value, String> {
    forward(UiRequest::Status).await
}

#[tauri::command]
async fn daemon_pair(addr: String) -> Result<serde_json::Value, String> {
    forward(UiRequest::Pair { addr }).await
}

#[tauri::command]
async fn daemon_pair_confirm(accept: bool) -> Result<serde_json::Value, String> {
    forward(UiRequest::PairConfirm { accept }).await
}

#[tauri::command]
async fn daemon_forget(device: String) -> Result<serde_json::Value, String> {
    forward(UiRequest::Forget { device }).await
}

#[tauri::command]
async fn daemon_set_feature(feature: Feature, enabled: bool) -> Result<serde_json::Value, String> {
    forward(UiRequest::SetFeature { feature, enabled }).await
}

#[tauri::command]
async fn daemon_set_layout(layout: serde_json::Value) -> Result<serde_json::Value, String> {
    forward(UiRequest::SetLayout { layout }).await
}

/// Bring the main window to the front (creating nothing — it always exists,
/// just hidden when "closed").
fn show_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(daemon::ProcMgr::default())
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            daemon_pair,
            daemon_pair_confirm,
            daemon_forget,
            daemon_set_feature,
            daemon_set_layout,
            daemon::daemon_bin_info,
            daemon::set_daemon_bin,
            daemon::daemon_lifecycle,
            daemon::daemon_start,
            daemon::daemon_stop,
            daemon::pair_start,
            daemon::pair_respond,
            daemon::pair_cancel,
            daemon::pair_reap,
        ])
        .setup(|app| {
            // Load the persisted daemon-binary override before any command runs.
            let mgr = app.state::<daemon::ProcMgr>();
            tauri::async_runtime::block_on(daemon::load_override(mgr.inner()));

            // Tray menu: open the GUI, or quit the app outright.
            let open = MenuItem::with_id(app, "open", "Open Deskoryn", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            TrayIconBuilder::with_id("main-tray")
                .tooltip("Deskoryn")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                // Show the Open/Quit menu on a LEFT click; this also suppresses
                // the right-click menu (the flag is an XOR, not an addition).
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Show the window on first launch; thereafter the tray governs it.
            show_main(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it to the tray instead of quitting.
            // Quitting is an explicit choice from the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running the Deskoryn tray UI");
}
