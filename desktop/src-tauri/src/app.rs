//! The desktop app object: owns the daemon supervisor and the quit/close lifecycle the tray and
//! window drive. Parity port of the lifecycle half of `$REF/desktop/app.go` (the Wails `App`):
//!   - hide-on-close vs. quit: closing the window hides it (tray + daemon keep running); quitting
//!     shows a "Shutting down…" overlay and drains the daemon OFF the main thread
//!     (`OnBeforeClose` / `beginShutdown` / `drainDaemon` / `OnShutdown` semantics).
//!   - `start_daemon` refuses without a WORKFLOW.md (`StartDaemon`); a Start on a stopped daemon
//!     rebuilds the supervisor first so fresh tool overrides take effect (`refreshStoppedSupervisor`).
//!
//! It deliberately has NO Tauri dependency, so the lifecycle is unit-testable headlessly (the bin's
//! `tray` module + the `run` event loop supply the Tauri effects: emit the overlay event, prevent the
//! exit, and re-issue it once the drain completes). The credential/tool-override inputs to
//! `make_supervisor` are wired in P7-D4; here they default (empty key, no overrides).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

use crate::menu::{MenuModel, menu_from_status};
use crate::supervisor::{
    DaemonOutput, Options, State, Status, Supervisor, resolve_binary, resources_dir_for,
};
use crate::tooldirs::agent_tool_dirs;

/// A one-shot broadcast the single stop task "closes" when the drain completes. Created on the FIRST
/// close; re-entrant closes and `on_shutdown` wait on the SAME signal instead of issuing a second
/// Stop. Mirrors Go's `stopDone chan struct{}` (a retained receiver keeps the channel live so a
/// `close`/`send` always lands, exactly as [`crate::supervisor`]'s `SupRun` does).
struct StopSignal {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl StopSignal {
    fn new() -> Arc<Self> {
        let (tx, rx) = watch::channel(false);
        Arc::new(StopSignal { tx, rx })
    }

    /// Releases every waiter (Go's `close(stopDone)`). Idempotent.
    fn close(&self) {
        let _ = self.tx.send(true);
    }

    /// Non-blocking "has the drain finished?" (Go's `select { case <-done: … default: … }`).
    fn is_closed(&self) -> bool {
        *self.rx.borrow()
    }

    /// Completes once closed (returns at once if already closed). Clones the retained receiver so the
    /// channel stays open for `close`.
    async fn wait(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// Recovers a poisoned lock rather than propagating the panic — the guarded sections are tiny field
/// reads/writes, never held across an `.await`, so a poisoned value is still consistent (same policy
/// as [`crate::supervisor`]).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a background Start is in flight for `sup` (`m.starting_sup == sup` by `Arc` identity).
/// Mirrors the Go `a.startingSup == cur` / `a.startingSup == sup` guards.
fn starting_for(m: &Mutable, sup: &Supervisor) -> bool {
    m.starting_sup.as_ref().is_some_and(|s| s.ptr_eq(sup))
}

/// The mutable app state guarded by one mutex — mirrors Go's single `a.mu` guarding `sup`,
/// `startingSup`, `overrides`, and the shutdown flag together.
struct Mutable {
    /// The current supervisor (RestartDaemon reassigns it to pick up new overrides).
    sup: Option<Supervisor>,
    /// The supervisor a background Start is in flight for (so a rebuild never orphans a live launch).
    starting_sup: Option<Supervisor>,
    /// Tool-doctor per-tool path overrides (name -> path); wired by P7-D4, empty here.
    overrides: HashMap<String, String>,
    /// `Some` once shutdown is underway — collapses Go's `shuttingDown` bool + `stopDone` channel
    /// (always set together) into one field. The single stop task closes it when the drain completes.
    stop_done: Option<Arc<StopSignal>>,
}

struct AppInner {
    /// The WORKFLOW.md the app supervises (`None` when `$HOME` is unset); its existence == configured.
    workflow_path: Option<PathBuf>,
    /// The resolved `rhapsodyd` sidecar path (empty until resolved), passed to each supervisor.
    binary_path: PathBuf,
    mu: Mutex<Mutable>,
    /// Short-timeout client for the `/api/v1/state` agent-count probe (Go's `http.DefaultClient`).
    http: reqwest::Client,
}

/// Owns the daemon supervisor and the app lifecycle. Cheap to [`Clone`] (an `Arc` handle); all
/// methods are safe for concurrent use. Mirrors the lifecycle surface of Go `*App`.
#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

/// What the `run` event loop must do for a quit (`ExitRequested`). Mirrors the three outcomes of Go
/// `OnBeforeClose`: prevent + start the drain, prevent + wait, or let the exit proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// First close: prevent the exit, show the "Shutting down…" overlay, and drain off the main
    /// thread; when the drain completes, re-issue the exit (which resolves to [`Self::Proceed`]).
    StartDrain,
    /// Re-entrant close while the drain is still in flight: prevent the exit, keep the overlay — never
    /// tear down mid-drain.
    WaitForDrain,
    /// Re-entrant close after the drain completed: let the exit proceed, or the app hangs on the
    /// overlay forever.
    Proceed,
}

impl CloseDecision {
    /// Whether the quit must be vetoed (Go `OnBeforeClose`'s `prevent` return).
    pub fn prevents(self) -> bool {
        !matches!(self, CloseDecision::Proceed)
    }
}

/// Why a tray/UI `start_daemon` (or `restart_daemon`) could not proceed. Mirrors the two Go
/// `fmt.Errorf` refusals in `StartDaemon`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDaemonError {
    /// The supervisor was never initialized (OnStartup did not run).
    NotInitialized,
    /// No WORKFLOW.md exists yet — the daemon would only fail startup validation.
    NotConfigured,
}

impl std::fmt::Display for StartDaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartDaemonError::NotInitialized => f.write_str("supervisor not initialized"),
            StartDaemonError::NotConfigured => f.write_str(
                "not configured: create a WORKFLOW.md (finish onboarding) before starting the daemon",
            ),
        }
    }
}

