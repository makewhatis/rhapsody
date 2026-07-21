//! Rhapsody desktop — the macOS app that supervises the `rhapsodyd` sidecar and shows its dashboard.
//! Tauri v2 port of the Wails shell (`$REF/desktop/main.go` + `app.go`). P7-D3 wires the menu-bar
//! tray + the app lifecycle: closing the window hides it (the tray + daemon keep running), while
//! quitting shows a "Shutting down…" overlay and drains the daemon OFF the main thread. Settings /
//! credential / onboarding (D4) and packaging (D5) land in later chain tasks.

mod tray;

use std::time::Duration;

use rhapsody_desktop::app::{App, CloseDecision, CredentialStatusDto, StatusDto};
use rhapsody_desktop::linearprojects::Project;
use rhapsody_desktop::logbridge::{LogBridge, LogMsg};
use rhapsody_desktop::toolcheck::ToolResult;
use rhapsody_desktop::update::{self, UpdateState};
use rhapsody_desktop::version::{self, VersionDto};
use rhapsody_desktop::windowserver;
use tauri::{Emitter, Manager};

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

// ---- D4 settings commands (Tauri stand-ins for the Wails-bound App methods) -----------------------

/// Probe the external CLIs (claude, gh, gt, git) for the Tool-doctor panel. Mirrors Go `App.ProbeTools`.
#[tauri::command]
async fn probe_tools(app: tauri::State<'_, App>) -> Result<Vec<ToolResult>, String> {
    Ok(app.probe_tools().await)
}

/// Record an explicit path for a tool (empty path clears it). Mirrors Go `App.SetToolOverride`.
#[tauri::command]
async fn set_tool_override(
    app: tauri::State<'_, App>,
    name: String,
    path: String,
) -> Result<(), String> {
    app.set_tool_override(&name, &path)
}

/// The Linear credential panel status. Mirrors Go `App.CredentialStatus`.
#[tauri::command]
async fn credential_status(app: tauri::State<'_, App>) -> Result<CredentialStatusDto, String> {
    Ok(app.credential_status())
}

/// Store a pasted Linear token (Keychain, file fallback) and restart the daemon. Mirrors Go
/// `App.SetLinearToken`.
#[tauri::command]
async fn set_linear_token(app: tauri::State<'_, App>, token: String) -> Result<(), String> {
    app.set_linear_token(&token).await
}

/// Revoke the stored Linear token. Mirrors Go `App.ClearLinearToken`.
#[tauri::command]
async fn clear_linear_token(app: tauri::State<'_, App>) -> Result<(), String> {
    app.clear_linear_token().await
}

/// The deferred "Connect Linear" OAuth action (a clear message until a client_id exists). Mirrors Go
/// `App.StartLinearOAuth`.
#[tauri::command]
async fn start_linear_oauth(app: tauri::State<'_, App>) -> Result<(), String> {
    app.start_linear_oauth()
}

/// List the workspace's Linear projects for the onboarding picker. Mirrors Go `App.ListLinearProjects`.
#[tauri::command]
async fn list_linear_projects(app: tauri::State<'_, App>) -> Result<Vec<Project>, String> {
    app.list_linear_projects().await
}

/// Onboarding's final step: seed WORKFLOW.md for the chosen project and start the daemon. Mirrors Go
/// `App.WriteInitialConfig`.
#[tauri::command]
async fn write_initial_config(
    app: tauri::State<'_, App>,
    project_slug: String,
) -> Result<(), String> {
    app.write_initial_config(&project_slug).await
}

/// Open an external `http(s)` URL in the user's default browser (Linear links, the create-token page,
/// "Open ticket"). The embedded webview must not navigate away, so this shells out to macOS `open`.
/// Replaces the Wails runtime's `BrowserOpenURL` (which had no App-method equivalent to port).
#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    windowserver::open_external(&url)
}

/// Start the Logs view's live tail: connect the host to the supervised daemon's SSE log stream and
/// re-emit each frame over the given IPC `channel` (TRA-252). The packaged app can't tail the stream
/// through the buffered custom-protocol proxy, so it subscribes to this channel instead of `EventSource`.
/// Restarts cleanly if called again (the previous stream is aborted).
#[tauri::command]
async fn start_log_stream(
    app: tauri::State<'_, App>,
    bridge: tauri::State<'_, LogBridge>,
    channel: tauri::ipc::Channel<LogMsg>,
) -> Result<(), String> {
    let app = app.inner().clone();
    // Resolve the live daemon target fresh on every (re)connect — a restart rebinds a new loopback port.
    bridge.start(channel, move || app.daemon_base_url());
    Ok(())
}

/// Stop the Logs view's live tail (the view unmounted): abort the streaming task for the channel with
/// `stream_id` and drop its upstream connection. Mirrors closing an `EventSource`. The id targets the
/// exact stream so a rapid unmount/remount never aborts the wrong one.
#[tauri::command]
async fn stop_log_stream(
    bridge: tauri::State<'_, LogBridge>,
    stream_id: u32,
) -> Result<(), String> {
    bridge.stop(stream_id);
    Ok(())
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
    let builder = tauri::Builder::default()
        // P11-U1 in-app auto-update: the updater plugin drives check/download/install with built-in
        // minisign signature verification against tauri.conf.json's pubkey (the `update_*` commands below
        // wrap it). The process plugin exposes the JS relaunch/exit the frontend may call; the Rust
        // install path relaunches via the core `AppHandle::restart`, the same primitive it wraps.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // TRA-268: the native folder/file chooser for Settings' "Logs path" + Tools executable-path
        // pickers (frontend calls `open` from @tauri-apps/plugin-dialog). Gated by `dialog:allow-open`
        // in capabilities/default.json — without that grant the IPC is silently denied at runtime.
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            status,
            app_version,
            start_daemon,
            stop_daemon,
            restart_daemon,
            probe_tools,
            set_tool_override,
            credential_status,
            set_linear_token,
            clear_linear_token,
            start_linear_oauth,
            list_linear_projects,
            write_initial_config,
            open_external,
            start_log_stream,
            stop_log_stream,
            update::update_check,
            update::update_download,
            update::update_install,
            update::active_run_count
        ]);
    // Serve the top-level window from the embedded `web/` bundle and reverse-proxy its same-origin
    // `/api/*` fetches to the supervised rhapsodyd (the real double-chrome fix, TRA-251). A default
    // client (no request timeout) so a slow but finite API call is never cut short.
    windowserver::register(builder, reqwest::Client::new())
        .setup(|app| {
            let application = App::from_env();
            app.manage(application.clone());
            // Owns the Logs view's host-side log-stream bridge (TRA-252); the start/stop_log_stream
            // commands drive it.
            app.manage(LogBridge::default());
            // Owns the P11-U1 updater session (the checked update + downloaded bytes) shared by the
            // update_* commands and the quiet launch check.
            app.manage(UpdateState::default());
            // Quiet on-launch update check (non-blocking): emits `update:available` if a newer version
            // exists so the UI can badge the affordance; never delays or fails launch.
            update::spawn_launch_check(app.handle().clone());
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
                            // P11-U1: with the daemon drained (no live work to lose), install any update
                            // deferred by the active-runs guard so the new bundle is used next launch. A
                            // no-op when nothing is pending; bounded so it never strands the quit.
                            update::install_pending_on_quit(h.clone(), app.clone()).await;
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
