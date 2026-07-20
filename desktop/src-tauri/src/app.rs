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
//! exit, and re-issue it once the drain completes). P7-D4 adds the settings surface — the Keychain
//! credential store, tool-doctor overrides (prefs), the Linear project picker, onboarding config
//! write-back, and the tool doctor — wiring the stored token + override dirs into `make_supervisor`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

use crate::menu::{MenuModel, menu_from_status};
use crate::supervisor::{
    DaemonOutput, Options, State, Status, Supervisor, is_executable_file, resolve_binary,
    resources_dir_for,
};
use crate::tooldirs::agent_tool_dirs;
use crate::{credential, linearoauth, linearprojects, onboarding, prefs, toolcheck};

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
    /// Tool-doctor per-tool path overrides (name -> path); loaded from prefs at [`App::on_startup`].
    overrides: HashMap<String, String>,
    /// The active Linear-token store: `None` until [`App::on_startup`] resolves the backend (Go's
    /// `a.cred == nil`), then the Keychain or (fallback) file store. Swapped by set/clear.
    cred: Option<Arc<dyn credential::Store>>,
    /// `Some` once shutdown is underway — collapses Go's `shuttingDown` bool + `stopDone` channel
    /// (always set together) into one field. The single stop task closes it when the drain completes.
    stop_done: Option<Arc<StopSignal>>,
}

struct AppInner {
    /// The WORKFLOW.md the app supervises (`None` when `$HOME` is unset); its existence == configured.
    workflow_path: Option<PathBuf>,
    /// The resolved `rhapsodyd` sidecar path (empty until resolved), passed to each supervisor.
    binary_path: PathBuf,
    /// Where tool-doctor overrides persist (`~/.symphony/tools.json`); `None` when `$HOME` is unset.
    prefs_path: Option<PathBuf>,
    /// The "install the pending update on the next graceful quit" marker (`~/.symphony/pending-update`),
    /// co-located with the prefs so it lives with the app's other local state; `None` when `$HOME` is unset.
    /// Its mere existence is the flag (P11-U1): `update_install` writes it when it refuses an install
    /// because runs are active, and the quit path installs + relaunches when it is present.
    pending_update_path: Option<PathBuf>,
    /// The 0600 credential file fallback (`~/.symphony/credentials`), used when the Keychain is unusable.
    cred_path: PathBuf,
    /// The Keychain store the credential methods build fresh (Go's repeated `credential.New()`), behind
    /// an `Arc<dyn Store>` so tests inject an in-memory double instead of touching the login keychain.
    keychain: Arc<dyn credential::Store>,
    mu: Mutex<Mutable>,
    /// Serializes [`App::set_tool_override`]'s read-merge-swap-persist so two overlapping saves cannot
    /// each start from the same stale snapshot and drop one another's override (Go's `a.saveMu`).
    save_mu: Mutex<()>,
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

/// Reports whether a Linear token is stored, which backend holds it, and whether the deferred OAuth
/// path is available (spec §7). The serde field names match Go `CredentialStatusDTO`'s json tags so the
/// webview's `CredentialStatus` sees the identical shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialStatusDto {
    pub has_token: bool,
    pub backend: String,
    pub oauth_available: bool,
}

impl App {
    /// Builds the app from the resolved sidecar + workflow paths, with the production Keychain store and
    /// no prefs/cred paths — kept as the 2-arg constructor the lifecycle tests use. The supervisor is
    /// wired by [`App::on_startup`]; tests set it directly via [`App::set_sup`]. Credential-touching
    /// methods are not exercised through this constructor in tests, so the real Keychain is never hit.
    pub fn new(workflow_path: Option<PathBuf>, binary_path: PathBuf) -> App {
        App::new_with(
            workflow_path,
            binary_path,
            None,
            PathBuf::new(),
            Arc::new(credential::new()),
        )
    }

    /// The full constructor: also takes where tool-override prefs and the credential file fallback live,
    /// plus the Keychain store (production `credential::new()`, or a test double). Used by
    /// [`App::from_env`] with production wiring and by the credential tests with an in-memory keychain.
    pub fn new_with(
        workflow_path: Option<PathBuf>,
        binary_path: PathBuf,
        prefs_path: Option<PathBuf>,
        cred_path: PathBuf,
        keychain: Arc<dyn credential::Store>,
    ) -> App {
        // A short-timeout client for the agent-count probe; builder failure is practically impossible
        // for this http-only client, so fall back to the default rather than panic (errors are values).
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // The pending-update marker sits beside the prefs (same `~/.symphony` dir), so it shares the
        // prefs' `$HOME`-derived lifetime — no marker when there is nowhere to persist it.
        let pending_update_path = prefs_path
            .as_deref()
            .and_then(Path::parent)
            .map(|d| d.join("pending-update"));
        App {
            inner: Arc::new(AppInner {
                workflow_path,
                binary_path,
                prefs_path,
                pending_update_path,
                cred_path,
                keychain,
                mu: Mutex::new(Mutable {
                    sup: None,
                    starting_sup: None,
                    overrides: HashMap::new(),
                    cred: None,
                    stop_done: None,
                }),
                save_mu: Mutex::new(()),
                http,
            }),
        }
    }

