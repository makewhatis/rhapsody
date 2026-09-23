//! Supervisor lifecycle integration tests — parity port of
//! `$REF/desktop/internal/supervisor/supervisor_test.go`.
//!
//! Like the Go tests (which compile `testdata/fakedaemon` and drive the REAL launch/health-poll/
//! SIGTERM/restart machinery end to end), these launch the compiled `fakedaemon` bin — located via
//! Cargo's `CARGO_BIN_EXE_fakedaemon` — rather than mocking process/exec.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRead, CredentialState, CredentialStateTag, Revision,
};
use rhapsody_credential_ipc::owner::{
    CredentialOwner, CredentialStatus, MutationError, MutationOutcome,
};
use rhapsody_credential_ipc::session::{ClientSession, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, HelloFrame, ServerFrame, read_frame, write_frame,
};
use rhapsody_desktop::credential_bootstrap::{ChannelObservations, CredentialOwnerLookup};
use rhapsody_desktop::supervisor::{CredentialBootstrap, Options, StartError, State, Supervisor};

/// Path to the compiled fakedaemon stand-in for rhapsodyd (Cargo builds it for us).
fn fake_daemon() -> &'static str {
    env!("CARGO_BIN_EXE_fakedaemon")
}

/// Options tuned for quick, deterministic tests against the fake daemon (mirror of `fastOptions`),
/// with any extra `KEY=VALUE` env entries appended to the base env.
fn fast_options(bin: &str, extra_env: &[String]) -> Options {
    let mut base = vec!["PATH=/usr/bin:/bin".to_string()];
    base.extend(extra_env.iter().cloned());
    Options {
        binary_path: PathBuf::from(bin),
        base_env: Some(base),
        startup_timeout: Duration::from_secs(3),
        poll_interval: Duration::from_millis(15),
        stop_grace: Duration::from_secs(2),
        max_restarts: 3,
        backoff: Some(Arc::new(|_: i64| Duration::from_millis(15))),
        ..Default::default()
    }
}

/// A cancellation future for `start`/`restart` that fires after `dur` (the Rust stand-in for a
/// `context.WithTimeout` bound on the readiness wait).
fn cancel_after(dur: Duration) -> tokio::time::Sleep {
    tokio::time::sleep(dur)
}

fn env(kvs: &[&str]) -> Vec<String> {
    kvs.iter().map(|s| (*s).to_string()).collect()
}

/// A unique scratch directory removed on drop (STUDIO-1031), so a lifecycle run leaves no
/// `rhapsody-d2-sup-*` entry in `$TMPDIR`. `Deref`s to `Path` for `dir.join(..)` call sites.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "rhapsody-d2-sup-{}-{}-{nonce}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

// TestStartBecomesHealthyThenStop: Start launches the daemon, waits for /healthz to go green, reports
// Running, and Stop terminates it cleanly so health stops answering.
#[tokio::test]
async fn start_becomes_healthy_then_stop() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &env(&["FAKE_READY_DELAY_MS=120"])));

    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("start");
    let st = sup.status();
    assert_eq!(st.state, State::Running, "want Running: {st:?}");
    assert!(st.pid > 0, "want a PID: {st:?}");
    assert!(
        sup.healthy().await,
        "healthy() false after Start reported running"
    );

    sup.stop().await;
    assert_eq!(sup.status().state, State::Stopped);
    assert!(
        !sup.healthy().await,
        "daemon still healthy after Stop; SIGTERM did not terminate it"
    );
}

// TestRestartsOnCrash: the daemon exits non-zero on its first launch, and the supervisor relaunches
// it until it stays healthy, recording the restart.
#[tokio::test]
async fn restarts_on_crash() {
    let bin = fake_daemon();
    let dir = TempDir::new();
    let marker = dir.join("crash.marker");
    let sup = Supervisor::new(fast_options(
        bin,
        &env(&[&format!("FAKE_CRASH_MARKER={}", marker.display())]),
    ));

    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("start should recover from one crash");
    assert_eq!(
        sup.status().state,
        State::Running,
        "want Running after recover"
    );
    assert!(
        sup.status().restarts >= 1,
        "want restarts >= 1 after crash-and-recover"
    );
    // The crash marker must have been written (proving the first launch actually crashed).
    assert!(marker.exists(), "crash marker missing");
    sup.stop().await;
}

