//! handoff — the daemon-mediated review handoff (TRA-242). A NEW capability beyond Go Symphony
//! v0.4.0 (documented as a divergence): the interim review-gated handoff had the worker prompt move
//! the ticket to "In Review" via the agent's own Linear-write MCP (PR #58); this makes it bulletproof
//! by moving the daemon's own tracker, so the dispatched agent needs no Linear-write access and gets
//! ONE confident terminal action.
//!
//! The `symphony_handoff` MCP write tool proxies `POST /api/v1/runs/{id}/handoff`; this module is the
//! daemon side. [`ControlHandle::handoff_run`] moves the run's ticket to the configured review handoff
//! state so the ticket leaves the active set. Unlike [`stop_run`](ControlHandle::stop_run) it does NOT
//! kill the live agent (the agent itself is calling the tool and finishes its turn) and does NOT touch
//! the in-memory suppression: the move ALONE is the clean end-of-run — the worker's next per-turn
//! state refresh sees the non-active state and winds the turn loop down (worker.rs `run_turns`), and
//! the control loop records the run's terminal outcome exactly as it does for the interim Linear-MCP
//! handoff. That is why "the daemon treats a successful `symphony_handoff` as the clean end-of-run".
//!
//! # By name, not by type
//!
//! The P3 move port has two arms: `MoveIssueToType` (config-free, resolves a Linear state TYPE — used
//! by stop→"backlog" / resume→"unstarted") and `MoveIssueState` (by NAME, team-scoped). Review handoff
//! cannot use the by-TYPE arm: Linear's `WorkflowState` types are triage / backlog / unstarted /
//! started / completed / canceled — there is NO "review" type, and the nearest ("started") resolves to
//! an ACTIVE state (e.g. "In Progress"), which would keep the ticket active and spin the turn loop to
//! `max_turns`. So the daemon moves to the run's configured `review_states[0]` by NAME via
//! `MoveIssueState` — the "falling back to review_states[0]" path in the ticket, which for review
//! handoff is the only workspace-agnostic-yet-correct target (state names vary per workspace, so the
//! configured `review_states` is the source of truth). Empty `review_states` ⇒ the feature is off and
//! the tool reports `not_configured` so the agent uses the documented Linear-MCP fallback.

use std::sync::PoisonError;

use crate::control_loop::{CancelWait, Event};
use crate::orchestrator::Orchestrator;
use crate::stop::{ControlHandle, StopError};

/// The HTTP-layer result of a Handoff (`POST /api/v1/runs/{id}/handoff`, Go has no analog — TRA-242).
/// A daemon-mediated review handoff: move the run's ticket to the configured review state so it leaves
/// the active set and the run cleanly ends. Unlike stop/resume a failed move is NOT a partial success —
/// the move IS the handoff, so `move_err` / `not_configured` surfaces to the agent as a tool error and
/// it falls back to the documented Linear-MCP path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HandoffResult {
    /// No live run has this `run_id` (⇒ 409 `not_running`). The agent normally calls this from its own
    /// live run, so this is an edge case (a stale/foreign run id).
    pub not_running: bool,
    /// The run's project has no configured `review_states` (⇒ 409 `handoff_not_configured`): the review
    /// handoff feature is off, so the agent must use the Linear-MCP fallback.
    pub not_configured: bool,
    /// Human ticket id, e.g. `"INF-217"`.
    pub identifier: String,
    /// The review state name the ticket was moved to (`""` if the move failed / was not attempted).
    pub moved_to: String,
    /// Non-empty when the review-state move was attempted but the tracker rejected it.
    pub move_err: String,
}

/// The control-task reply for `evHandoffRun`: whether a live run was found + the issue/team the
/// off-loop move targets + the resolved review state name (empty ⇒ not configured / no config loaded).
#[derive(Debug, Clone, Default)]
pub struct HandoffPlan {
    pub found: bool,
    pub issue_id: String,
    pub team_id: String,
    pub identifier: String,
    pub review_state: String,
    /// The review-quorum fan-out to fire once the review-state move SUCCEEDS (STUDIO-659, T7;
    /// design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.12). `None` whenever the
    /// quorum does not fire — which is every handoff on an installation that has not opted in, and
    /// the common case even on one that has (see
    /// [`plan_quorum`](Orchestrator::plan_quorum) for the four gates).
    ///
    /// Decided HERE, on the control task, from state already in memory — the reviewers, the PR and
    /// the target state are all resolved before this struct exists — so the handoff itself never
    /// waits on the quorum and the quorum never reaches into loop-owned state.
    pub quorum: Option<crate::quorum::QuorumRequest>,
}

