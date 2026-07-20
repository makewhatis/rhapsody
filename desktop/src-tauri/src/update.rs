//! P11-U1 in-app auto-update — the Tauri commands the dashboard drives, backed by `tauri-plugin-updater`
//! (check → download → install). The post-install relaunch uses the core `AppHandle::restart` (the same
//! primitive `tauri-plugin-process`, registered in `main`, exposes to the frontend as `relaunch`). This
//! is a Rhapsody-only feature with NO Go parity reference; the sole daemon-side input is the live
//! active-run count ([`App::active_run_count`]), the same `/api/v1/state` `counts.running` the tray reads.
//!
//! Signature verification is Tauri's BUILT-IN minisign check against `plugins.updater.pubkey`
//! (tauri.conf.json), performed inside `Update::download`/`install` — a tampered or wrong-key artifact is
//! rejected there and surfaces as an `Err`, never installed. Nothing here re-implements or weakens it.
//!
//! The safety-critical rule (spec: "never silently restart with active runs"): [`update_install`] refuses
//! to install-and-relaunch while runs are active unless the caller passes `force`, and instead persists a
//! pending flag ([`App::set_pending_update`]) so [`install_pending_on_quit`] installs it on the next
//! graceful quit — after the daemon has drained, when no work is in flight to lose.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::app::App;

/// Emitted (once, non-blocking) by the quiet on-launch check when a newer version exists, so the UI can
/// badge the update affordance without the user asking. Payload: [`UpdateInfo`].
pub const EVENT_UPDATE_AVAILABLE: &str = "update:available";
/// Emitted for each downloaded chunk during [`update_download`] so the UI can render a progress bar.
/// Payload: [`DownloadProgress`].
pub const EVENT_DOWNLOAD_PROGRESS: &str = "update:download-progress";

/// The upper bound on a quit-time install: a graceful quit must never hang indefinitely on a stalled
/// download, so [`install_pending_on_quit`] abandons the install (and proceeds to exit) past this.
const QUIT_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The result of an [`update_check`]: whether a newer version is available and its metadata. The serde
/// field names are the wire contract the `web/` bindings type against.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// True when the server announced a version newer than the running one.
    pub available: bool,
    /// The announced version (empty when none is available).
    pub version: String,
    /// The currently-running app version.
    pub current_version: String,
    /// The release notes / changelog body (empty when the manifest carried none).
    pub notes: String,
}

/// One download-progress tick for the UI's progress bar. `total` is `None` when the server sent no
/// Content-Length, so the UI shows an indeterminate bar in that case.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    /// Cumulative bytes downloaded so far.
    pub downloaded: u64,
    /// Total bytes to download, when the server reported it.
    pub total: Option<u64>,
}

/// The outcome of an [`update_install`]. When the install proceeds the app relaunches, so JS rarely
/// observes `installed: true`; the meaningful case is `blocked_active_runs > 0` — the install was refused
/// because runs are active and a pending flag was set to install on the next graceful quit.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    /// True only if the install completed without a relaunch superseding this return (rare in practice).
    pub installed: bool,
    /// The active-run count that blocked an unforced install (0 when the install was allowed to proceed).
    pub blocked_active_runs: i64,
}

/// Shared updater session state: the [`Update`] the last successful `check()` found (so download/install
/// act on the same one) and the bytes a prior download produced (so install skips a re-download). Managed
/// by the Tauri app and reached by the update commands via `State`.
#[derive(Default)]
pub struct UpdateState {
    /// The most recent checked update, or `None` when the app is up to date / unchecked.
    current: Mutex<Option<Update>>,
    /// Bytes from a completed [`update_download`], consumed by the next install; `None` until then.
    downloaded: Mutex<Option<Vec<u8>>>,
}

/// The install guard's pure core, unit-tested headlessly: an install may proceed to restart the app only
/// when it is `force`d or there are no active runs. This is the "never silently restart with active runs"
/// guarantee — a non-zero run count refuses (and defers) unless the user explicitly overrode it.
pub(crate) fn may_install_now(active_run_count: i64, force: bool) -> bool {
    force || active_run_count <= 0
}