    /// Builds the app from the environment — the OnStartup path resolution: the WORKFLOW.md
    /// (`SYMPHONY_WORKFLOW` override, else `~/.rhapsody/WORKFLOW.md`), the `rhapsodyd` sidecar
    /// (`SYMPHONY_DAEMON` dev override, else the app bundle's `Resources`, else PATH), the tool-override
    /// prefs (`~/.symphony/tools.json`), and the credential file fallback (`~/.symphony/credentials`).
    /// A missing sidecar is logged and left empty (the daemon simply cannot start until it is found),
    /// mirroring Go `OnStartup`'s resolvers.
    pub fn from_env() -> App {
        App::new_with(
            resolve_workflow_path(),
            resolve_daemon_binary(),
            resolve_prefs_path(),
            resolve_credential_path(),
            Arc::new(credential::new()),
        )
    }

    /// Loads persisted tool overrides + resolves the credential backend, then builds the supervisor and
    /// — when configured with a resolved sidecar — kicks off the daemon in the background (so startup
    /// never blocks on the readiness wait). Mirrors Go `OnStartup`.
    pub fn on_startup(&self) {
        // Load persisted tool overrides (a missing file yields an empty map) so the daemon's first
        // launch already has the user's override dirs on the agent PATH.
        if let Some(prefs_path) = &self.inner.prefs_path {
            match prefs::load_tool_overrides(prefs_path) {
                Ok(overrides) => lock(&self.inner.mu).overrides = overrides,
                Err(e) => eprintln!("rhapsody-desktop: could not load tool overrides: {e}"),
            }
        }
        // Resolve the credential backend (Keychain, or the file fallback when the Keychain is empty but
        // a previously-written fallback file exists) so the first supervisor launches with the token.
        let cred = resolve_credential_store(self.inner.keychain.clone(), &self.inner.cred_path);
        lock(&self.inner.mu).cred = Some(cred);

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

    /// Builds a supervisor from the current paths, tool overrides, and stored Linear token. The agent
    /// PATH is the override dirs first, then the known-good defaults; the token becomes the daemon's
    /// `$LINEAR_API_KEY` so it resolves `api_key: $LINEAR_API_KEY` without the user exporting anything.
    /// Mirrors `makeSupervisor`.
    pub fn make_supervisor(&self) -> Supervisor {
        let home = std::env::var("HOME").unwrap_or_default();
        Supervisor::new(Options {
            binary_path: self.inner.binary_path.clone(),
            workflow_path: self.inner.workflow_path.clone(),
            tool_dirs: Some(agent_tool_dirs(&home, &self.snapshot_overrides())),
            linear_api_key: self.linear_token(),
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

    /// The live daemon target for the window's same-origin API proxy, or `None` when the daemon is
    /// not usable (no supervisor yet, not Running, or an unbound/zero port). This is the per-request
    /// `base_url` resolver [`crate::windowserver`] hands to [`crate::apiproxy::handle`]; the usability
    /// core (Running + a real port) lives in [`crate::apiproxy::usable_base_url`]. Mirrors Go
    /// `App.daemonBaseURL` (`$REF/desktop/apiproxy.go`).
    pub fn daemon_base_url(&self) -> Option<String> {
        let sup = self.get_sup()?;
        crate::apiproxy::usable_base_url(sup.status().state, &sup.url())
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

    // ---- in-app auto-update (P11-U1) ---------------------------------------------------------------

    /// The number of runs the daemon is actively executing right now — the count the updater's install
    /// guard consults so a self-restart never kills work in flight. It is the same live `/api/v1/state`
    /// `counts.running` the tray already reads; a not-yet-started or non-Running supervisor (and any
    /// probe error) yields 0, the safe default (no supervisor → nothing to protect). Because a probe
    /// failure reads as 0, callers requiring safety MUST pair this with an explicit `force` override
    /// rather than trusting 0 to mean "definitely idle".
    pub async fn active_run_count(&self) -> i64 {
        match self.get_sup() {
            Some(sup) if sup.status().state == State::Running => self.agent_count(&sup).await,
            _ => 0,
        }
    }

    /// Whether an update install is pending for the next graceful quit (the marker file exists). `false`
    /// when there is nowhere to persist it (`$HOME` unset) or the file cannot be observed. Mirrors the
    /// prefs' "missing == default" convention.
    pub fn pending_update(&self) -> bool {
        self.inner
            .pending_update_path
            .as_deref()
            .is_some_and(Path::exists)
    }

    /// Records (or clears) the "install the pending update on next graceful quit" flag by creating or
    /// removing the marker file (its existence is the flag). A no-op when there is nowhere to persist it
    /// (`$HOME` unset) — the guard simply cannot defer in that case. Clearing an absent marker is not an
    /// error (idempotent), matching `os.Remove` + `IsNotExist` in the Go prefs idiom.
    pub fn set_pending_update(&self, pending: bool) -> Result<(), String> {
        let Some(path) = self.inner.pending_update_path.as_deref() else {
            return Ok(());
        };
        if pending {
            // A single byte written 0600 via the shared atomic writer (temp + rename), so a reader never
            // sees a torn marker and it is owner-only like the sibling prefs/credential files.
            crate::atomicfile::write_0600(path, b"1").map_err(|e| e.to_string())
        } else {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }

    // ---- credential (Linear token) ----------------------------------------------------------------

    /// The stored Linear token, or `$LINEAR_API_KEY` when none is stored (dev convenience). Fed to the
    /// daemon's launch env so it resolves `api_key: $LINEAR_API_KEY`. Mirrors `linearToken`.
    fn linear_token(&self) -> String {
        self.linear_token_with_env(env_linear_api_key().as_deref().unwrap_or(""))
    }

    /// The env-injected core of [`linear_token`], taking the `$LINEAR_API_KEY` value so tests need not
    /// mutate the process environment (Go's tests use `t.Setenv`, which Rust cannot do parallel-safely).
    /// A read error is NOT "no token": it returns "" WITHOUT falling back to the env var, so a stored
    /// credential that is momentarily unreadable is never masked by a stray dev key (it stays visible).
    fn linear_token_with_env(&self, env_token: &str) -> String {
        let cred = lock(&self.inner.mu).cred.clone();
        if let Some(cred) = cred {
            match cred.get() {
                Ok(token) if !token.is_empty() => return token,
                Ok(_) => {}
                Err(e) => {
                    eprintln!(
                        "rhapsody-desktop: reading the stored Linear token failed ({}); not falling back to $LINEAR_API_KEY: {e}",
                        cred.backend()
                    );
                    return String::new();
                }
            }
        }
        env_token.to_string()
    }

    /// Drives the credential panel: whether a token is stored, in which backend, and whether OAuth is
    /// available. Mirrors `CredentialStatus`.
    pub fn credential_status(&self) -> CredentialStatusDto {
        self.credential_status_with(
            env_linear_api_key().as_deref().unwrap_or(""),
            self.linear_oauth_available(),
        )
    }

    /// The env-injected core of [`credential_status`] (see [`linear_token_with_env`] for why). On a read
    /// error it reports the unreadable backend and does NOT promote `$LINEAR_API_KEY` — consistent with
    /// [`linear_token_with_env`], so the panel never shows a token the daemon will not actually use.
    fn credential_status_with(
        &self,
        env_token: &str,
        oauth_available: bool,
    ) -> CredentialStatusDto {
        let cred = lock(&self.inner.mu).cred.clone();
        let mut dto = CredentialStatusDto {
            has_token: false,
            backend: String::new(),
            oauth_available,
        };
        if let Some(cred) = cred {
            match cred.get() {
                Ok(token) if !token.is_empty() => {
                    dto.has_token = true;
                    dto.backend = cred.backend().to_string();
                    return dto;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!(
                        "rhapsody-desktop: reading the stored Linear token failed ({}): {e}",
                        cred.backend()
                    );
                    dto.backend = cred.backend().to_string();
                    return dto;
                }
            }
        }
        // Only when there is no stored credential at all do we report the dev env var — labelled "env"
        // so the UI does not offer a Keychain "Remove" that is a no-op against $LINEAR_API_KEY.
        if !env_token.is_empty() {
            dto.has_token = true;
            dto.backend = "env".to_string();
        }
        dto
    }

    /// Stores a pasted Linear token in the Keychain (falling back to a 0600 file if the Keychain rejects
    /// the write), clears the other backend, then rebuilds/restarts the daemon so it picks up the new
    /// credential. Mirrors `SetLinearToken`.
    pub async fn set_linear_token(&self, token: &str) -> Result<(), String> {
        let token = token.trim();
        if token.is_empty() {
            return Err("token is empty".to_string());
        }
        let kc = self.inner.keychain.clone();
        let file: Arc<dyn credential::Store> =
            Arc::new(credential::new_file(&self.inner.cred_path));
        let active: Arc<dyn credential::Store> = match kc.set(token) {
            Ok(()) => kc.clone(),
            Err(e) => {
                eprintln!(
                    "rhapsody-desktop: keychain write failed; falling back to a 0600 file store: {e}"
                );
                file.set(token)
                    .map_err(|fe| format!("store token (keychain: {e}; file: {fe})"))?;
                file.clone()
            }
        };
        // Clear the OTHER backend so a token never lingers in two places (an old Keychain entry after a
        // file fallback, or a stale file after a later successful Keychain write).
        let other = if active.backend() == "keychain" {
            file
        } else {
            kc
        };
        if let Err(e) = other.delete() {
            eprintln!(
                "rhapsody-desktop: could not clear the inactive credential backend ({}); a stale token may remain: {e}",
                other.backend()
            );
        }
        lock(&self.inner.mu).cred = Some(active);
        self.apply_credential_change().await
    }

    /// Rebuilds the supervisor so its launch env reflects the current stored credential, restarting the
    /// daemon if it was already running. Rebuilding even while stopped is essential: the token is
    /// captured at `make_supervisor` time, so without this a token saved before the first Start (the
    /// onboarding sequence) would never reach the daemon. Mirrors `applyCredentialChange`.
    async fn apply_credential_change(&self) -> Result<(), String> {
        let old = self.get_sup();
        let was_active = old
            .as_ref()
            .is_some_and(|s| s.status().state != State::Stopped);
        if let Some(old) = old
            && tokio::time::timeout(Duration::from_secs(10), old.stop())
                .await
                .is_err()
        {
            // The previous rhapsodyd may still be alive; installing + starting a new supervisor would
            // launch a SECOND instance on a different loopback port. Abort — the credential is already
            // persisted, so the next explicit Start/Restart picks it up once the old process is gone.
            eprintln!(
                "rhapsody-desktop: not rebuilding the daemon: stopping the previous instance failed (a second instance would race the first)"
            );
            return Err("the credential change was applied, but the daemon could not be restarted to pick it up — click Restart (or quit and relaunch)".to_string());
        }
        self.set_sup(self.make_supervisor());
        if was_active && !self.inner.binary_path.as_os_str().is_empty() {
            self.ensure_started();
        }
        Ok(())
    }

    /// Revokes the stored token: deletes from BOTH backends (so a token can never be orphaned after a
    /// prior file fallback), resets to the Keychain backend, then rebuilds/restarts the daemon so the
    /// live process drops the now-revoked `$LINEAR_API_KEY`. Mirrors `ClearLinearToken`.
    pub async fn clear_linear_token(&self) -> Result<(), String> {
        let mut first_err: Option<String> = None;
        let file: Arc<dyn credential::Store> =
            Arc::new(credential::new_file(&self.inner.cred_path));
        for store in [self.inner.keychain.clone(), file] {
            if let Err(e) = store.delete()
                && first_err.is_none()
            {
                first_err = Some(e.to_string());
            }
        }
        // Reset to the default (Keychain) backend so a subsequent Set goes back through the Keychain.
        lock(&self.inner.mu).cred = Some(self.inner.keychain.clone());
        // A delete error is the more important signal, so only surface the restart-failure notice when
        // the revoke itself succeeded.
        if let Err(e) = self.apply_credential_change().await
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Reports whether the deferred "Connect Linear" OAuth flow can run (a client_id is configured).
    /// Always false in v1. Mirrors `LinearOAuthAvailable`.
    pub fn linear_oauth_available(&self) -> bool {
        linearoauth::configured(&std::env::var("SYMPHONY_LINEAR_CLIENT_ID").unwrap_or_default())
    }

    /// The "Connect Linear" button's action. The flow is scaffolded but its token exchange is deferred,
    /// so this returns a clear, non-fatal message instead of running it. Mirrors `StartLinearOAuth`.
    pub fn start_linear_oauth(&self) -> Result<(), String> {
        if !self.linear_oauth_available() {
            return Err("connect Linear isn't available yet: no client_id is configured — paste a Linear API token instead".to_string());
        }
        Err("the Linear OAuth flow is scaffolded but not yet enabled in this build".to_string())
    }

    /// Lists the workspace's Linear projects for the onboarding picker, using the token the wizard just
    /// saved. Queries Linear directly (the daemon + its config do not exist yet). Mirrors
    /// `ListLinearProjects`.
    pub async fn list_linear_projects(&self) -> Result<Vec<linearprojects::Project>, String> {
        let token = self.linear_token();
        if token.is_empty() {
            return Err("no Linear token saved — go back and re-enter your API key".to_string());
        }
        linearprojects::list(&token)
            .await
            .map_err(|e| e.to_string())
    }

    // ---- tool doctor ------------------------------------------------------------------------------

    /// Runs the Tool-doctor preflight: detect/version/health for claude, gh, gt, git across the
    /// known-good dirs plus any per-tool overrides, searched in the daemon's agent-launch PATH order (so
    /// the doctor health-checks the same binary the daemon will resolve). Mirrors `ProbeTools`.
    pub async fn probe_tools(&self) -> Vec<toolcheck::ToolResult> {
        let home = std::env::var("HOME").unwrap_or_default();
        let overrides = self.snapshot_overrides();
        let search_dirs = agent_tool_dirs(&home, &overrides);
        let prober = toolcheck::Prober {
            search_dirs,
            overrides,
            timeout: Duration::ZERO,
        };
        prober.probe(&toolcheck::default_tools()).await
    }

    /// Records an explicit path for a tool and persists it; the new path's directory reaches the
    /// daemon's agent-launch PATH on the next daemon restart. An empty path clears the override. Mirrors
    /// `SetToolOverride`.
    pub fn set_tool_override(&self, name: &str, path: &str) -> Result<(), String> {
        if !path.is_empty() && !is_executable_file(Path::new(path)) {
            return Err(format!("{path:?} is not an executable file"));
        }
        // Serialize the whole read-merge-swap-persist so two overlapping saves can't each start from the
        // same stale snapshot and drop one another's override (a lost update).
        let _save_guard = lock(&self.inner.save_mu);
        // Swap the in-memory overrides FIRST (the source of truth for the daemon's agent PATH), then
        // persist; on a failed write we revert so an unpersisted override is not left applied.
        let (prev, next) = {
            let mut m = lock(&self.inner.mu);
            let prev = m.overrides.clone();
            let mut next = prev.clone();
            if path.is_empty() {
                next.remove(name);
            } else {
                next.insert(name.to_string(), path.to_string());
            }
            m.overrides = next.clone();
            (prev, next)
        };
        if let Some(prefs_path) = &self.inner.prefs_path
            && let Err(e) = prefs::save_tool_overrides(prefs_path, &next)
        {
            lock(&self.inner.mu).overrides = prev;
            return Err(e.to_string());
        }
        Ok(())
    }

    // ---- onboarding (config write-back) -----------------------------------------------------------

    /// The onboarding wizard's final step: write a minimal valid WORKFLOW.md for the chosen Linear
    /// project, then (re)build the supervisor and start the daemon. Refuses to overwrite an existing
    /// config (race-free via an exclusive create) so it can never clobber Settings edits, and verifies
    /// the sidecar BEFORE writing so a failure leaves `configured()` false. Mirrors `WriteInitialConfig`.
    pub async fn write_initial_config(&self, project_slug: &str) -> Result<(), String> {
        let workflow_path = match &self.inner.workflow_path {
            Some(p) => p.clone(),
            None => return Err("no workflow path resolved".to_string()),
        };
        if self.configured() {
            return Err(
                "already configured: edit the config in Settings instead of re-running onboarding"
                    .to_string(),
            );
        }
        // Verify the sidecar BEFORE writing anything: if rhapsodyd can't be located, fail without
        // writing WORKFLOW.md so `configured()` stays false and the wizard stays mounted with this error.
        if self.inner.binary_path.as_os_str().is_empty() {
            return Err("the rhapsodyd sidecar could not be located; reinstall the app (it ships in Contents/Resources)".to_string());
        }
        let data = onboarding::render_initial_workflow(project_slug).map_err(|e| e.to_string())?;
        if let Some(parent) = workflow_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Create EXCLUSIVELY (O_CREATE|O_EXCL): the configured() check above is advisory, so the
        // exclusive open makes "refuse to overwrite an existing WORKFLOW.md" race-free.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&workflow_path)
            .map_err(|e| format!("create {}: {e}", workflow_path.display()))?;
        if let Err(e) = file.write_all(&data) {
            // Remove the partial file so a later attempt is not blocked by the "already configured" guard.
            drop(file);
            let _ = std::fs::remove_file(&workflow_path);
            return Err(e.to_string());
        }
        drop(file);
        // Stop any prior supervisor before replacing it, so an already-running daemon is not orphaned.
        if let Some(old) = self.get_sup()
            && tokio::time::timeout(Duration::from_secs(10), old.stop())
                .await
                .is_err()
        {
            eprintln!(
                "rhapsody-desktop: config written but not starting the daemon: stopping the previous instance failed (a second instance would race the first)"
            );
            return Err("config saved, but the previous daemon could not be stopped to start the new one — quit and relaunch (or Restart)".to_string());
        }
        // Now configured — (re)build the supervisor with the current credential and start it.
        self.set_sup(self.make_supervisor());
        self.ensure_started();
        Ok(())
    }
}

/// Resolves the WORKFLOW.md the app supervises: a `SYMPHONY_WORKFLOW` override (dev), else
/// `~/.rhapsody/WORKFLOW.md` (TRA-238: Rhapsody's runtime home, diverging from Go v0.4.0's
/// `~/.symphony`). `HOME` is read directly, matching `os.UserHomeDir` on macOS. Mirrors Go
/// `resolveWorkflowPath`.
fn resolve_workflow_path() -> Option<PathBuf> {
    resolve_workflow_path_from(
        std::env::var("SYMPHONY_WORKFLOW").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure resolver for [`resolve_workflow_path`], taking the env values so it is unit-testable without
/// mutating the process environment. A non-empty override wins; otherwise a non-empty home yields
/// `<home>/.rhapsody/WORKFLOW.md`; an empty/absent home (Go's `os.UserHomeDir` error) yields `None`.
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
        Some(h) if !h.is_empty() => Some(Path::new(h).join(".rhapsody").join("WORKFLOW.md")),
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

/// The `$LINEAR_API_KEY` dev-env value, or `None` when unset/empty (Go's `os.Getenv` returning "").
fn env_linear_api_key() -> Option<String> {
    std::env::var("LINEAR_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Where the app stores local prefs (tool overrides): `~/.symphony/tools.json`, or `None` when `$HOME`
/// is unset. Mirrors Go `resolvePrefsPath`.
fn resolve_prefs_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".symphony").join("tools.json"))
}

/// The 0600 credential file fallback: `~/.symphony/credentials`, or `$TMPDIR/symphony-credentials` when
/// `$HOME` is unset (so there is always a usable path). Mirrors Go `resolveCredentialPath`.
fn resolve_credential_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => Path::new(&h).join(".symphony").join("credentials"),
        _ => std::env::temp_dir().join("symphony-credentials"),
    }
}

/// Picks the credential backend at startup: the Keychain by default, but the 0600 file fallback at
/// `cred_path` when the Keychain holds no token yet a previously-written fallback file does — so a token
/// saved via the fallback on an unsigned machine survives a relaunch (design §5). A READ error (Keychain
/// locked, or an unreadable fallback file) is NOT "no token": it keeps that backend rather than masking
/// a possibly-current credential with a stale/absent one. Mirrors `resolveCredentialStore`.
fn resolve_credential_store(
    keychain: Arc<dyn credential::Store>,
    cred_path: &Path,
) -> Arc<dyn credential::Store> {
    match keychain.get() {
        Err(e) => {
            eprintln!(
                "rhapsody-desktop: reading Linear token from Keychain failed; keeping the Keychain store (not masking it with the file fallback): {e}"
            );
            keychain
        }
        Ok(t) if !t.is_empty() => keychain,
        Ok(_) => {
            // Keychain is definitively empty: a token saved to the file fallback (a Keychain write
            // rejected on an unsigned machine) must survive the relaunch (design §5).
            let file: Arc<dyn credential::Store> = Arc::new(credential::new_file(cred_path));
            match file.get() {
                Err(e) => {
                    eprintln!(
                        "rhapsody-desktop: reading the Linear token file fallback failed; keeping the file store (not falling back to $LINEAR_API_KEY): {e}"
                    );
                    file
                }
                Ok(ft) if !ft.is_empty() => file,
                Ok(_) => keychain,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::Store;
    use crate::credential::mock::{MockKeyring, keychain as mock_keychain};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_app() -> App {
        App::new(None, PathBuf::new())
    }

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rhapsody-d4-app-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    /// An App wired with an in-memory keychain (the Rust analog of `keyring.MockInit`) and a temp
    /// credential file path, for the credential/onboarding tests. `binary_path` is empty so no daemon is
    /// ever launched. Returns the App and a handle to the keychain store so tests can inspect it.
    fn cred_app(backend: Arc<MockKeyring>, cred_path: PathBuf) -> (App, Arc<dyn Store>) {
        let keychain: Arc<dyn Store> = Arc::new(mock_keychain(backend));
        let app = App::new_with(None, PathBuf::new(), None, cred_path, keychain.clone());
        (app, keychain)
    }

    #[cfg(test)]
    impl App {
        /// Test hook: simulate a background Start in flight for `s` (Go tests set `a.startingSup`).
        fn set_starting_sup_for_test(&self, s: Option<Supervisor>) {
            lock(&self.inner.mu).starting_sup = s;
        }

        /// Test hook: set the active credential store directly (Go tests build `&App{cred: ...}`).
        fn set_cred_for_test(&self, cred: Arc<dyn Store>) {
            lock(&self.inner.mu).cred = Some(cred);
        }
    }

    /// An App whose prefs live under `dir`, so its pending-update marker resolves to `dir/pending-update`
    /// (the P11-U1 flag path is derived from the prefs dir). `binary_path` is empty so no daemon launches.
    fn pending_app(dir: &Path) -> App {
        App::new_with(
            None,
            PathBuf::new(),
            Some(dir.join("tools.json")),
            PathBuf::new(),
            Arc::new(credential::new()),
        )
    }

    // The pending-update flag persists across App instances via the marker file: set → observed true by a
    // fresh App, clear → false. This is what lets an install refused mid-run be honored on the next quit.
    #[test]
    fn pending_update_flag_round_trips() {
        let dir = temp_dir();
        let a = pending_app(&dir);
        assert!(!a.pending_update(), "a fresh app has no pending update");

        a.set_pending_update(true).expect("set pending");
        assert!(a.pending_update(), "pending must read back true after set");
        assert!(
            pending_app(&dir).pending_update(),
            "the flag must survive as a file so a later quit (a new App) sees it"
        );

        a.set_pending_update(false).expect("clear pending");
        assert!(
            !a.pending_update(),
            "pending must read back false after clear"
        );
        // Clearing again is idempotent (the marker is already gone) — the quit path may clear twice.
        a.set_pending_update(false)
            .expect("clearing an absent marker is not an error");
        std::fs::remove_dir_all(&dir).ok();
    }

    // With no `$HOME`-derived prefs path there is nowhere to persist the flag: set/clear are no-ops that
    // never error, and the flag always reads false (the guard simply cannot defer).
    #[test]
    fn pending_update_is_a_noop_without_a_path() {
        let a = test_app(); // no prefs_path → no pending_update_path
        assert!(!a.pending_update());
        a.set_pending_update(true)
            .expect("set is a no-op without a path");
        assert!(
            !a.pending_update(),
            "still false — there is nowhere to record it"
        );
        a.set_pending_update(false)
            .expect("clear is a no-op without a path");
    }

    // The install guard's run count is 0 whenever the daemon is not actively running — no supervisor
    // wired yet, and a stopped supervisor — so an idle app never blocks its own update.
    #[tokio::test]
    async fn active_run_count_is_zero_when_not_running() {
        let a = test_app();
        assert_eq!(
            a.active_run_count().await,
            0,
            "no supervisor → 0 active runs"
        );
        a.set_sup(Supervisor::new(Options::default())); // freshly built = Stopped, never probed
        assert_eq!(
            a.active_run_count().await,
            0,
            "a stopped supervisor → 0 active runs (no /api/v1/state probe)"
        );
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
            Some(PathBuf::from("/home/u/.rhapsody/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_defaults_to_home_when_no_override() {
        assert_eq!(
            resolve_workflow_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.rhapsody/WORKFLOW.md")),
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

    // ---- D4: credential resolution ----------------------------------------------------------------

    // Mirrors TestResolveCredentialStorePrefersFileFallback: with an empty Keychain and a token in the
    // file fallback, resolve_credential_store selects the file store and the token is visible.
    #[test]
    fn resolve_credential_store_prefers_file_fallback() {
        let dir = temp_dir();
        let file_path = dir.join("credentials");

        // Nothing stored anywhere -> default (Keychain) store, no token.
        let kc: Arc<dyn Store> = Arc::new(mock_keychain(MockKeyring::empty()));
        assert_eq!(
            resolve_credential_store(kc, &file_path).backend(),
            "keychain",
            "want keychain when nothing is stored"
        );

        // A token saved to the file fallback (Keychain still empty) must be picked up next launch.
        credential::new_file(&file_path)
            .set("lin_api_fromfile")
            .expect("seed file fallback");
        let kc: Arc<dyn Store> = Arc::new(mock_keychain(MockKeyring::empty()));
        let store = resolve_credential_store(kc, &file_path);
        assert_eq!(
            store.backend(),
            "file",
            "the fallback should win when the Keychain is empty"
        );
        assert_eq!(store.get().expect("get"), "lin_api_fromfile");

        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestResolveCredentialStoreKeepsKeychainOnReadError: a Keychain READ error keeps the
    // Keychain store and must NOT promote a possibly-stale file token.
    #[test]
    fn resolve_credential_store_keeps_keychain_on_read_error() {
        let dir = temp_dir();
        let file_path = dir.join("credentials");
        credential::new_file(&file_path)
            .set("lin_api_stale_file")
            .expect("seed stale file token");
        let kc: Arc<dyn Store> = Arc::new(mock_keychain(MockKeyring::erroring("keychain locked")));
        assert_eq!(
            resolve_credential_store(kc, &file_path).backend(),
            "keychain",
            "a Keychain read error must keep the Keychain backend, not select the stale file token"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestResolveCredentialStoreKeepsFileOnReadError: an empty Keychain but an existing-yet-
    // unreadable file fallback (a directory path) keeps the file store so the failure surfaces.
    #[test]
    fn resolve_credential_store_keeps_file_on_read_error() {
        let unreadable = temp_dir(); // a directory makes File.get's read fail with a non-NotFound error
        let kc: Arc<dyn Store> = Arc::new(mock_keychain(MockKeyring::empty()));
        assert_eq!(
            resolve_credential_store(kc, &unreadable).backend(),
            "file",
            "must keep the file store when the fallback exists but is unreadable (not mask it with env)"
        );
        std::fs::remove_dir_all(&unreadable).ok();
    }

    // Mirrors TestSetLinearTokenClearsOtherBackend: a token saved to the Keychain must clear a
    // previously-saved file-fallback token so it can't resurface / outlive the new token.
    #[tokio::test]
    async fn set_linear_token_clears_other_backend() {
        let dir = temp_dir();
        let cred_path = dir.join("credentials");
        // Seed a stale token in the file fallback.
        credential::new_file(&cred_path)
            .set("lin_api_stale_file")
            .expect("seed stale file token");

        let (app, keychain) = cred_app(MockKeyring::empty(), cred_path.clone());
        app.set_linear_token("lin_api_new")
            .await
            .expect("set_linear_token");

        // The active backend is the Keychain and holds the new token.
        assert_eq!(keychain.get().expect("keychain get"), "lin_api_new");
        // The file fallback must have been cleared (no stale token lingering).
        assert_eq!(
            credential::new_file(&cred_path).get().expect("file get"),
            "",
            "the file fallback must be cleared after a successful Keychain write"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestLinearTokenNoEnvFallbackOnReadError: a Keychain READ error must NOT silently fall back
    // to $LINEAR_API_KEY — linear_token returns "" so the missing credential is visible rather than wrong.
    #[test]
    fn linear_token_no_env_fallback_on_read_error() {
        let (app, _kc) = cred_app(MockKeyring::empty(), PathBuf::new());
        app.set_cred_for_test(Arc::new(mock_keychain(MockKeyring::erroring(
            "keychain locked",
        ))));
        assert_eq!(
            app.linear_token_with_env("lin_api_env_should_not_be_used"),
            "",
            "a read error must not be masked by the env var"
        );
    }

    // Mirrors TestCredentialStatusReadErrorDoesNotPromoteEnv: CredentialStatus must agree with
    // linear_token on a read error — it must NOT report a token sourced from $LINEAR_API_KEY.
    #[test]
    fn credential_status_read_error_does_not_promote_env() {
        let (app, _kc) = cred_app(MockKeyring::empty(), PathBuf::new());
        app.set_cred_for_test(Arc::new(mock_keychain(MockKeyring::erroring(
            "keychain locked",
        ))));
        assert!(
            !app.credential_status_with("lin_api_env", false).has_token,
            "CredentialStatus.has_token must be false on a read error (env must not be promoted)"
        );
    }

    // Additive (Go tests the env promotion implicitly): with no stored credential, CredentialStatus
    // reports the dev env var labelled "env" so the UI hides the no-op Keychain "Remove".
    #[test]
    fn credential_status_reports_env_when_nothing_stored() {
        let (app, _kc) = cred_app(MockKeyring::empty(), PathBuf::new());
        app.set_cred_for_test(Arc::new(mock_keychain(MockKeyring::empty())));
        let dto = app.credential_status_with("lin_api_env", false);
        assert!(
            dto.has_token,
            "an env token should be reported when nothing is stored"
        );
        assert_eq!(
            dto.backend, "env",
            "the dev env var backend is labelled env"
        );
    }

    // ---- D4: tool overrides -----------------------------------------------------------------------

    // Additive: set_tool_override rejects a non-executable path, persists an executable one to prefs and
    // the in-memory map, and clears an override when given an empty path.
    #[test]
    fn set_tool_override_validates_persists_and_clears() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let prefs_path = dir.join("tools.json");
        let exe = dir.join("claude");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write stub");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let exe_str = exe.to_string_lossy().into_owned();

        let app = App::new_with(
            None,
            PathBuf::new(),
            Some(prefs_path.clone()),
            PathBuf::new(),
            Arc::new(mock_keychain(MockKeyring::empty())),
        );

        // A non-executable path is rejected and nothing is persisted.
        assert!(
            app.set_tool_override("claude", dir.join("nope").to_str().unwrap())
                .is_err(),
            "a non-executable override path must be rejected"
        );

        // An executable override persists to the in-memory map and to tools.json.
        app.set_tool_override("claude", &exe_str)
            .expect("set override");
        assert_eq!(app.snapshot_overrides().get("claude"), Some(&exe_str));
        let persisted = prefs::load_tool_overrides(&prefs_path).expect("load prefs");
        assert_eq!(
            persisted.get("claude"),
            Some(&exe_str),
            "override must be persisted to prefs"
        );

        // An empty path clears the override.
        app.set_tool_override("claude", "").expect("clear override");
        assert!(
            !app.snapshot_overrides().contains_key("claude"),
            "empty path clears the override"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- D4: onboarding config write-back ---------------------------------------------------------

    // Mirrors TestWriteInitialConfigRefusesExistingFile: onboarding must never clobber an existing
    // WORKFLOW.md (the user's config / Settings edits).
    #[tokio::test]
    async fn write_initial_config_refuses_existing_file() {
        let dir = temp_dir();
        let wf = dir.join("WORKFLOW.md");
        std::fs::write(&wf, b"existing config").expect("seed existing config");

        // A resolvable sidecar path so the refusal is the "already configured" guard, not the sidecar one.
        let app = App::new_with(
            Some(wf.clone()),
            PathBuf::from("/usr/bin/true"),
            None,
            PathBuf::new(),
            Arc::new(mock_keychain(MockKeyring::empty())),
        );
        assert!(
            app.write_initial_config("proj").await.is_err(),
            "write_initial_config should refuse to overwrite an existing WORKFLOW.md"
        );
        assert_eq!(
            std::fs::read_to_string(&wf).expect("read wf"),
            "existing config",
            "the existing config must not be clobbered"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestWriteInitialConfigErrorsWithoutSidecar: with no resolvable rhapsodyd, the wizard fails
    // WITHOUT writing WORKFLOW.md — so configured() stays false and the wizard stays on screen.
    #[tokio::test]
    async fn write_initial_config_errors_without_sidecar() {
        let dir = temp_dir();
        let wf = dir.join("WORKFLOW.md");
        let app = App::new_with(
            Some(wf.clone()),
            PathBuf::new(), // no sidecar
            None,
            PathBuf::new(),
            Arc::new(mock_keychain(MockKeyring::empty())),
        );
        assert!(
            app.write_initial_config("proj").await.is_err(),
            "expected an error when the rhapsodyd sidecar is missing"
        );
        assert!(
            !wf.exists(),
            "WORKFLOW.md must NOT be written when the sidecar is missing (configured stays false)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
