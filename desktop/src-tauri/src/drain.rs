//! drain — drain-and-restart: let in-flight runs finish before an upgrade (STUDIO-880).
//!
//! # Careful: two unrelated things in this app are called "drain"
//!
//! [`App::drain_daemon`](crate::app::App::drain_daemon) — Go's `drainDaemon` — is the QUIT path. It
//! means "stop the daemon, bounded", and it kills whatever is running. This module is the opposite:
//! it asks the DAEMON to stop taking new work and waits for what is already running to finish on its
//! own. They meet only at the end, when [`App::drain_and_restart`] restarts an already-idle daemon.
//!
//! # Why this exists
//!
//! Restarting the daemon to install an upgrade throws away the turn in flight. It cannot do
//! otherwise — the daemon owns the agent's stdio pipe, so re-attaching a live agent to a fresh
//! daemon is impossible, not merely unbuilt. Worse, the supervisor's stop escalates to **SIGKILL
//! after `stop_grace` (5s)**, and a SIGKILL means `Drop` never runs, which means the agent's
//! `KillTreeOnDrop` never runs, which means the agent and its whole process tree are ORPHANED.
//!
//! So the restart is sequenced around the one boundary the architecture has:
//!
//! 1. Ask the daemon to drain (`POST /api/v1/drain`). New dispatch stops immediately; in-flight runs
//!    finish their current turn and wind down, keeping their claims.
//! 2. Wait, bounded, for `counts.running` to reach 0.
//! 3. Only then restart — at which point there is no agent process left to kill, so the 5s grace
//!    cannot orphan anything.
//!
//! # What happens when the budget expires — the decision most likely to be fudged
//!
//! **Nothing is interrupted and the daemon is NOT restarted.** Expiry yields
//! [`DrainOutcome::Expired`], naming how many runs are still in flight, and leaves the drain ARMED
//! so the daemon keeps settling. It never silently falls through to the old behaviour, because the
//! old behaviour is precisely the data loss this feature exists to prevent — an expiry that
//! restarted anyway would make the whole thing a slower version of the bug.
//!
//! That makes the budget a policy number rather than a correctness one, which is the point: a drain
//! is bounded below by the daemon's own `claude.turn_timeout_ms` (one hour by default), so ANY
//! budget shorter than that can expire on a genuinely long turn. [`DEFAULT_DRAIN_BUDGET`] is
//! therefore chosen for how long an operator will wait, not for what is safe — expiry is safe by
//! construction. The caller then decides: wait longer, cancel the drain, or stop the daemon anyway
//! knowing exactly what that costs.
//!
//! # "A probe that failed is not idle"
//!
//! The wait reads [`App::active_run_count`](crate::app::App::active_run_count), which holds the last
//! count the daemon actually RESOLVED when a probe times out or 503s (STUDIO-551). For this caller
//! that bias is the whole safety property: an unreachable daemon must read as "still busy" and hold
//! the restart, never as "idle, go ahead".

use std::time::Duration;

use serde::Serialize;

use crate::app::App;
use crate::supervisor::State;

/// How long [`App::drain_and_restart`] waits for in-flight runs to reach a turn boundary before
/// reporting [`DrainOutcome::Expired`].
///
/// A policy number, not a safety one — see the module docs. Thirty minutes covers the overwhelming
/// majority of turns while still bounding how long an operator stares at "draining…"; a turn that
/// outlasts it is reported, not interrupted.
pub const DEFAULT_DRAIN_BUDGET: Duration = Duration::from_secs(30 * 60);

/// `reason` for a drain a human asked for (the tray action, the console). Mirrors the daemon's
/// `DrainReason` wire spelling; an unrecognized value there reads as this one.
pub const REASON_OPERATOR: &str = "operator";

/// `reason` for a drain an upgrade asked for, so the console can say the daemon is settling to
/// install rather than because somebody paused it.
pub const REASON_UPDATE: &str = "update";