impl Orchestrator {
    /// Runs ON the control task for `evHandoffRun`: resolve the live run's issue/team + the configured
    /// review state to move to. Read-only — no kill, no suppression change (the agent is calling the
    /// tool and will finish its turn; the move alone winds the run down). Returns `found = false` when
    /// no live run matches the id. It is the plan half of [`handle_stop_run`](Orchestrator::handle_stop_run),
    /// minus the mutation.
    pub(crate) fn handle_handoff_run(&self, run_id: i64) -> HandoffPlan {
        let id = self.issue_id_for_run(run_id);
        if id.is_empty() {
            return HandoffPlan::default(); // found = false
        }
        let Some(re) = self.running.get(&id) else {
            return HandoffPlan::default();
        };
        HandoffPlan {
            found: true,
            issue_id: id.clone(),
            team_id: re.issue.team_id.clone(),
            identifier: re.issue.identifier.clone(),
            review_state: self.review_handoff_state(&re.project_slug),
            // §0.12's trigger: "a teammate's handoff with a linked PR". This is that moment, and it
            // is the moment the daemon EXECUTES rather than merely infers, which is why the design
            // chose it over "PR opened" (the PR exists mid-run, long before it is reviewable) or
            // "review posted" (that is the quorum's output, not its input).
            quorum: self.plan_quorum(re),
        }
    }

    /// The configured review handoff state NAME for a run's owning project: the first ordered
    /// `review_states`, resolved per-project ⊕ top-level via [`effective_for`](rhapsody_config::effective_for)
    /// — the same resolution the poll/select paths use. Empty when review handoff is not configured OR
    /// no effective config is loaded yet. Consulted only by [`handle_handoff_run`](Orchestrator::handle_handoff_run).
    fn review_handoff_state(&self, project_slug: &str) -> String {
        let Some(eff) = self.eff.as_ref() else {
            return String::new();
        };
        // Match the run's owning project (multi-project) by slug; the legacy single-project path (empty
        // slug) resolves the top-level review_states.
        let project = if project_slug.is_empty() {
            None
        } else {
            eff.cfg
                .projects
                .iter()
                .find(|p| p.slugs.iter().any(|s| s == project_slug))
        };
        rhapsody_config::effective_for(&eff.cfg, project)
            .review_states
            .into_iter()
            .next()
            .unwrap_or_default()
    }
}

