//! Quit-path integration tests (STUDIO-1116): quitting the app must always finish.
//!
//! `RunEvent::Exit` runs on the macOS main thread, and the hang it suffered needs two things at once,
//! both reproduced here headlessly against the real compiled `fakedaemon`:
//!
//! - a multi-thread tokio runtime built the way `tauri::async_runtime` builds its own
//!   (`Runtime::new()`), whose I/O + time driver only a parked WORKER thread ever polls — a thread
//!   blocked in `block_on` from outside the runtime never does;
//! - the tray's 2s refresh task, which a timer wakes on the very worker that holds the driver, and
//!   which then marshals each `MenuItem`/`TrayIcon` setter onto the main thread and blocks that
//!   worker until the main thread runs it (tauri's `run_item_main_thread!`).
//!
//! Once the main thread is inside the Exit handler, that worker stays blocked and nobody re-takes the
//! driver: no timer fires and no child exit is ever observed, so a quit waiting on either never ends.
//! [`MainThread`] plays the main thread's event loop and [`spawn_tray_refresh`] the tray loop.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use rhapsody_desktop::app::{App, CloseDecision, ShutdownWait};
use rhapsody_desktop::supervisor::{Options, State, Supervisor};

mod support;
use support::SupervisorGuard;

/// The wall-clock bound the tests give the quit — the production one.
const BOUND: Duration = rhapsody_desktop::app::SHUTDOWN_WAIT_BOUND;

/// How long the test waits AFTER setup reaches the Exit handler before calling a quit hung.
/// Runtime creation, child startup, and pre-quit pumping have their own setup watchdog.
const HUNG_AFTER: Duration = Duration::from_secs(30);

const SETUP_LIMIT: Duration = Duration::from_secs(120);

/// How long `fakedaemon` takes to finish shutting down after SIGTERM — like the real daemon, which
/// spent five seconds draining in the incident. It must comfortably outlast one tray tick so the tray
/// blocks a worker while the stop is still in flight.
const SLOW_STOP_MS: &str = "1500";

/// Serializes this file's tests. tokio's signal handling is process-global: a child exiting under
/// one test's runtime wakes signal listeners in every runtime in the process, which can rescue
/// another test's starved runtime by accident. The real app has exactly one runtime, so each scenario
/// runs alone to match it.
static ALONE: Mutex<()> = Mutex::new(());

fn alone() -> MutexGuard<'static, ()> {
    ALONE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The tray refresh period. Production is 2s; shorter here so a tick always lands mid-stop.
const TRAY_TICK: Duration = Duration::from_millis(50);

type Job = Box<dyn FnOnce() + Send>;

/// A stand-in for the main thread's event loop: work marshalled onto it runs only while it pumps.
struct MainThread {
    rx: mpsc::Receiver<Job>,
}

impl MainThread {
    /// Runs queued main-thread work for `dur` — the event loop idling before the quit.
    fn pump_for(&self, dur: Duration) {
        let until = Instant::now() + dur;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if let Ok(job) = self.rx.recv_timeout(left) {
                job();
            }
        }
    }

    /// Runs queued main-thread work until `cond` holds, or `limit` passes.
    fn pump_until(&self, limit: Duration, cond: impl Fn() -> bool) -> bool {
        let until = Instant::now() + limit;
        while !cond() {
            if Instant::now() >= until {
                return false;
            }
            if let Ok(job) = self.rx.recv_timeout(Duration::from_millis(10)) {
                job();
            }
        }
        true
    }
}

/// Marshals a no-op onto the main thread and blocks the calling thread until it has run — what each
/// tray setter does through tauri's `run_on_main_thread` + a synchronous `recv`.
fn run_on_main_thread(main: &mpsc::Sender<Job>) {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let job: Job = Box::new(move || {
        let _ = done_tx.send(());
    });
    if main.send(job).is_ok() {
        let _ = done_rx.recv();
    }
}

/// The tray refresh loop's shape (`tray.rs`): a timer wakes it, then it blocks on the main thread.
fn spawn_tray_refresh(runtime: &tokio::runtime::Runtime, main: mpsc::Sender<Job>) {
    runtime.spawn(async move {
        let mut ticker = tokio::time::interval(TRAY_TICK);
        loop {
            ticker.tick().await;
            run_on_main_thread(&main);
        }
    });
}

/// Supervisor options for a `fakedaemon` with the given extra env entries.
fn daemon_options(extra_env: &[&str]) -> Options {
    let mut env = vec!["PATH=/usr/bin:/bin".to_string()];
    env.extend(extra_env.iter().map(|s| (*s).to_string()));
    Options {
        binary_path: PathBuf::from(env!("CARGO_BIN_EXE_fakedaemon")),
        base_env: Some(env),
        startup_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(15),
        stop_grace: Duration::from_secs(5),
        max_restarts: 1,
        backoff: Some(std::sync::Arc::new(|_: i64| Duration::from_millis(15))),
        ..Default::default()
    }
}

