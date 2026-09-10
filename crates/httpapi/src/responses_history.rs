//! responses_history — the history + run-detail wire views. Parity port of Go
//! `$REF/internal/httpapi/responses_history.go` (`runDetailJSON` + `toRunDetailFrom*` +
//! `eventRowsToRecentJSON` + the `historyResponse`/`issueHistoryResponse`/`runEventsResponse`/
//! `runTranscriptJSON`/`eventSearchResponse`/`metricsResponse` envelopes) and the `eventRecordJSON`/
//! `logEntryJSON` DTOs of `responses.go`.
//!
//! Following the [`crate`]'s established convention (H1's `snapshot_json::render`, the config crate's
//! `effective_json`), each view is built as a [`serde_json::Value`] rather than a `#[derive(Serialize)]`
//! DTO: the domain [`rhapsody_store`] types already carry the wire field names (their Go `json:` tags),
//! and building the value directly (a) reuses those types with no parallel struct to drift, and (b)
//! serializes an empty [`Vec`] as `[]` (never `null`) intrinsically — the guarantee Go must hand-write
//! a `MarshalJSON` for on every list envelope.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_orchestrator::{EventRecord, IssueLifecycleRow, RunningRow, review};
use rhapsody_store::{DayRollup, DayTotals, EventHit, EventRow, RunSummary};
use serde_json::{Value, json};

/// Bounds the activity timeline a finished run's detail carries, matching the live snapshot's
/// recent-events ring (orchestrator `maxRecentEvents`) so both render the same "recent activity"
/// depth. The full stream stays at `/runs/{id}/events`. Mirrors Go `maxRecentActivity`.
const MAX_RECENT_ACTIVITY: usize = 50;