impl ControlHandle {
    /// Moves the run's ticket to the configured review handoff state (its `review_states[0]`, by NAME)
    /// so it leaves the active set and the run cleanly ends. The agent calls this as its terminal
    /// action; the daemon does the Linear write, so the agent needs no Linear-write access. It does NOT
    /// kill the agent or change suppression — the move alone winds the turn loop down. The plan is built
    /// ON the control task (`evHandoffRun`); the slow Linear move runs here, off-loop. The shape mirrors
    /// [`stop_run`](ControlHandle::stop_run): the admission SEND is the commit point (a request cancel
    /// before it is honest `Canceled`), and the reply-wait is bounded by the lifetime ctx. NEW beyond Go
    /// v0.4.0 (TRA-242). `req_ctx` is the HTTP request cancellation.
    pub async fn handoff_run(
        &self,
        req_ctx: CancelWait,
        run_id: i64,
    ) -> Result<HandoffResult, StopError> {
        if req_ctx.is_cancelled() {
            return Err(StopError::Canceled);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::HandoffRun { run_id, reply: tx })
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
            return Ok(HandoffResult {
                not_running: true,
                ..Default::default()
            });
        }
        if plan.review_state.is_empty() {
            return Ok(HandoffResult {
                not_configured: true,
                identifier: plan.identifier,
                ..Default::default()
            });
        }
        let mut res = HandoffResult {
            identifier: plan.identifier,
            ..Default::default()
        };
        match self
            .move_issue_state(&plan.issue_id, &plan.team_id, &plan.review_state)
            .await
        {
            Ok(()) => res.moved_to = plan.review_state,
            Err(e) => res.move_err = e,
        }
        if !res.move_err.is_empty() {
            tracing::error!(issue_identifier = %res.identifier, err = %res.move_err, "handoff: review-state move failed");
        }
        // The review quorum fires only on a handoff that actually LANDED (STUDIO-659, §0.12): a
        // move the tracker refused is not a handoff, and fanning review tickets out for a ticket
        // still sitting in an active state would ask two teammates to review work whose author is
        // about to keep going. The send itself cannot fail meaningfully — the channel is unbounded,
        // so it never blocks the agent's tool call, and a closed one only means the daemon is
        // already shutting down.
        if res.move_err.is_empty() {
            self.request_quorum(plan.quorum);
        }
        Ok(res)
    }

    /// Hands a planned fan-out to the off-loop quorum task (STUDIO-659, T7). A no-op when the
    /// quorum did not fire, when no task is running, or when that task has already stopped —
    /// none of which is worth failing the handoff over: the ticket has moved, the run is winding
    /// down, and a missed fan-out costs a review, not the work.
    fn request_quorum(&self, req: Option<crate::quorum::QuorumRequest>) {
        let (Some(req), Some(tx)) = (req, self.quorum.as_ref()) else {
            return;
        };
        let identifier = req.parent_identifier.clone();
        if tx.send(req).is_err() {
            tracing::warn!(
                issue_identifier = %identifier,
                "handoff: the teams review-quorum task is gone; no review was requested"
            );
        }
    }

    /// The off-loop by-NAME `MoveIssueState` for handoff, resolving the tracker exactly like
    /// [`stop_run`](ControlHandle::stop_run)'s `move_to` (the `control()`-time snapshot, else the shared
    /// reads tracker so the daemon — which builds the handle before the first reload — still moves
    /// tickets). Returns the tracker error text on failure; no tracker at all (before the first config
    /// load) is a move failure so the agent falls back to the Linear-MCP path. Clones the handle out
    /// before any await; the reads guard is never held across it.
    async fn move_issue_state(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
    ) -> Result<(), String> {
        let tracker = self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .tracker
                .clone()
        });
        match tracker {
            Some(tr) => tr
                .move_issue_state(issue_id, team_id, state_name)
                .await
                .map_err(|e| e.to_string()),
            None => Err("no effective tracker".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_loop::{CancelSignal, CancelWait};
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{empty_effective, issue, set_of};
    use rhapsody_core::Issue;
    use rhapsody_store::{Sqlite, Store, StorePath};
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The off-loop bookkeeping a handoff test holds alongside the loop-owned orchestrator: the
    /// per-issue worker cancel observers (to assert the agent is NOT killed) + the lifetime cancel.
    struct Env {
        cancelled: Arc<Mutex<HashMap<String, CancelWait>>>,
        signal: CancelSignal,
    }

    /// Builds an orchestrator wired to an in-memory store + `tr`, with `review_states` configured, a
    /// fake spawn that records each worker's cancel observer, and the lifetime ctx set. The loop is NOT
    /// started (the caller seeds state race-free first). Mirrors the stop harness (`newStopHarness`),
    /// with the review-state config the handoff resolution needs.
    fn handoff_orch(tr: Arc<Fake>, review_states: &[&str]) -> (Orchestrator, Env) {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
        let mut eff = empty_effective(tr);
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.review_states = set_of(review_states);
        // The ordered source `review_handoff_state` resolves against (the normalized set above is the
        // scheduling view; the by-name move target comes from the ordered config vec).
        eff.cfg.tracker.review_states = review_states.iter().map(|s| s.to_string()).collect();
        eff.max_concurrent = 10;
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
        (o, Env { cancelled, signal })
    }

    /// Snapshots the control handle and launches the loop, returning its task + the handle.
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

    // The happy path: a live run's ticket is moved to the configured review state by NAME, the agent
    // is NOT killed, and the result carries the identifier + moved-to state.
    #[tokio::test(flavor = "multi_thread")]
    async fn handoff_moves_ticket_to_review_state_without_killing() {
        let tr = Arc::new(Fake::new());
        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        o.dispatch_issue(
            issue_team("ID-1", "MT-1", "In Progress", "TEAM-1"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-1"].run_id;
        assert_ne!(run_id, 0, "expected a non-zero run_id");
        let cancel_obs = env
            .cancelled
            .lock()
            .expect("cancelled lock")
            .get("ID-1")
            .expect("cancel observer")
            .clone();
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(
            !res.not_running && !res.not_configured,
            "unexpected: {res:?}"
        );
        assert_eq!(res.identifier, "MT-1");
        assert_eq!(res.moved_to, "In Review");
        assert!(res.move_err.is_empty(), "move_err = {}", res.move_err);

        // The move went through the by-NAME arm with the configured review state (NOT move_to_type).
        let calls = tr.move_calls();
        assert_eq!(calls.len(), 1, "move_calls = {calls:?}");
        assert_eq!(
            (
                calls[0].issue_id.as_str(),
                calls[0].team_id.as_str(),
                calls[0].state_name.as_str()
            ),
            ("ID-1", "TEAM-1", "In Review")
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "handoff must move by name, not by type"
        );

        // The agent is NOT killed — handoff ends the run via the ticket move, not a SIGKILL.
        assert!(
            !cancel_obs.is_cancelled(),
            "handoff must not cancel the worker"
        );

        env.signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.running.contains_key("ID-1"),
            "the run stays live until its own turn winds down (handoff does not evict it)"
        );
    }

    // An unknown run id ⇒ not_running, and the tracker is never called.
    #[tokio::test(flavor = "multi_thread")]
    async fn handoff_unknown_run_is_not_running() {
        let tr = Arc::new(Fake::new());
        let (o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        let (task, handle) = start(o, &env.signal);
        let res = handle
            .handoff_run(CancelWait::default(), 4242)
            .await
            .expect("handoff_run");
        assert!(
            res.not_running,
            "expected not_running for an unknown run id"
        );
        assert!(
            tr.move_calls().is_empty(),
            "tracker must not be called when not running"
        );
        env.signal.cancel();
        let _ = task.await;
    }

    // No configured review_states ⇒ not_configured (the agent falls back to Linear MCP), no move.
    #[tokio::test(flavor = "multi_thread")]
    async fn handoff_no_review_states_is_not_configured() {
        let tr = Arc::new(Fake::new());
        let (mut o, env) = handoff_orch(Arc::clone(&tr), &[]); // review handoff OFF
        o.dispatch_issue(
            issue_team("ID-2", "MT-2", "In Progress", "TEAM-2"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-2"].run_id;
        let (task, handle) = start(o, &env.signal);
        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(
            res.not_configured,
            "expected not_configured with no review_states, got {res:?}"
        );
        assert_eq!(res.identifier, "MT-2");
        assert!(
            tr.move_calls().is_empty(),
            "no review state ⇒ no move attempted"
        );
        env.signal.cancel();
        let _ = task.await;
    }

    // A tracker move rejection surfaces as move_err with NO moved_to (a handoff failure, not a partial
    // success) — so the agent's tool sees an error and falls back to the Linear-MCP path.
    #[tokio::test(flavor = "multi_thread")]
    async fn handoff_move_failure_surfaces_error() {
        let mut f = Fake::new();
        f.move_err = Some(TrackerError::Other("no review state for team".to_string()));
        let tr = Arc::new(f);
        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        o.dispatch_issue(
            issue_team("ID-3", "MT-3", "In Progress", "TEAM-3"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running["ID-3"].run_id;
        let (task, handle) = start(o, &env.signal);
        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(
            !res.move_err.is_empty(),
            "expected a non-empty move_err when the review move fails"
        );
        assert!(
            res.moved_to.is_empty(),
            "a failed move must not report moved_to"
        );
        env.signal.cancel();
        let _ = task.await;
    }
}