impl std::error::Error for StartDaemonError {}

/// The frontend-facing status snapshot. Mirrors Go `StatusDTO` (`$REF/desktop/app.go`); the serde
/// field names match its json tags so the webview shell sees the identical shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusDto {
    pub state: String,
    pub pid: i64,
    pub restarts: i64,
    pub last_err: String,
    pub url: String,
    pub healthy: bool,
    pub agent_count: i64,
    pub configured: bool,
}

impl App {
    /// Builds the app from the resolved sidecar + workflow paths (the OnStartup inputs). The
    /// supervisor is wired by [`App::on_startup`]; tests set it directly via [`App::set_sup`].
    pub fn new(workflow_path: Option<PathBuf>, binary_path: PathBuf) -> App {
        // A short-timeout client for the agent-count probe; builder failure is practically impossible
        // for this http-only client, so fall back to the default rather than panic (errors are values).
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        App {
            inner: Arc::new(AppInner {
                workflow_path,
                binary_path,
                mu: Mutex::new(Mutable {
                    sup: None,
                    starting_sup: None,
                    overrides: HashMap::new(),
                    stop_done: None,
                }),
                http,
            }),
        }
    }

    /// Builds the app from the environment — the OnStartup path resolution: the WORKFLOW.md
    /// (`SYMPHONY_WORKFLOW` override, else `~/.symphony/WORKFLOW.md`) and the `rhapsodyd` sidecar
    /// (`SYMPHONY_DAEMON` dev override, else the app bundle's `Resources`, else PATH). A missing
    /// sidecar is logged and left empty (the daemon simply cannot start until it is found), mirroring
    /// Go `OnStartup`'s `resolveWorkflowPath` + `resolveDaemonBinary`.
    pub fn from_env() -> App {
        App::new(resolve_workflow_path(), resolve_daemon_binary())
    }

    /// Builds the supervisor and — when configured with a resolved sidecar — kicks off the daemon in
    /// the background (so startup never blocks on the readiness wait). Mirrors Go `OnStartup`'s tail.
    pub fn on_startup(&self) {
        self.set_sup(self.make_supervisor());
        if self.configured() && !self.inner.binary_path.as_os_str().is_empty() {
            self.ensure_started();
        }
    }

    /// The current supervisor (a cheap `Arc` clone), or `None` before OnStartup. Mirrors `getSup`.
    pub fn get_sup(&self) -> Option<Supervisor> {
        lock(&self.inner.mu).sup.clone()
    }

