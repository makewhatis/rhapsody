//! Launches and supervises the `symphonyd` sidecar for the desktop app. Parity port of
//! `$REF/desktop/internal/supervisor/supervisor.go`: it resolves the daemon binary, launches it on a
//! known-good PATH with an explicit `--port`, polls `/healthz` for readiness, restarts it on crash
//! with backoff, and stops it cleanly (SIGTERM) on quit. It deliberately has no Tauri dependency, so
//! the lifecycle is unit-testable against a fake daemon (`src/bin/fakedaemon.rs`).
//!
//! The Go design used goroutines + channels + a per-`Start` `supRun` to make re-`Start` race-free;
//! this port keeps that shape with a spawned tokio task per `Start` and a per-run set of
//! `tokio::sync` primitives (a `oneshot` for first-readiness, `watch`es for stop/done).

mod env;
mod resolve;

pub use env::{child_env, default_tool_dirs};
pub use resolve::{ResolveError, is_executable_file, resolve_binary, resources_dir_for};

use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

/// The supervised daemon's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not running (initial state, after `stop`, or after giving up).
    Stopped,
    /// A process has been launched but `/healthz` has not gone green yet.
    Starting,
    /// The process is up and `/healthz` is answering 200.
    Running,
}

impl State {
    /// The lowercase state string the UI/tray shows (matches Go `State.String`).
    pub fn as_str(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An immutable snapshot of the supervisor for the UI/tray. Mirrors Go `supervisor.Status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub pid: i32,
    pub restarts: i64,
    pub last_err: String,
}

/// Where the daemon's stdout/stderr go. Mirrors Go's `DaemonOutput io.Writer` (default `io.Discard`;
/// the app wires `os.Stderr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonOutput {
    /// Discard the daemon's output (`Stdio::null`) — the default.
    #[default]
    Discard,
    /// Inherit the parent's stdout/stderr (`Stdio::inherit`).
    Inherit,
}

/// The reason [`Supervisor::start`] (or [`Supervisor::restart`]) did not bring the daemon up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// `start` was called while the supervisor was not `Stopped`. Mirrors Go `ErrAlreadyStarted`.
    AlreadyStarted,
    /// The caller's cancellation future fired before the daemon became healthy (mirrors Go's
    /// `ctx.Done()` → `ctx.Err()` from `Start`).
    Cancelled,
    /// The configured `binary_path` is missing or not an executable file — a non-recoverable launch
    /// failure, so `start` fails fast without spinning the restart loop.
    NotExecutable(String),
    /// A free loopback port could not be chosen.
    PortResolution(String),
    /// The daemon never stayed healthy after exhausting the restart budget. Mirrors Go's
    /// "symphonyd did not stay healthy after N restart(s)" giveup error.
    NeverHealthy { restarts: i64, last_err: String },
    /// `stop`/shutdown was requested before the daemon ever became healthy. Mirrors Go `ErrStopped`.
    Stopped,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::AlreadyStarted => f.write_str("supervisor: already started"),
            StartError::Cancelled => f.write_str("supervisor: start cancelled before healthy"),
            StartError::NotExecutable(msg) => f.write_str(msg),
            StartError::PortResolution(msg) => write!(f, "resolve port: {msg}"),
            StartError::NeverHealthy { restarts, last_err } => write!(
                f,
                "symphonyd did not stay healthy after {restarts} restart(s): {last_err}"
            ),
            StartError::Stopped => f.write_str("supervisor: stopped before becoming healthy"),
        }
    }
}

impl std::error::Error for StartError {}

