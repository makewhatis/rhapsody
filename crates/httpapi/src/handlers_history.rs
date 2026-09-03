//! handlers_history — the history + run-detail read handlers. Parity port of Go
//! `$REF/internal/httpapi/handlers_history.go` (`handleHistory`/`handleIssueHistory`/`handleRunEvents`/
//! `handleRunDetail`/`handleEventSearch`/`handleMetrics` + `parseNonNegInt`). The run-transcript
//! handler (also in that Go file) lands with the transcript surface in [`crate::handlers_history`]'s
//! sibling addition below.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use chrono::{SecondsFormat, Utc};
use rhapsody_orchestrator::IssueKey;
use rhapsody_store::{EventQuery, RunFilter, effective_run_limit};

use crate::handlers::{SNAPSHOT_TIMEOUT, require_get};
use crate::responses::{write_error, write_json};
use crate::responses_history::{
    event_search_response, history_response, history_summary_response, issue_history_response,
    issue_runs_response, metrics_response, run_detail_from_running, run_detail_from_summary,
    run_events_response, run_transcript_json,
};
use crate::server::StateProvider;

/// The `/metrics` default window when `?days=` is omitted (the history page sizes are defaulted by the
/// store itself when limit<=0). Mirrors Go `metricsDefaultDays`.
const METRICS_DEFAULT_DAYS: i64 = 30;

/// `GET /api/v1/history?issue=&outcome=&project=&since=&limit=&offset=`: a paged, filterable list of
/// recent runs. `limit`/`offset` must be non-negative integers when present (else 400).
///
/// `next_offset` is derived from the page size the store ACTUALLY applied — `effective_run_limit`,
/// which resolves an absent/`<=0` limit to the store's own default — not from whether the caller
/// happened to send one. Go computes it from the requested limit alone, so its default path reports
/// `next_offset: null` on a page the store silently truncated and every row past the first page
/// becomes unreachable; that under-reported a 192-run store as 50 runs (TRA-320). This is a
/// deliberate divergence from Go `handleHistory` — see README "Divergences".
pub(crate) async fn handle_history(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let f = match run_filter_from_query(&q) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let (offset, effective_limit) = (f.offset, effective_run_limit(f.limit));
    let runs = match provider.history().list_runs(f) {
        Ok(runs) => runs,
        Err(_) => return store_error("history query failed"),
    };
    write_json(
        StatusCode::OK,
        &history_response(&runs, next_offset(runs.len(), offset, effective_limit)),
    )
}

/// `GET /api/v1/history/issues?issue=&outcome=&project=&since=&limit=&offset=`: the same filters as
/// `/history`, but ONE row per issue — each issue's latest matching run — paged by issue, so an
/// issue in a retry loop occupies one row instead of crowding every other issue off the page
/// (TRA-320). `next_offset` follows the same effective-limit rule as `/history`, counting issues.
///
/// Rhapsody-only: Go has no issue-level listing, and the dashboard's issue-grouped Jobs list used to
/// group a run-paged fetch client-side — which is what made one noisy ticket hide 73 others.
pub(crate) async fn handle_issue_runs(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let f = match run_filter_from_query(&q) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let (offset, effective_limit) = (f.offset, effective_run_limit(f.limit));
    let runs = match provider.history().list_issue_runs(f) {
        Ok(runs) => runs,
        Err(_) => return store_error("issue listing query failed"),
    };
    // The ticket lifecycles that turn a run OUTCOME into a real status (STUDIO-702), and the
    // durable assignee that keeps a finished job attributed (STUDIO-735). Both are asked for
    // exactly the rows this page returned, and both are best-effort: a row with no answer carries
    // no field and renders as it did before the fields existed.
    //
    // The two are decorations of the same page but NOT one lookup: they resolve from different
    // records and are cached separately, so a tracker that cannot say what state a ticket is in
    // must not also erase who worked it.
    let ids: Vec<String> = runs.iter().map(|r| r.issue_id.clone()).collect();
    let keys: Vec<IssueKey> = runs
        .iter()
        .map(|r| IssueKey {
            id: r.issue_id.clone(),
            identifier: r.issue_identifier.clone(),
        })
        .collect();
    let (lifecycles, assignees) = tokio::join!(
        provider.issue_lifecycles(&ids),
        provider.issue_assignees(&keys)
    );
    write_json(
        StatusCode::OK,
        &issue_runs_response(
            &runs,
            next_offset(runs.len(), offset, effective_limit),
            &lifecycles,
            &assignees,
        ),
    )
}

/// `GET /api/v1/history/summary?since=`: whole-store run/token/runtime totals over the runs that
/// STARTED at or after `since`, plus the token totals of the most recent [`SUMMARY_RHYTHM_RUNS`] of
/// them (the dashboard's rhythm sparkline). Backs the header "today" cells, which must never be a
/// fold over one fetched page at any page size (TRA-320).
///
/// `since` is the CALLER's day boundary, which is deliberately a LOCAL one: the dashboard sends its
/// own local midnight (as a UTC RFC3339 instant), preserving the local-day semantics the client-side
/// fold had. Omitting it falls back to the DAEMON host's local midnight — the same wall clock in the
/// single-machine deployment this dashboard serves. Neither path uses a UTC day boundary; that would
/// silently shift the numbers for anyone not on UTC. An unparseable `since` is a 400 rather than a
/// silent whole-table sum. Rhapsody-only; Go has no day-summary endpoint.
pub(crate) async fn handle_history_summary(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let raw_since = qget(&q, "since");
    let since = if raw_since.is_empty() {
        local_day_start()
    } else {
        match chrono::DateTime::parse_from_rfc3339(raw_since) {
            Ok(t) => t
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            Err(_) => {
                return write_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_param",
                    "since must be an RFC3339 timestamp",
                    None,
                );
            }
        }
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let history = provider.history();
    let totals = match history.day_totals(&since, &now) {
        Ok(t) => t,
        Err(_) => return store_error("history summary query failed"),
    };
    // The rhythm series is the ONE figure that is legitimately a bounded window rather than a total:
    // it draws the most recent N runs, so a most-recent-first page of exactly N is the whole answer.
    // Rendered oldest→newest so the last (brightest) bar is the most recent run.
    let recent = match history.list_runs(RunFilter {
        since: since.clone(),
        limit: SUMMARY_RHYTHM_RUNS,
        ..RunFilter::default()
    }) {
        Ok(runs) => runs,
        Err(_) => return store_error("history summary query failed"),
    };
    let rhythm: Vec<i64> = recent.iter().rev().map(|r| r.total_tokens).collect();
    write_json(
        StatusCode::OK,
        &history_summary_response(&since, &totals, &rhythm),
    )
}

/// How many recent runs the summary's rhythm series carries — the dashboard sparkline's bar cap.
const SUMMARY_RHYTHM_RUNS: i64 = 14;