// TestStartTimeoutWhenNeverHealthy: a daemon that never serves /healthz must cause Start to fail
// (after exhausting restarts) rather than hang forever, and leave the supervisor stopped.
#[tokio::test]
async fn start_timeout_when_never_healthy() {
    let bin = fake_daemon();
    let mut opts = fast_options(bin, &env(&["FAKE_READY_DELAY_MS=60000"]));
    opts.startup_timeout = Duration::from_millis(250);
    opts.max_restarts = 1;
    let sup = Supervisor::new(opts);

    let err = sup.start(cancel_after(Duration::from_secs(10))).await;
    assert!(
        err.is_err(),
        "want an error when the daemon never becomes healthy"
    );
    assert_eq!(
        sup.status().state,
        State::Stopped,
        "want Stopped after giving up"
    );
}

// TestStartStopStartCycle: re-Start after a clean Stop on the SAME Supervisor. It must come back
// healthy each cycle with no panic (the per-run channel design).
#[tokio::test]
async fn start_stop_start_cycle() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &[]));
    for i in 0..3 {
        sup.start(cancel_after(Duration::from_secs(10)))
            .await
            .unwrap_or_else(|e| panic!("cycle {i} start: {e}"));
        assert_eq!(sup.status().state, State::Running, "cycle {i}");
        sup.stop().await;
    }
}

// TestRestartMethod: the public Restart (Stop+Start) in a loop — the UI's Restart control.
#[tokio::test]
async fn restart_method() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &[]));
    sup.start(cancel_after(Duration::from_secs(15)))
        .await
        .expect("start");
    for i in 0..3 {
        sup.restart(cancel_after(Duration::from_secs(15)))
            .await
            .unwrap_or_else(|e| panic!("restart {i}: {e}"));
        assert_eq!(sup.status().state, State::Running, "after restart {i}");
    }
    sup.stop().await;
}

// TestReStartAfterGiveUp: after the supervisor gives up, a fresh Start on the SAME instance must not
// panic or hang — it returns promptly (with an error here, since this fake never gets healthy).
#[tokio::test]
async fn restart_after_give_up() {
    let bin = fake_daemon();
    let mut opts = fast_options(bin, &env(&["FAKE_READY_DELAY_MS=60000"]));
    opts.startup_timeout = Duration::from_millis(120);
    opts.max_restarts = 1;
    let sup = Supervisor::new(opts);

    for i in 0..2 {
        let err = sup.start(cancel_after(Duration::from_secs(5))).await;
        assert!(err.is_err(), "attempt {i}: want give-up error");
        assert_eq!(
            sup.status().state,
            State::Stopped,
            "attempt {i}: want Stopped"
        );
    }
}

// TestConcurrentStartStopRestart: hammer the lifecycle from many tasks (Restart + the read-only
// Status/URL/Healthy accessors). Guards the per-run design against races/panics. The tight reader
// loops `yield_now()` each iteration because tokio (unlike Go) does not preempt non-awaiting tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_stop_restart() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &[]));
    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("initial start");

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    {
        let sup = sup.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = sup.restart(cancel_after(Duration::from_secs(5))).await;
            }
        }));
    }
    for _ in 0..2 {
        let sup = sup.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = sup.status();
                tokio::task::yield_now().await;
            }
        }));
    }
    {
        let sup = sup.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = sup.url();
                tokio::task::yield_now().await;
            }
        }));
    }
    {
        let sup = sup.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = sup.healthy().await;
            }
        }));
    }

    tokio::time::sleep(Duration::from_millis(400)).await;
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.await;
    }
    sup.stop().await;
}

