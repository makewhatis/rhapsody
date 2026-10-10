//! Noop — the disabled Store used when persistence is off (`storage.path: off`).
//!
//! Port of Go `noopStore` (`internal/store/noop.go`): every write is a successful no-op and every
//! read returns empty, so non-daemon paths and the no-store daemon behave identically to having no
//! history. It is the zero-cost default for callers that hold a [`Store`] but were started without
//! storage, which makes ALL call sites guard-free.

use crate::*;

/// The disabled [`Store`]. It never errors and never persists anything (port of Go `Noop()`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Noop;

impl Store for Noop {
    fn release_subject_episode(
        &self,
        _id: i64,
        _kind: &str,
        _reason: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn resolve_lead_escalation(&self, _decision: i64, _reason: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn subject_resume_pending(&self, _subject: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn link_subject_pr(&self, _ticket: &str, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn subject_hold(&self, _subject: &str) -> Result<Option<SubjectHold>, StoreError> {
        Ok(None)
    }
    fn load_subject_holds(&self) -> Result<Vec<SubjectHold>, StoreError> {
        Ok(Vec::new())
    }
    fn park_subject(&self, _hold: &SubjectHold) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn release_subject(&self, _subject: &str, _reason: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn update_hold_snapshot(&self, _id: i64, _snapshot: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn same_head_findings(&self, _key: &ReviewWatchKey, _head: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn allow_review_rerun(&self, _pr: &str, _head: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn request_review_dispute(
        &self,
        _key: &ReviewWatchKey,
        _head: &str,
        _manager_mode: Option<&str>,
        _at: &str,
    ) -> Result<ReviewDisputeOutcome, StoreError> {
        Ok(ReviewDisputeOutcome::Pending)
    }
    fn review_dispute_pending(
        &self,
        _key: &ReviewWatchKey,
        _head: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn notices_enabled(&self) -> bool {
        false
    }
    fn observe_notices(&self, _rows: &[Notice], _at: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn notices(&self) -> Result<Vec<Notice>, StoreError> {
        Ok(Vec::new())
    }
    fn read_notice(&self, _id: i64, _at: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn reserve_lead_run_daily(
        &self,
        _item: i64,
        _pr: &str,
        _max: i64,
        _day: &str,
        _daily_max: i64,
    ) -> Result<LeadRunReservation, StoreError> {
        Ok(LeadRunReservation::Exhausted)
    }
    fn lead_report_count(&self, _key: &str) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn reserve_lead_report(&self, _key: &str, _max: i64) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn reserve_lead_page(&self, _decision: i64) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn overrule_lead_decision(
        &self,
        _id: i64,
        _note: &str,
        _at: &str,
    ) -> Result<Option<i64>, StoreError> {
        Ok(None)
    }
    fn enqueue_lead_item(&self, _trigger: &LeadTrigger, _at: &str) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn retire_resolved_lead_missing_pr(&self, _item: i64) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn load_lead_items(&self) -> Result<Vec<LeadItem>, StoreError> {
        Ok(Vec::new())
    }
    fn load_lead_decisions(&self) -> Result<Vec<LeadDecisionRow>, StoreError> {
        Ok(Vec::new())
    }
    fn lead_digest_entries(&self, _since: &str) -> Result<Vec<LeadDecisionRow>, StoreError> {
        Ok(Vec::new())
    }
    fn lead_execution(&self, _item: i64) -> Result<Option<LeadExecution>, StoreError> {
        Ok(None)
    }
    fn save_lead_execution(&self, _row: &LeadExecution) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_lead_item_state(&self, _item: i64, _state: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn reserve_lead_route_back(&self, _item: i64) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn save_lead_decision(&self, _row: &LeadDecisionRow) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn reserve_lead_run(&self, _item: i64, _pr: &str, _max: i64) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn start_run(&self, _r: RunStart) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn end_run(&self, _run_id: i64, _e: RunEnd) -> Result<(), StoreError> {
        Ok(())
    }
    fn update_run_progress(&self, _run_id: i64, _p: RunProgress) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_run_tokens(&self, _run_id: i64, _t: &RunTokens) -> Result<(), StoreError> {
        Ok(())
    }
    fn append_events(&self, _run_id: i64, _ev: &[EventRow]) -> Result<(), StoreError> {
        Ok(())
    }

    fn save_retry(&self, _r: RetryRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn delete_retry(&self, _issue_id: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn save_claim(
        &self,
        _issue_id: &str,
        _state: &str,
        _project_slug: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn delete_claim(&self, _issue_id: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_recovery(&self) -> Result<Recovery, StoreError> {
        Ok(Recovery::default())
    }
    fn mark_running_interrupted(&self) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn save_totals(&self, _t: Totals) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_totals(&self) -> Result<Totals, StoreError> {
        Ok(Totals::default())
    }

    fn list_runs(&self, _f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        Ok(Vec::new())
    }
    fn list_issue_runs(&self, _f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        Ok(Vec::new())
    }
    fn day_totals(&self, _since: &str, _now: &str) -> Result<DayTotals, StoreError> {
        Ok(DayTotals::default())
    }
    fn issue_history(
        &self,
        _identifier: &str,
        _project: &str,
        _limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        Ok(Vec::new())
    }
    fn runs_for_issues(
        &self,
        _identifiers: &[String],
        _limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        Ok(Vec::new())
    }
    fn get_run(&self, _run_id: i64) -> Result<Option<RunSummary>, StoreError> {
        Ok(None)
    }
    fn run_events(&self, _run_id: i64) -> Result<Vec<EventRow>, StoreError> {
        Ok(Vec::new())
    }
    fn search_events(&self, _q: EventQuery) -> Result<Vec<EventHit>, StoreError> {
        Ok(Vec::new())
    }
    /// `None` — the honest answer for a store that holds nothing: it can vouch for no instant at
    /// all, so a caller that would act on an absence must not act.
    fn earliest_run_start(&self) -> Result<Option<String>, StoreError> {
        Ok(None)
    }
    fn metrics(&self, _since_days: i64, _project: &str) -> Result<Vec<DayRollup>, StoreError> {
        Ok(Vec::new())
    }
    fn metrics_by_provider(
        &self,
        _since_days: i64,
        _project: &str,
    ) -> Result<Vec<DayProviderRollup>, StoreError> {
        Ok(Vec::new())
    }

    // Provenance (STUDIO-909) disappears with the rest of the history: a store that holds nothing
    // has no run to attribute, so it answers "no row" and no per-provider tally.
    fn set_run_provenance(&self, _run_id: i64, _p: &RunProvenance) -> Result<(), StoreError> {
        Ok(())
    }
    fn run_provenance(&self, _run_id: i64) -> Result<Option<RunProvenance>, StoreError> {
        Ok(None)
    }
    fn load_run_provenances(
        &self,
        _run_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, RunProvenance>, StoreError> {
        Ok(std::collections::HashMap::new())
    }
    // Broker usage (STUDIO-987) disappears with the rest of the history: a store that holds nothing
    // has no run to attribute, so it answers "no row".
    fn set_run_usage(&self, _run_id: i64, _u: &RunUsage) -> Result<(), StoreError> {
        Ok(())
    }
    fn run_usage(&self, _run_id: i64) -> Result<Option<RunUsage>, StoreError> {
        Ok(None)
    }
    // Durable UTC-day provider budget (STUDIO-979). The disabled store has no counter to charge, so
    // a charge is a refusal that changes nothing (`Ok(false)`) and the read is zero — never an
    // error, exactly like every other Noop method. A configured day cap over this backend is
    // refused at daemon startup, so this fail-closed answer is not a reachable production path; it
    // exists to keep the guard-free Noop contract intact.
    fn charge_provider_day_tokens(
        &self,
        _provider_id: &str,
        _utc_day: i64,
        _tokens: u64,
        _cap: u64,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn provider_day_tokens(&self, _provider_id: &str, _utc_day: i64) -> Result<u64, StoreError> {
        Ok(0)
    }
    // Per-run review verdicts (STUDIO-1020) disappear with the rest of the history: a store that
    // holds nothing has no review run to attribute, so it answers "no verdict".
    fn set_review_verdict(&self, _run_id: i64, _verdict: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn review_verdict(&self, _run_id: i64) -> Result<Option<String>, StoreError> {
        Ok(None)
    }
    fn load_review_verdicts(
        &self,
        _run_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, String>, StoreError> {
        Ok(std::collections::HashMap::new())
    }
    // Runaway-loop breaker crossings (STUDIO-1026). With persistence off there is no run history to
    // count and nowhere to remember a crossing, so the breaker simply never fires — the same "no
    // history ⇒ no spend signal" the rest of this backend gives every cost surface.
    fn count_completed_review_runs(
        &self,
        _owner: &str,
        _repo: &str,
        _number: i64,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn count_runs_for(&self, _identifier: &str) -> Result<i64, StoreError> {
        Ok(0)
    }

    fn account_credit_spend(&self, _account: &str, _since: &str) -> Result<f64, StoreError> {
        Ok(0.0)
    }
    fn account_credit_notified(&self, _account: &str, _since: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn ticket_spend_by_provider(
        &self,
        _ticket: &str,
        _owner: &str,
        _repo: &str,
        _number: i64,
    ) -> Result<Vec<ProviderTokens>, StoreError> {
        Ok(Vec::new())
    }
    fn save_breaker_crossing(&self, _row: &BreakerCrossingRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_breaker_crossings(&self) -> Result<Vec<BreakerCrossingRow>, StoreError> {
        Ok(Vec::new())
    }
    // Structured review findings (STUDIO-1008): a store that holds nothing has no finding revisions,
    // so every write is a silent success and every read is empty — the guard-free contract above.
    fn save_review_finding(&self, _row: ReviewFindingRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_review_findings(&self, _pr: &str) -> Result<Vec<ReviewFindingRow>, StoreError> {
        Ok(Vec::new())
    }
    fn open_blocking_findings(&self, _pr: &str) -> Result<Vec<ReviewFindingRow>, StoreError> {
        Ok(Vec::new())
    }
    fn resolve_review_findings(
        &self,
        _pr: &str,
        _generation: i64,
        _reviewer: &str,
        _resolved_by: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    // The manager approval record (STUDIO-1011) disappears with the rest of the review state: with
    // persistence off there is nowhere to record an approval and nothing to read back, so every
    // write is a silent success and every read is empty/None — the guard-free contract above. A
    // recheck against a Noop store therefore finds no approval and fails closed.
    fn save_manager_approval(&self, _row: ManagerApprovalRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_manager_approval_state(
        &self,
        _intervention_id: &str,
        _state: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_approval(
        &self,
        _intervention_id: &str,
    ) -> Result<Option<ManagerApprovalRow>, StoreError> {
        Ok(None)
    }
    fn load_manager_approvals(&self) -> Result<Vec<ManagerApprovalRow>, StoreError> {
        Ok(Vec::new())
    }

    // The manager intervention lifecycle (STUDIO-1015) disappears with the rest of the durable
    // state: with persistence off there is nowhere to hold an intervention, so every write is a
    // silent success and every read is empty/None. A reservation against a Noop store is therefore
    // `Absent` — nothing can be launched, which is the fail-closed direction, and the reason
    // `review_authority` other than `off` requires durable storage (§7.6).
    fn save_manager_intervention(&self, _row: ManagerInterventionRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_intervention(
        &self,
        _id: &str,
    ) -> Result<Option<ManagerInterventionRow>, StoreError> {
        Ok(None)
    }
    fn active_manager_intervention(
        &self,
        _pr: &str,
    ) -> Result<Option<ManagerInterventionRow>, StoreError> {
        Ok(None)
    }
    fn load_manager_interventions(&self) -> Result<Vec<ManagerInterventionRow>, StoreError> {
        Ok(Vec::new())
    }
    fn merge_manager_stall_kinds(&self, _id: &str, _kinds: &[String]) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn set_manager_intervention_state(&self, _id: &str, _state: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_manager_intervention_phase_hint(
        &self,
        _id: &str,
        _phase_hint: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn mark_manager_intervention_running(
        &self,
        _id: &str,
        _run_id: Option<i64>,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn record_manager_decision(
        &self,
        _id: &str,
        _decision_json: &str,
        _decision_head: &str,
        _decision_evidence_rev: i64,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn stop_manager_intervention(
        &self,
        _id: &str,
        _terminal_state: &str,
        _reason: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn reserve_manager_run(
        &self,
        _id: &str,
        _boot_id: &str,
        _lease_expires_at: &str,
        _max_runs: i64,
        _max_attempts: i64,
        _max_interventions: i64,
    ) -> Result<ManagerReservation, StoreError> {
        Ok(ManagerReservation::Absent)
    }
    fn expire_manager_leases(
        &self,
        _boot_id: &str,
        _now: &str,
    ) -> Result<Vec<ManagerInterventionRow>, StoreError> {
        Ok(Vec::new())
    }
    fn stop_manager_generation(&self, _pr: &str, _reason: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_budget(&self, _pr: &str) -> Result<Option<ManagerBudgetRow>, StoreError> {
        Ok(None)
    }

    fn tokens_by_provider(&self, _since: &str) -> Result<Vec<ProviderTokens>, StoreError> {
        Ok(Vec::new())
    }
    fn usd_accounting_available(&self) -> bool {
        false
    }
    fn set_turn_spend(&self, _run_id: i64, _spend: &TurnSpend) -> Result<(), StoreError> {
        Ok(())
    }
    fn turn_spend_since(&self, _since: &str) -> Result<Vec<TurnSpend>, StoreError> {
        Ok(Vec::new())
    }
    fn model_has_reported_cost(&self, _provider: &str, _model: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn run_costs(&self) -> Result<Vec<RunCostBucket>, StoreError> {
        Ok(Vec::new())
    }

    fn insert_run_message(
        &self,
        _run_id: i64,
        _body: &str,
        _created_at_ms: i64,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn mark_oldest_run_message_delivered(
        &self,
        _run_id: i64,
        _turn: i64,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn expire_run_messages(&self, _run_id: i64) -> Result<(), StoreError> {
        Ok(())
    }
    fn list_run_messages(&self, _run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
        Ok(Vec::new())
    }

    // Ticketless review watch set (STUDIO-711). With persistence off there is no watch set to
    // survive a restart, so every write succeeds silently and the set reads back empty — the same
    // guard-free contract every other method here keeps.
    fn save_review_watch(&self, _w: ReviewWatchRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn mark_review_requested(
        &self,
        _key: &ReviewWatchKey,
        _requested_sha: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn mark_review_completed(
        &self,
        _key: &ReviewWatchKey,
        _reviewed_sha: &str,
        _status: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn mark_review_truncated(&self, _key: &ReviewWatchKey) -> Result<(), StoreError> {
        Ok(())
    }
    // The review evidence ledger (STUDIO-1009) disappears with the rest of the watch state: with
    // persistence off there is no watch row to carry a completed-review record and no bound row to
    // carry a generation or an evidence revision, so every write is a silent success and every read
    // is empty — the guard-free contract above.
    fn record_review_completion(
        &self,
        _key: &ReviewWatchKey,
        _status: &str,
        _completed: &ReviewCompleted,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn review_completed(
        &self,
        _key: &ReviewWatchKey,
    ) -> Result<Option<ReviewCompleted>, StoreError> {
        Ok(None)
    }
    fn ensure_review_generation(&self, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn increment_review_generation(&self, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_review_evidence_rev(&self, _pr: &str, _evidence_rev: i64) -> Result<(), StoreError> {
        Ok(())
    }
    fn review_bound(&self, _pr: &str) -> Result<Option<ReviewBoundRow>, StoreError> {
        Ok(None)
    }

    // STUDIO-1012: with persistence off there is nowhere to remember an exchange authorization, so
    // the review-side gates find none and an `act`-mode install arms no gated round — the fail-closed
    // direction, and why `review_authority: act` requires durable storage. Exactly the
    // `summon_watermark`/`review_bound` stance above: a no-op store is a store, not a second policy.
    fn save_manager_exchange(&self, _exchange: ManagerExchange) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_exchanges(&self, _pr: &str) -> Result<Vec<ManagerExchange>, StoreError> {
        Ok(Vec::new())
    }
    fn set_manager_exchange_state(&self, _id: &str, _state: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn invalidate_manager_exchanges(&self, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    // STUDIO-1016: with persistence off there is nowhere to remember a wake obligation, and
    // `review_authority` other than `off` already requires durable storage. The fail-closed
    // direction: no wake rows exist, and `activate_manager_intervention` activates nothing.
    fn save_manager_wake(&self, _row: ManagerWakeRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_wake(&self, _intervention_id: &str) -> Result<Option<ManagerWakeRow>, StoreError> {
        Ok(None)
    }
    fn load_manager_wakes(&self) -> Result<Vec<ManagerWakeRow>, StoreError> {
        Ok(Vec::new())
    }
    fn set_manager_wake_state(
        &self,
        _intervention_id: &str,
        _state: &str,
        _run_id: Option<i64>,
        _reason: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn manager_wake_unspent_for_issue(&self, _issue_id: &str) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn activate_manager_intervention(
        &self,
        _request: ManagerActivation,
    ) -> Result<ManagerActivationOutcome, StoreError> {
        Ok(ManagerActivationOutcome::Absent)
    }
    fn record_manager_outcome(
        &self,
        _intervention_id: &str,
        _outcome: &str,
        _now: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_manager_memory_state(
        &self,
        _intervention_id: &str,
        _memory_state: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    // STUDIO-1014: with persistence off the host has nowhere to record what it served, so M3's §6.4
    // condition 3 finds no evidence coverage — the fail-closed direction, and why `review_authority`
    // other than `off` requires durable storage.
    fn record_evidence_access(&self, _access: EvidenceAccess) -> Result<(), StoreError> {
        Ok(())
    }
    fn evidence_accesses(&self, _run_id: i64) -> Result<Vec<EvidenceAccess>, StoreError> {
        Ok(Vec::new())
    }
    fn drop_review_watch(&self, _key: &ReviewWatchKey) -> Result<(), StoreError> {
        Ok(())
    }
    fn get_review_watch(
        &self,
        _key: &ReviewWatchKey,
    ) -> Result<Option<ReviewWatchRow>, StoreError> {
        Ok(None)
    }
    fn find_review_watch(
        &self,
        _key: &ReviewWatchKey,
    ) -> Result<Option<ReviewWatchRow>, StoreError> {
        Ok(None)
    }
    fn load_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError> {
        Ok(Vec::new())
    }
    fn load_live_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError> {
        Ok(Vec::new())
    }

    fn record_summon_watermark(&self, _w: SummonWatermark) -> Result<(), StoreError> {
        Ok(())
    }
    fn summon_watermark(&self, _identifier: &str) -> Result<Option<SummonWatermark>, StoreError> {
        Ok(None)
    }

    // STUDIO-956: with persistence off there is nowhere to remember a bound, so the daemon keeps
    // the pre-STUDIO-956 per-boot behaviour — the in-memory counter and ledger still bound the
    // loop for as long as the process lives, and a restart still refunds it. Exactly the
    // `summon_watermark` stance above: a no-op store is a store, not a second policy.
    fn set_review_rounds(&self, _pr: &str, _dispatches: i64) -> Result<(), StoreError> {
        Ok(())
    }
    fn record_review_adjudication(
        &self,
        _pr: &str,
        _adjudication: &ReviewAdjudication,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear_review_adjudication(&self, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear_review_bound(&self, _pr: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_review_bounds(&self) -> Result<Vec<ReviewBoundRow>, StoreError> {
        Ok(Vec::new())
    }

    // STUDIO-1007: with persistence off there is nowhere to remember an owed terminal move, so the
    // daemon keeps the pre-STUDIO-1007 behaviour — a refused auto-Done move is simply lost, and the
    // ticket stays in review. Exactly the `summon_watermark`/`review_bound` stance above: a no-op
    // store is a store, not a second policy.
    fn save_review_done(&self, _row: ReviewDoneRow) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear_review_done(&self, _identifier: &str) -> Result<(), StoreError> {
        Ok(())
    }
    fn load_review_done(&self) -> Result<Vec<ReviewDoneRow>, StoreError> {
        Ok(Vec::new())
    }

    fn prune(&self, _retention_days: i64) -> Result<(), StoreError> {
        Ok(())
    }
    fn close(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror of Go `TestNoopReturnsEmptyAndNeverErrors` (noop_test.go): the guard-free contract —
    // every write is a successful no-op and every read returns empty. Go's nil slices map to empty
    // Vecs; Go's `(RunSummary{}, false, nil)` GetRun maps to `Ok(None)`.
    #[test]
    fn noop_returns_empty_and_never_errors() {
        let st = Noop;

        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-1".into(),
                ..Default::default()
            })
            .expect("start_run");
        assert_eq!(id, 0, "StartRun must return id 0");
        st.end_run(0, RunEnd::default()).expect("end_run");
        st.update_run_progress(0, RunProgress::default())
            .expect("update_run_progress");
        st.append_events(
            0,
            &[EventRow {
                seq: 1,
                ..Default::default()
            }],
        )
        .expect("append_events");
        st.save_retry(RetryRow::default()).expect("save_retry");
        st.delete_retry("x").expect("delete_retry");
        st.save_claim("x", CLAIM_RUNNING, "").expect("save_claim");
        st.delete_claim("x").expect("delete_claim");
        st.save_totals(Totals {
            input_tokens: 1,
            ..Default::default()
        })
        .expect("save_totals");

        let rec = st.load_recovery().expect("load_recovery");
        assert!(
            rec.retries.is_empty() && rec.claims.is_empty(),
            "LoadRecovery must be empty, got {rec:?}"
        );
        assert_eq!(
            st.mark_running_interrupted()
                .expect("mark_running_interrupted"),
            0
        );
        assert_eq!(st.load_totals().expect("load_totals"), Totals::default());

        assert!(
            st.list_runs(RunFilter::default())
                .expect("list_runs")
                .is_empty()
        );
        assert!(
            st.issue_history("MT-1", "", 0)
                .expect("issue_history")
                .is_empty()
        );
        assert!(st.get_run(1).expect("get_run").is_none());
        assert!(st.run_events(1).expect("run_events").is_empty());
        assert!(
            st.search_events(EventQuery::default())
                .expect("search_events")
                .is_empty()
        );
        assert!(st.metrics(0, "").expect("metrics").is_empty());

        // Operator-message no-ops (INF-250): the disabled store persists nothing.
        assert_eq!(
            st.insert_run_message(1, "hi", 1000)
                .expect("insert_run_message"),
            0
        );
        st.mark_oldest_run_message_delivered(1, 3)
            .expect("mark_oldest_run_message_delivered");
        st.expire_run_messages(1).expect("expire_run_messages");
        assert!(
            st.list_run_messages(1)
                .expect("list_run_messages")
                .is_empty()
        );

        // Provenance + broker usage (STUDIO-909 / STUDIO-987): the disabled store persists neither,
        // so the write is a no-op and the read is the honest `None`.
        st.set_run_provenance(
            1,
            &RunProvenance {
                provider_origin: "default".into(),
                ..Default::default()
            },
        )
        .expect("set_run_provenance");
        assert!(st.run_provenance(1).expect("run_provenance").is_none());
        assert!(st.load_run_provenances(&[1]).expect("batch").is_empty());
        st.set_run_usage(
            1,
            &RunUsage {
                provider_reported_tokens: Some(1),
                reserved_tokens: 2,
                usage_authority: USAGE_AUTHORITY_PROVIDER_REPORTED_UNVERIFIED.into(),
                usage_incomplete: false,
                unknown_usage_requests: 0,
            },
        )
        .expect("set_run_usage");
        assert!(st.run_usage(1).expect("run_usage").is_none());

        // Durable UTC-day provider budget (STUDIO-979): the disabled store charges nothing and reads
        // zero, and never errors.
        assert!(
            !st.charge_provider_day_tokens("p", 20_000, 1, 100)
                .expect("charge_provider_day_tokens"),
            "a disabled store refuses the day charge and charges nothing"
        );
        assert_eq!(
            st.provider_day_tokens("p", 20_000)
                .expect("provider_day_tokens"),
            0
        );

        // Evidence-access log (STUDIO-1014, §5.5): the disabled store records nothing and reads
        // empty, so §6.4 condition 3 never finds coverage — fail-closed, never an error.
        st.record_evidence_access(EvidenceAccess {
            run_id: 1,
            kind: EVIDENCE_ACCESS_DIFF.into(),
            from_sha: "a".into(),
            to_sha: "b".into(),
            recorded_at: "2026-09-23T00:00:00Z".into(),
        })
        .expect("record_evidence_access");
        assert!(
            st.evidence_accesses(1)
                .expect("evidence_accesses")
                .is_empty()
        );

        st.prune(30).expect("prune");
        st.close().expect("close");
    }
}