/// Runs a fresh update check and stashes any discovered [`Update`] in `state` (invalidating stale
/// downloaded bytes) so a later download/install acts on the same one. Returns the metadata for the UI.
async fn check_into(handle: &AppHandle, state: &UpdateState) -> Result<UpdateInfo, String> {
    let updater = handle.updater().map_err(|e| e.to_string())?;
    let found = updater.check().await.map_err(|e| e.to_string())?;
    // A new check supersedes any prior download; clear it so install never uses bytes for a stale update.
    *state.downloaded.lock().await = None;
    match found {
        Some(update) => {
            let info = UpdateInfo {
                available: true,
                version: update.version.clone(),
                current_version: update.current_version.clone(),
                notes: update.body.clone().unwrap_or_default(),
            };
            *state.current.lock().await = Some(update);
            Ok(info)
        }
        None => {
            *state.current.lock().await = None;
            Ok(UpdateInfo {
                available: false,
                version: String::new(),
                current_version: handle.package_info().version.to_string(),
                notes: String::new(),
            })
        }
    }
}

/// Installs the pending update — reusing bytes from a prior [`update_download`] when present, else
/// downloading now — and clears the deferral flag. The BUILT-IN signature check runs during
/// download/install, so a bad artifact fails here rather than being installed. Does NOT relaunch (the
/// caller decides): the immediate path restarts, the quit path lets the new bundle take effect on the
/// next launch. Returns `Err` on any failure, leaving the app on its current version.
async fn install_update(handle: &AppHandle, state: &UpdateState, app: &App) -> Result<(), String> {
    // Ensure an Update is available to act on: reuse the last check's, else check now (the quiet launch
    // check usually already stashed one). Bind the emptiness to a `bool` so the guard is released at the
    // `;` before `check_into` re-locks the same mutex (a held guard across the check would deadlock).
    let needs_check = state.current.lock().await.is_none();
    if needs_check {
        check_into(handle, state).await?;
    }
    let current = state.current.lock().await;
    let update = current
        .as_ref()
        .ok_or_else(|| "no update available to install".to_string())?;
    // Consume any pre-downloaded bytes; otherwise download+install in one step (both verify the signature).
    let pre_downloaded = state.downloaded.lock().await.take();
    match pre_downloaded {
        Some(bytes) => update.install(bytes).map_err(|e| e.to_string())?,
        None => update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|e| e.to_string())?,
    }
    drop(current);
    // The install landed; clear the pending flag so it is not re-attempted on the next quit.
    app.set_pending_update(false)?;
    Ok(())
}

/// Check for an update on demand (the Settings "Check for updates" action). Stashes any found update so a
/// follow-up [`update_download`]/[`update_install`] reuses it. Errors (offline, bad manifest) surface to
/// the UI.
#[tauri::command]
pub async fn update_check(
    handle: AppHandle,
    state: State<'_, UpdateState>,
) -> Result<UpdateInfo, String> {
    check_into(&handle, &state).await
}

/// Download the checked update, emitting [`EVENT_DOWNLOAD_PROGRESS`] per chunk so the UI can show a
/// progress bar, and caching the verified bytes for a subsequent [`update_install`]. Requires a prior
/// [`update_check`] (or the quiet launch check) to have found an update. Downloading does NOT touch the
/// running app, so it is safe regardless of active runs — only install/relaunch is guarded.
#[tauri::command]
pub async fn update_download(
    handle: AppHandle,
    state: State<'_, UpdateState>,
) -> Result<(), String> {
    let current = state.current.lock().await;
    let update = current
        .as_ref()
        .ok_or_else(|| "no update available to download (check for updates first)".to_string())?;
    let emitter = handle.clone();
    let mut downloaded_total: u64 = 0;
    let on_chunk = move |chunk: usize, total: Option<u64>| {
        downloaded_total = downloaded_total.saturating_add(chunk as u64);
        // Best-effort progress: a dropped event never fails the download.
        let _ = emitter.emit(
            EVENT_DOWNLOAD_PROGRESS,
            DownloadProgress {
                downloaded: downloaded_total,
                total,
            },
        );
    };
    let bytes = update
        .download(on_chunk, || {})
        .await
        .map_err(|e| e.to_string())?;
    drop(current);
    *state.downloaded.lock().await = Some(bytes);
    Ok(())
}