/// An app supervising a running `fakedaemon` (no WORKFLOW.md, so nothing else launches).
fn app_with_daemon(runtime: &tokio::runtime::Runtime, extra_env: &[&str]) -> App {
    let app = App::new(None, PathBuf::new());
    let sup = Supervisor::new(daemon_options(extra_env));
    runtime
        .block_on(async { sup.start(tokio::time::sleep(Duration::from_secs(5))).await })
        .expect("fakedaemon should start");
    app.set_sup(sup);
    app
}

/// What a quit scenario reports back once its "main thread" got through the Exit handler.
struct Quit {
    wait: ShutdownWait,
    elapsed: Duration,
    app: App,
}

/// Runs one quit on a fresh "main thread": builds the runtime + tray loop, lets `before_exit` set the
/// scene (start the daemon, begin a drain, …) while the event loop pumps, then performs the Exit
/// handler's shutdown exactly as `main.rs` does. Returns `None` if the quit hung.
fn quit_scenario(
    before_exit: impl FnOnce(&tokio::runtime::Runtime, &MainThread) -> App + Send + 'static,
) -> Option<Quit> {
    let _alone = alone();
    let (result_tx, result_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    // Setup and Exit have separate watchdogs; the caller owns the held setter and a stop guard so
    // even a failing quit releases the worker and terminates the child.
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let (main_tx, main_rx) = mpsc::channel::<Job>();
        let main = MainThread { rx: main_rx };
        let app = before_exit(&runtime, &main);
        let _guard = app.get_sup().map(|sup| SupervisorGuard::new(&sup));
        // Startup uses block_on before the fake main loop pumps. A tray setter here would starve
        // readiness instead of testing Exit (the false "quit hung" in STUDIO-1268).
        spawn_tray_refresh(&runtime, main_tx);
        // Idle a few tray ticks so the loop is demonstrably healthy before the quit.
        main.pump_for(TRAY_TICK * 4);

        // RunEvent::Exit: the main thread stops pumping. Within a tick the tray loop marshals its next
        // setter here and blocks the worker that ran it — the one holding the driver. The event queue
        // holds that setter until the Exit handler returns; let the runtime settle into that state.
        let held = main.rx.recv_timeout(TRAY_TICK * 20);
        std::thread::sleep(TRAY_TICK);

        let _ = ready_tx.send((app.clone(), held));
        let started = Instant::now();
        let wait = app.on_shutdown_blocking(runtime.handle(), BOUND);
        let elapsed = started.elapsed();
        let _ = result_tx.send(Quit { wait, elapsed, app });

        drop(main);
        runtime.shutdown_background();
    });
    let (app, held) = ready_rx
        .recv_timeout(SETUP_LIMIT)
        .expect("quit scenario setup failed to reach Exit; the shutdown bound was not exercised");
    let _guard = app.get_sup().map(|sup| SupervisorGuard::new(&sup));
    let result = result_rx.recv_timeout(HUNG_AFTER).ok();
    if let Ok(job) = held {
        job();
    }
    result
}

/// Asserts the quit finished in time, and its shutdown task really ran to completion.
fn assert_completed(quit: Option<Quit>, path: &str) -> Quit {
    let quit = quit.unwrap_or_else(|| {
        panic!("{path}: the quit hung — Exit's shutdown never returned within {HUNG_AFTER:?}")
    });
    assert_eq!(
        quit.wait,
        ShutdownWait::Completed,
        "{path}: the shutdown should finish inside the bound, not be cut off by it"
    );
    assert!(
        quit.elapsed < BOUND,
        "{path}: the quit took {:?}, past the {BOUND:?} bound",
        quit.elapsed
    );
    quit
}

/// Asserts the supervised daemon was really stopped by the quit.
fn assert_daemon_stopped(app: &App, path: &str) {
    let sup = app.get_sup().expect("the app keeps its supervisor");
    assert_eq!(
        sup.status().state,
        State::Stopped,
        "{path}: the quit must leave the daemon stopped"
    );
}

// ⌘Q / the app-menu Quit: `terminate:` goes straight to applicationWillTerminate → RunEvent::Exit,
// never through ExitRequested, so no drain was started and the Exit handler does the stop itself.
// This is the incident's path: the daemon got SIGTERM and exited, and the app never noticed.
#[test]
fn app_menu_quit_with_a_running_daemon_finishes() {
    let quit = quit_scenario(|runtime, _main| {
        app_with_daemon(runtime, &[&format!("FAKE_STOP_DELAY_MS={SLOW_STOP_MS}")])
    });
    let quit = assert_completed(quit, "app-menu quit");
    assert_daemon_stopped(&quit.app, "app-menu quit");
}

