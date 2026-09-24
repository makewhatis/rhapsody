//! managerapply — APPLYING A DECISION: the mandatory explanation, the off-loop applier's effect
//! model, and the ONE activation transaction (STUDIO-1016, design record
//! `manager-agent-design.md` §7.6, §7.7, §7.8, §7.9, §6.6, §8.3, §11). **No Go v0.4.0 counterpart**:
//! the whole ticketless review loop and the manager that adjudicates it are Rhapsody additions.
//!
//! # The one rule
//!
//! Nothing a decision grants takes effect until ONE activation transaction commits on the control
//! task ([`Store::activate_manager_intervention`]). A confirmed external effect — a posted
//! explanation, a confirmed ticket move — is **one prerequisite** for activation, **never permission
//! to activate**. The transaction re-runs the full §7.7 revalidation against current loop-owned
//! state; if it fails, every pending record is cancelled and the intervention ends `superseded` or
//! `stale` with `unapplied_explanation` recorded.
//!
//! # Effects and delivery
//!
//! Effects run off the control loop, structured like [`crate::runautomerge`]. Every decision except
//! `ESCALATE` has one mandatory explanation comment; `ROUTE_TO_AUTHOR` additionally has a mandatory
//! ticket move. Each comment carries a hidden marker
//! `<!-- rhapsody-manager:{id}:{effect} -->`, so a timed-out request can be reconciled by searching
//! for it (at-least-once delivery; a duplicate is harmless because no manager comment carries a
//! summon token or authority). Local records (`effects_json`, the pending approval) are written by
//! the control task; the applier only performs external effects.
//!
//! # Where the off-loop half lives
//!
//! [`crate::runmanagerapply`] performs the external effects (the `gh` comment and the tracker move)
//! behind a [`ManagerApplySink`]; the control task hands it a [`ManagerApplyRequest`] and folds the
//! [`ManagerEffectResult`]s back in through [`Orchestrator::handle_manager_effect`]. Tests install a
//! recording sink; the composition root installs the real task.

use chrono::{DateTime, Duration as ChronoDuration};
use rhapsody_config::teams::ReviewAuthority;
use rhapsody_store::{
    MANAGER_APPROVAL_EFFECTIVE, MANAGER_APPROVAL_PENDING, MANAGER_EXCHANGE_ACTIVE,
    MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_REVIEW_ROUND,
    MANAGER_INTERVENTION_APPLY_FAILED, MANAGER_INTERVENTION_APPLY_UNCERTAIN,
    MANAGER_INTERVENTION_APPLYING, MANAGER_INTERVENTION_AWAITING_EFFECT,
    MANAGER_INTERVENTION_COMPLETE, MANAGER_INTERVENTION_EFFECT_TIMEOUT,
    MANAGER_INTERVENTION_ESCALATED, MANAGER_WAKE_PENDING, ManagerActivation,
    ManagerActivationOutcome, ManagerActivationVerdict, ManagerApprovalRow, ManagerExchange,
    ManagerInterventionRow, ManagerWakeRow, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REQUESTED,
};
use serde::{Deserialize, Serialize};

use crate::managerdecision::{self, DecisionKind, ManagerDecision, Revalidation};
use crate::orchestrator::Orchestrator;

/// The mandatory explanation effect: one comment per decision except `ESCALATE` (§7.6).
pub const MANAGER_EFFECT_EXPLANATION: &str = "explanation";
/// The mandatory ticket move for `ROUTE_TO_AUTHOR` (§7.6).
pub const MANAGER_EFFECT_TICKET_MOVE: &str = "ticket_move";
/// The best-effort question comment for `ESCALATE`; it gates nothing (§7.6).
pub const MANAGER_EFFECT_ESCALATION: &str = "escalation";
/// The best-effort "not applied: <reason>" follow-up after a refused activation (§7.7).
pub const MANAGER_EFFECT_UNAPPLIED: &str = "unapplied";

/// An effect has been requested locally but not yet confirmed.
pub const MANAGER_EFFECT_PENDING: &str = "pending";
/// The effect is confirmed done (the comment's response succeeded, or its marker was found).
pub const MANAGER_EFFECT_DONE: &str = "done";
/// A later effect was refused, so this one is cancelled and never runs.
pub const MANAGER_EFFECT_CANCELLED: &str = "cancelled";
/// The effect's outcome could not be confirmed within its bound.
pub const MANAGER_EFFECT_UNKNOWN: &str = "unknown";
/// The effect failed definitively (for example a 4xx comment rejection).
pub const MANAGER_EFFECT_FAILED: &str = "failed";

/// One effect's per-intervention status (§7.1 `effects_json`). Serialized as a small JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerEffect {
    /// One of the `MANAGER_EFFECT_*` effect names.
    pub effect: String,
    /// One of the `MANAGER_EFFECT_*` states.
    pub state: String,
}

/// The hidden marker a manager comment carries, so a timed-out request can be reconciled by search
/// (§7.6). It names the intervention and the effect.
pub fn manager_explanation_marker(intervention_id: &str, effect: &str) -> String {
    format!("<!-- rhapsody-manager:{intervention_id}:{effect} -->")
}

/// Strip every summon token from a manager comment body. **No manager comment carries a summon
/// token** (§7.9): a manager comment can wake nobody and carry no manager authority, so any token
/// the model's own rationale happened to contain is removed before the body is ever posted.
fn strip_summon_tokens(body: &str) -> String {
    let mut out = body.to_string();
    for token in [
        rhapsody_core::SUMMON_TOKEN_SYMPHONY,
        rhapsody_core::SUMMON_TOKEN_RHAPSODY,
    ] {
        out = out.replace(token, "");
    }
    out
}

/// The MANDATORY explanation body (§7.6). It names the decision and its rationale, **every dismissed
/// finding by id and revision with its rationale**, and — when they apply — the optional
/// `rerun.note` and `route.fix` with `route.instructions`. The note is content INSIDE the
/// explanation, never a substitute for it. The trailing comment is the effect marker.
///
/// No summon token can survive into the returned body.
pub fn manager_explanation_body(decision: &ManagerDecision, marker: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("Manager decision: {}.\n", decision.variant()));
    out.push_str(&format!("Rationale: {}\n", decision.rationale));
    if !decision.dismiss.is_empty() {
        out.push_str("\nDismissed findings:\n");
        for d in &decision.dismiss {
            out.push_str(&format!(
                "- {} @ r{}: {}\n",
                d.finding.finding, d.finding.revision, d.rationale
            ));
        }
    }
    match &decision.kind {
        DecisionKind::RerunReview { note, .. } => {
            if let Some(note) = note {
                out.push_str(&format!("\nNote: {note}\n"));
            }
        }
        DecisionKind::RouteToAuthor { fix, instructions } => {
            out.push_str("\nFix:\n");
            for f in fix {
                out.push_str(&format!("- {} @ r{}\n", f.finding, f.revision));
            }
            out.push_str(&format!("\nInstructions: {instructions}\n"));
        }
        DecisionKind::Approve | DecisionKind::Escalate { .. } => {}
    }
    out.push('\n');
    out.push_str(marker);
    strip_summon_tokens(&out)
}