// TestStartCancelTearsDownRun: when the caller's cancellation fires before the daemon is healthy,
// Start must tear down the run it installed. After the cancelled Start, state converges to Stopped,
// no daemon is left answering health, and a follow-up Start is not rejected with AlreadyStarted.
#[tokio::test]
async fn start_cancel_tears_down_run() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &env(&["FAKE_READY_DELAY_MS=60000"])));

    let err = sup.start(cancel_after(Duration::from_millis(80))).await;
    assert_eq!(err, Err(StartError::Cancelled), "want Cancelled");

    // Teardown is async; allow brief convergence to Stopped.
    let deadline = Instant::now() + Duration::from_secs(3);
    while sup.status().state != State::Stopped {
        assert!(
            Instant::now() < deadline,
            "state = {:?}; want Stopped after a cancelled Start tore down its run",
            sup.status().state
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !sup.healthy().await,
        "daemon still healthy after cancelled Start; the process leaked"
    );

    // A follow-up Start must not get AlreadyStarted (the cancelled run is gone).
    let err2 = sup.start(cancel_after(Duration::from_millis(200))).await;
    assert_ne!(
        err2,
        Err(StartError::AlreadyStarted),
        "follow-up Start returned AlreadyStarted; the cancelled run was not torn down"
    );
    sup.stop().await;
}

// TestStartFailsFastWhenBinaryMissing: an unresolved sidecar (empty BinaryPath) must fail Start
// IMMEDIATELY with a descriptive error instead of spinning the restart loop. Zero restarts, error
// surfaced via Status.
#[tokio::test]
async fn start_fails_fast_when_binary_missing() {
    let sup = Supervisor::new(fast_options("", &[]));
    let start = Instant::now();
    let err = sup.start(cancel_after(Duration::from_secs(2))).await;
    assert!(
        err.is_err(),
        "want an error when the sidecar binary is unresolved"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "Start took {:?}; want a fast failure (no restart loop)",
        start.elapsed()
    );
    let st = sup.status();
    assert_eq!(
        st.state,
        State::Stopped,
        "want Stopped after a fast launch failure"
    );
    assert_eq!(
        st.restarts, 0,
        "a missing binary must not enter the restart loop"
    );
    assert!(
        !st.last_err.is_empty(),
        "want a descriptive error for a missing sidecar"
    );
}

// TestStartFailsFastWhenBinaryNotExecutable: a path that exists but is not an executable file is
// likewise non-recoverable and must fail fast without retrying.
#[tokio::test]
async fn start_fails_fast_when_binary_not_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let p = dir.join("rhapsodyd");
    std::fs::write(&p, b"not an executable").expect("write");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).expect("chmod"); // no exec bit
    let sup = Supervisor::new(fast_options(p.to_str().unwrap(), &[]));

    let err = sup.start(cancel_after(Duration::from_secs(2))).await;
    assert!(err.is_err(), "want an error for a non-executable binary");
    assert_eq!(
        sup.status().restarts,
        0,
        "a non-executable binary must not enter the restart loop"
    );
}

// TestBuildCmdSetsProcessGroup (behavioral): the daemon is launched as its own process-group leader
// so the supervisor can signal the WHOLE group on stop — preventing orphaned processes on quit.
// setpgid(0,0) in the pre_exec hook makes getpgid(pid) == pid.
#[tokio::test]
async fn launched_daemon_leads_its_own_process_group() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &[]));
    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("start");
    let pid = sup.status().pid;
    assert!(pid > 0);
    // SAFETY: getpgid is a plain syscall wrapper reading the group of a live pid.
    let pgid = unsafe { libc::getpgid(pid) };
    assert_eq!(
        pgid, pid,
        "daemon must lead its own process group (pgid == pid)"
    );
    sup.stop().await;
}

// ---- STUDIO-1035: credential-bootstrap wiring -------------------------------------------------

/// A short-lived scratch dir directly under `/tmp`, for the credential-listener socket: a Unix
/// `sun_path` is capped at ~104 bytes, and the per-session `$TMPDIR` on macOS is long enough on its
/// own to blow that budget (the same reason `credential_bootstrap`'s own socket tests do this).
struct ShortDir {
    path: PathBuf,
}