/// `offset + effective_limit` when a FULL page came back (so another page may exist), else `None`.
/// `effective_limit` must be the page size the store actually applied, never the raw request value.
fn next_offset(returned: usize, offset: i64, effective_limit: i64) -> Option<i64> {
    (effective_limit > 0 && returned as i64 == effective_limit).then_some(offset + effective_limit)
}

/// Parse the shared `issue`/`outcome`/`since`/`project`/`limit`/`offset` query params into a
/// [`RunFilter`]. `limit`/`offset` must be non-negative integers when present (else a 400 envelope).
/// Shared by `/history` and `/history/issues`, which take identical filters and differ only in what
/// a page counts.
fn run_filter_from_query(q: &HashMap<String, String>) -> Result<RunFilter, Box<Response>> {
    Ok(RunFilter {
        issue: qget(q, "issue").to_string(),
        outcome: qget(q, "outcome").to_string(),
        since: qget(q, "since").to_string(),
        project: qget(q, "project").to_string(),
        limit: parse_non_neg_int(qget(q, "limit"), "limit")?,
        offset: parse_non_neg_int(qget(q, "offset"), "offset")?,
    })
}

/// Midnight of the DAEMON host's current local day, as a UTC RFC3339 instant — the `/history/summary`
/// fallback when the caller sends no `since`. Local, not UTC: see [`handle_history_summary`].
///
/// Panic-free: the instant is derived by shifting the naive local midnight by the host's CURRENT
/// UTC offset, so there is no ambiguous `LocalResult` to unwrap. On a DST spring-forward day that
/// skips 00:00 the boundary lands an hour off for that one day rather than the handler failing.
fn local_day_start() -> String {
    use chrono::Offset;
    let now = chrono::Local::now();
    let shift = chrono::TimeDelta::try_seconds(now.offset().fix().local_minus_utc() as i64)
        .unwrap_or_default();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc() - shift)
        .unwrap_or_else(Utc::now);
    midnight.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `GET /api/v1/issues/{id}/history?limit=&project=`: a single issue's run history (most-recent
/// first). `limit` must be a non-negative integer when present (else 400). Mirrors Go
/// `handleIssueHistory`.
pub(crate) async fn handle_issue_history(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    if id.is_empty() {
        return write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such issue history",
            None,
        );
    }
    let limit = match parse_non_neg_int(qget(&q, "limit"), "limit") {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let runs = match provider
        .history()
        .issue_history(&id, qget(&q, "project"), limit)
    {
        Ok(runs) => runs,
        Err(_) => return store_error("issue history query failed"),
    };
    write_json(StatusCode::OK, &issue_history_response(&id, &runs))
}

/// `GET /api/v1/runs/{id}/events`: the captured events for one run, ordered by seq. `{id}` must be a
/// valid positive integer run id (else 404) — run id 0 is reserved for the persistence-disabled state.
/// Mirrors Go `handleRunEvents`.
pub(crate) async fn handle_run_events(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let events = match provider.history().run_events(run_id) {
        Ok(events) => events,
        Err(_) => return store_error("run events query failed"),
    };
    write_json(StatusCode::OK, &run_events_response(run_id, &events))
}

/// `GET /api/v1/runs/{id}`: one run's detail, rendered identically whether it is running or finished.
/// Live first — the same control-task snapshot `/state` uses (run rows expose `run_id`) — then the
/// durable history row + its coarse events. A snapshot failure must NOT block a finished run that
/// lives only in persistence, so it falls through to the store rather than 503ing (sibling
/// `/runs/{id}/events` + `/transcript` serve without a snapshot too). `{id}` must be a positive
/// integer (else 404); an unknown run is 404 `run_not_found`. Mirrors Go `handleRunDetail`.
pub(crate) async fn handle_run_detail(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let now = now_rfc3339();

    // Live first: scan the snapshot for the requested run id. A snapshot failure/timeout is logged and
    // falls through to the store (Go's warn-then-continue), NOT a 503.
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, provider.snapshot()).await {
        Ok(Ok(snap)) => {
            if let Some(row) = snap.running.iter().find(|r| r.run_id == run_id) {
                return write_json(StatusCode::OK, &run_detail_from_running(row, &now));
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(run_id, error = %err, "run detail: snapshot unavailable; serving from history store");
        }
        Err(_elapsed) => {
            tracing::warn!(
                run_id,
                "run detail: snapshot timed out; serving from history store"
            );
        }
    }

    // Finished: the durable history row + its coarse events feed the activity timeline.
    let run = match provider.history().get_run(run_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            return write_error(
                StatusCode::NOT_FOUND,
                "run_not_found",
                format!("no run with id: {id}"),
                None,
            );
        }
        Err(_) => return store_error("run lookup failed"),
    };
    // The detail is still useful without the timeline; log + continue empty rather than 500-ing the
    // whole view on a secondary query failure.
    let events = match provider.history().run_events(run_id) {
        Ok(events) => events,
        Err(err) => {
            tracing::error!(run_id, error = ?err, "run detail events lookup failed");
            Vec::new()
        }
    };
    write_json(
        StatusCode::OK,
        &run_detail_from_summary(&run, &events, &now),
    )
}

/// `GET /api/v1/events?q=&issue=&kind=&limit=`: a cross-run substring search over event text. `limit`
/// must be a non-negative integer when present (else 400). Mirrors Go `handleEventSearch`.
pub(crate) async fn handle_event_search(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let limit = match parse_non_neg_int(qget(&q, "limit"), "limit") {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let query = EventQuery {
        text: qget(&q, "q").to_string(),
        issue: qget(&q, "issue").to_string(),
        kind: qget(&q, "kind").to_string(),
        limit,
    };
    let hits = match provider.history().search_events(query) {
        Ok(hits) => hits,
        Err(_) => return store_error("event search failed"),
    };
    write_json(StatusCode::OK, &event_search_response(&hits))
}

/// `GET /api/v1/metrics?days=30&project=`: per-day run/success/token rollups over the last N days.
/// `days` defaults to [`METRICS_DEFAULT_DAYS`] and must be a non-negative integer when present (else
/// 400); `days=0` means "all time" (store convention). Mirrors Go `handleMetrics`.
pub(crate) async fn handle_metrics(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let days = match qget(&q, "days") {
        "" => METRICS_DEFAULT_DAYS,
        raw => match parse_non_neg_int(raw, "days") {
            Ok(n) => n,
            Err(resp) => return *resp,
        },
    };
    let rollups = match provider.history().metrics(days, qget(&q, "project")) {
        Ok(rollups) => rollups,
        Err(_) => return store_error("metrics query failed"),
    };
    write_json(StatusCode::OK, &metrics_response(&rollups))
}

/// `GET /api/v1/runs/{id}/transcript`: the humanized RICH transcript for a single historical run (the
/// run's own concrete `*.jsonl`, in the SAME shape as the live `/log` response). `{id}` must be a
/// positive integer (else 404); an unknown run is 404 `run_not_found`; a known run whose transcript
/// file was pruned/never recorded returns 200 with `entries:[]`. `seq` is assigned 1..n here after the
/// orchestrator's cap. Mirrors Go `handleRunTranscript`.
pub(crate) async fn handle_run_transcript(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    match provider.run_transcript(run_id) {
        Some(entries) => write_json(
            StatusCode::OK,
            &run_transcript_json(run_id, &entries, &now_rfc3339()),
        ),
        None => write_error(
            StatusCode::NOT_FOUND,
            "run_not_found",
            format!("no run with id: {id}"),
            None,
        ),
    }
}

/// The current instant as an RFC3339 (UTC, seconds precision) string — the `generated_at` stamp. Go's
/// `time.Now().UTC().Format(time.RFC3339)`.
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `q.get(key)` as a `&str`, or `""` when absent — Go's `r.URL.Query().Get(key)` (empty string when
/// unset).
fn qget<'a>(q: &'a HashMap<String, String>, key: &str) -> &'a str {
    q.get(key).map(String::as_str).unwrap_or("")
}

