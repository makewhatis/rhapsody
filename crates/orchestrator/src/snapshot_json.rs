//! snapshot_json — the `GET /api/v1/state` wire view over an orchestrator [`Snapshot`].
//!
//! Parity port of Go httpapi `toStateJSON` + `toRunningSessionJSON` + `toRetryEntryJSON` +
//! `toRateLimitsJSON` (`internal/httpapi/responses.go`). Following the config crate's `effective_json`
//! convention, the domain crate owns its wire view (so P6's HTTP handler reuses this module rather
//! than reimplementing the serialization) AND the parity gate against the committed fixture
//! (`harness/fixtures/api/state.json`). O4's completion criterion is that the snapshot shape matches
//! that fixture; [`render`] is what proves it.
//!
//! Wire shape (SPA contract, `web/lib/api.ts`): nested tokens are FLATTENED onto the running row,
//! `last_event` is renamed `last_codex_event`, timestamps are bare RFC3339 strings (`""` when the
//! zero instant), and `status`/`poll_interval_ms`/`counts` are the SPA-required additions the
//! `Snapshot` itself does not carry. `rate_limits` is always a (possibly empty) array, never null.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::snapshot::{RateLimit, RetryRow, RunningRow, Snapshot};

/// The constant health value emitted on `/state` (the orchestrator surfaces no health field yet).
/// Mirrors Go `stateStatusOK`.
const STATE_STATUS_OK: &str = "ok";
/// The refetch cadence the SPA's `useStateQuery` keys off; the orchestrator does not surface its poll
/// interval through the snapshot, so a constant is used. Mirrors Go `statePollIntervalMS`.
const STATE_POLL_INTERVAL_MS: i64 = 2000;

/// Renders a [`Snapshot`] as the `GET /api/v1/state` payload. Mirrors Go `toStateJSON`. `s.projects`
/// is intentionally NOT emitted here — the per-project rollup is served by the agents/projects
/// surfaces, not `/state` (matching Go's `stateJSON`, which omits it).
pub fn render(s: &Snapshot) -> Value {
    json!({
        "status": STATE_STATUS_OK,
        "poll_interval_ms": STATE_POLL_INTERVAL_MS,
        "generated_at": rfc3339_or_empty(s.generated_at),
        "counts": {
            "running": s.running.len(),
            "retrying": s.retrying.len(),
        },
        "running": s.running.iter().map(running_session_json).collect::<Vec<_>>(),
        "retrying": s.retrying.iter().map(retry_entry_json).collect::<Vec<_>>(),
        "codex_totals": {
            "input_tokens": s.totals.input_tokens,
            "output_tokens": s.totals.output_tokens,
            "total_tokens": s.totals.total_tokens,
            "seconds_running": s.totals.seconds_running,
        },
        "rate_limits": s.rate_limits.iter().map(rate_limit_json).collect::<Vec<_>>(),
    })
}

/// The flat `RunningSession` the SPA expects: nested tokens flattened, `last_event` renamed
/// `last_codex_event`, timestamps RFC3339 (`""` when zero). Mirrors Go `toRunningSessionJSON`.
fn running_session_json(r: &RunningRow) -> Value {
    json!({
        "issue_id": r.issue_id,
        "issue_identifier": r.issue_identifier,
        "title": r.title,
        "state": r.state,
        "project": r.project,
        "repo": r.repo,
        "run_id": r.run_id,
        "turn_count": r.turn_count,
        "last_codex_event": r.last_event,
        "started_at": rfc3339_or_empty(r.started_at),
        "last_event_at": rfc3339_or_empty(r.last_event_at),
        "input_tokens": r.tokens.input_tokens,
        "output_tokens": r.tokens.output_tokens,
        "total_tokens": r.tokens.total_tokens,
    })
}

/// The flat `RetryEntry` the SPA expects. Mirrors Go `toRetryEntryJSON`.
fn retry_entry_json(r: &RetryRow) -> Value {
    json!({
        "issue_identifier": r.issue_identifier,
        "attempt": r.attempt,
        "due_at": rfc3339_or_empty(r.due_at),
        "error": r.error,
    })
}

/// One rate-limit row. Go's `toRateLimitsJSON` always returns `[]` (no orchestrator source yet); here
/// [`render`] maps the (currently always-empty) `s.rate_limits`, which serializes identically to `[]`
/// while staying forward-compatible with a future source (P6 §2e). Mirrors Go `rateLimitJSON`.
fn rate_limit_json(r: &RateLimit) -> Value {
    json!({
        "type": r.kind,
        "resets_at": r.resets_at,
        "used_percent": r.used_percent,
    })
}