/// How often the wait re-reads the in-flight count. Each read is one loopback request, so a few
/// seconds keeps a 30-minute wait to a few hundred requests while still restarting promptly once the
/// last run lands.
pub const DRAIN_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// What a drain-and-restart actually did. Every variant is distinguishable on purpose: the operator
/// has to be able to tell "restarted cleanly" from "still waiting" from "could not ask".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DrainOutcome {
    /// Nothing was running, so nothing had to wait; the daemon was restarted immediately.
    AlreadyIdle,
    /// Every in-flight run reached a turn boundary within the budget and the daemon was restarted
    /// with nothing left to kill.
    Drained {
        /// How long the wait took, in seconds.
        waited_secs: u64,
    },
    /// The budget expired with work still in flight. **Nothing was interrupted and the daemon was
    /// NOT restarted**; the drain is still armed and the daemon is still settling. Cancel it with
    /// `set_drain(false)`, or wait and try again.
    Expired {
        /// How many runs were still in flight when the budget ran out.
        running: i64,
        waited_secs: u64,
    },
    /// The daemon is not running, so there is nothing to drain and nothing to restart.
    NotRunning,
    /// The drain could not be requested (the daemon did not accept `POST /api/v1/drain`), so nothing
    /// was paused and nothing was restarted. Deliberately NOT a restart-anyway: if the daemon cannot
    /// be told to stop taking work, restarting it is exactly the unprotected restart this avoids.
    RequestFailed {
        /// Why the request failed, for the operator.
        error: String,
    },
    /// The drain succeeded but the restart itself failed. The daemon is idle and un-drained work is
    /// parked; a manual start is all that is needed.
    RestartFailed { error: String },
}

impl App {
    /// Arms (`active`) or cancels a drain on the running daemon via `POST /api/v1/drain`.
    ///
    /// `Ok(true)` ⇒ the daemon reports a drain armed; `Ok(false)` ⇒ it reports none. An `Err` is a
    /// failed round trip — the daemon is unreachable or refused — and the caller must treat that as
    /// "dispatch was NOT paused", never as a drain in progress.
    pub async fn set_daemon_drain(&self, active: bool, reason: &str) -> Result<bool, String> {
        let sup = self
            .get_sup()
            .ok_or_else(|| "daemon not started".to_string())?;
        if sup.status().state != State::Running {
            return Err("daemon is not running".to_string());
        }
        let url = format!("{}/api/v1/drain", sup.url());
        let resp = self
            .http_client()
            .post(&url)
            .json(&serde_json::json!({ "active": active, "reason": reason }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("daemon answered {} for {url}", resp.status()));
        }
        #[derive(serde::Deserialize)]
        struct Body {
            active: bool,
        }
        Ok(resp.json::<Body>().await.map_err(|e| e.to_string())?.active)
    }

    /// Drain the daemon and WAIT, bounded, for every in-flight run to reach a turn boundary —
    /// without restarting anything.
    ///
    /// This is the half the in-app updater needs: it is about to replace and relaunch the whole app,
    /// so restarting the daemon first would be pointless churn. [`Self::drain_and_restart`] is this
    /// plus the restart.
    ///
    /// A success is [`DrainOutcome::AlreadyIdle`] or [`DrainOutcome::Drained`]; every other variant
    /// means the caller must NOT proceed as if the daemon were idle.
    ///
    /// `reason` ([`REASON_OPERATOR`] / [`REASON_UPDATE`]) is what `/api/v1/state` reports and what
    /// the console banner renders, so it is the CALLER's to supply: telling an operator who paused
    /// the daemon themselves that it is "draining for an update" is the one thing this annotation
    /// exists to get right.
    pub async fn drain_and_wait(&self, budget: Duration, reason: &str) -> DrainOutcome {
        match self.get_sup() {
            Some(sup) if sup.status().state == State::Running => {}
            _ => return DrainOutcome::NotRunning,
        }
        // Ask first, THEN look at the count. The other order has a hole: a run dispatched between
        // reading "0 running" and arming the drain would be killed by the restart.
        if let Err(e) = self.set_daemon_drain(true, reason).await {
            return DrainOutcome::RequestFailed { error: e };
        }
        let app = self.clone();
        match wait_for_idle(budget, DRAIN_POLL_INTERVAL, move || {
            let app = app.clone();
            async move { app.active_run_count().await }
        })
        .await
        {
            Ok(waited) if waited.is_zero() => DrainOutcome::AlreadyIdle,
            Ok(waited) => DrainOutcome::Drained {
                waited_secs: waited.as_secs(),
            },
            Err((running, waited)) => DrainOutcome::Expired {
                running,
                waited_secs: waited.as_secs(),
            },
        }
    }

