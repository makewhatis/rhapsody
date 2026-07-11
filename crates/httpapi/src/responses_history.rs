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

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_orchestrator::{EventRecord, RunningRow};
use rhapsody_store::{DayRollup, EventHit, EventRow, RunSummary};
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