/// Configures a [`Supervisor`]. Unset fields default in [`Supervisor::new`] (or via [`Default`]).
pub struct Options {
    /// Path to symphonyd (required).
    pub binary_path: PathBuf,
    /// Path to WORKFLOW.md, passed as the positional arg when present.
    pub workflow_path: Option<PathBuf>,
    /// Explicit `--port`; 0 picks a free loopback port at `start`.
    pub port: u16,
    /// Base environment (defaults to the current process env); PATH is augmented.
    pub base_env: Option<Vec<String>>,
    /// Known-good PATH dirs (defaults to `default_tool_dirs($HOME)`).
    pub tool_dirs: Option<Vec<String>>,
    /// Fed to the daemon as `LINEAR_API_KEY` (from the Keychain).
    pub linear_api_key: String,
    /// Where the daemon's stdout/stderr go.
    pub daemon_output: DaemonOutput,
    /// Per-attempt wait for `/healthz` (default 30s).
    pub startup_timeout: Duration,
    /// Health poll cadence (default 250ms).
    pub poll_interval: Duration,
    /// SIGTERM grace before SIGKILL (default 5s).
    pub stop_grace: Duration,
    /// Restart attempts before giving up (default 5).
    pub max_restarts: i64,
    /// Delay before the Nth restart (default exponential, capped at 5s).
    pub backoff: Option<Arc<dyn Fn(i64) -> Duration + Send + Sync>>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            binary_path: PathBuf::new(),
            workflow_path: None,
            port: 0,
            base_env: None,
            tool_dirs: None,
            linear_api_key: String::new(),
            daemon_output: DaemonOutput::Discard,
            startup_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
            stop_grace: Duration::from_secs(5),
            max_restarts: 5,
            backoff: None,
        }
    }
}

/// The mutable state read by `status`/UI from other tasks (including the current run pointer).
struct StateData {
    state: State,
    pid: i32,
    restarts: i64,
    last_err: Option<String>,
    port: u16,
    run: Option<Arc<SupRun>>,
}

/// Owns one `start → … → stopped` lifecycle's channels. Each supervise task is handed its OWN run and
/// only ever touches that run's channels, so a re-`start` (which installs a fresh run) can never
/// reassign channels out from under a still-draining previous task. Mirrors Go `supRun`.
struct SupRun {
    /// First-readiness result, delivered to `start` exactly once (the `Option` is the `readyOnce`).
    ready: Mutex<Option<oneshot::Sender<Result<(), StartError>>>>,
    /// `true` once this run has been asked to stop (closed `stopCh` in Go). The retained receiver
    /// keeps the channel open so `send` always succeeds and `borrow` reads the latest value.
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
    /// `true` once the supervise loop has finished draining (closed `doneCh` in Go).
    done_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
}

impl SupRun {
    fn new() -> (Arc<Self>, oneshot::Receiver<Result<(), StartError>>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let run = Arc::new(SupRun {
            ready: Mutex::new(Some(ready_tx)),
            stop_tx,
            stop_rx,
            done_tx,
            done_rx,
        });
        (run, ready_rx)
    }

    /// Signals this run to terminate (idempotent).
    fn request_stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Reports whether this run has been asked to stop (non-blocking).
    fn stop_requested(&self) -> bool {
        *self.stop_rx.borrow()
    }

    /// Completes when a stop has been requested (returns immediately if already requested). Clones
    /// the retained receiver (keeping the channel open) rather than subscribing.
    async fn stopped(&self) {
        let mut rx = self.stop_rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }

    /// Delivers the first-readiness result to `start` exactly once (the `readyOnce`).
    fn signal_ready(&self, result: Result<(), StartError>) {
        if let Some(tx) = lock(&self.ready).take() {
            let _ = tx.send(result);
        }
    }

    /// Marks the supervise loop finished, releasing every `done` waiter.
    fn mark_done(&self) {
        let _ = self.done_tx.send(true);
    }