impl ShortDir {
    fn new(prefix: &str) -> ShortDir {
        let path = PathBuf::from("/tmp").join(format!("{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create short dir");
        ShortDir { path }
    }
}

impl std::ops::Deref for ShortDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for ShortDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A read-only in-memory `CredentialOwner` double: one Present binding at a fixed revision. The
/// lifecycle test only needs the listener to serve and record a revision; the production CAS owner
/// is exercised by `provider_credential`'s own tests.
struct FixedOwner {
    binding: Binding,
    revision: Revision,
}

impl CredentialOwner for FixedOwner {
    fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
        let state = if expected_binding == &self.binding {
            CredentialState::Present(BoundCredentialLease::new(
                self.binding.clone(),
                "sk-lifecycle-fake".to_string(),
            ))
        } else {
            CredentialState::BindingMismatch
        };
        CredentialRead {
            revision: self.revision,
            state,
        }
    }
    fn connect(
        &self,
        _expected_revision: Revision,
        _binding: Binding,
        _value: String,
    ) -> Result<MutationOutcome, MutationError> {
        Err(MutationError::PreconditionFailed)
    }
    fn replace(
        &self,
        _expected_revision: Revision,
        _current_binding: &Binding,
        _new_value: String,
    ) -> Result<MutationOutcome, MutationError> {
        Err(MutationError::PreconditionFailed)
    }
    fn rebind(
        &self,
        _expected_revision: Revision,
        _new_binding: Binding,
    ) -> Result<MutationOutcome, MutationError> {
        Err(MutationError::PreconditionFailed)
    }
    fn remove(&self, _expected_revision: Revision) -> Result<MutationOutcome, MutationError> {
        Err(MutationError::PreconditionFailed)
    }
    fn status(&self) -> CredentialStatus {
        CredentialStatus::Configured
    }
}

/// Routes exactly one account to one owner.
struct FixedLookup {
    account: String,
    owner: Arc<dyn CredentialOwner>,
}

impl CredentialOwnerLookup for FixedLookup {
    fn owner_for(&self, account: &str) -> Option<Arc<dyn CredentialOwner>> {
        (account == self.account).then(|| self.owner.clone())
    }
}

