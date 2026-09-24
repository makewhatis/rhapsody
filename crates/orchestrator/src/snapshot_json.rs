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
    let mut out = json!({
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
    });
    // STUDIO-880: the drain key is emitted ONLY while a drain is armed.
    //
    // That conditional is deliberate and load-bearing. `/api/v1/state` is byte-pinned to the Go
    // daemon's `harness/fixtures/api/state.json` golden, so an unconditional Rhapsody-only key here
    // would be parity drift — the reason `teams_enabled` lives on `/api/v1/version` instead. A key
    // that appears only in a state the Go daemon cannot be in leaves every payload it CAN produce
    // byte-identical, which is what `state_json_matches_state_fixture` and
    // `a_daemon_that_is_not_draining_emits_no_drain_key` pin between them. Clients read it as
    // `state.drain?.active`.
    if let Some(d) = &s.drain
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "drain".to_string(),
            json!({
                "active": d.active,
                "reason": d.reason.as_str(),
                "requested_at": d.requested_at.map(rfc3339_or_empty).unwrap_or_default(),
            }),
        );
    }
    // STUDIO-898: the review_divergence key is emitted ONLY when the reconciliation sweep has
    // something to report, for the `drain` key's reason above and under the same two guards — the
    // golden comparison plus `a_healthy_daemon_emits_no_review_divergence_key`, which asserts the
    // ABSENCE directly so this cannot decay into an unconditional `[]` on a Go-pinned surface.
    //
    // The DETAIL and not just a flag, because the advisory on `/api/v1/projects` is a fixed string
    // and an operator's next question is always "which one". Clients read
    // `state.review_divergence?.length`.
    if !s.review_divergence.is_empty()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "review_divergence".to_string(),
            Value::Array(
                s.review_divergence
                    .iter()
                    .map(|d| {
                        let mut row = json!({
                            "pr": d.pr,
                            "kind": d.kind.as_str(),
                            "detail": d.kind.detail(),
                            "ticket": d.ticket,
                            "reviewer": d.reviewer,
                            "stale_secs": d.stale_secs,
                        });
                        // STUDIO-950: the capacity annotation, conditional exactly as the key and
                        // the row are. When the review watcher is HOLDING this round for want of a
                        // global slot it is a deliberate wait, and the row must say so — otherwise
                        // the console (and anything else reading this row) can only repeat the
                        // unenriched "not reported blocked" framing the ticket exists to kill.
                        // Absent with no hold, so a divergence the sweep found before this ticket
                        // keeps the row shape it had, and the healthy payload is untouched either
                        // way. `budget` is the key an operator would loosen; `holders` is the
                        // watcher's own count.
                        if let (Some(hold), Some(obj)) = (d.capacity_held, row.as_object_mut()) {
                            obj.insert(
                                "capacity_held".to_string(),
                                json!({
                                    "holders": hold.holders,
                                    "budget": hold.budget_key(),
                                }),
                            );
                        }
                        // STUDIO-950 round 21: the OTHER capacity annotation, conditional on the
                        // same key and row. When the hold was DENIED because GitHub stopped
                        // answering for the coordinate, the advisory ends "see `review_divergence`
                        // on /api/v1/state" — and without this the row it points at is
                        // indistinguishable from an ordinary divergence, so the operator cannot map
                        // the advisory to the pull request it is about. Mutually exclusive with
                        // `capacity_held` (the denial is what suppresses the hold), so a row never
                        // carries both. The count is what the operator needs to see; presence is
                        // what identifies the row.
                        if let (Some(attempts), Some(obj)) =
                            (d.capacity_unreadable, row.as_object_mut())
                        {
                            obj.insert(
                                "capacity_unreadable".to_string(),
                                json!({ "attempts": attempts }),
                            );
                        }
                        // STUDIO-1005: a SUPERSEDED escalation, and ONLY a superseded one. The
                        // adjudication `reason` is written once and never revalidated, so an
                        // operator reading it has no way to know the branch moved on. When the
                        // watcher has OBSERVED a different current head the row must say so, and it
                        // must carry the manager's own words beside the notice so the operator sees
                        // exactly which present-tense claim is now a snapshot.
                        //
                        // CONDITIONAL on purpose, and load-bearing (the ticket's last acceptance):
                        // an escalation whose head has NOT moved adds NOTHING, so it renders
                        // byte-identically to before this ticket. The head fields and the reason are
                        // absent rather than empty there — the same rule the key, the row and the
                        // capacity annotations all follow on this Go-pinned surface.
                        if d.superseded()
                            && let Some(obj) = row.as_object_mut()
                        {
                            obj.insert("adjudicated_head".to_string(), json!(d.adjudicated_head));
                            obj.insert("current_head".to_string(), json!(d.current_head));
                            obj.insert("superseded".to_string(), json!(true));
                            obj.insert("reason".to_string(), json!(d.reason));
                            obj.insert("findings".to_string(), json!(d.findings));
                            obj.insert(
                                "supersession".to_string(),
                                json!(d.supersession().unwrap_or_default()),
                            );
                        }
                        // STUDIO-1015: a manager-owned stall the manager could not adopt keeps its
                        // row and carries the §10.2 human-feed sentence (`manager deferred: drain`,
                        // `manager unavailable: CLI contract`, …). Conditional on the kind, so every
                        // other row shape (and the healthy payload) is untouched.
                        if d.kind == crate::reviewreconcile::DivergenceKind::ManagerDeferred
                            && let Some(obj) = row.as_object_mut()
                        {
                            obj.insert("reason".to_string(), json!(d.reason));
                        }
                        row
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    // STUDIO-949: the held_for_human key is emitted ONLY while the dispatcher is holding at least
    // one `rhapsody:human` ticket, for the `drain` key's reason above and under the same two guards
    // — the golden comparison plus `a_daemon_with_no_human_hold_emits_no_held_for_human_key`, which
    // asserts the ABSENCE directly so this cannot decay into an unconditional `[]` on a Go-pinned
    // surface. Clients read `state.held_for_human?.length`.
    if !s.held_for_human.is_empty()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "held_for_human".to_string(),
            Value::Array(
                s.held_for_human
                    .iter()
                    .map(|h| {
                        json!({
                            "issue_identifier": h.issue_identifier,
                            "title": h.title,
                            "project": h.project,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    // STUDIO-957: the per-provider budget refusals, emitted ONLY while at least one dispatch is
    // being held — for the `held_for_human` key's reason and under the same guard, so a daemon with
    // no configured budget (the default) serves a payload byte-identical to the Go capture's.
    if !s.budget_held.is_empty()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "budget_held".to_string(),
            Value::Array(
                s.budget_held
                    .iter()
                    .map(|h| {
                        json!({
                            "subject": h.subject,
                            "title": h.title,
                            "project": h.project,
                            "provider": h.provider,
                            "daily_tokens": h.daily_tokens,
                            "spent_tokens": h.spent_tokens,
                            // STUDIO-970: the pull request coordinate, empty for a TICKET hold. The
                            // console board draws cards for the ticket half only and must tell the
                            // two apart: a review hold is surfaced by the reconciliation sweep and
                            // naming it as a ticket card would invent work that does not exist. The
                            // struct already carries this discriminator, so it travels rather than
                            // being re-derived from the subject's `pr:` convention.
                            "pr": h.pr,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    // STUDIO-1026: pending desktop notifications for the runaway-loop breaker, emitted ONLY while
    // there is at least one — for the `held_for_human` key's reason and under the same guard. The
    // queue only fills when `notify.macos: true`, so a daemon that never configured it serves a
    // payload byte-identical to the Go capture's. Clients read `state.notifications?.length` and
    // de-dupe on `id`.
    if !s.notifications.is_empty()
        && let Some(obj) = out.as_object_mut()
    {
        obj.insert(
            "notifications".to_string(),
            serde_json::to_value(&s.notifications).unwrap_or(Value::Array(Vec::new())),
        );
    }
    out
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
                identity: String::new(),
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

    // STUDIO-880, the parity guard: a daemon that is not draining emits NO `drain` key at all.
    //
    // `state_json_matches_state_fixture` above already proves the whole non-draining payload is
    // byte-identical to the Go golden, but it would keep proving that even if this key were
    // rendered as `null` or `false` and the golden were quietly recaptured. This asserts the
    // ABSENCE directly, so the conditional cannot decay into an unconditional Rhapsody-only key on
    // a surface that is pinned to Go's.
    #[test]
    fn a_daemon_that_is_not_draining_emits_no_drain_key() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(
            rendered.get("drain").is_none(),
            "a non-draining daemon must serve the Go-identical payload, got: {rendered}"
        );
    }

    // STUDIO-898, the same parity guard for the same reason: a healthy daemon emits NO
    // `review_divergence` key. The sweep reports nothing on the overwhelming majority of ticks, so
    // an unconditional `[]` here would be a Rhapsody-only key on every payload of a Go-pinned
    // surface — and `state_json_matches_state_fixture` would keep passing if the golden were
    // recaptured with it. This asserts the ABSENCE directly.
    #[test]
    fn a_healthy_daemon_emits_no_review_divergence_key() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(
            rendered.get("review_divergence").is_none(),
            "a healthy daemon must serve the Go-identical payload, got: {rendered}"
        );
    }

    // STUDIO-1026: the same parity guard for the macOS notification key. It only fills when
    // `notify.macos: true`, so a daemon that never configured a channel must emit NO `notifications`
    // key — otherwise it would be a Rhapsody-only key on every Go-pinned payload.
    #[test]
    fn a_daemon_with_no_notifications_emits_no_notifications_key() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(
            rendered.get("notifications").is_none(),
            "a daemon with no pending notification must serve the Go-identical payload, got: {rendered}"
        );

        // And the other half: a pending one reaches the surface with the fields the desktop needs.
        o.notifications.push(
            now,
            "Review loop held: STUDIO-988".to_string(),
            "body".to_string(),
            "STUDIO-988".to_string(),
            "makewhatis/rhapsody#218".to_string(),
        );
        let rendered = render(&o.build_snapshot());
        let rows = rendered["notifications"]
            .as_array()
            .expect("notifications is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ticket"], "STUDIO-988");
        assert_eq!(rows[0]["pr"], "makewhatis/rhapsody#218");
        assert_eq!(rows[0]["body"], "body");
        assert_eq!(rows[0]["id"], 1);
    }

    // And the other half: a reported divergence reaches `/api/v1/state` with enough to act on —
    // WHICH pull request, which ticket, how it diverged and for how long. The advisory on
    // `/api/v1/projects` is a fixed string, so this is the only surface that can say which.
    #[test]
    fn a_reported_divergence_reaches_state_with_its_detail() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.review_divergence = vec![crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#164".to_string(),
            kind: crate::reviewreconcile::DivergenceKind::ChangesRequestedNoRun,
            ticket: "STUDIO-893".to_string(),
            reviewer: "jimmy".to_string(),
            stale_secs: 21_600,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: String::new(),
        }];

        let rendered = render(&o.build_snapshot());
        let rows = rendered["review_divergence"]
            .as_array()
            .expect("review_divergence is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["pr"], "makewhatis/rhapsody#164");
        assert_eq!(rows[0]["kind"], "changes_requested_no_run");
        assert_eq!(rows[0]["ticket"], "STUDIO-893");
        assert_eq!(rows[0]["reviewer"], "jimmy");
        assert_eq!(rows[0]["stale_secs"], 21_600);
        // The human sentence travels with it: a console must not have to own a copy of the wording,
        // which is how the two drift apart.
        assert_eq!(
            rows[0]["detail"],
            "a reviewer asked for changes and the ticket has had no run since"
        );
        // ...and no `capacity_held` key when there is no hold: the annotation is conditional, like
        // the key and the row it lives on.
        assert!(
            rows[0].get("capacity_held").is_none(),
            "a divergence with no hold must not carry the annotation, got: {}",
            rows[0]
        );
        // ...and neither capacity annotation is unconditional (STUDIO-950 round 21).
        assert!(
            rows[0].get("capacity_unreadable").is_none(),
            "a readable coordinate must not carry the denial, got: {}",
            rows[0]
        );
    }

    // STUDIO-1015 (§10.2): a manager-owned stall the manager could NOT adopt (a deferred launch, or
    // an unavailable manager) reaches the console as a `manager_deferred` row carrying the manager's
    // own sentence. MUTATION: drop the conditional `reason` insert and the operator sees the generic
    // detail with no way to tell why the manager did not act.
    #[test]
    fn a_manager_deferred_divergence_carries_its_reason_on_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.review_divergence = vec![crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#164".to_string(),
            kind: crate::reviewreconcile::DivergenceKind::ManagerDeferred,
            ticket: "STUDIO-1015".to_string(),
            reviewer: "alice".to_string(),
            stale_secs: 0,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: "manager deferred: drain".to_string(),
        }];

        let rendered = render(&o.build_snapshot());
        let rows = rendered["review_divergence"]
            .as_array()
            .expect("review_divergence is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "manager_deferred");
        assert_eq!(
            rows[0]["reason"], "manager deferred: drain",
            "the manager's own wording reaches the operator"
        );
    }

    // STUDIO-950: when the review watcher IS holding the reported round for want of a global slot,
    // the state row carries the hold — the fact that lets the console say the wait is deliberate
    // and name the budget, instead of repeating "not reported blocked". The whole divergence key is
    // Rhapsody-only and conditional, so annotating a row on it leaves the Go-pinned healthy payload
    // byte-identical.
    #[test]
    fn a_capacity_held_divergence_carries_its_hold_on_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.review_divergence = vec![crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#164".to_string(),
            kind: crate::reviewreconcile::DivergenceKind::ReviewRequestedNoRun,
            ticket: "STUDIO-950".to_string(),
            reviewer: "alice".to_string(),
            stale_secs: 21_600,
            auto_merge_reason: None,
            capacity_held: Some(crate::reviewwatch::CapacityHold {
                holders: 2,
                separate: true,
                recorded: now,
            }),
            capacity_unreadable: None,
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: String::new(),
        }];

        let rendered = render(&o.build_snapshot());
        let row = &rendered["review_divergence"][0];
        assert_eq!(row["kind"], "review_requested_no_run");
        assert_eq!(
            row["capacity_held"]["holders"], 2,
            "the holder count the watcher recorded"
        );
        assert_eq!(
            row["capacity_held"]["budget"], "agent.max_concurrent_reviews",
            "the annotation names the budget an operator would loosen"
        );
        // It is still reported in full — the hold ANNOTATES, it never suppresses.
        assert!(row.get("detail").is_some());
        assert_eq!(row["stale_secs"], 21_600);
    }

    // STUDIO-950 round 21: the unreadable denial is the other capacity annotation, and it must reach
    // the state row for the same reason the hold does — the advisory whose wording ends "see
    // `review_divergence` on /api/v1/state" would otherwise point an operator at a row they cannot
    // tell apart from an ordinary divergence.
    #[test]
    fn an_unreadable_denial_carries_its_attempts_on_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.review_divergence = vec![crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#164".to_string(),
            kind: crate::reviewreconcile::DivergenceKind::ReviewRequestedNoRun,
            ticket: "STUDIO-950".to_string(),
            reviewer: "alice".to_string(),
            stale_secs: 21_600,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: Some(3),
            adjudicated_head: String::new(),
            current_head: String::new(),
            rounds: 0,
            findings: Vec::new(),
            reason: String::new(),
        }];

        let rendered = render(&o.build_snapshot());
        let row = &rendered["review_divergence"][0];
        assert_eq!(
            row["capacity_unreadable"]["attempts"], 3,
            "the attempt count the watcher recorded"
        );
        // It is still reported in full — the annotation never suppresses.
        assert!(row.get("detail").is_some());
        assert_eq!(row["stale_secs"], 21_600);
    }

    // STUDIO-949, the same parity guard for the same reason: a daemon holding no `rhapsody:human`
    // ticket emits NO `held_for_human` key. The hold is rare, so an unconditional `[]` here would be
    // a Rhapsody-only key on every payload of a Go-pinned surface — and
    // `state_json_matches_state_fixture` would keep passing if the golden were recaptured with it.
    // This asserts the ABSENCE directly.
    #[test]
    fn a_daemon_with_no_human_hold_emits_no_held_for_human_key() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(
            rendered.get("held_for_human").is_none(),
            "a daemon with no human hold must serve the Go-identical payload, got: {rendered}"
        );
    }

    // And the other half: a held ticket reaches `/api/v1/state`, so the console board can read it as
    // deliberately held rather than mysteriously idle.
    #[test]
    fn a_held_ticket_reaches_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.human_holds.hold(crate::dispatch::HeldForHuman {
            issue_identifier: "STUDIO-939".to_string(),
            title: "wire the stores to RevenueCat".to_string(),
            project: "booch".to_string(),
        });

        let rendered = render(&o.build_snapshot());
        let rows = rendered["held_for_human"]
            .as_array()
            .expect("held_for_human is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["issue_identifier"], "STUDIO-939");
        assert_eq!(rows[0]["title"], "wire the stores to RevenueCat");
        assert_eq!(rows[0]["project"], "booch");
    }

    // STUDIO-957, the same parity guard as `held_for_human`: a daemon with no configured budget (the
    // default) emits NO `budget_held` key, so `/api/v1/state` stays byte-identical to the Go capture.
    // Mutation: emit the key unconditionally and this reds (and the golden's absence guard with it).
    #[test]
    fn a_daemon_with_no_budget_hold_emits_no_budget_held_key() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        let rendered = render(&o.build_snapshot());
        assert!(
            rendered.get("budget_held").is_none(),
            "a daemon with no budget refusal must serve the Go-identical payload, got: {rendered}"
        );
    }

    // And the other half: a budget refusal reaches `/api/v1/state`, so an operator can see WHY a
    // ticket stopped dispatching rather than reading it as an unexplained stall.
    #[test]
    fn a_budget_held_subject_reaches_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.note_budget_hold(
            "STUDIO-957",
            "meter spend per provider",
            "rhapsody",
            "anthropic",
            200_000_000,
            361_000_000,
        );

        let rendered = render(&o.build_snapshot());
        let rows = rendered["budget_held"]
            .as_array()
            .expect("budget_held is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["subject"], "STUDIO-957");
        assert_eq!(rows[0]["provider"], "anthropic");
        assert_eq!(rows[0]["daily_tokens"], 200_000_000);
        assert_eq!(rows[0]["spent_tokens"], 361_000_000);
        // STUDIO-970: a ticket hold carries no pull request coordinate, which is how the console
        // board tells it from a review hold and draws a card for it.
        assert_eq!(rows[0]["pr"], "");
    }

    // STUDIO-970, the other half: a REVIEW hold's coordinate reaches the wire, so the console can
    // exclude it from the ticket cards the board draws. Mutation: drop `pr` from the row and this
    // reds with `null` for every hold — including the ticket half the board must still card.
    #[test]
    fn a_review_budget_hold_carries_its_coordinate_on_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.note_review_budget_hold(
            "pr:makewhatis/rhapsody#199@alice",
            "makewhatis/rhapsody#199",
            "rhapsody",
            "anthropic",
            200_000_000,
            361_000_000,
        );

        let rendered = render(&o.build_snapshot());
        let rows = rendered["budget_held"]
            .as_array()
            .expect("budget_held is an array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["subject"], "pr:makewhatis/rhapsody#199@alice");
        assert_eq!(
            rows[0]["pr"], "makewhatis/rhapsody#199",
            "a review hold names the pull request the sweep will report it against"
        );
    }

    // And the other half: while a drain IS armed the key appears, carrying the two annotations an
    // operator needs to tell a deliberate pause from a wedged daemon.
    #[test]
    fn a_draining_daemon_reports_the_drain_on_state() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let now = fixed_now();
        o.now = Box::new(move || now);
        o.drain.arm(now, crate::drain::DrainReason::Update);

        let rendered = render(&o.build_snapshot());
        let drain = &rendered["drain"];
        assert_eq!(drain["active"], true);
        assert_eq!(drain["reason"], "update");
        assert_eq!(
            drain["requested_at"], "2026-05-28T12:00:00Z",
            "the drain reports when it was asked for, so a waiter can say how long it has run"
        );
        // Cancelling takes the key away again rather than leaving `active: false` behind.
        o.drain.disarm();
        assert!(
            render(&o.build_snapshot()).get("drain").is_none(),
            "a cancelled drain returns the payload to its Go-identical shape"
        );
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