/// The mandatory effects for a decision (§7.6), in application order. `ESCALATE` has none — its
/// question comment is best effort and an escalation activates nothing.
pub fn plan_effects(decision: &ManagerDecision) -> Vec<&'static str> {
    match decision.kind {
        DecisionKind::Escalate { .. } => Vec::new(),
        DecisionKind::RouteToAuthor { .. } => {
            vec![MANAGER_EFFECT_EXPLANATION, MANAGER_EFFECT_TICKET_MOVE]
        }
        DecisionKind::RerunReview { .. } | DecisionKind::Approve => {
            vec![MANAGER_EFFECT_EXPLANATION]
        }
    }
}

/// Parse `effects_json`. An unreadable or empty value yields no effects (fail closed: the caller
/// treats a missing mandatory effect as still pending).
pub fn parse_effects(json: &str) -> Vec<ManagerEffect> {
    if json.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(json).unwrap_or_default()
}

/// Render `effects` back to the stored form. Deterministic so a round trip is byte-stable.
pub fn render_effects(effects: &[ManagerEffect]) -> String {
    serde_json::to_string(effects).unwrap_or_default()
}

/// Whether every MANDATORY effect is confirmed done. An effect that is still pending, cancelled,
/// unknown or failed is not done.
pub fn all_effects_done(effects: &[ManagerEffect]) -> bool {
    effects.iter().all(|e| e.state == MANAGER_EFFECT_DONE)
}

/// Whether any effect ended definitively failed or unconfirmable.
pub fn effect_failure(effects: &[ManagerEffect]) -> Option<&'static str> {
    if effects.iter().any(|e| e.state == MANAGER_EFFECT_FAILED) {
        return Some(MANAGER_EFFECT_FAILED);
    }
    if effects.iter().any(|e| e.state == MANAGER_EFFECT_UNKNOWN) {
        return Some(MANAGER_EFFECT_UNKNOWN);
    }
    None
}

/// One external effect the off-loop applier is asked to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerApplyRequest {
    /// The intervention this request belongs to.
    pub intervention_id: String,
    /// `owner/repo#number`, case-folded.
    pub pr: String,
    /// `owner` coordinate, split out for the applier.
    pub owner: String,
    /// `repo` coordinate.
    pub repo: String,
    /// `number` coordinate.
    pub number: i64,
    /// The effect names still to perform, in order.
    pub effects: Vec<String>,
    /// The explanation body (with its marker), when an explanation is among `effects`.
    pub explanation: String,
    /// The marker's intervention/effect pair, for the applier's marker search.
    pub marker: String,
    /// The ticket move's coordinates, when a ticket move is among `effects`.
    pub ticket_move: Option<ManagerTicketMove>,
}

/// A ticket move the applier performs off-loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerTicketMove {
    /// The opaque tracker issue id.
    pub issue_id: String,
    /// The tracker team id.
    pub team_id: String,
    /// The state NAME to move to.
    pub state: String,
}

/// The applier's report of what it did for one intervention.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagerEffectResult {
    /// The intervention.
    pub intervention_id: String,
    /// `owner/repo#number`, case-folded.
    pub pr: String,
    /// `(effect, state)` pairs, states from the `MANAGER_EFFECT_*` set.
    pub outcomes: Vec<(String, String)>,
    /// A human-facing reason for a failure/uncertainty (empty when all done).
    pub reason: String,
    /// Set when a §8.3 pre-effect check stopped the run before an effect: `superseded` or `stale`.
    /// The control task then refuses activation with that classification.
    pub halted: Option<String>,
}

/// The off-loop applier seam. The composition root installs the real task
/// ([`crate::runmanagerapply`]); tests install a recorder. `submit` never blocks the control task.
pub trait ManagerApplySink: Send + Sync {
    /// Hands one request to the applier.
    fn submit(&self, request: ManagerApplyRequest);
}

/// The §8.3 cheap check the applier's round-trip answers before EACH external effect, so a revoked
/// decision stops early. The decisive check remains the activation transaction (§7.7); this only
/// saves work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreEffectCheck {
    /// The decision still holds: perform the effect.
    Proceed,
    /// The generation, authority, hold, PR-open or enablement changed: stop (`superseded`).
    Superseded,
    /// The evidence moved in a way §8.2 would treat as `stale`: stop (`stale`).
    Stale,
}

/// The §8.3 pre-effect check, as a pure table over the current answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreEffectInputs {
    /// The generation the decision was bound to.
    pub generation: i64,
    /// The current generation.
    pub current_generation: i64,
    /// `review_authority` is still `act`.
    pub authority_act: bool,
    /// The hold set has been read this process (fail closed when false).
    pub hold_known: bool,
    /// A hold is applied.
    pub hold_applied: bool,
    /// The pull request is still open.
    pub pr_open: bool,
    /// The manager is still enabled.
    pub manager_enabled: bool,
    /// The evidence moved such that §8.2 would treat the decision as `stale`.
    pub evidence_stale: bool,
}

/// §8.3: whether an effect may proceed. The order refuses the most terminal conditions first; an
/// unknown hold set fails closed.
pub fn pre_effect_check(inputs: PreEffectInputs) -> PreEffectCheck {
    if inputs.current_generation != inputs.generation
        || !inputs.authority_act
        || !inputs.pr_open
        || !inputs.manager_enabled
        || !inputs.hold_known
        || inputs.hold_applied
    {
        return PreEffectCheck::Superseded;
    }
    if inputs.evidence_stale {
        return PreEffectCheck::Stale;
    }
    PreEffectCheck::Proceed
}

