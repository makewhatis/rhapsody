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
use rhapsody_store::{EventQuery, RunFilter};

use crate::handlers::{SNAPSHOT_TIMEOUT, require_get};
use crate::responses::{write_error, write_json};
use crate::responses_history::{
    event_search_response, history_response, issue_history_response, metrics_response,
    run_detail_from_running, run_detail_from_summary, run_events_response, run_transcript_json,
};
use crate::server::StateProvider;

/// The `/metrics` default window when `?days=` is omitted (the history page sizes are defaulted by the
/// store itself when limit<=0). Mirrors Go `metricsDefaultDays`.
const METRICS_DEFAULT_DAYS: i64 = 30;

/// `GET /api/v1/history?issue=&outcome=&project=&since=&limit=&offset=`: a paged, filterable list of
/// recent runs. `limit`/`offset` must be non-negative integers when present (else 400). `next_offset`
/// is set (offset+limit) only when a bounded full page (`limit>0`) came back, so the client knows
/// another page exists — the store applies its own default when `limit<=0`, which we cannot observe
/// from here. Mirrors Go `handleHistory`.
pub(crate) async fn handle_history(
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
    let offset = match parse_non_neg_int(qget(&q, "offset"), "offset") {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let f = RunFilter {
        issue: qget(&q, "issue").to_string(),
        outcome: qget(&q, "outcome").to_string(),
        since: qget(&q, "since").to_string(),
        project: qget(&q, "project").to_string(),
        limit,
        offset,
    };
    let runs = match provider.history().list_runs(f) {
        Ok(runs) => runs,
        Err(_) => return store_error("history query failed"),
    };
    // next_offset is meaningful only when the caller bounded the page (limit>0) and a full page came
    // back — the effective page size is the requested limit.
    let next = (limit > 0 && runs.len() as i64 == limit).then_some(offset + limit);
    write_json(StatusCode::OK, &history_response(&runs, next))
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
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
    use rhapsody_agent::LogEntry;
    use rhapsody_orchestrator::{EventRecord, Snapshot, TokenCounts, Totals};
    use rhapsody_store::{EventRow, OUTCOME_COMPLETED, RunEnd, RunStart, Sqlite, Store, StorePath};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{
        FakeProvider, empty_snapshot, epoch, fixed_instant, retry_row, running_row, spawn_router,
    };

    // ---- helpers ----

    /// A fresh in-memory store (Go `store.Open(":memory:")`).
    fn mem_store() -> Sqlite {
        Sqlite::open(StorePath::InMemory).expect("open in-memory store")
    }

    fn rfc3339(t: chrono::DateTime<Utc>) -> String {
        t.to_rfc3339_opts(SecondsFormat::Secs, true)
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
        spawn_router(new_handler(Arc::new(provider), None)).await
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
        assert_eq!(body["next_offset"], Value::Null, "unbounded page => null");
    }

    // Mirrors Go `TestHistoryPagingNextOffset`.
    #[tokio::test]
    async fn history_paging_next_offset() {
        let store = mem_store();
        seed_completed_run(&store);
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store))).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/history?limit=1")).await;
        assert_eq!(body["next_offset"], 1, "limit=1 full page => next_offset 1");
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
