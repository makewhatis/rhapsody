//! stop — parity port of Go `internal/orchestrator/stop.go` (the operator Stop / Resume flows).
//!
//! `StopRun` kills a live agent, records the run `stopped`, and moves its ticket to the team's Backlog
//! so the daemon won't re-dispatch it; `ResumeRun` moves a stopped ticket back to Todo and clears the
//! in-memory suppression. The kill + bookkeeping happen ON the control task (as `evStopRun` / `evResume`
//! handled by [`Orchestrator::handle`]); the slow Linear move runs off the loop.
//!
//! Go reaches these off-loop by calling `o.StopRun` on the shared `*Orchestrator` pointer while the
//! control goroutine mutates state. Rust cannot alias `&mut self` (the loop) with an off-loop `&self`,
//! so the off-loop surface is a cloneable [`ControlHandle`] (obtained via [`Orchestrator::control`])
//! that carries only thread-safe handles — the control-event sender, the lifetime cancellation, and
//! the tracker + store the off-loop moves need. This matches the OBSERVABLE behavior the stop tests
//! assert (the P5 plan's "single owning task + mpsc; semantics over structure" adaptation).

/// The HTTP-layer result of a Stop (Go `StopResult`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StopResult {
    /// The `run_id` had no live worker (already finished) ⇒ 409.
    pub not_running: bool,
    /// Human ticket id, e.g. `"INF-217"`.
    pub identifier: String,
    /// The Backlog state name the ticket was moved to (`""` if the move failed).
    pub moved_to: String,
    /// Non-empty when the agent was killed but the Backlog move failed.
    pub move_err: String,
}

/// The HTTP-layer result of a Resume (Go `ResumeResult`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeResult {
    /// No such run row ⇒ 404.
    pub not_found: bool,
    /// The run row lacks a team id (pre-v4 row) ⇒ can't move ⇒ 409.
    pub no_team: bool,
    /// The run's outcome isn't `stopped` (only stopped runs are resumable) ⇒ 409.
    pub not_stopped: bool,
    /// A NEWER run for the same issue is currently executing ⇒ 409.
    pub live_run: bool,
    /// A NEWER run for the same issue already FINISHED non-stopped ⇒ 409.
    pub superseded: bool,
    pub identifier: String,
    pub moved_to: String,
    pub move_err: String,
}

/// The control-task reply for `evStopRun` (Go `stopPlan`): whether a live run was found + the
/// issue/team the off-loop Backlog move targets.
#[derive(Debug, Clone, Default)]
pub struct StopPlan {
    pub found: bool,
    pub issue_id: String,
    pub team_id: String,
    pub identifier: String,
}

/// The control-task reply for `evResume` (Go `resumePlan`): the admission decision. `live` / `superseded`
/// abort the resume; `err` surfaces a store-read failure as `resume_failed`.
#[derive(Debug, Default)]
pub struct ResumePlan {
    pub live: bool,
    pub superseded: bool,
    pub err: Option<rhapsody_store::StoreError>,
}

