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
    /// Set when the handoff REFUSED to move the ticket because it is already terminal or its pull
    /// request has merged (STUDIO-1007). The ticket is already out of the active set, so this is a
    /// success for the run — the caller treats it exactly like a landed move — but the review
    /// quorum / ticketless introduction, which ride on a move this handoff did not make, do not
    /// fire.
    pub already_done: bool,
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
    /// The configured terminal state name (`teams.review.done_state`), non-empty only when the
    /// merge→Done transition is configured (STUDIO-1007). **Empty is the whole gate for the
    /// terminal/merged guard below**: an installation with no `done_state` has no auto-done for a
    /// late handoff to race, so the guard is inert and the handoff is byte-identical to a daemon
    /// built before this ticket — including the extra tracker read the guard would otherwise make.
    pub done_state: String,
    /// The run's owning project's terminal-state set, NORMALIZED (the form the tracker's state
    /// names are compared in), so the off-loop guard can classify a freshly-read ticket state.
    pub terminal_states: Vec<String>,
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
    /// The un-primed human-hold refusal `plan_quorum` carried out, to RECORD on the project's
    /// advisory surface once the review-state move lands (STUDIO-949 round 18). `None` whenever the
    /// fan-out was not refused for that reason.
    ///
    /// Carried rather than recorded at plan time because `handle_handoff_run` runs BEFORE
    /// [`handoff_run`](ControlHandle::handoff_run) attempts the move, and a move the tracker
    /// refused is not a handoff: recording there named a review as permanently lost while the agent
    /// was still retrying, and re-armed it on every attempt. It is recorded beside
    /// [`quorum`](Self::quorum) it stands in for, under the same landed-move gate.
    pub lost_review: Option<crate::quorum::DroppedQuorum>,
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

/// How many times the review-state move is attempted before the handoff reports it failed
/// (STUDIO-838). Only a TRANSIENT failure earns another attempt; a refusal fails at the first.
///
/// Three, [`crate::quorum::QUORUM_FANOUT_ATTEMPTS`]'s value for its reason — the failure it exists
/// for is a single connection blip — but paced very differently, because this one runs on the
/// request path of an agent's `symphony_handoff` tool call rather than on a background task. The
/// quorum can afford the exponential back-off and spread three attempts over minutes; here that
/// would hang the agent's terminal action, so the delays are fixed and short.
pub const HANDOFF_MOVE_ATTEMPTS: u32 = 3;

/// The delay before each retry, in milliseconds — one entry per attempt after the first.
///
/// Sized against the whole budget rather than against Linear: three attempts cost at most one
/// second of extra latency on the agent's terminal tool call, which is imperceptible next to a
/// turn. A longer back-off would ride out more outages and is the wrong trade here — the failures
/// that need minutes are the ones the ADOPT path repairs without an agent waiting at all.
const HANDOFF_MOVE_RETRY_DELAYS_MS: [u64; 2] = [250, 750];

/// Whether a failed review-state move is worth attempting again (STUDIO-838).
///
/// The distinction is whether the tracker CONSIDERED the request. A transport failure and a
/// "not now" status both mean it did not: the move may still be possible, and the introduction
/// that rides on it is worth one more attempt. Everything else — a rejected move, a malformed
/// query, a state that does not exist, no tracker at all — is the tracker having considered the
/// request and refused it, and no number of retries changes a refusal.
///
/// Deliberately narrow, because the sibling defect this ticket cites (STUDIO-836) was a retry with
/// no bound on a failure that was never going to clear. A refusal must fail FAST so the agent gets
/// its error and falls back; only the failures that plausibly self-heal within seconds are retried.
///
/// A stub-shaped [`TrackerError::Other`] is not retried either. It is what the file adapter and the
/// test double return, and — the case that matters in production — it is how "no effective tracker"
/// reaches here, which no retry can fix.
pub(crate) fn move_is_transient(err: &rhapsody_tracker::TrackerError) -> bool {
    let rhapsody_tracker::TrackerError::Linear(e) = err else {
        return false;
    };
    match e.kind {
        rhapsody_tracker::linear::LinearErrorKind::ApiRequest => true,
        rhapsody_tracker::linear::LinearErrorKind::ApiStatus => {
            http_status_is_transient(&e.context)
        }
        _ => false,
    }
}

