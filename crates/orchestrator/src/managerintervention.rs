//! managerintervention — the manager's INTERVENTION LIFECYCLE (STUDIO-1015, design record
//! `manager-agent-design.md` §7.1–§7.5, §10.2, §5.1). **No Go v0.4.0 counterpart**: the whole
//! ticketless review loop and the manager that adjudicates it are Rhapsody additions.
//!
//! M8 builds the durable lifecycle around the launch M7 already provided
//! ([`Orchestrator::dispatch_manager`]):
//!
//! * **one active intervention per pull request**, enforced by the store's unique partial index,
//!   across all stall kinds;
//! * **stall routing** — when `manager.review_authority` is not `off`, the reconciliation sweep's
//!   stall signals are handed to the manager instead of the human feed. The sweep still acts on
//!   nothing itself: it enqueues an intervention exactly as the review watcher enqueues a review
//!   ([`Orchestrator::route_stalls_to_manager`]);
//! * **atomic budgets** — every launch is charged through [`Store::reserve_manager_run`], and
//!   nothing is refunded (§7.3);
//! * **leases** — `launching`/`running` hold a lease; a lease from another boot, or one that has
//!   expired, becomes a `failed_attempt` (§7.5);
//! * **every launch gate, at every launch including retries** (§10.2), checked per the drain rule
//!   in `crates/orchestrator/CLAUDE.md`: each gate refuses in its own entry point;
//! * **the stopped generation** — once `manager_stopped` is set the sweep never creates another
//!   intervention for that generation, so a terminal failure is never recreated (§7.2);
//! * **the case packet** — the stall kinds and every watch row's state, rendered as DATA.
//!
//! M9 owns the `applying` internals and the activation transaction; nothing here applies a
//! decision's effects.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rhapsody_config::room::Message;
use rhapsody_config::teams::ReviewAuthority;
use rhapsody_store::{
    MANAGER_INTERVENTION_COMPLETE, MANAGER_INTERVENTION_DECIDED, MANAGER_INTERVENTION_DEFERRED,
    MANAGER_INTERVENTION_FAILED_ATTEMPT, MANAGER_INTERVENTION_LAUNCHING,
    MANAGER_INTERVENTION_NO_REVIEW_GAP, MANAGER_INTERVENTION_PROPOSED, MANAGER_INTERVENTION_QUEUED,
    MANAGER_INTERVENTION_RUNNING, MANAGER_INTERVENTION_STALE, MANAGER_INTERVENTION_SUPERSEDED,
    MANAGER_INTERVENTION_VALIDATED, MANAGER_MODE_ACT, MANAGER_MODE_ADVISE,
    MANAGER_PHASE_POST_THRESHOLD, MANAGER_PHASE_PRE_THRESHOLD, ManagerInterventionRow,
    ManagerReservation, REVIEW_FINDING_OPEN, manager_intervention_is_terminal,
};

use crate::managerdecision::{
    self, ApprovalInputs, DecisionKind, FindingRef, KnownFinding, ManagerDecision,
    ManagerReviewRow, Revalidation, RevalidationInputs, ThresholdPhase,
};
use crate::managerrun::{ManagerDispatchOutcome, ManagerRun};
use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::reviewreconcile::{Divergence, DivergenceKind};

/// Extra wall-clock grace added to `manager.run_timeout_ms` when a launch writes its lease
/// (`lease_expires_at = now + run_timeout + slack`), so a run that is still winding down at its
/// nominal timeout is not recovered out from under itself (§7.5).
pub const MANAGER_LEASE_SLACK: Duration = Duration::seconds(120);

/// The `stall_kind` token a divergence maps to when the manager owns it (§5.1, D3). The five
/// signals the ticket names: `review_escalated`, `review_shipped`, approved-still-open, author
/// token-ceiling and the round threshold. Every other divergence kind is not a manager stall — it
/// is ordinary progress, a merge gate, or a ticket-side duty the sweep reports alone.
pub fn stall_kind_for(kind: DivergenceKind) -> Option<&'static str> {
    match kind {
        DivergenceKind::ReviewEscalated => Some("review_escalated"),
        DivergenceKind::ReviewShipped => Some("review_shipped"),
        DivergenceKind::ApprovedStillOpen => Some("approved_still_open"),
        DivergenceKind::AuthorTokenCeilingStopped => Some("author_token_ceiling"),
        DivergenceKind::RoundBudgetExhausted => Some("round_budget_exhausted"),
        DivergenceKind::ChangesRequestedNoRun
        | DivergenceKind::ReviewRequestedNoRun
        | DivergenceKind::ReviewTokenCeilingStopped
        | DivergenceKind::MergedTicketNotTerminal
        | DivergenceKind::ManagerDeferred => None,
    }
}

/// What to do with a stall signal once the active intervention and the stopped flag are known
/// (§7.2 deduplication).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueDecision {
    /// No active intervention and no stop: create a `queued` intervention covering `kinds`.
    Create { kinds: Vec<String> },
    /// An active, NOT-yet-launched intervention: union `add` into its `stall_kinds`.
    Merge { id: String, add: Vec<String> },
    /// An active intervention that has already launched (or there is nothing to do): drop the
    /// signal, to be re-detected once the intervention is terminal.
    Drop,
    /// The generation is stopped: never create another intervention for it (§7.2).
    Stopped,
    /// `review_authority: off` — the manager does not act and the human feed keeps the signal.
    ModeOff,
}

/// What [`Orchestrator::route_stalls_to_manager`] did with this sweep's stall signals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManagerRouting {
    /// Pull-request keys the manager ADOPTED: the caller drops their signals from the human feed.
    pub adopted: Vec<String>,
    /// Pull-request keys the manager could NOT adopt this sweep, each with the §10.2 human-feed
    /// sentence. Their signals stay on the feed, annotated with the manager's own wording.
    pub surfaced: Vec<(String, String)>,
}

/// The pure routing rule (§7.2). Kept separate from the store writes so the deduplication across
/// stall kinds is a table fixture rather than an integration guess.
pub fn plan_enqueue(
    active: Option<&ManagerInterventionRow>,
    stopped: bool,
    kinds: &[String],
) -> EnqueueDecision {
    let mut deduped: Vec<String> = Vec::new();
    for k in kinds {
        if !k.is_empty() && !deduped.contains(k) {
            deduped.push(k.clone());
        }
    }
    if deduped.is_empty() {
        return EnqueueDecision::Drop;
    }
    if stopped {
        return EnqueueDecision::Stopped;
    }
    match active {
        None => EnqueueDecision::Create { kinds: deduped },
        Some(row) => {
            // "Hasn't launched yet" is `queued` or `deferred`; anything else drops the signal.
            if row.state == MANAGER_INTERVENTION_QUEUED
                || row.state == MANAGER_INTERVENTION_DEFERRED
            {
                let add: Vec<String> = deduped
                    .into_iter()
                    .filter(|k| !row.stall_kinds.contains(k))
                    .collect();
                if add.is_empty() {
                    EnqueueDecision::Drop
                } else {
                    EnqueueDecision::Merge {
                        id: row.id.clone(),
                        add,
                    }
                }
            } else {
                EnqueueDecision::Drop
            }
        }
    }
}

/// Which §10.2 gate deferred a launch. A deferral is visible straight away, counts as "needs you"
/// after 30 minutes, and **never consumes a budget**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// A drain is armed. The intervention goes to `deferred`.
    Drain,
    /// A provider budget is exhausted. The intervention goes to `deferred`.
    Budget,
    /// The credential preflight failed. The intervention goes to `deferred`.
    Credentials,
}

impl DeferReason {
    /// The stable token for logs and the human feed.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeferReason::Drain => "drain",
            DeferReason::Budget => "budget",
            DeferReason::Credentials => "credentials",
        }
    }

    /// The human-feed sentence shown while deferred.
    pub fn human(&self) -> &'static str {
        match self {
            DeferReason::Drain => "manager deferred: drain",
            DeferReason::Budget => "manager deferred: budget",
            DeferReason::Credentials => "manager deferred: credentials",
        }
    }
}

/// The outcome of evaluating the §10.2 launch gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchGate {
    /// Every gate passed: reserve and dispatch.
    Proceed,
    /// A gate refused and the intervention goes to `deferred` (returns to `queued` once it clears).
    Deferred(DeferReason),
    /// A hold is applied, or the hold set is not yet known: the intervention is `superseded`.
    Superseded,
    /// The §4.7 self-test has not passed on the current CLI: **not launched** (stays `queued`).
    Unavailable,
    /// `manager.max_concurrent` is full: stays `queued`.
    AtCapacity,
}

/// The inputs to [`evaluate_launch_gates`], gathered from loop-owned state at the moment of a
/// launch. Pure data so the ordering and the fail-closed directions are testable without an
/// orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchGateEnv {
    /// The drain signal is armed.
    pub drain_active: bool,
    /// The `rhapsody:human` label set has been read this process (never true for an un-primed
    /// ledger, which is why an un-primed ledger supersedes).
    pub hold_known: bool,
    /// The pull request's origin ticket currently wears the hold.
    pub hold_active: bool,
    /// A provider budget is exhausted for this pull request.
    pub provider_budget_exhausted: bool,
    /// The credential preflight is not reporting dead.
    pub credential_healthy: bool,
    /// The §4.7 self-test passed on the installed CLI version.
    pub selftest_permitted: bool,
    /// `manager.max_concurrent` live manager runs already exist.
    pub at_capacity: bool,
}

/// Evaluate the §10.2 gates **in order**. Each gate is checked at its own entry point; the order
/// is drain, hold, budget, credentials, self-test, capacity — the order the design table lists, and
/// the order that refuses the cheapest, most-terminal condition first.
pub fn evaluate_launch_gates(env: LaunchGateEnv) -> LaunchGate {
    if env.drain_active {
        return LaunchGate::Deferred(DeferReason::Drain);
    }
    // Fail CLOSED while the hold set is unknown: an un-primed ledger is "no pass has looked", not
    // "no hold", and a decision taken without the hold is the one direction the design forbids.
    if !env.hold_known || env.hold_active {
        return LaunchGate::Superseded;
    }
    if env.provider_budget_exhausted {
        return LaunchGate::Deferred(DeferReason::Budget);
    }
    if !env.credential_healthy {
        return LaunchGate::Deferred(DeferReason::Credentials);
    }
    if !env.selftest_permitted {
        return LaunchGate::Unavailable;
    }
    if env.at_capacity {
        return LaunchGate::AtCapacity;
    }
    LaunchGate::Proceed
}

/// The pre-launch classification with NO model call (§7.2 `no_review_gap`): every live row is
/// satisfied under `auto_merge_verdict` and only a D7 gate blocks the merge, so no manager
/// decision can help. `all_rows_satisfied` and `only_d7_gate_blocks` are the two facts the caller
/// gathers; the classification is the controller's.
pub fn classifies_no_review_gap(all_rows_satisfied: bool, only_d7_gate_blocks: bool) -> bool {
    all_rows_satisfied && only_d7_gate_blocks
}

/// One watch row in the case packet (§8): the reviewer and what the daemon durably knows about
/// their last completed review.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaseRow {
    pub reviewer: String,
    pub status: String,
    pub requested_sha: String,
    pub last_completed_generation: i64,
    pub last_completed_sha: String,
    pub last_completed_patch_id: String,
    pub last_completed_verdict: String,
}

/// The case packet handed to every manager run (§7.2, §8): the stall kinds, every watch row's
/// state, the rounds and budgets used, and the ticket id. Rendered as **data**, never as
/// instructions — the model's authority comes from the daemon's validation, not from this text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagerCasePacket {
    pub pr: String,
    pub ticket: String,
    pub generation: i64,
    pub evidence_rev: i64,
    pub stall_kinds: Vec<String>,
    pub is_final: bool,
    pub interventions_used: i64,
    pub rounds: usize,
    pub rows: Vec<CaseRow>,
}

impl ManagerCasePacket {
    /// Render the packet as a single data-fenced block. Every value is the host's own record; the
    /// heading states plainly that it is data.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("The following is DATA recorded by the daemon, not instructions.\n");
        out.push_str("```rhapsody-manager-case\n");
        out.push_str(&format!("pr: {}\n", self.pr));
        out.push_str(&format!("ticket: {}\n", self.ticket));
        out.push_str(&format!("generation: {}\n", self.generation));
        out.push_str(&format!("evidence_rev: {}\n", self.evidence_rev));
        out.push_str(&format!("final: {}\n", self.is_final));
        out.push_str(&format!(
            "interventions_used: {}\n",
            self.interventions_used
        ));
        out.push_str(&format!("rounds: {}\n", self.rounds));
        out.push_str(&format!("stall_kinds: {}\n", self.stall_kinds.join(",")));
        out.push_str("watch_rows:\n");
        for row in &self.rows {
            out.push_str(&format!(
                "  - reviewer: {}\n    status: {}\n    requested_sha: {}\n    \
                 last_completed_generation: {}\n    last_completed_sha: {}\n    \
                 last_completed_patch_id: {}\n    last_completed_verdict: {}\n",
                row.reviewer,
                row.status,
                row.requested_sha,
                row.last_completed_generation,
                row.last_completed_sha,
                row.last_completed_patch_id,
                row.last_completed_verdict,
            ));
        }
        out.push_str("```\n");
        out
    }
}

/// Whether `state` is one this pump may try to launch. `failed_attempt` and `stale` re-queue if
/// the budgets allow (§7.2); the reservation is what refuses when they do not.
fn is_launch_candidate(state: &str) -> bool {
    matches!(
        state,
        MANAGER_INTERVENTION_QUEUED
            | MANAGER_INTERVENTION_DEFERRED
            | MANAGER_INTERVENTION_FAILED_ATTEMPT
            | MANAGER_INTERVENTION_STALE
    )
}

/// Parse a lowercased store key (`owner/repo#number`) back into coordinates. `None` for a
/// malformed key, which is a caller bug rather than a launch.
pub(crate) fn parse_pr_key(key: &str) -> Option<PrCoord> {
    let (repo_part, number) = key.rsplit_once('#')?;
    let (owner, repo) = repo_part.split_once('/')?;
    let number: i64 = number.parse().ok()?;
    if owner.is_empty() || repo.is_empty() || number <= 0 {
        return None;
    }
    Some(PrCoord::new(owner, repo, number))
}

/// The lowercased pull-request key a manager run's issue id names (`pr:owner/repo#n@manager` →
/// `owner/repo#n`), or `None` for any other key.
pub(crate) fn manager_pr_key(issue_id: &str) -> Option<String> {
    let rest = issue_id.strip_prefix(crate::review::REVIEW_KEY_PREFIX)?;
    let rest = rest.strip_suffix(crate::managerrun::MANAGER_KEY_SUFFIX)?;
    let coord = parse_pr_key(rest)?;
    Some(format!("{}/{}#{}", coord.owner, coord.repo, coord.number).to_ascii_lowercase())
}

/// Whether a manager run still owns the intervention: `launching` or `running` (§7.2).
fn is_in_flight(state: &str) -> bool {
    state == MANAGER_INTERVENTION_LAUNCHING || state == MANAGER_INTERVENTION_RUNNING
}

/// The pull request's current patch-id as the eligibility predicates need it, from the live rows'
/// recorded completions: the shared non-empty patch id when every completed row agrees, else empty.
/// Empty never matches a stored completion (fail closed in `completion_approved_at_current_patch`),
/// which is the cautious direction for a process with no `gh` read on this path.
pub(crate) fn current_patch_id(rows: &[ManagerReviewRow]) -> String {
    let mut seen: Option<String> = None;
    for r in rows {
        let Some(c) = r.completed.as_ref() else {
            continue;
        };
        if c.patch_id.is_empty() {
            continue;
        }
        match &seen {
            None => seen = Some(c.patch_id.clone()),
            Some(p) if p == &c.patch_id => {}
            Some(_) => return String::new(), // rows disagree: not one current patch
        }
    }
    seen.unwrap_or_default()
}

impl Orchestrator {
    /// Whether the manager acts (`act` or `advise`), i.e. the sweep routes stalls to it. `off`
    /// keeps today's byte-identical human feed.
    pub(crate) fn manager_routing_enabled(&self) -> bool {
        self.manager_review_authority() != ReviewAuthority::Off
    }