/// The off-loop control surface for the daemon's HTTP API — the Rust stand-in for calling
/// `o.StopRun` / `o.Snapshot` / `o.ListLinearProjects` / … on the shared `*Orchestrator` pointer
/// while the control task owns the state. Cloneable; carries only thread-safe handles: the
/// control-event sender (to drive on-loop work — the kill/admission, snapshot, refresh, message), the
/// orchestrator lifetime cancellation (the reply-wait + finalize ctx), the tracker + store the
/// off-loop Linear move / run lookup / history reads need, the shared reads cell backing the
/// read-only Linear surfaces, and the workflow path the config endpoint reads/validates. Obtain via
/// [`Orchestrator::control`].
///
/// It began (O7) as the Stop/Resume surface; F1 (the assembly) grows it into the full off-loop
/// HTTP surface — the pieces `cmd/symphony` needs after the orchestrator moves into the control-loop
/// task, since the daemon can then no longer reach the loop-owned `&self` read methods directly.
#[derive(Clone)]
pub struct ControlHandle {
    pub(crate) events: tokio::sync::mpsc::UnboundedSender<crate::control_loop::Event>,
    /// The read side of the orchestrator's published-snapshot cell (STUDIO-551). Read by
    /// [`ControlHandle::snapshot`] with no control-channel round-trip, so `GET /api/v1/state` is
    /// never queued behind a network-bound tick. A `watch::Receiver` is cheap to clone and to read.
    pub(crate) snapshot_pub:
        tokio::sync::watch::Receiver<Option<std::sync::Arc<crate::snapshot::Snapshot>>>,
    pub(crate) ctx: crate::control_loop::CancelWait,
    /// The top-level effective tracker snapshotted at [`Orchestrator::control`] time (Go reads
    /// `o.eff.tracker` off-loop; the Rust snapshot avoids aliasing the loop-owned `o.eff`). `None`
    /// when built before the first config load — [`ControlHandle::move_to`] then falls back to the
    /// live [`Self::reads`] tracker (the SAME top-level tracker the reload path publishes), so the
    /// daemon, which builds the handle before `Run`'s first reload, still moves tickets.
    pub(crate) tracker: Option<std::sync::Arc<dyn rhapsody_tracker::Tracker>>,
    pub(crate) store: std::sync::Arc<dyn rhapsody_store::Store + Send + Sync>,
    /// The SAME [`Arc`]-shared reads cell as [`Orchestrator::reads`], so the daemon's live Linear
    /// read surfaces + the stop/resume move-tracker fallback reflect every hot-reload (F1).
    pub(crate) reads: std::sync::Arc<std::sync::RwLock<crate::reads::ReadsTarget>>,
    /// The workflow path the config endpoint reads / rewrites / validates (Go `WorkflowPath`).
    pub(crate) workflow_path: String,
    /// The SAME `Arc`-shared retention atomics as [`Orchestrator`], so the daemon's off-loop prune
    /// scheduler reads a hot-reloaded `retention_days` (and the retention-loaded gate) each cycle
    /// without racing the control task's reload (Go `CurrentRetentionDays` / `RetentionLoaded`).
    pub(crate) retention_days: std::sync::Arc<std::sync::atomic::AtomicI64>,
    pub(crate) retention_loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The SAME `Arc`-shared Teams memory runtime as
    /// [`Orchestrator::teams_memory`](crate::orchestrator::Orchestrator::teams_memory), so the
    /// daemon's `/api/v1/teams/*` handlers read the live run bindings and drive the backend
    /// **without a control round-trip at all** (STUDIO-645, T4). `None` when the daemon has no
    /// Teams runtime.
    ///
    /// Deliberately not routed through [`Self::events`]: §5.1 requires a retain never to block the
    /// control task, and an event-channel round-trip would queue it behind the current tick — the
    /// head-of-line class the T3a/T3b split already exists to avoid.
    pub(crate) teams_memory: Option<std::sync::Arc<crate::teamsmemory::TeamsMemory>>,
    /// The off-loop review-quorum task's inbox, cloned from
    /// [`Orchestrator::quorum_tx`](crate::orchestrator::Orchestrator) (STUDIO-659, T7). `None`
    /// whenever the quorum is off — so on a default installation a handoff cannot even represent a
    /// fan-out, let alone perform one.
    ///
    /// It lives on the handle rather than being sent through [`Self::events`] because the fan-out
    /// must be gated on the review-state move SUCCEEDING, and that move runs here, off-loop, after
    /// the control round-trip has already returned. Sending it back through the loop would put a
    /// tracker-write decision behind the current tick for no benefit.
    pub(crate) quorum: Option<tokio::sync::mpsc::UnboundedSender<crate::quorum::QuorumRequest>>,
}

impl crate::orchestrator::Orchestrator {
    /// The off-loop [`ControlHandle`] for driving the daemon's HTTP API while the control task runs.
    /// Snapshots the control-event sender + lifetime ctx + top-level tracker + store, and clones the
    /// SHARED reads cell + retention atomics (so a reload updates what the handle sees). Obtain it
    /// before the loop task takes ownership of the orchestrator: the daemon builds it right after
    /// [`set_ctx`](Orchestrator::set_ctx) + [`set_store`](Orchestrator::set_store) and BEFORE `Run`,
    /// so the `eff`-derived snapshot (`tracker`) is `None` and the reads-cell fallbacks fill in on the
    /// first reload; tests build it after setting `eff`.
    pub fn control(&self) -> ControlHandle {
        ControlHandle {
            events: self.events.clone(),
            snapshot_pub: self.snapshot_pub.subscribe(),
            ctx: self.ctx.clone().unwrap_or_default(),
            tracker: self.eff.as_ref().map(|e| std::sync::Arc::clone(&e.tracker)),
            store: std::sync::Arc::clone(&self.store),
            reads: std::sync::Arc::clone(&self.reads),
            workflow_path: self.workflow_path.clone(),
            retention_days: std::sync::Arc::clone(&self.retention_days),
            retention_loaded: std::sync::Arc::clone(&self.retention_loaded),
            teams_memory: self.teams_memory.as_ref().map(std::sync::Arc::clone),
            quorum: self.quorum_tx.clone(),
        }
    }
}

impl ControlHandle {
    /// The shared Teams memory runtime backing the `/api/v1/teams/*` handlers (STUDIO-645, T4).
    /// `None` when the daemon has no Teams runtime, which the handlers render as `teams_disabled`.
    pub fn teams_memory(&self) -> Option<&std::sync::Arc<crate::teamsmemory::TeamsMemory>> {
        self.teams_memory.as_ref()
    }
}

