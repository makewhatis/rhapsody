//! persist — parity port of Go `internal/orchestrator/persist.go`.
//!
//! The orchestrator's write-through + recovery seam over the [`Store`](rhapsody_store::Store)
//! (Phase 4 §3.6, §3.8). Every write is BEST-EFFORT: a failure is logged and skipped, never
//! crashing the control task. The low-volume, recovery-critical methods
//! (`persist_start_run`/`persist_end_run`/`persist_retry`/`persist_release`/`save_claim`/
//! `delete_claim`/`persist_totals`/`persist_progress`) run SYNCHRONOUSLY on the control task; the
//! high-volume history events are batched asynchronously by the writer thread
//! ([`Orchestrator::start_event_writer`] / [`Orchestrator::enqueue_event`]).
//!
//! # Keys
//!
//! The in-memory maps (`running`/`retry_attempts`) key by the opaque tracker issue id. The store's
//! `claims`/`retry_queue` PK is the issue IDENTIFIER (e.g. `"MT-12"`) so recovery can re-arm
//! identifier-addressable retries after a restart that does not yet know the opaque id. The persist
//! helpers translate id → identifier at the call site (they always have `re.issue.identifier` in
//! scope); the `runs` row records BOTH (`issue_id` = opaque, `issue_identifier` = human).
//!
//! # Deviations from Go
//!
//!   * Go's writer goroutine + `writerWG`/`writerOnce` become a dedicated `std::thread` joined via a
//!     [`JoinHandle`](std::thread::JoinHandle): the writer does blocking SQLite I/O, which belongs on
//!     an OS thread rather than a tokio worker, and this keeps `enqueue_event`/`start_event_writer`/
//!     `stop_event_writer` synchronous exactly like Go's. The bounded `sync_channel` reproduces the
//!     buffered-channel drop semantics (`enqueue_event` never blocks the control task).
//!   * Best-effort logging goes through `tracing` (the workspace convention) rather than Go's `slog`.
//!   * `open_store` (the daemon-bootstrap store selector) and the operator-message ADMISSION/DELIVERY
//!     helpers (`persistRunMessage`/`persistRunMessageDelivered`) are ported by their owning tickets
//!     (the Run bootstrap = O7; operator messages = O6). `persist_run_messages_expired` lives here
//!     because [`Orchestrator::persist_end_run`] calls it on every run teardown (INF-250).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TrySendError};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_agent as agent;
use rhapsody_store::{self as store, Store};

use crate::orchestrator::{Orchestrator, RunningEntry};

/// Sizes the async event-writer feed (Phase 4 §3.1). A full buffer drops events (counted in
/// `dropped`); the raw `.jsonl` transcript on disk stays the lossless record. Mirrors Go `eventBufCap`.
pub(crate) const EVENT_BUF_CAP: usize = 4096;

/// The event-count flush threshold for the async writer (~200). Mirrors Go `flushBatch`.
const FLUSH_BATCH: usize = 200;

/// The time-based flush cadence for the async writer (~1s). Mirrors Go `flushInterval`.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// One captured session event addressed to its owning run, queued on the event feed for the batched
/// writer thread. Mirrors Go `storeEventWrite`.
pub(crate) struct StoreEventWrite {
    pub(crate) run_id: i64,
    pub(crate) row: store::EventRow,
}

/// Formats a time as UTC RFC3339 (seconds precision — the store's column format, matching Go
/// `t.UTC().Format(time.RFC3339)`). Mirrors Go `rfc3339`.
pub(crate) fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Returns the token tallies to persist for a run: the committed per-turn totals PLUS any uncommitted
/// live in-flight estimate (`cur_*`), and whether that sum leans on the estimate. `cur_*` is non-zero
/// ONLY for a turn that never committed an authoritative result (a no-result teardown:
/// handoff/timeout/crash) — a committed result resets it — so the sum is authoritative when
/// `cur_total_tokens == 0` and a best-available FLOOR (estimated) otherwise. This is the fix for runs
/// that ended without a clean `result` recording 0 tokens (INF-208). Mirrors Go `flooredUsage`.
fn floored_usage(re: &RunningEntry) -> (i64, i64, i64, bool) {
    (
        re.input_tokens + re.cur_input_tokens,
        re.output_tokens + re.cur_output_tokens,
        re.total_tokens + re.cur_total_tokens,
        re.cur_total_tokens > 0,
    )
}

