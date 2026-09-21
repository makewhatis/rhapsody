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
    fn start_run(&self, _r: RunStart) -> Result<i64, StoreError> {
        Ok(0)
    }
    fn end_run(&self, _run_id: i64, _e: RunEnd) -> Result<(), StoreError> {
        Ok(())
    }
    fn update_run_progress(&self, _run_id: i64, _p: RunProgress) -> Result<(), StoreError> {
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
    fn tokens_by_provider(&self, _since: &str) -> Result<Vec<ProviderTokens>, StoreError> {
        Ok(Vec::new())
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

        st.prune(30).expect("prune");
        st.close().expect("close");
    }
}