/// A Stop / Resume failure surfaced to the HTTP layer: a cancellation (the request or the orchestrator
/// lifetime ended before the operation committed → Go returns `ctx.Err()`) or a store read failure (Go
/// returns the `GetRun` / `IssueHistory` error).
#[derive(Debug, thiserror::Error)]
pub enum StopError {
    /// The request or lifetime ctx was cancelled before the operation could commit.
    #[error("canceled")]
    Canceled,
    /// A store read failed (`GetRun` in `ResumeRun`, or the supersession `IssueHistory` scan).
    #[error(transparent)]
    Store(#[from] rhapsody_store::StoreError),
}

use rhapsody_store as store;

use crate::control_loop::{CancelWait, Event};
use crate::orchestrator::Orchestrator;

impl Orchestrator {
    // --- on-loop handlers (Go stop.go's control-goroutine bookkeeping) ---

    /// Runs ON the control task for `evStopRun`: kill + record canceled + suppress. Returns the
    /// issue/team so the off-loop caller can do the slow Linear move. Mirrors Go `handleStopRun`.
    pub(crate) fn handle_stop_run(&mut self, run_id: i64) -> StopPlan {
        // `issue_id_for_run` is O6's (`message.rs`), returning "" when no live run matches.
        let id = self.issue_id_for_run(run_id);
        if id.is_empty() {
            return StopPlan::default(); // found = false
        }
        // `terminate` removes the entry, fires the worker cancellation (Go `re.cancel()` → SIGKILL),
        // and returns the entry we persist the `stopped` outcome from.
        let Some(re) = self.terminate(&id) else {
            return StopPlan::default();
        };
        let plan = StopPlan {
            found: true,
            issue_id: id.clone(),
            team_id: re.issue.team_id.clone(),
            identifier: re.issue.identifier.clone(),
        };
        self.persist_end_run(&re, store::OUTCOME_STOPPED, "stopped by user");
        self.persist_totals();
        self.persist_complete(&re.issue.identifier); // drop persistent retry/claim rows (boot-recovery)
        self.completed.remove(&id);
        self.claimed.insert(id); // in-memory suppression for the off-loop move window
        plan
    }

    /// Runs ON the control task after the off-loop Backlog move. Mirrors Go `handleStopFinalize`.
    pub(crate) fn handle_stop_finalize(&mut self, issue_id: &str, moved: bool) {
        if moved {
            self.claimed.remove(issue_id); // Backlog state now suppresses it; clearing lets a future resume work
        }
        // moved == false: keep o.claimed[issueID] so it stays dead this session (spec §7).
    }

    /// The resume admission check, ON the control task. Aborts when a NEWER run for the same issue is
    /// currently executing (`live`) OR has already FINISHED non-stopped (`superseded`). The supersession
    /// scan reads `issue_history` HERE, on the loop, so it serializes against `persist_end_run` (also
    /// on the loop) — closing the off-loop race where a run finishing between an off-loop read and
    /// admission would slip through. Never clears `o.claimed`. Mirrors Go `handleResume`.
    pub(crate) fn handle_resume(
        &self,
        issue_id: &str,
        identifier: &str,
        project: &str,
        run_id: i64,
    ) -> ResumePlan {
        if self.running.contains_key(issue_id) {
            return ResumePlan {
                live: true,
                ..Default::default()
            };
        }
        let hist = match self.store.issue_history(identifier, project, 0) {
            Ok(h) => h,
            Err(e) => {
                return ResumePlan {
                    err: Some(e),
                    ..Default::default()
                };
            }
        };
        for h in &hist {
            if h.id > run_id
                && h.outcome != store::OUTCOME_STOPPED
                && h.outcome != store::OUTCOME_RUNNING
            {
                return ResumePlan {
                    superseded: true,
                    ..Default::default()
                };
            }
        }
        ResumePlan::default()
    }

    /// Runs ON the control task after the off-loop move-to-Todo. Mirrors Go `handleResumeFinalize`.
    pub(crate) fn handle_resume_finalize(&mut self, issue_id: &str, moved: bool) {
        if moved {
            self.claimed.remove(issue_id);
        }
    }
}

impl ControlHandle {
    /// Kills the agent for `run_id`, records the run canceled, and moves its ticket to the team's
    /// Backlog so the daemon won't re-dispatch it. The kill + bookkeeping happen on the control task
    /// (via `evStopRun`); the slow Linear move runs here, off-loop. The admission SEND is the commit
    /// point — a request-ctx cancel before it is honest (`Canceled`); once accepted, the reply wait +
    /// finalize use the LIFETIME ctx, so a late request cancel cannot turn a committed kill into a
    /// failure. Mirrors Go `StopRun`. `req_ctx` is the HTTP request cancellation.
    pub async fn stop_run(
        &self,
        req_ctx: CancelWait,
        run_id: i64,
    ) -> Result<StopResult, StopError> {
        if req_ctx.is_cancelled() {
            return Err(StopError::Canceled);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::StopRun { run_id, reply: tx })
            .is_err()
        {
            return Err(StopError::Canceled);
        }
        let mut lifetime = self.ctx.clone();
        let plan = tokio::select! {
            p = rx => p.map_err(|_| StopError::Canceled)?,
            _ = lifetime.cancelled() => return Err(StopError::Canceled),
        };
        if !plan.found {
            return Ok(StopResult {
                not_running: true,
                ..Default::default()
            });
        }
        let mut res = StopResult {
            identifier: plan.identifier,
            ..Default::default()
        };
        self.move_to(
            &plan.issue_id,
            &plan.team_id,
            "backlog",
            &mut res.moved_to,
            &mut res.move_err,
        )
        .await;
        if !res.move_err.is_empty() {
            tracing::error!(issue_identifier = %res.identifier, err = %res.move_err, "stop: agent killed but Backlog move failed");
        }
        self.finalize(true, &plan.issue_id, res.move_err.is_empty())
            .await;
        Ok(res)
    }