impl Orchestrator {
    /// The control tick's apply pass (§7.6, §7.7): begin applying validated decisions, recover
    /// `applying` rows, and complete/timed-out `awaiting_effect` rows. Called from
    /// [`Orchestrator::pump_manager_interventions`] right after the saved-decision revalidation.
    pub(crate) fn pump_manager_applying(&mut self) {
        if self.manager_review_authority() != ReviewAuthority::Act {
            return;
        }
        let Ok(rows) = self.store().load_manager_interventions() else {
            return;
        };
        for row in rows {
            if row.state == MANAGER_INTERVENTION_AWAITING_EFFECT {
                self.check_manager_completion(&row);
            }
        }
        // A second pass so a row that BEGINS applying is re-read with its `applying` state; the
        // begin/recover halves are separated from completion so a row cannot be both in one tick.
        let Ok(rows) = self.store().load_manager_interventions() else {
            return;
        };
        for row in rows {
            match row.state.as_str() {
                rhapsody_store::MANAGER_INTERVENTION_VALIDATED => self.begin_manager_apply(&row),
                MANAGER_INTERVENTION_APPLYING => self.recover_manager_apply(&row),
                _ => {}
            }
        }
    }

    /// §7.9: whether ordinary selection must skip `issue` because it has an UNSPENT manager wake
    /// obligation (`pending`, or `admitted` and not yet `delivered`). Until M10 lands the admission
    /// loop, this is what guarantees no route-back dispatches without its seed.
    ///
    /// Only meaningful in `act` mode; `off`/`advise` never write a wake row. Fails CLOSED on a store
    /// read error in `act` mode — a wake row this process cannot read might be the one owed.
    pub(crate) fn manager_wake_blocks_selection(&self, issue_id: &str) -> bool {
        if self.manager_review_authority() != ReviewAuthority::Act || issue_id.is_empty() {
            return false;
        }
        self.store()
            .manager_wake_unspent_for_issue(issue_id)
            .unwrap_or(true)
    }

    /// §8.3: answer the applier's cheap check before an external effect. The decisive check is the
    /// activation transaction (§7.7); this only stops early to save work.
    pub(crate) fn manager_pre_effect_check(&self, intervention_id: &str) -> PreEffectCheck {
        let Some(row) = self
            .store()
            .manager_intervention(intervention_id)
            .ok()
            .flatten()
        else {
            return PreEffectCheck::Superseded;
        };
        // A re-sent request for a row that has moved on (activated, refused, terminal) must not
        // perform an effect. Only an `applying` row is mid-flight; a best-effort notice skips this
        // check entirely (runmanagerapply.rs).
        if row.state != MANAGER_INTERVENTION_APPLYING {
            return PreEffectCheck::Superseded;
        }
        let bound = self.store().review_bound(&row.pr).ok().flatten();
        let (labelled, hold_known) = self.human_holds.labelled_and_primed();
        let hold_applied = self.manager_pr_held(&row.pr, &labelled);
        let authority_act = self.manager_review_authority() == ReviewAuthority::Act;
        let pr_open = self
            .store()
            .load_live_review_watch()
            .map(|rows| {
                rows.iter().any(|r| {
                    r.open
                        && format!("{}/{}#{}", r.key.owner, r.key.repo, r.key.number)
                            .to_ascii_lowercase()
                            == row.pr
                })
            })
            .unwrap_or(false);
        let manager_enabled = self.teams.as_ref().is_some_and(|t| t.enabled);
        // §8.3 limits the evidence-staleness arm to APPROVE and ROUTE_TO_AUTHOR: a `RERUN_REVIEW`
        // whose evidence moved (for example a finding resolved) is not stopped by this cheap check,
        // and the decisive activation transaction is where it is judged.
        let evidence_checked = self.manager_stored_decision(&row).is_some_and(|d| {
            matches!(
                d.kind,
                DecisionKind::Approve | DecisionKind::RouteToAuthor { .. }
            )
        });
        let evidence_stale = evidence_checked
            && bound
                .as_ref()
                .is_some_and(|b| b.evidence_rev != row.decision_evidence_rev);
        pre_effect_check(PreEffectInputs {
            generation: row.generation,
            current_generation: bound.as_ref().map_or(row.generation, |b| b.generation),
            authority_act,
            hold_known,
            hold_applied,
            pr_open,
            manager_enabled,
            evidence_stale,
        })
    }

    /// §7.6: a validated decision begins applying. The mandatory effects are planned and marked
    /// `pending` locally (control-task writes), the pending approval record is written for an
    /// `APPROVE`, and the off-loop applier is handed the request. `ESCALATE` skips effects: its
    /// question is best effort and it activates immediately.
    pub(crate) fn begin_manager_apply(&mut self, row: &ManagerInterventionRow) {
        if row.mode != rhapsody_store::MANAGER_MODE_ACT {
            return;
        }
        let Some(decision) = self.manager_stored_decision(row) else {
            return;
        };
        if let DecisionKind::Escalate { question, checked } = &decision.kind {
            self.manager_escalate(row, question, checked);
            return;
        }
        let effects: Vec<ManagerEffect> = plan_effects(&decision)
            .into_iter()
            .map(|effect| ManagerEffect {
                effect: effect.to_string(),
                state: MANAGER_EFFECT_PENDING.to_string(),
            })
            .collect();
        // The memory mirror is owed from here on: it is written LAST, best effort, and any failure
        // leaves `pending` for a retry (§11.3). Set in the SAME row write so the whole-row upsert
        // cannot overwrite it back to empty.
        let memory_state = if row.memory_state.is_empty() {
            "pending".to_string()
        } else {
            row.memory_state.clone()
        };
        if let Err(e) = self
            .store()
            .save_manager_intervention(ManagerInterventionRow {
                state: MANAGER_INTERVENTION_APPLYING.to_string(),
                effects_json: render_effects(&effects),
                memory_state,
                ..row.clone()
            })
        {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: moving the intervention to applying failed");
            return;
        }
        // The APPROVE's local record is `pending` until activation makes it `effective` (§6.5).
        if matches!(decision.kind, DecisionKind::Approve)
            && let Some(approval) = self.manager_pending_approval(row, &decision)
            && let Err(e) = self.store().save_manager_approval(approval)
        {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: writing the pending approval failed");
        }
        let request = self.manager_apply_request(row, &decision, &effects);
        if self.manager_apply.is_none() {
            tracing::warn!(pr = %row.pr, id = %row.id,
                "manager apply: no applier is installed; the effects will be retried on recovery");
        }
        // Through the dedupe helper, so the next tick's `recover_manager_apply` does not submit the
        // same mandatory effects a second time.
        self.submit_manager_apply(request);
    }