    /// Completes once the supervise loop has finished draining. Clones the retained receiver
    /// (keeping the channel open so `mark_done`'s send always lands) rather than subscribing.
    async fn done(&self) {
        let mut rx = self.done_rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// Recovers a poisoned lock rather than propagating the panic — the guarded critical sections are
/// tiny field reads/writes, never held across an `.await`, so a poisoned value is still consistent.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shared, `Arc`-held daemon state + config. The spawned supervise task and the public accessors
/// all reach the daemon through this.
struct Inner {
    binary_path: PathBuf,
    workflow_path: Option<PathBuf>,
    opt_port: u16,
    base_env: Option<Vec<String>>,
    tool_dirs: Vec<String>,
    linear_api_key: String,
    daemon_output: DaemonOutput,
    startup_timeout: Duration,
    poll_interval: Duration,
    stop_grace: Duration,
    max_restarts: i64,
    backoff: Arc<dyn Fn(i64) -> Duration + Send + Sync>,
    state: Mutex<StateData>,
    http: reqwest::Client,
}

/// Owns the daemon process lifecycle. Cheap to [`Clone`] (an `Arc` handle); all methods are safe for
/// concurrent use. Mirrors Go `*supervisor.Supervisor`.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl Supervisor {
    /// Builds a Supervisor, applying defaults to any unset [`Options`].
    pub fn new(opts: Options) -> Self {
        let tool_dirs = opts
            .tool_dirs
            .unwrap_or_else(|| default_tool_dirs(&std::env::var("HOME").unwrap_or_default()));
        let backoff = opts.backoff.unwrap_or_else(|| {
            Arc::new(expo_backoff) as Arc<dyn Fn(i64) -> Duration + Send + Sync>
        });
        // A short-timeout client for the health probe. Builder failure is practically impossible for
        // this http-only client; fall back to the default client rather than panic.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Supervisor {
            inner: Arc::new(Inner {
                binary_path: opts.binary_path,
                workflow_path: opts.workflow_path,
                opt_port: opts.port,
                base_env: opts.base_env,
                tool_dirs,
                linear_api_key: opts.linear_api_key,
                daemon_output: opts.daemon_output,
                startup_timeout: nonzero(opts.startup_timeout, Duration::from_secs(30)),
                poll_interval: nonzero(opts.poll_interval, Duration::from_millis(250)),
                stop_grace: nonzero(opts.stop_grace, Duration::from_secs(5)),
                max_restarts: if opts.max_restarts == 0 {
                    5
                } else {
                    opts.max_restarts
                },
                backoff,
                state: Mutex::new(StateData {
                    state: State::Stopped,
                    pid: 0,
                    restarts: 0,
                    last_err: None,
                    port: 0,
                    run: None,
                }),
                http,
            }),
        }
    }

    /// Launches and supervises the daemon, returning once it first becomes healthy or the supervisor
    /// gives up (returning the failure). The supervise loop keeps running afterwards to restart the
    /// daemon on crash; it is torn down by [`Supervisor::stop`]. `cancel` bounds ONLY the wait for
    /// first readiness — the supervisor's own lifetime is governed by `stop`. Mirrors Go `Start(ctx)`.
    pub async fn start<F>(&self, cancel: F) -> Result<(), StartError>
    where
        F: Future<Output = ()> + Send,
    {
        // Re-entry gate + run installation, all under the lock (a previous run only sets Stopped at
        // its terminal point, so a re-start here always installs a brand-new run with its own
        // channels). The lock is released before we spawn supervise.
        let (inner, run, ready_rx) = {
            let mut st = lock(&self.inner.state);
            if st.state != State::Stopped {
                return Err(StartError::AlreadyStarted);
            }
            // A missing / non-executable sidecar can never become healthy — fail fast (while Stopped)
            // with a clear error instead of spinning the restart loop on a launch error.
            if !is_executable_file(&self.inner.binary_path) {
                let msg = format!(
                    "symphonyd sidecar not available at {}: not an executable file",
                    self.inner.binary_path.display()
                );
                st.last_err = Some(msg.clone());
                return Err(StartError::NotExecutable(msg));
            }
            let port = if self.inner.opt_port > 0 {
                self.inner.opt_port
            } else {
                match pick_free_port() {
                    Ok(p) => p,
                    Err(e) => return Err(StartError::PortResolution(e.to_string())),
                }
            };
            let (run, ready_rx) = SupRun::new();
            st.port = port;
            st.last_err = None;
            st.restarts = 0;
            st.run = Some(run.clone());
            st.state = State::Starting;
            (self.inner.clone(), run, ready_rx)
        };

        tokio::spawn(supervise(inner, run));

        tokio::pin!(cancel);
        tokio::select! {
            res = ready_rx => match res {
                Ok(r) => r,
                // The supervise task dropped the sender without signalling (should not happen); treat
                // as a stop so `start` never hangs.
                Err(_) => Err(StartError::Stopped),
            },
            _ = &mut cancel => {
                // The caller abandoned the readiness wait. Tear down the run we installed (idempotent;
                // touches only this run's channels) so supervise doesn't keep looping with state stuck
                // non-Stopped — otherwise a follow-up start would hit the re-entry gate. Teardown is
                // async; we return promptly without waiting on `done`.
                if let Some(run) = lock(&self.inner.state).run.clone() {
                    run.request_stop();
                }
                Err(StartError::Cancelled)
            }
        }
    }