    /// Moves a stopped run's ticket back to the team's Todo (unstarted) state and clears any lingering
    /// suppression, so the next poll re-dispatches it. Only a STOPPED run is resumable, and only when no
    /// NEWER run for the same issue is currently executing or already finished non-stopped. The
    /// admission check + suppression clear happen on the control task; the slow Linear move runs here,
    /// off-loop, and suppression clears only after a successful move. Mirrors Go `ResumeRun`.
    pub async fn resume_run(
        &self,
        req_ctx: CancelWait,
        run_id: i64,
    ) -> Result<ResumeResult, StopError> {
        let run = match self.store.get_run(run_id)? {
            Some(r) => r,
            None => {
                return Ok(ResumeResult {
                    not_found: true,
                    ..Default::default()
                });
            }
        };
        if run.outcome != store::OUTCOME_STOPPED {
            return Ok(ResumeResult {
                not_stopped: true,
                identifier: run.issue_identifier,
                ..Default::default()
            });
        }
        if run.team_id.is_empty() {
            return Ok(ResumeResult {
                no_team: true,
                identifier: run.issue_identifier,
                ..Default::default()
            });
        }
        if req_ctx.is_cancelled() {
            return Err(StopError::Canceled);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let accepted = self.events.send(Event::Resume {
            issue_id: run.issue_id.clone(),
            identifier: run.issue_identifier.clone(),
            project: run.project_slug.clone(),
            run_id: run.id,
            reply: tx,
        });
        if accepted.is_err() {
            return Err(StopError::Canceled);
        }
        let mut lifetime = self.ctx.clone();
        let plan = tokio::select! {
            p = rx => p.map_err(|_| StopError::Canceled)?,
            _ = lifetime.cancelled() => return Err(StopError::Canceled),
        };
        if let Some(e) = plan.err {
            return Err(StopError::Store(e));
        }
        if plan.live {
            return Ok(ResumeResult {
                live_run: true,
                identifier: run.issue_identifier,
                ..Default::default()
            });
        }
        if plan.superseded {
            return Ok(ResumeResult {
                superseded: true,
                identifier: run.issue_identifier,
                ..Default::default()
            });
        }
        let mut res = ResumeResult {
            identifier: run.issue_identifier.clone(),
            ..Default::default()
        };
        self.move_to(
            &run.issue_id,
            &run.team_id,
            "unstarted",
            &mut res.moved_to,
            &mut res.move_err,
        )
        .await;
        if !res.move_err.is_empty() {
            tracing::error!(issue_identifier = %run.issue_identifier, err = %res.move_err, "resume: move to Todo failed; keeping suppression");
        }
        self.finalize(false, &run.issue_id, res.move_err.is_empty())
            .await;
        Ok(res)
    }

    /// The off-loop `MoveIssueToType` shared by stop (`backlog`) + resume (`unstarted`): records the
    /// moved-to state name or the error text into the caller's result fields.
    async fn move_to(
        &self,
        issue_id: &str,
        team_id: &str,
        state_type: &str,
        moved_to: &mut String,
        move_err: &mut String,
    ) {
        // Prefer the `control()`-time snapshot (set when tests build the handle after `eff`); fall
        // back to the live shared reads tracker (the SAME top-level tracker the reload path publishes)
        // so the daemon — which builds the handle before the first reload, leaving the snapshot `None`
        // — still moves tickets. Clone out before any await; the guard is never held across it.
        let tracker = self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tracker
                .clone()
        });
        match tracker {
            Some(tr) => match tr.move_issue_to_type(issue_id, team_id, state_type).await {
                Ok(name) => *moved_to = name,
                Err(e) => *move_err = e.to_string(),
            },
            // No tracker at all (before the first config load): treat as a move failure so suppression
            // is retained (never re-dispatch stopped work).
            None => *move_err = "no effective tracker".to_string(),
        }
    }

    /// The durable history + recovery store (Go `StateProvider.Store()`, `Arc`-shared), backing the
    /// daemon's read-only history endpoints. Never absent ([`rhapsody_store::Noop`] when disabled).
    pub fn store(&self) -> std::sync::Arc<dyn rhapsody_store::Store + Send + Sync> {
        std::sync::Arc::clone(&self.store)
    }

    /// The absolute path of the WORKFLOW.md this daemon loads + watches (Go `WorkflowPath`). The
    /// config endpoint reads + rewrites it; validation runs against it.
    pub fn workflow_path(&self) -> &str {
        &self.workflow_path
    }

