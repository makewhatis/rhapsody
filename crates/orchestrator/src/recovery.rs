//! recovery — parity port of Go `internal/orchestrator/recovery.go` (crash recovery, Phase 4 §3.7).
//!
//! [`Orchestrator::boot_recovery`] rebuilds the in-memory recovery state from the store at startup —
//! before the first tick, so the tick's reconcile validates every recovered claim against live Linear
//! and releases any that are no longer eligible. Every step is best-effort: a failed read is logged
//! and recovery proceeds with whatever loaded, never crashing the control task.
//!
//! Sequence:
//!  1. `mark_running_interrupted`: any run left "running" (process died mid-flight) → "interrupted".
//!  2. `load_recovery`: re-arm each retry at `max(0, due-now)` + restore claims.
//!  3. Convert interrupted-running claims (no live worker) to immediate retries (kept claimed).
//!  4. `load_totals`: seed `o.totals` so dashboard aggregates continue across restarts.
//!
//! KEY HANDLING (the load-bearing recovery fix): the live maps key by opaque issue ID, but at boot we
//! have ONLY the identifier (the store PK). So recovered entries are keyed by IDENTIFIER with
//! `issue_id == ""` and `recovered == true`; [`Orchestrator::on_retry`] resolves their candidate via
//! [`crate::orchestrator::find_by_identifier`] and re-keys to the real ID before dispatch.
//!
//! Deviation from the Go source (behavior-preserving): the live retry TIMER (Go `time.AfterFunc` →
//! `o.events <- evRetry`) is O7's — it needs the control-event channel the loop owns. O5 records the
//! recovered [`crate::orchestrator::RetryEntry`] (with its persisted `due_at_ms`); O7 arms the timer
//! against it. The recovery tests drive the fired retry synchronously via [`Orchestrator::on_retry`].

use std::collections::HashMap;

use chrono::{DateTime, Duration};
use rhapsody_core::Issue;
use rhapsody_store as store;

use crate::orchestrator::{Orchestrator, RetryEntry, zero_time};