    /// Installs `s` as the current supervisor. Mirrors `setSup`.
    pub fn set_sup(&self, s: Supervisor) {
        lock(&self.inner.mu).sup = Some(s);
    }

    /// A copy of the tool overrides under the lock. Mirrors `snapshotOverrides`.
    fn snapshot_overrides(&self) -> HashMap<String, String> {
        lock(&self.inner.mu).overrides.clone()
    }

    /// Builds a supervisor from the current paths + tool overrides (credential is P7-D4). The agent
    /// PATH is the override dirs first, then the known-good defaults. Mirrors `makeSupervisor`.
    pub fn make_supervisor(&self) -> Supervisor {
        let home = std::env::var("HOME").unwrap_or_default();
        Supervisor::new(Options {
            binary_path: self.inner.binary_path.clone(),
            workflow_path: self.inner.workflow_path.clone(),
            tool_dirs: Some(agent_tool_dirs(&home, &self.snapshot_overrides())),
            // The Wails shell wires os.Stderr so the sidecar's logs reach the app's stderr.
            daemon_output: DaemonOutput::Inherit,
            ..Options::default()
        })
    }

    /// Reports whether a WORKFLOW.md exists to run. Mirrors Go `Configured` (`os.Stat` + `!IsDir`).
    pub fn configured(&self) -> bool {
        self.inner
            .workflow_path
            .as_deref()
            .is_some_and(path_is_file)
    }

    // ---- quit / close lifecycle -------------------------------------------------------------

    /// Marks shutdown underway and, on the FIRST call, allocates the [`StopSignal`] the single stop
    /// task closes when the drain completes. Returns `first=true` exactly once and the same signal so
    /// re-entrant closes block on the same drain. Mirrors `beginShutdown`.
    fn begin_shutdown(&self) -> (bool, Arc<StopSignal>) {
        let mut m = lock(&self.inner.mu);
        // `stop_done` present == shutdown already underway; return the SAME signal so re-entrant
        // closes wait on the in-flight drain instead of starting a second Stop.
        if let Some(done) = &m.stop_done {
            return (false, done.clone());
        }
        let done = StopSignal::new();
        m.stop_done = Some(done.clone());
        (true, done)
    }

    /// The in-flight stop signal, if a shutdown has begun.
    fn stop_done(&self) -> Option<Arc<StopSignal>> {
        lock(&self.inner.mu).stop_done.clone()
    }

    /// Stops the daemon (bounded by `timeout`) and then closes `stop_done` so any waiter (a re-entrant
    /// close, `on_shutdown`) is released. The single owner of the stop. Mirrors `drainDaemon`.
    pub async fn drain_daemon(&self, timeout: Duration) {
        if let Some(sup) = self.get_sup()
            && tokio::time::timeout(timeout, sup.stop()).await.is_err()
        {
            eprintln!("rhapsody-desktop: daemon did not stop within {timeout:?} on shutdown");
        }
        // Close stop_done unconditionally (Go's deferred `close`) so every waiter is released even if
        // the Stop timed out.
        if let Some(done) = self.stop_done() {
            done.close();
        }
    }

    /// The final-teardown backstop. When a drain is in flight it WAITS on `stop_done` (bounded) rather
    /// than issuing a fresh blocking Stop on the main thread; only a quit that bypassed the close hook
    /// (no drain started) runs its own Stop. Mirrors `OnShutdown`.
    pub async fn on_shutdown(&self) {
        if let Some(done) = self.stop_done() {
            // A drain is in flight (or finished): wait for it instead of starting another Stop.
            if tokio::time::timeout(Duration::from_secs(10), done.wait())
                .await
                .is_err()
            {
                eprintln!(
                    "rhapsody-desktop: daemon drain did not complete before shutdown timeout"
                );
            }
            return;
        }
        // No drain was started (the close hook was bypassed) — do the stop here.
        if let Some(sup) = self.get_sup()
            && tokio::time::timeout(Duration::from_secs(10), sup.stop())
                .await
                .is_err()
        {
            eprintln!("rhapsody-desktop: daemon stop on shutdown timed out");
        }
    }