/// A 500 `store_error` envelope with `message`. Every store read maps its error to this (Go's
/// `writeError(w, 500, "store_error", …)`).
fn store_error(message: &'static str) -> Response {
    write_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "store_error",
        message,
        None,
    )
}

/// Parse a `{id}` path segment as a positive run id, or a 404 `not_found` envelope (invalid or ≤0 —
/// run id 0 is reserved for the persistence-disabled state and is never a real run). Mirrors the Go
/// handlers' `strconv.ParseInt(idStr, 10, 64)` + `runID <= 0` guard. The [`Response`] error is boxed
/// so the common `Ok` path stays small (clippy `result_large_err`).
fn parse_run_id(id: &str) -> Result<i64, Box<Response>> {
    match id.parse::<i64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(Box::new(write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("invalid run id: {id}"),
            None,
        ))),
    }
}

/// Parse an optional non-negative integer query value. An empty string yields `Ok(0)` ("unset",
/// letting the store apply its own default). A malformed or negative value yields a 400 `invalid_param`
/// envelope (boxed — clippy `result_large_err`). Mirrors Go `parseNonNegInt`.
fn parse_non_neg_int(raw: &str, field: &str) -> Result<i64, Box<Response>> {
    if raw.is_empty() {
        return Ok(0);
    }
    match raw.parse::<i64>() {
        Ok(n) if n >= 0 => Ok(n),
        _ => Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "invalid_param",
            format!("{field} must be a non-negative integer"),
            None,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{SUMMARY_RHYTHM_RUNS, local_day_start};
    use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
    use rhapsody_agent::LogEntry;
    use rhapsody_orchestrator::{
        EventRecord, IssueKey, IssueLifecycle, IssueLifecycleRow, Snapshot, TokenCounts, Totals,
    };
    use rhapsody_store::{
        DEFAULT_RUN_LIMIT, EventRow, OUTCOME_COMPLETED, RunEnd, RunProgress, RunStart, Sqlite,
        Store, StorePath,
    };
    use serde_json::{Value, json};

    use crate::testutil::{
        FakeProvider, empty_snapshot, epoch, fixed_instant, retry_row, running_row, spawn_router,
    };
    use crate::{StateProvider, new_handler};

    // ---- helpers ----

    /// A fresh in-memory store (Go `store.Open(":memory:")`).
    fn mem_store() -> Sqlite {
        Sqlite::open(StorePath::InMemory).expect("open in-memory store")
    }

    fn rfc3339(t: chrono::DateTime<Utc>) -> String {
        t.to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Seed one completed run for `issue` starting (and ending) at `started`, returning its id.
    /// The lightweight seeder the TRA-320 paging/aggregate tests use to build stores larger than one
    /// page, where only the identity + timestamp of each row matters.
    fn seed_run_at(store: &Sqlite, issue: &str, started: &str) -> i64 {
        let id = store
            .start_run(RunStart {
                issue_identifier: issue.into(),
                started_at: started.into(),
                ..Default::default()
            })
            .expect("start run");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: started.into(),
                    ..Default::default()
                },
            )
            .expect("end run");
        id
    }

    /// Seed one completed run for a ticket with a real tracker `issue_id` — what the lifecycle
    /// decoration keys off, and what [`seed_run_at`] (identifier-only) deliberately leaves blank.
    fn seed_run_for(issue_id: &str, identifier: &str, started: &str, store: &Sqlite) -> i64 {
        let id = store
            .start_run(RunStart {
                issue_id: issue_id.into(),
                issue_identifier: identifier.into(),
                started_at: started.into(),
                ..Default::default()
            })
            .expect("start run");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: started.into(),
                    ..Default::default()
                },
            )
            .expect("end run");
        id
    }

    /// Seed one completed run (MT-1) + two events into `store`, returning the run id. Mirrors Go
    /// `seedStore`: timestamps anchor to "now-1h" so the run stays inside the /metrics?days=30 window.
    fn seed_completed_run(store: &Sqlite) -> i64 {
        let base = Utc::now() - ChronoDuration::hours(1);
        let run_id = store
            .start_run(RunStart {
                issue_id: "abc".into(),
                issue_identifier: "MT-1".into(),
                title: "fix the bug".into(),
                attempt: 1,
                started_at: rfc3339(base),
                project_slug: "core".into(),
                repo: "git@example.com:core.git".into(),
                ..Default::default()
            })
            .expect("start run");
        store
            .append_events(
                run_id,
                &[
                    EventRow {
                        seq: 1,
                        at: rfc3339(base + ChronoDuration::seconds(1)),
                        kind: "tool_use".into(),
                        tool: "Bash".into(),
                        text: "command=ls".into(),
                    },
                    EventRow {
                        seq: 2,
                        at: rfc3339(base + ChronoDuration::seconds(2)),
                        kind: "text".into(),
                        tool: String::new(),
                        text: "all done".into(),
                    },
                ],
            )
            .expect("append events");
        store
            .end_run(
                run_id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: rfc3339(base + ChronoDuration::minutes(5)),
                    turns: 2,
                    input_tokens: 100,
                    output_tokens: 50,
                    total_tokens: 150,
                    ..Default::default()
                },
            )
            .expect("end run");
        run_id
    }

    /// The Go `sampleSnapshot`: one running row (RunID 101) with a single recent event, plus a retry.
    fn sample_snapshot() -> Snapshot {
        let mut snap = empty_snapshot();
        snap.generated_at = fixed_instant();
        let mut row = running_row("MT-1");
        row.issue_id = "abc".into();
        row.title = "fix the bug".into();
        row.state = "In Progress".into();
        row.session_id = "thread-1-2".into();
        row.turn_count = 2;
        row.last_event = "turn_completed".into();
        row.workspace_path = "/ws/MT-1".into();
        row.run_id = 101;
        row.attempt = 1;
        row.project = "core".into();
        row.repo = "git@example.com:core.git".into();
        row.tokens = TokenCounts {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
        };
        row.recent_events = vec![EventRecord {
            at: epoch(),
            event: "turn_completed".into(),
            message: "did work".into(),
        }];
        row.transcript_path = "/logs/MT-1/latest.jsonl".into();
        snap.running.push(row);
        let mut retry = retry_row("MT-2");
        retry.attempt = 3;
        retry.error = "no available orchestrator slots".into();
        snap.retrying.push(retry);
        snap.totals = Totals {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            seconds_running: 12.5,
        };
        snap
    }

    async fn spawn(provider: FakeProvider) -> String {
        spawn_arc(Arc::new(provider)).await
    }

    /// `spawn` for a provider a test also holds, so it can read back what the handler asked it.
    async fn spawn_arc(provider: Arc<dyn StateProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn get_json(url: &str) -> (reqwest::StatusCode, Value) {
        let resp = reqwest::get(url).await.expect("GET");
        let status = resp.status();
        let body: Value = serde_json::from_str(&resp.text().await.expect("body")).expect("json");
        (status, body)
    }

    async fn post_status(url: &str) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(url)
            .send()
            .await
            .expect("POST")
            .status()
    }

    // ---- history (mirrors history_test.go) ----

    // Mirrors Go `TestHistoryRoundTrip`.
    #[tokio::test]
    async fn history_round_trip() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (status, body) = get_json(&format!("{base}/api/v1/history")).await;
        assert_eq!(status, 200);
        let runs = body["runs"].as_array().expect("runs array");
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r["issue_identifier"], "MT-1");
        assert_eq!(r["title"], "fix the bug");
        assert_eq!(r["outcome"], "completed");
        assert_eq!(r["total_tokens"], 150);
        assert_eq!(r["project_slug"], "core");
        // One row against the store's 50-row default page: the table is genuinely exhausted, so
        // there is no next page. (Updated from "unbounded page => null" — no page is unbounded;
        // the store always applies a limit, and next_offset now reflects the one it applied. TRA-320)
        assert_eq!(
            body["next_offset"],
            Value::Null,
            "1 row < the default page => exhausted"
        );
    }

    // Mirrors Go `TestHistoryPagingNextOffset`, extended for TRA-320: `next_offset` is derived from
    // the page size actually applied, so a full page yields one whether or not the caller asked for
    // a limit. Go computes it from the requested limit alone and reports null on the default path.
    #[tokio::test]
    async fn history_paging_next_offset() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/history?limit=1")).await;
        assert_eq!(body["next_offset"], 1, "limit=1 full page => next_offset 1");
        // An explicit offset carries through.
        let (_s, body) = get_json(&format!("{base}/api/v1/history?limit=1&offset=0")).await;
        assert_eq!(body["next_offset"], 1);
    }

    // TRA-320 Defect 1, the direct regression: with MORE runs than the store's default page and NO
    // `limit` in the request, the response must NOT claim the history is exhausted. Before the fix
    // this returned 50 rows with next_offset: null and the remaining rows were unreachable without
    // guessing a limit.
    #[tokio::test]
    async fn history_default_page_reports_next_offset() {
        let store = mem_store();
        let total = DEFAULT_RUN_LIMIT + 12;
        for i in 0..total {
            seed_run_at(
                &store,
                &format!("MT-{i}"),
                &format!("2026-08-01T00:{:02}:00Z", i % 60),
            );
        }
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        let (_s, body) = get_json(&format!("{base}/api/v1/history")).await;
        assert_eq!(
            body["runs"].as_array().expect("runs").len() as i64,
            DEFAULT_RUN_LIMIT,
            "the store's default page is applied"
        );
        assert_eq!(
            body["next_offset"], DEFAULT_RUN_LIMIT,
            "a full default page must advertise the next page, not null"
        );

        // Walking that offset reaches the rest, and the final short page terminates the walk.
        let (_s, body) =
            get_json(&format!("{base}/api/v1/history?offset={DEFAULT_RUN_LIMIT}")).await;
        assert_eq!(
            body["runs"].as_array().expect("runs").len() as i64,
            total - DEFAULT_RUN_LIMIT
        );
        assert_eq!(
            body["next_offset"],
            Value::Null,
            "genuinely exhausted => null"
        );
    }

    // TRA-320 — an explicit limit that exactly exhausts the table still reports null: the rule is
    // "a FULL page may have more", and a full page that happens to be the whole table is caught by
    // the follow-up request, exactly as Go's contract intends.
    #[tokio::test]
    async fn history_explicit_limit_beyond_the_table_is_exhausted() {
        let store = mem_store();
        for i in 0..3 {
            seed_run_at(
                &store,
                &format!("MT-{i}"),
                &format!("2026-08-01T00:0{i}:00Z"),
            );
        }
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/history?limit=500")).await;
        assert_eq!(body["runs"].as_array().expect("runs").len(), 3);
        assert_eq!(body["next_offset"], Value::Null, "3 of 500 => exhausted");
    }

    // TRA-320 Defect 3, mirroring the observed incident: one issue with 90 runs and nine quiet
    // issues must render as TEN job rows on the first page, not three.
    #[tokio::test]
    async fn issue_runs_one_row_per_issue() {
        let store = mem_store();
        for i in 0..9 {
            seed_run_at(
                &store,
                &format!("TRA-4{i:02}"),
                &format!("2026-08-01T01:{i:02}:00Z"),
            );
        }
        for i in 0..90 {
            seed_run_at(
                &store,
                "TRA-309",
                &format!("2026-08-01T{:02}:{:02}:00Z", 2 + i / 60, i % 60),
            );
        }
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        // The run-paged view is the broken behavior it replaces: one issue fills the whole page.
        let (_s, runs_body) = get_json(&format!("{base}/api/v1/history")).await;
        let visible: std::collections::HashSet<String> = runs_body["runs"]
            .as_array()
            .expect("runs")
            .iter()
            .map(|r| {
                r["issue_identifier"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            visible.len(),
            1,
            "precondition: run paging hides the other issues"
        );

        let (status, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        assert_eq!(status, 200);
        let issues = body["issues"].as_array().expect("issues array");
        assert_eq!(issues.len(), 10, "ten issues, not three");
        let idents: std::collections::HashSet<String> = issues
            .iter()
            .map(|r| {
                r["issue_identifier"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(idents.len(), 10, "every row is a distinct issue");
        assert_eq!(
            issues[0]["issue_identifier"], "TRA-309",
            "most recent activity first"
        );
        assert_eq!(
            body["next_offset"],
            Value::Null,
            "10 of a 50-issue page => exhausted"
        );
    }

    // TRA-320 — the issue listing pages by ISSUE and reports next_offset on the same effective-limit
    // rule as /history.
    #[tokio::test]
    async fn issue_runs_paging_next_offset() {
        let store = mem_store();
        for i in 0..4 {
            seed_run_at(
                &store,
                &format!("MT-{i}"),
                &format!("2026-08-01T00:0{i}:00Z"),
            );
            seed_run_at(
                &store,
                &format!("MT-{i}"),
                &format!("2026-08-01T01:0{i}:00Z"),
            );
        }
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/history/issues?limit=2")).await;
        assert_eq!(body["issues"].as_array().expect("issues").len(), 2);
        assert_eq!(body["next_offset"], 2, "full page of issues => next offset");
        let (_s, body) = get_json(&format!("{base}/api/v1/history/issues?limit=2&offset=2")).await;
        assert_eq!(body["issues"].as_array().expect("issues").len(), 2);
        let (_s, body) = get_json(&format!("{base}/api/v1/history/issues?limit=2&offset=4")).await;
        assert_eq!(
            body["issues"].as_array().expect("issues").len(),
            0,
            "4 issues total"
        );
    }

    // STUDIO-702 — the issue listing carries the TICKET's current lifecycle, so a completed run on
    // a merged ticket stops reading as "in review" forever. Both fields are present, and the daemon
    // is asked about exactly the ids it served.
    #[tokio::test]
    async fn issue_runs_carry_the_ticket_lifecycle() {
        let store = mem_store();
        seed_run_for("iss_done", "MT-1", "2026-08-01T00:00:00Z", &store);
        seed_run_for("iss_review", "MT-2", "2026-08-01T00:01:00Z", &store);
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_history(Arc::new(store))
                .with_issue_lifecycles(HashMap::from([
                    (
                        "iss_done".to_string(),
                        IssueLifecycleRow {
                            state: "Done".into(),
                            lifecycle: IssueLifecycle::Done,
                        },
                    ),
                    (
                        "iss_review".to_string(),
                        IssueLifecycleRow {
                            state: "In Review".into(),
                            lifecycle: IssueLifecycle::InReview,
                        },
                    ),
                ])),
        );
        let base = spawn_arc(Arc::clone(&provider) as Arc<dyn StateProvider>).await;

        let (status, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        assert_eq!(status, 200);
        let issues = body["issues"].as_array().expect("issues array");
        let by_ident: std::collections::HashMap<&str, &Value> = issues
            .iter()
            .map(|r| (r["issue_identifier"].as_str().unwrap_or_default(), r))
            .collect();
        assert_eq!(by_ident["MT-1"]["lifecycle"], "done");
        assert_eq!(by_ident["MT-1"]["tracker_state"], "Done");
        assert_eq!(by_ident["MT-2"]["lifecycle"], "in_review");
        assert_eq!(by_ident["MT-2"]["tracker_state"], "In Review");

        let mut asked = provider.issue_lifecycles_asked();
        asked.sort();
        assert_eq!(
            asked,
            vec!["iss_done".to_string(), "iss_review".to_string()],
            "the handler asks about exactly the page it served",
        );
    }

    // STUDIO-702 — a ticket the daemon cannot resolve carries NEITHER field, so "no answer" stays
    // distinguishable from any state it could have resolved and the client falls back cleanly.
    #[tokio::test]
    async fn issue_runs_omit_the_lifecycle_when_there_is_no_answer() {
        let store = mem_store();
        seed_run_for("iss_1", "MT-1", "2026-08-01T00:00:00Z", &store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        let (status, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        assert_eq!(status, 200);
        let row = &body["issues"][0];
        assert_eq!(row["issue_identifier"], "MT-1");
        assert!(
            row.get("lifecycle").is_none(),
            "no answer => no field: {row}"
        );
        assert!(
            row.get("tracker_state").is_none(),
            "no answer => no field: {row}"
        );
    }

    // STUDIO-735 — the issue listing carries the ticket's DURABLE assignee, so a job that has left
    // "running" keeps naming the teammate who did it. A ticket nobody was routed for carries no
    // field at all, which is what keeps the column's "—" honest.
    #[tokio::test]
    async fn issue_runs_carry_the_durable_assignee() {
        let store = mem_store();
        seed_run_for("iss_done", "MT-1", "2026-08-01T00:00:00Z", &store);
        seed_run_for("iss_solo", "MT-2", "2026-08-01T00:01:00Z", &store);
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_history(Arc::new(store))
                .with_issue_assignees(HashMap::from([
                    ("iss_done".to_string(), "alice".to_string()),
                    // The daemon's "nobody was routed for this one" answer.
                    ("iss_solo".to_string(), String::new()),
                ])),
        );
        let base = spawn_arc(Arc::clone(&provider) as Arc<dyn StateProvider>).await;

        let (status, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        assert_eq!(status, 200);
        let issues = body["issues"].as_array().expect("issues array");
        let by_ident: std::collections::HashMap<&str, &Value> = issues
            .iter()
            .map(|r| (r["issue_identifier"].as_str().unwrap_or_default(), r))
            .collect();
        assert_eq!(by_ident["MT-1"]["assignee"], "alice");
        assert!(
            by_ident["MT-2"].get("assignee").is_none(),
            "nobody routed => no field, never an empty name: {}",
            by_ident["MT-2"],
        );

        // Both halves of the key are forwarded: the tracker read goes by id, the daemon's own run
        // ledger by identifier, and neither substitutes for the other.
        let mut asked = provider.issue_assignees_asked();
        asked.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            asked,
            vec![
                IssueKey {
                    id: "iss_done".into(),
                    identifier: "MT-1".into()
                },
                IssueKey {
                    id: "iss_solo".into(),
                    identifier: "MT-2".into()
                },
            ],
        );
    }

    // STUDIO-735 — a daemon that cannot resolve an assignee carries no field, so the console falls
    // back to the live roster exactly as it did before the field existed.
    #[tokio::test]
    async fn issue_runs_omit_the_assignee_when_there_is_no_answer() {
        let store = mem_store();
        seed_run_for("iss_1", "MT-1", "2026-08-01T00:00:00Z", &store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        let (status, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        assert_eq!(status, 200);
        let row = &body["issues"][0];
        assert_eq!(row["issue_identifier"], "MT-1");
        assert!(
            row.get("assignee").is_none(),
            "no answer => no field: {row}"
        );
    }

    // STUDIO-735 — the two decorations are independent: a tracker that cannot say what state a
    // ticket is in must not also erase who worked it, since the store alone can answer that.
    #[tokio::test]
    async fn an_unresolvable_lifecycle_still_carries_its_assignee() {
        let store = mem_store();
        seed_run_for("iss_1", "MT-1", "2026-08-01T00:00:00Z", &store);
        let base = spawn(
            FakeProvider::ok(empty_snapshot())
                .with_history(Arc::new(store))
                .with_issue_assignees(HashMap::from([("iss_1".to_string(), "alice".to_string())])),
        )
        .await;

        let (_s, body) = get_json(&format!("{base}/api/v1/history/issues")).await;
        let row = &body["issues"][0];
        assert_eq!(row["assignee"], "alice");
        assert!(row.get("lifecycle").is_none(), "still no lifecycle: {row}");
    }

    // STUDIO-702 — /history is byte-pinned to the Go daemon's golden and must NOT grow the fields
    // the Rhapsody-only issue listing does.
    #[tokio::test]
    async fn run_history_never_carries_the_lifecycle_fields() {
        let store = mem_store();
        seed_run_for("iss_done", "MT-1", "2026-08-01T00:00:00Z", &store);
        let base = spawn(
            FakeProvider::ok(empty_snapshot())
                .with_history(Arc::new(store))
                .with_issue_lifecycles(HashMap::from([(
                    "iss_done".to_string(),
                    IssueLifecycleRow {
                        state: "Done".into(),
                        lifecycle: IssueLifecycle::Done,
                    },
                )]))
                .with_issue_assignees(HashMap::from([(
                    "iss_done".to_string(),
                    "alice".to_string(),
                )])),
        )
        .await;

        let (_s, body) = get_json(&format!("{base}/api/v1/history")).await;
        let row = &body["runs"][0];
        assert!(
            row.get("lifecycle").is_none(),
            "run paging stays Go-shaped: {row}"
        );
        assert!(
            row.get("tracker_state").is_none(),
            "run paging stays Go-shaped: {row}"
        );
        assert!(
            row.get("assignee").is_none(),
            "run paging stays Go-shaped: {row}"
        );
    }

    // TRA-320 — the issue listing validates limit/offset exactly like /history.
    #[tokio::test]
    async fn issue_runs_invalid_limit_400() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = get_json(&format!("{base}/api/v1/history/issues?limit=-1")).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_param");
    }

    // TRA-320 Defect 2: the day totals are computed over the WHOLE store, so they are identical
    // whether a client fetched one page or four — and they match a direct SUM over the window.
    #[tokio::test]
    async fn history_summary_totals_are_page_independent() {
        let store = mem_store();
        let total_runs = DEFAULT_RUN_LIMIT * 2 + 5; // more than two default pages
        for i in 0..total_runs {
            let id = store
                .start_run(RunStart {
                    issue_identifier: format!("MT-{i}"),
                    started_at: format!("2026-08-01T{:02}:{:02}:00Z", i / 60, i % 60),
                    ..Default::default()
                })
                .expect("start");
            store
                .end_run(
                    id,
                    RunEnd {
                        outcome: "completed".into(),
                        ended_at: format!("2026-08-01T{:02}:{:02}:00Z", (i + 1) / 60, (i + 1) % 60),
                        input_tokens: 3,
                        output_tokens: 7,
                        total_tokens: 1000,
                        ..Default::default()
                    },
                )
                .expect("end");
        }
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        let url = format!("{base}/api/v1/history/summary?since=2026-08-01T00:00:00Z");
        let (status, body) = get_json(&url).await;
        assert_eq!(status, 200);
        assert_eq!(
            body["since"], "2026-08-01T00:00:00Z",
            "the window is echoed back"
        );
        assert_eq!(
            body["runs"], total_runs,
            "every run in the window, not one page"
        );
        assert_eq!(body["completed"], total_runs);
        assert_eq!(body["input_tokens"], total_runs * 3);
        assert_eq!(body["output_tokens"], total_runs * 7);
        assert_eq!(body["total_tokens"], total_runs * 1000);
        assert_eq!(body["seconds"], total_runs * 60);

        // Identical regardless of how much history the client has fetched — the point of the fix.
        for page in ["limit=1", "limit=50", "limit=200"] {
            let (_s, page_body) = get_json(&format!("{base}/api/v1/history?{page}")).await;
            let fetched = page_body["runs"].as_array().expect("runs").len();
            let (_s, again) = get_json(&url).await;
            assert_eq!(
                again["total_tokens"], body["total_tokens"],
                "totals moved after fetching {fetched} rows"
            );
        }

        // The rhythm series is capped at the sparkline's bar count and runs oldest→newest.
        let rhythm = body["rhythm"].as_array().expect("rhythm array");
        assert_eq!(rhythm.len(), SUMMARY_RHYTHM_RUNS as usize);
        assert!(rhythm.iter().all(|v| v == &Value::from(1000)));
    }

    // TRA-320 — an in-flight run contributes its elapsed time and live token progress, counted once.
    #[tokio::test]
    async fn history_summary_includes_in_flight_runs() {
        let store = mem_store();
        let started = Utc::now() - ChronoDuration::seconds(120);
        let running = store
            .start_run(RunStart {
                issue_identifier: "MT-live".into(),
                started_at: started.to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            })
            .expect("start");
        store
            .update_run_progress(
                running,
                RunProgress {
                    turns: 2,
                    input_tokens: 11,
                    output_tokens: 13,
                    total_tokens: 900,
                    ..Default::default()
                },
            )
            .expect("progress");
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        // RFC3339 with a `Z` suffix carries no `+`, so it needs no percent-encoding here.
        let since = rfc3339(started - ChronoDuration::seconds(60));
        let (_s, body) = get_json(&format!("{base}/api/v1/history/summary?since={since}")).await;
        assert_eq!(body["runs"], 1, "the in-flight run is counted exactly once");
        assert_eq!(body["completed"], 0);
        assert_eq!(body["total_tokens"], 900, "live progress is included");
        let seconds = body["seconds"].as_i64().expect("seconds");
        assert!(
            (115..=180).contains(&seconds),
            "elapsed-so-far should be ~120s, got {seconds}"
        );
    }

    // TRA-320 — a store with nothing in the window answers all-zero rather than erroring, and an
    // unparseable `since` is a 400 rather than a silent whole-table sum.
    #[tokio::test]
    async fn history_summary_empty_window_and_bad_since() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;

        let (status, body) = get_json(&format!(
            "{base}/api/v1/history/summary?since=2999-01-01T00:00:00Z"
        ))
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["runs"], 0);
        assert_eq!(body["total_tokens"], 0);
        assert_eq!(body["seconds"], 0);
        assert_eq!(body["rhythm"], json!([]), "no runs => no rhythm bars");

        let (status, body) =
            get_json(&format!("{base}/api/v1/history/summary?since=yesterday")).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_param");
    }

    // TRA-320 — with no `since` the daemon falls back to ITS OWN local midnight, so a run that
    // started today is in the window and one from a week ago is not.
    #[tokio::test]
    async fn history_summary_defaults_to_the_daemon_local_day() {
        let store = mem_store();
        store
            .start_run(RunStart {
                issue_identifier: "MT-today".into(),
                started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            })
            .expect("today");
        store
            .start_run(RunStart {
                issue_identifier: "MT-old".into(),
                started_at: (Utc::now() - ChronoDuration::days(7))
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            })
            .expect("old");
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (status, body) = get_json(&format!("{base}/api/v1/history/summary")).await;
        assert_eq!(status, 200);
        assert_eq!(
            body["runs"], 1,
            "only today's run falls in the default window"
        );
        assert_eq!(
            body["since"],
            local_day_start(),
            "the daemon's local midnight"
        );
    }

    // Mirrors Go `TestHistoryInvalidLimit400`.
    #[tokio::test]
    async fn history_invalid_limit_400() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = get_json(&format!("{base}/api/v1/history?limit=-1")).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_param");
    }

    // Mirrors Go `TestIssueHistoryRoundTrip`.
    #[tokio::test]
    async fn issue_history_round_trip() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/issues/MT-1/history")).await;
        assert_eq!(body["issue_identifier"], "MT-1");
        assert_eq!(body["runs"].as_array().expect("runs").len(), 1);
    }

    // Mirrors Go `TestRunEventsRoundTrip`.
    #[tokio::test]
    async fn run_events_round_trip() {
        let store = mem_store();
        let run_id = seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/runs/{run_id}/events")).await;
        assert_eq!(body["run_id"], run_id);
        let events = body["events"].as_array().expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["tool"], "Bash");
        assert_eq!(events[0]["kind"], "tool_use");
    }

    // Mirrors Go `TestRunEventsBadID404`.
    #[tokio::test]
    async fn run_events_bad_id_404() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let (status, _b) = get_json(&format!("{base}/api/v1/runs/notanint/events")).await;
        assert_eq!(status, 404);
    }

    // Mirrors Go `TestEventSearchRoundTrip`.
    #[tokio::test]
    async fn event_search_round_trip() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/events?q=done")).await;
        let hits = body["hits"].as_array().expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["text"], "all done");
        assert_eq!(hits[0]["issue_identifier"], "MT-1");
    }

    // Mirrors Go `TestMetricsRoundTrip`.
    #[tokio::test]
    async fn metrics_round_trip() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/metrics?days=30")).await;
        let days = body["days"].as_array().expect("days");
        assert_eq!(days.len(), 1);
        assert_eq!(days[0]["completed"], 1);
        assert_eq!(days[0]["total_tokens"], 150);
    }

    // Mirrors Go `TestHistoryNoopStoreEmpty`: every history endpoint degrades to [] (200) on Noop.
    #[tokio::test]
    async fn history_noop_store_empty() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await; // default Noop store
        for (path, key) in [
            ("/api/v1/history", "runs"),
            ("/api/v1/issues/MT-1/history", "runs"),
            ("/api/v1/runs/1/events", "events"),
            ("/api/v1/events?q=x", "hits"),
            ("/api/v1/metrics?days=7", "days"),
        ] {
            let (status, body) = get_json(&format!("{base}{path}")).await;
            assert_eq!(status, 200, "{path}");
            assert_eq!(body[key], serde_json::json!([]), "{path}: {key} must be []");
        }
    }

    // ---- run detail (mirrors run_detail_test.go) ----

    // Mirrors Go `TestRunDetailLive`.
    #[tokio::test]
    async fn run_detail_live() {
        let base = spawn(FakeProvider::ok(sample_snapshot())).await; // running row RunID 101
        let (status, body) = get_json(&format!("{base}/api/v1/runs/101")).await;
        assert_eq!(status, 200);
        assert_eq!(body["outcome"], "running");
        assert_eq!(body["live"], true);
        assert_eq!(body["run_id"], 101);
        assert_eq!(body["issue_identifier"], "MT-1");
        assert_eq!(body["last_codex_event"], "turn_completed");
        assert_eq!(body["ended_at"], "");
        assert_eq!(body["total_tokens"], 150);
        assert_eq!(body["turn_count"], 2);
        let ev = body["recent_events"].as_array().expect("recent_events");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["event"], "turn_completed");
    }

    // Mirrors Go `TestRunDetailFinished`.
    #[tokio::test]
    async fn run_detail_finished() {
        let store = mem_store();
        let id = store
            .start_run(RunStart {
                issue_identifier: "MT-9".into(),
                title: "old run".into(),
                attempt: 2,
                project_slug: "core".into(),
                repo: "git@example.com:core.git".into(),
                ..Default::default()
            })
            .expect("start");
        store
            .append_events(
                id,
                &[EventRow {
                    seq: 1,
                    at: "2026-05-28T11:00:00Z".into(),
                    kind: "event".into(),
                    tool: String::new(),
                    text: "session started".into(),
                }],
            )
            .expect("events");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: "2026-05-28T11:05:00Z".into(),
                    turns: 3,
                    total_tokens: 200,
                    ..Default::default()
                },
            )
            .expect("end");
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (status, body) = get_json(&format!("{base}/api/v1/runs/{id}")).await;
        assert_eq!(status, 200);
        assert_eq!(body["outcome"], "completed");
        assert_eq!(body["live"], false);
        assert_eq!(body["issue_identifier"], "MT-9");
        assert_eq!(body["title"], "old run");
        assert_eq!(body["attempt"], 2);
        assert_eq!(body["turn_count"], 3);
        assert_eq!(body["total_tokens"], 200);
        assert_eq!(body["ended_at"], "2026-05-28T11:05:00Z");
        // Live-only fields blank for a finished run.
        assert_eq!(body["last_codex_event"], "");
        assert_eq!(body["issue_state"], "");
        let ev = body["recent_events"].as_array().expect("recent_events");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["event"], "event");
        assert_eq!(ev[0]["message"], "session started");
    }

    // Mirrors Go `TestRunDetailNotFound`.
    #[tokio::test]
    async fn run_detail_not_found() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await; // Noop store
        let (status, body) = get_json(&format!("{base}/api/v1/runs/99999")).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"]["code"], "run_not_found");
    }

    // Mirrors Go `TestRunDetailInvalidID`.
    #[tokio::test]
    async fn run_detail_invalid_id() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let (status, _b) = get_json(&format!("{base}/api/v1/runs/not-a-number")).await;
        assert_eq!(status, 404);
    }

    // Mirrors Go `TestRunDetailMethodNotAllowed`.
    #[tokio::test]
    async fn run_detail_method_not_allowed() {
        let base = spawn(FakeProvider::ok(sample_snapshot())).await;
        assert_eq!(post_status(&format!("{base}/api/v1/runs/101")).await, 405);
    }

    // Mirrors Go `TestRunDetailLiveThenFinished`: the SAME run_id resolves live first, then from the
    // finalized history row once it leaves the snapshot — never a 404. Two providers share the store
    // (the Rust fake's snapshot is immutable), pinning both the live-wins precedence and the fallback.
    #[tokio::test]
    async fn run_detail_live_then_finished() {
        let store = Arc::new(mem_store());
        let id = store
            .start_run(RunStart {
                issue_identifier: "MT-7".into(),
                title: "live then done".into(),
                ..Default::default()
            })
            .expect("start");

        // Phase 1 — LIVE: the run is in the snapshot (and, as outcome=running, in the store). The
        // snapshot must win.
        let mut snap = empty_snapshot();
        let mut row = running_row("MT-7");
        row.issue_id = "id-7".into();
        row.title = "live then done".into();
        row.state = "In Progress".into();
        row.run_id = id;
        row.attempt = 1;
        row.turn_count = 1;
        row.last_event = "notification".into();
        row.tokens = TokenCounts {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        };
        snap.running.push(row);
        let base = spawn(FakeProvider::ok(snap).with_history(store.clone())).await;
        let (status, live) = get_json(&format!("{base}/api/v1/runs/{id}")).await;
        assert_eq!(status, 200);
        assert_eq!(live["live"], true);
        assert_eq!(live["outcome"], "running");
        assert_eq!(live["run_id"], id);

        // The run finishes: it leaves the snapshot and the row is finalized.
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: "2026-05-28T12:00:00Z".into(),
                    turns: 2,
                    total_tokens: 42,
                    ..Default::default()
                },
            )
            .expect("end");

        // Phase 2 — FINISHED: the SAME run_id now resolves from the store, NOT a 404.
        let base2 = spawn(FakeProvider::ok(empty_snapshot()).with_history(store.clone())).await;
        let (status2, done) = get_json(&format!("{base2}/api/v1/runs/{id}")).await;
        assert_eq!(status2, 200);
        assert_eq!(done["live"], false);
        assert_eq!(done["outcome"], "completed");
        assert_eq!(done["ended_at"], "2026-05-28T12:00:00Z");
        assert_eq!(done["total_tokens"], 42);
    }

    // Mirrors Go `TestRunDetailSnapshotFailureFallsBackToStore`: a Snapshot failure must NOT 503 — a
    // finished run that exists only in persistence still loads (the live scan is skipped).
    #[tokio::test]
    async fn run_detail_snapshot_failure_falls_back_to_store() {
        let store = mem_store();
        let id = store
            .start_run(RunStart {
                issue_identifier: "MT-9".into(),
                title: "old run".into(),
                ..Default::default()
            })
            .expect("start");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: "2026-05-28T11:05:00Z".into(),
                    turns: 2,
                    ..Default::default()
                },
            )
            .expect("end");
        let provider = FakeProvider::failing("snapshot_timeout").with_history(Arc::new(store));
        let base = spawn(provider).await;
        let (status, body) = get_json(&format!("{base}/api/v1/runs/{id}")).await;
        assert_eq!(status, 200, "snapshot failure must not 503 a finished run");
        assert_eq!(body["live"], false);
        assert_eq!(body["outcome"], "completed");
    }

    // ---- run transcript (mirrors run_transcript_test.go) ----

    // Mirrors the handler assertions of Go `TestRunTranscriptHandlerEndToEnd`: given the humanized
    // entries, the handler assigns 1-based seq and passes kind/tool/text through in the live-`/log`
    // wire shape. (The raw-jsonl → LogEntry humanize step is covered by the agent crate's own tests
    // and wired end-to-end in F1; here the handler is fed the already-humanized entries via the fake.)
    #[tokio::test]
    async fn run_transcript_handler_shapes_entries() {
        let entries = vec![
            LogEntry {
                kind: "event".into(),
                tool: String::new(),
                text: "session started".into(),
            },
            LogEntry {
                kind: "thinking".into(),
                tool: String::new(),
                text: "hmm".into(),
            },
            LogEntry {
                kind: "tool_use".into(),
                tool: "Bash".into(),
                text: "command=ls".into(),
            },
            LogEntry {
                kind: "tool_result".into(),
                tool: String::new(),
                text: "output".into(),
            },
            LogEntry {
                kind: "text".into(),
                tool: String::new(),
                text: "done".into(),
            },
            LogEntry {
                kind: "event".into(),
                tool: String::new(),
                text: "turn completed".into(),
            },
        ];
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(Some(entries))).await;
        let (status, body) = get_json(&format!("{base}/api/v1/runs/5/transcript")).await;
        assert_eq!(status, 200);
        assert_eq!(body["run_id"], 5);
        assert_ne!(body["generated_at"], "");
        let got = body["entries"].as_array().expect("entries");
        let want_kinds = [
            "event",
            "thinking",
            "tool_use",
            "tool_result",
            "text",
            "event",
        ];
        assert_eq!(got.len(), want_kinds.len());
        for (i, (e, want)) in got.iter().zip(want_kinds).enumerate() {
            assert_eq!(e["seq"], i as i64 + 1, "1-based seq");
            assert_eq!(e["kind"], want);
        }
        assert_eq!(got[2]["tool"], "Bash");
        assert_eq!(got[2]["text"], "command=ls");
    }

    // Mirrors Go `TestRunTranscriptHandlerUnknownRun`.
    #[tokio::test]
    async fn run_transcript_unknown_run() {
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(None)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/runs/42/transcript")).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"]["code"], "run_not_found");
    }

    // Mirrors Go `TestRunTranscriptHandlerInvalidID`.
    #[tokio::test]
    async fn run_transcript_invalid_id() {
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(Some(vec![]))).await;
        let (status, _b) = get_json(&format!("{base}/api/v1/runs/abc/transcript")).await;
        assert_eq!(status, 404);
    }

    // Mirrors Go `TestRunTranscriptHandlerFoundButEmpty`: a known run whose transcript was pruned
    // returns 200 with entries:[] (never null).
    #[tokio::test]
    async fn run_transcript_found_but_empty() {
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(Some(vec![]))).await;
        let (status, body) = get_json(&format!("{base}/api/v1/runs/7/transcript")).await;
        assert_eq!(status, 200);
        assert_eq!(body["entries"], serde_json::json!([]));
    }

    // Mirrors Go `TestRunTranscriptHandlerMethodNotAllowed`.
    #[tokio::test]
    async fn run_transcript_method_not_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(Some(vec![]))).await;
        assert_eq!(
            post_status(&format!("{base}/api/v1/runs/1/transcript")).await,
            405
        );
    }
}