    /// Round-trips `evStopFinalize` (`stop`) / `evResumeFinalize` (resume) to clear (moved) or keep
    /// (not moved) the in-memory suppression after the off-loop move resolves. Runs on the LIFETIME ctx
    /// (not the request ctx): the move outcome is settled, so bookkeeping must always follow it. Mirrors
    /// Go `finalizeStop` / `finalizeResume`.
    async fn finalize(&self, stop: bool, issue_id: &str, moved: bool) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let issue_id = issue_id.to_string();
        let ev = if stop {
            Event::StopFinalize {
                issue_id,
                moved,
                reply: tx,
            }
        } else {
            Event::ResumeFinalize {
                issue_id,
                moved,
                reply: tx,
            }
        };
        if self.events.send(ev).is_err() {
            return;
        }
        let mut ctx = self.ctx.clone();
        tokio::select! {
            _ = rx => {}
            _ = ctx.cancelled() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_loop::{CancelSignal, CancelWait, Event};
    use crate::orchestrator::Orchestrator;
    use crate::retry::EvWorkerExit;
    use crate::testsupport::{empty_effective, issue, set_of};
    use chrono::{SecondsFormat, Utc};
    use rhapsody_core::Issue;
    use rhapsody_store::{
        OUTCOME_COMPLETED, OUTCOME_FAILED, OUTCOME_RUNNING, OUTCOME_STOPPED, RunEnd, RunStart,
        Sqlite, Store, StorePath,
    };
    use rhapsody_tracker::fake::Fake;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The off-loop bookkeeping a stop test holds alongside the loop-owned orchestrator: the per-issue
    /// worker cancel observers (Go's `cancelled` channel map), the lifetime cancellation, and the store.
    struct Env {
        cancelled: Arc<Mutex<HashMap<String, CancelWait>>>,
        signal: CancelSignal,
        store: Arc<dyn Store + Send + Sync>,
    }

    /// Builds an orchestrator wired to an in-memory store + `tr`, with a fake spawn that records each
    /// worker's cancel observer, and the lifetime ctx set. The loop is NOT started (the caller seeds
    /// state race-free first). Mirrors Go `newStopHarness`.
    fn stop_orch(tr: Arc<Fake>) -> (Orchestrator, Env) {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
        let mut eff = empty_effective(tr);
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.max_retry_backoff_ms = 300_000;
        eff.poll_interval = Duration::from_secs(3600); // effectively disable the auto-tick
        eff.stall_timeout = Duration::from_secs(3600);
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.set_store(Arc::clone(&store));
        o.eff = Some(eff);
        let cancelled = Arc::new(Mutex::new(HashMap::<String, CancelWait>::new()));
        let cancelled2 = Arc::clone(&cancelled);
        o.spawn = Some(Box::new(move |iss, _attempt, re| {
            cancelled2
                .lock()
                .expect("cancelled lock")
                .insert(iss.id.clone(), re.cancel.wait());
        }));
        let signal = CancelSignal::new();
        o.ctx = Some(signal.wait());
        (
            o,
            Env {
                cancelled,
                signal,
                store,
            },
        )
    }

    /// Snapshots the control handle and launches the loop, returning its task (which yields the
    /// orchestrator back on exit so the test can read the maps race-free) + the handle.
    fn start(
        o: Orchestrator,
        signal: &CancelSignal,
    ) -> (tokio::task::JoinHandle<Orchestrator>, ControlHandle) {
        let handle = o.control();
        let loop_ctx = signal.wait();
        let task = tokio::spawn(async move {
            let mut o = o;
            o.run_loaded(loop_ctx).await;
            o
        });
        (task, handle)
    }

    fn issue_team(id: &str, ident: &str, state: &str, team: &str) -> Issue {
        let mut i = issue(id, ident, state);
        i.team_id = team.to_string();
        i
    }

    fn now_rfc3339() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Seeds a run row, returning its id.
    fn seed_run(
        store: &dyn Store,
        id: &str,
        ident: &str,
        team: &str,
        outcome: Option<&str>,
    ) -> i64 {
        let run_id = store
            .start_run(RunStart {
                issue_id: id.to_string(),
                issue_identifier: ident.to_string(),
                title: "t".to_string(),
                team_id: team.to_string(),
                ..Default::default()
            })
            .expect("start_run");
        if let Some(o) = outcome {
            store
                .end_run(
                    run_id,
                    RunEnd {
                        outcome: o.to_string(),
                        ended_at: now_rfc3339(),
                        ..Default::default()
                    },
                )
                .expect("end_run");
        }
        run_id
    }

    /// Blocks until every event already enqueued on the loop has been processed (Go `flushLoop`): a
    /// no-op `ResumeFinalize` whose reply guarantees prior events (e.g. a `WorkerExit` that wrote a
    /// terminal outcome) have committed and are observable to a subsequent store read.
    async fn flush_loop(handle: &ControlHandle) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle
            .events
            .send(Event::ResumeFinalize {
                issue_id: "__flush_noop__".to_string(),
                moved: false,
                reply: tx,
            })
            .expect("flush send");
        rx.await.expect("flush reply");
    }

