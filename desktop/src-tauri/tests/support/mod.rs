//! Shared helpers for the desktop integration tests that launch a REAL `rhapsodyd`.
//!
//! STUDIO-1038: the gated e2e tests spawn a real daemon through the supervisor, which puts the child
//! in its OWN process group (`setpgid(0, 0)` — see `supervisor/mod.rs`). A test that reaches an
//! assertion panic before its `sup.stop().await` therefore leaves the daemon running: it is
//! reparented to launchd and keeps listening on its loopback port until something kills it. Twelve
//! such orphans accumulated on the shared Mac after one review of PR #254.
//!
//! `Supervisor::stop` is async, so it cannot run from a `Drop` on a panicking unwind — the runtime
//! that drives the supervise task is no longer being polled, and every `await` there would never
//! resolve. The guard below instead terminates the daemon synchronously, at the process-group level,
//! on every exit path — the same TERM-then-KILL escalation the supervisor itself uses on a clean
//! stop. (Because the supervise task is not polled during unwind, killing the child cannot trigger a
//! restart.)

use std::time::{Duration, Instant};

use rhapsody_desktop::supervisor::Supervisor;

/// How long a [`SupervisorGuard`] waits after SIGTERM before escalating to SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Terminates a supervised daemon (and its process group) when dropped, unless the daemon was
/// already stopped.
///
/// Bind one immediately after `Supervisor::new` and before `start`, so it outlives every assertion
/// and drops before the test's temp-dir guard removes the daemon's working directory. On the clean
/// path the guard is a no-op: `stop()` clears the recorded pid.
pub struct SupervisorGuard {
    supervisor: Supervisor,
}

impl SupervisorGuard {
    pub fn new(supervisor: &Supervisor) -> SupervisorGuard {
        SupervisorGuard {
            supervisor: supervisor.clone(),
        }
    }
}

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        let pid = self.supervisor.status().pid;
        // 0 means the supervisor already reached Stopped (the clean-stop path ran); nothing to do.
        if pid <= 0 {
            return;
        }
        signal_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if !group_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        signal_group(pid, libc::SIGKILL);
    }
}

/// True while any process in the daemon's process group still exists.
fn group_alive(pid: i32) -> bool {
    // SAFETY: `kill` is a plain syscall wrapper; signal 0 performs only the existence/permission
    // check. A dead group returns -1/ESRCH.
    unsafe { libc::kill(-pid, 0) == 0 }
}

/// Signals the daemon leader AND its process group (the leader first, then `-pid`, exactly as the
/// supervisor's own `signal_group` does on a clean stop).
fn signal_group(pid: i32, sig: libc::c_int) {
    // SAFETY: `kill` is a plain syscall wrapper; a stale/reaped pid returns -1/ESRCH.
    unsafe {
        if libc::kill(pid, sig) == -1 {
            return;
        }
        let _ = libc::kill(-pid, sig);
    }
}