    pub(crate) fn manager_mode_token(&self) -> &'static str {
        if self.manager_review_authority() == ReviewAuthority::Act {
            MANAGER_MODE_ACT
        } else {
            MANAGER_MODE_ADVISE
        }
    }

    pub(crate) fn manager_max_runs_per_generation(&self) -> i64 {
        self.teams
            .as_ref()
            .map_or(12, |t| t.manager.max_runs_per_generation)
    }

    /// §9: whether the generation's SHADOW budget is already spent — the sum of `attempts` over
    /// every `advise` intervention for `(pr, generation)`, against the same 12-run bound. A read
    /// failure is fail-closed for CREATION only (treat as spent): a shadow run is not worth an
    /// unbounded retry loop against an unreadable store.
    pub(crate) fn manager_shadow_budget_spent(&self, pr: &str, generation: i64) -> bool {
        let Ok(rows) = self.store().load_manager_interventions() else {
            return true;
        };
        let used: i64 = rows
            .iter()
            .filter(|r| {
                r.pr.eq_ignore_ascii_case(pr)
                    && r.generation == generation
                    && r.mode == MANAGER_MODE_ADVISE
            })
            .map(|r| r.attempts)
            .sum();
        used >= self.manager_max_runs_per_generation()
    }

    /// §9: whether an `advise` proposal has ALREADY been recorded for `(pr, generation)` covering
    /// EVERY one of `kinds` — the "one shadow run per stall" rule. A proposal is terminal
    /// (`proposed`), so it is never the ACTIVE intervention and [`plan_enqueue`] would otherwise
    /// create a fresh one on every sweep a persistent stall is re-detected: one stall would buy a
    /// new shadow run, a new proposal and a new room post each sweep, up to the whole shadow budget.
    ///
    /// Scoped to the generation and to the stall KINDS, so a genuinely new stall on the same
    /// generation still earns its own proposal. A store read failure is fail-closed for CREATION
    /// (treat as already recorded): a shadow run is not worth an unbounded retry loop against an
    /// unreadable store, and creation would fail there anyway.
    pub(crate) fn manager_advise_stall_recorded(
        &self,
        pr: &str,
        generation: i64,
        kinds: &[String],
    ) -> bool {
        let Ok(rows) = self.store().load_manager_interventions() else {
            return true;
        };
        let mut covered: Vec<&str> = Vec::new();
        for row in rows.iter().filter(|r| {
            r.pr.eq_ignore_ascii_case(pr)
                && r.generation == generation
                && r.mode == MANAGER_MODE_ADVISE
                && manager_intervention_is_terminal(&r.state)
        }) {
            for kind in &row.stall_kinds {
                if !covered.contains(&kind.as_str()) {
                    covered.push(kind.as_str());
                }
            }
        }
        !kinds.is_empty() && kinds.iter().all(|k| covered.contains(&k.as_str()))
    }

    /// §9: under `act` the review watcher owns the adjudication boundary but the MANAGER owns the
    /// threshold stall, and — because the legacy turn is not invoked — there is no legacy
    /// adjudication to yield a `review_escalated` divergence for the reconciliation sweep to route.
    /// The watcher signals that stall HERE, through the same routing and dedup path the sweep uses,
    /// so exactly one intervention is created or merged and every budget/concurrency rule still
    /// applies. Deliberately silent when the manager is not authoritative (`off`/`advise`): those
    /// modes keep today's turn and route through the sweep's own divergences.
    pub(crate) fn signal_manager_threshold_stall(&self, pr: &str) {
        if self.manager_review_authority() != ReviewAuthority::Act {
            return;
        }
        self.route_stalls_to_manager(&[Divergence {
            pr: pr.to_string(),
            kind: DivergenceKind::ReviewEscalated,
            ticket: String::new(),
            reviewer: String::new(),
            stale_secs: 0,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: String::new(),
        }]);
    }

    pub(crate) fn manager_max_interventions(&self) -> i64 {
        self.teams
            .as_ref()
            .map_or(3, |t| t.manager.max_interventions)
    }

    pub(crate) fn manager_max_concurrent(&self) -> i64 {
        self.teams.as_ref().map_or(1, |t| t.manager.max_concurrent)
    }

    /// Hands the sweep's stall signals to the manager (§5.1, D3), and returns the pull-request keys
    /// it ADOPTED so the caller can drop those signals from the human feed — plus the ones it could
    /// not adopt (a deferred launch, or an unavailable manager), which STAY on the feed with the
    /// manager's own wording (§10.2).
    ///
    /// **The sweep still acts on nothing itself.** This only enqueues or merges an intervention; a
    /// launch happens on the control tick ([`Orchestrator::pump_manager_interventions`]). A signal
    /// for a PR whose generation is stopped is deliberately NOT adopted — the stop is a visible
    /// human-feed fact, not something to swallow.
    pub(crate) fn route_stalls_to_manager(&self, found: &[Divergence]) -> ManagerRouting {
        if !self.manager_routing_enabled() {
            return ManagerRouting::default();
        }
        let now = (self.now)().to_rfc3339_opts(SecondsFormat::Secs, true);
        let mode = self.manager_mode_token();

        // Group the mapped stall kinds by pull request, preserving first-seen order for stability.
        let mut order: Vec<String> = Vec::new();
        let mut by_pr: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for d in found {
            let Some(kind) = stall_kind_for(d.kind) else {
                continue;
            };
            let key = d.pr.to_ascii_lowercase();
            let entry = by_pr.entry(key.clone()).or_default();
            if entry.is_empty() {
                order.push(key);
            }
            if !entry.iter().any(|k| k == kind) {
                entry.push(kind.to_string());
            }
        }

        let mut routing = ManagerRouting::default();
        for pr in order {
            let kinds = by_pr.remove(&pr).unwrap_or_default();
            let budget = self.store().manager_budget(&pr).ok().flatten();
            let stopped = budget.as_ref().is_some_and(|b| b.is_stopped());
            let stop_reason = budget.filter(|b| b.is_stopped()).map(|b| b.stopped);
            let active = self.store().active_manager_intervention(&pr).ok().flatten();
            // Every state the pump would still try to launch — `queued`, `deferred`, a
            // `failed_attempt` or a `stale` row — is one a §10.2 gate can refuse, so the stall stays
            // on the human feed with the manager's wording. Only an intervention that has actually
            // LAUNCHED is the manager's to swallow, and its signal is re-detected once it is
            // terminal.
            let surfaceable = active
                .as_ref()
                .is_none_or(|r| is_launch_candidate(&r.state));
            let surface = if surfaceable {
                self.manager_surface_reason(self.manager_gate_env(&pr))
            } else {
                None
            };
            let decision = plan_enqueue(active.as_ref(), stopped, &kinds);
            match decision {
                EnqueueDecision::Stopped => {
                    // The generation is stopped: the stall stays on the human feed carrying the stop
                    // reason (§7.2), so an operator can tell "the manager gave up" from "the manager
                    // never looked". No log: this branch runs on every sweep while the generation is
                    // stopped, and the feed row IS the report.
                    if let Some(reason) = stop_reason {
                        routing.surfaced.push((pr, reason));
                    }
                }
                EnqueueDecision::ModeOff => {}
                EnqueueDecision::Drop => {
                    if let Some(reason) = surface {
                        routing.surfaced.push((pr, reason));
                    } else {
                        routing.adopted.push(pr);
                    }
                }
                EnqueueDecision::Merge { id, add } => {
                    if let Err(e) = self.store().merge_manager_stall_kinds(&id, &add) {
                        tracing::warn!(pr = %pr, id = %id, err = %e,
                            "manager: merging stall kinds into the intervention failed");
                    }
                    if let Some(reason) = surface {
                        routing.surfaced.push((pr, reason));
                    } else {
                        routing.adopted.push(pr);
                    }
                }
                EnqueueDecision::Create { kinds } => {
                    // Establish the generation row first: the intervention's budgets live on it, and
                    // a `manager_budget` read must answer for a fresh intervention too.
                    if let Err(e) = self.store().ensure_review_generation(&pr) {
                        tracing::warn!(pr = %pr, err = %e,
                            "manager: establishing the generation failed");
                        continue;
                    }
                    let generation = self
                        .store()
                        .review_bound(&pr)
                        .ok()
                        .flatten()
                        .map_or(1, |b| b.generation.max(1));
                    // §9: once the generation's SHADOW budget is spent, another advise intervention
                    // would only be created and immediately exhausted on every sweep. The stall
                    // stays on the human feed (as it does in `advise` anyway) and no proposal is
                    // funded — and, critically, the live generation is NOT stopped.
                    if mode == MANAGER_MODE_ADVISE
                        && self.manager_shadow_budget_spent(&pr, generation)
                    {
                        continue;
                    }
                    // §9: a proposal already recorded for this stall is the whole point — it is
                    // terminal, so it never shows up as the active intervention and would otherwise
                    // be re-created (with a fresh shadow run) on every sweep. Only a stall whose
                    // kinds no recorded proposal covers creates a new one.
                    if mode == MANAGER_MODE_ADVISE
                        && self.manager_advise_stall_recorded(&pr, generation, &kinds)
                    {
                        continue;
                    }
                    let row = ManagerInterventionRow {
                        id: new_intervention_id(&self.daemon_id, &now),
                        pr: pr.clone(),
                        generation,
                        stall_kinds: kinds,
                        mode: mode.to_string(),
                        state: MANAGER_INTERVENTION_QUEUED.to_string(),
                        ..ManagerInterventionRow::default()
                    };
                    match self.store().save_manager_intervention(row) {
                        Ok(()) => {
                            tracing::info!(pr = %pr, "manager: intervention enqueued");
                            if let Some(reason) = surface {
                                routing.surfaced.push((pr, reason));
                            } else {
                                routing.adopted.push(pr);
                            }
                        }
                        Err(e) => {
                            // A constraint failure means another writer created the active row
                            // first; the signal is dropped for this sweep and re-detected next.
                            tracing::warn!(pr = %pr, err = %e,
                                "manager: enqueuing an intervention failed");
                        }
                    }
                }
            }
        }
        if mode == MANAGER_MODE_ADVISE {
            // §9: in `advise` the manager is NOT authoritative — today's STUDIO-956 turn still is —
            // so a shadow proposal never ADOPTS a stall. The signal stays on the human feed exactly
            // as it would with `off`; only the proposal is added, to the room and the console.
            routing = ManagerRouting::default();
        }
        routing
    }

    /// The §10.2 human-feed sentence for a pull request whose manager launch is refused by a
    /// deferral (drain/budget/credentials) or by the §4.7 CLI self-test, or `None` when the launch
    /// is not refused by one of those gates.
    pub(crate) fn manager_surface_reason(&self, env: LaunchGateEnv) -> Option<String> {
        match evaluate_launch_gates(env) {
            LaunchGate::Deferred(reason) => Some(reason.human().to_string()),
            LaunchGate::Unavailable => Some("manager unavailable: CLI contract".to_string()),
            _ => None,
        }
    }

    /// The control tick's manager pass: recover dead leases, then try to launch every candidate
    /// whose gates pass, AT ITS OWN ENTRY POINT (§10.2). Called from `on_tick` beside the other
    /// sweeps.
    pub(crate) fn pump_manager_interventions(&mut self) {
        if !self.manager_routing_enabled() {
            return;
        }
        let now_dt = (self.now)();
        let now = now_dt.to_rfc3339_opts(SecondsFormat::Secs, true);

        // §7.5 recovery: a `decided`/`validated` row re-runs its validation WITHOUT the model, using
        // M3. Run before the candidate load so a row that revalidates to `stale` is re-queued in the
        // same tick, and before the lease sweep so the ordering matches the design (both are pure
        // control-task work).
        self.revalidate_saved_manager_decisions();

        // §7.6/§7.7 (STUDIO-1016): begin applying validated decisions, recover `applying` rows, and
        // complete or time out `awaiting_effect` rows. Local records are written here; the external
        // effects run off-loop through the applier.
        self.pump_manager_applying();

        // §7.5 recovery: any `launching`/`running` lease from another boot, or one that has
        // expired, becomes a `failed_attempt`. Fail CLOSED on a store read error — nothing is
        // launched against a store we cannot read.
        match self.store().expire_manager_leases(&self.daemon_id, &now) {
            Ok(recovered) => {
                for row in recovered {
                    tracing::warn!(pr = %row.pr, id = %row.id,
                        "manager: a dead lease was recovered as a failed attempt");
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "manager: recovering dead leases failed; not launching");
                return;
            }
        }

        let all = match self.store().load_manager_interventions() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "manager: the interventions could not be read");
                return;
            }
        };
        let running = all
            .iter()
            .filter(|r| {
                r.state == MANAGER_INTERVENTION_LAUNCHING || r.state == MANAGER_INTERVENTION_RUNNING
            })
            .count();
        let mut slots = (self.manager_max_concurrent().max(0) as usize).saturating_sub(running);

        for row in all.iter().filter(|r| is_launch_candidate(&r.state)) {
            if slots == 0 {
                break; // `manager.max_concurrent` full: the rest stay queued
            }
            let env = self.manager_gate_env(&row.pr);
            match evaluate_launch_gates(env) {
                LaunchGate::Deferred(reason) => {
                    if row.state != MANAGER_INTERVENTION_DEFERRED {
                        if let Err(e) = self
                            .store()
                            .set_manager_intervention_state(&row.id, MANAGER_INTERVENTION_DEFERRED)
                        {
                            tracing::warn!(pr = %row.pr, err = %e,
                                "manager: marking the intervention deferred failed");
                        }
                        tracing::info!(pr = %row.pr, reason = reason.human(),
                            "manager: launch deferred");
                    }
                    continue;
                }
                LaunchGate::Superseded => {
                    if let Err(e) = self
                        .store()
                        .set_manager_intervention_state(&row.id, MANAGER_INTERVENTION_SUPERSEDED)
                    {
                        tracing::warn!(pr = %row.pr, err = %e,
                            "manager: marking the intervention superseded failed");
                    }
                    continue;
                }
                LaunchGate::Unavailable => {
                    // Stays queued; the self-test watcher re-runs and the next tick retries.
                    continue;
                }
                LaunchGate::AtCapacity => {
                    continue; // stays queued
                }
                LaunchGate::Proceed => {}
            }

            let Some(run) = self.manager_run_for(&row.pr) else {
                tracing::warn!(pr = %row.pr,
                    "manager: no configured project owns the pull request's repo; not launched");
                continue;
            };
            let key = run.key();

            // The pre-launch classification with no model call (§7.2): ONLY a stall that is purely
            // "approved still open" qualifies — an intervention that merged a `review_escalated`
            // into it still needs the manager. The "every live row is satisfied" fact is read from
            // the rows, not inferred from the stall kind.
            // §9: `no_review_gap` STOPs the live generation, so it can only be classified for an
            // `act` intervention. In `advise` the shadow run still happens — a proposal is worth
            // recording even when a merge gate is what blocks — and nothing live is stopped.
            let only_approved_still_open = !row.stall_kinds.is_empty()
                && row.stall_kinds.iter().all(|k| k == "approved_still_open");
            if row.mode != MANAGER_MODE_ADVISE
                && classifies_no_review_gap(
                    self.manager_all_rows_satisfied(&row.pr),
                    only_approved_still_open,
                )
            {
                // ONE transaction: the intervention ends `no_review_gap` AND the generation is
                // stopped, so a crash between the two writes can never leave a terminal row with a
                // live generation for the next sweep to recreate (§7.2, §15.4).
                if let Err(e) = self.store().stop_manager_intervention(
                    &row.id,
                    MANAGER_INTERVENTION_NO_REVIEW_GAP,
                    "no review gap: every reviewer row is satisfied; a merge gate blocks",
                ) {
                    tracing::warn!(pr = %row.pr, err = %e,
                        "manager: recording no_review_gap failed");
                    continue;
                }
                tracing::warn!(pr = %row.pr,
                    "manager: no review gap; the stall goes to the human feed");
                continue;
            }

            // The phase hint is a case-packet hint only; the authoritative classification is made
            // at activation (§7.8).
            let coord = parse_pr_key(&row.pr);
            let phase = if coord.as_ref().is_some_and(|c| {
                self.adjudication_threshold()
                    .is_some_and(|t| self.rounds_used(c) >= t)
            }) {
                MANAGER_PHASE_POST_THRESHOLD
            } else {
                MANAGER_PHASE_PRE_THRESHOLD
            };
            if let Err(e) = self
                .store()
                .set_manager_intervention_phase_hint(&row.id, phase)
            {
                tracing::warn!(pr = %row.pr, err = %e, "manager: writing the phase hint failed");
            }

            let lease = lease_expiry(now_dt, self.manager_run_timeout_ms());
            let reservation = self.store().reserve_manager_run(
                &row.id,
                &self.daemon_id,
                &lease,
                self.manager_max_runs_per_generation(),
                3,
                self.manager_max_interventions(),
            );
            match reservation {
                Ok(ManagerReservation::Reserved) => {}
                Ok(ManagerReservation::Exhausted) => {
                    if row.mode == MANAGER_MODE_ADVISE {
                        // §9: the shadow budget is spent; only this proposal ends. The live
                        // generation is deliberately untouched — a shadow run can never stop it.
                        tracing::warn!(pr = %row.pr,
                            "manager: the shadow budget is spent; the proposal ends exhausted");
                    } else {
                        tracing::warn!(pr = %row.pr,
                            "manager: the run budget is spent; the generation is stopped");
                    }
                    continue;
                }
                Ok(ManagerReservation::Absent) => continue,
                Err(e) => {
                    tracing::warn!(pr = %row.pr, err = %e,
                        "manager: reserving a run failed; not launched");
                    continue;
                }
            }

            // Render the packet from the row AS RESERVED: `final` is set by the reservation (§7.3),
            // so rendering it from the pre-reservation snapshot would hand the run `final: false`
            // on the generation's LAST allocation.
            let mut run = run;
            let updated = self
                .store()
                .manager_intervention(&row.id)
                .ok()
                .flatten()
                .unwrap_or_else(|| row.clone());
            run.case_packet = self.manager_case_packet(&updated).render();

            match self.dispatch_manager(run) {
                ManagerDispatchOutcome::Dispatched => {
                    slots = slots.saturating_sub(1);
                    // §7.2: the run is live, so the intervention is `running` and still holds its
                    // lease. `on_manager_exit` settles it from here.
                    let run_id = self.running.get(&key).map(|re| re.run_id);
                    if let Err(e) = self
                        .store()
                        .mark_manager_intervention_running(&row.id, run_id)
                    {
                        tracing::warn!(pr = %row.pr, err = %e,
                            "manager: marking the intervention running failed");
                    }
                }
                other => {
                    // The lease is written and charged already; a refused dispatch leaves the
                    // intervention `launching` and recovery turns it into a `failed_attempt`. This
                    // should be unreachable because every gate was checked above.
                    tracing::warn!(pr = %row.pr, outcome = ?other,
                        "manager: a reserved run was not dispatched");
                }
            }
        }
    }

    /// The `manager.run_timeout_ms` default when Teams is absent.
    pub(crate) fn manager_run_timeout_ms(&self) -> i64 {
        self.teams
            .as_ref()
            .map_or(1_800_000, |t| t.manager.run_timeout_ms)
    }

    /// Gather the §10.2 gate inputs for `pr`. Pure, synchronous reads of loop-owned state.
    pub(crate) fn manager_gate_env(&self, pr: &str) -> LaunchGateEnv {
        let (labelled, primed) = self.human_holds.labelled_and_primed();
        let hold_active = self.manager_pr_held(pr, &labelled);
        let ticket = self.manager_pr_ticket(pr).unwrap_or_default();
        LaunchGateEnv {
            drain_active: self.drain.is_draining(),
            hold_known: primed,
            hold_active,
            provider_budget_exhausted: !ticket.is_empty()
                && self.budget_hold_for(pr, &ticket).is_some(),
            credential_healthy: !self.credential_probe_dead(),
            selftest_permitted: self.manager_launch_permitted().is_ok(),
            at_capacity: false,
        }
    }

    /// The origin ticket of the first live watch row for `pr`, if any (used by the hold and budget
    /// gates). `None` when the watch set cannot be read or holds no row for the pull request.
    pub(crate) fn manager_pr_ticket(&self, pr: &str) -> Option<String> {
        let rows = self.store().load_live_review_watch().ok()?;
        rows.into_iter()
            .find(|r| {
                format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase()
                    == pr
            })
            .and_then(|r| crate::reviewdone::origin_ticket(&r.introduced_by).map(str::to_string))
    }

    /// Whether any live watch row for `pr` has an origin ticket wearing the hold. Fails CLOSED on a
    /// store read error (`true`), because an unreadable watch set cannot show that the pull request
    /// is free of a hold — the direction §10.2 requires.
    pub(crate) fn manager_pr_held(
        &self,
        pr: &str,
        labelled: &std::collections::HashSet<String>,
    ) -> bool {
        let Ok(rows) = self.store().load_live_review_watch() else {
            return true;
        };
        rows.iter().any(|r| {
            format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase() == pr
                && crate::reviewdone::origin_ticket(&r.introduced_by)
                    .is_some_and(|t| labelled.contains(&t.to_ascii_lowercase()))
        })
    }

    /// Build the trusted [`ManagerRun`] for `pr`: the coordinates parsed from the key, and the
    /// repository URL taken from a CONFIGURED project that owns them — never from untrusted text.
    pub(crate) fn manager_run_for(&self, pr: &str) -> Option<ManagerRun> {
        let coord = parse_pr_key(pr)?;
        // A URL-shaped candidate, because `same_repository` parses hosts — a bare `owner/repo`
        // would fail to parse and fall back to raw equality.
        let candidate = format!("https://github.com/{}/{}", coord.owner, coord.repo);
        let repo_url = self
            .eff
            .as_ref()?
            .projects
            .iter()
            .find(|p| !p.disabled && crate::reviewintro::same_repository(&p.repo, &candidate))
            .map(|p| p.repo.clone())?;
        Some(ManagerRun {
            owner: coord.owner,
            repo: coord.repo,
            number: coord.number,
            repo_url,
            team_id: String::new(),
            case_packet: String::new(),
        })
    }

    /// The case packet handed to `row`'s run (§7.2, §8): the stall kinds, every live watch row's
    /// durable state, the rounds and interventions used, and the ticket id. Assembled from the
    /// host's own records and rendered as DATA, never as instructions.
    pub(crate) fn manager_case_packet(&self, row: &ManagerInterventionRow) -> ManagerCasePacket {
        let pr = row.pr.clone();
        let bound = self.store().review_bound(&pr).ok().flatten();
        let evidence_rev = bound.as_ref().map_or(0, |b| b.evidence_rev);
        let interventions_used = self
            .store()
            .manager_budget(&pr)
            .ok()
            .flatten()
            .map_or(0, |b| b.interventions_applied);
        let rounds = parse_pr_key(&pr).map_or(0, |c| self.rounds_used(&c));
        let ticket = self.manager_pr_ticket(&pr).unwrap_or_default();

        let mut rows = Vec::new();
        if let Ok(watch) = self.store().load_live_review_watch() {
            for r in watch {
                if format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase()
                    != pr
                {
                    continue;
                }
                let completed = self.store().review_completed(&r.key).ok().flatten();
                rows.push(CaseRow {
                    reviewer: r.key.reviewer.clone(),
                    status: r.status.clone(),
                    requested_sha: r.requested_sha.clone(),
                    last_completed_generation: completed.as_ref().map_or(0, |c| c.generation),
                    last_completed_sha: completed
                        .as_ref()
                        .map(|c| c.sha.clone())
                        .unwrap_or_default(),
                    last_completed_patch_id: completed
                        .as_ref()
                        .map(|c| c.patch_id.clone())
                        .unwrap_or_default(),
                    last_completed_verdict: completed
                        .as_ref()
                        .map(|c| c.verdict.clone())
                        .unwrap_or_default(),
                });
            }
        }

        ManagerCasePacket {
            pr,
            ticket,
            generation: row.generation,
            evidence_rev,
            stall_kinds: row.stall_kinds.clone(),
            is_final: row.is_final,
            interventions_used,
            rounds,
            rows,
        }
    }

    // --- the run's exit: parse the decision, settle the state machine (§7.2, §8.2) --------------

    /// The exit path of a manager run (§7.2, §7.5). A run that produced a valid
    /// `rhapsody-manager-decision` block moves the intervention `decided` (storing the decision),
    /// then revalidates it WITHOUT the model via M3 and advances to `validated`, `stale`,
    /// `superseded` or `complete` (`proposed` in `advise`). Anything else — a failed run, no block,
    /// an invalid block, a refused final decision — is a `failed_attempt`, which the pump re-queues
    /// while the budgets allow. Called after [`Orchestrator::on_manager_exit`]'s run bookkeeping.
    pub(crate) fn settle_manager_intervention(
        &mut self,
        issue_id: &str,
        e: &crate::retry::EvWorkerExit,
    ) {
        if !self.manager_routing_enabled() {
            return;
        }
        let Some(pr) = manager_pr_key(issue_id) else {
            return;
        };
        let Some(row) = self.store().active_manager_intervention(&pr).ok().flatten() else {
            // A clear may have superseded it while the run was in flight; nothing to settle.
            return;
        };
        if !is_in_flight(&row.state) {
            return;
        }
        if e.failed {
            self.record_manager_failed_attempt(&row, "manager run failed");
            return;
        }
        // A `launching`/`running` row the exit names but that produced no text is a failed attempt,
        // exactly as a lease expiry is.
        let Some(text) = e.manager_text.as_deref() else {
            self.record_manager_failed_attempt(&row, "manager run produced no result text");
            return;
        };
        let known = self.manager_known_findings(&pr);
        match managerdecision::parse_decision(text, &known) {
            Ok(decision) => self.advance_manager_decision(&row, &decision, text),
            Err(err) => {
                self.record_manager_failed_attempt(
                    &row,
                    &format!("invalid manager decision: {err:?}"),
                );
            }
        }
    }

    /// Advance a PARSED decision through the deterministic checks and the `decided` state (§7.2).
    pub(crate) fn advance_manager_decision(
        &mut self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
        text: &str,
    ) {
        // §7.3: a final intervention may only APPROVE (with an optional dismiss) or ESCALATE. A
        // refusal is a failed attempt, so the run is charged but nothing is applied.
        if let Err(err) = managerdecision::validate_final(decision, row.is_final) {
            self.record_manager_failed_attempt(row, &format!("refused: {err:?}"));
            return;
        }
        // §6.2 deterministic preconditions (a `RERUN_REVIEW` needs at least one eligible row). No
        // model call.
        let rows = self.manager_review_rows(&row.pr);
        let patch_id = current_patch_id(&rows);
        let precondition = managerdecision::PreconditionInputs {
            eligible_rows: managerdecision::eligible_rows(&rows, row.generation, &patch_id).len(),
        };
        if let Err(err) = managerdecision::preconditions(decision, &precondition) {
            self.record_manager_failed_attempt(row, &format!("refused: {err:?}"));
            return;
        }
        // Store the decision and move to `decided`, clearing the lease. The stored body is the
        // fenced block itself, so M9's activation can re-parse it.
        let body =
            crate::reviewfindings::fenced_blocks(text, managerdecision::MANAGER_DECISION_TAG)
                .into_iter()
                .next()
                .unwrap_or_default();
        if let Err(e) = self.store().record_manager_decision(
            &row.id,
            &body,
            &decision.head,
            decision.evidence_rev,
        ) {
            tracing::warn!(pr = %row.pr, err = %e, "manager: recording the decision failed");
            return;
        }
        // Re-read so the state writes below name the row as stored.
        let Some(saved) = self.store().manager_intervention(&row.id).ok().flatten() else {
            return;
        };
        if saved.mode == MANAGER_MODE_ADVISE {
            // §9: `advise` records the decision and never applies it. Terminal.
            self.set_manager_state(&saved, MANAGER_INTERVENTION_PROPOSED);
            // Shadow output goes to the room and the console, NEVER to the PR (§9). This is the
            // only thing an advise run produces.
            self.record_manager_proposal(&saved, decision);
            return;
        }
        match self.revalidate_manager_decision(&saved, decision) {
            Revalidation::StillValid => {
                self.set_manager_state(&saved, MANAGER_INTERVENTION_VALIDATED)
            }
            Revalidation::Complete => self.set_manager_state(&saved, MANAGER_INTERVENTION_COMPLETE),
            Revalidation::Stale => {
                // §7.2: revalidation needs a new run; the pump re-queues it while the budgets allow.
                self.set_manager_state(&saved, MANAGER_INTERVENTION_STALE)
            }
            Revalidation::Superseded => {
                self.set_manager_state(&saved, MANAGER_INTERVENTION_SUPERSEDED)
            }
        }
    }

    /// Re-runs §8.1–§8.2's deterministic checks against current loop-owned state, with NO model call
    /// (§7.5, §8.2). This is the same M3 [`managerdecision::revalidate`] the activation transaction
    /// (M9, §7.7) re-runs in full; M8 uses it to decide between `validated` and `stale`.
    ///
    /// `review_completed_since` and `finding_set_unchanged` (§8.2's APPROVE rule) are computed from
    /// real state on EVERY call, exactly as the activation transaction needs them: the evidence
    /// revision is the durable detector of a completion (a completed review is an evidence input,
    /// §5.2, so any completion moves it), and the open-blocking set is compared with the revisions
    /// the decision itself dismissed. This is the same computation M8 uses to decide `validated` vs
    /// `stale`, so a decision cannot be `validated` by one rule and activated by another.
    pub(crate) fn revalidate_manager_decision(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
    ) -> Revalidation {
        let pr = &row.pr;
        let bound = self.store().review_bound(pr).ok().flatten();
        let after_generation = bound.as_ref().map_or(row.generation, |b| b.generation);
        let after_evidence_rev = bound.as_ref().map_or(0, |b| b.evidence_rev);
        let (labelled, hold_known) = self.human_holds.labelled_and_primed();
        let hold_applied = self.manager_pr_held(pr, &labelled);
        let manager_enabled = self.teams.as_ref().is_some_and(|t| t.enabled)
            && self.manager_review_authority() != ReviewAuthority::Off;
        let rows = self.manager_review_rows(pr);
        let patch_id = current_patch_id(&rows);
        let eligible = managerdecision::eligible_rows(&rows, after_generation, &patch_id).len();
        let all_rows_approved = !rows.is_empty()
            && managerdecision::live_reviewer_rows(&rows).iter().all(|r| {
                crate::reviewevidence::completion_approved_at_current_patch(
                    r.completed.as_ref(),
                    after_generation,
                    &patch_id,
                )
            });
        let route_fix_still_open = self.manager_route_fix_still_open(pr, decision);
        let approval_still_eligible =
            self.manager_approval_still_eligible(row, decision, &rows, after_generation, &patch_id);
        // §8.2's APPROVE inputs, evaluated NOW. A completion is an evidence input, so a moved
        // evidence revision is the durable signal that one happened since the decision; the
        // finding set is compared with the revisions the decision names in `dismiss` (for an
        // APPROVE, §6.4 condition 4 makes that the whole open-blocking set). Conservative by
        // construction: an evidence move refuses the APPROVE, the safe direction.
        let review_completed_since = after_evidence_rev != row.decision_evidence_rev;
        let finding_set_unchanged = if matches!(decision.kind, DecisionKind::Approve) {
            self.manager_finding_set_unchanged(pr, decision)
        } else {
            true // unused for a non-APPROVE decision
        };
        // §8.1: the decision is bound to (generation, evidence_rev, head). A moved head that is not
        // the decision's is a moved patch until proven otherwise.
        let patch_id_unchanged = self.manager_head_unchanged(pr, &decision.head);
        let pr_open = self
            .store()
            .load_live_review_watch()
            .map(|w| {
                w.iter().any(|r| {
                    r.open
                        && format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number)
                            .to_ascii_lowercase()
                            == *pr
                })
            })
            .unwrap_or(false);
        let phase_at_launch = if row.phase_hint == MANAGER_PHASE_POST_THRESHOLD {
            ThresholdPhase::PostThreshold
        } else {
            ThresholdPhase::PreThreshold
        };
        let answered_exchanges = parse_pr_key(pr).map_or(0, |c| self.rounds_used(&c) as i64);
        let adjudicate_after_rounds = self.adjudication_threshold().map_or(0, |t| t as i64);
        let interventions_applied = self
            .store()
            .manager_budget(pr)
            .ok()
            .flatten()
            .map_or(0, |b| b.interventions_applied);

        managerdecision::revalidate(&RevalidationInputs {
            decision,
            before_generation: row.generation,
            after_generation,
            authority_act: self.manager_review_authority() == ReviewAuthority::Act,
            pr_open,
            hold_known,
            hold_applied,
            manager_enabled,
            before_evidence_rev: row.decision_evidence_rev,
            after_evidence_rev,
            patch_id_unchanged,
            eligible_rows: eligible,
            all_rows_approved,
            route_fix_still_open,
            review_completed_since,
            finding_set_unchanged,
            approval_still_eligible,
            phase_at_launch,
            answered_exchanges,
            adjudicate_after_rounds,
            interventions_applied,
            max_interventions: self.manager_max_interventions(),
            final_intervention: row.is_final,
        })
    }

    /// §7.5: every `decided`/`validated` intervention re-runs its saved decision's validation with
    /// NO model call. A `decided` row that revalidates to `validated` advances; one that no longer
    /// holds becomes `stale` (re-queued by the pump while the budgets allow) or `superseded`/`complete`.
    /// `advise` decisions are recorded, never applied, and end `proposed`.
    pub(crate) fn revalidate_saved_manager_decisions(&self) {
        let Ok(rows) = self.store().load_manager_interventions() else {
            return;
        };
        for row in rows {
            if row.state != MANAGER_INTERVENTION_DECIDED
                && row.state != MANAGER_INTERVENTION_VALIDATED
            {
                continue;
            }
            if row.decision_json.is_empty() {
                continue;
            }
            if row.mode == MANAGER_MODE_ADVISE {
                self.set_manager_state(&row, MANAGER_INTERVENTION_PROPOSED);
                continue;
            }
            // Re-parse the stored block (a fenced body) so revalidation names the same decision.
            // `parse_stored_decision` skips the §6.1 open-status rule the block already satisfied
            // when it was stored, so a `route.fix` revision the author has since resolved revalidates
            // `stale` (§8.2) instead of failing to parse and pinning the row `validated` forever.
            let wrapped = format!(
                "```{}\n{}\n```",
                managerdecision::MANAGER_DECISION_TAG,
                row.decision_json
            );
            let known = self.manager_known_findings(&row.pr);
            let Ok(decision) = managerdecision::parse_stored_decision(&wrapped, &known) else {
                continue; // a stored decision that no longer parses is left where it is
            };
            match self.revalidate_manager_decision(&row, &decision) {
                Revalidation::StillValid => {
                    if row.state != MANAGER_INTERVENTION_VALIDATED {
                        self.set_manager_state(&row, MANAGER_INTERVENTION_VALIDATED);
                    }
                }
                Revalidation::Complete => {
                    self.set_manager_state(&row, MANAGER_INTERVENTION_COMPLETE)
                }
                Revalidation::Stale => self.set_manager_state(&row, MANAGER_INTERVENTION_STALE),
                Revalidation::Superseded => {
                    self.set_manager_state(&row, MANAGER_INTERVENTION_SUPERSEDED)
                }
            }
        }
    }

    /// §9: record an `advise` proposal's shadow output. It is posted to the ROOM, which cannot
    /// dispatch anything by design, and is visible on the console through the row's `proposed` state.
    /// **It never reaches the pull request** — no comment, no summon, no ticket move, no approval,
    /// and no live counter is touched (the run was charged to the shadow budget). A memory
    /// observation of kind *proposal* is written for later calibration; the maintainer's later
    /// action on the PR is recorded as the row's outcome by the PR-state watcher (§11.1).
    pub(crate) fn record_manager_proposal(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
    ) {
        let prose = manager_proposal_prose(decision);
        let line = crate::managerapply::strip_summon_tokens(&format!(
            "manager proposal on {} (advise mode; PROPOSED, not applied): {prose}",
            row.pr
        ));
        if let Some(room) = self.teams_room.as_ref()
            && let Err(e) = room.append(
                &Message::room(
                    crate::reviewadjudicate::MANAGER_IDENTITY,
                    (self.now)(),
                    line,
                )
                .with_refs([row.pr.clone()]),
            )
        {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager: the proposal room post failed");
        }
        self.write_manager_proposal_memory(row, &prose);
    }

    /// §9/§11.3: the proposal's memory observation, best effort. `memory_state` records whether it
    /// landed; a failure never fails the proposal.
    fn write_manager_proposal_memory(&self, row: &ManagerInterventionRow, prose: &str) {
        let Some(bank) = self.teams_bank.as_ref() else {
            return;
        };
        let prefix = self
            .teams
            .as_ref()
            .map(|t| t.memory.bank_prefix.clone())
            .unwrap_or_else(|| "agent-".to_string());
        let bank_id = rhapsody_config::manager::manager_bank_id(&prefix);
        let record = rhapsody_config::memory::Record {
            identity: rhapsody_config::room::MANAGER_IDENTITY.to_string(),
            document_id: format!("proposal-{}", row.id),
            ticket: self.manager_pr_ticket(&row.pr).unwrap_or_default(),
            commit_sha: String::new(),
            pr: row.pr.clone(),
            run_id: row.run_id.map(|r| r.to_string()).unwrap_or_default(),
            at: (self.now)(),
            content: format!("Proposal (advise mode, not applied) on {}: {prose}", row.pr),
        };
        let state = match bank.retain_shared(&bank_id, &record) {
            Ok(_) => "done",
            Err(e) => {
                tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                    "manager: the proposal memory observation failed; memory_state stays pending");
                "pending"
            }
        };
        if let Err(e) = self.store().set_manager_memory_state(&row.id, state) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager: writing the proposal's memory state failed");
        }
    }

    /// §11.1: when the PR-state watcher observes a proposal's pull request merge or close, record it
    /// as that proposal's outcome (set once, never overwritten). This is the calibration evidence
    /// for switching to `act`: what the maintainer actually did with the PR the manager proposed on.
    pub(crate) fn record_manager_proposal_outcomes(&self, pr: &str, outcome: &str) {
        let Ok(rows) = self.store().load_manager_interventions() else {
            return;
        };
        let now = (self.now)().to_rfc3339_opts(SecondsFormat::Secs, true);
        let key = pr.to_ascii_lowercase();
        for row in rows {
            if row.mode != MANAGER_MODE_ADVISE
                || row.state != MANAGER_INTERVENTION_PROPOSED
                || !row.outcome.is_empty()
                || row.pr.to_ascii_lowercase() != key
            {
                continue;
            }
            if let Err(e) = self.store().record_manager_outcome(&row.id, outcome, &now) {
                tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                    "manager: recording the proposal outcome failed");
            }
        }
    }

    /// §6.2/§8.2: every `route.fix` revision the decision names is still an open finding.
    pub(crate) fn manager_route_fix_still_open(
        &self,
        pr: &str,
        decision: &ManagerDecision,
    ) -> bool {
        let DecisionKind::RouteToAuthor { fix, .. } = &decision.kind else {
            return true; // not a ROUTE_TO_AUTHOR decision; the input is unused for it
        };
        let Ok(findings) = self.store().load_review_findings(pr) else {
            return false; // fail closed
        };
        fix.iter().all(|want| {
            findings.iter().any(|f| {
                f.finding_id == want.finding
                    && f.revision == want.revision
                    && f.status == REVIEW_FINDING_OPEN
            })
        })
    }

    /// §8.2 `finding_set_unchanged` for an APPROVE: the CURRENT open blocking finding revisions are
    /// exactly the ones the decision itself dismissed. For an APPROVE, §6.4 condition 4 makes that
    /// dismissed set the whole open-blocking set the decision was made against, so a new finding (or
    /// a resolved one) changes it and the decision is `stale`. A store read that fails answers
    /// `false` — fail closed against refusing to apply an approval whose finding set cannot be read.
    pub(crate) fn manager_finding_set_unchanged(
        &self,
        pr: &str,
        decision: &ManagerDecision,
    ) -> bool {
        let Ok(current) = self.store().open_blocking_findings(pr) else {
            return false; // fail closed
        };
        let mut now: Vec<(String, i64)> = current
            .iter()
            .map(|f| (f.finding_id.clone(), f.revision))
            .collect();
        let mut then: Vec<(String, i64)> = decision
            .dismiss
            .iter()
            .map(|d| (d.finding.finding.clone(), d.finding.revision))
            .collect();
        now.sort();
        then.sort();
        now == then
    }

    /// §6.4 re-evaluated now for an APPROVE decision's `approval_still_eligible` input.
    pub(crate) fn manager_approval_still_eligible(
        &self,
        _row: &ManagerInterventionRow,
        decision: &ManagerDecision,
        rows: &[ManagerReviewRow],
        generation: i64,
        patch_id: &str,
    ) -> bool {
        if !matches!(decision.kind, DecisionKind::Approve) {
            return true; // unused for a non-APPROVE decision
        }
        let open_blocking = self
            .store()
            .open_blocking_findings(&_row.pr)
            .unwrap_or_default()
            .into_iter()
            .map(|f| FindingRef {
                finding: f.finding_id,
                revision: f.revision,
            })
            .collect::<Vec<_>>();
        let dismissed = decision
            .dismiss
            .iter()
            .map(|d| d.finding.clone())
            .collect::<Vec<_>>();
        let required = self
            .teams
            .as_ref()
            .map_or_else(Vec::new, |t| t.review.required.clone());
        let effective_reviewers = self
            .teams
            .as_ref()
            .map_or(0, |t| t.review.effective_reviewers());
        let (limit, answered) = {
            let limit = self.adjudication_threshold().map_or(0, |t| t as i64);
            let answered = parse_pr_key(&_row.pr).map_or(0, |c| self.rounds_used(&c) as i64);
            (limit, answered)
        };
        managerdecision::approval_eligibility(&ApprovalInputs {
            rows,
            required: &required,
            effective_reviewers,
            generation,
            current_patch_id: patch_id,
            threshold_reached: answered >= limit,
            final_intervention: _row.is_final,
            open_blocking: &open_blocking,
            dismissed: &dismissed,
        })
        .is_ok()
    }

    /// §8.2: whether the decision's head is still one of the pull request's observed heads. With no
    /// observed head the answer is `true` — a head this process cannot read is not evidence it moved.
    pub(crate) fn manager_head_unchanged(&self, pr: &str, head: &str) -> bool {
        if head.is_empty() {
            return true;
        }
        let Ok(rows) = self.store().load_live_review_watch() else {
            return false; // fail closed on an unreadable watch set
        };
        let mut observed = Vec::new();
        for r in rows {
            if format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase() != pr
            {
                continue;
            }
            if !r.requested_sha.is_empty() {
                observed.push(r.requested_sha.clone());
            }
            if !r.last_reviewed_sha.is_empty() {
                observed.push(r.last_reviewed_sha.clone());
            }
        }
        observed.is_empty() || observed.iter().any(|h| h == head)
    }

    /// §7.2's `no_review_gap` fact read from the rows: every live reviewer row is approved at the
    /// current generation and patch. An empty live set is NOT satisfied (fail closed — a stall with
    /// no reviewer row is not "every row is satisfied").
    pub(crate) fn manager_all_rows_satisfied(&self, pr: &str) -> bool {
        let rows = self.manager_review_rows(pr);
        let live = managerdecision::live_reviewer_rows(&rows);
        if live.is_empty() {
            return false;
        }
        let patch_id = current_patch_id(&rows);
        let generation = self
            .store()
            .review_bound(pr)
            .ok()
            .flatten()
            .map_or(0, |b| b.generation);
        live.iter().all(|r| {
            crate::reviewevidence::completion_approved_at_current_patch(
                r.completed.as_ref(),
                generation,
                &patch_id,
            )
        })
    }

    /// The daemon's finding ledger for `pr`, as M3's parser needs it (§6.1).
    pub(crate) fn manager_known_findings(&self, pr: &str) -> Vec<KnownFinding> {
        self.store()
            .load_review_findings(pr)
            .unwrap_or_default()
            .into_iter()
            .map(|f| KnownFinding {
                finding_id: f.finding_id,
                revision: f.revision,
                status: f.status,
                blocking: f.blocking,
            })
            .collect()
    }

    /// The pull request's live watch rows as approval/precondition eligibility sees them (§6.2,
    /// §6.4). `diff_covered` is carried `true`: M8 has no evidence-access reader, and the activation
    /// transaction re-evaluates §6.4 condition 3 against the real ledger before anything applies.
    pub(crate) fn manager_review_rows(&self, pr: &str) -> Vec<ManagerReviewRow> {
        let mut out = Vec::new();
        let Ok(watch) = self.store().load_live_review_watch() else {
            return out;
        };
        for r in watch {
            if format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase() != pr
            {
                continue;
            }
            let completed = self.store().review_completed(&r.key).ok().flatten();
            out.push(ManagerReviewRow {
                reviewer: r.key.reviewer.clone(),
                is_manager: r.key.reviewer == "manager",
                status: r.status.clone(),
                completed,
                diff_covered: true,
            });
        }
        out
    }

    /// Record a `failed_attempt` (with its reason logged) so the pump may re-queue it (§7.2).
    pub(crate) fn record_manager_failed_attempt(&self, row: &ManagerInterventionRow, reason: &str) {
        tracing::warn!(pr = %row.pr, id = %row.id, reason = reason,
            "manager: recording a failed attempt");
        self.set_manager_state(row, MANAGER_INTERVENTION_FAILED_ATTEMPT);
    }

    /// Idempotent state write with a warn on failure.
    pub(crate) fn set_manager_state(&self, row: &ManagerInterventionRow, state: &str) {
        if let Err(e) = self.store().set_manager_intervention_state(&row.id, state) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager: writing the intervention state failed");
        }
    }
}