    /// Drain the daemon and restart it once nothing is in flight — the safe upgrade sequence.
    ///
    /// See the module docs for what each outcome means, and in particular for why an expired budget
    /// restarts nothing: the restart happens ONLY on a drained or already-idle daemon, so the
    /// supervisor's 5-second SIGKILL grace has no live agent to orphan.
    pub async fn drain_and_restart(&self, budget: Duration, reason: &str) -> DrainOutcome {
        let drained = self.drain_and_wait(budget, reason).await;
        match drained {
            DrainOutcome::AlreadyIdle | DrainOutcome::Drained { .. } => {}
            // Expired / NotRunning / RequestFailed: nothing is idle, so nothing is restarted.
            other => return other,
        }
        if let Err(e) = self.restart_daemon().await {
            return DrainOutcome::RestartFailed {
                error: e.to_string(),
            };
        }
        drained
    }
}

/// Whether a [`App::drain_and_restart`] outcome actually restarted the daemon.
///
/// Reads that function's contract rather than restating a policy: only `AlreadyIdle` and `Drained`
/// reach the restart at all, and `Drained` is returned unchanged only when it succeeded. Every other
/// variant means the daemon is still up and still on the old build — and for [`DrainOutcome::Expired`]
/// still DRAINING, because an expired budget deliberately leaves the drain armed. `NotRunning` is not
/// a restart either: there was nothing to restart, and an operator who asked for one should be told
/// that rather than left to assume.
pub fn restarted(outcome: &DrainOutcome) -> bool {
    matches!(
        outcome,
        DrainOutcome::AlreadyIdle | DrainOutcome::Drained { .. }
    )
}