    /// §7.5/§7.7 recovery: an `applying` row is reconciled and then run through THE SAME activation
    /// transaction with THE SAME full revalidation. A confirmed effect is a prerequisite, never
    /// permission on its own. Effects still unresolved are re-submitted (the applier's marker search
    /// makes that idempotent); when every mandatory effect is done, activation is attempted.
    pub(crate) fn recover_manager_apply(&mut self, row: &ManagerInterventionRow) {
        if row.mode != rhapsody_store::MANAGER_MODE_ACT {
            return;
        }
        let effects = parse_effects(&row.effects_json);
        if all_effects_done(&effects) {
            self.manager_try_activate(row);
            return;
        }
        if effect_failure(&effects).is_some() {
            return; // a terminal failure is already recorded; do not re-run effects
        }
        let Some(decision) = self.manager_stored_decision(row) else {
            return;
        };
        let request = self.manager_apply_request(row, &decision, &effects);
        self.submit_manager_apply(request);
    }

    /// Submit one request to the off-loop applier, at most once per process per (intervention,
    /// effect-set), so an `applying` row is not re-posted on every tick while its effects are in
    /// flight. A restart starts with an empty set, which is how recovery re-submits after a crash
    /// (the applier's marker search makes the duplicate harmless). Keying on the effect set keeps a
    /// best-effort escalation/unapplied notice distinct from the mandatory effects.
    fn submit_manager_apply(&mut self, request: ManagerApplyRequest) {
        let key = format!("{}:{}", request.intervention_id, request.effects.join(","));
        if self.manager_apply_submitted.contains(&key) {
            return;
        }
        let Some(sink) = &self.manager_apply else {
            return; // no applier: effects are retried on a later recovery, never inline
        };
        self.manager_apply_submitted.insert(key);
        sink.submit(request);
    }

