//! Rhapsody desktop — the macOS app that supervises the `rhapsodyd` sidecar and shows its dashboard.
//! Tauri v2 port of the Wails shell (`$REF/desktop/main.go` + `app.go`). P7-D3 wires the menu-bar
//! tray + the app lifecycle: closing the window hides it (the tray + daemon keep running), while
//! quitting shows a "Shutting down…" overlay and drains the daemon OFF the main thread. Settings /
//! credential / onboarding (D4) and packaging (D5) land in later chain tasks.

mod tray;
mod version;

use std::time::Duration;

use rhapsody_desktop::app::{App, CloseDecision, StatusDto};
use tauri::{Emitter, Manager};
use version::VersionDto;

/// The current daemon status snapshot for the shell + tray. Mirrors Go `App.Status`
/// (`$REF/desktop/app.go`): now backed by the live supervisor (D1 shipped a `stopped` stub).
#[tauri::command]
async fn status(app: tauri::State<'_, App>) -> Result<StatusDto, String> {
    Ok(app.status().await)
}

/// The compiled-in build stamp for the footer. Mirrors Go `App.AppVersion` (`$REF/desktop/app.go`).
#[tauri::command]
fn app_version() -> VersionDto {
    version::dto()
}

/// Start the daemon on demand (shell button + tray). Async so the App's background start task has a
/// tokio runtime context; refuses without a WORKFLOW.md. Mirrors Go `App.StartDaemon`.
#[tauri::command]
async fn start_daemon(app: tauri::State<'_, App>) -> Result<(), String> {
    app.start_daemon().map_err(|e| e.to_string())
}

/// Stop the daemon. Mirrors Go `App.StopDaemon`.
#[tauri::command]
async fn stop_daemon(app: tauri::State<'_, App>) -> Result<(), String> {
    app.stop_daemon().await;
    Ok(())
}

/// Restart the daemon (rebuilds the supervisor so new tool overrides take effect). Mirrors Go
/// `App.RestartDaemon`.
#[tauri::command]
async fn restart_daemon(app: tauri::State<'_, App>) -> Result<(), String> {
    app.restart_daemon().await.map_err(|e| e.to_string())
}

fn main() {
    // Errors are values (no panic on the startup path): mirror Go `main`, which logs the run error
    // ($REF/desktop/main.go). A failed launch exits non-zero so a supervising shell notices.
    if let Err(err) = run() {
        eprintln!("Rhapsody desktop failed to start: {err}");
        std::process::exit(1);
    }
}

fn run() -> tauri::Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            status,
            app_version,
            start_daemon,
            stop_daemon,
            restart_daemon
        ])
        .setup(|app| {
            let application = App::from_env();
            app.manage(application.clone());
            // The menu-bar tray is built on the main thread (menu items live there). Mirrors
            // OnStartup's a.startTray().
            tray::start_tray(app.handle())?;
            // Build the supervisor and — when configured — kick off the daemon inside the async
            // runtime so the App's background start task has a tokio context. Mirrors OnStartup's tail.
            tauri::async_runtime::spawn(async move { application.on_startup() });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Menu-bar app behaviour: closing the window hides it (the tray + daemon keep running);
            // the user quits explicitly from the tray's Quit item. Mirrors Wails HideWindowOnClose.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?
        .run(|handle, event| match event {
            // Quit: drain the daemon OFF the main thread behind a "Shutting down…" overlay, then let
            // the drain's re-issued exit through. Mirrors Go `OnBeforeClose`.
            tauri::RunEvent::ExitRequested { api, .. } => {
                // `try_state` (not `state`) so a teardown with the state already gone never panics.
                let Some(app) = handle.try_state::<App>().map(|s| s.inner().clone()) else {
                    return;
                };
                match app.on_before_close() {
                    CloseDecision::StartDrain => {
                        api.prevent_exit();
                        let _ = handle.emit("app:shutting-down", ());
                        let h = handle.clone();
                        tauri::async_runtime::spawn(async move {
                            app.drain_daemon(Duration::from_secs(10)).await;
                            h.exit(0); // re-enters ExitRequested → Proceed → teardown
                        });
                    }
                    CloseDecision::WaitForDrain => api.prevent_exit(),
                    CloseDecision::Proceed => {}
                }
            }
            // Final-teardown backstop: when the drain already ran this returns at once; a quit that
            // bypassed the close hook does its own bounded Stop here. Mirrors Go `OnShutdown`.
            tauri::RunEvent::Exit => {
                if let Some(app) = handle.try_state::<App>().map(|s| s.inner().clone()) {
                    tauri::async_runtime::block_on(app.on_shutdown());
                }
            }
            _ => {}
        });
    Ok(())
}