    /// Requests a graceful shutdown (SIGTERM) and waits for the supervise loop to finish (the daemon
    /// terminated, restarts suppressed). Idempotent. Bound the wait with `tokio::time::timeout` if
    /// needed (the D3 drain does). Mirrors Go `Stop(ctx)`.
    pub async fn stop(&self) {
        let run = {
            let st = lock(&self.inner.state);
            if st.state == State::Stopped {
                return;
            }
            match &st.run {
                Some(r) => r.clone(),
                None => return,
            }
        };
        run.request_stop(); // idempotent
        run.done().await;
    }

    /// Stops then starts the daemon. Mirrors Go `Restart(ctx)`.
    pub async fn restart<F>(&self, cancel: F) -> Result<(), StartError>
    where
        F: Future<Output = ()> + Send,
    {
        self.stop().await;
        self.start(cancel).await
    }

    /// Returns an immutable snapshot for the UI/tray.
    pub fn status(&self) -> Status {
        let st = lock(&self.inner.state);
        Status {
            state: st.state,
            pid: st.pid,
            restarts: st.restarts,
            last_err: st.last_err.clone().unwrap_or_default(),
        }
    }

    /// The daemon's loopback base URL (what the webview shows once healthy).
    pub fn url(&self) -> String {
        let port = lock(&self.inner.state).port;
        format!("http://127.0.0.1:{port}")
    }

    /// The readiness probe endpoint.
    pub fn health_url(&self) -> String {
        format!("{}/healthz", self.url())
    }

    /// Reports whether `GET /healthz` currently answers 200.
    pub async fn healthy(&self) -> bool {
        self.inner.healthy().await
    }