/// Whether the HTTP status an [`ApiStatus`](rhapsody_tracker::linear::LinearErrorKind::ApiStatus)
/// context names is one the server may answer differently a moment later.
///
/// The context is the linear client's `"status {code}: {body snippet}"`, so the code is read off
/// the front rather than sniffed out of the body. A context that names NO parseable status says
/// nothing about whether the request landed and is therefore not retried — and the body is never
/// consulted, which keeps this from turning a wording change at Linear into a retry loop.
fn http_status_is_transient(context: &str) -> bool {
    let Some(code) = context
        .strip_prefix("status ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|code| code.trim().parse::<u16>().ok())
    else {
        return false;
    };
    // 408 Request Timeout and 429 Too Many Requests are the server declining to answer NOW; 5xx is
    // it failing to. Every other 4xx is a refusal of this request as written.
    matches!(code, 408 | 429) || (500..600).contains(&code)
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
        // The quorum decision, and the un-primed refusal inside it if that is why it refused. The
        // refusal travels to `handoff_run` rather than being recorded here — see
        // [`HandoffPlan::lost_review`].
        let quorum_plan = self.plan_quorum(re);
        // The terminal/merged guard's inputs (STUDIO-1007), resolved per the run's owning project
        // exactly as the review state above is. `done_state` empty ⇒ the guard is inert.
        let (done_state, terminal_states) = self.handoff_done_guard(&re.project_slug);
        HandoffPlan {
            found: true,
            issue_id: id.clone(),
            team_id: re.issue.team_id.clone(),
            identifier: re.issue.identifier.clone(),
            review_state: self.review_handoff_state(&re.project_slug),
            done_state,
            terminal_states,
            // §0.12's trigger: "a teammate's handoff with a linked PR". This is that moment, and it
            // is the moment the daemon EXECUTES rather than merely infers, which is why the design
            // chose it over "PR opened" (the PR exists mid-run, long before it is reviewable) or
            // "review posted" (that is the quorum's output, not its input).
            quorum: quorum_plan.request,
            lost_review: quorum_plan.dropped,
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

    /// The terminal/merged guard's inputs for a run's owning project (STUDIO-1007): the configured
    /// terminal state NAME (`teams.review.done_state`, empty ⇒ the transition is off) and the
    /// project's NORMALIZED terminal-state set, for classifying a freshly-read ticket state.
    ///
    /// Read together and per-project for [`review_handoff_state`](Orchestrator::review_handoff_state)'s
    /// reason: the guard must agree with the auto-done transition it exists to protect, and both are
    /// resolved through the same `effective_for` overlay.
    fn handoff_done_guard(&self, project_slug: &str) -> (String, Vec<String>) {
        let done_state = self
            .teams
            .as_ref()
            .and_then(|t| t.review_done_state())
            .unwrap_or_default()
            .to_string();
        // The guard is inert without a `done_state`, so the terminal set is not even resolved —
        // which is what keeps a default installation's plan byte-identical (no config clone, no
        // tracker read, no terminal set).
        if done_state.is_empty() {
            return (done_state, Vec::new());
        }
        let Some(eff) = self.eff.as_ref() else {
            return (done_state, Vec::new());
        };
        let project = if project_slug.is_empty() {
            None
        } else {
            eff.cfg
                .projects
                .iter()
                .find(|p| p.slugs.iter().any(|s| s == project_slug))
        };
        let terminal_states = rhapsody_config::effective_for(&eff.cfg, project)
            .terminal_states
            .into_iter()
            .map(|s| rhapsody_core::normalize_state(&s))
            .collect();
        (done_state, terminal_states)
    }
}

/// Why a handoff refused to move its ticket into the review state (STUDIO-1007): the ticket is
/// already terminal, or the pull request it belongs to has merged and the terminal move is still
/// owed. The auto-done move wins either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DoneRefusal {
    /// The ticket's pull request merged and its terminal move is still owed — the durable ledger
    /// named the pull request.
    Merged { pr: String },
    /// The ticket's CURRENT tracker state is already terminal.
    Terminal { state: String },
}

impl DoneRefusal {
    /// The refusal's log line, naming the ticket (always) and the pull request (when it is known).
    fn log(&self, plan: &HandoffPlan) {
        match self {
            DoneRefusal::Merged { pr } => tracing::info!(
                issue_identifier = %plan.identifier,
                pr = %pr,
                review_state = %plan.review_state,
                "handoff: not moving {} to {} — its pull request {} merged and the terminal move \
                 owns the ticket now (the auto-done move wins)",
                plan.identifier,
                plan.review_state,
                pr
            ),
            DoneRefusal::Terminal { state } => tracing::info!(
                issue_identifier = %plan.identifier,
                current_state = %state,
                review_state = %plan.review_state,
                "handoff: not moving {} to {} — it is already in the terminal state {} (the \
                 auto-done move wins)",
                plan.identifier,
                plan.review_state,
                state
            ),
        }
    }
}

/// Whether a freshly-read tracker state names a terminal state for this run's project.
///
/// A failure to READ the state is deliberately not terminal: the guard exists to stop a handoff
/// moving a ticket back, and refusing every handoff on a tracker blip would be a worse trade than
/// letting one through in the window before the next fresh read. See
/// [`ControlHandle::done_refusal`].
fn state_is_terminal(state: &str, terminal_states: &[String]) -> bool {
    let normalized = rhapsody_core::normalize_state(state);
    (!normalized.is_empty()) && terminal_states.contains(&normalized)
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
        // A handoff never moves a terminal ticket back (STUDIO-1007). Only checked when the
        // merge→Done transition is configured: with no `done_state` there is no auto-done for a
        // late handoff to race, so the guard is inert (and makes no extra tracker read), which is
        // what keeps an installation without it byte-identical to a daemon built before this ticket.
        if !plan.done_state.is_empty()
            && let Some(refusal) = self.done_refusal(&plan).await
        {
            refusal.log(&plan);
            return Ok(HandoffResult {
                identifier: plan.identifier,
                already_done: true,
                ..Default::default()
            });
        }
        let mut res = HandoffResult {
            identifier: plan.identifier,
            ..Default::default()
        };
        match self
            .move_issue_state_retrying(
                &plan.issue_id,
                &plan.team_id,
                &plan.review_state,
                &res.identifier,
            )
            .await
        {
            Ok(()) => res.moved_to = plan.review_state,
            Err(e) => res.move_err = e.to_string(),
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
            // A quorum the un-primed fail-closed gate dropped is recorded on the project advisory
            // ONLY now the move has landed (STUDIO-949 round 18): `plan_quorum` decided it on the
            // control task, before this attempt, and a move the tracker refused is not a handoff —
            // recording at plan time named a review as permanently lost while the agent was still
            // retrying, and re-armed it on every attempt. `give_up`'s own doc gives the reason the
            // advisory is more than the `WARN` line the gate already logged.
            if let Some(dropped) = plan.lost_review.as_ref() {
                self.warnings
                    .record_lost_review(&dropped.group, &dropped.identifier, &dropped.why);
            }
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

    /// [`move_issue_state`](Self::move_issue_state), attempted up to [`HANDOFF_MOVE_ATTEMPTS`]
    /// times while the failure is TRANSIENT (STUDIO-838).
    ///
    /// The retry is here rather than inside the tracker because what is worth rescuing is not the
    /// move: it is the ticketless review INTRODUCTION, which `handoff_run` fires only on a move
    /// that landed. Discarding it because one request never reached Linear leaves the pull request
    /// orphaned — open, green, in the review state and invisible to everything that assigns a
    /// reviewer — which is the whole of STUDIO-838.
    ///
    /// A refusal returns at the first attempt, so the agent's tool call is never delayed by a
    /// failure that was never going to clear ([`move_is_transient`] states the line exactly). The
    /// wait is a plain `sleep` and not the lifetime ctx's `select!`, unlike the quorum's: the
    /// longest this can hold is a second, and a handoff already in flight when the daemon is asked
    /// to stop should finish rather than report a false failure.
    async fn move_issue_state_retrying(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
        identifier: &str,
    ) -> Result<(), rhapsody_tracker::TrackerError> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let err = match self.move_issue_state(issue_id, team_id, state_name).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            if !move_is_transient(&err) || attempt >= HANDOFF_MOVE_ATTEMPTS {
                return Err(err);
            }
            tracing::warn!(
                issue_identifier = %identifier,
                attempt,
                attempts = HANDOFF_MOVE_ATTEMPTS,
                err = %err,
                "handoff: the review-state move did not reach the tracker; retrying so the review \
                 introduction is not lost with it"
            );
            // In range by construction — the loop returned above unless `attempt` is below
            // `HANDOFF_MOVE_ATTEMPTS`, and the table holds one entry per attempt after the first.
            // Indexed rather than sliced anyway, so a future edit to either constant degrades to a
            // short wait instead of a panic on the agent's terminal tool call.
            let delay = HANDOFF_MOVE_RETRY_DELAYS_MS
                .get(attempt as usize - 1)
                .copied()
                .unwrap_or(250);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
    }

    /// The terminal/merged guard's decision for `plan` (STUDIO-1007): `Some` when the handoff must
    /// NOT move the ticket into the review state, `None` when it may proceed.
    ///
    /// Two independent facts, checked in cheapest-first order:
    ///
    /// 1. **The durable merge.** A row in the terminal-move ledger for this ticket means its pull
    ///    request MERGED and the auto-Done move is still owed. That row is written before the first
    ///    move attempt and cleared only when the move lands, so it is the restart-surviving form of
    ///    "the PR merged" — not a note that this process happened to run auto-done. No tracker call
    ///    is needed to see it, and it names WHICH pull request for the log.
    /// 2. **The ticket's own state**, read fresh from the tracker. This is what catches the case the
    ///    ledger cannot: auto-done LANDED (so its row is gone) and a still-running author's turn
    ///    finishes thirteen seconds later and calls handoff. The in-memory issue snapshot is stale
    ///    by exactly that window, which is why the state is re-read here rather than trusted.
    ///
    /// A tracker read that FAILS is not terminal and not a refusal (see [`Self::fresh_ticket_state`]
    /// and [`state_is_terminal`]): the guard fails OPEN, because blocking every handoff on a tracker
    /// blip is a worse trade than one late move the next tick's auto-done re-does.
    pub(crate) async fn done_refusal(&self, plan: &HandoffPlan) -> Option<DoneRefusal> {
        if let Some(pr) = self.owed_review_done_pr(&plan.identifier) {
            return Some(DoneRefusal::Merged { pr });
        }
        let state = self.fresh_ticket_state(&plan.issue_id).await?;
        state_is_terminal(&state, &plan.terminal_states).then_some(DoneRefusal::Terminal { state })
    }

    /// The pull request of the terminal move still owed to `identifier`, or `None` when nothing is
    /// owed. A store that cannot be read answers `None` (the guard then falls through to the fresh
    /// tracker read) and warns, exactly as every other un-actionable store failure here does.
    fn owed_review_done_pr(&self, identifier: &str) -> Option<String> {
        match self.store.load_review_done() {
            Ok(rows) => rows
                .into_iter()
                .find(|r| r.identifier == identifier)
                .map(|r| r.pr),
            Err(e) => {
                tracing::warn!(
                    issue_identifier = %identifier,
                    err = %e,
                    "handoff: the owed terminal moves could not be read; the merge guard falls \
                     through to the ticket's own state"
                );
                None
            }
        }
    }

    /// The ticket's CURRENT tracker state, read off-loop, or `None` when the tracker cannot be
    /// reached. Resolves the tracker exactly like [`Self::move_issue_state`] (the `control()`-time
    /// snapshot, else the shared reads tracker) and never holds the reads guard across the await.
    async fn fresh_ticket_state(&self, issue_id: &str) -> Option<String> {
        let tracker = self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .tracker
                .clone()
        })?;
        match tracker
            .fetch_issue_states_by_ids(&[issue_id.to_string()])
            .await
        {
            // Empty (the issue was not found) is "unknown", not terminal — never a refusal.
            Ok(issues) => issues.into_iter().next().map(|i| i.state),
            Err(e) => {
                tracing::warn!(
                    issue_id = %issue_id,
                    err = %e,
                    "handoff: the ticket's current state could not be read; the terminal guard \
                     cannot refuse on it"
                );
                None
            }
        }
    }

    /// The off-loop by-NAME `MoveIssueState` for handoff, resolving the tracker exactly like
    /// [`stop_run`](ControlHandle::stop_run)'s `move_to` (the `control()`-time snapshot, else the shared
    /// reads tracker so the daemon — which builds the handle before the first reload — still moves
    /// tickets). Returns the tracker error text on failure; no tracker at all (before the first config
    /// load) is a move failure so the agent falls back to the Linear-MCP path. Clones the handle out
    /// before any await; the reads guard is never held across it.
    pub(crate) async fn move_issue_state(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
    ) -> Result<(), rhapsody_tracker::TrackerError> {
        let tracker = self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .tracker
                .clone()
        });
        match tracker {
            Some(tr) => tr.move_issue_state(issue_id, team_id, state_name).await,
            // Typed rather than a bare string since STUDIO-838, because the caller now CLASSIFIES
            // the failure: `Other` is not transient, which is the right answer — no retry conjures
            // a tracker that has not been configured yet.
            None => Err(rhapsody_tracker::TrackerError::Other(
                "no effective tracker".to_string(),
            )),
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
    use rhapsody_tracker::linear::LinearErrorKind;
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
    ///
    /// `prime` seeds the human-hold ledger with one no-hold pass. Every handoff fixture primes, so the
    /// un-primed fail-closed branch in `plan_quorum` (STUDIO-949 round 12) is exercised by its own
    /// tests rather than being what every fixture happens to trip; the round-18 landed-gate tests pass
    /// `false` on purpose.
    fn handoff_orch_with(
        tr: Arc<Fake>,
        review_states: &[&str],
        prime: bool,
    ) -> (Orchestrator, Env) {
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
        if prime {
            o.human_holds.begin_pass(true);
        }
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

    /// [`handoff_orch_with`], primed — the shape every ordinary handoff test wants.
    fn handoff_orch(tr: Arc<Fake>, review_states: &[&str]) -> (Orchestrator, Env) {
        handoff_orch_with(tr, review_states, true)
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

    // STUDIO-822 inverts §0.12's "once per ticket": a re-handoff after review fixes STILL sends a
    // request, because the marker label says only "this ticket was reviewed once" and the rounds it
    // used to refuse are exactly the rounds that exist because a reviewer found something. Whether
    // anything is fanned OUT is now the off-loop task's per-head decision, which is the only place
    // the head is known.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parent_already_marked_still_requests_because_the_guard_is_per_head() {
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
        let req = rx.try_recv().expect(
            "the marker label must not refuse the request: it cannot carry a head, so it cannot \
             tell a second round from a repeat of the first",
        );
        assert_eq!(req.parent_identifier, "MT-1");

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

    // The identity gate still refuses BEFORE the attachment question is reached, so an
    // attachment-less ticket without an identity still costs no request at all: STUDIO-674 widened
    // exactly one gate and left the rest where they were. The marker gate is no longer one of them
    // (STUDIO-822) — see `a_parent_already_marked_still_requests_because_the_guard_is_per_head`.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_identity_gate_still_refuses_an_attachment_less_ticket() {
        let tr = Arc::new(Fake::new());
        let mut parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        parent.title = "do the thing".into();
        let snapshot = vec![parent.clone()];
        let (task, handle, mut rx, run_id, signal) = quorum_harness(
            Arc::clone(&tr),
            quorum_teams(&["alice", "bob", "carol"]),
            parent,
            "",
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

    /// Drives one handoff of an UN-PRIMED daemon — the config-gated-since-boot shape — for `iss`,
    /// returning the loop task, handle, quorum receiver, run id and signal. The run is marked
    /// `proj-a` so the advisory group is readable from the returned orchestrator's warnings.
    fn unprimed_handoff_harness(
        tr: Arc<Fake>,
        iss: Issue,
    ) -> (
        tokio::task::JoinHandle<Orchestrator>,
        ControlHandle,
        tokio::sync::mpsc::UnboundedReceiver<crate::quorum::QuorumRequest>,
        i64,
        CancelSignal,
    ) {
        let (mut o, env) = handoff_orch_with(Arc::clone(&tr), &["In Review"], false);
        o.teams = Some(quorum_teams(&["alice", "bob", "carol"]));
        let rx = o.open_quorum_channel();
        o.record_quorum_state(std::iter::once(&iss));
        let id = iss.id.clone();
        o.dispatch_issue(iss, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            re.project_repo = "git@github.com:o/r.git".to_string();
            re.project_slug = "proj-a".to_string();
            re.project_group = "proj-a".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);
        (task, handle, rx, run_id, env.signal)
    }

    // STUDIO-949 round 18 — the un-primed refusal records its advisory ONLY once the review-state
    // move lands. `plan_quorum` runs at PLAN time, before `handoff_run` attempts the move, and a
    // move the tracker refused is not a handoff (`handoff_run` fires the quorum only on a landed
    // move), so recording at plan time named a review as permanently lost while the agent was still
    // retrying — and re-armed the advisory on every attempt.
    //
    // MUTATION: record the refusal inside `plan_quorum` instead of at the landed gate and the
    // `fails` arm reds (an advisory appears for a move that never landed); drop the landed gate
    // around the record here and the same arm reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unprimed_refusal_is_recorded_only_once_the_move_lands() {
        for fails in [false, true] {
            let mut fake = Fake::new();
            if fails {
                fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
            }
            let tr = Arc::new(fake);
            let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
            let (task, handle, mut rx, run_id, signal) = unprimed_handoff_harness(tr, parent);

            let res = handle
                .handoff_run(CancelWait::default(), run_id)
                .await
                .expect("handoff_run");
            assert_eq!(res.move_err.is_empty(), !fails, "move result: {res:?}");
            assert!(
                rx.try_recv().is_err(),
                "the un-primed refusal fans nothing out"
            );

            signal.cancel();
            let o = task.await.expect("loop task");
            let advisories = o.warnings.merged_for("proj-a");
            assert_eq!(
                advisories.iter().any(|l| l.contains("MT-1")),
                !fails,
                "recorded only when the move landed (fails={fails}): {advisories:?}"
            );
        }
    }

    // STUDIO-949 round 18 — the advisory is keyed by ticket, so the un-primed refusal re-firing on
    // every handoff attempt for one ticket refreshes one line instead of spending the whole
    // five-slot cap on it and evicting the group's other lost reviews.
    //
    // MUTATION: push instead of refresh in `WarningsState::record_lost_review` and this reds (six
    // calls produce five lines).
    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_unprimed_handoffs_keep_one_advisory_line() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", "TEAM-1");
        let (task, handle, mut rx, run_id, signal) = unprimed_handoff_harness(tr, parent);

        for _ in 0..6 {
            handle
                .handoff_run(CancelWait::default(), run_id)
                .await
                .expect("handoff_run");
        }
        assert!(rx.try_recv().is_err(), "nothing fans out while un-primed");

        signal.cancel();
        let o = task.await.expect("loop task");
        let lines: Vec<_> = o
            .warnings
            .merged_for("proj-a")
            .into_iter()
            .filter(|l| l.contains("MT-1"))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "one line per ticket, not per attempt: {lines:?}"
        );
    }

    // STUDIO-949 round 18 — a team-less ticket has no review to lose, primed or not, so the
    // un-primed refusal sits BELOW the team-id refusal and records no advisory for it. The other
    // order would claim a review was dropped, and advise a re-summon that could fix nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unprimed_teamless_handoff_records_no_advisory() {
        let tr = Arc::new(Fake::new());
        let parent = issue_with_pr("ID-1", "MT-1", ""); // no team
        let (task, handle, mut rx, run_id, signal) = unprimed_handoff_harness(tr, parent);

        handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");
        assert!(rx.try_recv().is_err(), "no team ⇒ no fan-out");

        signal.cancel();
        let o = task.await.expect("loop task");
        assert!(
            o.warnings.merged_for("proj-a").is_empty(),
            "a team-less ticket was never reviewable, so no review was lost: {:?}",
            o.warnings.merged_for("proj-a")
        );
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

    // ── which move failures may be retried (STUDIO-838) ─────────────────────────────────────────

    fn linear(kind: LinearErrorKind, context: &str) -> TrackerError {
        TrackerError::Linear(rhapsody_tracker::linear::LinearError::new(kind, context))
    }

    /// The classification, stated exactly, because "be exact about which errors are transient" is
    /// the ticket's own warning and the sibling defect (STUDIO-836) was a retry that never stopped.
    ///
    /// Transient means the REQUEST did not land: the transport failed, or the server said "not
    /// now". Everything else is the tracker having considered the request and refused it, and no
    /// number of retries changes a refusal.
    #[test]
    fn only_a_transport_failure_or_a_now_now_status_is_retried() {
        for (want, err) in [
            // The transport never delivered it. This is the failure STUDIO-822 lost a review round
            // to — a single `error sending request` that the next attempt would have ridden out.
            (
                true,
                linear(LinearErrorKind::ApiRequest, "error sending request"),
            ),
            (true, linear(LinearErrorKind::ApiRequest, "read body: eof")),
            // The server took it and said "not now".
            (
                true,
                linear(LinearErrorKind::ApiStatus, "status 429: slow down"),
            ),
            (
                true,
                linear(LinearErrorKind::ApiStatus, "status 408: timeout"),
            ),
            (true, linear(LinearErrorKind::ApiStatus, "status 500: oops")),
            (
                true,
                linear(LinearErrorKind::ApiStatus, "status 502: bad gateway"),
            ),
            (
                true,
                linear(LinearErrorKind::ApiStatus, "status 503: unavailable"),
            ),
            // **The observed STUDIO-836 failure, and deliberately NOT retried.** Linear reports
            // hourly quota exhaustion as a 429 body inside a 400, so it lands here rather than as a
            // real 429 — and an hour-long quota is not something a retry seconds later rides out.
            // Retrying it would spend attempts to fail identically. This is the case the ADOPT path
            // exists for; the retry covers the blip, not the quota.
            (
                false,
                linear(
                    LinearErrorKind::ApiStatus,
                    "status 400: \"Rate limit exceeded. Only 2500 requests are allowed per 1 hour\"",
                ),
            ),
            (false, linear(LinearErrorKind::ApiStatus, "status 401: no")),
            (false, linear(LinearErrorKind::ApiStatus, "status 403: no")),
            (false, linear(LinearErrorKind::ApiStatus, "status 404: no")),
            // A context that does not name a status at all says nothing about whether the request
            // landed, so it is not retried.
            (false, linear(LinearErrorKind::ApiStatus, "unparseable")),
            // The tracker CONSIDERED the request and refused it.
            (
                false,
                linear(LinearErrorKind::MoveRejected, "success:false"),
            ),
            (false, linear(LinearErrorKind::GraphqlErrors, "bad field")),
            (false, linear(LinearErrorKind::UnknownPayload, "junk")),
            (false, linear(LinearErrorKind::ViewerUnresolved, "")),
            (false, TrackerError::StateNotFound("no such state".into())),
            // Including the daemon's own "there is no tracker yet", which no retry can fix.
            (false, TrackerError::Other("no effective tracker".into())),
        ] {
            assert_eq!(move_is_transient(&err), want, "{err:?}");
        }
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
                ..rhapsody_config::teams::Review::default()
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

    /// **STUDIO-838, the retry, asserted to the row.** A review-state move that fails TRANSIENTLY
    /// and then succeeds ends with the watch row that assigns a reviewer — not merely with a
    /// request that was not discarded.
    ///
    /// The row is the property, because a handoff that moved the ticket and lost the introduction
    /// is exactly the orphan this ticket is about, and every hop between the request and the row
    /// is somewhere it could still be lost. So the real off-loop
    /// [`run_review_intro_task`](crate::reviewintro::run_review_intro_task) runs here over the real
    /// control loop; only GitHub is faked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_move_failure_is_retried_and_still_ends_with_a_watch_row() {
        let mut fake = Fake::new();
        fake.move_err = Some(linear(LinearErrorKind::ApiRequest, "error sending request"));
        fake.move_err_calls = 1; // the first attempt fails, the second lands
        let tr = Arc::new(fake);
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");

        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        if let Some(eff) = o.eff.as_mut() {
            let mut p = crate::testsupport::empty_resolved_project("proj-a", Arc::clone(&tr) as _);
            p.repo = "git@github.com:o/r.git".to_string();
            eff.projects = vec![p];
        }
        o.teams = Some(ticketless_teams(&["alice", "bob"]));
        let rx = o.open_review_intro_channel();
        let store = Arc::clone(&o.store);
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        if let Some(re) = o.running.get_mut(&id) {
            re.identity = "alice".to_string();
            re.project_repo = "git@github.com:o/r.git".to_string();
        }
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);
        let intro = tokio::spawn(crate::reviewintro::run_review_intro_task(
            env.signal.wait(),
            crate::reviewintro::ReviewIntroDeps {
                pr_source: Some(Arc::new(OnePr("https://github.com/o/r/pull/7"))),
                sink: Arc::new(crate::reviewintro::ControlIntroSink::new(handle.clone())),
                linker: None,
            },
            rx,
        ));

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        assert!(res.move_err.is_empty(), "the blip was ridden out");
        assert_eq!(res.moved_to, "In Review");
        assert_eq!(
            tr.move_calls().len(),
            2,
            "one retry, and no more than the failure needed"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let rows = loop {
            let rows = store.load_review_watch().expect("read");
            if !rows.is_empty() {
                break rows;
            }
            if tokio::time::Instant::now() > deadline {
                env.signal.cancel();
                panic!("the introduction was lost with the failed attempt");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].key.number, 7);
        assert_eq!(rows[0].key.reviewer, "bob");
        assert_eq!(rows[0].introduced_by, "handoff:MT-1");

        env.signal.cancel();
        let _ = task.await;
        let _ = intro.await;
    }

    /// An [`OpenPrSource`](crate::ghsummons::OpenPrSource) that always answers the same open pull
    /// request — the one hop a test cannot really make.
    struct OnePr(&'static str);

    #[async_trait::async_trait]
    impl crate::ghsummons::OpenPrSource for OnePr {
        async fn open_pr_for_branch(
            &self,
            _owner: &str,
            _repo: &str,
            _branch: &str,
        ) -> crate::ghsummons::OpenPrResult {
            Ok(Some(crate::ghsummons::OpenPr {
                url: self.0.to_string(),
                head_sha: String::new(),
            }))
        }
    }

    /// The other half, and the one STUDIO-836's lesson is about: a move the tracker CONSIDERED and
    /// refused is attempted exactly once. Retrying a refusal only delays the error the agent needs
    /// in order to fall back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_move_is_never_retried() {
        let mut fake = Fake::new();
        fake.move_err = Some(linear(LinearErrorKind::MoveRejected, "success: false"));
        let tr = Arc::new(fake);
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

        assert!(!res.move_err.is_empty(), "the refusal still surfaces");
        assert_eq!(
            tr.move_calls().len(),
            1,
            "considered and refused: asked once"
        );

        env.signal.cancel();
        let _ = task.await;
    }

    /// A transient failure that NEVER clears stops at the attempt bound rather than looping — the
    /// defect STUDIO-836 is, in the module this ticket touches.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_failure_that_never_clears_stops_at_the_attempt_bound() {
        let mut fake = Fake::new();
        fake.move_err = Some(linear(
            LinearErrorKind::ApiStatus,
            "status 503: unavailable",
        ));
        let tr = Arc::new(fake);
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

        assert!(!res.move_err.is_empty(), "it still fails, and says so");
        assert_eq!(
            tr.move_calls().len(),
            HANDOFF_MOVE_ATTEMPTS as usize,
            "bounded"
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

    // ── STUDIO-1007: a handoff never moves a terminal ticket back ────────────────────────────────

    /// Teams on, ticketless review, a named `done_state` — the only shape the terminal/merged guard
    /// is armed on.
    fn done_guard_teams() -> rhapsody_config::teams::Teams {
        rhapsody_config::teams::Teams {
            enabled: true,
            review: rhapsody_config::teams::Review {
                mode: rhapsody_config::teams::ReviewMode::Ticketless,
                done_state: "Done".to_string(),
                ..rhapsody_config::teams::Review::default()
            },
            ..rhapsody_config::teams::Teams::disabled()
        }
    }

    /// Dispatches one live run and arms the guard: `done_state` configured, `Done` terminal, and
    /// `by_id` (the fresh tracker read) answering the state the test wants. Returns the loop task,
    /// the handle, the run id and the signal.
    fn done_guard_harness(
        fake: Fake,
        by_id_state: Option<&str>,
    ) -> (
        tokio::task::JoinHandle<Orchestrator>,
        ControlHandle,
        i64,
        CancelSignal,
        Arc<Fake>,
    ) {
        let mut fake = fake;
        if let Some(state) = by_id_state {
            fake.by_id.insert(
                "ID-1".to_string(),
                issue_team("ID-1", "MT-1", state, "TEAM-1"),
            );
        }
        let tr = Arc::new(fake);
        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        o.teams = Some(done_guard_teams());
        if let Some(eff) = o.eff.as_mut() {
            eff.cfg.tracker.terminal_states = vec!["Done".to_string()];
        }
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);
        (task, handle, run_id, env.signal, tr)
    }

    /// **STUDIO-1007 acceptance: replay STUDIO-995.** Auto-done moved the ticket to `Done` thirteen
    /// seconds ago, so its durable row is already gone; the still-running author's turn finishes and
    /// calls handoff. The ticket must stay Done: the handoff REFUSES and makes no move at all.
    ///
    /// MUTATION (the ticket's ⚠️): drop the terminal read from `done_refusal` and the ticket is
    /// moved back into review — the incident this ticket exists to close.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_leaves_an_auto_done_ticket_in_its_terminal_state() {
        let (task, handle, run_id, signal, tr) = done_guard_harness(Fake::new(), Some("Done"));

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        assert!(res.already_done, "the handoff refused: {res:?}");
        assert!(res.moved_to.is_empty(), "no review move was made: {res:?}");
        assert!(
            tr.move_calls().is_empty(),
            "a terminal ticket is never moved back into review: {:?}",
            tr.move_calls()
        );

        signal.cancel();
        let _ = task.await;
    }

    /// **STUDIO-1007: the durable merge wins even before the ticket reads terminal.** The pull
    /// request merged (the owed-move row exists) but the tracker's state has not caught up; a late
    /// handoff must not move the ticket back while that move is owed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_leaves_a_ticket_whose_merged_pull_request_still_owes_its_move() {
        let (task, handle, run_id, signal, tr) = done_guard_harness(Fake::new(), None); // the tracker answers nothing: the ROW is the fact

        // The merge's own durable row — written by auto-done before its first move attempt.
        handle
            .store
            .save_review_done(rhapsody_store::ReviewDoneRow {
                identifier: "MT-1".to_string(),
                pr: "o/r#7".to_string(),
                issue_id: "ID-1".to_string(),
                team_id: "TEAM-1".to_string(),
                state: "Done".to_string(),
                attempts: 1,
                next_at: String::new(),
                gave_up: false,
            })
            .expect("record the owed move");

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        assert!(res.already_done, "the merged pull request wins: {res:?}");
        assert!(tr.move_calls().is_empty(), "{:?}", tr.move_calls());

        signal.cancel();
        let _ = task.await;
    }

    /// **The default installation is byte-identical.** With no `done_state` there is no auto-done
    /// for a late handoff to race, so the guard is inert: a ticket that happens to read terminal is
    /// still moved to review exactly as it was before STUDIO-1007, and no extra tracker read is
    /// made.
    #[tokio::test(flavor = "multi_thread")]
    async fn with_no_done_state_the_handoff_is_unchanged() {
        let mut fake = Fake::new();
        fake.by_id.insert(
            "ID-1".to_string(),
            issue_team("ID-1", "MT-1", "Done", "TEAM-1"),
        );
        let tr = Arc::new(fake);
        let (mut o, env) = handoff_orch(Arc::clone(&tr), &["In Review"]);
        // Teams left OFF: `review_done_state()` is `None`, so the guard never runs.
        if let Some(eff) = o.eff.as_mut() {
            eff.cfg.tracker.terminal_states = vec!["Done".to_string()];
        }
        let parent = issue_team("ID-1", "MT-1", "In Progress", "TEAM-1");
        let id = parent.id.clone();
        o.dispatch_issue(parent, None, None, String::new());
        let run_id = o.running[&id].run_id;
        let (task, handle) = start(o, &env.signal);

        let res = handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .expect("handoff_run");

        assert!(
            !res.already_done,
            "the guard is inert without done_state: {res:?}"
        );
        assert_eq!(
            res.moved_to, "In Review",
            "the move happens exactly as before"
        );
        assert_eq!(
            tr.move_calls().len(),
            1,
            "and it is the only review-state move"
        );

        env.signal.cancel();
        let _ = task.await;
    }
}