/// One authenticated `Hello` + `ReadBound`, returning the served revision and state tag. A wrong
/// token makes the listener close silently, which surfaces here as an error/Eof after the timeout.
async fn authenticated_read(
    socket_path: &str,
    token: &str,
    account: &str,
    binding: Binding,
) -> Result<(Revision, CredentialStateTag), String> {
    let run = async {
        let mut stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| e.to_string())?;
        let mut session = ClientSession::new(Token::new(token.to_string()));
        write_frame(
            &mut stream,
            &HelloFrame {
                token: session.hello_token().to_string(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let seq = session.next_outgoing_seq();
        write_frame(
            &mut stream,
            &ClientFrame::ReadBound {
                seq,
                account: account.to_string(),
                expected_binding: binding,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        loop {
            let frame: ServerFrame = read_frame(&mut stream).await.map_err(|e| e.to_string())?;
            match frame {
                ServerFrame::ReadBoundResult {
                    seq,
                    revision,
                    state,
                    ..
                } => {
                    session.accept_server_seq(seq).map_err(|e| e.to_string())?;
                    return Ok((revision, state));
                }
                ServerFrame::RevisionChanged { seq, .. } => {
                    session.accept_server_seq(seq).map_err(|e| e.to_string())?;
                }
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(3), run).await {
        Ok(result) => result,
        Err(_) => Err("timed out waiting for the credential listener".to_string()),
    }
}

/// The supervisor spawns the fake daemon with `--credential-bootstrap` and a piped stdin carrying
/// the ONE bootstrap frame, serves the authenticated credential channel it names from the configured
/// owner lookup, and records exactly the served revision into the shared observations map
/// (STUDIO-1035). An unauthenticated read is never recorded.
///
/// MUTATION GUARDS: drop the `--credential-bootstrap` arg / the frame write and the capture file
/// never appears; serve a fresh owner instead of the lookup's and the Present read fails; record on
/// connection rather than after delivery and the wrong-token read records a revision too.
#[tokio::test]
async fn supervisor_serves_the_credential_channel_and_records_observed_revisions() {
    let bin = fake_daemon();
    let socket_dir = ShortDir::new("rd-sup-cred");
    let capture = TempDir::new();
    let capture_path = capture.join("bootstrap.frame");
    let account = "v1:lifecycle-test".to_string();
    let binding = Binding {
        provider_id: "lifecycle-test".into(),
        adapter: "openai-chat-completions-bearer-v1".into(),
        base_url: "https://api.example/v1".into(),
    };
    let owner: Arc<dyn CredentialOwner> = Arc::new(FixedOwner {
        binding: binding.clone(),
        revision: Revision(7),
    });
    let observations = Arc::new(ChannelObservations::new());

    let options = Options {
        credential_bootstrap: Some(CredentialBootstrap {
            socket_dir: socket_dir.to_path_buf(),
            lookup: Arc::new(FixedLookup {
                account: account.clone(),
                owner,
            }),
            observations: observations.clone(),
        }),
        ..fast_options(
            bin,
            &[format!(
                "FAKE_CAPTURE_BOOTSTRAP={}",
                capture_path.to_str().expect("utf-8 capture path")
            )],
        )
    };
    let sup = Supervisor::new(options);
    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("start");

    // The supervisor must have piped and written the frame; poll for the capture file.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !capture_path.exists() {
        assert!(
            Instant::now() < deadline,
            "supervisor never wrote a credential bootstrap frame to the child's stdin"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let bytes = std::fs::read(&capture_path).expect("read captured frame");
    // Canary: the bootstrap frame carries the socket path + one-time token and NOTHING else — never
    // the credential the owner would return. The frame is also on disk as a test artifact here.
    assert!(
        !String::from_utf8_lossy(&bytes).contains("sk-lifecycle-fake"),
        "the bootstrap frame must never carry the credential"
    );
    let mut cursor: &[u8] = &bytes;
    let message: BootstrapMessage = read_frame(&mut cursor)
        .await
        .expect("captured bytes are one length-prefixed BootstrapMessage");
    // The frame names the listener's socket, bound by THIS process (the supervisor).
    let expected_socket = socket_dir.join(format!("cred-{}.sock", std::process::id()));
    assert_eq!(
        message.socket_path,
        expected_socket.to_string_lossy(),
        "the frame must name the socket the supervisor bound"
    );

    // An unauthenticated peer is rejected silently and records NOTHING.
    let wrong = authenticated_read(
        &message.socket_path,
        "a-token-nobody-issued",
        &account,
        binding.clone(),
    )
    .await;
    assert!(wrong.is_err(), "a wrong token must not be served");
    assert_eq!(
        observations.observed(&account),
        None,
        "an unauthenticated connection must never record a revision"
    );

    // The authenticated read is served from the configured owner and recorded.
    let (revision, state) = authenticated_read(
        &message.socket_path,
        &message.token,
        &account,
        binding.clone(),
    )
    .await
    .expect("an authenticated read must be served");
    assert_eq!(revision, Revision(7));
    assert_eq!(state, CredentialStateTag::Present);
    assert_eq!(
        observations.observed(&account),
        Some(Revision(7)),
        "the served revision must be recorded for the sync verdict"
    );

    sup.stop().await;
}

// TestURLReflectsChosenPort: with no explicit port the supervisor picks a free loopback port and
// exposes it as the dashboard URL (what the webview navigates to).
#[tokio::test]
async fn url_reflects_chosen_port() {
    let bin = fake_daemon();
    let sup = Supervisor::new(fast_options(bin, &[]));
    sup.start(cancel_after(Duration::from_secs(10)))
        .await
        .expect("start");

    let url = sup.url();
    assert!(
        !url.is_empty() && url != "http://127.0.0.1:0",
        "want a concrete loopback URL with the chosen port; got {url}"
    );
    assert_eq!(sup.health_url(), format!("{url}/healthz"));
    sup.stop().await;
}