/// A stable intervention id. The store's unique index is on `pr`, so the id only needs to be
/// unique across the daemon's lifetime; the process-local counter guarantees that within a boot and
/// the daemon id + timestamp keep two boots from colliding.
fn new_intervention_id(daemon_id: &str, now: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("manager-{daemon_id}-{now}-{n}")
}

/// `now + run_timeout_ms + [`MANAGER_LEASE_SLACK`]`, RFC3339 UTC seconds.
fn lease_expiry(now: DateTime<Utc>, run_timeout_ms: i64) -> String {
    let timeout = Duration::milliseconds(run_timeout_ms.max(0));
    (now + timeout + MANAGER_LEASE_SLACK).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A one-line prose summary of a proposal for the room and the memory observation (§9). It names the
/// decision, its rationale, and every dismissed finding by id and revision, so the calibration
/// evidence is self-contained.
fn manager_proposal_prose(decision: &ManagerDecision) -> String {
    let mut out = format!("{}: {}", decision.variant(), decision.rationale);
    if !decision.dismiss.is_empty() {
        let dismissed: Vec<String> = decision
            .dismiss
            .iter()
            .map(|d| format!("{} @ r{}", d.finding.finding, d.finding.revision))
            .collect();
        out.push_str(&format!(" (dismisses {})", dismissed.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rhapsody_config::teams::{Identity, Manager, Review, ReviewAuthority, ReviewMode, Teams};
    use rhapsody_store::{
        MANAGER_INTERVENTION_APPLY_FAILED, MANAGER_INTERVENTION_APPLYING,
        MANAGER_INTERVENTION_AWAITING_EFFECT, MANAGER_INTERVENTION_DECIDED,
        MANAGER_INTERVENTION_ESCALATED, MANAGER_INTERVENTION_EXHAUSTED, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::managerselftest::{SelfTestRecord, SelfTestVerdict};
    use crate::testsupport::{
        DispatchedEntries, TempDir, empty_effective, empty_resolved_project, issue,
    };

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const PR_KEY: &str = "makewhatis/rhapsody#12";
    const PR_DISPLAY: &str = "makewhatis/rhapsody#12";

    fn test_cli_command() -> String {
        "/bin/echo 9.9.9".to_string()
    }

    fn test_cli_version() -> String {
        crate::managerselftest::probe_cli_version(&test_cli_command()).expect("probe fake cli")
    }

    fn record_entries(sink: &DispatchedEntries) -> crate::orchestrator::SpawnFn {
        let sink = Arc::clone(sink);
        Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        })
    }

    fn orch(authority: ReviewAuthority) -> (Orchestrator, DispatchedEntries) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = ["todo".to_string(), "in progress".to_string()]
            .into_iter()
            .collect();
        eff.terminal_states = ["done".to_string()].into_iter().collect();
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        proj.mcfg.claude.command = test_cli_command();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Default::default()
            }],
            manager: Manager {
                review_authority: authority,
                max_concurrent: 1,
                ..Default::default()
            },
            ..Teams::disabled()
        });
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&dispatched));
        (o, dispatched)
    }

    fn pass_self_test(o: &Orchestrator) {
        o.manager_selftest.record(SelfTestRecord {
            cli_version: test_cli_version(),
            verdict: SelfTestVerdict::Passed,
        });
    }

    fn prime_holds(o: &Orchestrator) {
        o.human_holds.begin_pass(true);
    }

    fn divergence(kind: DivergenceKind, pr: &str) -> Divergence {
        Divergence {
            pr: pr.to_string(),
            kind,
            ticket: "STUDIO-1".to_string(),
            reviewer: "alice".to_string(),
            stale_secs: 100_000,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: String::new(),
        }
    }

    fn active(o: &Orchestrator) -> Option<ManagerInterventionRow> {
        o.store()
            .active_manager_intervention(PR_KEY)
            .expect("active read")
    }

    fn state_of(o: &Orchestrator, id: &str) -> String {
        o.store()
            .manager_intervention(id)
            .expect("read")
            .expect("row")
            .state
    }

    // --- pure rules ---------------------------------------------------------------------------

    #[test]
    fn stall_kind_mapping_covers_the_five_named_signals() {
        assert_eq!(
            stall_kind_for(DivergenceKind::ReviewEscalated),
            Some("review_escalated")
        );
        assert_eq!(
            stall_kind_for(DivergenceKind::ReviewShipped),
            Some("review_shipped")
        );
        assert_eq!(
            stall_kind_for(DivergenceKind::ApprovedStillOpen),
            Some("approved_still_open")
        );
        assert_eq!(
            stall_kind_for(DivergenceKind::AuthorTokenCeilingStopped),
            Some("author_token_ceiling")
        );
        assert_eq!(
            stall_kind_for(DivergenceKind::RoundBudgetExhausted),
            Some("round_budget_exhausted")
        );
        // Ordinary progress and ticket-side duties do not route to the manager.
        assert_eq!(stall_kind_for(DivergenceKind::ReviewRequestedNoRun), None);
        assert_eq!(stall_kind_for(DivergenceKind::ChangesRequestedNoRun), None);
        assert_eq!(
            stall_kind_for(DivergenceKind::ReviewTokenCeilingStopped),
            None
        );
        assert_eq!(
            stall_kind_for(DivergenceKind::MergedTicketNotTerminal),
            None
        );
    }

    #[test]
    fn plan_enqueue_creates_with_both_kinds_deduped() {
        let kinds = vec![
            "review_escalated".to_string(),
            "approved_still_open".to_string(),
        ];
        assert_eq!(
            plan_enqueue(None, false, &kinds),
            EnqueueDecision::Create { kinds }
        );
    }

    #[test]
    fn plan_enqueue_merges_before_launch_and_drops_after() {
        let queued = ManagerInterventionRow {
            id: "iv".to_string(),
            stall_kinds: vec!["review_escalated".to_string()],
            state: MANAGER_INTERVENTION_QUEUED.to_string(),
            ..ManagerInterventionRow::default()
        };
        assert_eq!(
            plan_enqueue(Some(&queued), false, &["review_shipped".to_string()]),
            EnqueueDecision::Merge {
                id: "iv".to_string(),
                add: vec!["review_shipped".to_string()]
            }
        );
        // A signal already covered is a no-op (Drop), not a duplicate merge.
        assert_eq!(
            plan_enqueue(Some(&queued), false, &["review_escalated".to_string()]),
            EnqueueDecision::Drop
        );
        let launched = ManagerInterventionRow {
            state: "running".to_string(),
            ..queued.clone()
        };
        assert_eq!(
            plan_enqueue(Some(&launched), false, &["review_shipped".to_string()]),
            EnqueueDecision::Drop
        );
    }

    // A stopped generation never creates another intervention, however many stalls arrive
    // (§15.4 acceptance). MUTATION: release the PR on exhausted and this reds.
    #[test]
    fn plan_enqueue_stopped_never_creates() {
        assert_eq!(
            plan_enqueue(None, true, &["review_escalated".to_string()]),
            EnqueueDecision::Stopped
        );
    }

    #[test]
    fn launch_gates_are_ordered_and_fail_closed() {
        let open = LaunchGateEnv {
            hold_known: true,
            credential_healthy: true,
            selftest_permitted: true,
            ..LaunchGateEnv::default()
        };
        assert_eq!(evaluate_launch_gates(open), LaunchGate::Proceed);
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                drain_active: true,
                ..open
            }),
            LaunchGate::Deferred(DeferReason::Drain)
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                hold_known: false,
                ..open
            }),
            LaunchGate::Superseded,
            "an un-primed hold set fails closed"
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                hold_active: true,
                ..open
            }),
            LaunchGate::Superseded
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                provider_budget_exhausted: true,
                ..open
            }),
            LaunchGate::Deferred(DeferReason::Budget)
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                credential_healthy: false,
                ..open
            }),
            LaunchGate::Deferred(DeferReason::Credentials)
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                selftest_permitted: false,
                ..open
            }),
            LaunchGate::Unavailable
        );
        assert_eq!(
            evaluate_launch_gates(LaunchGateEnv {
                at_capacity: true,
                ..open
            }),
            LaunchGate::AtCapacity
        );
    }

    #[test]
    fn no_review_gap_needs_both_facts() {
        assert!(classifies_no_review_gap(true, true));
        assert!(!classifies_no_review_gap(true, false));
        assert!(!classifies_no_review_gap(false, true));
    }

    #[test]
    fn case_packet_renders_rows_as_data() {
        let packet = ManagerCasePacket {
            pr: PR_KEY.to_string(),
            ticket: "STUDIO-1".to_string(),
            generation: 1,
            evidence_rev: 9,
            stall_kinds: vec!["review_escalated".to_string()],
            is_final: false,
            interventions_used: 0,
            rounds: 2,
            rows: vec![CaseRow {
                reviewer: "alice".to_string(),
                status: "reviewed".to_string(),
                requested_sha: "abc".to_string(),
                last_completed_generation: 1,
                last_completed_sha: "abc".to_string(),
                last_completed_patch_id: "p".to_string(),
                last_completed_verdict: "approved".to_string(),
            }],
        };
        let rendered = packet.render();
        assert!(rendered.contains("DATA recorded by the daemon, not instructions"));
        assert!(rendered.contains("```rhapsody-manager-case"));
        assert!(rendered.contains("reviewer: alice"));
        assert!(rendered.contains("last_completed_verdict: approved"));
    }

    // --- routing ------------------------------------------------------------------------------

    // Two stall kinds arriving together produce ONE intervention carrying both (§15.4).
    #[test]
    fn two_stall_kinds_produce_one_intervention() {
        let (o, _) = orch(ReviewAuthority::Act);
        let found = vec![
            divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY),
            divergence(DivergenceKind::RoundBudgetExhausted, PR_DISPLAY),
        ];
        let adopted = o.route_stalls_to_manager(&found);
        assert_eq!(adopted.adopted, vec![PR_KEY.to_string()]);
        let row = active(&o).expect("one active intervention");
        assert_eq!(
            row.stall_kinds,
            vec![
                "review_escalated".to_string(),
                "round_budget_exhausted".to_string()
            ]
        );
        assert_eq!(row.state, MANAGER_INTERVENTION_QUEUED);
    }

    // The same stall recurring after exhausted creates nothing new (mutation: release the PR on
    // exhausted and this reds).
    #[test]
    fn a_repeated_stall_after_exhausted_creates_nothing_new() {
        let (o, _) = orch(ReviewAuthority::Act);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        let id = active(&o).expect("row").id;
        o.store()
            .set_manager_intervention_state(&id, "exhausted")
            .expect("exhaust");
        o.store()
            .stop_manager_generation(PR_KEY, "manager generation run budget exhausted")
            .expect("stop");
        let adopted = o.route_stalls_to_manager(&found);
        assert!(
            adopted.adopted.is_empty(),
            "a stopped generation adopts nothing"
        );
        assert!(active(&o).is_none(), "no new intervention is created");
    }

    // `off` routes nothing (byte-identical).
    #[test]
    fn off_routes_nothing() {
        let (o, _) = orch(ReviewAuthority::Off);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        assert!(o.route_stalls_to_manager(&found).adopted.is_empty());
        assert!(active(&o).is_none());
    }

    // §7.9: with `review_authority: off` the new wake-skip is inert, so ordinary selection is
    // unchanged even if a pending wake row somehow exists.
    #[test]
    fn off_leaves_the_wake_skip_inert() {
        let (o, _) = orch(ReviewAuthority::Off);
        o.store()
            .save_manager_wake(rhapsody_store::ManagerWakeRow {
                intervention_id: "iv-off".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                issue_id: "STUDIO-1".to_string(),
                state: rhapsody_store::MANAGER_WAKE_PENDING.to_string(),
                ..rhapsody_store::ManagerWakeRow::default()
            })
            .expect("wake");
        assert!(
            !o.manager_wake_blocks_selection("STUDIO-1"),
            "off never blocks ordinary selection"
        );
    }

    // --- the pump -----------------------------------------------------------------------------

    // A dead lease is recovered and never left blocking (§15.4). max_concurrent=0 keeps the pump
    // from immediately relaunching, isolating the recovery.
    #[test]
    fn a_dead_lease_is_recovered_by_the_pump() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: "iv-1".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                mode: MANAGER_MODE_ACT.to_string(),
                state: MANAGER_INTERVENTION_LAUNCHING.to_string(),
                lease_boot_id: "some-other-boot".to_string(),
                lease_expires_at: "2099-01-01T00:00:00Z".to_string(),
                ..ManagerInterventionRow::default()
            })
            .expect("save");
        {
            let teams = o.teams.as_mut().expect("teams");
            teams.manager.max_concurrent = 0;
        }
        o.pump_manager_interventions();
        assert_eq!(
            state_of(&o, "iv-1"),
            MANAGER_INTERVENTION_FAILED_ATTEMPT,
            "a lease from another boot is dead"
        );
    }

    // Every gate is honoured at every retry (§15.4, mutation: check a gate on the first launch only
    // and this reds): a drain defers, and only when it clears does a launch charge the budget.
    #[test]
    fn gates_are_honoured_on_every_retry() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        let id = active(&o).expect("row").id;

        // Tick 1: a drain is armed → deferred, no run charged.
        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            0,
            "a deferral never consumes a budget"
        );
        assert!(dispatched.lock().expect("lock").is_empty());

        // Tick 2: still draining → still deferred, still nothing charged (the retry re-checks).
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            0
        );

        // Tick 3: the drain clears → the launch proceeds and charges exactly one run.
        o.drain.disarm();
        o.pump_manager_interventions();
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1
        );
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    // The happy path end to end: route a stall, pump with every gate open, and exactly one manager
    // run rides the shared dispatch funnel with no watch row.
    #[test]
    fn route_then_pump_launches_once() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        o.pump_manager_interventions();
        {
            let d = dispatched.lock().expect("lock");
            assert_eq!(d.len(), 1, "one manager run dispatched");
            assert_eq!(d[0].issue.identifier, "pr:makewhatis/rhapsody#12@manager");
        }
        let id = active(&o).expect("row").id;
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_RUNNING);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1
        );
    }

    // The case packet is assembled from the host's own records: a seeded watch row's reviewer,
    // status and completed review all reach the rendered DATA block.
    #[test]
    fn case_packet_is_assembled_from_the_watch_set() {
        let (o, _) = orch(ReviewAuthority::Act);
        let key = rhapsody_store::ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "alice".to_string(),
        };
        o.store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                key: key.clone(),
                author: "bob".to_string(),
                introduced_by: "adopt:STUDIO-1".to_string(),
                requested_sha: "deadbeef".to_string(),
                last_reviewed_sha: String::new(),
                status: "reviewed".to_string(),
                open: true,
            })
            .expect("save watch");
        o.store()
            .record_review_completion(
                &key,
                "reviewed",
                &rhapsody_store::ReviewCompleted {
                    generation: 1,
                    sha: "deadbeef".to_string(),
                    patch_id: "patch-1".to_string(),
                    verdict: "approve".to_string(),
                },
            )
            .expect("completion");
        let row = ManagerInterventionRow {
            pr: PR_KEY.to_string(),
            generation: 1,
            stall_kinds: vec!["review_escalated".to_string()],
            ..ManagerInterventionRow::default()
        };
        let rendered = o.manager_case_packet(&row).render();
        assert!(rendered.contains("reviewer: alice"), "{rendered}");
        assert!(rendered.contains("status: reviewed"), "{rendered}");
        assert!(
            rendered.contains("last_completed_verdict: approve"),
            "{rendered}"
        );
        assert!(rendered.contains("ticket: STUDIO-1"), "{rendered}");
    }

    // --- the run's exit: parse + revalidate (B1) ----------------------------------------------

    fn seed_watch(o: &Orchestrator, introduced_by: &str) {
        o.store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                key: rhapsody_store::ReviewWatchKey {
                    owner: "makewhatis".to_string(),
                    repo: "rhapsody".to_string(),
                    number: 12,
                    reviewer: "alice".to_string(),
                },
                author: "bob".to_string(),
                introduced_by: introduced_by.to_string(),
                requested_sha: "deadbeef".to_string(),
                last_reviewed_sha: String::new(),
                status: "reviewed".to_string(),
                open: true,
            })
            .expect("save watch");
    }

    fn decision_text(json: &str) -> String {
        format!(
            "prose\n\n```{}\n{json}\n```\n\nHANDOFF: done\n",
            managerdecision::MANAGER_DECISION_TAG
        )
    }

    fn exit_with(text: Option<&str>) -> crate::retry::EvWorkerExit {
        crate::retry::EvWorkerExit {
            issue_id: "pr:makewhatis/rhapsody#12@manager".to_string(),
            failed: false,
            started_at: Utc::now(),
            err_msg: String::new(),
            last_state: String::new(),
            declared_handoff: true,
            review_verdict: None,
            manager_text: text.map(str::to_string),
            refused: false,
        }
    }

    fn launch_running(o: &mut Orchestrator) -> String {
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        o.pump_manager_interventions();
        let id = active(o).expect("row").id;
        // The recording spawn seam leaves the run in `running`/`claimed`; drop it so a later pump can
        // dispatch a relaunch, exactly as a real run's exit does.
        o.running.remove("pr:makewhatis/rhapsody#12@manager");
        o.claimed.remove("pr:makewhatis/rhapsody#12@manager");
        id
    }

    // --- STUDIO-1016: applying a decision ------------------------------------------------------

    /// A recording applier: captures the requests the control task hands out and lets a test drive
    /// the results back in, exactly as the real off-loop task would.
    struct RecordingApply(
        std::sync::Arc<std::sync::Mutex<Vec<crate::managerapply::ManagerApplyRequest>>>,
    );
    impl crate::managerapply::ManagerApplySink for RecordingApply {
        fn submit(&self, request: crate::managerapply::ManagerApplyRequest) {
            self.0.lock().expect("apply lock").push(request);
        }
    }

    type ApplyRequests =
        std::sync::Arc<std::sync::Mutex<Vec<crate::managerapply::ManagerApplyRequest>>>;

    fn install_applier(o: &mut Orchestrator) -> ApplyRequests {
        let sink: ApplyRequests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        o.manager_apply = Some(std::sync::Arc::new(RecordingApply(std::sync::Arc::clone(
            &sink,
        ))));
        sink
    }

    fn last_request(requests: &ApplyRequests) -> crate::managerapply::ManagerApplyRequest {
        requests
            .lock()
            .expect("apply lock")
            .last()
            .cloned()
            .expect("an apply request")
    }

    /// The `(effect, state)` result the real applier would send for every planned effect.
    fn effect_result(
        id: &str,
        effects: &[&str],
        state: &str,
    ) -> crate::managerapply::ManagerEffectResult {
        crate::managerapply::ManagerEffectResult {
            intervention_id: id.to_string(),
            pr: PR_KEY.to_string(),
            outcomes: effects
                .iter()
                .map(|e| ((*e).to_string(), state.to_string()))
                .collect(),
            reason: String::new(),
            halted: None,
        }
    }

    fn seed_open_finding(o: &Orchestrator, finding: &str) {
        // A test may seed before the intervention's launch created the generation row.
        o.store()
            .ensure_review_generation(PR_KEY)
            .expect("generation");
        let generation = o
            .store()
            .review_bound(PR_KEY)
            .expect("bound")
            .expect("row")
            .generation;
        o.store()
            .save_review_finding(rhapsody_store::ReviewFindingRow {
                pr: PR_KEY.to_string(),
                generation,
                reviewer: "alice".to_string(),
                finding_id: finding.to_string(),
                revision: 1,
                review_run_id: 7,
                raised_at_sha: "deadbeef".to_string(),
                blocking: true,
                status: rhapsody_store::REVIEW_FINDING_OPEN.to_string(),
                ..rhapsody_store::ReviewFindingRow::default()
            })
            .expect("finding");
        o.store().set_review_evidence_rev(PR_KEY, 1).expect("ev");
    }

    /// Drive a decision to `validated` (settle a clean exit), then through `begin_manager_apply`
    /// (a pump), returning the intervention id and the requests the applier saw.
    fn validated_then_applying(o: &mut Orchestrator, json: &str) -> (String, ApplyRequests) {
        pass_self_test(o);
        prime_holds(o);
        seed_watch(o, "adopt:STUDIO-1");
        let requests = install_applier(o);
        let id = launch_running(o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some(&decision_text(json))),
        );
        assert_eq!(state_of(o, &id), MANAGER_INTERVENTION_VALIDATED);
        o.pump_manager_interventions();
        assert_eq!(state_of(o, &id), MANAGER_INTERVENTION_APPLYING);
        (id, requests)
    }

    fn rerun_json(dismiss: &str) -> String {
        format!(
            r#"{{"decision":"RERUN_REVIEW","head":"deadbeef","evidence_rev":1,
                "rerun":{{}},
                "dismiss":[{dismiss}],
                "rationale":"both reviewers need another look"}}"#
        )
    }

    fn route_json(finding: &str) -> String {
        format!(
            r#"{{"decision":"ROUTE_TO_AUTHOR","head":"deadbeef","evidence_rev":1,
                "route":{{"fix":[{{"finding":"{finding}","revision":1}}],"instructions":"fix it"}},
                "rationale":"the finding needs code"}}"#
        )
    }

    fn approve_json(finding: &str) -> String {
        format!(
            r#"{{"decision":"APPROVE","head":"deadbeef","evidence_rev":1,
                "dismiss":[{{"finding":"{finding}","revision":1,"rationale":"settled"}}],
                "rationale":"everyone read this code"}}"#
        )
    }

    /// A live watch row for `alice` plus a completed review at `deadbeef`/`patch-1` — the state an
    /// APPROVE is eligible against (§6.4). `verdict` lets a later same-head round replace it.
    fn seed_approved_review(o: &Orchestrator, verdict: &str) {
        seed_watch(o, "adopt:STUDIO-1");
        o.store()
            .record_review_completion(
                &alice_key(),
                "reviewed",
                &rhapsody_store::ReviewCompleted {
                    generation: 1,
                    sha: "deadbeef".to_string(),
                    patch_id: "patch-1".to_string(),
                    verdict: verdict.to_string(),
                },
            )
            .expect("review completion");
    }

    fn alice_key() -> rhapsody_store::ReviewWatchKey {
        rhapsody_store::ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "alice".to_string(),
        }
    }

    /// Replace alice's watch-row status, simulating the watcher moving a re-requested round on.
    fn set_alice_status(o: &Orchestrator, status: &str) {
        o.store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                key: alice_key(),
                author: "bob".to_string(),
                introduced_by: "adopt:STUDIO-1".to_string(),
                requested_sha: "deadbeef".to_string(),
                last_reviewed_sha: "deadbeef".to_string(),
                status: status.to_string(),
                open: true,
            })
            .expect("save watch");
    }

    /// Replace alice's completed review, simulating a fresh round at another patch.
    fn set_alice_completion(o: &Orchestrator, patch_id: &str, verdict: &str) {
        o.store()
            .record_review_completion(
                &alice_key(),
                "reviewed",
                &rhapsody_store::ReviewCompleted {
                    generation: 1,
                    sha: "deadbeef".to_string(),
                    patch_id: patch_id.to_string(),
                    verdict: verdict.to_string(),
                },
            )
            .expect("review completion");
    }

    // A RERUN_REVIEW with dismissals and NO note still plans and posts the mandatory explanation
    // (§15.4, "Mandatory explanations"). MUTATION: skip planning the explanation when the note is
    // absent and the request carries no explanation body.
    #[test]
    fn a_rerun_with_dismissals_and_no_note_still_posts_the_explanation() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (_, requests) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        let req = last_request(&requests);
        assert!(
            req.effects
                .contains(&crate::managerapply::MANAGER_EFFECT_EXPLANATION.to_string()),
            "the explanation is mandatory: {:?}",
            req.effects
        );
        assert!(
            req.explanation.contains("RERUN_REVIEW"),
            "{}",
            req.explanation
        );
        assert!(req.explanation.contains("alice:F1"), "{}", req.explanation);
        assert!(
            req.explanation.contains("<!-- rhapsody-manager:"),
            "{}",
            req.explanation
        );
        assert!(
            !req.explanation
                .contains(rhapsody_core::SUMMON_TOKEN_SYMPHONY),
            "no manager comment carries a summon token"
        );
    }

    // A HOLD arriving after the explanation request starts but before it is acknowledged refuses
    // activation (§15.4, "Activation boundary"). MUTATION: activate on the comment acknowledgement
    // alone and this test goes green where it must refuse.
    #[test]
    fn a_hold_between_request_and_ack_refuses_activation() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, requests) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        // The explanation request is out; a hold lands before the acknowledgement.
        o.human_holds.note_human_label("STUDIO-1");
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.state, MANAGER_INTERVENTION_SUPERSEDED);
        assert!(
            row.unapplied_explanation,
            "the posted explanation is unapplied"
        );
        // The human feed's reporting is paired with a best-effort "not applied" notice on the PR.
        assert!(
            requests.lock().expect("apply lock").iter().any(|r| r
                .effects
                .contains(&crate::managerapply::MANAGER_EFFECT_UNAPPLIED.to_string())),
            "a refused activation posts a best-effort not-applied notice"
        );
        // The dismissal never became effective.
        assert_eq!(
            o.store()
                .load_review_findings(PR_KEY)
                .expect("findings")
                .into_iter()
                .find(|f| f.finding_id == "alice:F1")
                .map(|f| f.status),
            Some(rhapsody_store::REVIEW_FINDING_OPEN.to_string()),
        );
    }

    // An authority change in that same interval refuses activation (`superseded`).
    #[test]
    fn an_authority_change_between_request_and_ack_refuses_activation() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.teams.as_mut().expect("teams").manager.review_authority = ReviewAuthority::Off;
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_SUPERSEDED);
    }

    // §9 "When the mode changes mid-flight": an `act` intervention switched to `advise` stops at its
    // next effect and ends `superseded` — it must NOT become a proposal or apply anything. MUTATION:
    // activation on the comment acknowledgement alone (ignoring the live authority) and this reds.
    #[test]
    fn a_switch_to_advise_mid_flight_supersedes_an_act_intervention() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, requests) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.teams.as_mut().expect("teams").manager.review_authority = ReviewAuthority::Advise;
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_SUPERSEDED);
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert!(!row.effects_json.is_empty(), "the effect report is kept");
        assert_eq!(
            last_request(&requests).intervention_id,
            id,
            "the explanation it had already planned is what ran"
        );
    }

    // §9 "when the mode changes mid-flight", the `validated` case: an act decision that has NOT
    // begun applying when the authority flips to `advise` must still end `superseded` — otherwise it
    // holds the one-active slot forever, blocking any proposal for that pull request, and would
    // apply on a flip back to `act`. It must run no effect and must NOT become a proposal.
    //
    // The stored decision is deliberately UNPARSEABLE: the revalidation pass leaves such a row where
    // it is (`revalidate_saved_manager_decisions`, "a stored decision that no longer parses is left
    // where it is"), so `pump_manager_applying` is what must supersede it. MUTATION: drop the
    // `supersede_validated_act_interventions` call and this reds (the row stays `validated`).
    #[test]
    fn a_validated_act_decision_is_superseded_when_the_mode_flips_to_advise() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let requests = install_applier(&mut o);
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: "iv-1".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                stall_kinds: vec!["review_escalated".to_string()],
                mode: MANAGER_MODE_ACT.to_string(),
                state: MANAGER_INTERVENTION_VALIDATED.to_string(),
                decision_json: "not a rhapsody-manager-decision block".to_string(),
                decision_head: "deadbeef".to_string(),
                ..ManagerInterventionRow::default()
            })
            .expect("save validated row");

        o.teams.as_mut().expect("teams").manager.review_authority = ReviewAuthority::Advise;
        o.pump_manager_interventions();

        assert_eq!(
            state_of(&o, "iv-1"),
            MANAGER_INTERVENTION_SUPERSEDED,
            "a validated act decision is superseded, not applied, once the authority is gone"
        );
        assert!(
            requests.lock().expect("apply lock").is_empty(),
            "nothing reached the applier"
        );
        let row = o
            .store()
            .manager_intervention("iv-1")
            .expect("read")
            .expect("row");
        assert_eq!(row.mode, MANAGER_MODE_ACT, "the row keeps its own mode");
        // The one-active slot is freed, so a proposal can now be created for the pull request.
        assert!(active(&o).is_none(), "the superseded row frees the slot");
    }

    // Evidence moving at the same head — the `route.fix` revision the decision named is resolved —
    // refuses activation as `stale` (§15.4, "Activation boundary": a same-head blocking review).
    #[test]
    fn a_route_whose_fix_revision_resolved_refuses_activation_as_stale() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        // The author resolves the finding while the explanation is in flight.
        let generation = o
            .store()
            .review_bound(PR_KEY)
            .expect("bound")
            .expect("row")
            .generation;
        o.store()
            .resolve_review_findings(PR_KEY, generation, "alice", "run-9")
            .expect("resolve");
        o.store().set_review_evidence_rev(PR_KEY, 2).expect("ev");
        o.handle_manager_effect(&effect_result(
            &id,
            &[
                crate::managerapply::MANAGER_EFFECT_EXPLANATION,
                crate::managerapply::MANAGER_EFFECT_TICKET_MOVE,
            ],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_STALE);
    }

    // §7.6: a DEFINITIVELY failed explanation leaves the dismissals ineffective and ends the
    // generation (§15.4, "Mandatory explanations"). MUTATION: apply the dismissals without the
    // explanation and the status assert reds.
    #[test]
    fn a_failed_explanation_leaves_the_dismissals_ineffective() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_FAILED,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_APPLY_FAILED);
        assert!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped(),
            "a terminal effect failure stops the generation"
        );
        assert_eq!(
            o.store()
                .load_review_findings(PR_KEY)
                .expect("findings")
                .into_iter()
                .find(|f| f.finding_id == "alice:F1")
                .map(|f| f.status),
            Some(rhapsody_store::REVIEW_FINDING_OPEN.to_string()),
            "a failed explanation leaves the dismissal ineffective"
        );
    }

    // §11.3: a memory failure leaves the decision APPLIED and `memory_state = pending` — it never
    // re-runs or reverts. No memory backend is configured here, so the mirror fails.
    #[test]
    fn a_memory_failure_leaves_the_decision_applied_and_pending() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(
            row.state, MANAGER_INTERVENTION_AWAITING_EFFECT,
            "the decision is applied"
        );
        assert_ne!(row.activated_at, "", "activation committed");
        assert_eq!(
            row.memory_state, "pending",
            "the failed mirror stays pending"
        );
        // And the dismissal DID take effect — a memory failure reverts nothing.
        assert_eq!(
            o.store()
                .load_review_findings(PR_KEY)
                .expect("findings")
                .into_iter()
                .find(|f| f.finding_id == "alice:F1")
                .map(|f| f.status),
            Some(rhapsody_store::REVIEW_FINDING_DISMISSED.to_string()),
        );
    }

    // §7.3: the post-threshold slot is reserved ONLY at activation. A REFUSED activation consumes
    // none. MUTATION: reserve the slot at `applying` and this budget assert reds.
    #[test]
    fn a_refused_activation_reserves_no_slot() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        // Post-threshold: a configured threshold with one answered exchange charged, so the
        // decision WOULD reserve a slot if it activated.
        o.teams
            .as_mut()
            .expect("teams")
            .review
            .adjudicate_after_rounds = 1;
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 12)),
            1,
        );
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .interventions_applied,
            0,
            "nothing is reserved while applying"
        );
        o.human_holds.note_human_label("STUDIO-1");
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_SUPERSEDED);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .interventions_applied,
            0,
            "a refused activation reserves no post-threshold slot"
        );
    }

    // A successful activation writes the pending records effective atomically: the dismissal takes
    // effect, the eligible row is re-requested, and the state is `awaiting_effect`.
    #[test]
    fn a_passing_activation_applies_the_decision() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.state, MANAGER_INTERVENTION_AWAITING_EFFECT);
        assert_eq!(
            o.store()
                .load_review_findings(PR_KEY)
                .expect("findings")
                .into_iter()
                .find(|f| f.finding_id == "alice:F1")
                .map(|f| f.status),
            Some(rhapsody_store::REVIEW_FINDING_DISMISSED.to_string()),
        );
    }

    // §15.4 / B1: an APPROVE refuses activation when a same-head blocking review completed between
    // the decision and its acknowledgement. Alice changes her verdict at the SAME sha and patch and
    // re-raises the finding the decision dismissed at the same revision, so §6.4 condition 4 still
    // passes and the only signal is that a review COMPLETED since the decision. MUTATION: carry
    // `review_completed_since` as the permissive default and the row activates instead of `stale`.
    #[test]
    fn an_approve_refuses_activation_after_a_same_head_blocking_review() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_approved_review(&o, "approve");
        seed_open_finding(&o, "alice:F1");
        install_applier(&mut o);
        let id = launch_running(&mut o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some(&decision_text(&approve_json("alice:F1")))),
        );
        // At settle time the completion IS the decision's, so it validates.
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_VALIDATED);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_APPLYING);

        // Alice completes a CHANGES review at the same sha and patch, re-raising F1 unchanged. The
        // evidence revision moves; the finding set and §6.4 are otherwise untouched.
        seed_approved_review(&o, "changes");
        o.store().set_review_evidence_rev(PR_KEY, 2).expect("ev");

        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_STALE,
            "a same-head blocking review between request and ack refuses activation"
        );
        assert_eq!(
            o.store()
                .load_review_findings(PR_KEY)
                .expect("findings")
                .into_iter()
                .find(|f| f.finding_id == "alice:F1")
                .map(|f| f.status),
            Some(rhapsody_store::REVIEW_FINDING_OPEN.to_string()),
            "the dismissal never became effective"
        );
    }

    // §6.6: a RERUN_REVIEW completes once every re-requested row's round has finished, without
    // waiting for the timeout. MUTATION: leave completion to the timeout alone and this reds (the
    // row stays `awaiting_effect` after the round finishes).
    #[test]
    fn a_rerun_completes_when_every_rerequested_row_finished() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(
            row.rerequested,
            vec!["alice".to_string()],
            "the re-requested set is durable (§6.6)"
        );
        // The re-requested round has not finished (alice is `requested`): still awaiting effect.
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);

        // Alice's re-requested round finishes.
        set_alice_status(&o, "reviewed");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_COMPLETE);
    }

    // §6.6: a RERUN_REVIEW completes when the patch-id moves, even if a re-requested row has not
    // answered — the code it was asked about no longer exists.
    #[test]
    fn a_rerun_completes_when_the_patch_id_moves() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_approved_review(&o, "changes");
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(
            &mut o,
            &rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#),
        );
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.activation_patch_id, "patch-1");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);

        // A new patch-id lands (alice's completion moves to patch-2).
        set_alice_completion(&o, "patch-2", "changes");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_COMPLETE);
    }

    // §6.6: a ROUTE_TO_AUTHOR completes when the author pushes a new patch-id AND the following
    // review round completes at it.
    #[test]
    fn a_route_completes_when_the_author_pushes_and_the_round_finishes() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_approved_review(&o, "approve");
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        o.handle_manager_effect(&effect_result(
            &id,
            &[
                crate::managerapply::MANAGER_EFFECT_EXPLANATION,
                crate::managerapply::MANAGER_EFFECT_TICKET_MOVE,
            ],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.activation_patch_id, "patch-1");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);

        // The author pushes patch-2 and the review round answers it.
        set_alice_completion(&o, "patch-2", "approve");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_COMPLETE);
    }

    // §6.6: an APPROVE still unmerged at its 2 h timeout with the approval still `effective` goes
    // to the human feed and stops the generation (the D7-blocked signature). MUTATION: record a
    // bare `effect_timeout` and the escalated/stopped asserts red.
    #[test]
    fn an_approve_timeout_with_a_still_effective_approval_escalates_and_stops() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_approved_review(&o, "approve");
        seed_open_finding(&o, "alice:F1");
        install_applier(&mut o);
        let id = launch_running(&mut o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some(&decision_text(&approve_json("alice:F1")))),
        );
        o.pump_manager_interventions();
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);

        // Three hours pass; the approval is still effective and the PR has not merged.
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        o.store()
            .save_manager_intervention(rhapsody_store::ManagerInterventionRow {
                activated_at: "2020-01-01T00:00:00Z".to_string(),
                ..row.clone()
            })
            .expect("age the activation");
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_ESCALATED);
        assert!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped(),
            "a D7-blocked APPROVE stops the generation for a human"
        );
    }

    // §6.6 (B5): an APPROVE whose pull request MERGES completes, rather than escalating at its 2 h
    // timeout with a false "did not merge" reason. The watcher retires every live watch row for a
    // merged pull request, which is the control task's observable signal that the PR left the open
    // state. MUTATION: ignore the PR state and the row escalates instead of completing.
    #[test]
    fn an_approve_completes_when_the_pull_request_merges() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_approved_review(&o, "approve");
        seed_open_finding(&o, "alice:F1");
        install_applier(&mut o);
        let id = launch_running(&mut o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some(&decision_text(&approve_json("alice:F1")))),
        );
        o.pump_manager_interventions();
        o.handle_manager_effect(&effect_result(
            &id,
            &[crate::managerapply::MANAGER_EFFECT_EXPLANATION],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);

        // The pull request merges: the watcher retires its live watch rows.
        o.store()
            .drop_review_watch(&alice_key())
            .expect("retire the watch row");
        o.pump_manager_interventions();
        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_COMPLETE,
            "a merged APPROVE completes rather than escalating at the timeout"
        );
        assert!(
            !o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped(),
            "a merge must not stop the generation"
        );
    }

    // §7.9: a `ROUTE_TO_AUTHOR` activation writes a wake obligation for the ticket, and ordinary
    // selection skips it until the obligation is spent.
    #[test]
    fn a_route_activation_writes_a_wake_obligation_that_blocks_selection() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        o.handle_manager_effect(&effect_result(
            &id,
            &[
                crate::managerapply::MANAGER_EFFECT_EXPLANATION,
                crate::managerapply::MANAGER_EFFECT_TICKET_MOVE,
            ],
            crate::managerapply::MANAGER_EFFECT_DONE,
        ));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_AWAITING_EFFECT);
        let wake = o.store().manager_wake(&id).expect("wake").expect("row");
        assert_eq!(wake.state, rhapsody_store::MANAGER_WAKE_PENDING);
        assert_eq!(wake.issue_id, "STUDIO-1", "the origin ticket owns the wake");
        assert_eq!(wake.pr, PR_KEY);
        assert!(
            o.manager_wake_blocks_selection(&wake.issue_id),
            "a pending wake blocks ordinary selection"
        );
        o.store()
            .set_manager_wake_state(&id, rhapsody_store::MANAGER_WAKE_DELIVERED, None, "")
            .expect("deliver");
        assert!(!o.manager_wake_blocks_selection(&wake.issue_id));
    }

    // §15.4 "Waking the author": a manager comment observed BEFORE activation has no effect — no
    // wake obligation exists yet, so nothing can be admitted. MUTATION: write the wake at `applying`
    // and the wake-absent assert reds.
    #[test]
    fn no_wake_obligation_exists_before_activation() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_APPLYING);
        assert!(
            o.store().manager_wake(&id).expect("read").is_none(),
            "the wake obligation is written only by the activation transaction"
        );
        let cand = issue("ID-1", "STUDIO-1", "In Progress");
        o.pump_manager_wakes(&[(&cand, None)]);
        assert!(
            o.running.is_empty(),
            "nothing wakes the author pre-activation"
        );
    }

    // §15.4 "Waking the author": the explanation succeeds but the TICKET MOVE fails. There is no
    // activation, no wake obligation and no wake-up. MUTATION: activate (and write the wake) before
    // every mandatory effect is done and the apply_failed/wake-absent asserts red.
    #[test]
    fn a_failed_ticket_move_writes_no_wake_obligation() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        o.handle_manager_effect(&crate::managerapply::ManagerEffectResult {
            intervention_id: id.clone(),
            pr: PR_KEY.to_string(),
            outcomes: vec![
                (
                    crate::managerapply::MANAGER_EFFECT_EXPLANATION.to_string(),
                    crate::managerapply::MANAGER_EFFECT_DONE.to_string(),
                ),
                (
                    crate::managerapply::MANAGER_EFFECT_TICKET_MOVE.to_string(),
                    crate::managerapply::MANAGER_EFFECT_FAILED.to_string(),
                ),
            ],
            reason: "the tracker refused the move".to_string(),
            halted: None,
        });
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_APPLY_FAILED);
        assert!(
            o.store().manager_wake(&id).expect("read").is_none(),
            "a failed mandatory effect writes no wake obligation"
        );
        assert!(o.running.is_empty(), "nobody was woken");
    }

    // §7.5/§7.7 recovery: a crash after the comment succeeded but BEFORE activation revalidates and
    // refuses when current evidence invalidates the decision. MUTATION: skip revalidation in
    // recovery and this test reds (the decision activates).
    #[test]
    fn a_restart_after_the_comment_revalidates_and_refuses() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        seed_open_finding(&o, "alice:F1");
        let (id, _) = validated_then_applying(&mut o, &route_json("alice:F1"));
        // The comment succeeded, but the daemon crashed before activation: mark the effect done
        // locally, exactly as a recovery would find it.
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        o.store()
            .save_manager_intervention(rhapsody_store::ManagerInterventionRow {
                effects_json: crate::managerapply::render_effects(&[
                    crate::managerapply::ManagerEffect {
                        effect: crate::managerapply::MANAGER_EFFECT_EXPLANATION.to_string(),
                        state: crate::managerapply::MANAGER_EFFECT_DONE.to_string(),
                    },
                    crate::managerapply::ManagerEffect {
                        effect: crate::managerapply::MANAGER_EFFECT_TICKET_MOVE.to_string(),
                        state: crate::managerapply::MANAGER_EFFECT_DONE.to_string(),
                    },
                ]),
                ..row.clone()
            })
            .expect("save effects");
        // Meanwhile the evidence invalidates the decision: the named fix is resolved.
        let generation = o
            .store()
            .review_bound(PR_KEY)
            .expect("bound")
            .expect("row")
            .generation;
        o.store()
            .resolve_review_findings(PR_KEY, generation, "alice", "run-9")
            .expect("resolve");
        o.store().set_review_evidence_rev(PR_KEY, 2).expect("ev");
        // Recovery runs on the next tick through the SAME activation path. No concurrent slot, so
        // the stale row is not immediately relaunched and its state is observable.
        o.teams.as_mut().expect("teams").manager.max_concurrent = 0;
        o.pump_manager_interventions();
        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_STALE,
            "recovery refuses activation when current evidence invalidates the decision"
        );
    }

    // A clean exit with a VALID decision moves the intervention to `decided` → `validated` via M3,
    // with no re-launch (B1). MUTATION: leave `on_manager_exit` untouched and the row stays
    // `running`, so the pump recovers it as a failed attempt and re-runs the model.
    #[test]
    fn a_valid_decision_moves_to_validated() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let id = launch_running(&mut o);
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_RUNNING);

        let text = decision_text(
            r#"{"decision":"ESCALATE","head":"deadbeef","evidence_rev":0,
                "escalate":{"question":"which base?","checked":"compared both diffs"},
                "rationale":"needs a human"}"#,
        );
        o.settle_manager_intervention("pr:makewhatis/rhapsody#12@manager", &exit_with(Some(&text)));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_VALIDATED);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1,
            "a settled exit charges no second run"
        );
    }

    // A clean exit with NO decision block is a `failed_attempt`, which re-queues (B1).
    #[test]
    fn a_clean_exit_without_a_decision_is_a_failed_attempt() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let id = launch_running(&mut o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some("no block here")),
        );
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_FAILED_ATTEMPT);
    }

    // A failed run is a `failed_attempt`, not a stuck `running` row.
    #[test]
    fn a_failed_manager_run_is_a_failed_attempt() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let id = launch_running(&mut o);
        let mut e = exit_with(None);
        e.failed = true;
        o.settle_manager_intervention("pr:makewhatis/rhapsody#12@manager", &e);
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_FAILED_ATTEMPT);
    }

    // `advise` records the decision and never applies it: terminal `proposed` (§9).
    #[test]
    fn an_advise_decision_is_proposed_and_never_applied() {
        let (mut o, _) = orch(ReviewAuthority::Advise);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let id = launch_running(&mut o);
        let text = decision_text(
            r#"{"decision":"ESCALATE","head":"deadbeef","evidence_rev":0,
                "escalate":{"question":"which base?","checked":"compared both diffs"},
                "rationale":"needs a human"}"#,
        );
        o.settle_manager_intervention("pr:makewhatis/rhapsody#12@manager", &exit_with(Some(&text)));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_PROPOSED);
    }

    // A decision that no longer holds (a moved generation) revalidates `superseded`.
    #[test]
    fn a_decision_on_a_moved_generation_is_superseded() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let id = launch_running(&mut o);
        // Bump the generation while the run was in flight.
        o.store()
            .increment_review_generation(PR_KEY)
            .expect("clear");
        let text = decision_text(
            r#"{"decision":"ESCALATE","head":"deadbeef","evidence_rev":0,
                "escalate":{"question":"which base?","checked":"compared both diffs"},
                "rationale":"needs a human"}"#,
        );
        o.settle_manager_intervention("pr:makewhatis/rhapsody#12@manager", &exit_with(Some(&text)));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_SUPERSEDED);
    }

    // A stored decision that names a finding which is then RESOLVED revalidates `stale` rather than
    // pinning the row `validated` forever (B6, §7.5/§8.2). Re-parsing the stored block with the
    // live §6.1 open-status rule fails (`WrongStatusFinding`), the sweep `continue`s, and the active
    // index is held until an operator `/clear`. MUTATION: parse with `parse_decision` (requiring
    // open) and this reds — the row stays `validated`.
    #[test]
    fn a_resolved_named_finding_makes_a_stored_decision_stale() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let id = launch_running(&mut o);
        let generation = o
            .store()
            .review_bound(PR_KEY)
            .expect("bound")
            .expect("row")
            .generation;
        o.store()
            .save_review_finding(rhapsody_store::ReviewFindingRow {
                pr: PR_KEY.to_string(),
                generation,
                reviewer: "alice".to_string(),
                finding_id: "alice:F1".to_string(),
                revision: 1,
                review_run_id: 7,
                raised_at_sha: "deadbeef".to_string(),
                blocking: true,
                status: rhapsody_store::REVIEW_FINDING_OPEN.to_string(),
                ..rhapsody_store::ReviewFindingRow::default()
            })
            .expect("finding");
        o.store()
            .set_review_evidence_rev(PR_KEY, 1)
            .expect("evidence rev");
        let text = decision_text(
            r#"{"decision":"ROUTE_TO_AUTHOR","head":"deadbeef","evidence_rev":1,
                "route":{"fix":[{"finding":"alice:F1","revision":1}],"instructions":"fix it"},
                "rationale":"the finding needs code"}"#,
        );
        o.settle_manager_intervention("pr:makewhatis/rhapsody#12@manager", &exit_with(Some(&text)));
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_VALIDATED);

        // The author fixes the finding: it resolves and the finding-set change moves evidence_rev.
        o.store()
            .resolve_review_findings(PR_KEY, generation, "alice", "run-9")
            .expect("resolve");
        o.store()
            .set_review_evidence_rev(PR_KEY, 2)
            .expect("evidence rev moved");

        // Isolate the revalidation from the relaunch: no concurrent slot, so a `stale` row stays put.
        o.teams.as_mut().expect("teams").manager.max_concurrent = 0;
        o.pump_manager_interventions();
        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_STALE,
            "a resolved named finding makes the stored decision stale"
        );
    }

    // --- deferral visibility (B2) --------------------------------------------------------------

    // A drain defers the launch, and the stall STAYS on the human feed with the manager's wording
    // (§10.2). MUTATION: adopt (drop) the signal while deferred and the surface assert reds.
    #[test]
    fn a_deferred_launch_stays_on_the_human_feed() {
        let (o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        let routing = o.route_stalls_to_manager(&found);
        assert!(
            routing.adopted.is_empty(),
            "nothing is adopted while deferred"
        );
        assert_eq!(
            routing.surfaced,
            vec![(PR_KEY.to_string(), "manager deferred: drain".to_string())]
        );
    }

    // An unavailable manager (the §4.7 self-test has not passed) surfaces its own wording.
    #[test]
    fn an_unavailable_manager_stays_on_the_human_feed() {
        let (o, _) = orch(ReviewAuthority::Act);
        prime_holds(&o); // holds read; the self-test is deliberately NOT passed
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        let routing = o.route_stalls_to_manager(&found);
        assert!(routing.adopted.is_empty());
        assert_eq!(
            routing.surfaced,
            vec![(
                PR_KEY.to_string(),
                "manager unavailable: CLI contract".to_string()
            )]
        );
    }

    // A retry the self-test gate refuses is surfaced on the human feed, not silently adopted
    // (B2a): a `failed_attempt` is a launch candidate, so a refused relaunch is the manager's
    // deferral to show, exactly as a never-launched row's is. MUTATION: treat only queued/deferred
    // as pre-launch for surfacing and this reds (the relaunch is adopted and the stall disappears).
    #[test]
    fn a_refused_relaunch_stays_on_the_human_feed() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let id = launch_running(&mut o);
        // The run ended with an invalid block: a `failed_attempt`, which the pump would relaunch.
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_FAILED_ATTEMPT)
            .expect("failed attempt");
        // The §4.7 self-test now refuses the CLI contract, so the relaunch cannot proceed.
        o.manager_selftest.record(SelfTestRecord {
            cli_version: test_cli_version(),
            verdict: SelfTestVerdict::Failed(crate::managerselftest::ManagerUnavailable {
                cli_version: test_cli_version(),
                detail: "Bash succeeded".to_string(),
            }),
        });
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        let routing = o.route_stalls_to_manager(&found);
        assert!(
            routing.adopted.is_empty(),
            "a refused relaunch is never silently adopted"
        );
        assert_eq!(
            routing.surfaced,
            vec![(
                PR_KEY.to_string(),
                "manager unavailable: CLI contract".to_string()
            )]
        );
    }

    // A stopped generation is on the human feed WITH its reason (§7.2): the operator can tell "the
    // manager gave up" from "the manager never looked". MUTATION: push neither adopted nor surfaced
    // on a stop and the row keeps its generic detail with no reason.
    #[test]
    fn a_stopped_generation_stays_on_the_human_feed_with_its_reason() {
        let (o, _) = orch(ReviewAuthority::Act);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        let id = active(&o).expect("row").id;
        o.store()
            .stop_manager_intervention(
                &id,
                rhapsody_store::MANAGER_INTERVENTION_EXHAUSTED,
                "manager generation run budget exhausted",
            )
            .expect("stop");
        let routing = o.route_stalls_to_manager(&found);
        assert!(
            routing.adopted.is_empty(),
            "a stopped generation adopts nothing"
        );
        assert_eq!(
            routing.surfaced,
            vec![(
                PR_KEY.to_string(),
                "manager generation run budget exhausted".to_string()
            )]
        );
        assert!(active(&o).is_none(), "no new intervention is created");
    }

    // A budget-exhausted manager surfaces the budget wording.
    #[test]
    fn a_budget_refusal_surfaces_the_budget_wording() {
        let (o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        o.note_budget_hold("STUDIO-1", "t", "rhapsody", "claude", 100, 100);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        let routing = o.route_stalls_to_manager(&found);
        assert!(routing.adopted.is_empty());
        assert_eq!(
            routing.surfaced,
            vec![(PR_KEY.to_string(), "manager deferred: budget".to_string())]
        );
    }

    // A dead credential surfaces the credentials wording.
    #[test]
    fn a_dead_credential_surfaces_the_credentials_wording() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        o.probe_cache = Some(crate::preflight::ProbeCache {
            checked_at: Utc::now(),
            healthy: false,
            last_logged_dead_at: None,
        });
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        let routing = o.route_stalls_to_manager(&found);
        assert!(routing.adopted.is_empty());
        assert_eq!(
            routing.surfaced,
            vec![(
                PR_KEY.to_string(),
                "manager deferred: credentials".to_string()
            )]
        );
    }

    // --- every gate at every relaunch (B4) -----------------------------------------------------

    // Every §10.2 gate is honoured when RELAUNCHING a `failed_attempt` — not just on the first
    // launch. MUTATION: return `Proceed` for `failed_attempt` (gate only the pre-launch states) and
    // every assert in the gate loop below reds on the run count.
    #[test]
    fn gates_are_honoured_when_relaunching_a_failed_attempt() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let id = launch_running(&mut o);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1
        );
        // The run ended with an invalid block: a failed attempt, which is a launch candidate.
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_FAILED_ATTEMPT)
            .expect("failed attempt");

        // (a) drain
        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        o.drain.disarm();
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1
        );
        // (b) self-test (a fresh failed attempt, so the refusal leaves it where it was)
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_FAILED_ATTEMPT)
            .expect("failed attempt");
        o.manager_selftest.record(SelfTestRecord {
            cli_version: test_cli_version(),
            verdict: SelfTestVerdict::Failed(crate::managerselftest::ManagerUnavailable {
                cli_version: test_cli_version(),
                detail: "Bash succeeded".to_string(),
            }),
        });
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_FAILED_ATTEMPT);
        pass_self_test(&o);
        // (c) provider budget
        seed_watch(&o, "adopt:STUDIO-1");
        o.note_budget_hold("STUDIO-1", "t", "rhapsody", "claude", 100, 100);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        o.release_budget_hold("STUDIO-1");
        // (d) credentials
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_FAILED_ATTEMPT)
            .expect("failed attempt");
        o.probe_cache = Some(crate::preflight::ProbeCache {
            checked_at: Utc::now(),
            healthy: false,
            last_logged_dead_at: None,
        });
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        o.probe_cache = None;

        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1,
            "no refused relaunch charges a run"
        );

        // Every gate clear: the relaunch proceeds and charges exactly one more run.
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_RUNNING);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            2
        );
    }

    // The same for a `stale` row (§7.2): a stalled re-run is gated at every gate.
    #[test]
    fn gates_are_honoured_when_relaunching_a_stale_row() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        let id = launch_running(&mut o);
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_STALE)
            .expect("stale");

        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_DEFERRED);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            1
        );

        o.drain.disarm();
        // A closed self-test gate leaves the stale row where it was too.
        o.store()
            .set_manager_intervention_state(&id, MANAGER_INTERVENTION_STALE)
            .expect("stale");
        o.manager_selftest.record(SelfTestRecord {
            cli_version: test_cli_version(),
            verdict: SelfTestVerdict::Failed(crate::managerselftest::ManagerUnavailable {
                cli_version: test_cli_version(),
                detail: "Bash succeeded".to_string(),
            }),
        });
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_STALE);
        pass_self_test(&o);
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_RUNNING);
        assert_eq!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .runs_used,
            2
        );
    }

    // --- the case packet carries the RESERVED row (B3) -----------------------------------------

    // The generation's LAST allocation is launched `final: true`, and the packet rendered for it
    // says so. With `max_interventions = 1` the first reservation is already the final one, so the
    // packet must come from the row AS RESERVED, not the pre-reservation snapshot (which has
    // `final: false`). MUTATION: render the packet before the reservation and the stored row is
    // still final but the packet handed to the run would say `final: false`.
    #[test]
    fn the_final_launch_packet_says_final_true() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        o.teams.as_mut().expect("teams").manager.max_interventions = 1;

        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        o.pump_manager_interventions();
        assert_eq!(dispatched.lock().expect("lock").len(), 1);

        let row = active(&o).expect("row");
        assert!(
            row.is_final,
            "the reservation sets `final` on the generation's last allocation"
        );
        let rendered = o.manager_case_packet(&row).render();
        assert!(
            rendered.contains("final: true"),
            "the packet handed to the run must carry the reserved final flag: {rendered}"
        );
    }

    // --- terminal stops (B5) -------------------------------------------------------------------
    // A repeated `apply_failed` stops the generation: the first one already did, and the stall
    // creates nothing new (§15.4). MUTATION: release the generation on a terminal failure and a new
    // intervention appears.
    #[test]
    fn a_repeated_apply_failed_creates_nothing_new() {
        let (o, _) = orch(ReviewAuthority::Act);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        o.route_stalls_to_manager(&found);
        let id = active(&o).expect("row").id;
        o.store()
            .stop_manager_intervention(
                &id,
                rhapsody_store::MANAGER_INTERVENTION_APPLY_FAILED,
                "an effect failed",
            )
            .expect("apply_failed stops the generation");
        assert!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped()
        );
        let routing = o.route_stalls_to_manager(&found);
        assert!(
            routing.adopted.is_empty(),
            "a stopped generation adopts nothing"
        );
        assert_eq!(
            routing.surfaced,
            vec![(PR_KEY.to_string(), "an effect failed".to_string())],
            "the stall stays on the feed carrying the stop reason (§7.2)"
        );
        assert!(active(&o).is_none(), "no new intervention is created");
    }

    // A second pump while the first run holds the single slot launches nothing more.
    #[test]
    fn max_concurrent_full_stays_queued() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: "iv-1".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                mode: MANAGER_MODE_ACT.to_string(),
                state: MANAGER_INTERVENTION_QUEUED.to_string(),
                ..ManagerInterventionRow::default()
            })
            .expect("save");
        o.pump_manager_interventions();
        o.pump_manager_interventions();
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            1,
            "max_concurrent = 1"
        );
    }

    // --- no_review_gap (§7.2) ------------------------------------------------------------------

    fn record_approved(o: &Orchestrator) {
        o.store()
            .record_review_completion(
                &rhapsody_store::ReviewWatchKey {
                    owner: "makewhatis".to_string(),
                    repo: "rhapsody".to_string(),
                    number: 12,
                    reviewer: "alice".to_string(),
                },
                "reviewed",
                &rhapsody_store::ReviewCompleted {
                    generation: 1,
                    sha: "deadbeef".to_string(),
                    patch_id: "patch-1".to_string(),
                    verdict: rhapsody_store::REVIEW_COMPLETION_APPROVE.to_string(),
                },
            )
            .expect("completion");
    }

    // A purely `approved_still_open` stall, with every live row approved at the current patch, is
    // `no_review_gap`: the generation is stopped and nothing is launched (§7.2). MUTATION: stop
    // reading the rows and a PR with no approved row would stop too.
    #[test]
    fn a_purely_approved_still_open_stall_is_no_review_gap() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        record_approved(&o);
        o.route_stalls_to_manager(&[divergence(DivergenceKind::ApprovedStillOpen, PR_DISPLAY)]);
        o.pump_manager_interventions();
        assert!(
            o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped(),
            "the generation is stopped"
        );
        assert!(
            dispatched.lock().expect("lock").is_empty(),
            "no manager run is spent on a no_review_gap stall"
        );
    }

    // An intervention that MERGED a `review_escalated` into an `approved_still_open` is NOT a
    // no_review_gap — the escalation still needs the manager (§7.2). MUTATION: classify from
    // `contains("approved_still_open")` and this reds (nothing launches).
    #[test]
    fn a_merged_escalation_is_not_no_review_gap() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        record_approved(&o);
        o.route_stalls_to_manager(&[
            divergence(DivergenceKind::ApprovedStillOpen, PR_DISPLAY),
            divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY),
        ]);
        o.pump_manager_interventions();
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            1,
            "the merged escalation still launches the manager"
        );
        assert!(
            !o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped()
        );
    }

    // --- STUDIO-1018: advise mode (§9) ---------------------------------------------------------

    /// A RERUN_REVIEW sufficient to pass the §6.2 preconditions after `seed_watch`'s single
    /// eligible row.
    fn plain_rerun_json() -> String {
        rerun_json(r#"{"finding":"alice:F1","revision":1,"rationale":"superseded"}"#)
    }

    /// Drive an `advise` run to its terminal proposal. Returns the intervention id.
    fn propose(o: &mut Orchestrator) -> String {
        pass_self_test(o);
        prime_holds(o);
        seed_open_finding(o, "alice:F1");
        seed_watch(o, "adopt:STUDIO-1");
        let id = launch_running(o);
        o.settle_manager_intervention(
            "pr:makewhatis/rhapsody#12@manager",
            &exit_with(Some(&decision_text(&plain_rerun_json()))),
        );
        id
    }

    fn room_lines(room: &rhapsody_config::room::LocalRoom) -> Vec<String> {
        room.read_since("reader", &rhapsody_config::room::Cursor::default(), 100)
            .expect("read room")
            .messages
            .into_iter()
            .map(|m| format!("{}: {}", m.from, m.body))
            .collect()
    }

    /// **Acceptance {@15.4 "Modes"}, mutation 1.** An `advise` decision is recorded as a terminal
    /// proposal and produces NO PR comment, no approval and no live-budget charge. MUTATION: drop the
    /// `mode == ADVISE` early return in `advance_manager_decision` and the decision is revalidated,
    /// planned and submitted to the applier — this reds on the apply requests and the approval read.
    #[test]
    fn an_advise_decision_is_recorded_as_a_proposal_and_never_applied() {
        let (mut o, _dispatched) = orch(ReviewAuthority::Advise);
        let requests = install_applier(&mut o);
        let id = propose(&mut o);

        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_PROPOSED,
            "an advise decision ends `proposed`"
        );
        let budget = o.store().manager_budget(PR_KEY).expect("budget");
        assert!(
            budget.as_ref().is_none_or(|b| b.runs_used == 0),
            "an advise run never charges the live budget"
        );

        // Never applied: a pump plans nothing, submits nothing, and records no approval.
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_PROPOSED);
        assert!(
            requests.lock().expect("apply lock").is_empty(),
            "a proposal reaches no applier"
        );
        assert!(
            o.store()
                .load_manager_approvals()
                .expect("approvals")
                .is_empty(),
            "a proposal records no approval"
        );
    }

    /// **Acceptance {@15.4 "Modes"}, mutation 3.** A proposal can never be promoted to an applied
    /// decision: neither `begin_manager_apply` nor recovery may move it off `proposed`. MUTATION:
    /// drop the `row.mode != ACT` guard in `begin_manager_apply` (or the `proposed` re-pin in
    /// `revalidate_saved_manager_decisions`) and this reds.
    #[test]
    fn a_proposal_is_never_promoted() {
        let (mut o, _dispatched) = orch(ReviewAuthority::Advise);
        let requests = install_applier(&mut o);
        let id = propose(&mut o);
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_PROPOSED);

        // A direct attempt to begin applying the proposal is refused: a proposal is not an act
        // decision, and no effect may be planned or submitted for it.
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        o.begin_manager_apply(&row);
        assert_eq!(
            state_of(&o, &id),
            MANAGER_INTERVENTION_PROPOSED,
            "a proposal is never promoted to applying"
        );
        o.pump_manager_interventions();
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_PROPOSED);
        assert!(requests.lock().expect("apply lock").is_empty());
        assert!(
            o.store()
                .manager_approval(&id)
                .expect("approval read")
                .is_none()
        );

        // Recovery pins a `decided` advise row (as a crash between the decision write and the
        // terminal write leaves it) to `proposed` — never to a validated or applied state.
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: "iv-crashed".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                stall_kinds: vec!["review_escalated".to_string()],
                mode: MANAGER_MODE_ADVISE.to_string(),
                state: MANAGER_INTERVENTION_DECIDED.to_string(),
                decision_json: plain_rerun_json(),
                decision_head: "deadbeef".to_string(),
                ..ManagerInterventionRow::default()
            })
            .expect("save crashed row");
        o.revalidate_saved_manager_decisions();
        assert_eq!(
            state_of(&o, "iv-crashed"),
            MANAGER_INTERVENTION_PROPOSED,
            "recovery of a decided advise row records a proposal only"
        );
    }

    /// §9: shadow output goes to the ROOM (which cannot dispatch), never to the PR. MUTATION: drop
    /// the room append in `record_manager_proposal` and this reds.
    #[test]
    fn an_advise_proposal_is_posted_to_the_room_only() {
        let dir = TempDir::new();
        let room = Arc::new(rhapsody_config::room::LocalRoom::new(dir.child("room")));
        let (mut o, _dispatched) = orch(ReviewAuthority::Advise);
        o.teams_room = Some(Arc::clone(&room));
        let _ = propose(&mut o);

        let lines = room_lines(&room);
        assert_eq!(lines.len(), 1, "exactly one proposal line");
        let line = &lines[0];
        assert!(line.contains("@manager"), "posted as the manager: {line}");
        assert!(line.contains("advise"), "names the mode: {line}");
        assert!(line.contains("PROPOSED"), "marks it as not applied: {line}");
        assert!(
            !line.contains(rhapsody_core::SUMMON_TOKEN_SYMPHONY)
                && !line.contains(rhapsody_core::SUMMON_TOKEN_RHAPSODY),
            "a proposal carries no summon token: {line}"
        );
    }

    /// §9: once the generation's shadow budget is spent, another stall creates no new proposal —
    /// and, critically, the live generation is not stopped. MUTATION: drop the
    /// `manager_shadow_budget_spent` guard and this reds (a new advise row appears on every sweep).
    #[test]
    fn a_spent_shadow_budget_creates_no_more_proposals_and_stops_nothing() {
        let (o, _dispatched) = orch(ReviewAuthority::Advise);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        o.store()
            .ensure_review_generation(PR_KEY)
            .expect("generation");
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: "iv-old".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                stall_kinds: vec!["review_escalated".to_string()],
                mode: MANAGER_MODE_ADVISE.to_string(),
                state: MANAGER_INTERVENTION_EXHAUSTED.to_string(),
                attempts: 12,
                ..ManagerInterventionRow::default()
            })
            .expect("save");

        o.route_stalls_to_manager(&[divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)]);

        assert!(active(&o).is_none(), "no new proposal is created");
        assert!(
            !o.store()
                .manager_budget(PR_KEY)
                .expect("budget")
                .expect("row")
                .is_stopped(),
            "a spent shadow budget never stops the live generation"
        );
    }

    /// **Acceptance (STUDIO-1018, §9 "one per stall"), review B2.** A proposal is TERMINAL, so it
    /// never appears as the ACTIVE intervention: without a recorded-proposal guard a persistent
    /// stall is re-detected on every sweep and buys a fresh shadow run, proposal and room post each
    /// time, up to the whole shadow budget. One stall must buy exactly one proposal.
    ///
    /// MUTATION: drop the `manager_advise_stall_recorded` guard and this reds (twenty sweeps create
    /// twenty proposals).
    #[test]
    fn a_persistent_stall_buys_one_proposal() {
        let dir = TempDir::new();
        let room = Arc::new(rhapsody_config::room::LocalRoom::new(dir.child("room")));
        let (mut o, _dispatched) = orch(ReviewAuthority::Advise);
        o.teams_room = Some(Arc::clone(&room));
        let id = propose(&mut o);
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_PROPOSED);

        // The same stall is re-detected on every sweep. A terminal proposal is invisible to
        // `active_manager_intervention`, so each sweep would otherwise fund a new shadow run.
        for _ in 0..20 {
            o.route_stalls_to_manager(&[divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)]);
            o.pump_manager_interventions();
        }

        let rows = o.store().load_manager_interventions().expect("rows");
        let advises: Vec<&ManagerInterventionRow> = rows
            .iter()
            .filter(|r| r.mode == MANAGER_MODE_ADVISE)
            .collect();
        assert_eq!(
            advises.len(),
            1,
            "one stall buys one proposal, got {}",
            advises.len()
        );
        assert_eq!(advises[0].id, id);
        assert_eq!(advises[0].state, MANAGER_INTERVENTION_PROPOSED);
        assert_eq!(
            room_lines(&room).len(),
            1,
            "and one room post, not one per sweep"
        );
    }

    /// §9: in `advise` the manager is NOT authoritative — today's STUDIO-956 turn still is — so a
    /// shadow proposal never ADOPTS a stall. The signal must stay on the human feed exactly as it
    /// would with `off`, with only the proposal added (to the room and the console).
    ///
    /// MUTATION: drop the `mode == ADVISE` reset in `route_stalls_to_manager` and this reds
    /// (`adopted` names the pull request, so the reconciliation sweep would drop its escalation).
    #[test]
    fn advise_never_adopts_a_stall_from_the_human_feed() {
        let (o, _dispatched) = orch(ReviewAuthority::Advise);
        pass_self_test(&o);
        prime_holds(&o);
        seed_watch(&o, "adopt:STUDIO-1");
        let routing =
            o.route_stalls_to_manager(&[divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)]);
        assert!(
            routing.adopted.is_empty(),
            "advise must not adopt the stall: {routing:?}"
        );
        let rows = o.store().load_manager_interventions().expect("rows");
        assert_eq!(rows.len(), 1, "the shadow run is still enqueued");
        assert_eq!(rows[0].mode, MANAGER_MODE_ADVISE);
    }

    /// §9/§11.1: the maintainer's later action on the PR is recorded once as the proposal's outcome —
    /// the calibration evidence for switching to `act`. MUTATION: skip the `proposed`/advise filter
    /// and a second observation would overwrite the first.
    #[test]
    fn a_proposals_outcome_is_recorded_once() {
        let (mut o, _dispatched) = orch(ReviewAuthority::Advise);
        let id = propose(&mut o);

        o.record_manager_proposal_outcomes(PR_KEY, "merged");
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.outcome, "merged", "the observed action is the outcome");
        assert!(!row.outcome_at.is_empty(), "the outcome is dated");

        // Set once: a later, different observation never overwrites it (§11.2).
        o.record_manager_proposal_outcomes(PR_KEY, "closed_unmerged");
        let row = o
            .store()
            .manager_intervention(&id)
            .expect("read")
            .expect("row");
        assert_eq!(row.outcome, "merged", "an outcome is set once");
    }
}