    /// Fold an applier's report back into the local record and, when every mandatory effect is done,
    /// attempt the activation transaction.
    pub(crate) fn handle_manager_effect(&mut self, result: &ManagerEffectResult) {
        let Some(row) = self
            .store()
            .manager_intervention(&result.intervention_id)
            .ok()
            .flatten()
        else {
            return;
        };
        if row.state != MANAGER_INTERVENTION_APPLYING {
            return;
        }
        // §8.3: a pre-effect check stopped the run. The decisive refusal is the activation
        // transaction, which cancels every pending record and records the unapplied explanation.
        if let Some(halted) = &result.halted {
            let verdict = if halted == "stale" {
                ManagerActivationVerdict::Stale
            } else {
                ManagerActivationVerdict::Superseded
            };
            if let Some(decision) = self.manager_stored_decision(&row) {
                let effects = parse_effects(&row.effects_json);
                let request = self.manager_activation_request(&row, &decision, verdict, &effects);
                if let Err(e) = self.store().activate_manager_intervention(request) {
                    tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                        "manager apply: refusing activation after a pre-effect check failed");
                }
            }
            return;
        }
        let mut effects = parse_effects(&row.effects_json);
        for (effect, state) in &result.outcomes {
            if let Some(slot) = effects.iter_mut().find(|e| &e.effect == effect) {
                slot.state = state.clone();
            }
        }
        let stored = ManagerInterventionRow {
            effects_json: render_effects(&effects),
            ..row.clone()
        };
        if let Err(e) = self.store().save_manager_intervention(stored.clone()) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: recording effect results failed");
            return;
        }
        // A definitive failure or an unconfirmable effect is terminal and stops the generation: it
        // never reaches activation, and its pending records never become effective (§7.6).
        if let Some(kind) = effect_failure(&effects) {
            let (state, reason) = if kind == MANAGER_EFFECT_FAILED {
                (
                    MANAGER_INTERVENTION_APPLY_FAILED,
                    format!("an effect failed: {}", result.reason),
                )
            } else {
                (
                    MANAGER_INTERVENTION_APPLY_UNCERTAIN,
                    format!("an effect could not be confirmed: {}", result.reason),
                )
            };
            self.finish_manager_apply_failure(&stored, state, &reason);
            return;
        }
        if all_effects_done(&effects) {
            self.manager_try_activate(&stored);
        }
    }

    /// The activation transaction (§7.7): compute the full revalidation NOW, build the activation
    /// request, and commit it atomically. A refusal still goes through the transaction so its
    /// pending records are cancelled and the unapplied explanation recorded.
    pub(crate) fn manager_try_activate(&mut self, row: &ManagerInterventionRow) {
        let Some(decision) = self.manager_stored_decision(row) else {
            self.record_manager_failed_attempt(row, "the stored decision could not be re-parsed");
            return;
        };
        let verdict = match self.revalidate_manager_decision(row, &decision) {
            Revalidation::StillValid => ManagerActivationVerdict::Pass,
            Revalidation::Complete => {
                // The effect is moot (for example a RERUN whose every row is now approved). No
                // pending record takes effect.
                self.set_manager_state(row, MANAGER_INTERVENTION_COMPLETE);
                self.record_manager_outcome_if_absent(row, "unconfirmed");
                return;
            }
            Revalidation::Stale => ManagerActivationVerdict::Stale,
            Revalidation::Superseded => ManagerActivationVerdict::Superseded,
        };
        let effects = parse_effects(&row.effects_json);
        let request = self.manager_activation_request(row, &decision, verdict, &effects);
        match self.store().activate_manager_intervention(request) {
            Ok(ManagerActivationOutcome::Activated) => {
                if verdict == ManagerActivationVerdict::Pass {
                    self.manager_write_memory(row);
                }
            }
            Ok(ManagerActivationOutcome::Refused) => {
                tracing::warn!(pr = %row.pr, id = %row.id,
                    "manager apply: activation refused; the decision was not applied");
                self.manager_record_unapplied(row, &decision);
                // `superseded` is terminal (§7.2); a `stale` refusal re-queues, so recording an
                // outcome for it would pin a row that is about to be re-planned.
                if verdict == ManagerActivationVerdict::Superseded {
                    self.record_manager_outcome_if_absent(row, "superseded");
                }
            }
            Ok(ManagerActivationOutcome::Absent) => {}
            Err(e) => {
                // Fail closed: a store that cannot commit the activation applies nothing.
                tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                    "manager apply: the activation transaction failed; nothing was applied");
            }
        }
    }

    /// §7.6 `ESCALATE`: a best-effort question comment, the item on the human feed, and the
    /// generation stopped. An escalation activates nothing.
    fn manager_escalate(&mut self, row: &ManagerInterventionRow, question: &str, checked: &str) {
        let marker = manager_explanation_marker(&row.id, MANAGER_EFFECT_ESCALATION);
        let body = strip_summon_tokens(&format!(
            "Manager escalation.\nQuestion: {question}\nChecked: {checked}\n\n{marker}"
        ));
        let (owner, repo, number) = crate::managerintervention::parse_pr_key(&row.pr)
            .map(|c| (c.owner, c.repo, c.number))
            .unwrap_or_default();
        let escalate_request = ManagerApplyRequest {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            owner,
            repo,
            number,
            effects: vec![MANAGER_EFFECT_ESCALATION.to_string()],
            explanation: body,
            marker,
            ticket_move: None,
        };
        self.submit_manager_apply(escalate_request);
        let request = ManagerActivation {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            generation: row.generation,
            now: (self.now)().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            verdict: ManagerActivationVerdict::Pass,
            escalate: Some(format!("manager escalated: {question}")),
            ..ManagerActivation::default()
        };
        if let Err(e) = self.store().activate_manager_intervention(request) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: recording the escalation failed");
        }
    }

    /// §7.6: a terminal effect failure ends the intervention and stops the generation.
    fn finish_manager_apply_failure(
        &mut self,
        row: &ManagerInterventionRow,
        state: &str,
        reason: &str,
    ) {
        if let Err(e) = self
            .store()
            .stop_manager_intervention(&row.id, state, reason)
        {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: stopping the generation after an effect failure failed");
        }
        self.record_manager_outcome_if_absent(row, "unconfirmed");
    }

    /// §6.6 completion: an `awaiting_effect` intervention completes when its effect condition is met
    /// or its timeout passes. This owns every decision's completion: the APPROVE expiry, the
    /// reviewer-driven `RERUN_REVIEW`/`ROUTE_TO_AUTHOR` conditions, and the APPROVE D7 timeout.
    fn check_manager_completion(&mut self, row: &ManagerInterventionRow) {
        let Some(decision) = self.manager_stored_decision(row) else {
            return;
        };
        // APPROVE: the approval expiring or being cancelled completes the intervention (§6.6).
        if matches!(decision.kind, DecisionKind::Approve) {
            match self.store().manager_approval(&row.id) {
                Ok(Some(a)) if a.state == MANAGER_APPROVAL_EFFECTIVE => {}
                Ok(_) => {
                    self.set_manager_state(row, MANAGER_INTERVENTION_COMPLETE);
                    self.record_manager_outcome_if_absent(row, "unconfirmed");
                    return;
                }
                Err(_) => return, // fail closed: do not complete on an unreadable store
            }
        }
        // §6.6: a `RERUN_REVIEW`/`ROUTE_TO_AUTHOR` completes when its effect condition is met. An
        // outcome is left unset for the PR-state watcher to record `merged`/`closed_unmerged`.
        if matches!(
            decision.kind,
            DecisionKind::RerunReview { .. } | DecisionKind::RouteToAuthor { .. }
        ) && self.manager_effect_complete(row, &decision)
        {
            self.set_manager_state(row, MANAGER_INTERVENTION_COMPLETE);
            return;
        }
        let timeout = manager_effect_timeout(&decision);
        let Some(activated) = DateTime::parse_from_rfc3339(&row.activated_at)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
        else {
            return;
        };
        if (self.now)() - activated >= timeout {
            // §6.6: an APPROVE still unmerged at its timeout WITH the approval still `effective` is
            // the D7-blocked signature this control task can observe — the draft/conflict/CI gates
            // live in the off-loop merge half, which read them from GitHub. The item goes to the
            // human feed and the generation stops rather than recording a bare timeout.
            if matches!(decision.kind, DecisionKind::Approve) {
                self.finish_manager_apply_failure(
                    row,
                    MANAGER_INTERVENTION_ESCALATED,
                    "manager approval did not merge before its 2 h timeout (a merge gate still \
                     blocks); the generation is stopped for a human",
                );
                return;
            }
            self.set_manager_state(row, MANAGER_INTERVENTION_EFFECT_TIMEOUT);
            self.record_manager_outcome_if_absent(row, "unconfirmed");
            tracing::warn!(pr = %row.pr, id = %row.id,
                "manager apply: the decision's effect timed out");
        }
    }

    /// §6.6's reviewer-driven completion. `RERUN_REVIEW` is complete when the patch-id moves, or
    /// every re-requested row's round has finished (a completed verdict, `truncated` or `dropped`).
    /// `ROUTE_TO_AUTHOR` is complete when the author pushes a new patch-id AND the following review
    /// round completes at it.
    ///
    /// A re-requested row is `requested` when the activation transaction arms it, so "finished" is
    /// the row having LEFT `requested`/`in_flight` — read from `status`, which the watcher moves as
    /// the round actually runs. (`last_completed` alone cannot say: the row already carried a
    /// completion for the same code from before the re-request.) The `ROUTE_TO_AUTHOR` half that
    /// waits on "the author's run ends without a push" depends on the wake obligation's run, which
    /// M10 owns; until then an un-pushed route is bounded by the 4 h timeout.
    fn manager_effect_complete(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
    ) -> bool {
        let rows = self.manager_review_rows(&row.pr);
        let current_patch = crate::managerintervention::current_patch_id(&rows);
        let patch_moved = !row.activation_patch_id.is_empty()
            && !current_patch.is_empty()
            && current_patch != row.activation_patch_id;
        match &decision.kind {
            DecisionKind::RerunReview { .. } => {
                if patch_moved {
                    return true;
                }
                if row.rerequested.is_empty() {
                    return false;
                }
                row.rerequested.iter().all(|reviewer| {
                    // A row absent from the live watch set is dropped or otherwise gone — finished
                    // for this purpose (§6.6 counts `dropped`).
                    match rows.iter().find(|r| &r.reviewer == reviewer) {
                        None => true,
                        Some(r) => !matches!(
                            r.status.as_str(),
                            REVIEW_STATUS_REQUESTED | REVIEW_STATUS_IN_FLIGHT
                        ),
                    }
                })
            }
            DecisionKind::RouteToAuthor { .. } => {
                if !patch_moved {
                    return false;
                }
                let live = managerdecision::live_reviewer_rows(&rows);
                !live.is_empty()
                    && live.iter().all(|r| {
                        r.completed
                            .as_ref()
                            .is_some_and(|c| !c.patch_id.is_empty() && c.patch_id == current_patch)
                    })
            }
            _ => false,
        }
    }

    /// §11.2: set an intervention's outcome once. Never fails the decision.
    pub(crate) fn record_manager_outcome_if_absent(
        &self,
        row: &ManagerInterventionRow,
        outcome: &str,
    ) {
        let now = (self.now)().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if let Err(e) = self.store().record_manager_outcome(&row.id, outcome, &now) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: recording the outcome failed");
        }
    }

    /// §11.3: the memory mirror is written LAST, best effort. A failure leaves the decision applied
    /// and `memory_state = pending` for a retry — nothing is re-run or reverted.
    ///
    /// The mirror lands in the manager's OWN bank (`agent-manager`, STUDIO-1013): a local append is
    /// the only backend the control task can reach without awaiting, and the record is prose
    /// describing what the daemon applied, never a transcript.
    fn manager_write_memory(&self, row: &ManagerInterventionRow) {
        let Some(bank) = self.teams_bank.as_ref() else {
            // No memory backend is configured, so the mirror cannot be written and the local record
            // correctly stays `pending`. The decision itself is already applied and is NOT reverted.
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
            document_id: format!("intervention-{}", row.id),
            ticket: self.manager_pr_ticket(&row.pr).unwrap_or_default(),
            commit_sha: String::new(),
            pr: row.pr.clone(),
            run_id: row.run_id.map(|r| r.to_string()).unwrap_or_default(),
            at: (self.now)(),
            content: format!(
                "Applied manager decision on {}: {}",
                row.pr,
                manager_summary_prose(row)
            ),
        };
        let state = match bank.retain_shared(&bank_id, &record) {
            Ok(_) => "done",
            Err(e) => {
                // Best effort: a memory failure never re-runs or reverts anything (§11.3).
                tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                    "manager apply: the memory mirror failed; the decision stands and memory_state \
                     stays pending");
                "pending"
            }
        };
        if let Err(e) = self.store().set_manager_memory_state(&row.id, state) {
            tracing::warn!(pr = %row.pr, id = %row.id, err = %e,
                "manager apply: writing the memory state failed");
        }
    }

    /// §7.7: report the already-posted explanation on the human feed as unapplied, and post a
    /// best-effort "not applied: <reason>" follow-up on the pull request.
    fn manager_record_unapplied(
        &mut self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
    ) {
        tracing::warn!(pr = %row.pr, id = %row.id,
            "manager apply: the posted explanation is unapplied; the decision did not take effect");
        let reason = manager_unapplied_reason(decision);
        let marker = manager_explanation_marker(&row.id, MANAGER_EFFECT_UNAPPLIED);
        let body = strip_summon_tokens(&format!(
            "Not applied: {reason}\n(decision {})\n\n{marker}",
            decision.variant()
        ));
        let (owner, repo, number) = crate::managerintervention::parse_pr_key(&row.pr)
            .map(|c| (c.owner, c.repo, c.number))
            .unwrap_or_default();
        let unapplied_request = ManagerApplyRequest {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            owner,
            repo,
            number,
            effects: vec![MANAGER_EFFECT_UNAPPLIED.to_string()],
            explanation: body,
            marker,
            ticket_move: None,
        };
        self.submit_manager_apply(unapplied_request);
    }

    // --- request builders ------------------------------------------------------------------------

    /// Re-parse an intervention's stored decision block (a fenced body). `None` when it cannot be
    /// re-parsed, which the caller treats as a failed attempt.
    fn manager_stored_decision(&self, row: &ManagerInterventionRow) -> Option<ManagerDecision> {
        let wrapped = format!(
            "```{}\n{}\n```",
            managerdecision::MANAGER_DECISION_TAG,
            row.decision_json
        );
        let known = self.manager_known_findings(&row.pr);
        managerdecision::parse_stored_decision(&wrapped, &known).ok()
    }

    fn manager_apply_request(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
        effects: &[ManagerEffect],
    ) -> ManagerApplyRequest {
        let marker = manager_explanation_marker(&row.id, MANAGER_EFFECT_EXPLANATION);
        let explanation = manager_explanation_body(decision, &marker);
        let (owner, repo, number) = crate::managerintervention::parse_pr_key(&row.pr)
            .map(|c| (c.owner, c.repo, c.number))
            .unwrap_or_default();
        let pending: Vec<String> = effects
            .iter()
            .filter(|e| e.state != MANAGER_EFFECT_DONE)
            .map(|e| e.effect.clone())
            .collect();
        let ticket_move = if matches!(decision.kind, DecisionKind::RouteToAuthor { .. }) {
            self.manager_ticket_move(row)
        } else {
            None
        };
        ManagerApplyRequest {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            owner,
            repo,
            number,
            effects: pending,
            explanation,
            marker,
            ticket_move,
        }
    }

    /// The ticket move for a `ROUTE_TO_AUTHOR`, resolved on the control task (the off-loop applier
    /// makes one call and has no decision left to get wrong) — the same resolution the daemon's own
    /// route-back uses.
    fn manager_ticket_move(&self, row: &ManagerInterventionRow) -> Option<ManagerTicketMove> {
        let identifier = self.manager_pr_ticket(&row.pr)?;
        let plan = self.resolve_route_back(&row.pr, &identifier)?;
        Some(ManagerTicketMove {
            issue_id: plan.issue_id,
            team_id: plan.team_id,
            state: plan.state,
        })
    }

    /// The pending approval record for an `APPROVE` (§6.5). It is `pending` until activation makes
    /// it `effective`; it records what the decision was made against and the rows it stands in for.
    fn manager_pending_approval(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
    ) -> Option<ManagerApprovalRow> {
        let rows = self.manager_review_rows(&row.pr);
        let patch_id = crate::managerintervention::current_patch_id(&rows);
        let covered: Vec<String> = managerdecision::eligible_rows(&rows, row.generation, &patch_id)
            .iter()
            .map(|r| r.reviewer.clone())
            .collect();
        let membership = crate::managerapproval::membership_hash(
            &managerdecision::live_reviewer_rows(&rows)
                .iter()
                .map(|r| r.reviewer.clone())
                .collect::<Vec<_>>(),
        );
        Some(ManagerApprovalRow {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            generation: row.generation,
            head: decision.head.clone(),
            patch_id,
            evidence_rev: decision.evidence_rev,
            covered_reviewers: covered,
            membership_hash: membership,
            state: MANAGER_APPROVAL_PENDING.to_string(),
        })
    }

    /// Build the activation transaction's request for `verdict` (§7.7). The writes are populated
    /// only for a passing verdict; a refusal carries just the rescind reason.
    fn manager_activation_request(
        &self,
        row: &ManagerInterventionRow,
        decision: &ManagerDecision,
        verdict: ManagerActivationVerdict,
        effects: &[ManagerEffect],
    ) -> ManagerActivation {
        let now = (self.now)().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let explanation_posted = effects
            .iter()
            .any(|e| e.effect == MANAGER_EFFECT_EXPLANATION && e.state == MANAGER_EFFECT_DONE);
        let mut request = ManagerActivation {
            intervention_id: row.id.clone(),
            pr: row.pr.clone(),
            generation: row.generation,
            now,
            verdict,
            explanation_posted,
            rescind_reason: manager_unapplied_reason(decision),
            ..ManagerActivation::default()
        };
        if verdict != ManagerActivationVerdict::Pass {
            return request;
        }
        let rows = self.manager_review_rows(&row.pr);
        let patch_id = crate::managerintervention::current_patch_id(&rows);
        // §6.6's completion reads both back: the patch-id to detect a move, and the exact
        // re-requested set so an already-approved row cannot hold the decision open.
        request.activation_patch_id = patch_id.clone();
        request.dismissals = decision
            .dismiss
            .iter()
            .map(|d| (d.finding.finding.clone(), d.finding.revision))
            .collect();
        match &decision.kind {
            DecisionKind::Approve => {
                request.approval = self.manager_pending_approval(row, decision);
            }
            DecisionKind::RerunReview { reviewers, .. } => {
                request.rerequest =
                    managerdecision::eligible_rows(&rows, row.generation, &patch_id)
                        .iter()
                        .filter(|r| {
                            reviewers.is_empty() || reviewers.iter().any(|x| x == &r.reviewer)
                        })
                        .map(|r| r.reviewer.clone())
                        .collect();
                request.rerequested = request.rerequest.clone();
            }
            DecisionKind::RouteToAuthor { .. } => {
                let body = manager_route_wake_body(decision);
                // Prefer the OPAQUE tracker issue id (what ordinary selection keys by), resolved
                // from the ticket's own run history; fall back to the identifier, which selection
                // also checks and which is all a daemon with no run row in history can name.
                let issue_id = self
                    .manager_ticket_move(row)
                    .map(|m| m.issue_id)
                    .filter(|id| !id.is_empty())
                    .or_else(|| self.manager_pr_ticket(&row.pr))
                    .unwrap_or_default();
                request.wake = Some(ManagerWakeRow {
                    intervention_id: row.id.clone(),
                    pr: row.pr.clone(),
                    generation: row.generation,
                    issue_id,
                    body,
                    state: MANAGER_WAKE_PENDING.to_string(),
                    run_id: None,
                    reason: String::new(),
                });
            }
            DecisionKind::Escalate { .. } => {}
        }
        // The exchange authorization is written ONLY for a non-final, post-threshold decision
        // (§7.8). The final intervention, `APPROVE` and `ESCALATE` create none.
        let post_threshold = self.manager_post_threshold(row);
        if post_threshold && !row.is_final {
            let kind = match decision.kind {
                DecisionKind::RerunReview { .. } => MANAGER_EXCHANGE_REVIEW_ROUND,
                DecisionKind::RouteToAuthor { .. } => MANAGER_EXCHANGE_AUTHOR_ROUND,
                _ => "",
            };
            if !kind.is_empty() {
                request.exchange = Some(ManagerExchange {
                    id: format!("{}-{}", row.id, kind),
                    intervention_id: row.id.clone(),
                    pr: row.pr.clone(),
                    generation: row.generation,
                    kind: kind.to_string(),
                    authorized_head: decision.head.clone(),
                    authorized_patch_id: patch_id,
                    state: MANAGER_EXCHANGE_ACTIVE.to_string(),
                });
            }
        }
        request.reserve_slot = post_threshold;
        request
    }

    /// §7.8's authoritative classification, made AT activation: the later of the launch hint and
    /// the current count of answered exchanges.
    fn manager_post_threshold(&self, row: &ManagerInterventionRow) -> bool {
        let Some(coord) = crate::managerintervention::parse_pr_key(&row.pr) else {
            return false;
        };
        let launched_post = row.phase_hint == rhapsody_store::MANAGER_PHASE_POST_THRESHOLD;
        let now_post = self
            .adjudication_threshold()
            .is_some_and(|t| self.rounds_used(&coord) >= t);
        launched_post || now_post
    }
}

