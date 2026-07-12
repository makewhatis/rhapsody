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

use rhapsody_desktop::supervisor::{Options, StartError, State, Supervisor};

/// Path to the compiled fakedaemon stand-in for symphonyd (Cargo builds it for us).
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

/// A unique temp path (parent exists) that a test may create a file under.
fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rhapsody-d2-sup-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(name)
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
    let marker = temp_path("crash.marker");
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
    let p = temp_path("symphonyd");
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