// A second Quit while the "Shutting down…" overlay is up: the tray Quit already started the drain
// (ExitRequested → StartDrain), and ⌘Q now lands on RunEvent::Exit mid-drain. The Exit handler waits
// on that drain, which must still be able to finish.
#[test]
fn second_quit_during_the_drain_finishes() {
    let quit = quit_scenario(|runtime, _main| {
        let app = app_with_daemon(runtime, &[&format!("FAKE_STOP_DELAY_MS={SLOW_STOP_MS}")]);
        assert_eq!(app.on_before_close(), CloseDecision::StartDrain);
        let drain = app.clone();
        runtime.spawn(async move { drain.drain_daemon(Duration::from_secs(10)).await });
        // A tray Quit during the drain is vetoed (the overlay stays up) rather than starting another.
        assert_eq!(app.on_before_close(), CloseDecision::WaitForDrain);
        app
    });
    let quit = assert_completed(quit, "second quit during the drain");
    assert_daemon_stopped(&quit.app, "second quit during the drain");
    assert_eq!(
        quit.app.on_before_close(),
        CloseDecision::Proceed,
        "the drain must have completed, releasing the final quit"
    );
}

// The tray Quit: ExitRequested starts the drain off the main thread while the event loop keeps
// running; the drain's re-issued exit then proceeds and RunEvent::Exit returns at once.
#[test]
fn tray_quit_drains_then_exits() {
    let quit = quit_scenario(|runtime, main| {
        let app = app_with_daemon(runtime, &[&format!("FAKE_STOP_DELAY_MS={SLOW_STOP_MS}")]);
        assert_eq!(app.on_before_close(), CloseDecision::StartDrain);
        let drain = app.clone();
        let (drained_tx, drained_rx) = mpsc::channel();
        runtime.spawn(async move {
            drain.drain_daemon(Duration::from_secs(10)).await;
            let _ = drained_tx.send(()); // main.rs re-issues `h.exit(0)` here
        });
        assert!(
            main.pump_until(Duration::from_secs(15), || drained_rx.try_recv().is_ok()),
            "the drain should finish while the event loop is running"
        );
        assert_eq!(
            app.on_before_close(),
            CloseDecision::Proceed,
            "the re-issued exit must be let through"
        );
        app
    });
    let quit = assert_completed(quit, "tray quit");
    assert_daemon_stopped(&quit.app, "tray quit");
    assert!(
        quit.elapsed < Duration::from_secs(2),
        "the drain already finished, so the Exit handler must return at once (took {:?})",
        quit.elapsed
    );
}

// Quit when the daemon has already exited on its own (and used up its restarts).
#[test]
fn quit_after_the_daemon_already_exited_finishes() {
    let quit = quit_scenario(|runtime, main| {
        let app = app_with_daemon(runtime, &["FAKE_EXIT_AFTER_MS=100"]);
        let sup = app.get_sup().expect("supervisor set");
        assert!(
            main.pump_until(Duration::from_secs(10), || sup.status().state
                == State::Stopped),
            "the supervisor should give up once the daemon keeps exiting"
        );
        app
    });
    assert_completed(quit, "quit after the daemon exited");
}

// Quit when the daemon ignores SIGTERM: the supervisor's wall-clock SIGKILL backstop still fires,
// and the quit finishes.
#[test]
fn quit_with_a_hung_daemon_finishes() {
    let quit = quit_scenario(|runtime, _main| {
        let app = App::new(None, PathBuf::new());
        let mut opts = daemon_options(&["FAKE_STOP_DELAY_MS=600000"]);
        opts.stop_grace = Duration::from_secs(1);
        let sup = Supervisor::new(opts);
        runtime
            .block_on(async { sup.start(tokio::time::sleep(Duration::from_secs(5))).await })
            .expect("fakedaemon should start");
        app.set_sup(sup);
        app
    });
    let quit = assert_completed(quit, "quit with a hung daemon");
    assert_daemon_stopped(&quit.app, "quit with a hung daemon");
}

