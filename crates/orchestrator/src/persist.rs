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

/// Derive the PROVIDER a run actually billed, from the harness it ran on and the model string it
/// used (STUDIO-909). The model string is the authority: opencode names models as
/// `provider/model` (`fireworks-ai/accounts/fireworks/models/…`), so the segment before the first
/// `/` IS the provider and is kept verbatim — never case-folded or normalized into a name the CLI
/// did not use. Claude model names carry no `/`, and Claude's provider is unambiguously Anthropic,
/// so that one case is named explicitly. Anything else — an empty model, a bare model name on a
/// harness whose provider is not knowable — answers the empty string, which the console renders as
/// unknown rather than guessing. Being DERIVED here, once, at dispatch, and then PERSISTED means a
/// later config change cannot make the recorded provider disagree with the recorded model.
pub(crate) fn derive_provider(harness: &str, model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    if let Some((prefix, _rest)) = model.split_once('/')
        && !prefix.trim().is_empty()
    {
        return prefix.trim().to_string();
    }
    match harness {
        "claude" => "anthropic".to_string(),
        _ => String::new(),
    }
}

/// The ORIGIN of the provider [`derive_provider`] derived (STUDIO-987): the third provenance origin,
/// recorded beside the value so a later reader can tell a selected provider from an inferred one.
///
/// No configured provider tier selects a provider on this build's dispatch path yet — the pure
/// resolver (STUDIO-986) is not wired into dispatch, which is the later PB5/P6 slice. So a non-empty
/// provider here is always INFERRED from the model, and the resolver's own spelling for "an implicit
/// value with no configured tier behind it" is [`DEFAULT`](crate::selection::Origin::Default)
/// — `"default"`. An empty provider has no origin at all, exactly as an empty model does.
///
/// When PB5 wires the resolver in, this becomes `resolved.origins.provider`, and a row's origin
/// names the real tier (`global`, `profile`, `ticket`, …) instead.
pub(crate) fn derive_provider_origin(provider: &str) -> String {
    if provider.is_empty() {
        String::new()
    } else {
        crate::selection::Origin::Default.as_str().to_string()
    }
}

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

    /// Persist a brokered run's finalized usage (PB7, STUDIO-1002; design §§7.3, 10.3) and REPLACE
    /// the run's child-reported token tallies with the broker-collected figures.
    ///
    /// The run id is carried on the event rather than looked up from the live entry: a production
    /// cancellation (`terminate`) removes the entry before the worker's future drops, so a
    /// cancellation receipt would otherwise be lost here (STUDIO-1002 review A2). A zero run id
    /// (store off) is a no-op; a store failure is logged, never fatal — accounting must never fail a
    /// run.
    ///
    /// When the entry is still live (the ordinary path — the worker sends this before its exit),
    /// the run's committed token tallies are replaced from the drained ledgers, so the `runs` row and
    /// the per-provider budget see the broker's numbers, not the child's (design §7.3: "replaces
    /// brokered-turn token/cache counts with broker-collected provider reports"; child values are
    /// comparison-only diagnostics). When it is NOT live — every production cancellation, where
    /// `terminate` + `persist_end_run` closed the row with the child's figures first — the closed
    /// row's tallies are rewritten from the receipt instead (STUDIO-1047). The committed total is the
    /// provider-reported total, or the conservative reservation when the broker could not report —
    /// never the child's figure, so an unknown request is not filled from a child value. The
    /// input/output split is not part of the broker receipt, so those are zeroed for the brokered
    /// run; the usage row keeps the reported and reserved totals separately.
    pub(crate) fn on_broker_usage(&mut self, issue_id: &str, run_id: i64, usage: &store::RunUsage) {
        if run_id != 0
            && let Err(e) = self.store.set_run_usage(run_id, usage)
        {
            tracing::warn!(
                issue_id = %issue_id,
                run_id,
                err = %e,
                "broker usage persistence failed; the run's own history is unaffected"
            );
        }
        self.replace_child_usage_with_broker(issue_id, run_id, usage);
        // Converge the DURABLE cumulative tally too. On a cancellation the caller already ran
        // `persist_totals` BEFORE this receipt arrived, so without this the corrected aggregate
        // would sit only in memory until the next run teardown; on a normal exit the later
        // worker-exit `persist_totals` would cover it, and this write is simply an idempotent
        // earlier one (STUDIO-1047).
        self.persist_totals();
    }

    /// Replace a run's committed token tallies with the finalized broker receipt (design §7.3) and
    /// settle the cumulative aggregate from that receipt.
    ///
    /// The LIVE path is the ordinary teardown: the worker sends the receipt before its exit, so the
    /// running entry is still present and its committed figures are replaced in place, then
    /// `persist_end_run` writes the broker figures to the run row.
    ///
    /// The NO-LIVE path is every production cancellation, and it is the one STUDIO-1002 left broken:
    /// `terminate` removes the entry and its caller closes the run row SYNCHRONOUSLY (operator Stop,
    /// stall kill, terminal reconcile, per-run token ceiling), all before the worker's
    /// `Event::BrokerUsage` arrives. The child's figure is therefore already on the `runs` row — and
    /// `tokens_by_provider`, the per-provider budget, reads that row. So when there is no live entry
    /// and the event carries a run id, the row's tallies are rewritten from the receipt too, using
    /// the same "reported total, else the conservative reservation" rule as the live path. A zero run
    /// id (store disabled) has no row to correct.
    ///
    /// Neither path subtracts a child contribution from the aggregate, and that is deliberate: a
    /// brokered run's child figure is never folded into `totals` in the first place (see
    /// `RunningEntry::brokered` and `on_agent_update`), so the settlement is a single addition in
    /// BOTH cases — which is exactly why the cancelled case needs no record of what the child
    /// reported. The committed total is the provider-reported total; when the broker could not
    /// report, it is the conservative reservation, so an unknown/aborted request stays charged and
    /// is never filled from the child's figure. The input/output split is not part of the broker
    /// receipt, so those are zeroed for the brokered run (the usage row keeps reported and reserved
    /// separately).
    fn replace_child_usage_with_broker(
        &mut self,
        issue_id: &str,
        run_id: i64,
        usage: &store::RunUsage,
    ) {
        let broker_total = usage
            .provider_reported_tokens
            .unwrap_or(usage.reserved_tokens);
        if let Some(re) = self.running.get_mut(issue_id) {
            // The live entry must be THIS run's (STUDIO-1047, alice F2). A ticket can be
            // re-dispatched while a terminated run's receipt is still in flight; matching on
            // `issue_id` alone would let that stale receipt rewrite the NEW entry while the old run's
            // row keeps its child figure. `on_worker_exit` guards the same race with `started_at`;
            // the run id is the guard here. With the store off (`run_id == 0`) there is no id to
            // compare and the entry is the only run this ticket has, so the live path stands.
            if run_id == 0 || re.run_id == run_id {
                re.input_tokens = 0;
                re.output_tokens = 0;
                re.total_tokens = broker_total;
                re.cur_input_tokens = 0;
                re.cur_output_tokens = 0;
                re.cur_total_tokens = 0;
                self.settle_broker_total_in_totals(broker_total);
                return;
            }
        }
        // No live entry — or one that belongs to a LATER run of the same ticket, whose receipt this
        // is not: a cancellation already closed THIS run's row, so its tallies are rewritten from the
        // receipt. Settle the in-memory aggregate FIRST, because it applies whether or not a row
        // exists: a brokered run's child figure was never folded in, so with the store off
        // (`run_id == 0`) there is no row to correct but the aggregate still needs the receipt
        // (STUDIO-1047, alice F1). The rewrite is best-effort like every other persist call.
        self.settle_broker_total_in_totals(broker_total);
        if run_id == 0 {
            return;
        }
        if let Err(e) = self.store.set_run_tokens(
            run_id,
            &store::RunTokens {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: broker_total,
                usage_estimated: false,
            },
        ) {
            tracing::warn!(
                issue_id = %issue_id,
                run_id,
                err = %e,
                "broker usage run-row rewrite failed; the run's own history is unaffected"
            );
        }
    }

    /// Adds a brokered run's finalized figure to the cumulative aggregate. Shared by the live and
    /// cancelled paths of
    /// [`replace_child_usage_with_broker`](Orchestrator::replace_child_usage_with_broker); since the
    /// child's own tokens were never counted for a brokered run, there is nothing to subtract first.
    fn settle_broker_total_in_totals(&mut self, broker_total: i64) {
        self.totals.total_tokens = self.totals.total_tokens.saturating_add(broker_total);
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
        // What this run ACTUALLY runs on (STUDIO-909), recorded once at dispatch beside the run row
        // so a later WORKFLOW.md hot-reload cannot rewrite what a finished run says it ran on. This
        // is a provenance RECORD, not a config echo: the harness/model are the values the worker was
        // dispatched with, and the origin names the key each came from. Best-effort like every other
        // persist call — a run without a row (store disabled / insert failed) records nothing and
        // renders as unknown rather than failing the dispatch.
        if re.run_id != 0 {
            let prov = self.run_provenance_for(re);
            if let Err(e) = self.store.set_run_provenance(re.run_id, &prov) {
                tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist run provenance failed");
            }
        }
        self.save_claim(&re.issue.identifier, store::CLAIM_RUNNING, &re.project_slug);
    }

    /// The per-run provenance record (STUDIO-909): the harness the run actually uses, the model it
    /// actually uses, and the origin of each, with the provider DERIVED once from the two.
    ///
    /// The model is the override the dispatch resolved (the routed teammate's profile, or the
    /// `review.model` override) when there is one, else the actual harness's own configured model —
    /// never `re.model`, which is a Go-parity telemetry label deliberately stamped from
    /// `claude.model` regardless of harness. Reading the live config HERE, at dispatch, and
    /// persisting the answer is the whole point: the config hot-reloads, the run is history.
    fn run_provenance_for(&self, re: &RunningEntry) -> store::RunProvenance {
        let (harness, model) =
            self.resolved_harness_model(&re.harness, &re.model_override, &re.project_slug);
        // An origin is only meaningful beside a value (STUDIO-909 round 1). `re.model_origin` can
        // name a key that resolved nothing — `agent.backend: codex` is a recognized harness this
        // build has no runner for, so `configured_model_for` answers empty while the origin fallback
        // would still spell `codex.model`. Recording an origin for a model that was never resolved
        // asserts something untrue about the row; drop it instead.
        let model_origin = if model.is_empty() {
            String::new()
        } else {
            re.model_origin.clone()
        };
        let provider = derive_provider(&harness, &model);
        store::RunProvenance {
            provider_origin: derive_provider_origin(&provider),
            provider,
            harness,
            harness_origin: re.harness_origin.clone(),
            model,
            model_origin,
        }
    }

    /// The harness and model a run will ACTUALLY use, from the harness it names (empty ⇒ the
    /// configured backend), its model/effort override (empty model ⇒ the harness's configured
    /// model), and the owning project. The single resolution shared by
    /// [`run_provenance_for`](Orchestrator::run_provenance_for) (which persists it) and the
    /// per-provider budget gate (which reads it before dispatch), so the provider the budget checks
    /// can never disagree with the provider the run records (STUDIO-957).
    pub(crate) fn resolved_harness_model(
        &self,
        harness: &str,
        model_override: &rhapsody_agent::ModelOverride,
        project_slug: &str,
    ) -> (String, String) {
        let harness = self.effective_harness(harness);
        let model = if model_override.model.is_empty() {
            self.configured_model_for(project_slug, &harness)
        } else {
            model_override.model.clone()
        };
        (harness, model)
    }

    /// The provider a prospective run will bill, derived from the harness+model it will actually
    /// use (STUDIO-957). The budget gate's key.
    pub(crate) fn projected_provider(
        &self,
        harness: &str,
        model_override: &rhapsody_agent::ModelOverride,
        project_slug: &str,
    ) -> String {
        let (harness, model) = self.resolved_harness_model(harness, model_override, project_slug);
        derive_provider(&harness, &model)
    }

    /// The model the ACTUAL harness would run with when nothing overrides it: the owning project's
    /// materialized value when the slug resolves, else the top-level one. `codex` has no model knob
    /// in this build, and an unknown harness has none either, so both answer empty rather than a
    /// guess.
    fn configured_model_for(&self, project_slug: &str, harness: &str) -> String {
        let Some(eff) = self.eff.as_ref() else {
            return String::new();
        };
        let project = eff.project_by_slug(project_slug);
        match harness {
            "claude" => project.map_or_else(
                || eff.cfg.claude.model.clone(),
                |p| p.mcfg.claude.model.clone(),
            ),
            "opencode" => project.map_or_else(
                || eff.cfg.opencode.model.clone(),
                |p| p.mcfg.opencode.model.clone(),
            ),
            _ => String::new(),
        }
    }

    /// Records the terminal outcome + end time + final tallies for a run. A zero `run_id` (store
    /// disabled / `StartRun` failed) is a no-op. Also expires any still-`sent` operator messages so
    /// the UI/GET don't show them as forever-pending (INF-250) — this is the single end-of-run point
    /// every termination path flows through. Mirrors Go `persistEndRun`.
    pub fn persist_end_run(&self, re: &RunningEntry, outcome: &str, err_str: &str) {
        // The per-run operator mailbox (INF-250) dies with the run — drop it at this single end-of-run
        // chokepoint every termination path flows through, BEFORE the store-gated early return below
        // (the mailbox is independent of the store). Mirrors Go's `runningEntry.mailbox` being GC'd
        // once the entry leaves `o.running`.
        self.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&re.issue.id);
        // STUDIO-957: the per-provider budget's cached spend must see the total this run just
        // closed rather than a stale window — the cache's own doc (`BudgetLedger::invalidate_spend`)
        // names this call, and alice round 1 finding 2 was that it had none. Before the `run_id`
        // gate: a run that recorded no row left the spend unmoved, so this is a harmless no-op that
        // keeps the "every end-of-run path" promise the mailbox drop above already makes.
        self.budget_ledger.invalidate_spend();
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

    /// Inserts a new operator-message row (status "sent") with the operator's ORIGINAL text, returning
    /// the row id (0 when persistence is disabled / `run_id == 0`). Best-effort: a failure is logged
    /// and the admission still succeeds (the message is already on the mailbox). Mirrors Go
    /// `persistRunMessage` (INF-250).
    pub(crate) fn persist_run_message(&self, re: &RunningEntry, body: &str) -> i64 {
        if re.run_id == 0 {
            return 0;
        }
        match self
            .store
            .insert_run_message(re.run_id, body, (self.now)().timestamp_millis())
        {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist run message failed");
                0
            }
        }
    }

    /// Marks the OLDEST still-"sent" message for a run delivered with the turn it was folded into (FIFO
    /// matches the mailbox order). Best-effort / no-op on `run_id == 0`. Mirrors Go
    /// `persistRunMessageDelivered` (INF-250).
    pub(crate) fn persist_run_message_delivered(&self, re: &RunningEntry, turn: i64) {
        if re.run_id == 0 {
            return;
        }
        if let Err(e) = self
            .store
            .mark_oldest_run_message_delivered(re.run_id, turn)
        {
            tracing::error!(issue_identifier = %re.issue.identifier, error = %e, "persist run message delivered failed");
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
    use rhapsody_agent::{EVENT_NOTIFICATION, EVENT_TURN_COMPLETED, Event, Usage};
    use rhapsody_store::{
        CLAIM_RETRY_QUEUED, CLAIM_RUNNING, OUTCOME_COMPLETED, OUTCOME_CONTINUED, OUTCOME_FAILED,
        OUTCOME_RUNNING, OUTCOME_STOPPED, OUTCOME_TOKEN_CEILING, RUN_MESSAGE_EXPIRED, RunFilter,
    };

    use super::*;
    use crate::agentupdate::AgentUpdate;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{issue, orch_with_store, running_entry};

    fn re_for(id: &str, ident: &str, state: &str) -> RunningEntry {
        running_entry(issue(id, ident, state), "", "")
    }

    /// The provider is derived from the model string the CLI was handed, not from the harness alone:
    /// opencode names models `provider/model`, so that prefix is the authority. Claude models carry
    /// no slash, so that one harness is named explicitly. Anything genuinely unknowable answers
    /// empty — the console renders "unknown" rather than inventing a provider.
    #[test]
    fn derive_provider_reads_the_model_string_then_the_one_known_harness() {
        assert_eq!(
            derive_provider("opencode", "fireworks-ai/accounts/fireworks/models/x"),
            "fireworks-ai"
        );
        assert_eq!(
            derive_provider("opencode", "openrouter/anthropic/claude"),
            "openrouter"
        );
        assert_eq!(derive_provider("claude", "claude-sonnet-4"), "anthropic");
        assert_eq!(
            derive_provider("claude", "anthropic/claude-sonnet-4"),
            "anthropic",
            "a slash-bearing model names its provider even on claude"
        );
        assert_eq!(derive_provider("opencode", "bare-model"), "", "no guessing");
        assert_eq!(derive_provider("claude", ""), "");
        assert_eq!(derive_provider("opencode", "  "), "");
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

    // PB7 (STUDIO-1002): a brokered run's finalized usage lands in its own `rhapsody_run_usage` row,
    // keyed by the store run id, and REPLACES the run's child-reported token tallies with the broker
    // figures (design §7.3). The child here reports 1000 tokens; the broker receipt reports 42, and
    // the run must record 42. MUTATION GUARDS: dropping `set_run_usage` loses the usage row; not
    // replacing leaves the child's 1000 in the run row and the per-provider budget path.
    #[test]
    fn on_broker_usage_writes_a_usage_row_and_replaces_child_usage() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        re.brokered = true; // the fixture must model a prepared (brokered) dispatch
        o.persist_start_run(&mut re, 0);
        let run_id = re.run_id;
        let issue_id = re.issue.id.clone();
        o.running.insert(issue_id.clone(), re);
        // The child's turn_completed result commits its own token figure — on the ENTRY and, being
        // brokered, deliberately NOT into the cumulative aggregate (STUDIO-1047).
        o.on_agent_update(AgentUpdate {
            issue_id: issue_id.clone(),
            ev: Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                usage: Some(Usage {
                    input_tokens: 700,
                    output_tokens: 300,
                    total_tokens: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        assert_eq!(
            o.running[&issue_id].total_tokens, 1000,
            "the child usage is committed on the entry before the receipt arrives"
        );
        assert_eq!(
            o.totals.total_tokens, 0,
            "a brokered run's child figure is not folded into the aggregate"
        );

        let usage = store::RunUsage {
            provider_reported_tokens: Some(42),
            reserved_tokens: 900,
            usage_authority: store::USAGE_AUTHORITY_PROVIDER_REPORTED_UNVERIFIED.to_string(),
            usage_incomplete: true,
            unknown_usage_requests: 2,
        };
        o.on_broker_usage(&issue_id, run_id, &usage);

        let got = st
            .run_usage(run_id)
            .expect("read usage")
            .expect("a usage row");
        assert_eq!(got, usage);
        // The run's committed tally is the broker's report, not the child's 1000, and the aggregate
        // follows the replacement.
        assert_eq!(o.running[&issue_id].total_tokens, 42);
        assert_eq!(o.totals.total_tokens, 42);

        // End the run: the `runs` row (and thus the per-provider budget) records the broker figure.
        let re = o.running.get(&issue_id).expect("live entry").clone();
        o.persist_end_run(&re, OUTCOME_COMPLETED, "");
        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(
            runs[0].total_tokens, 42,
            "the run records the finalized broker receipt, not the child usage"
        );
    }

    // STUDIO-1002 review A2: a production cancellation (`terminate`) removes the running entry
    // BEFORE the worker's future drops, so the finalized receipt arrives with no live entry to
    // resolve a run id from. Carrying the run id on the event persists the cancellation receipt
    // anyway. This reds if `on_broker_usage` looks the run id up from `self.running`.
    #[test]
    fn a_cancelled_runs_broker_receipt_is_persisted_after_terminate() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        o.persist_start_run(&mut re, 0);
        let run_id = re.run_id;
        let issue_id = re.issue.id.clone();
        o.running.insert(issue_id.clone(), re);

        assert!(
            o.terminate(&issue_id).is_some(),
            "the fixture run must be running to cancel"
        );
        assert!(
            !o.running.contains_key(&issue_id),
            "terminate removed the entry"
        );

        let usage = store::RunUsage {
            reserved_tokens: 0,
            usage_incomplete: true,
            ..Default::default()
        };
        o.on_broker_usage(&issue_id, run_id, &usage);

        let got = st.run_usage(run_id).expect("read usage");
        assert_eq!(
            got,
            Some(usage),
            "a cancelled brokered run's finalized receipt must be persisted"
        );
    }

    // A zero run id (store off / StartRun failed) is a no-op rather than a panic or a wrong-run write.
    #[test]
    fn on_broker_usage_with_a_zero_run_id_is_a_noop() {
        let (mut o, _st) = orch_with_store();
        let usage = store::RunUsage::default();
        o.on_broker_usage("ghost", 0, &usage); // must not panic
    }

    // LEGACY accounting is unchanged (STUDIO-1047 acceptance): a non-brokered run never receives an
    // `Event::BrokerUsage`, so its child figures stand on both the run row and the aggregate exactly
    // as before the broker feature existed.
    #[test]
    fn legacy_run_accounting_is_unchanged() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "Todo");
        o.persist_start_run(&mut re, 0);
        let issue_id = re.issue.id.clone();
        o.running.insert(issue_id.clone(), re);
        o.on_agent_update(AgentUpdate {
            issue_id: issue_id.clone(),
            ev: Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                usage: Some(Usage {
                    input_tokens: 700,
                    output_tokens: 300,
                    total_tokens: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        assert_eq!(
            o.totals.total_tokens, 1000,
            "a legacy run's child figure IS folded into the aggregate"
        );

        let re = o.terminate(&issue_id).expect("running");
        o.persist_end_run(&re, OUTCOME_COMPLETED, "");

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(
            runs[0].total_tokens, 1000,
            "legacy row keeps the child total"
        );
        assert_eq!(runs[0].input_tokens, 700);
        assert_eq!(runs[0].output_tokens, 300);
        assert_eq!(o.totals.total_tokens, 1000, "legacy aggregate is unchanged");
    }

    // alice's STUDIO-1002 round-3 reproduction, as a real test (STUDIO-1047). A production
    // cancellation (`terminate` + `persist_end_run`) closes the run row with the CHILD's committed
    // figure synchronously, before the worker's `Event::BrokerUsage` arrives. The receipt must still
    // replace that figure on the row — which the per-provider budget (`tokens_by_provider`) reads —
    // and correct the aggregate. MUTATION GUARD: reverting `replace_child_usage_with_broker` to its
    // live-entry-only form leaves the row at 1000 and reds this test.
    #[test]
    fn a_cancelled_brokered_run_records_the_receipt_not_the_child_usage() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        re.brokered = true; // alice's reproduction is a prepared (brokered) run
        o.persist_start_run(&mut re, 0);
        let run_id = re.run_id;
        let issue_id = re.issue.id.clone();
        o.running.insert(issue_id.clone(), re);
        // The child's turn_completed commits its own token figure into the entry.
        o.on_agent_update(AgentUpdate {
            issue_id: issue_id.clone(),
            ev: Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                usage: Some(Usage {
                    input_tokens: 700,
                    output_tokens: 300,
                    total_tokens: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        assert_eq!(
            o.totals.total_tokens, 0,
            "a brokered child figure never enters the aggregate"
        );

        // operator Stop: terminate then persist_end_run, all before the receipt arrives.
        let re = o.terminate(&issue_id).expect("running");
        o.persist_end_run(&re, OUTCOME_STOPPED, "stopped by user");
        assert_eq!(
            st.list_runs(RunFilter::default()).expect("list runs")[0].total_tokens,
            1000,
            "the child figure is what the closed run row records first"
        );

        let usage = store::RunUsage {
            provider_reported_tokens: Some(42),
            reserved_tokens: 900,
            usage_authority: store::USAGE_AUTHORITY_PROVIDER_REPORTED_UNVERIFIED.to_string(),
            usage_incomplete: true,
            unknown_usage_requests: 2,
        };
        o.on_broker_usage(&issue_id, run_id, &usage);

        assert_eq!(st.run_usage(run_id).expect("read usage"), Some(usage));
        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(
            runs[0].total_tokens, 42,
            "the closed run row records the broker receipt, not the child's 1000"
        );
        assert_eq!(
            runs[0].input_tokens, 0,
            "the receipt carries no input split"
        );
        assert_eq!(
            runs[0].output_tokens, 0,
            "the receipt carries no output split"
        );
        assert!(
            !runs[0].usage_estimated,
            "a broker receipt is authoritative"
        );
        assert_eq!(
            o.totals.total_tokens, 42,
            "the aggregate follows the receipt"
        );
        assert_eq!(
            st.load_totals().expect("load totals").total_tokens,
            42,
            "the durable aggregate is converged at receipt time, not deferred"
        );
    }

    // The same correction on EVERY production cancellation path, each of which does exactly what
    // `stop.rs` / `reconcile_run.rs` / `agentupdate.rs` do: `terminate` then `persist_end_run` with
    // that path's outcome, before the receipt arrives (STUDIO-1047 acceptance).
    #[test]
    fn every_cancellation_path_records_the_receipt_not_the_child_usage() {
        for (outcome, reason) in [
            (OUTCOME_STOPPED, "stopped by user"),
            (OUTCOME_FAILED, "stalled"),
            (
                OUTCOME_TOKEN_CEILING,
                "stopped at its per-run token ceiling",
            ),
        ] {
            let (mut o, st) = orch_with_store();
            let mut re = re_for("ID-1", "MT-1", "In Progress");
            re.brokered = true;
            o.persist_start_run(&mut re, 0);
            let run_id = re.run_id;
            let issue_id = re.issue.id.clone();
            o.running.insert(issue_id.clone(), re);
            o.on_agent_update(AgentUpdate {
                issue_id: issue_id.clone(),
                ev: Event {
                    event_type: EVENT_TURN_COMPLETED.to_string(),
                    usage: Some(Usage {
                        total_tokens: 1000,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            });

            let re = o.terminate(&issue_id).expect("running");
            o.persist_end_run(&re, outcome, reason);
            o.on_broker_usage(
                &issue_id,
                run_id,
                &store::RunUsage {
                    provider_reported_tokens: Some(42),
                    reserved_tokens: 900,
                    ..Default::default()
                },
            );

            let runs = st.list_runs(RunFilter::default()).expect("list runs");
            assert_eq!(
                runs[0].total_tokens, 42,
                "{outcome}/{reason}: the row must record the receipt, not the child's 1000"
            );
            assert_eq!(runs[0].outcome, outcome, "{outcome}: outcome is preserved");
            assert_eq!(o.totals.total_tokens, 42, "{outcome}: aggregate");
        }
    }

    // The receipt's fallback rule is unchanged on the cancelled path too: with the broker unable to
    // report, the run is charged the conservative RESERVATION, never left at the child's figure and
    // never zeroed. The row is rewritten even though it was already closed.
    #[test]
    fn a_cancelled_broker_receipt_with_no_report_charges_the_reservation() {
        let (mut o, st) = orch_with_store();
        let mut re = re_for("ID-1", "MT-1", "In Progress");
        re.brokered = true;
        o.persist_start_run(&mut re, 0);
        let run_id = re.run_id;
        let issue_id = re.issue.id.clone();
        o.running.insert(issue_id.clone(), re);
        o.on_agent_update(AgentUpdate {
            issue_id: issue_id.clone(),
            ev: Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                usage: Some(Usage {
                    total_tokens: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        let re = o.terminate(&issue_id).expect("running");
        o.persist_end_run(&re, OUTCOME_STOPPED, "stopped by user");

        o.on_broker_usage(
            &issue_id,
            run_id,
            &store::RunUsage {
                provider_reported_tokens: None,
                reserved_tokens: 900,
                usage_incomplete: true,
                ..Default::default()
            },
        );

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        assert_eq!(
            runs[0].total_tokens, 900,
            "an unreported receipt charges the reservation, not the child figure or zero"
        );
    }

    // STUDIO-1047 (alice's review F1): with the store OFF (`run_id == 0`) a cancelled brokered run
    // has no row to rewrite, but its receipt must still reach the in-memory aggregate — its child
    // figure was never folded in, so without the settlement the run counts as zero. MUTATION GUARD:
    // restoring the `run_id == 0` early return above the settlement reds this.
    #[test]
    fn a_cancelled_brokered_receipt_reaches_the_aggregate_with_the_store_off() {
        let mut o = Orchestrator::new("WORKFLOW.md"); // no store injected => Noop
        o.on_broker_usage(
            "ghost",
            0,
            &store::RunUsage {
                provider_reported_tokens: Some(42),
                reserved_tokens: 900,
                ..Default::default()
            },
        );
        assert_eq!(
            o.totals.total_tokens, 42,
            "a store-off cancelled run's receipt still settles the aggregate"
        );
    }

    // STUDIO-1047 (alice's review F2): a terminated run's receipt can arrive after the ticket was
    // re-dispatched, so the live entry under that issue id belongs to a DIFFERENT run. Matching on
    // `issue_id` alone would let the stale receipt rewrite the new entry while the old run's row kept
    // its child figure. MUTATION GUARD: dropping the `re.run_id == run_id` check reds this.
    #[test]
    fn a_late_receipt_for_an_earlier_run_does_not_land_on_a_redispatched_entry() {
        let (mut o, st) = orch_with_store();
        // Run A: terminated and closed with its child figure.
        let mut a = re_for("ID-1", "MT-1", "In Progress");
        a.brokered = true;
        o.persist_start_run(&mut a, 0);
        let run_a = a.run_id;
        let issue_id = a.issue.id.clone();
        o.running.insert(issue_id.clone(), a);
        let a = o.terminate(&issue_id).expect("running");
        o.persist_end_run(&a, OUTCOME_STOPPED, "stopped by user");

        // The ticket is re-dispatched: run B is live under the SAME issue id.
        let mut b = re_for("ID-1", "MT-1", "In Progress");
        b.brokered = true;
        o.persist_start_run(&mut b, 0);
        let run_b = b.run_id;
        assert_ne!(run_a, run_b, "the two runs have distinct row ids");
        o.running.insert(issue_id.clone(), b);

        // A's receipt arrives late, carrying A's run id.
        o.on_broker_usage(
            &issue_id,
            run_a,
            &store::RunUsage {
                provider_reported_tokens: Some(42),
                reserved_tokens: 900,
                ..Default::default()
            },
        );

        let runs = st.list_runs(RunFilter::default()).expect("list runs");
        let row_a = runs.iter().find(|r| r.id == run_a).expect("run A row");
        assert_eq!(
            row_a.total_tokens, 42,
            "the stale receipt still corrects ITS OWN run's row"
        );
        assert_eq!(
            o.running[&issue_id].total_tokens, 0,
            "the receipt must not land on the re-dispatched entry"
        );
        assert_eq!(
            o.totals.total_tokens, 42,
            "the aggregate takes A's receipt exactly once"
        );
    }
}