/// Formats `t` as RFC3339 (UTC, seconds precision), or `""` when `t` is the zero instant (Unix epoch,
/// the entry constructors' default) — the SPA's bare-RFC3339-or-`""` contract (NOT null). Mirrors Go
/// `rfc3339OrEmpty`; the same rule as H1's private `snapshot_json::rfc3339_or_empty` (not re-exported,
/// so mirrored here for the live run-detail path).
fn rfc3339_or_empty(t: DateTime<Utc>) -> String {
    if t.timestamp() == 0 && t.timestamp_subsec_nanos() == 0 {
        String::new()
    } else {
        t.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

/// One recent-activity row `{at, event, message}`. Shared shape for the live ring ([`EventRecord`],
/// whose `at` is a zero-able instant) and a finished run's coarse events. Mirrors Go `eventRecordJSON`.
fn event_record_from_live(r: &EventRecord) -> Value {
    json!({ "at": rfc3339_or_empty(r.at), "event": r.event, "message": r.message })
}

/// A finished run's coarse events normalized into the live activity-timeline shape (`event=kind`,
/// `message=text`), keeping only the most recent [`MAX_RECENT_ACTIVITY`] entries. Relies on rows
/// arriving OLDEST-first (the store's `run_events` orders by seq), so the tail slice is the most-recent
/// window — the client renders newest-first, matching the live ring. Mirrors Go `eventRowsToRecentJSON`.
fn events_recent_from_rows(rows: &[EventRow]) -> Value {
    let tail = if rows.len() > MAX_RECENT_ACTIVITY {
        &rows[rows.len() - MAX_RECENT_ACTIVITY..]
    } else {
        rows
    };
    Value::Array(
        tail.iter()
            .map(|r| json!({ "at": r.at, "event": r.kind, "message": r.text }))
            .collect(),
    )
}

/// The `GET /api/v1/runs/{id}` payload from a LIVE snapshot row: `outcome` is "running", live
/// telemetry (turn/tokens/state/recent_events) comes straight from the snapshot, `ended_at` is empty.
/// Mirrors Go `toRunDetailFromRunning`.
pub(crate) fn run_detail_from_running(r: &RunningRow, now: &str) -> Value {
    json!({
        "run_id": r.run_id,
        "issue_id": r.issue_id,
        "issue_identifier": r.issue_identifier,
        "title": r.title,
        "project": r.project,
        "repo": r.repo,
        "attempt": r.attempt,
        "outcome": rhapsody_store::OUTCOME_RUNNING,
        "live": true,
        "issue_state": r.state,
        "last_codex_event": r.last_event,
        "turn_count": r.turn_count,
        "input_tokens": r.tokens.input_tokens,
        "output_tokens": r.tokens.output_tokens,
        "total_tokens": r.tokens.total_tokens,
        "usage_estimated": r.usage_estimated,
        "started_at": rfc3339_or_empty(r.started_at),
        "ended_at": "",
        "last_event_at": rfc3339_or_empty(r.last_event_at),
        "error": "",
        "recent_events": Value::Array(r.recent_events.iter().map(event_record_from_live).collect()),
        "generated_at": now,
    })
}

/// The `GET /api/v1/runs/{id}` payload from a FINISHED history row + its coarse events. The live-only
/// fields (`issue_state`/`last_codex_event`/`last_event_at`) are blank; `outcome` is the terminal
/// disposition and `live` is false. Mirrors Go `toRunDetailFromSummary`.
pub(crate) fn run_detail_from_summary(run: &RunSummary, events: &[EventRow], now: &str) -> Value {
    json!({
        "run_id": run.id,
        "issue_id": run.issue_id,
        "issue_identifier": run.issue_identifier,
        "title": run.title,
        "project": run.project_slug,
        "repo": run.repo,
        "attempt": run.attempt,
        "outcome": run.outcome,
        "live": false,
        "issue_state": "",
        "last_codex_event": "",
        "turn_count": run.turns,
        "input_tokens": run.input_tokens,
        "output_tokens": run.output_tokens,
        "total_tokens": run.total_tokens,
        "usage_estimated": run.usage_estimated,
        "started_at": run.started_at,
        "ended_at": run.ended_at,
        "last_event_at": "",
        "error": run.error,
        "recent_events": events_recent_from_rows(events),
        "generated_at": now,
    })
}

/// A run row on the wire (`store.RunSummary` serialized). All 20 fields, exactly Go's `json:` tags.
pub(crate) fn run_summary_json(r: &RunSummary) -> Value {
    json!({
        "id": r.id,
        "issue_id": r.issue_id,
        "issue_identifier": r.issue_identifier,
        "title": r.title,
        "attempt": r.attempt,
        "session_uuid": r.session_uuid,
        "branch": r.branch,
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "outcome": r.outcome,
        "turns": r.turns,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "total_tokens": r.total_tokens,
        "usage_estimated": r.usage_estimated,
        "error": r.error,
        "transcript_path": r.transcript_path,
        "project_slug": r.project_slug,
        "repo": r.repo,
        "team_id": r.team_id,
    })
}

/// One captured event on the wire (`store.EventRow` serialized). Mirrors Go's `EventRow` json tags.
pub(crate) fn event_row_json(e: &EventRow) -> Value {
    json!({ "seq": e.seq, "at": e.at, "kind": e.kind, "tool": e.tool, "text": e.text })
}

/// One event-search hit on the wire (`store.EventHit` serialized): the event plus its run's identity.
pub(crate) fn event_hit_json(h: &EventHit) -> Value {
    json!({
        "run_id": h.run_id,
        "issue_identifier": h.issue_identifier,
        "seq": h.seq,
        "at": h.at,
        "kind": h.kind,
        "tool": h.tool,
        "text": h.text,
    })
}

/// One per-day metrics rollup on the wire (`store.DayRollup` serialized).
pub(crate) fn day_rollup_json(d: &DayRollup) -> Value {
    json!({
        "date": d.date,
        "runs": d.runs,
        "completed": d.completed,
        "failed": d.failed,
        "total_tokens": d.total_tokens,
    })
}

/// `{runs:[…], next_offset:…}` — the `GET /api/v1/history` payload. `runs` serializes as `[]` when
/// empty; `next_offset` is null unless a bounded full page was returned. Mirrors Go `historyResponse`.
pub(crate) fn history_response(runs: &[RunSummary], next_offset: Option<i64>) -> Value {
    json!({
        "runs": Value::Array(runs.iter().map(run_summary_json).collect()),
        "next_offset": next_offset,
    })
}

/// `{issues:[…], next_offset:…}` — the `GET /api/v1/history/issues` payload (TRA-320). Each entry is
/// a full run summary: the LATEST run of one issue. Same envelope shape as `history_response`, under
/// a distinct key so a client can never mistake an issue-paged listing for a run-paged one.
/// Rhapsody-only — Go has no issue-level listing.
///
/// Each entry additionally carries the TICKET's current lifecycle when `lifecycles` has an answer
/// for its `issue_id` (STUDIO-702): `tracker_state`, the workflow-state name verbatim, and
/// `lifecycle`, the normalized `open`/`in_review`/`done`/`canceled` the dashboard paints its status
/// Pill from. Both are OMITTED rather than blanked when there is no answer, so "the daemon could not
/// resolve this ticket" stays distinguishable from any state it could have resolved.
///
/// It carries `review_ticket: true` when `reviews` holds the row's issue id (STUDIO-780): this
/// ticket's own job is to review somebody else's work, which is what lets the console say
/// "reviewing" where it would otherwise say "in review" — two different claims that read
/// identically without it. Only the POSITIVE is serialized, and the omission is honest rather than
/// lazy: the daemon cannot tell an ordinary ticket from one it could not resolve or one minted
/// before the marker label existed, and all three mean the same thing to the client — paint this
/// row exactly as it was painted before the field existed.
///
/// It carries `review_run: true` when the row's own run is a REVIEW RUN — a run dispatched against
/// a synthetic `pr:owner/repo#n@reviewer` issue rather than a tracker ticket (STUDIO-826). This is
/// the sibling fact to `review_ticket`, not a restatement of it: `review_ticket` says the TICKET's
/// job is to review somebody's work, while a ticketless review job has no ticket at all, so no
/// label could ever mark it and no lifecycle will ever be resolved for it. Without the field the
/// console has only the run outcome, and `completed` there reads "awaiting a reviewer" — the exact
/// inverse of what a finished review means.
///
/// It is read off `issue_id`, which for such a run IS the `pr:` key — the same predicate the
/// orchestrator's own dispatch and retry paths key on ([`review::is_review_key`]) — and never off
/// the title: `Review owner/repo#n at <sha>` is a string the daemon happens to mint, not a fact
/// about the run. Positive-only for the same reason `review_ticket` is.
///
/// It carries `assignee` on the same terms when `assignees` names one (STUDIO-735): the teammate
/// the ticket's newest run was dispatched under, which is what keeps a finished job attributed
/// after its teammate has left the live roster. A ticket nobody was routed for — solo, unrouted, or
/// a Teams-off daemon — carries NO field rather than an empty one, for the same reason: "nobody ran
/// this as a teammate" and "the daemon has no answer" both fall back to the same client behaviour,
/// and neither is a name.
///
/// The decoration is applied HERE and not in [`run_summary_json`] deliberately: that renderer is
/// byte-pinned to the Go daemon's `/api/v1/history` golden, and this listing is the Rhapsody-only
/// endpoint that may grow fields.
pub(crate) fn issue_runs_response(
    runs: &[RunSummary],
    next_offset: Option<i64>,
    lifecycles: &HashMap<String, IssueLifecycleRow>,
    assignees: &HashMap<String, String>,
    reviews: &HashSet<String>,
) -> Value {
    let issues: Vec<Value> = runs
        .iter()
        .map(|r| {
            let mut row = run_summary_json(r);
            let Some(obj) = row.as_object_mut() else {
                return row;
            };
            if let Some(life) = lifecycles.get(&r.issue_id) {
                obj.insert("tracker_state".to_string(), json!(life.state));
                obj.insert("lifecycle".to_string(), json!(life.lifecycle.as_str()));
            }
            if let Some(name) = assignees.get(&r.issue_id).filter(|n| !n.is_empty()) {
                obj.insert("assignee".to_string(), json!(name));
            }
            if reviews.contains(&r.issue_id) {
                obj.insert("review_ticket".to_string(), json!(true));
            }
            // No lookup: a review run announces itself in the id it was dispatched under, which is
            // why this one needs no provider surface beside the three above.
            if review::is_review_key(&r.issue_id) {
                obj.insert("review_run".to_string(), json!(true));
            }
            row
        })
        .collect();
    json!({
        "issues": Value::Array(issues),
        "next_offset": next_offset,
    })
}

/// The distinct combination of STATUS INPUTS one issue carries — the key the whole-store per-status
/// tally in [`issue_counts_response`] groups by (STUDIO-828).
///
/// It is deliberately the run/ticket FACTS the issue listing already serves per row, and not the
/// console's own vocabulary. The console maps this tuple onto the word its Pill paints
/// (`consoleJobStatus`, `web/src/lib/console-jobs.ts`) and onto its "Needs you" flag
/// (`needsOperator`), and that mapping stays in ONE place — the client — precisely so the strip's
/// count and the table's pill can never be derived by two rules that drift. The daemon's job here is
/// the part the client genuinely cannot do: fold every issue in the store, not the page it fetched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IssueStatusKey {
    /// The run outcome the console's `jobStatus` would see for this issue — the stored row's
    /// `outcome`, or `running` when the live snapshot has the ticket in flight or parked for retry.
    pub outcome: String,
    /// The normalized tracker lifecycle, or `None` when the daemon resolved none — the same absence
    /// the per-row `lifecycle` field expresses by omission.
    pub lifecycle: Option<String>,
    pub review_ticket: bool,
    pub review_run: bool,
}

/// `{issues, buckets:[…]}` — the `GET /api/v1/history/issues/counts` payload (STUDIO-828).
///
/// `issues` is how many issues the tally covers, and equals the sum of the buckets' counts; it is
/// carried rather than left to be summed because "the whole store" is the claim this endpoint
/// exists to make, and a client that wants to check the claim should not have to reconstruct it.
///
/// Each bucket serializes its key exactly as the per-row listing serializes the same facts —
/// `lifecycle` present only when resolved, `review_ticket`/`review_run` positive-only — so the two
/// endpoints speak one vocabulary and a client reads a bucket with the code it already has for a
/// row. The array is ordered by the key, so the payload is stable for a given store.
///
/// Rhapsody-only; Go has neither the issue listing nor an aggregate over it.
pub(crate) fn issue_counts_response(buckets: &BTreeMap<IssueStatusKey, i64>) -> Value {
    let mut issues: i64 = 0;
    let mut out: Vec<Value> = Vec::with_capacity(buckets.len());
    for (key, count) in buckets {
        issues += *count;
        let mut obj = serde_json::Map::new();
        obj.insert("outcome".to_string(), json!(key.outcome));
        if let Some(life) = &key.lifecycle {
            obj.insert("lifecycle".to_string(), json!(life));
        }
        if key.review_ticket {
            obj.insert("review_ticket".to_string(), json!(true));
        }
        if key.review_run {
            obj.insert("review_run".to_string(), json!(true));
        }
        obj.insert("count".to_string(), json!(count));
        out.push(Value::Object(obj));
    }
    json!({
        "issues": issues,
        "buckets": Value::Array(out),
    })
}

/// The `GET /api/v1/history/summary` payload (TRA-320): whole-store totals over the runs that
/// started at or after `since`, echoed back so the client can confirm which window it was served.
/// `total_tokens` is the cache-INCLUSIVE billed total (`cached = total − in − out`), unchanged in
/// meaning from the per-run column. `rhythm` is the most recent runs' `total_tokens`, oldest→newest.
/// Rhapsody-only — Go has no day-summary endpoint.
pub(crate) fn history_summary_response(since: &str, t: &DayTotals, rhythm: &[i64]) -> Value {
    json!({
        "since": since,
        "runs": t.runs,
        "completed": t.completed,
        "input_tokens": t.input_tokens,
        "output_tokens": t.output_tokens,
        "total_tokens": t.total_tokens,
        "seconds": t.seconds,
        "rhythm": Value::Array(rhythm.iter().map(|v| json!(v)).collect()),
    })
}

/// `{issue_identifier, runs:[…]}` — the `GET /api/v1/issues/{id}/history` payload. Mirrors Go
/// `issueHistoryResponse`.
pub(crate) fn issue_history_response(identifier: &str, runs: &[RunSummary]) -> Value {
    json!({
        "issue_identifier": identifier,
        "runs": Value::Array(runs.iter().map(run_summary_json).collect()),
    })
}

/// `{run_id, events:[…]}` — the `GET /api/v1/runs/{id}/events` payload. Mirrors Go `runEventsResponse`.
pub(crate) fn run_events_response(run_id: i64, events: &[EventRow]) -> Value {
    json!({
        "run_id": run_id,
        "events": Value::Array(events.iter().map(event_row_json).collect()),
    })
}

/// `{hits:[…]}` — the `GET /api/v1/events` payload. Mirrors Go `eventSearchResponse`.
pub(crate) fn event_search_response(hits: &[EventHit]) -> Value {
    json!({ "hits": Value::Array(hits.iter().map(event_hit_json).collect()) })
}

/// `{days:[…]}` — the `GET /api/v1/metrics` payload. Mirrors Go `metricsResponse`.
pub(crate) fn metrics_response(days: &[DayRollup]) -> Value {
    json!({ "days": Value::Array(days.iter().map(day_rollup_json).collect()) })
}

/// `{run_id, entries:[{seq,kind,tool,text}], generated_at}` — the `GET /api/v1/runs/{id}/transcript`
/// payload. `entries` mirrors the live `/log` shape so the shared frontend renderer is fed the same
/// `LogEntry`; `seq` is 1-based, assigned here after the orchestrator's cap. Mirrors Go
/// `runTranscriptJSON` (+ `logEntryJSON`).
pub(crate) fn run_transcript_json(
    run_id: i64,
    entries: &[rhapsody_agent::LogEntry],
    now: &str,
) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(
            |(i, e)| json!({ "seq": i as i64 + 1, "kind": e.kind, "tool": e.tool, "text": e.text }),
        )
        .collect();
    json!({ "run_id": run_id, "entries": Value::Array(entries), "generated_at": now })
}
