//! Menu-bar tray for the desktop shell — the Tauri glue over the pure status→menu mapping
//! ([`rhapsody_desktop::menu`]). Parity port of `$REF/desktop/tray.go`: a menu-bar item whose status
//! header and which actions are live track the daemon status (refreshed every 2s), wiring
//! Open / Settings / Start / Stop / Restart / Quit to the [`App`] lifecycle.
//!
//! The tray renders icon-only (TRA-259): the build always embeds the app icon (TRA-254), so no text
//! title is set — a "Rhapsody" title is kept only as a fallback for when no icon is embedded. The
//! tooltip stays "Rhapsody" (the Go reference uses the upstream vendor's Symphony branding); the
//! dynamic status-header text comes from [`rhapsody_desktop::menu::menu_from_status`], which the D3
//! tests pin.

use std::time::Duration;

use rhapsody_desktop::app::App;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, Runtime};

// Stable menu-item ids, matched in the menu-event handler.
const ID_OPEN: &str = "tray-open";
const ID_SETTINGS: &str = "tray-settings";
const ID_START: &str = "tray-start";
const ID_STOP: &str = "tray-stop";
const ID_RESTART: &str = "tray-restart";
const ID_QUIT: &str = "tray-quit";

// `MenuItem::with_id` needs a concrete accelerator type even when there is none.
const NO_ACCEL: Option<&str> = None;

/// The tray items the refresh loop keeps in sync with the daemon status (the status-header text plus
/// which actions are enabled). Settings + Quit have fixed titles/enabled, so they need no handle.
/// Mirrors Go `trayItems`.
struct TrayItems<R: Runtime> {
    status: MenuItem<R>,
    open: MenuItem<R>,
    start: MenuItem<R>,
    stop: MenuItem<R>,
    restart: MenuItem<R>,
}

/// Registers the menu-bar tray on the running app and starts the status-refresh loop. Mirrors Go
/// `startTray` + `onTrayReady`.
pub fn start_tray<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "tray-status", "Rhapsody — Stopped", false, NO_ACCEL)?;
    let open = MenuItem::with_id(app, ID_OPEN, "Open Dashboard", true, NO_ACCEL)?;
    let settings = MenuItem::with_id(app, ID_SETTINGS, "Settings…", true, NO_ACCEL)?;
    let start = MenuItem::with_id(app, ID_START, "Start Daemon", true, NO_ACCEL)?;
    let stop = MenuItem::with_id(app, ID_STOP, "Stop Daemon", true, NO_ACCEL)?;
    let restart = MenuItem::with_id(app, ID_RESTART, "Restart Daemon", true, NO_ACCEL)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit Rhapsody", true, NO_ACCEL)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &status, &sep1, &open, &settings, &sep2, &start, &stop, &restart, &sep3, &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("rhapsody-tray")
        .tooltip("Rhapsody")
        .menu(&menu)
        .on_menu_event(|app, event| handle_menu_event(app, event));
    // Render icon-only when the build embedded an app icon (it always does since TRA-254) — a text
    // title next to the icon just wastes menu-bar space (TRA-259). macOS renders the icon as-is (not a
    // template) to match the colored app icon. Only when no icon is embedded do we fall back to a
    // "Rhapsody" text title so the menu-bar item is still identifiable.
    match app.default_window_icon() {
        Some(icon) => builder = builder.icon(icon.clone()),
        None => builder = builder.title("Rhapsody"),
    }
    let tray = builder.build(app)?;

    let items = TrayItems {
        status,
        open,
        start,
        stop,
        restart,
    };
    spawn_refresh_loop(app.clone(), tray, items);
    Ok(())
}

/// Dispatches a tray menu click to the [`App`] action. Mirrors the `items.*.Click(...)` wiring in Go
/// `onTrayReady`.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id.as_ref() {
        ID_OPEN => {
            show_window(app);
            navigate(app, "dashboard");
        }
        ID_SETTINGS => {
            show_window(app);
            navigate(app, "settings");
        }
        ID_START => {
            // Run in the async runtime so the App's background start task has a tokio context. The
            // tray, like Go (`_ = a.StartDaemon()`), ignores the refusal — the shell surfaces it.
            let a = app_state(app);
            tauri::async_runtime::spawn(async move {
                let _ = a.start_daemon();
            });
        }
        ID_STOP => {
            let a = app_state(app);
            tauri::async_runtime::spawn(async move { a.stop_daemon().await });
        }
        ID_RESTART => {
            let a = app_state(app);
            tauri::async_runtime::spawn(async move {
                let _ = a.restart_daemon().await;
            });
        }
        ID_QUIT => {
            // Go `quit()`: show the window (so the "Shutting down…" overlay is visible), then Quit —
            // which the run loop's ExitRequested handler turns into the off-main-thread drain.
            show_window(app);
            app.exit(0);
        }
        _ => {}
    }
}

/// The managed [`App`] (a cheap `Arc` clone) so it can move into an async task.
fn app_state<R: Runtime>(app: &AppHandle<R>) -> App {
    app.state::<App>().inner().clone()
}

/// Shows + focuses the main window. Mirrors Go `showWindow` (`wruntime.WindowShow`).
fn show_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Asks the frontend to switch views (dashboard/settings) via the tray navigate event. Mirrors Go
/// `navigate` (`EventsEmit("tray:navigate", view)`); acting on "settings" lands in P7-D4.
fn navigate<R: Runtime>(app: &AppHandle<R>, view: &str) {
    let _ = app.emit("tray:navigate", view);
}

/// Keeps the menu-bar item in sync with the daemon status (status-header text + which actions are
/// live + the tooltip), polling every 2s until the app exits. Mirrors Go `refreshTrayLoop` +
/// `applyTray`. The `MenuItem`/`TrayIcon` setters marshal to the main thread internally, so driving
/// them from this task is safe.
fn spawn_refresh_loop<R: Runtime>(app: AppHandle<R>, tray: TrayIcon<R>, items: TrayItems<R>) {
    let state = app_state(&app);
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await; // fires at once on the first tick (applyTray before the loop)
            let model = state.tray_menu_model().await;
            let _ = items.status.set_text(&model.status_text);
            let _ = items.open.set_enabled(model.can_open);
            let _ = items.start.set_enabled(model.can_start);
            let _ = items.stop.set_enabled(model.can_stop);
            let _ = items.restart.set_enabled(model.can_restart);
            let tooltip = if model.tooltip.is_empty() {
                "Rhapsody".to_string()
            } else {
                model.tooltip
            };
            let _ = tray.set_tooltip(Some(&tooltip));
        }
    });
}
