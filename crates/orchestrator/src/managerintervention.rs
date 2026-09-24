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
use rhapsody_config::teams::ReviewAuthority;
use rhapsody_store::{
    MANAGER_INTERVENTION_DEFERRED, MANAGER_INTERVENTION_FAILED_ATTEMPT,
    MANAGER_INTERVENTION_LAUNCHING, MANAGER_INTERVENTION_NO_REVIEW_GAP,
    MANAGER_INTERVENTION_QUEUED, MANAGER_INTERVENTION_RUNNING, MANAGER_INTERVENTION_STALE,
    MANAGER_INTERVENTION_SUPERSEDED, MANAGER_MODE_ACT, MANAGER_MODE_ADVISE,
    MANAGER_PHASE_POST_THRESHOLD, MANAGER_PHASE_PRE_THRESHOLD, ManagerInterventionRow,
    ManagerReservation,
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
        | DivergenceKind::MergedTicketNotTerminal => None,
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
fn parse_pr_key(key: &str) -> Option<PrCoord> {
    let (repo_part, number) = key.rsplit_once('#')?;
    let (owner, repo) = repo_part.split_once('/')?;
    let number: i64 = number.parse().ok()?;
    if owner.is_empty() || repo.is_empty() || number <= 0 {
        return None;
    }
    Some(PrCoord::new(owner, repo, number))
}

impl Orchestrator {
    /// Whether the manager acts (`act` or `advise`), i.e. the sweep routes stalls to it. `off`
    /// keeps today's byte-identical human feed.
    pub(crate) fn manager_routing_enabled(&self) -> bool {
        self.manager_review_authority() != ReviewAuthority::Off
    }

    fn manager_mode_token(&self) -> &'static str {
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

    pub(crate) fn manager_max_interventions(&self) -> i64 {
        self.teams
            .as_ref()
            .map_or(3, |t| t.manager.max_interventions)
    }

    pub(crate) fn manager_max_concurrent(&self) -> i64 {
        self.teams.as_ref().map_or(1, |t| t.manager.max_concurrent)
    }

    /// Hands the sweep's stall signals to the manager (§5.1, D3), and returns the pull-request keys
    /// it ADOPTED so the caller can drop those signals from the human feed.
    ///
    /// **The sweep still acts on nothing itself.** This only enqueues or merges an intervention; a
    /// launch happens on the control tick ([`Orchestrator::pump_manager_interventions`]). A signal
    /// for a PR whose generation is stopped is deliberately NOT adopted — the stop is a visible
    /// human-feed fact, not something to swallow.
    pub(crate) fn route_stalls_to_manager(&self, found: &[Divergence]) -> Vec<String> {
        if !self.manager_routing_enabled() {
            return Vec::new();
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

        let mut adopted = Vec::new();
        for pr in order {
            let kinds = by_pr.remove(&pr).unwrap_or_default();
            let stopped = self
                .store()
                .manager_budget(&pr)
                .ok()
                .flatten()
                .is_some_and(|b| b.is_stopped());
            let active = self.store().active_manager_intervention(&pr).ok().flatten();
            match plan_enqueue(active.as_ref(), stopped, &kinds) {
                EnqueueDecision::Stopped => {
                    // The generation is stopped; the signal stays on the human feed.
                    tracing::warn!(pr = %pr, "manager: the generation is stopped; no intervention");
                }
                EnqueueDecision::ModeOff => {}
                EnqueueDecision::Drop => {
                    adopted.push(pr);
                }
                EnqueueDecision::Merge { id, add } => {
                    if let Err(e) = self.store().merge_manager_stall_kinds(&id, &add) {
                        tracing::warn!(pr = %pr, id = %id, err = %e,
                            "manager: merging stall kinds into the intervention failed");
                    }
                    adopted.push(pr);
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
                            adopted.push(pr);
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
        adopted
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

            let Some(mut run) = self.manager_run_for(&row.pr) else {
                tracing::warn!(pr = %row.pr,
                    "manager: no configured project owns the pull request's repo; not launched");
                continue;
            };
            run.case_packet = self.manager_case_packet(row).render();

            // The pre-launch classification with no model call (§7.2): an `approved_still_open`
            // stall is exactly "every live row is satisfied, and only a merge gate blocks", so no
            // manager decision can help.
            let approved_still_open = row.stall_kinds.iter().any(|k| k == "approved_still_open");
            if classifies_no_review_gap(approved_still_open, approved_still_open) {
                if let Err(e) = self
                    .store()
                    .set_manager_intervention_state(&row.id, MANAGER_INTERVENTION_NO_REVIEW_GAP)
                {
                    tracing::warn!(pr = %row.pr, err = %e,
                        "manager: recording no_review_gap failed");
                    continue;
                }
                self.stop_manager_generation(
                    &row.pr,
                    "no review gap: every reviewer row is satisfied; a merge gate blocks",
                );
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
                    tracing::warn!(pr = %row.pr,
                        "manager: the run budget is spent; the generation is stopped");
                    continue;
                }
                Ok(ManagerReservation::Absent) => continue,
                Err(e) => {
                    tracing::warn!(pr = %row.pr, err = %e,
                        "manager: reserving a run failed; not launched");
                    continue;
                }
            }

            match self.dispatch_manager(run) {
                ManagerDispatchOutcome::Dispatched => {
                    slots = slots.saturating_sub(1);
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
    fn manager_run_timeout_ms(&self) -> i64 {
        self.teams
            .as_ref()
            .map_or(1_800_000, |t| t.manager.run_timeout_ms)
    }

    /// Gather the §10.2 gate inputs for `pr`. Pure, synchronous reads of loop-owned state.
    fn manager_gate_env(&self, pr: &str) -> LaunchGateEnv {
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
    fn manager_pr_ticket(&self, pr: &str) -> Option<String> {
        let rows = self.store().load_live_review_watch().ok()?;
        rows.into_iter()
            .find(|r| {
                format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number).to_ascii_lowercase()
                    == pr
            })
            .and_then(|r| crate::reviewdone::origin_ticket(&r.introduced_by).map(str::to_string))
    }

    /// Whether any live watch row for `pr` has an origin ticket wearing the hold.
    fn manager_pr_held(&self, pr: &str, labelled: &std::collections::HashSet<String>) -> bool {
        let Ok(rows) = self.store().load_live_review_watch() else {
            return false;
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

    /// Stops `pr`'s generation and logs the reason (§7.2). A no-op with an empty reason.
    pub(crate) fn stop_manager_generation(&self, pr: &str, reason: &str) {
        if reason.is_empty() {
            return;
        }
        if let Err(e) = self.store().stop_manager_generation(pr, reason) {
            tracing::warn!(pr = %pr, err = %e, "manager: stopping the generation failed");
        } else {
            tracing::warn!(pr = %pr, reason = reason, "manager: the generation is stopped");
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rhapsody_config::teams::{Identity, Manager, Review, ReviewAuthority, ReviewMode, Teams};
    use rhapsody_store::{Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::managerselftest::{SelfTestRecord, SelfTestVerdict};
    use crate::testsupport::{DispatchedEntries, empty_effective, empty_resolved_project};

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
        assert_eq!(adopted, vec![PR_KEY.to_string()]);
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
        assert!(adopted.is_empty(), "a stopped generation adopts nothing");
        assert!(active(&o).is_none(), "no new intervention is created");
    }

    // `off` routes nothing (byte-identical).
    #[test]
    fn off_routes_nothing() {
        let (o, _) = orch(ReviewAuthority::Off);
        let found = vec![divergence(DivergenceKind::ReviewEscalated, PR_DISPLAY)];
        assert!(o.route_stalls_to_manager(&found).is_empty());
        assert!(active(&o).is_none());
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
        assert_eq!(state_of(&o, &id), MANAGER_INTERVENTION_LAUNCHING);
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
        assert!(rendered.contains("last_completed_verdict: approve"), "{rendered}");
        assert!(rendered.contains("ticket: STUDIO-1"), "{rendered}");
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
}
