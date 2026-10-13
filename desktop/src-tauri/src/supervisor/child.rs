//! Per-launch child ownership shared with the wall-clock stop backstop (STUDIO-1268). Signaling and
//! reaping take the SAME short lock, so the backstop never signals a PID an async waiter has reaped.

use std::future::{Future, poll_fn};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::{Duration, Instant};

use super::{lock, signal_group};

struct ChildState {
    child: tokio::process::Child,
    waiter: Option<Waker>,
}

pub(super) struct DaemonChild {
    state: Mutex<ChildState>,
    stopping: AtomicBool,
    stop_grace: Duration,
}

impl DaemonChild {
    pub(super) fn new(child: tokio::process::Child, stop_grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ChildState {
                child,
                waiter: None,
            }),
            stopping: AtomicBool::new(false),
            stop_grace,
        })
    }

    /// Polls the cancellation-safe Tokio wait with the lock held only for ONE poll. The wall-clock
    /// thread can meanwhile try_wait (a non-blocking waitpid), cache the exit, and wake this waiter.
    pub(super) async fn wait(&self) -> std::io::Result<ExitStatus> {
        poll_fn(|cx| {
            let mut state = lock(&self.state);
            state.waiter = Some(cx.waker().clone());
            let wait = std::pin::pin!(state.child.wait());
            wait.poll(cx)
        })
        .await
    }

    /// Arms SIGTERM → grace → SIGKILL exactly once, without needing a Tokio worker or driver. This
    /// also reaps the child when the driver is blocked; kill_on_drop alone would need task teardown.
    pub(super) fn request_stop(self: &Arc<Self>) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let deadline = Instant::now() + self.stop_grace;
        self.signal(libc::SIGTERM);
        let child = self.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("daemon-stop".to_string())
            .spawn(move || child.stop_blocking(deadline))
        {
            eprintln!("rhapsody-desktop: could not start daemon stop backstop: {e}; killing now");
            self.signal(libc::SIGKILL);
        }
    }

    fn signal(&self, sig: libc::c_int) {
        let state = lock(&self.state);
        if let Some(pid) = state.child.id() {
            // Holding the child lock excludes BOTH the async and synchronous reapers.
            signal_group(pid as i32, sig);
        }
    }

    fn stop_blocking(&self, deadline: Instant) {
        let mut killed = false;
        loop {
            let (result, waiter) = {
                let mut state = lock(&self.state);
                let result = state.child.try_wait();
                let waiter = if matches!(result, Ok(Some(_))) {
                    state.waiter.take()
                } else {
                    None
                };
                (result, waiter)
            };
            if let Some(waiter) = waiter {
                waiter.wake();
            }
            match result {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(e) => {
                    eprintln!("rhapsody-desktop: daemon stop wait failed: {e}; killing now");
                    self.signal(libc::SIGKILL);
                    return;
                }
            }
            if !killed && Instant::now() >= deadline {
                self.signal(libc::SIGKILL);
                killed = true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