/// Waits for `count` to report zero, giving up after `budget`.
///
/// `Ok(waited)` ⇒ it reached zero (a zero `waited` means it already was). `Err((last, waited))` ⇒
/// the budget expired with `last` still in flight.
///
/// Split out from [`App::drain_and_restart`] so the timing policy — the FIRST read happens before
/// any sleep, the budget is measured from entry, and expiry reports the last count rather than
/// assuming idle — is testable without a daemon, a supervisor or a clock.
pub(crate) async fn wait_for_idle<F, Fut>(
    budget: Duration,
    poll: Duration,
    count: F,
) -> Result<Duration, (i64, Duration)>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = i64>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + budget;
    loop {
        let n = count().await;
        if n <= 0 {
            return Ok(started.elapsed());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            // The last count, never an assumed-idle 0: an expiry that reported zero would be
            // indistinguishable from a successful drain, which is the exact confusion this whole
            // outcome type exists to prevent.
            return Err((n, started.elapsed()));
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    //! The wait's timing policy, driven on a paused clock so a 30-minute budget costs no wall time.
    //! The daemon round trips (`POST /api/v1/drain`, the restart) belong to `App` and are covered by
    //! its own tests; what is asserted here is the decision this module owns — when to stop waiting,
    //! and what to report when it does.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use super::*;

    /// A counter that answers `counts[i]` on the i-th call and repeats the last value forever.
    fn scripted(counts: &[i64]) -> (impl Fn() -> std::future::Ready<i64>, Arc<AtomicUsize>) {
        let counts: Arc<Vec<i64>> = Arc::new(counts.to_vec());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let f = move || {
            let i = calls2.fetch_add(1, Ordering::SeqCst);
            let last = counts.len().saturating_sub(1);
            std::future::ready(counts[i.min(last)])
        };
        (f, calls)
    }

    // The desktop cannot import the daemon's `DrainReason`, so these two spellings are a hand-copied
    // cross-process contract. A drift is SILENT — the daemon's parse is total and degrades anything
    // it does not recognize to "operator" — so a renamed constant would quietly relabel every
    // update drain as an operator pause rather than failing anywhere.
    #[test]
    fn the_reason_spellings_are_the_daemons_own_vocabulary() {
        assert_eq!(REASON_OPERATOR, "operator");
        assert_eq!(REASON_UPDATE, "update");
    }

    // An idle daemon is not made to wait: the FIRST read happens before any sleep, so a restart of
    // an idle daemon is immediate rather than one poll interval late.
    #[tokio::test(start_paused = true)]
    async fn an_idle_daemon_returns_at_once() {
        let (count, calls) = scripted(&[0]);
        let waited = wait_for_idle(DEFAULT_DRAIN_BUDGET, DRAIN_POLL_INTERVAL, count).await;
        assert_eq!(waited, Ok(Duration::ZERO));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one read, no sleep");
    }

    // The ordinary case: runs land one by one and the wait ends the moment the last one does.
    #[tokio::test(start_paused = true)]
    async fn it_returns_as_soon_as_the_last_run_lands() {
        let (count, calls) = scripted(&[2, 2, 1, 0]);
        let waited = wait_for_idle(DEFAULT_DRAIN_BUDGET, DRAIN_POLL_INTERVAL, count)
            .await
            .expect("the count reached zero inside the budget");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            waited,
            DRAIN_POLL_INTERVAL * 3,
            "it waited exactly the three polls it needed, and not a fourth"
        );
    }

    // THE decision: an expired budget reports the runs still in flight. It must never answer `Ok`,
    // and it must never report 0 — either would be indistinguishable from a successful drain, and
    // the caller would restart into live work. This is the branch that never runs in practice and so
    // is the one most likely to be wrong.
    #[tokio::test(start_paused = true)]
    async fn an_expired_budget_reports_the_runs_still_in_flight() {
        let budget = Duration::from_secs(30);
        let (count, _) = scripted(&[3]);
        let out = wait_for_idle(budget, DRAIN_POLL_INTERVAL, count).await;
        match out {
            Err((running, waited)) => {
                assert_eq!(running, 3, "the LAST count, never an assumed-idle zero");
                assert!(
                    waited >= budget,
                    "it waited out the whole budget before giving up, got {waited:?}"
                );
            }
            Ok(w) => panic!("a busy daemon must not report a clean drain (waited {w:?})"),
        }
    }

    // The wait never overruns its budget waiting out a poll interval: the final sleep is clamped to
    // the deadline, so a 1-second budget with a 3-second poll still gives up at 1 second.
    #[tokio::test(start_paused = true)]
    async fn the_last_sleep_is_clamped_to_the_deadline() {
        let budget = Duration::from_secs(1);
        let (count, _) = scripted(&[1]);
        let started = tokio::time::Instant::now();
        let out = wait_for_idle(budget, Duration::from_secs(30), count).await;
        assert!(out.is_err(), "a busy daemon expires");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait overran its budget by a whole poll interval: {:?}",
            started.elapsed()
        );
    }

    // A zero budget is still one honest read — "is it idle right now?" — and not an instant refusal.
    #[tokio::test(start_paused = true)]
    async fn a_zero_budget_still_reads_once() {
        let (count, calls) = scripted(&[0]);
        assert_eq!(
            wait_for_idle(Duration::ZERO, DRAIN_POLL_INTERVAL, count).await,
            Ok(Duration::ZERO)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // A probe that cannot resolve holds the LAST count it did (STUDIO-551), which for this caller
    // means the wait keeps waiting rather than reading an unreachable daemon as idle. Modelled here
    // the way `App::active_run_count` behaves: the held value is returned, never zero.
    #[tokio::test(start_paused = true)]
    async fn an_unreachable_daemon_reads_as_busy_not_idle() {
        let held = Arc::new(AtomicI64::new(1));
        let held2 = Arc::clone(&held);
        // Every probe "fails", so the held last-resolved count of 1 is what the caller sees.
        let count = move || std::future::ready(held2.load(Ordering::SeqCst));
        let out = wait_for_idle(Duration::from_secs(10), DRAIN_POLL_INTERVAL, count).await;
        assert_eq!(
            out.map_err(|(n, _)| n),
            Err(1),
            "an unreachable daemon must hold the restart, never wave it through"
        );
    }

    // STUDIO-880: the tray asks for a drain-and-restart and then has to decide whether anything
    // visible happened. A drain that did NOT restart leaves the daemon Running — so the tray still
    // reads Running — and an expired one leaves it drained and taking no work at all. Getting this
    // predicate wrong is the difference between surfacing that and an `eprintln!` nobody reads.
    #[test]
    fn only_a_settled_daemon_is_ever_actually_restarted() {
        assert!(restarted(&DrainOutcome::AlreadyIdle));
        assert!(restarted(&DrainOutcome::Drained { waited_secs: 7 }));
        assert!(
            !restarted(&DrainOutcome::Expired {
                running: 2,
                waited_secs: 1800
            }),
            "an expired budget restarts NOTHING and leaves the drain armed"
        );
        assert!(
            !restarted(&DrainOutcome::NotRunning),
            "there was nothing to restart — not the same as having restarted it"
        );
        assert!(!restarted(&DrainOutcome::RequestFailed {
            error: "connection refused".into()
        }));
        assert!(
            !restarted(&DrainOutcome::RestartFailed {
                error: "spawn failed".into()
            }),
            "the restart was attempted and FAILED — the daemon is still on the old build"
        );
    }
}