    /// Decides what the quit path must do (see [`CloseDecision`]). Pure + headless-testable; the Tauri
    /// effects (emit the overlay, prevent/re-issue the exit) live in the `run` loop. Mirrors the
    /// decision in Go `OnBeforeClose`.
    pub fn on_before_close(&self) -> CloseDecision {
        let (first, done) = self.begin_shutdown();
        if !first {
            // Re-entrant close: proceed once the drain completed (let the final quit through), else
            // keep vetoing so the overlay stays up and nothing tears down mid-drain.
            return if done.is_closed() {
                CloseDecision::Proceed
            } else {
                CloseDecision::WaitForDrain
            };
        }
        CloseDecision::StartDrain
    }

    // ---- start / stop / restart -------------------------------------------------------------

    /// Starts the daemon on demand (tray/UI). Refuses when there is no WORKFLOW.md (matching the tray
    /// gating), and — when stopped — rebuilds the supervisor first so a fresh override takes effect on
    /// Start, not only Restart. A no-op if already running/starting. Mirrors `StartDaemon`.
    pub fn start_daemon(&self) -> Result<(), StartDaemonError> {
        if self.get_sup().is_none() {
            return Err(StartDaemonError::NotInitialized);
        }
        if !self.configured() {
            return Err(StartDaemonError::NotConfigured);
        }
        self.refresh_stopped_supervisor();
        self.ensure_started();
        Ok(())
    }

    /// Swaps in a freshly-built supervisor ONLY when the daemon is fully stopped and no Start is in
    /// flight, so a Start after a new override launches with the override dir on the agent PATH. Never
    /// replaces a running/starting supervisor (that would orphan the live process). Mirrors
    /// `refreshStoppedSupervisor`.
    pub fn refresh_stopped_supervisor(&self) {
        // Inspect the current supervisor under the lock; bail unless it is fully stopped with no Start
        // in flight for it.
        let cur = {
            let m = lock(&self.inner.mu);
            match &m.sup {
                Some(cur) if !starting_for(&m, cur) && cur.status().state == State::Stopped => {
                    cur.clone()
                }
                _ => return,
            }
        };
        let fresh = self.make_supervisor(); // reads overrides without the lock held
        let mut m = lock(&self.inner.mu);
        // Re-check under the lock: only swap if the supervisor we inspected is still current, still
        // stopped, and still has no Start in flight — so a racing ensure_started/restart is never
        // clobbered.
        if let Some(existing) = &m.sup
            && existing.ptr_eq(&cur)
            && !starting_for(&m, &cur)
            && existing.status().state == State::Stopped
        {
            m.sup = Some(fresh);
        }
    }