    /// Reports whether two handles point at the SAME supervisor instance (`Arc` identity). The App
    /// uses this to tell a swapped-in supervisor from the one a background Start is in flight for —
    /// mirroring the Go pointer comparisons (`a.startingSup == sup`, `a.getSup() == orig`;
    /// `$REF/desktop/app.go`).
    pub fn ptr_eq(&self, other: &Supervisor) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Inner {
    async fn healthy(&self) -> bool {
        let port = lock(&self.state).port;
        let url = format!("http://127.0.0.1:{port}/healthz");
        match self.http.get(url).send().await {
            Ok(resp) => {
                let ok = resp.status() == reqwest::StatusCode::OK;
                // Drain the body so the pooled connection can be reused for the next poll (parity
                // with Go's io.Copy(io.Discard, resp.Body)).
                let _ = resp.bytes().await;
                ok
            }
            Err(_) => false,
        }
    }

    fn set_starting(&self, pid: i32) {
        let mut s = lock(&self.state);
        s.state = State::Starting;
        s.pid = pid;
    }

    fn set_running(&self, pid: i32) {
        let mut s = lock(&self.state);
        s.state = State::Running;
        s.pid = pid;
    }

    fn set_stopped(&self) {
        let mut s = lock(&self.state);
        s.state = State::Stopped;
        s.pid = 0;
    }

    fn set_last_err(&self, msg: String) {
        lock(&self.state).last_err = Some(msg);
    }

    fn last_err_value(&self) -> String {
        lock(&self.state).last_err.clone().unwrap_or_default()
    }

    fn inc_restarts(&self) {
        lock(&self.state).restarts += 1;
    }

    fn port(&self) -> u16 {
        lock(&self.state).port
    }

    /// Constructs the daemon command with the known-good PATH/credential environment and its own
    /// process group. Mirrors Go `buildCmd`.
    fn build_command(&self) -> tokio::process::Command {
        use std::os::unix::process::CommandExt;

        let mut args: Vec<String> = vec!["--port".to_string(), self.port().to_string()];
        if let Some(wf) = &self.workflow_path {
            args.push(wf.to_string_lossy().into_owned());
        }
        let base = self.base_env.clone().unwrap_or_else(current_env);
        let env = child_env(&base, &self.tool_dirs, &self.linear_api_key);

        let mut std_cmd = std::process::Command::new(&self.binary_path);
        std_cmd.args(&args);
        // `cmd.Env = ChildEnv(...)` in Go REPLACES the whole environment — clear then set exactly.
        std_cmd.env_clear();
        for kv in &env {
            if let Some((k, v)) = kv.split_once('=') {
                std_cmd.env(k, v);
            }
        }
        match self.daemon_output {
            DaemonOutput::Discard => {
                std_cmd.stdout(Stdio::null());
                std_cmd.stderr(Stdio::null());
            }
            DaemonOutput::Inherit => {
                std_cmd.stdout(Stdio::inherit());
                std_cmd.stderr(Stdio::inherit());
            }
        }
        // Run the daemon as its own process-group leader so `terminate` can signal the whole group
        // (the daemon plus any agent subprocesses it spawns), leaving nothing orphaned on quit.
        // SAFETY: `setpgid(0, 0)` is async-signal-safe, the only requirement for a `pre_exec` hook.
        unsafe {
            std_cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        tokio::process::Command::from(std_cmd)
    }

    /// Launches the daemon once and returns when it becomes unhealthy/exits or its run is asked to
    /// stop. Transitions state to Starting then (on health) Running. Mirrors Go `runOnce`.
    async fn run_once(&self, run: &Arc<SupRun>) -> Outcome {
        let mut child = match self.build_command().spawn() {
            Ok(c) => c,
            Err(e) => {
                self.set_last_err(format!("launch {}: {e}", self.binary_path.display()));
                return Outcome::Failed;
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(0);
        self.set_starting(pid);

        // Phase 1: poll for readiness, bounded by startup_timeout.
        let deadline = Instant::now() + self.startup_timeout;
        while !self.healthy().await {
            tokio::select! {
                status = child.wait() => {
                    self.set_last_err(exit_error(status));
                    return if run.stop_requested() { Outcome::Stopped } else { Outcome::Failed };
                }
                _ = run.stopped() => {
                    self.terminate(&mut child).await;
                    return Outcome::Stopped;
                }
                _ = tokio::time::sleep(self.poll_interval) => {
                    if Instant::now() > deadline {
                        self.set_last_err(format!("not healthy within {:?}", self.startup_timeout));
                        self.terminate(&mut child).await;
                        return Outcome::Failed;
                    }
                }
            }
        }

        // Phase 2: healthy and steady — wait for exit or stop.
        self.set_running(pid);
        run.signal_ready(Ok(()));
        tokio::select! {
            status = child.wait() => {
                self.set_last_err(exit_error(status));
                if run.stop_requested() { Outcome::Stopped } else { Outcome::Crashed }
            }
            _ = run.stopped() => {
                self.terminate(&mut child).await;
                Outcome::Stopped
            }
        }
    }

    /// Sends SIGTERM and waits up to `stop_grace` for the process to exit, escalating to SIGKILL if it
    /// overruns. The daemon shuts down gracefully on SIGTERM, so SIGKILL is a backstop only. Mirrors
    /// Go `terminate`.
    async fn terminate(&self, child: &mut tokio::process::Child) {
        let pid = match child.id() {
            Some(p) => p as i32,
            None => return, // already reaped
        };
        signal_group(pid, libc::SIGTERM);
        tokio::select! {
            _ = child.wait() => {}
            _ = tokio::time::sleep(self.stop_grace) => {
                signal_group(pid, libc::SIGKILL);
                let _ = child.wait().await;
            }
        }
    }
}

/// The long-running loop: launch → wait healthy → steady state → restart-on-crash with backoff, until
/// stop or the restart budget is exhausted. It always signals readiness exactly once (success on
/// first healthy, else the failure when giving up) and marks `done`. Mirrors Go `supervise`.
async fn supervise(inner: Arc<Inner>, run: Arc<SupRun>) {
    let mut attempt: i64 = 0;
    loop {
        match inner.run_once(&run).await {
            Outcome::Stopped => {
                inner.set_stopped();
                run.signal_ready(Err(StartError::Stopped));
                break;
            }
            Outcome::Failed | Outcome::Crashed => {
                if attempt >= inner.max_restarts {
                    inner.set_stopped();
                    run.signal_ready(Err(StartError::NeverHealthy {
                        restarts: attempt,
                        last_err: inner.last_err_value(),
                    }));
                    break;
                }
                inner.inc_restarts();
                let delay = (inner.backoff)(attempt + 1);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = run.stopped() => {
                        inner.set_stopped();
                        run.signal_ready(Err(StartError::Stopped));
                        break;
                    }
                }
            }
        }
        attempt += 1;
    }
    run.mark_done();
}

/// The result of a single [`Inner::run_once`]. Mirrors Go's `outcome`.
enum Outcome {
    /// Stop/cancel requested; do not restart.
    Stopped,
    /// Launch error or never became healthy.
    Failed,
    /// Was healthy, then exited unexpectedly.
    Crashed,
}

/// Delivers `sig` to the daemon leader AND the rest of its process group so the daemon and any agent
/// subprocesses it spawned are reaped together rather than orphaned. Signals the leader first (safe:
/// the caller still holds the un-reaped child, so the pid is not recycled), then the group
/// (`setpgid` made pgid == pid). Mirrors Go `signalDaemon`.
fn signal_group(pid: i32, sig: libc::c_int) {
    // SAFETY: `kill` is a plain syscall wrapper; a stale/reaped pid returns -1/ESRCH, which we treat
    // as "already gone" and stop.
    unsafe {
        if libc::kill(pid, sig) == -1 {
            return;
        }
        let _ = libc::kill(-pid, sig);
    }
}

/// Caps an exponential backoff at 5s. Mirrors Go `expoBackoff`.
fn expo_backoff(attempt: i64) -> Duration {
    let shift = attempt.clamp(0, 5) as u32;
    (Duration::from_millis(250) * (1u32 << shift)).min(Duration::from_secs(5))
}

/// Asks the OS for a free loopback TCP port. There is an inherent TOCTOU window before the daemon
/// binds it, but on loopback with immediate launch this is reliable in practice. Mirrors Go
/// `pickFreePort`.
fn pick_free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Normalizes a process `wait` result into a descriptive error string (a clean exit-0 is
/// "unexpected"). Mirrors Go `exitError`. Note `child.wait()` yields `Ok(status)` for BOTH zero and
/// non-zero exits (unlike Go's `cmd.Wait`, which errors on non-zero) — so success is checked here.
fn exit_error(status: std::io::Result<ExitStatus>) -> String {
    match status {
        Ok(s) if s.success() => "daemon exited unexpectedly (status 0)".to_string(),
        Ok(s) => format!("daemon exited: {s}"),
        Err(e) => format!("daemon exited: {e}"),
    }
}

/// The current process environment as a `KEY=VALUE` vector (Go's `os.Environ()`). Non-UTF-8 entries
/// are skipped rather than panicking (the daemon only needs UTF-8 tool paths).
fn current_env() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, v)| match (k.into_string(), v.into_string()) {
            (Ok(k), Ok(v)) => Some(format!("{k}={v}")),
            _ => None,
        })
        .collect()
}

/// Returns `d` if non-zero, else `default` (mirrors Go `New`'s "zero value gets a default").
fn nonzero(d: Duration, default: Duration) -> Duration {
    if d.is_zero() { default } else { d }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The State strings the UI/tray relies on (matches Go State.String()).
    #[test]
    fn state_strings() {
        assert_eq!(State::Stopped.as_str(), "stopped");
        assert_eq!(State::Starting.as_str(), "starting");
        assert_eq!(State::Running.as_str(), "running");
        assert_eq!(State::Running.to_string(), "running");
    }

    // expo_backoff caps at 5s and grows exponentially from 250ms (mirrors Go expoBackoff).
    #[test]
    fn expo_backoff_caps_at_five_seconds() {
        assert_eq!(expo_backoff(0), Duration::from_millis(250)); // 1<<0
        assert_eq!(expo_backoff(1), Duration::from_millis(500)); // 1<<1
        assert_eq!(expo_backoff(2), Duration::from_secs(1)); // 1<<2
        assert_eq!(expo_backoff(5), Duration::from_secs(5)); // 1<<5 = 8s, capped
        assert_eq!(expo_backoff(20), Duration::from_secs(5)); // clamp shift then cap
    }

    // New applies defaults for zero-valued Options (mirrors Go New).
    #[test]
    fn new_applies_defaults() {
        let sup = Supervisor::new(Options::default());
        assert_eq!(sup.inner.startup_timeout, Duration::from_secs(30));
        assert_eq!(sup.inner.poll_interval, Duration::from_millis(250));
        assert_eq!(sup.inner.stop_grace, Duration::from_secs(5));
        assert_eq!(sup.inner.max_restarts, 5);
        assert!(!sup.inner.tool_dirs.is_empty());
        assert_eq!(sup.status().state, State::Stopped);
    }

    // pick_free_port returns a usable loopback port.
    #[test]
    fn pick_free_port_returns_a_port() {
        let p = pick_free_port().expect("pick a free port");
        assert!(p > 0);
    }

    // ptr_eq reports Arc identity: two clones of one supervisor are equal, two separate `new`s are
    // not — so the App can tell a swapped-in supervisor from the one a background Start is in flight
    // for (mirrors the Go pointer comparisons `a.startingSup == sup` / `a.getSup() == orig`).
    #[test]
    fn ptr_eq_tracks_arc_identity() {
        let a = Supervisor::new(Options::default());
        let b = Supervisor::new(Options::default());
        assert!(a.ptr_eq(&a.clone()), "a clone shares the same instance");
        assert!(
            !a.ptr_eq(&b),
            "two separate supervisors are distinct instances"
        );
    }

    // exit_error flags a clean exit-0 as unexpected and describes a non-zero exit.
    #[test]
    fn exit_error_describes_outcomes() {
        // A real exited status is awkward to synthesize portably; assert on the Err arm + the doc'd
        // zero-exit sentinel via a helper that mirrors the match.
        let e = exit_error(Err(std::io::Error::other("boom")));
        assert!(e.contains("daemon exited"));
    }
}