/// Derives the history event kind from the coarse [`agent::Event`] (Phase 4 §6). Mirrors Go `mapKind`.
pub(crate) fn map_kind(ev: &agent::Event) -> String {
    if ev.event_type == agent::EVENT_NOTIFICATION {
        "text".to_string()
    } else {
        "event".to_string()
    }
}

/// Derives the history event tool attribution. The normalized events carry no tool_use attribution,
/// so it is always `""` for Phase 4 (Phase 4 §6). Mirrors Go `mapTool`.
pub(crate) fn map_tool(_ev: &agent::Event) -> String {
    String::new()
}

/// Derives the history event text from the coarse [`agent::Event`] (Phase 4 §6). Mirrors Go `mapText`.
pub(crate) fn map_text(ev: &agent::Event) -> String {
    match ev.event_type.as_str() {
        agent::EVENT_SESSION_STARTED => "session started".to_string(),
        agent::EVENT_TURN_COMPLETED => "turn completed".to_string(),
        agent::EVENT_TURN_FAILED => format!("turn failed: {}", ev.message),
        agent::EVENT_STARTUP_FAILED => format!("startup failed: {}", ev.message),
        _ => ev.message.clone(),
    }
}

impl Orchestrator {
    // --- async history-event writer -----------------------------------------------------------

    /// Launches the single thread that drains the event feed and writes events in batched
    /// transactions, grouped by `run_id`, flushing when a batch reaches [`FLUSH_BATCH`] rows or
    /// [`FLUSH_INTERVAL`] elapses, whichever comes first. Idempotent: a second call is a no-op (the
    /// receive end is taken only once). Mirrors Go `startEventWriter`.
    pub fn start_event_writer(&mut self) {
        let rx = self
            .store_events_rx
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(rx) = rx else {
            return; // already started (or the receiver was taken)
        };
        let store = Arc::clone(&self.store);
        match std::thread::Builder::new()
            .name("rhapsody-event-writer".to_string())
            .spawn(move || run_event_writer(rx, store))
        {
            Ok(handle) => self.writer_handle = Some(handle),
            Err(e) => {
                // Spawn failure (OS resource exhaustion) degrades gracefully: `rx` is dropped, so
                // later `enqueue_event`s shed to `dropped` and the `.jsonl` transcript stays lossless.
                tracing::error!(error = %e, "spawn event writer failed; history events disabled this run");
            }
        }
    }