    async fn assert_cancelled(env: &Env, id: &str) {
        let mut cw = env
            .cancelled
            .lock()
            .expect("cancelled lock")
            .get(id)
            .expect("cancel observer")
            .clone();
        tokio::time::timeout(Duration::from_secs(2), cw.cancelled())
            .await
            .expect("worker ctx was not cancelled");
    }

    // F1 self-review regression: when the daemon installs the lifetime ctx (`set_ctx`) BEFORE
    // snapshotting the handle (`control`), the handle carries the REAL ctx, so a cancelled lifetime
    // breaks an off-loop reply-wait. With the pre-fix never-cancelling default ctx this would hang
    // (the events receiver is still held by `o`, so the reply never arrives and only the ctx can
    // break the wait) — the `timeout` guards that regression.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_handle_lifetime_ctx_bounds_offloop_wait() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let signal = CancelSignal::new();
        o.set_ctx(signal.wait()); // the daemon's pre-Run ctx install
        let handle = o.control();
        signal.cancel(); // lifetime ends before the reply can arrive
        let res = tokio::time::timeout(Duration::from_secs(2), handle.send_run_message(1, "hi"))
            .await
            .expect("a cancelled lifetime ctx must break the reply-wait, not hang");
        assert!(
            res.not_running,
            "a cancelled lifetime ctx yields not_running"
        );
        drop(o); // keep `o` (and its event receiver) alive until after the call, then release
    }

    // Mirrors Go `TestStopRun_KillsRecordsCancelsAndSuppresses`.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_kills_records_cancels_and_suppresses() {
        let mut f = Fake::new();
        f.move_to_type_name = "Backlog".to_string();
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        o.dispatch_issue(
            issue_team("ID-1", "MT-1", "In Progress", "TEAM-1"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-1"].run_id;
        assert_ne!(run_id, 0, "expected a non-zero run_id");
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .stop_run(CancelWait::default(), run_id)
            .await
            .expect("stop_run");
        assert!(!res.not_running);
        assert_eq!(res.identifier, "MT-1");
        assert_eq!(res.moved_to, "Backlog");

        assert_cancelled(&env, "ID-1").await;
        let calls = tr.move_to_type_calls();
        assert_eq!(calls.len(), 1, "move_to_type_calls = {calls:?}");
        assert_eq!(
            (
                calls[0].issue_id.as_str(),
                calls[0].team_id.as_str(),
                calls[0].state_type.as_str()
            ),
            ("ID-1", "TEAM-1", "backlog")
        );

        let run = env
            .store
            .get_run(run_id)
            .expect("get_run")
            .expect("run found");
        assert_eq!(run.outcome, OUTCOME_STOPPED);

        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            !o.running.contains_key("ID-1"),
            "running still has the stopped issue"
        );
        assert!(
            !o.claimed.contains("ID-1"),
            "claimed[ID-1] should be cleared after a successful Backlog move"
        );
    }

    // Mirrors Go `TestStopRun_MoveFailKeepsClaimed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_move_fail_keeps_claimed() {
        let mut f = Fake::new();
        f.move_to_type_err = Some(rhapsody_tracker::TrackerError::Other(
            "no backlog state for team".to_string(),
        ));
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        o.dispatch_issue(
            issue_team("ID-1", "MT-1", "In Progress", "TEAM-1"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-1"].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .stop_run(CancelWait::default(), run_id)
            .await
            .expect("stop_run");
        assert!(!res.not_running);
        assert!(
            !res.move_err.is_empty(),
            "expected a non-empty move_err when the Backlog move fails"
        );

        assert_cancelled(&env, "ID-1").await;
        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.claimed.contains("ID-1"),
            "claimed[ID-1] should STILL be true after a failed Backlog move"
        );
    }

    // Mirrors Go `TestStopRun_NotRunning`.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_not_running() {
        let tr = Arc::new(Fake::new());
        let (o, env) = stop_orch(Arc::clone(&tr));
        let (task, handle) = start(o, &env.signal);
        let res = handle
            .stop_run(CancelWait::default(), 4242)
            .await
            .expect("stop_run");
        assert!(
            res.not_running,
            "expected not_running for an unknown run id"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "tracker should not be called when not running"
        );
        env.signal.cancel();
        let _ = task.await;
    }

    // Mirrors Go `TestResumeRun_MovesToUnstartedAndClearsClaim`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_moves_to_unstarted_and_clears_claim() {
        let mut f = Fake::new();
        f.move_to_type_name = "Todo".to_string();
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        let run_id = seed_run(
            env.store.as_ref(),
            "ID-9",
            "MT-9",
            "TEAM-9",
            Some(OUTCOME_STOPPED),
        );
        o.claimed.insert("ID-9".to_string());
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), run_id)
            .await
            .expect("resume_run");
        assert!(
            !res.not_found && !res.no_team,
            "unexpected result flags: {res:?}"
        );
        assert_eq!(res.identifier, "MT-9");
        assert_eq!(res.moved_to, "Todo");
        let calls = tr.move_to_type_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (
                calls[0].issue_id.as_str(),
                calls[0].team_id.as_str(),
                calls[0].state_type.as_str()
            ),
            ("ID-9", "TEAM-9", "unstarted")
        );

        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            !o.claimed.contains("ID-9"),
            "claimed[ID-9] should be cleared after resume"
        );
    }

    // Mirrors Go `TestResumeRun_MoveFailKeepsClaimed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_move_fail_keeps_claimed() {
        let mut f = Fake::new();
        f.move_to_type_err = Some(rhapsody_tracker::TrackerError::Other(
            "no unstarted state for team".to_string(),
        ));
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        let run_id = seed_run(
            env.store.as_ref(),
            "ID-9",
            "MT-9",
            "TEAM-9",
            Some(OUTCOME_STOPPED),
        );
        o.claimed.insert("ID-9".to_string());
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), run_id)
            .await
            .expect("resume_run");
        assert!(
            !res.move_err.is_empty(),
            "expected a non-empty move_err when the move fails"
        );
        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.claimed.contains("ID-9"),
            "claimed[ID-9] should STILL be true after a failed move-to-Todo"
        );
    }

    // Mirrors Go `TestResumeRun_NotStoppedRejected`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_not_stopped_rejected() {
        for outcome in [OUTCOME_COMPLETED, OUTCOME_FAILED, OUTCOME_RUNNING] {
            let mut f = Fake::new();
            f.move_to_type_name = "Todo".to_string();
            let tr = Arc::new(f);
            let (mut o, env) = stop_orch(Arc::clone(&tr));
            // OUTCOME_RUNNING is the StartRun default; don't end it.
            let end = if outcome == OUTCOME_RUNNING {
                None
            } else {
                Some(outcome)
            };
            let run_id = seed_run(env.store.as_ref(), "ID-8", "MT-8", "TEAM-8", end);
            o.claimed.insert("ID-8".to_string());
            let (task, handle) = start(o, &env.signal);

            let res = handle
                .resume_run(CancelWait::default(), run_id)
                .await
                .expect("resume_run");
            assert!(
                res.not_stopped,
                "expected not_stopped for outcome {outcome}, got {res:?}"
            );
            assert!(
                tr.move_to_type_calls().is_empty(),
                "tracker must not be called for a non-stopped run"
            );
            env.signal.cancel();
            let o = task.await.expect("loop task");
            assert!(
                o.claimed.contains("ID-8"),
                "claimed[ID-8] must be untouched when the run isn't stopped"
            );
        }
    }

    // Mirrors Go `TestResumeRun_LiveRunRejected`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_live_run_rejected() {
        let mut f = Fake::new();
        f.move_to_type_name = "Todo".to_string();
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        let old_run_id = seed_run(
            env.store.as_ref(),
            "ID-7",
            "MT-7",
            "TEAM-7",
            Some(OUTCOME_STOPPED),
        );
        // A NEWER run for the SAME issue is currently executing.
        o.dispatch_issue(
            issue_team("ID-7", "MT-7", "In Progress", "TEAM-7"),
            None,
            None,
            String::new(),
        );
        assert!(o.running.contains_key("ID-7"));
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), old_run_id)
            .await
            .expect("resume_run");
        assert!(
            res.live_run,
            "expected live_run when a newer run is executing, got {res:?}"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "tracker must not be called while a newer run is live"
        );
        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.claimed.contains("ID-7"),
            "claimed[ID-7] must remain set for the live run"
        );
    }

    // Mirrors Go `TestStopRun_RequestCancelAfterMoveStillFinalizes` (finding A).
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_request_cancel_after_move_still_finalizes() {
        let req = CancelSignal::new();
        let req2 = req.clone();
        let mut f = Fake::new();
        f.move_to_type_name = "Backlog".to_string();
        f.move_to_type_hook = Some(Box::new(move || req2.cancel())); // client disconnect after the move lands
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        o.dispatch_issue(
            issue_team("ID-1", "MT-1", "In Progress", "TEAM-1"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-1"].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .stop_run(req.wait(), run_id)
            .await
            .expect("StopRun must not error on a request canceled after the move");
        assert_eq!(res.moved_to, "Backlog");
        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            !o.claimed.contains("ID-1"),
            "claimed[ID-1] must be cleared even when the request ctx was canceled after the move (finalize uses the lifetime ctx)"
        );
    }

    // Mirrors Go `TestStopRun_RequestCancelAfterAdmissionNotError` (finding B).
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_request_cancel_after_admission_not_error() {
        let req = CancelSignal::new();
        let req2 = req.clone();
        let mut f = Fake::new();
        f.move_to_type_name = "Backlog".to_string();
        f.move_to_type_hook = Some(Box::new(move || req2.cancel()));
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        o.dispatch_issue(
            issue_team("ID-2", "MT-2", "In Progress", "TEAM-2"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-2"].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .stop_run(req.wait(), run_id)
            .await
            .expect("a committed stop must not return an error on late request cancel");
        assert!(!res.not_running);
        assert_eq!(res.identifier, "MT-2");
        env.signal.cancel();
        let _ = task.await;
    }

    // Mirrors Go `TestResumeRun_SupersededByNewerFinishedRun` (finding C).
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_superseded_by_newer_finished_run() {
        let mut f = Fake::new();
        f.move_to_type_name = "Todo".to_string();
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        let old_id = seed_run(
            env.store.as_ref(),
            "ID-5",
            "MT-5",
            "TEAM-5",
            Some(OUTCOME_STOPPED),
        );
        seed_run(
            env.store.as_ref(),
            "ID-5",
            "MT-5",
            "TEAM-5",
            Some(OUTCOME_COMPLETED),
        ); // newer succeeded
        o.claimed.insert("ID-5".to_string());
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), old_id)
            .await
            .expect("resume_run");
        assert!(
            res.superseded,
            "expected superseded when a newer run already succeeded, got {res:?}"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "tracker must not be called when superseded"
        );
        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.claimed.contains("ID-5"),
            "claimed[ID-5] must be untouched when the resume is superseded"
        );
    }

    // Mirrors Go `TestResumeRun_SupersededByRunFinishingOnLoop`: the supersession scan runs INSIDE
    // handle_resume on the loop, serializing against persist_end_run (also on the loop).
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_superseded_by_run_finishing_on_loop() {
        let mut f = Fake::new();
        f.move_to_type_name = "Todo".to_string();
        let tr = Arc::new(f);
        let (mut o, env) = stop_orch(Arc::clone(&tr));
        let old_id = seed_run(
            env.store.as_ref(),
            "ID-9",
            "MT-9",
            "TEAM-9",
            Some(OUTCOME_STOPPED),
        );
        // A NEWER run for the SAME issue is dispatched and currently LIVE.
        o.dispatch_issue(
            issue_team("ID-9", "MT-9", "In Progress", "TEAM-9"),
            None,
            None,
            String::new(),
        );
        let started_at = o.running["ID-9"].started_at;
        let (task, handle) = start(o, &env.signal);

        // While the newer run is live, resuming the old attempt reports LiveRun.
        let res = handle
            .resume_run(CancelWait::default(), old_id)
            .await
            .expect("resume_run (live)");
        assert!(
            res.live_run,
            "expected live_run while the newer run is executing, got {res:?}"
        );

        // The newer run finishes SUCCEEDED on the loop; flush so the terminal outcome commits.
        handle
            .events
            .send(Event::WorkerExit(EvWorkerExit {
                issue_id: "ID-9".to_string(),
                failed: false,
                started_at,
                err_msg: String::new(),
                last_state: "Done".to_string(),
                declared_handoff: true,
            }))
            .expect("worker-exit send");
        flush_loop(&handle).await;

        // Now the resume must see the loop-committed succeeded outcome and reject with superseded.
        let res = handle
            .resume_run(CancelWait::default(), old_id)
            .await
            .expect("resume_run (finished)");
        assert!(
            res.superseded,
            "expected superseded once the newer run finished on the loop, got {res:?}"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "tracker must not be called when superseded"
        );
        env.signal.cancel();
        let _ = task.await;
    }

    // Mirrors Go `TestResumeRun_NewerStoppedRunStillResumable`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_newer_stopped_run_still_resumable() {
        let mut f = Fake::new();
        f.move_to_type_name = "Todo".to_string();
        let tr = Arc::new(f);
        let (o, env) = stop_orch(Arc::clone(&tr));
        let old_id = seed_run(
            env.store.as_ref(),
            "ID-6",
            "MT-6",
            "TEAM-6",
            Some(OUTCOME_STOPPED),
        );
        seed_run(
            env.store.as_ref(),
            "ID-6",
            "MT-6",
            "TEAM-6",
            Some(OUTCOME_STOPPED),
        ); // newer, also stopped
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), old_id)
            .await
            .expect("resume_run");
        assert!(
            !res.superseded && !res.live_run && !res.not_stopped,
            "a newer STOPPED run must not block resume: {res:?}"
        );
        assert_eq!(res.moved_to, "Todo");
        env.signal.cancel();
        let _ = task.await;
    }

    // Mirrors Go `TestResumeRun_MissingTeamID`.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_run_missing_team_id() {
        let tr = Arc::new(Fake::new());
        let (o, env) = stop_orch(Arc::clone(&tr));
        let run_id = seed_run(
            env.store.as_ref(),
            "ID-0",
            "MT-0",
            "",
            Some(OUTCOME_STOPPED),
        ); // no team
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .resume_run(CancelWait::default(), run_id)
            .await
            .expect("resume_run");
        assert!(
            res.no_team,
            "expected no_team for a pre-v4 row, got {res:?}"
        );
        assert_eq!(res.identifier, "MT-0");
        assert!(
            tr.move_to_type_calls().is_empty(),
            "tracker should not be called with no team id"
        );
        env.signal.cancel();
        let _ = task.await;
    }
}