/// Install the update and relaunch into it — UNLESS runs are active and the caller did not `force`, in
/// which case it refuses, records a pending flag (so [`install_pending_on_quit`] installs on the next
/// graceful quit), and returns the blocking run count. The relaunch (`AppHandle::restart`, the process
/// plugin's primitive) diverges, so on the allowed path this never returns normally.
#[tauri::command]
pub async fn update_install(
    app: State<'_, App>,
    handle: AppHandle,
    state: State<'_, UpdateState>,
    force: bool,
) -> Result<InstallReport, String> {
    let active = app.active_run_count().await;
    if !may_install_now(active, force) {
        // Refuse the restart now; defer to the next graceful quit when work has drained.
        app.set_pending_update(true)?;
        return Ok(InstallReport {
            installed: false,
            blocked_active_runs: active,
        });
    }
    install_update(&handle, &state, &app).await?;
    // Relaunch into the freshly-installed version. `restart` diverges (`-> !`), coercing to the return
    // type; the app is replaced before any value is produced.
    handle.restart()
}

/// The live active-run count for the UI (so it can warn "N runs active — installing will restart the app"
/// before the user confirms). Same value the install guard consults; 0 when the daemon is not running.
#[tauri::command]
pub async fn active_run_count(app: State<'_, App>) -> Result<i64, String> {
    Ok(app.active_run_count().await)
}

/// The quiet, non-blocking on-launch check: spawns a task that checks for an update and, if one exists,
/// stashes it and emits [`EVENT_UPDATE_AVAILABLE`] so the UI can badge the affordance. Every failure is
/// swallowed (logged) — a missing network or unreachable update server must never disrupt launch.
pub fn spawn_launch_check(handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Some(state) = handle.try_state::<UpdateState>() else {
            return;
        };
        match check_into(&handle, state.inner()).await {
            Ok(info) if info.available => {
                if let Err(e) = handle.emit(EVENT_UPDATE_AVAILABLE, info) {
                    eprintln!("rhapsody-desktop: could not emit the update-available event: {e}");
                }
            }
            Ok(_) => {} // already up to date
            Err(e) => {
                eprintln!("rhapsody-desktop: on-launch update check failed (ignored): {e}")
            }
        }
    });
}

/// Honors the "install on next graceful quit" flag: if an install is pending, install the update so the
/// swapped bundle takes effect on the next launch, then let the quit proceed normally (no surprise
/// relaunch — the daemon has already drained). Best-effort and time-bounded: on any failure or timeout it
/// clears the flag and returns so the quit is never perpetually slowed retrying a broken install. A no-op
/// when nothing is pending. Call this only AFTER the daemon has drained, so nothing races live work.
pub async fn install_pending_on_quit(handle: AppHandle, app: App) {
    if !app.pending_update() {
        return;
    }
    let Some(state) = handle.try_state::<UpdateState>() else {
        return;
    };
    match tokio::time::timeout(
        QUIT_INSTALL_TIMEOUT,
        install_update(&handle, state.inner(), &app),
    )
    .await
    {
        Ok(Ok(())) => {} // installed; the new bundle is used on the next launch
        Ok(Err(e)) => {
            eprintln!(
                "rhapsody-desktop: pending update install on quit failed (proceeding to quit): {e}"
            );
            let _ = app.set_pending_update(false);
        }
        Err(_) => {
            eprintln!(
                "rhapsody-desktop: pending update install on quit timed out (proceeding to quit)"
            );
            let _ = app.set_pending_update(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The guard is the safety core: with runs active an unforced install is refused (deferred), and only
    // `force` or a zero count lets it proceed to restart. Mirrors the spec's "never silently restart with
    // active runs".
    #[test]
    fn may_install_now_blocks_active_runs_unless_forced() {
        assert!(may_install_now(0, false), "idle → may install");
        assert!(may_install_now(0, true), "idle + force → may install");
        assert!(
            !may_install_now(1, false),
            "one active run, no force → must refuse (defer to quit)"
        );
        assert!(
            !may_install_now(5, false),
            "several active runs, no force → must refuse"
        );
        assert!(
            may_install_now(5, true),
            "force overrides active runs (explicit user override)"
        );
    }

    // A negative count (an impossible/garbled probe) is treated as idle, never as "block" — the guard must
    // not wedge the updater on a bad reading; safety comes from the explicit `force`, not from trusting 0.
    #[test]
    fn may_install_now_treats_nonpositive_as_idle() {
        assert!(may_install_now(-1, false));
    }
}