    /// Closes the event feed and waits for the writer to drain (a final flush). Idempotent. Mirrors
    /// Go `stopEventWriter`.
    pub fn stop_event_writer(&mut self) {
        // Dropping the sole sender disconnects the channel, so the writer performs its final flush
        // and exits; then we join it.
        self.store_events_tx = None;
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join(); // best-effort: a writer panic (none expected) is not fatal at stop
        }
    }

    /// Queues an event for the batched writer; if the buffer is full the event is dropped and
    /// [`Orchestrator::dropped`] is incremented rather than blocking the control task. The raw
    /// `.jsonl` transcript on disk stays the lossless record. A zero `run_id` (no run row: store
    /// disabled or `StartRun` failed) is a no-op. Mirrors Go `enqueueEvent`.
    pub fn enqueue_event(&self, run_id: i64, row: store::EventRow) {
        if run_id == 0 {
            return;
        }
        let Some(tx) = &self.store_events_tx else {
            return; // writer stopped: nothing to attach to
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            tx.try_send(StoreEventWrite { run_id, row })
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    // --- synchronous write-through helpers ----------------------------------------------------

    /// Inserts the run row (outcome `running`), records its id on `re` for later
    /// `end_run`/progress/events, and marks the claim `running`. Mirrors Go `persistStartRun`.
    pub fn persist_start_run(&self, re: &mut RunningEntry, attempt: i64) {
        match self.store.start_run(store::RunStart {
            issue_id: re.issue.id.clone(),
            issue_identifier: re.issue.identifier.clone(),
            title: re.issue.title.clone(),
            attempt,
            // TranscriptPath is left EMPTY at dispatch: the concrete per-run file is not known until
            // the worker opens the transcript, which later stamps the concrete `*.jsonl` path so a
            // past run resolves to its OWN transcript rather than the ticket's `latest.jsonl` alias.
            started_at: rfc3339(re.started_at),
            project_slug: re.project_slug.clone(),
            repo: re.project_repo.clone(),
            team_id: re.issue.team_id.clone(),
            // session_uuid/branch left empty for Phase 4 (Phase 4 §3.4 Risk R4).
            ..Default::default()
        }) {
            Ok(id) => re.run_id = id,
            Err(e) => {
                tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist start run failed");
            }
        }
        self.save_claim(&re.issue.identifier, store::CLAIM_RUNNING, &re.project_slug);
    }

    /// Records the terminal outcome + end time + final tallies for a run. A zero `run_id` (store
    /// disabled / `StartRun` failed) is a no-op. Also expires any still-`sent` operator messages so
    /// the UI/GET don't show them as forever-pending (INF-250) — this is the single end-of-run point
    /// every termination path flows through. Mirrors Go `persistEndRun`.
    pub fn persist_end_run(&self, re: &RunningEntry, outcome: &str, err_str: &str) {
        if re.run_id == 0 {
            return;
        }
        let (input, output, total, estimated) = floored_usage(re);
        if let Err(e) = self.store.end_run(
            re.run_id,
            store::RunEnd {
                outcome: outcome.to_string(),
                ended_at: rfc3339((self.now)()),
                turns: re.turn_count,
                input_tokens: input,
                output_tokens: output,
                total_tokens: total,
                usage_estimated: estimated,
                error: err_str.to_string(),
                // Record the CONCRETE per-run transcript file (timestamped `*.jsonl`), not the
                // `latest.jsonl` alias (empty => the column keeps whatever `StartRun`/progress set).
                transcript_path: re.transcript_path.clone(),
            },
        ) {
            tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist end run failed");
        }
        self.persist_run_messages_expired(re);
    }

    /// Writes per-turn progress (turns + tokens + last event). Called per TURN, not per event. A zero
    /// `run_id` is a no-op. Mirrors Go `persistProgress`.
    pub fn persist_progress(&self, re: &RunningEntry) {
        if re.run_id == 0 {
            return;
        }
        let (input, output, total, estimated) = floored_usage(re);
        if let Err(e) = self.store.update_run_progress(
            re.run_id,
            store::RunProgress {
                turns: re.turn_count,
                input_tokens: input,
                output_tokens: output,
                total_tokens: total,
                usage_estimated: estimated,
                // Record the CONCRETE per-run transcript file once the worker reports it (empty until
                // then => the column keeps the `StartRun` value).
                transcript_path: re.transcript_path.clone(),
            },
        ) {
            tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist run progress failed");
        }
    }

    /// Marks any still-`sent` operator messages for a run as expired at run end (best-effort / no-op
    /// on `run_id == 0`). Mirrors Go `persistRunMessagesExpired`.
    pub(crate) fn persist_run_messages_expired(&self, re: &RunningEntry) {
        if re.run_id == 0 {
            return;
        }
        if let Err(e) = self.store.expire_run_messages(re.run_id) {
            tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist run messages expired failed");
        }
    }

    /// Upserts the retry row (wall-clock due) and marks the claim `retry_queued` so a restart re-arms
    /// the timer and keeps the claim. `identifier` is the store PK. Mirrors Go `persistRetry`.
    pub fn persist_retry(
        &self,
        identifier: &str,
        attempt: i64,
        due_at_ms: i64,
        reason: &str,
        project_slug: &str,
    ) {
        if let Err(e) = self.store.save_retry(store::RetryRow {
            issue_id: identifier.to_string(),
            identifier: identifier.to_string(),
            attempt,
            due_at_ms,
            error: reason.to_string(),
            project_slug: project_slug.to_string(),
        }) {
            tracing::error!(issue_identifier = %identifier, error = %e, "persist retry failed");
        }
        self.save_claim(identifier, store::CLAIM_RETRY_QUEUED, project_slug);
    }

    /// Drops the retry row and the claim for an issue that is gone/ineligible. Mirrors Go `persistRelease`.
    pub fn persist_release(&self, identifier: &str) {
        if let Err(e) = self.store.delete_retry(identifier) {
            tracing::error!(issue_identifier = %identifier, error = %e, "persist delete retry failed");
        }
        self.delete_claim(identifier);
    }

    /// Drops the retry row (if any) and the claim for an issue that finished cleanly (terminal/handoff).
    /// Its body is identical to `persist_release` today; it delegates so the delete sequence lives in
    /// one place if it ever changes. Mirrors Go `persistComplete`.
    pub fn persist_complete(&self, identifier: &str) {
        self.persist_release(identifier);
    }

    /// Best-effort claim upsert (state `running` | `retry_queued`). Mirrors Go `saveClaim`.
    pub fn save_claim(&self, identifier: &str, state: &str, project_slug: &str) {
        if let Err(e) = self.store.save_claim(identifier, state, project_slug) {
            tracing::error!(issue_identifier = %identifier, state = %state, error = %e, "persist save claim failed");
        }
    }

    /// Best-effort claim delete. Mirrors Go `deleteClaim`.
    pub fn delete_claim(&self, identifier: &str) {
        if let Err(e) = self.store.delete_claim(identifier) {
            tracing::error!(issue_identifier = %identifier, error = %e, "persist delete claim failed");
        }
    }

    /// Writes the cumulative token tally + ended-cumulative seconds so dashboard aggregates continue
    /// across restarts. `seconds_running` persists the ENDED cumulative (live elapsed of active
    /// sessions is recomputed at snapshot time and never persisted). Mirrors Go `persistTotals`.
    pub fn persist_totals(&self) {
        if let Err(e) = self.store.save_totals(store::Totals {
            input_tokens: self.totals.input_tokens,
            output_tokens: self.totals.output_tokens,
            total_tokens: self.totals.total_tokens,
            // Go casts the float64 `SecondsRunning` to int (truncates toward zero).
            seconds_running: self.totals.seconds_running as i64,
        }) {
            tracing::error!(error = %e, "persist totals failed");
        }
    }
}

/// The event-writer thread body: drains `rx`, batches by `run_id`, and flushes on a
/// [`FLUSH_BATCH`]-row batch, a [`FLUSH_INTERVAL`] tick, or channel close (final flush then exit).
/// Mirrors the goroutine Go's `startEventWriter` launches.
fn run_event_writer(rx: Receiver<StoreEventWrite>, store: Arc<dyn Store + Send + Sync>) {
    let mut pending: HashMap<i64, Vec<store::EventRow>> = HashMap::new();
    let mut count: usize = 0;
    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(w) => {
                if w.run_id != 0 {
                    pending.entry(w.run_id).or_default().push(w.row);
                    count += 1;
                    if count >= FLUSH_BATCH {
                        flush_events(store.as_ref(), &mut pending, &mut count);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_events(store.as_ref(), &mut pending, &mut count)
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_events(store.as_ref(), &mut pending, &mut count); // channel closed: final flush
                return;
            }
        }
    }
}

/// Flushes each run's batched rows in one `append_events` call, dropping a failed batch with a log
/// (never crashing the writer). Empties `pending` and resets `count`. Mirrors the closure inside Go
/// `startEventWriter`.
fn flush_events(
    store: &dyn Store,
    pending: &mut HashMap<i64, Vec<store::EventRow>>,
    count: &mut usize,
) {
    if *count == 0 {
        return;
    }
    for (run_id, rows) in pending.drain() {
        if run_id == 0 || rows.is_empty() {
            continue;
        }
        if let Err(e) = store.append_events(run_id, &rows) {
            tracing::error!(run_id, n = rows.len(), error = %e, "append events failed (dropping batch)");
        }
    }
    *count = 0;
}

#[cfg(test)]
mod tests {
    use rhapsody_agent::{EVENT_NOTIFICATION, EVENT_TURN_COMPLETED, Event};
    use rhapsody_store::{
        CLAIM_RETRY_QUEUED, CLAIM_RUNNING, OUTCOME_COMPLETED, OUTCOME_CONTINUED, OUTCOME_RUNNING,
        OUTCOME_STOPPED, RUN_MESSAGE_EXPIRED, RunFilter,
    };

    use super::*;
    use crate::agentupdate::AgentUpdate;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{issue, orch_with_store, running_entry};

    fn re_for(id: &str, ident: &str, state: &str) -> RunningEntry {
        running_entry(issue(id, ident, state), "", "")
    }

    // Mirrors Go `TestPersistStartRunOnDispatch` at the persist seam (dispatch itself is O5): the run
    // row lands with outcome=running and the claim is persisted running; `re.run_id` is stamped.
    #[test]
    fn persist_start_run_writes_run_and_running_claim() {
        let (o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "Todo");
        re.issue.title = "do".to_string();
        o.persist_start_run(&mut re, 0);

        assert!(re.run_id > 0, "expected a run row id, got {}", re.run_id);
        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.issue_id, "ID-1");
        assert_eq!(r.issue_identifier, "MT-1");
        assert_eq!(r.title, "do");
        assert_eq!(r.outcome, OUTCOME_RUNNING);
        // Claim persisted as running (keyed by IDENTIFIER).
        let rec = st.load_recovery().expect("load recovery");
        assert_eq!(rec.claims.len(), 1);
        assert_eq!(rec.claims[0].issue_id, "MT-1");
        assert_eq!(rec.claims[0].state, CLAIM_RUNNING);
    }

    // Mirrors Go `TestPersistEndRunFloorsEstimateOnNoResult` (INF-208): a teardown with a live `cur_*`
    // estimate but NO committed result persists the estimate as a FLOOR and marks usage_estimated.
    #[test]
    fn persist_end_run_floors_estimate_on_no_result() {
        let (o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        o.persist_start_run(&mut re, 0);
        re.cur_input_tokens = 139000;
        re.cur_output_tokens = 8000;
        re.cur_total_tokens = 412803;

        o.persist_end_run(&re, OUTCOME_COMPLETED, "");

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.total_tokens, 412803);
        assert_eq!(r.input_tokens, 139000);
        assert_eq!(r.output_tokens, 8000);
        assert!(
            r.usage_estimated,
            "usage_estimated must be true for a floored no-result run"
        );
    }

    // Mirrors Go `TestPersistEndRunAuthoritativeWhenResultCommitted`: a clean run whose result
    // committed (cur_* reset to 0) records the authoritative total with usage_estimated=false.
    #[test]
    fn persist_end_run_authoritative_when_result_committed() {
        let (o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "Todo");
        o.persist_start_run(&mut re, 0);
        re.input_tokens = 60;
        re.output_tokens = 12;
        re.total_tokens = 72; // committed by a result; cur_* is 0.

        o.persist_end_run(&re, OUTCOME_CONTINUED, "");

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].total_tokens, 72);
        assert!(
            !runs[0].usage_estimated,
            "authoritative run must not be marked estimated"
        );
    }

    // Mirrors Go `TestPersistTerminalOutcomes` at the persist seam (terminate is O5): a completed and
    // a stopped teardown each record their outcome and drop the retry row + claim on complete.
    #[test]
    fn persist_terminal_outcomes() {
        for want in [OUTCOME_COMPLETED, OUTCOME_STOPPED] {
            let (o, st) = orch_with_store();
            let mut re = re_for("ID-1", "MT-1", "In Progress");
            o.persist_start_run(&mut re, 0);
            o.persist_end_run(&re, want, "");
            o.persist_complete(&re.issue.identifier);

            let runs = st.list_runs(RunFilter::default()).expect("list runs");
            assert_eq!(runs.len(), 1, "want one run for {want}");
            assert_eq!(runs[0].outcome, want);
            let rec = st.load_recovery().expect("load recovery");
            assert!(
                rec.retries.is_empty() && rec.claims.is_empty(),
                "retry/claim should be dropped on complete: {rec:?}"
            );
        }
    }

    // Mirrors Go `TestPersistPerTurnProgress`: a turn-completed event triggers a synchronous
    // UpdateRunProgress through the O4 wiring in `on_agent_update`.
    #[test]
    fn persist_per_turn_progress() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "Todo");
        o.persist_start_run(&mut re, 0);
        re.turn_count = 2;
        re.input_tokens = 1;
        re.output_tokens = 2;
        re.total_tokens = 3;
        o.running.insert("ID-1".to_string(), re);

        o.on_agent_update(AgentUpdate {
            issue_id: "ID-1".to_string(),
            ev: Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                timestamp: Some((o.now)()),
                ..Default::default()
            },
        });

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].turns, 2);
        assert_eq!(runs[0].total_tokens, 3);
    }

    // Mirrors Go `TestEventBatchingFlushOnStop`: five enqueued events (via `on_agent_update`) are
    // batched by the writer thread and flushed on stop, landing in wire-shape order.
    #[test]
    fn event_batching_flush_on_stop() {
        let (mut o, st) = orch_with_store();
        o.start_event_writer();
        let mut re = re_for("ID-1", "MT-1", "Todo");
        o.persist_start_run(&mut re, 0);
        let run_id = re.run_id;
        o.running.insert("ID-1".to_string(), re);

        for _ in 0..5 {
            o.on_agent_update(AgentUpdate {
                issue_id: "ID-1".to_string(),
                ev: Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    message: "hello".to_string(),
                    timestamp: Some((o.now)()),
                    ..Default::default()
                },
            });
        }
        o.stop_event_writer(); // closes the feed + final flush

        let ev = st.run_events(run_id).expect("run events");
        assert_eq!(ev.len(), 5, "want 5 events");
        for (i, e) in ev.iter().enumerate() {
            assert_eq!(e.seq, i as i64 + 1, "seq");
            assert_eq!(e.kind, "text", "kind");
            assert_eq!(e.text, "hello", "text");
        }
    }

    // Mirrors Go `TestEnqueueEventNeverBlocks` (Risk R1): with no writer draining and the buffer
    // saturated, `enqueue_event` returns immediately and increments the drop counter. `try_send` is
    // non-blocking by construction, so the test completing at all IS the non-blocking proof.
    #[test]
    fn enqueue_event_never_blocks() {
        let o = Orchestrator::new("WORKFLOW.md");
        for i in 0..EVENT_BUF_CAP {
            o.enqueue_event(
                1,
                store::EventRow {
                    seq: i as i64,
                    ..Default::default()
                },
            );
        }
        // The buffer is now full (no writer drains it); this one must be shed, not block.
        o.enqueue_event(1, store::EventRow::default());
        assert!(
            o.dropped.load(Ordering::Relaxed) > 0,
            "expected a dropped-event count"
        );
    }

    // Mirrors Go `TestNoopStoreBehavesLikeToday`: with the default Noop store, the persist helpers
    // no-op (run_id stays 0) and history reads come back empty.
    #[test]
    fn noop_store_behaves_like_today() {
        let o = Orchestrator::new("WORKFLOW.md"); // pstore defaults to Noop
        let mut re = re_for("ID-1", "MT-1", "Todo");
        o.persist_start_run(&mut re, 0);
        assert_eq!(re.run_id, 0, "noop store: run_id must stay 0");
        o.persist_end_run(&re, OUTCOME_CONTINUED, ""); // no-op on run_id 0

        let runs = o
            .store()
            .list_runs(RunFilter::default())
            .expect("noop list runs");
        assert!(runs.is_empty(), "noop ListRuns must be empty");
    }

    // Direct coverage of `persist_retry` (Go exercises it via the retry lifecycle, O5): the retry row
    // + a retry_queued claim are persisted, keyed by identifier.
    #[test]
    fn persist_retry_writes_row_and_retry_queued_claim() {
        let (o, st) = orch_with_store();
        o.persist_retry(
            "MT-1",
            3,
            1_720_612_800_000,
            "no available orchestrator slots",
            "alpha",
        );

        let rec = st.load_recovery().expect("load recovery");
        assert_eq!(rec.retries.len(), 1);
        assert_eq!(rec.retries[0].identifier, "MT-1");
        assert_eq!(rec.retries[0].attempt, 3);
        assert_eq!(rec.claims.len(), 1);
        assert_eq!(rec.claims[0].state, CLAIM_RETRY_QUEUED);
    }

    // Direct coverage of `persist_release`: the retry row + claim are both dropped.
    #[test]
    fn persist_release_drops_retry_and_claim() {
        let (o, st) = orch_with_store();
        o.persist_retry("MT-1", 1, 0, "reason", "alpha");
        o.persist_release("MT-1");

        let rec = st.load_recovery().expect("load recovery");
        assert!(
            rec.retries.is_empty() && rec.claims.is_empty(),
            "release drops both: {rec:?}"
        );
    }

    // Direct coverage of `persist_totals`: the cumulative tally is written back (seconds truncated).
    #[test]
    fn persist_totals_writes_cumulative() {
        let (mut o, st) = orch_with_store();
        o.totals.input_tokens = 500;
        o.totals.output_tokens = 200;
        o.totals.total_tokens = 700;
        o.totals.seconds_running = 130.9;
        o.persist_totals();

        let t = st.load_totals().expect("load totals");
        assert_eq!(t.input_tokens, 500);
        assert_eq!(t.total_tokens, 700);
        assert_eq!(t.seconds_running, 130, "float seconds truncate toward zero");
    }

    // `persist_end_run` expires any still-`sent` operator messages at teardown (INF-250). The message
    // is admitted here via the store's own API (the operator-message ADMISSION helper is O6).
    #[test]
    fn persist_end_run_expires_pending_messages() {
        let (o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        o.persist_start_run(&mut re, 0);
        st.insert_run_message(re.run_id, "please rebase", 1_720_612_800_000)
            .expect("insert run message");

        o.persist_end_run(&re, OUTCOME_COMPLETED, "");

        let msgs = st.list_run_messages(re.run_id).expect("list run messages");
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].status, RUN_MESSAGE_EXPIRED,
            "pending message must expire at run end"
        );
    }
}