// The bound itself: with the runtime unable to run ANYTHING (its only worker is blocked), the quit
// must still return once the wall-clock bound passes. A `tokio::time::timeout` cannot do this — it is
// exactly the bound that failed in the incident.
#[test]
fn the_shutdown_wait_is_bounded_even_when_the_runtime_is_wedged() {
    let _alone = alone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let (wedge_tx, wedge_rx) = mpsc::channel::<()>();
    let (wedged_tx, wedged_rx) = mpsc::channel::<()>();
    runtime.spawn(async move {
        let _ = wedged_tx.send(());
        let _ = wedge_rx.recv(); // blocks the only worker until the test releases it
    });
    wedged_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the wedge task should start");

    let app = App::new(None, PathBuf::new());
    app.set_sup(Supervisor::new(Options::default()));
    // A drain is "in flight" that nothing will ever finish.
    assert_eq!(app.on_before_close(), CloseDecision::StartDrain);

    let (result_tx, result_rx) = mpsc::channel();
    let handle = runtime.handle().clone();
    let quitting = app.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let wait = quitting.on_shutdown_blocking(&handle, Duration::from_millis(300));
        let _ = result_tx.send((wait, started.elapsed()));
    });
    let (wait, elapsed) = result_rx
        .recv_timeout(HUNG_AFTER)
        .expect("the quit hung on a wedged runtime — its bound depends on the runtime");
    assert_eq!(wait, ShutdownWait::TimedOut);
    assert!(
        elapsed < Duration::from_secs(3),
        "the bound is 300ms; the wait took {elapsed:?}"
    );

    let _ = wedge_tx.send(());
    runtime.shutdown_background();
}

// Unlike the original scenarios, this holds the ONLY worker behind an acknowledged timer-woken
// tray setter. It cannot be rescued by an idle worker or a SIGCHLD on another runtime. Neither the
// quit deadline nor SIGKILL/reaping may require that worker to make progress (STUDIO-1268).
#[test]
fn quit_bound_holds_when_the_driver_worker_is_blocked() {
    blocked_driver_quit(false);
}

#[test]
fn second_quit_bound_holds_when_the_driver_worker_is_blocked() {
    blocked_driver_quit(true);
}

fn blocked_driver_quit(drain_started: bool) {
    let _alone = alone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let mut opts = daemon_options(&["FAKE_STOP_DELAY_MS=600000"]);
    opts.stop_grace = Duration::from_secs(1);
    let sup = Supervisor::new(opts);
    let guard = SupervisorGuard::new(&sup);
    runtime
        .block_on(async { sup.start(tokio::time::sleep(Duration::from_secs(15))).await })
        .expect("fakedaemon should start");
    let pid = sup.status().pid;
    assert!(pid > 0);
    let app = App::new(None, PathBuf::new());
    app.set_sup(sup.clone());
    if drain_started {
        assert_eq!(app.on_before_close(), CloseDecision::StartDrain);
    }

    let (main_tx, main_rx) = mpsc::channel::<Job>();
    runtime.spawn(async move {
        tokio::time::sleep(TRAY_TICK).await;
        run_on_main_thread(&main_tx);
    });
    let setter = main_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timer-woken tray setter should reach the main thread");
    if drain_started {
        let drain = app.clone();
        runtime.spawn(async move { drain.drain_daemon(Duration::from_secs(10)).await });
    }

    let (result_tx, result_rx) = mpsc::channel();
    let handle = runtime.handle().clone();
    let started = Instant::now();
    std::thread::spawn(move || {
        let wait = app.on_shutdown_blocking(&handle, BOUND);
        let _ = result_tx.send((wait, started.elapsed()));
    });

    // Check the one-second SIGTERM grace independently of the twelve-second quit bound, without
    // pumping the setter or polling a Tokio timer. Signal 0 still sees a zombie until it is reaped.
    let kill_deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) == 0 } && Instant::now() < kill_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let child_survived_grace = unsafe { libc::kill(pid, 0) == 0 };
    let result =
        result_rx.recv_timeout((BOUND + Duration::from_secs(2)).saturating_sub(started.elapsed()));
    let child_survived_quit = unsafe { libc::kill(pid, 0) == 0 };

    // Release and clean up BEFORE asserting: the failing-first run and runtime-timer mutation must
    // not leak their own worker, child, or quit thread.
    setter();
    drop(main_rx);
    drop(guard);
    runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(10), sup.stop()).await })
        .expect("cleanup should finish once the worker is released");
    runtime.shutdown_background();

    let (wait, elapsed) = result.expect("quit exceeded BOUND + 2s with the driver worker blocked");
    assert_eq!(wait, ShutdownWait::TimedOut);
    assert!(
        elapsed <= BOUND + Duration::from_secs(2),
        "quit took {elapsed:?}"
    );
    assert!(
        !child_survived_grace,
        "SIGKILL/reaping depended on the blocked driver: pid {pid}"
    );
    assert!(!child_survived_quit, "quit leaked fakedaemon pid {pid}");
}
