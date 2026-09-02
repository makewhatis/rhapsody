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
    /// [`plan_quorum`](Orchestrator::plan_quorum) for the gates).
    ///
    /// Decided HERE, on the control task, from state already in memory — the reviewers, the PR and
    /// the target state are all resolved before this struct exists — so the handoff itself never
    /// waits on the quorum and the quorum never reaches into loop-owned state.
    pub quorum: Option<crate::quorum::QuorumRequest>,
    /// The ticketless review INTRODUCTION to fire once the review-state move SUCCEEDS (STUDIO-720,
    /// slice 6; design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §15-a). `None`
    /// on every installation that has not opted into `review.mode: ticketless`, which is the
    /// default.
    ///
    /// Mutually exclusive with [`quorum`](Self::quorum) by construction, not by convention: the two
    /// gates subtract each other (`quorum_enabled` excludes the ticketless mode, and
    /// `review_ticketless_enabled` requires it), so one handoff fires exactly one review path
    /// (design §14.2, "config cutover double-fire").
    ///
    /// Decided HERE, on the control task, from the run's OWN resolved repository binding — the
    /// trusted origin the whole security argument rests on (§14.1 F-SEC).
    pub review: Option<crate::reviewintro::ReviewIntroRequest>,
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
            // The ticketless sibling of the line above, at the same moment and for the same
            // reason. The two are mutually exclusive by their gates, so at most one of them is
            // ever `Some` (STUDIO-720).
            review: self.plan_review_intro(re),
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
            // The ticketless path's introduction, gated on the same landed move and for the same
            // reason: a move the tracker refused is not a handoff, so it introduces no pull request
            // into the watch set either (STUDIO-720).
            self.request_review_intro(plan.review);
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

    // ── the Rhapsody Teams review quorum (STUDIO-659, T7; design record §0.6, §0.12) ────────────
    //
    // The handoff IS the quorum's trigger, so these tests live here: they drive the real
    // `handoff_run` and assert on what came out of the channel it feeds.

    /// A quorum-enabled Teams over `roster`.
    fn quorum_teams(roster: &[&str]) -> rhapsody_config::teams::Teams {
        rhapsody_config::teams::Teams {
            enabled: true,
            quorum: rhapsody_config::teams::Quorum {
                enabled: true,
                reviewers: 2,
            },
            roster: roster
                .iter()
                .map(|n| rhapsody_config::teams::Identity {
                    name: (*n).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..rhapsody_config::teams::Teams::disabled()
        }
    }

    /// A ticket with one open linked PR, so the poller's snapshot has a URL to hand the fan-out.
    fn issue_with_pr(id: &str, ident: &str, team: &str) -> Issue {
        let mut i = issue_team(id, ident, "In Progress", team);
        i.title = "do the thing".to_string();
        i.linked_pr = true;
        i.linked_prs = Some(vec![rhapsody_core::LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            merged: false,
        }]);
        i
    }

    /// Dispatches `iss` AS `identity` with the quorum on, opens the quorum channel, records the
    /// poller snapshot, and returns the loop task + handle + receiver. `snapshot` is what the
    /// candidate sweep saw this tick (the load and the PR/marker facts come from it).
    fn quorum_harness(
        tr: Arc<Fake>,
        teams: rhapsody_config::teams::Teams,
        iss: Issue,
        identity: &str,
        snapshot: &[Issue],
    ) -> (
        tokio::task::JoinHandle<Orchestrator>,
        ControlHandle,
        tokio::sync::mpsc::UnboundedReceiver<crate::quorum::QuorumRequest>,
        i64,
        CancelSignal,
    ) {
        let (mut o, env) = handoff_orch(tr, &["In Review"]);
        o.teams = Some(teams);
        let rx = o.open_quorum_channel();
        o.record_quorum_state(snapshot.iter());
        let id = iss.id.clone();
        o.dispatch_issue(iss, None, None, String::new());
        // `dispatch_issue` stamps the identity only when routing produced one; these tests state it
        // directly so the trigger, not the router, is what is under test. `project_repo` is the
        // remote the run's worktree pushed to, which is where STUDIO-674's head-branch PR lookup is
        // aimed, and `project_slug` is the project whose tracker STUDIO-677's fan-out creates
        // through; the resolved-project wiring that normally fills both is not under test here.
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = identity.to_string();
            re.project_repo = "git@github.com:o/r.git".to_string();
            re.project_slug = "proj-a".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);
        (task, handle, rx, run_id, env.signal)
    }

    // The acceptance path end to end: an identity-worn handoff with a PR yields exactly `reviewers`
    // review requests, author excluded, least-loaded first — and the handoff itself is unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_identity_handoff_with_a_pr_requests_a_quorum() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        // A known load state: carol holds one open ticket, dave three, bob none. Roster order is
        // bob, carol, dave — so "least-loaded first" and "roster order" disagree past the first pick.
        let mut carol_work = issue("w1", "MT-9", "Todo");
        carol_work.labels = Some(vec!["rhapsody:@carol".into()]);
        let mut dave_work = issue("w2", "MT-10", "Todo");
        dave_work.labels = Some(vec!["rhapsody:@dave".into()]);
        let mut dave_work2 = issue("w3", "MT-11", "Todo");
        dave_work2.labels = Some(vec!["rhapsody:@dave".into()]);
        let snapshot = vec![parent.clone(), carol_work, dave_work, dave_work2];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol", "dave"]),
            parent,
            "alice",
            &snapshot,
        );

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert_eq!(res.moved_to, "In Review", "the handoff itself is unchanged");

        let req = rx.try_recv().expect("a quorum request was sent");
        assert_eq!(req.parent_issue_id, "ID-1");
        assert_eq!(req.parent_team_id, "TEAM-1");
        assert_eq!(req.parent_identifier, "MT-1");
        assert_eq!(req.parent_title, "do the thing");
        assert_eq!(req.pr_url, "https://github.com/o/r/pull/7");
        assert_eq!(req.author, "alice");
        assert_eq!(
            req.parent_project_slug, "proj-a",
            "the run's OWNING project, so the off-loop task creates the review ticket through \
             that project's slug-bound tracker rather than the slug-less account one (STUDIO-677)"
        );
        assert_eq!(
            req.reviewers,
            vec!["bob".to_string(), "carol".to_string()],
            "author excluded, least-loaded first, capped at reviewers"
        );
        assert_eq!(
            req.state_name, "Todo",
            "the run's project's FIRST configured active state, in the config's own casing (the \
             by-name create resolves it exactly as the by-name move does) — never a hard-coded \
             literal, which would create review tickets this daemon cannot dispatch"
        );
        assert!(rx.try_recv().is_err(), "exactly one request per handoff");

        signal.cancel();
        let _ = task.await;
    }

    // §0.12's "once per ticket": a re-handoff after review fixes fans out NOTHING, decided from the
    // marker label the first fan-out wrote onto the parent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parent_already_marked_requests_nothing() {
        let tr = Arc::new(Fake::new());
        let mut parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        parent.labels = Some(vec![crate::quorum::QUORUM_REQUESTED_LABEL.to_string()]);
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "alice",
            &snapshot,
        );

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert_eq!(res.moved_to, "In Review", "the handoff still succeeds");
        assert!(
            rx.try_recv().is_err(),
            "a re-handoff of a marked parent fans out nothing"
        );

        signal.cancel();
        let _ = task.await;
    }

    // §0.12's "zero ⇒ skip with a loud room post": a roster of one still SENDS a request, carrying
    // no reviewers. The plan half and the task half have to agree on this — the task is where the
    // loud post lives, so a plan that returned `None` here (or a spawn gate that refused a
    // one-person roster) would delete the only signal an operator gets that nothing will ever be
    // reviewed. The post itself is asserted in `quorum::tests::a_roster_of_one_writes_nothing…`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_roster_of_one_still_sends_a_request_so_the_room_can_be_told() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice"]),
            parent,
            "alice",
            &snapshot,
        );

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        let req = rx.try_recv().expect("a request is still sent");
        assert!(
            req.reviewers.is_empty(),
            "nobody to ask, but the request carries the parent + PR the room post names"
        );
        assert_eq!(req.parent_identifier, "MT-1");
        assert_eq!(req.pr_url, "https://github.com/o/r/pull/7");

        signal.cancel();
        let _ = task.await;
    }

    // A run that was NOT dispatched as a roster identity is an ordinary Rhapsody run: there is no
    // author to exclude and no team to ask, so nothing fires.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_non_identity_handoff_requests_nothing() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "", // no identity
            &snapshot,
        );

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(rx.try_recv().is_err(), "no identity ⇒ no quorum");

        signal.cancel();
        let _ = task.await;
    }

    // STUDIO-674: a handoff whose ticket carries no Linear GitHub attachment is no longer dropped
    // on the control loop. This installation's Linear holds `attachments: []` on EVERY issue, so
    // the old gate made the quorum structurally dead — it refused every ticket, forever. The loop
    // now hands the off-loop task what it needs to ask GitHub itself (the run's repo and the
    // `symphony/<identifier>` branch its worktree pushed) and stays network-free doing it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_with_no_attachment_hands_the_branch_to_the_off_loop_task() {
        let tr = Arc::new(Fake::new());
        let mut parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        parent.title = "do the thing".into();
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "alice",
            &snapshot,
        );

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        let req = rx.try_recv().expect("a quorum request was sent");
        assert_eq!(
            req.pr_url, "",
            "the control task resolved nothing — that is the off-loop task's job"
        );
        assert_eq!(
            (req.pr_owner.as_str(), req.pr_repo.as_str()),
            ("o", "r"),
            "parsed from the run's own project repo"
        );
        assert_eq!(
            req.pr_head_branch, "symphony/MT-1",
            "the frozen `symphony/<key>` branch contract the worktree was created on"
        );
        assert_eq!(
            req.reviewers,
            vec!["bob".to_string(), "carol".to_string()],
            "every other gate is unchanged"
        );

        signal.cancel();
        let _ = task.await;
    }

    // STUDIO-674, the legacy single-project shape: `project_repo` is only populated by the
    // resolved-project dispatch path, so a config with no `projects:` block leaves it empty and
    // carries the repo top-level. Without this fallback the head-branch lookup would resolve
    // nothing on exactly the installations most likely to be running one tracker and one repo.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_single_project_run_falls_back_to_the_top_level_repo() {
        let tr = Arc::new(Fake::new());
        let mut parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        parent.title = "do the thing".into();
        let snapshot = [parent.clone()];

        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        if let Some(eff) = o.eff.as_mut() {
            eff.cfg.repo = "https://github.com/o/legacy.git".to_string();
        }
        o.teams = Some(quorum_teams(&["alice", "bob", "carol"]));
        let mut rx = o.open_quorum_channel();
        o.record_quorum_state(snapshot.iter());
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            // Left EMPTY on purpose: that is the legacy path this test exists for.
            assert!(re.project_repo.is_empty());
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        let req = rx.try_recv().expect("a quorum request was sent");
        assert_eq!(
            (req.pr_owner.as_str(), req.pr_repo.as_str()),
            ("o", "legacy"),
            "parsed from the top-level repo when the run carries no project repo"
        );
        assert_eq!(req.pr_head_branch, "symphony/MT-1");

        env.signal.cancel();
        let _ = task.await;
    }

    // The other three gates still refuse BEFORE the attachment question is reached, so an
    // attachment-less ticket that fails one of them still costs no request at all: STUDIO-674
    // widened exactly one gate and left the rest where they were.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_other_gates_still_refuse_an_attachment_less_ticket() {
        for (why, teams, identity, mark) in [
            (
                "no identity ⇒ no quorum",
                quorum_teams(&["alice", "bob", "carol"]),
                "",
                false,
            ),
            (
                "already requested ⇒ no quorum",
                quorum_teams(&["alice", "bob", "carol"]),
                "alice",
                true,
            ),
        ] {
            let tr = Arc::new(Fake::new());
            let mut parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
            parent.title = "do the thing".into();
            if mark {
                parent.labels = Some(vec![crate::quorum::QUORUM_REQUESTED_LABEL.to_string()]);
            }
            let snapshot = vec![parent.clone()];
            let (task, handle, mut rx, run_id, signal) =
                quorum_harness(Arc::clone(&tr), teams, parent, identity, &snapshot);

            handle
                .handoff_run(CancelWait::default(), run_id)
                .await
                .expect("handoff_run");
            assert!(rx.try_recv().is_err(), "{why}");

            signal.cancel();
            let _ = task.await;
        }
    }

    // A ticket with no team id can never be reviewed — `create_issue` and `add_issue_label` both
    // need one — so the quorum refuses up front rather than failing every write, leaving the parent
    // unmarked, and failing again on every subsequent handoff. Triage drops team-less tickets for
    // the same reason.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_with_no_team_id_requests_nothing() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", ""); // no team
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "alice",
            &snapshot,
        );

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(
            rx.try_recv().is_err(),
            "no team id ⇒ no quorum, and no recurring failure post"
        );

        signal.cancel();
        let _ = task.await;
    }

    // A handoff whose review-state move the tracker REFUSED is not a handoff, so it must not fan
    // review tickets out for work whose author is about to keep going.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_move_requests_nothing() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "alice",
            &snapshot,
        );

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(!res.move_err.is_empty(), "the move failed: {res:?}");
        assert!(
            rx.try_recv().is_err(),
            "a handoff that did not land fans out nothing"
        );

        signal.cancel();
        let _ = task.await;
    }

    // The acceptance criterion for the default installation: quorum OFF (and Teams off) means the
    // handoff is byte-identical to what it was before this slice — no channel, no request, and the
    // candidate snapshot is not even recorded.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_quorum_being_off_changes_nothing_about_a_handoff() {
        for teams in [
            None,
            Some(rhapsody_config::teams::Teams::disabled()),
            // Teams ON, quorum absent — the shipped shape of an existing Teams installation.
            Some(rhapsody_config::teams::Teams {
                enabled: true,
                roster: vec![rhapsody_config::teams::Identity {
                    name: "alice".into(),
                    ..Default::default()
                }],
                ..rhapsody_config::teams::Teams::disabled()
            }),
        ] {
            let tr = Arc::new(Fake::new());
            let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
            o.teams = teams.clone();
            let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
            o.record_quorum_state(std::iter::once(&parent));
            assert!(!o.quorum_enabled(), "the quorum must be off for {teams:?}");
            o.dispatch_issue(parent, None, None, String::new());
            if let Some(re) = o.running.get_mut("ID-1") {
                re.identity = "alice".to_string();
            }
            let run_id = o.running["ID-1"].run_id;
            let (task, handle) = start(o, &env.signal);

            let res = handle
                .handoff_run(CancelWait::default(), run_id)
                .await
                .expect("handoff_run");
            assert_eq!(res.moved_to, "In Review");
            assert!(
                tr.create_issue_calls().is_empty(),
                "no tracker create with the quorum off"
            );
            assert!(tr.add_label_calls().is_empty(), "and no label write either");

            env.signal.cancel();
            let o = task.await.expect("loop task");
            assert!(
                o.quorum_facts.is_empty() && o.quorum_load.is_empty(),
                "the candidate sweep is a hard no-op with the quorum off"
            );
        }
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

    // ── ticketless review introduction (STUDIO-720, slice 6) ────────────────────────────────────

    /// A ticketless `teams()` over `names`, with the ticket fan-out off.
    fn ticketless_teams(names: &[&str]) -> rhapsody_config::teams::Teams {
        rhapsody_config::teams::Teams {
            review: rhapsody_config::teams::Review {
                mode: rhapsody_config::teams::ReviewMode::Ticketless,
            },
            ..quorum_teams(names)
        }
    }

    /// The acceptance path: an identity-worn handoff under `review.mode: ticketless` introduces
    /// exactly the pull request of the RUN'S OWN repository binding — and it introduces it only
    /// after the review-state move has landed, exactly as the ticket fan-out does.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ticketless_handoff_introduces_its_own_pull_request() {
        let tr = Arc::new(Fake::new());
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        let snapshot = [parent.clone()];

        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        // The configured allowlist: the project whose repo the run is bound to.
        if let Some(eff) = o.eff.as_mut() {
            let mut p = crate::testsupport::empty_resolved_project("proj-a", Arc::clone(&tr) as _);
            p.repo = "git@github.com:o/r.git".to_string();
            eff.projects = vec![p];
        }
        o.teams = Some(ticketless_teams(&["alice", "bob", "carol"]));
        let quorum_rx = o.open_quorum_channel();
        let mut rx = o.open_review_intro_channel();
        o.record_quorum_state(snapshot.iter());
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            re.project_repo = "git@github.com:o/r.git".to_string();
            re.project_slug = "proj-a".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert_eq!(res.moved_to, "In Review", "the handoff itself is unchanged");

        let req = rx.try_recv().expect("an introduction was requested");
        assert_eq!(
            (req.owner.as_str(), req.repo.as_str()),
            ("o", "r"),
            "parsed from the run's own project repo — never from anything anybody typed"
        );
        assert_eq!(req.repo_url, "git@github.com:o/r.git");
        assert_eq!(req.head_branch, "symphony/MT-1");
        assert_eq!(
            req.reviewers,
            vec!["bob".to_string()],
            "one reviewer by default, and never the author"
        );
        assert_eq!(req.introduced_by, "handoff:MT-1");
        // §14.2's "config cutover double-fire": one handoff fires exactly ONE review path.
        assert!(
            quorum_rx.is_empty(),
            "the ticket fan-out must not fire on the ticketless path"
        );

        env.signal.cancel();
        let _ = task.await;
    }

    /// A handoff the tracker REFUSED has not happened, so it introduces nothing — the same gate the
    /// fan-out is behind, and for the same reason: a ticket still sitting in an active state has an
    /// author who is about to keep going.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_review_state_move_introduces_nothing() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");

        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        if let Some(eff) = o.eff.as_mut() {
            let mut p = crate::testsupport::empty_resolved_project("proj-a", Arc::clone(&tr) as _);
            p.repo = "git@github.com:o/r.git".to_string();
            eff.projects = vec![p];
        }
        o.teams = Some(ticketless_teams(&["alice", "bob"]));
        let mut rx = o.open_review_intro_channel();
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            re.project_repo = "git@github.com:o/r.git".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(!res.move_err.is_empty(), "the move failed");
        assert!(
            rx.try_recv().is_err(),
            "a refused handoff introduces nothing"
        );

        env.signal.cancel();
        let _ = task.await;
    }

    /// **F-SEC at the handoff.** A run bound to a repository no configured project owns introduces
    /// nothing, so no coordinate outside the daemon's own configuration can ever reach the watch set
    /// through this path either.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_on_an_off_allowlist_repository_introduces_nothing() {
        let tr = Arc::new(Fake::new());
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");

        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        if let Some(eff) = o.eff.as_mut() {
            let mut p = crate::testsupport::empty_resolved_project("proj-a", Arc::clone(&tr) as _);
            p.repo = "git@github.com:o/r.git".to_string();
            eff.projects = vec![p];
        }
        o.teams = Some(ticketless_teams(&["alice", "bob"]));
        let mut rx = o.open_review_intro_channel();
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            re.project_repo = "https://github.com/attacker/evil.git".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(
            rx.try_recv().is_err(),
            "a repository no project owns is never introduced"
        );

        env.signal.cancel();
        let _ = task.await;
    }

    /// Off the ticketless path — including the default — a handoff introduces nothing at all, and
    /// the ticket fan-out is exactly what it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_non_ticketless_handoff_introduces_nothing() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        let snapshot = vec![parent.clone()];
        let (task, handle, mut quorum_rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "alice",
            &snapshot,
        );

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        assert!(
            quorum_rx.try_recv().is_ok(),
            "the ticket fan-out is unchanged"
        );
        assert!(
            handle.review_intro.is_none(),
            "a daemon off the ticketless path cannot even represent an introduction"
        );

        signal.cancel();
        let _ = task.await;
    }
}