/// Formats `t` as RFC3339 (UTC, seconds precision), or `""` when `t` is the zero instant, matching the
/// SPA's wire contract (timestamps are bare RFC3339 strings, `""` when unset — NOT null). Mirrors Go
/// `rfc3339OrEmpty`; the Rust zero convention is the Unix epoch (what the entry constructors default
/// time fields to), the analog of Go's `time.Time{}`.
fn rfc3339_or_empty(t: DateTime<Utc>) -> String {
    if t.timestamp() == 0 && t.timestamp_subsec_nanos() == 0 {
        String::new()
    } else {
        t.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use rhapsody_core::Issue;

    use super::*;
    use crate::orchestrator::{Orchestrator, RetryEntry, Totals};
    use crate::testsupport::{issue, running_entry};

    /// Recursively sort object keys, mirroring the capture pipeline's `jq -S .` (stabilizes key order
    /// before the fixture is committed). Same helper the config crate's golden test uses.
    fn sort_keys(v: Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: std::collections::BTreeMap<String, Value> =
                    m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
            other => other,
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, 0)
            .single()
            .expect("valid fixed instant")
    }

    // The O4 completion gate: an assembled snapshot, rendered through `render` and normalized, is
    // byte-identical to the committed `GET /api/v1/state` fixture. Reproduces the fixture scenario
    // (0 running, 1 retrying RHA-1, codex_totals 20/20/40).
    #[test]
    fn state_json_matches_state_fixture() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.totals = Totals {
            input_tokens: 20,
            output_tokens: 20,
            total_tokens: 40,
            seconds_running: 0.0,
        };
        o.retry_attempts.insert(
            "rha1".to_string(),
            RetryEntry {
                issue_id: "rha1".to_string(),
                identifier: "RHA-1".to_string(),
                attempt: 1,
                due_at: now + Duration::seconds(60),
                err: String::new(),
                project_slug: String::new(),
                project_repo: String::new(),
                issue: Issue::default(),
                due_at_ms: 0,
                recovered: false,
            },
        );

        let s = o.build_snapshot();
        let rendered = sort_keys(render(&s));
        let pretty = format!(
            "{}\n",
            serde_json::to_string_pretty(&rendered).expect("serialize")
        );
        let got = harness_fixtures::normalize(&pretty);
        let want = harness_fixtures::normalize(&harness_fixtures::load("api/state.json"));
        assert_eq!(got, want, "state.json shape drift");
    }

    // The running-row wire shape (the fixture's `running` array is empty, so cover it directly): the
    // nested tokens are flattened, `last_event` is renamed `last_codex_event`, and project/repo/run_id
    // are carried.
    #[test]
    fn running_row_flattens_and_renames() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let mut re = running_entry(issue("id1", "MT-9", "In Progress"), "alpha", "alpha");
        re.started_at = now;
        re.last_event_at = now;
        re.last_event = "turn_completed".to_string();
        re.turn_count = 4;
        re.run_id = 77;
        re.project_repo = "git@github.com:o/r.git".to_string();
        re.input_tokens = 11;
        re.output_tokens = 3;
        re.total_tokens = 14;
        o.running.insert("id1".to_string(), re);

        let rendered = render(&o.build_snapshot());
        let row = &rendered["running"][0];
        assert_eq!(row["issue_identifier"], "MT-9");
        assert_eq!(row["last_codex_event"], "turn_completed"); // renamed from last_event
        assert!(
            row.get("last_event").is_none(),
            "wire uses last_codex_event, not last_event"
        );
        assert_eq!(row["turn_count"], 4);
        assert_eq!(row["run_id"], 77);
        assert_eq!(row["project"], "alpha");
        assert_eq!(row["repo"], "git@github.com:o/r.git");
        // Tokens are flattened onto the row (no nested `tokens` object).
        assert_eq!(row["input_tokens"], 11);
        assert_eq!(row["total_tokens"], 14);
        assert!(
            row.get("tokens").is_none(),
            "tokens are flattened, not nested"
        );
        // counts reflect the row.
        assert_eq!(rendered["counts"]["running"], 1);
    }

    // rate_limits always serializes as an array (never null), matching the fixture's `[]`.
    #[test]
    fn rate_limits_is_always_an_array() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(rendered["rate_limits"].is_array());
        assert_eq!(rendered["rate_limits"].as_array().expect("array").len(), 0);
    }
}