    /// Launches the CURRENT supervisor in the background if a Start is not already in flight for it.
    /// If the supervisor is swapped out while starting, the now-orphan launch is stopped. Mirrors
    /// `ensureStarted`.
    pub fn ensure_started(&self) {
        let sup = {
            let mut m = lock(&self.inner.mu);
            let sup = match &m.sup {
                Some(s) => s.clone(),
                None => return,
            };
            if starting_for(&m, &sup) {
                return;
            }
            m.starting_sup = Some(sup.clone());
            sup
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            // Start blocks until healthy or it gives up; a never-completing cancel matches Go's
            // context.Background() — the supervisor's own lifetime is governed by Stop.
            let _ = sup.start(std::future::pending::<()>()).await;
            {
                let mut m = lock(&inner.mu);
                if starting_for(&m, &sup) {
                    m.starting_sup = None;
                }
            }
            // Swapped out while we were starting it → now an orphan; stop it so a stale daemon does not
            // keep running on its own loopback port while the UI tracks the replacement.
            let still_current = lock(&inner.mu).sup.as_ref().is_some_and(|c| c.ptr_eq(&sup));
            if !still_current {
                let _ = tokio::time::timeout(Duration::from_secs(10), sup.stop()).await;
            }
        });
    }

    /// Stops the daemon. Mirrors `StopDaemon`.
    pub async fn stop_daemon(&self) {
        if let Some(sup) = self.get_sup() {
            let _ = tokio::time::timeout(Duration::from_secs(10), sup.stop()).await;
        }
    }

    /// Stops the daemon and starts a freshly-built supervisor, so any new tool overrides take effect.
    /// Mirrors `RestartDaemon`.
    pub async fn restart_daemon(&self) -> Result<(), StartDaemonError> {
        let sup = self.get_sup().ok_or(StartDaemonError::NotInitialized)?;
        let _ = tokio::time::timeout(Duration::from_secs(15), sup.stop()).await;
        self.set_sup(self.make_supervisor());
        self.ensure_started();
        Ok(())
    }

    // ---- status ------------------------------------------------------------------------------

    /// The current status snapshot for the UI/tray. Mirrors Go `Status`: probes health + the live
    /// agent count only while Running.
    pub async fn status(&self) -> StatusDto {
        let sup = match self.get_sup() {
            Some(s) => s,
            None => {
                return StatusDto {
                    state: State::Stopped.as_str().to_string(),
                    pid: 0,
                    restarts: 0,
                    last_err: String::new(),
                    url: String::new(),
                    healthy: false,
                    agent_count: 0,
                    configured: self.configured(),
                };
            }
        };
        let st = sup.status();
        let mut dto = StatusDto {
            state: st.state.as_str().to_string(),
            pid: i64::from(st.pid),
            restarts: st.restarts,
            last_err: st.last_err.clone(),
            url: sup.url(),
            healthy: false,
            agent_count: 0,
            configured: self.configured(),
        };
        if st.state == State::Running {
            dto.healthy = sup.healthy().await;
            dto.agent_count = self.agent_count(&sup).await;
        }
        dto
    }

    /// The tray's rendered menu model for the current status + live agent count. Mirrors Go
    /// `applyTray`'s status → [`menu_from_status`] mapping.
    pub async fn tray_menu_model(&self) -> MenuModel {
        let (st, agents) = match self.get_sup() {
            Some(sup) => {
                let st = sup.status();
                let agents = if st.state == State::Running {
                    self.agent_count(&sup).await
                } else {
                    0
                };
                (st, agents)
            }
            None => (
                Status {
                    state: State::Stopped,
                    pid: 0,
                    restarts: 0,
                    last_err: String::new(),
                },
                0,
            ),
        };
        menu_from_status(&st, agents, self.configured())
    }

    /// Fetches the live running-agent count from the daemon's `/api/v1/state`; 0 on any error (the
    /// daemon may be momentarily unavailable). Mirrors Go `agentCount`.
    async fn agent_count(&self, sup: &Supervisor) -> i64 {
        let url = format!("{}/api/v1/state", sup.url());
        let resp = match self.inner.http.get(&url).send().await {
            Ok(r) => r,
            Err(_) => return 0,
        };
        if resp.status() != reqwest::StatusCode::OK {
            return 0;
        }
        #[derive(serde::Deserialize)]
        struct StateBody {
            counts: HashMap<String, i64>,
        }
        match resp.json::<StateBody>().await {
            Ok(body) => body.counts.get("running").copied().unwrap_or(0),
            Err(_) => 0,
        }
    }
}

/// Resolves the WORKFLOW.md the app supervises: a `SYMPHONY_WORKFLOW` override (dev), else
/// `~/.symphony/WORKFLOW.md`. `HOME` is read directly, matching `os.UserHomeDir` on macOS. Mirrors Go
/// `resolveWorkflowPath`.
fn resolve_workflow_path() -> Option<PathBuf> {
    resolve_workflow_path_from(
        std::env::var("SYMPHONY_WORKFLOW").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure resolver for [`resolve_workflow_path`], taking the env values so it is unit-testable without
/// mutating the process environment. A non-empty override wins; otherwise a non-empty home yields
/// `<home>/.symphony/WORKFLOW.md`; an empty/absent home (Go's `os.UserHomeDir` error) yields `None`.
fn resolve_workflow_path_from(
    workflow_override: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = workflow_override
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    match home {
        Some(h) if !h.is_empty() => Some(Path::new(h).join(".symphony").join("WORKFLOW.md")),
        _ => None,
    }
}

/// Locates the `rhapsodyd` sidecar: a `SYMPHONY_DAEMON` dev override, else the app bundle's
/// `Resources`, else PATH. Returns an empty path (logged) when none is found — the daemon simply
/// cannot start until it is present. Mirrors Go `resolveDaemonBinary`.
fn resolve_daemon_binary() -> PathBuf {
    let over = std::env::var("SYMPHONY_DAEMON").unwrap_or_default();
    let resources = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.to_str().and_then(resources_dir_for))
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    match resolve_binary(&over, &resources) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rhapsody-desktop: could not locate rhapsodyd sidecar: {e}");
            PathBuf::new()
        }
    }
}