/// §6.6's completion timeout for a decision.
pub fn manager_effect_timeout(decision: &ManagerDecision) -> ChronoDuration {
    match decision.kind {
        DecisionKind::RouteToAuthor { .. } => ChronoDuration::hours(4),
        DecisionKind::RerunReview { .. } | DecisionKind::Approve => ChronoDuration::hours(2),
        DecisionKind::Escalate { .. } => ChronoDuration::zero(),
    }
}

/// The human-facing "not applied: <reason>" wording for a decision (§7.7).
fn manager_unapplied_reason(decision: &ManagerDecision) -> String {
    format!(
        "{} no longer held against current state",
        decision.variant()
    )
}

/// A short prose summary of an applied decision, for the memory mirror (§11.3). Derived from the
/// stored decision block so the record names what was actually applied.
fn manager_summary_prose(row: &ManagerInterventionRow) -> String {
    let variant = managerdecision::parse_stored_decision(
        &format!(
            "```{}\n{}\n```",
            managerdecision::MANAGER_DECISION_TAG,
            row.decision_json
        ),
        &[],
    )
    .map(|d| d.variant().to_string())
    .unwrap_or_else(|_| "decision".to_string());
    format!("{variant} applied ({})", row.id)
}

/// The wake obligation's seed body for `ROUTE_TO_AUTHOR` (§7.9): the route instructions and the
/// findings it names, taken from the VALIDATED decision, never from a comment.
fn manager_route_wake_body(decision: &ManagerDecision) -> String {
    let DecisionKind::RouteToAuthor { fix, instructions } = &decision.kind else {
        return String::new();
    };
    let mut out = format!("Manager route-to-author.\nInstructions: {instructions}\nFindings:\n");
    for f in fix {
        out.push_str(&format!("- {} @ r{}\n", f.finding, f.revision));
    }
    strip_summon_tokens(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managerdecision::Dismissal;

    fn decision(kind: DecisionKind) -> ManagerDecision {
        ManagerDecision {
            head: "deadbeef".to_string(),
            evidence_rev: 0,
            dismiss: Vec::new(),
            rationale: "because".to_string(),
            kind,
        }
    }

    // The explanation is mandatory for a RERUN_REVIEW with dismissals and NO note (§15.4): the
    // explanation is still rendered, and it names every dismissed finding by id and revision.
    // MUTATION: render the note INSTEAD of the explanation (skip the body when the note is absent)
    // and the dismissal/decision asserts red.
    #[test]
    fn a_rerun_with_dismissals_and_no_note_still_renders_the_explanation() {
        let mut d = decision(DecisionKind::RerunReview {
            reviewers: Vec::new(),
            note: None,
        });
        d.dismiss.push(Dismissal {
            finding: managerdecision::FindingRef {
                finding: "alice:F1".to_string(),
                revision: 3,
            },
            rationale: "superseded by the refactor".to_string(),
        });
        let marker = manager_explanation_marker("iv-1", MANAGER_EFFECT_EXPLANATION);
        let body = manager_explanation_body(&d, &marker);
        assert!(body.contains("RERUN_REVIEW"), "{body}");
        assert!(body.contains("because"), "{body}");
        assert!(body.contains("alice:F1 @ r3"), "{body}");
        assert!(body.contains("superseded by the refactor"), "{body}");
        assert!(body.contains(&marker), "{body}");
    }

    // The optional note is CONTENT INSIDE the explanation, never a substitute for it.
    #[test]
    fn a_rerun_note_is_content_inside_the_mandatory_explanation() {
        let d = decision(DecisionKind::RerunReview {
            reviewers: vec!["alice".to_string()],
            note: Some("check the shutdown path".to_string()),
        });
        let marker = manager_explanation_marker("iv-2", MANAGER_EFFECT_EXPLANATION);
        let body = manager_explanation_body(&d, &marker);
        assert!(body.contains("Rationale: because"), "{body}");
        assert!(body.contains("check the shutdown path"), "{body}");
    }

    // `ROUTE_TO_AUTHOR` carries the fix findings and the instructions.
    #[test]
    fn a_route_explanation_names_the_fix_and_instructions() {
        let d = decision(DecisionKind::RouteToAuthor {
            fix: vec![managerdecision::FindingRef {
                finding: "jimmy:B5".to_string(),
                revision: 1,
            }],
            instructions: "fix the leak".to_string(),
        });
        let marker = manager_explanation_marker("iv-3", MANAGER_EFFECT_EXPLANATION);
        let body = manager_explanation_body(&d, &marker);
        assert!(body.contains("jimmy:B5 @ r1"), "{body}");
        assert!(body.contains("fix the leak"), "{body}");
    }

    // NO manager comment carries a summon token (§7.9): a token the model's own rationale happened
    // to contain is stripped before the body is ever posted. MUTATION: drop the strip and a token in
    // the rationale survives.
    #[test]
    fn a_summon_token_in_the_rationale_is_stripped() {
        let mut d = decision(DecisionKind::Approve);
        d.rationale = format!("please {}", rhapsody_core::SUMMON_TOKEN_SYMPHONY);
        let marker = manager_explanation_marker("iv-4", MANAGER_EFFECT_EXPLANATION);
        let body = manager_explanation_body(&d, &marker);
        assert!(
            !body.contains(rhapsody_core::SUMMON_TOKEN_SYMPHONY),
            "a manager comment must never carry a summon token: {body}"
        );
        assert!(
            !body.contains(rhapsody_core::SUMMON_TOKEN_RHAPSODY),
            "nor the rhapsody token: {body}"
        );
    }

    #[test]
    fn planned_effects_match_the_decision_variant() {
        assert_eq!(
            plan_effects(&decision(DecisionKind::Approve)),
            vec![MANAGER_EFFECT_EXPLANATION]
        );
        assert_eq!(
            plan_effects(&decision(DecisionKind::RerunReview {
                reviewers: Vec::new(),
                note: None
            })),
            vec![MANAGER_EFFECT_EXPLANATION]
        );
        assert_eq!(
            plan_effects(&decision(DecisionKind::RouteToAuthor {
                fix: Vec::new(),
                instructions: "x".to_string()
            })),
            vec![MANAGER_EFFECT_EXPLANATION, MANAGER_EFFECT_TICKET_MOVE]
        );
        assert!(
            plan_effects(&decision(DecisionKind::Escalate {
                question: "q".to_string(),
                checked: "c".to_string()
            }))
            .is_empty()
        );
    }

    #[test]
    fn effects_round_trip_and_gate_activation() {
        let effects = vec![
            ManagerEffect {
                effect: MANAGER_EFFECT_EXPLANATION.to_string(),
                state: MANAGER_EFFECT_DONE.to_string(),
            },
            ManagerEffect {
                effect: MANAGER_EFFECT_TICKET_MOVE.to_string(),
                state: MANAGER_EFFECT_PENDING.to_string(),
            },
        ];
        let json = render_effects(&effects);
        assert_eq!(parse_effects(&json), effects);
        assert!(
            !all_effects_done(&effects),
            "a pending effect gates activation"
        );
        let mut done = effects.clone();
        done[1].state = MANAGER_EFFECT_DONE.to_string();
        assert!(all_effects_done(&done));
        assert_eq!(effect_failure(&done), None);
    }

    #[test]
    fn an_empty_or_unreadable_effects_column_is_no_effects() {
        assert!(parse_effects("").is_empty());
        assert!(parse_effects("not json").is_empty());
    }

    #[test]
    fn timeouts_match_the_design_table() {
        assert_eq!(
            manager_effect_timeout(&decision(DecisionKind::RerunReview {
                reviewers: Vec::new(),
                note: None
            })),
            ChronoDuration::hours(2)
        );
        assert_eq!(
            manager_effect_timeout(&decision(DecisionKind::RouteToAuthor {
                fix: Vec::new(),
                instructions: String::new()
            })),
            ChronoDuration::hours(4)
        );
        assert_eq!(
            manager_effect_timeout(&decision(DecisionKind::Approve)),
            ChronoDuration::hours(2)
        );
    }

    // §8.3: the applier's cheap check before EACH effect. A hold, a generation change, an authority
    // change, a closed PR or a disabled manager stops it (`superseded`); moved evidence stops it
    // (`stale`). MUTATION: answer `Proceed` for a hold and the hold case reds.
    #[test]
    fn the_pre_effect_check_stops_a_revoked_decision() {
        let ok = PreEffectInputs {
            generation: 1,
            current_generation: 1,
            authority_act: true,
            hold_known: true,
            hold_applied: false,
            pr_open: true,
            manager_enabled: true,
            evidence_stale: false,
        };
        assert_eq!(pre_effect_check(ok), PreEffectCheck::Proceed);
        let superseded = [
            PreEffectInputs {
                current_generation: 2,
                ..ok
            },
            PreEffectInputs {
                authority_act: false,
                ..ok
            },
            PreEffectInputs {
                hold_known: false,
                ..ok
            },
            PreEffectInputs {
                hold_applied: true,
                ..ok
            },
            PreEffectInputs {
                pr_open: false,
                ..ok
            },
            PreEffectInputs {
                manager_enabled: false,
                ..ok
            },
        ];
        for input in superseded {
            assert_eq!(pre_effect_check(input), PreEffectCheck::Superseded);
        }
        assert_eq!(
            pre_effect_check(PreEffectInputs {
                evidence_stale: true,
                ..ok
            }),
            PreEffectCheck::Stale
        );
    }
}