impl Orchestrator {
    /// Rebuilds the in-memory recovery state from the store at startup (Phase 4 §3.7). Mirrors Go
    /// `bootRecovery`. A control-loop (O7) entry point, called from `Run` before the first tick.
    pub fn boot_recovery(&mut self) {
        match self.store.mark_running_interrupted() {
            Ok(n) if n > 0 => tracing::info!(count = n, "recovery: marked interrupted runs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "recovery: mark interrupted failed"),
        }

        let rec = match self.store.load_recovery() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "recovery: load failed; starting with empty recovery state");
                store::Recovery::default()
            }
        };

        // Index retries by key so we can re-arm + look up the attempt for interrupted-running claims. A
        // recovered retry whose non-empty project_slug is no longer configured is RELEASED on boot.
        let mut retry_by_key: HashMap<String, store::RetryRow> = HashMap::new();
        for rr in rec.retries {
            if !rr.project_slug.is_empty() && self.project_unconfigured(&rr.project_slug) {
                tracing::info!(issue_identifier = %rr.identifier, project_slug = %rr.project_slug, "recovery: releasing recovered retry; project no longer configured");
                self.persist_release(&rr.identifier);
                continue;
            }
            let key = if rr.identifier.is_empty() {
                rr.issue_id.clone()
            } else {
                rr.identifier.clone()
            };
            retry_by_key.insert(key, rr.clone());
            self.re_arm_retry(rr);
        }

        // Convert interrupted-running claims (no live worker) to immediate retries, keeping them
        // claimed. A claim whose non-empty project_slug is no longer configured is released instead.
        // `ClaimRow.issue_id` IS the identifier (store PK).
        for cl in rec.claims {
            let identifier = cl.issue_id;
            if !cl.project_slug.is_empty() && self.project_unconfigured(&cl.project_slug) {
                tracing::info!(issue_identifier = %identifier, project_slug = %cl.project_slug, "recovery: releasing recovered claim; project no longer configured");
                self.persist_release(&identifier);
                self.claimed.remove(&identifier);
                self.retry_attempts.remove(&identifier);
                continue;
            }
            self.claimed.insert(identifier.clone());
            if cl.state != store::CLAIM_RUNNING {
                continue; // retry_queued claims are handled by their re-armed retry above
            }
            if self.retry_attempts.contains_key(&identifier) {
                continue; // already has a re-armed retry entry from the retry_queue
            }
            let attempt = retry_by_key.get(&identifier).map_or(0, |rr| rr.attempt);
            self.arm_immediate_retry(&identifier, attempt, &cl.project_slug);
        }

        match self.store.load_totals() {
            Ok(t) => {
                self.totals.input_tokens = t.input_tokens;
                self.totals.output_tokens = t.output_tokens;
                self.totals.total_tokens = t.total_tokens;
                // Our `seconds_running` is f64 (the store column is INTEGER): cast back.
                self.totals.seconds_running = t.seconds_running as f64;
            }
            Err(e) => tracing::error!(error = %e, "recovery: load totals failed"),
        }
    }

    /// Reports whether `slug` (non-empty) no longer resolves in the current effective set. Mirrors the
    /// Go `o.eff.projectBySlug(slug) == nil` boot-time guard.
    fn project_unconfigured(&self, slug: &str) -> bool {
        self.eff
            .as_ref()
            .and_then(|e| e.project_by_slug(slug))
            .is_none()
    }

    /// Re-creates an in-memory recovered retry entry for a persisted retry row (keyed by IDENTIFIER,
    /// `issue_id == ""`, `recovered == true`; the opaque ID is unknown at boot) and (re)marks the
    /// claim. The live timer firing at `max(0, due-now)` is O7's, armed against `due_at_ms`. Mirrors Go
    /// `reArmRetry`.
    pub(crate) fn re_arm_retry(&mut self, rr: store::RetryRow) {
        let key = if rr.identifier.is_empty() {
            rr.issue_id.clone()
        } else {
            rr.identifier.clone()
        };
        self.claimed.insert(key.clone());
        let due_at = DateTime::from_timestamp_millis(rr.due_at_ms).unwrap_or_else(zero_time);
        self.retry_attempts.insert(
            key.clone(),
            RetryEntry {
                issue_id: String::new(), // unknown at boot; resolved on first on_retry fire
                identifier: key,
                attempt: rr.attempt,
                due_at,
                due_at_ms: rr.due_at_ms,
                err: rr.error,
                project_slug: rr.project_slug,
                project_repo: String::new(), // not persisted in retry_queue; re-derived on dispatch
                issue: Issue::default(),
                recovered: true,
            },
        );
    }

    /// Re-arms a still-recovered (identifier-keyed) retry after [`Orchestrator::on_retry`] could not
    /// dispatch it yet (transient poll failure or no free slots). Mirrors `schedule_retry_for` but keeps
    /// the entry IDENTIFIER-keyed + `recovered` so the next fire still re-matches by identifier. The
    /// caller has already removed the prior identifier-keyed entry from `retry_attempts`. Mirrors Go
    /// `requeueRecovered`.
    pub(crate) fn requeue_recovered(
        &mut self,
        prev: &RetryEntry,
        attempt: i64,
        delay_ms: i64,
        reason: &str,
    ) {
        let identifier = prev.identifier.clone();
        let due_at = (self.now)() + Duration::milliseconds(delay_ms);
        let due_at_ms = due_at.timestamp_millis();
        self.claimed.insert(identifier.clone());
        self.retry_attempts.insert(
            identifier.clone(),
            RetryEntry {
                issue_id: String::new(),
                identifier: identifier.clone(),
                attempt,
                due_at,
                due_at_ms,
                err: reason.to_string(),
                project_slug: prev.project_slug.clone(),
                project_repo: prev.project_repo.clone(),
                issue: Issue::default(),
                recovered: true,
            },
        );
        self.persist_retry(&identifier, attempt, due_at_ms, reason, &prev.project_slug);
    }

    /// Queues a now-due retry for an interrupted-running claim so the next tick re-validates it against
    /// live Linear (re-dispatch if still active, release if not). The claim is kept; the entry is
    /// recovered (identifier-keyed) and the retry row is persisted so it survives a second restart.
    /// Mirrors Go `armImmediateRetry`.
    pub(crate) fn arm_immediate_retry(
        &mut self,
        identifier: &str,
        attempt: i64,
        project_slug: &str,
    ) {
        let now = (self.now)();
        let now_ms = now.timestamp_millis();
        self.claimed.insert(identifier.to_string());
        self.retry_attempts.insert(
            identifier.to_string(),
            RetryEntry {
                issue_id: String::new(),
                identifier: identifier.to_string(),
                attempt,
                due_at: now,
                due_at_ms: now_ms,
                err: "interrupted_recovery".to_string(),
                project_slug: project_slug.to_string(),
                project_repo: String::new(),
                issue: Issue::default(),
                recovered: true,
            },
        );
        self.persist_retry(
            identifier,
            attempt,
            now_ms,
            "interrupted_recovery",
            project_slug,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::EvRetry;
    use crate::testsupport::*;
    use chrono::Utc;
    use rhapsody_store::Store;
    use rhapsody_tracker::fake::Fake;
    use std::sync::Arc;

    /// A fresh in-memory store handle.
    fn mem_store() -> Arc<dyn Store + Send + Sync> {
        Arc::new(store::Sqlite::open(store::StorePath::InMemory).expect("open in-memory store"))
    }

    // Mirrors Go `TestBootRecoveryMarksInterruptedRuns`.
    #[test]
    fn boot_recovery_marks_interrupted_runs() {
        let st = mem_store();
        // A run left "running" (process died mid-flight).
        st.start_run(store::RunStart {
            issue_id: "ID-1".into(),
            issue_identifier: "MT-1".into(),
            ..Default::default()
        })
        .expect("start_run");

        let (mut o, _) = recovery_orch(Arc::clone(&st), Arc::new(Fake::new()), Utc::now());
        o.boot_recovery();

        let runs = st
            .list_runs(store::RunFilter {
                issue: "MT-1".into(),
                ..Default::default()
            })
            .expect("list_runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, store::OUTCOME_INTERRUPTED);
        assert!(!runs[0].ended_at.is_empty(), "ended_at should be set");
    }

    // Mirrors Go `TestBootRecoveryReArmsRetries`.
    #[tokio::test]
    async fn boot_recovery_rearms_retries() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        let st = mem_store();
        // Past-due retry (fires ~immediately) and future-due retry (re-armed for later).
        st.save_retry(store::RetryRow {
            issue_id: "MT-PAST".into(),
            identifier: "MT-PAST".into(),
            attempt: 2,
            due_at_ms: (now - chrono::Duration::minutes(1)).timestamp_millis(),
            error: "x".into(),
            ..Default::default()
        })
        .expect("save_retry");
        st.save_claim("MT-PAST", store::CLAIM_RETRY_QUEUED, "")
            .expect("save_claim");
        st.save_retry(store::RetryRow {
            issue_id: "MT-FUT".into(),
            identifier: "MT-FUT".into(),
            attempt: 1,
            due_at_ms: (now + chrono::Duration::hours(1)).timestamp_millis(),
            error: "y".into(),
            ..Default::default()
        })
        .expect("save_retry");
        st.save_claim("MT-FUT", store::CLAIM_RETRY_QUEUED, "")
            .expect("save_claim");

        let mut f = Fake::new();
        // The past-due retry's candidate must be present so the fired retry re-dispatches.
        f.candidates = vec![issue("ID-PAST", "MT-PAST", "In Progress")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = recovery_orch(Arc::clone(&st), Arc::clone(&tr), now);
        o.boot_recovery();

        // Both retries are in-memory, keyed by IDENTIFIER, flagged recovered, claimed.
        let past = o.retry_attempts.get("MT-PAST").expect("past re-armed");
        assert!(past.recovered && past.issue_id.is_empty() && past.identifier == "MT-PAST");
        let fut = o.retry_attempts.get("MT-FUT").expect("fut re-armed");
        assert!(fut.recovered && fut.attempt == 1);
        assert!(
            o.claimed.contains("MT-PAST") && o.claimed.contains("MT-FUT"),
            "recovered retries keep their claims"
        );

        // Drive the past-due timer's fire synchronously (it posts EvRetry{MT-PAST}).
        o.on_retry(EvRetry {
            issue_id: "MT-PAST".into(),
        })
        .await;

        // CRITICAL: the recovered claim must NOT be silently released — it re-dispatches.
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["ID-PAST".to_string()],
            "recovered past-due retry must RE-DISPATCH"
        );
        assert!(
            !o.retry_attempts.contains_key("MT-PAST"),
            "identifier-keyed retry entry gone after re-dispatch"
        );
        assert!(
            !o.claimed.contains("MT-PAST"),
            "stale identifier-keyed claim dropped after re-key"
        );
        assert!(
            o.running.contains_key("ID-PAST"),
            "re-dispatched issue running under its real ID"
        );
    }

    // Mirrors Go `TestBootRecoveryConvertsInterruptedClaimToImmediateRetry`.
    #[test]
    fn boot_recovery_converts_interrupted_claim_to_immediate_retry() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        let st = mem_store();
        // A "running" claim with no matching retry row (the worker was interrupted).
        st.save_claim("MT-1", store::CLAIM_RUNNING, "")
            .expect("save_claim");

        let (mut o, _) = recovery_orch(Arc::clone(&st), Arc::new(Fake::new()), now);
        o.boot_recovery();

        let re = o
            .retry_attempts
            .get("MT-1")
            .expect("converted to immediate retry");
        assert!(re.recovered && re.err == "interrupted_recovery");
        assert!(
            o.claimed.contains("MT-1"),
            "interrupted claim must stay claimed"
        );
        // The immediate retry is persisted so it survives a second restart.
        let rec = st.load_recovery().expect("load_recovery");
        assert_eq!(rec.retries.len(), 1);
        assert_eq!(rec.retries[0].identifier, "MT-1");
    }

    // Mirrors Go `TestBootRecoveryRestoresTotals`.
    #[test]
    fn boot_recovery_restores_totals() {
        let st = mem_store();
        st.save_totals(store::Totals {
            input_tokens: 100,
            output_tokens: 200,
            total_tokens: 300,
            seconds_running: 4242,
        })
        .expect("save_totals");

        let (mut o, _) = recovery_orch(Arc::clone(&st), Arc::new(Fake::new()), Utc::now());
        o.boot_recovery();

        assert_eq!(o.totals.input_tokens, 100);
        assert_eq!(o.totals.output_tokens, 200);
        assert_eq!(o.totals.total_tokens, 300);
        assert_eq!(o.totals.seconds_running, 4242.0);
    }

    // Mirrors Go `TestBootRecoveryReleasesRemovedProject`.
    #[test]
    fn boot_recovery_releases_removed_project() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        let st = mem_store();
        st.save_retry(store::RetryRow {
            issue_id: "MT-GONE".into(),
            identifier: "MT-GONE".into(),
            attempt: 1,
            due_at_ms: now.timestamp_millis(),
            project_slug: "removed".into(),
            ..Default::default()
        })
        .expect("save_retry");
        st.save_claim("MT-GONE", store::CLAIM_RETRY_QUEUED, "removed")
            .expect("save_claim");

        // eff.projects is empty, so project_by_slug("removed") == None => released.
        let (mut o, _) = recovery_orch(Arc::clone(&st), Arc::new(Fake::new()), now);
        o.boot_recovery();

        assert!(
            !o.retry_attempts.contains_key("MT-GONE"),
            "retry for a removed project must not be re-armed"
        );
        assert!(
            !o.claimed.contains("MT-GONE"),
            "claim for a removed project must be released"
        );
        let rec = st.load_recovery().expect("load_recovery");
        assert!(
            rec.retries.is_empty() && rec.claims.is_empty(),
            "removed-project rows should be deleted"
        );
    }

    // Mirrors Go `TestRecoveredRequeueStaysIdentifierKeyed` (§3.7 residual risk).
    #[tokio::test]
    async fn recovered_requeue_stays_identifier_keyed() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        let st = mem_store();
        st.save_retry(store::RetryRow {
            issue_id: "MT-1".into(),
            identifier: "MT-1".into(),
            attempt: 1,
            due_at_ms: now.timestamp_millis(),
            ..Default::default()
        })
        .expect("save_retry");
        st.save_claim("MT-1", store::CLAIM_RETRY_QUEUED, "")
            .expect("save_claim");

        let mut f = Fake::new();
        f.candidates = vec![issue("ID-1", "MT-1", "In Progress")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = recovery_orch(Arc::clone(&st), Arc::clone(&tr), now);
        o.eff.as_mut().unwrap().max_concurrent = 0; // force the no-slots requeue branch
        o.boot_recovery();

        // First fire: no slots → requeue, but MUST remain recovered + identifier-keyed.
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "should not dispatch with zero slots"
        );
        let req = o.retry_attempts.get("MT-1").expect("requeued");
        assert!(
            req.recovered && req.issue_id.is_empty(),
            "requeued recovered entry stays identifier-keyed + recovered"
        );
        assert!(o.claimed.contains("MT-1"), "claim kept across requeue");

        // Open a slot and fire again: now it must RE-DISPATCH (not release).
        o.eff.as_mut().unwrap().max_concurrent = 10;
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["ID-1".to_string()],
            "recovered entry re-dispatches after a slot frees"
        );
        assert!(
            o.running.contains_key("ID-1"),
            "re-dispatched issue running under its real ID"
        );
    }

    // Mirrors Go `TestRecoveredClaimSurvivesTickBeforeRetryFires` (§3.7 boot race).
    #[tokio::test]
    async fn recovered_claim_survives_tick_before_retry_fires() {
        let now = utc(2026, 1, 1, 12, 0, 0);
        let st = mem_store();
        st.save_retry(store::RetryRow {
            issue_id: "MT-1".into(),
            identifier: "MT-1".into(),
            attempt: 1,
            due_at_ms: (now - chrono::Duration::minutes(1)).timestamp_millis(),
            ..Default::default()
        })
        .expect("save_retry");
        st.save_claim("MT-1", store::CLAIM_RETRY_QUEUED, "")
            .expect("save_claim");

        let mut f = Fake::new();
        f.candidates = vec![issue("ID-1", "MT-1", "In Progress")]; // also a live poll candidate
        let tr = Arc::new(f);
        let (mut o, dispatched) = recovery_orch(Arc::clone(&st), Arc::clone(&tr), now);
        o.boot_recovery();

        assert!(o.retry_attempts.get("MT-1").expect("re-armed").recovered);

        // Drive a poll tick FIRST (the recovery-window race): selectDispatch must skip MT-1.
        let picked = o.select_dispatch(vec![issue("ID-1", "MT-1", "In Progress")]);
        assert!(
            picked.is_empty(),
            "tick must not dispatch an issue owned by a recovered claim"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "no issue should be dispatched by the tick"
        );

        // The persisted claim row for MT-1 must still be present.
        let rec = st.load_recovery().expect("load_recovery");
        assert!(
            rec.claims.iter().any(|c| c.issue_id == "MT-1"),
            "recovered claim for MT-1 must survive the tick"
        );

        // Now fire the recovered retry: it re-keys to the opaque ID and dispatches cleanly.
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["ID-1".to_string()],
            "recovered retry re-dispatches under the opaque ID"
        );
        assert!(
            o.running.contains_key("ID-1"),
            "re-dispatched issue running under its real ID"
        );
        // After re-dispatch the claim is persisted under the identifier in state=running.
        let rec = st.load_recovery().expect("load_recovery");
        assert_eq!(rec.claims.len(), 1);
        assert_eq!(rec.claims[0].issue_id, "MT-1");
        assert_eq!(rec.claims[0].state, store::CLAIM_RUNNING);
    }

    // Mirrors Go `TestBootRecoveryEmptyStoreIsNoop`.
    #[test]
    fn boot_recovery_empty_store_is_noop() {
        let st: Arc<dyn Store + Send + Sync> = Arc::new(store::Noop);
        let (mut o, _) = recovery_orch(st, Arc::new(Fake::new()), Utc::now());
        o.boot_recovery();
        assert!(
            o.claimed.is_empty() && o.retry_attempts.is_empty(),
            "noop recovery must leave state empty"
        );
        assert_eq!(
            o.totals,
            crate::orchestrator::Totals::default(),
            "noop recovery totals must be zero"
        );
    }
}