/// Reports whether `path` names an existing non-directory, matching Go's `os.Stat` + `!info.IsDir()`
/// in `Configured`.
fn path_is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| !m.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(None, PathBuf::new())
    }

    #[cfg(test)]
    impl App {
        /// Test hook: simulate a background Start in flight for `s` (Go tests set `a.startingSup`).
        fn set_starting_sup_for_test(&self, s: Option<Supervisor>) {
            lock(&self.inner.mu).starting_sup = s;
        }
    }

    // Only the FIRST close may spawn the stop task: begin_shutdown returns first=true exactly once;
    // every later call returns false and the SAME signal, so a re-entrant close waits on the in-flight
    // drain instead of starting a second Stop. Mirrors Go `TestBeginShutdownIsFirstOnlyOnce`.
    #[test]
    fn begin_shutdown_reports_first_only_once() {
        let a = test_app();
        let (first, done) = a.begin_shutdown();
        assert!(first, "first begin_shutdown should report first=true");
        for i in 0..3 {
            let (again, done2) = a.begin_shutdown();
            assert!(
                !again,
                "call {i}: begin_shutdown reported first=true again; only the first close may trigger the stop"
            );
            assert!(
                Arc::ptr_eq(&done, &done2),
                "call {i}: begin_shutdown returned a different signal; re-entrant closes must wait on the same drain"
            );
        }
    }

    // The single stop task releases all waiters: after drain_daemon runs, stop_done is closed so
    // on_shutdown / re-entrant closes unblock. Mirrors Go `TestDrainDaemonClosesStopDone`.
    #[tokio::test]
    async fn drain_daemon_closes_stop_done() {
        let a = test_app();
        a.set_sup(Supervisor::new(Options::default()));
        let (_first, done) = a.begin_shutdown();
        a.drain_daemon(Duration::from_secs(1)).await;
        assert!(
            done.is_closed(),
            "drain_daemon must close stop_done so waiters are released"
        );
    }

    // Once a drain is in flight (stop_done set), on_shutdown must WAIT on it rather than issue a fresh
    // blocking Stop. Here stop_done is already closed, so it must return promptly. Mirrors Go
    // `TestOnShutdownWaitsForExistingDrain`.
    #[tokio::test]
    async fn on_shutdown_waits_for_existing_drain() {
        let a = test_app();
        a.set_sup(Supervisor::new(Options::default()));
        let (_first, done) = a.begin_shutdown();
        done.close(); // the drain task already finished
        tokio::time::timeout(Duration::from_secs(2), a.on_shutdown())
            .await
            .expect("on_shutdown must return promptly once the drain is done (must not block on a fresh Stop)");
    }

    // The bypass path: a quit that skipped the close hook leaves stop_done unset, so on_shutdown must
    // perform the stop itself. With a stopped supervisor Stop is idempotent, so it simply returns.
    // Mirrors Go `TestOnShutdownStopsWhenNoDrainStarted`.
    #[tokio::test]
    async fn on_shutdown_stops_when_no_drain_started() {
        let a = test_app();
        a.set_sup(Supervisor::new(Options::default()));
        tokio::time::timeout(Duration::from_secs(2), a.on_shutdown())
            .await
            .expect("on_shutdown should run its own Stop promptly when no drain was started");
    }

    // The drain task finishes and re-issues the quit, which re-enters the close path; it must resolve
    // to Proceed (prevent=false) or no quit can ever finish and the app hangs on the overlay. Mirrors
    // Go `TestOnBeforeCloseAllowsFinalQuitAfterDrain`.
    #[test]
    fn on_before_close_allows_final_quit_after_drain() {
        let a = test_app();
        let (_first, done) = a.begin_shutdown();
        done.close(); // the drain task already finished
        assert!(
            !a.on_before_close().prevents(),
            "on_before_close must not prevent once the drain completed — else the final quit is vetoed and the app strands on the overlay"
        );
    }

    // A re-entrant close while the drain is STILL RUNNING must prevent — keep the overlay rendered and
    // never let the app tear down mid-drain. Mirrors Go `TestOnBeforeCloseVetoesWhileDrainInFlight`.
    #[test]
    fn on_before_close_vetoes_while_drain_in_flight() {
        let a = test_app();
        a.begin_shutdown(); // stop_done open: drain in flight
        assert!(
            a.on_before_close().prevents(),
            "on_before_close must prevent while the drain is in flight (mid-drain teardown race)"
        );
    }

    // Per the README, the app must not launch rhapsodyd until a WORKFLOW.md exists. Mirrors Go
    // `TestStartDaemonRefusesWhenNotConfigured`.
    #[test]
    fn start_daemon_refuses_when_not_configured() {
        let absent = std::env::temp_dir()
            .join(format!("rhapsody-d3-{}", std::process::id()))
            .join("absent")
            .join("WORKFLOW.md"); // does not exist
        let a = App::new(Some(absent), PathBuf::new());
        a.set_sup(Supervisor::new(Options::default()));
        assert!(
            a.start_daemon().is_err(),
            "start_daemon should refuse when not configured (no WORKFLOW.md)"
        );
    }

    // A Start on a stopped daemon rebuilds the (stopped) supervisor from current config before
    // launching, so tool overrides set since OnStartup take effect. Mirrors Go
    // `TestRefreshStoppedSupervisorRebuildsWhenStopped`.
    #[test]
    fn refresh_stopped_supervisor_rebuilds_when_stopped() {
        let a = test_app();
        a.set_sup(a.make_supervisor());
        let orig = a.get_sup().expect("sup set");
        a.refresh_stopped_supervisor();
        let after = a.get_sup().expect("sup set");
        assert!(
            !after.ptr_eq(&orig),
            "refresh_stopped_supervisor must rebuild the stopped supervisor so a Start picks up fresh overrides"
        );
    }

    // It must never replace a supervisor a background Start is already in flight for (that would
    // orphan the live/launching process). Mirrors Go `TestRefreshStoppedSupervisorSkipsWhenStartInFlight`.
    #[test]
    fn refresh_stopped_supervisor_skips_when_start_in_flight() {
        let a = test_app();
        a.set_sup(a.make_supervisor());
        let orig = a.get_sup().expect("sup set");
        a.set_starting_sup_for_test(Some(orig.clone())); // a background Start in flight for this sup
        a.refresh_stopped_supervisor();
        let after = a.get_sup().expect("sup set");
        assert!(
            after.ptr_eq(&orig),
            "refresh_stopped_supervisor must not replace a supervisor with a start in flight"
        );
    }

    // A non-empty SYMPHONY_WORKFLOW override wins over the home default. Mirrors the D1 status resolver
    // tests (kept here now that App owns the workflow path).
    #[test]
    fn resolve_prefers_a_non_empty_override() {
        assert_eq!(
            resolve_workflow_path_from(Some("/tmp/custom/WORKFLOW.md"), Some("/home/u")),
            Some(PathBuf::from("/tmp/custom/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_ignores_an_empty_override_and_uses_home() {
        assert_eq!(
            resolve_workflow_path_from(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.symphony/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_defaults_to_home_when_no_override() {
        assert_eq!(
            resolve_workflow_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.symphony/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_is_none_without_override_or_home() {
        assert_eq!(resolve_workflow_path_from(None, None), None);
        assert_eq!(resolve_workflow_path_from(None, Some("")), None);
    }

    // path_is_file / App::configured agree with Go's os.Stat + !IsDir: a regular file is configured, a
    // directory or missing path is not.
    #[test]
    fn configured_true_for_a_regular_file_false_for_dir_or_missing() {
        let dir = std::env::temp_dir().join(format!("rhapsody-d3-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("WORKFLOW.md");
        std::fs::write(&file, b"---\n").expect("write temp file");

        assert!(path_is_file(&file), "a regular file is configured");
        assert!(!path_is_file(&dir), "a directory is not");
        assert!(!path_is_file(&dir.join("nope")), "a missing path is not");

        assert!(
            App::new(Some(file), PathBuf::new()).configured(),
            "App::configured is true when the workflow file exists"
        );
        assert!(
            !App::new(Some(dir.join("nope")), PathBuf::new()).configured(),
            "App::configured is false when the workflow file is missing"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
