//! The `symphony_run_status` verdict — parity port of `$REF/internal/mcpfacade/verdict.go`.
//!
//! [`verdict`] is a pure function of composed inputs (`/state`, `/runs/{id}`,
//! `/issues/{id}/history`), so the full taxonomy is unit-testable with no I/O. It NEVER fabricates
//! a not-dispatched reason: an unresolvable reason is reported as `unknown`.

use crate::client::{RunDetail, RunningSession};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// Verdict kinds — the headline `symphony_run_status` answer (design §"The headline tool").
pub const KIND_ALIVE: &str = "alive";
pub const KIND_STALLED: &str = "stalled";
pub const KIND_COMPLETED: &str = "completed";
pub const KIND_FAILED: &str = "failed";
pub const KIND_INTERRUPTED: &str = "interrupted";
pub const KIND_NOT_DISPATCHED: &str = "not-dispatched";

/// The age past which a live run with no fresh events is reported as stalled (verdict.go's
/// `DefaultStallThreshold` = 10m). A facade-side heuristic; the verdict always exposes the raw age
/// too. `chrono::Duration::minutes` is not `const`, so this is a function.
pub(crate) fn default_stall_threshold() -> Duration {
    Duration::minutes(10)
}

/// The single verdict object `symphony_run_status` returns (verdict.go's `Status`). `summary` is
/// the one-line human form; the structured fields let a caller branch without re-parsing. Field
/// presence mirrors Go's `omitempty` tags: `kind`/`summary` always serialize; the rest are dropped
/// when empty/zero/absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub kind: String,
    pub summary: String,
    /// Set for [`KIND_COMPLETED`] (the run-outcome taxonomy value: completed|continued|stopped).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    /// Set for [`KIND_FAILED`] (the run's error text, verbatim).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Set for [`KIND_NOT_DISPATCHED`]. Never fabricated: an unresolvable reason is `unknown`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub run_id: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issue: String,
    /// `last_event_at` / `age_seconds` describe liveness for alive/stalled (raw age, never floored).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_event_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<i64>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// A reconstructed not-dispatched reason (verdict.go's `notDispatched`). An empty reason ⇒ unknown
/// (the honesty boundary: over the HTTP API alone, blocked-by/pr-suppressed/caps are usually not
/// retrievable, so the verdict says "unknown — check daemon logs" rather than fabricating one).
#[derive(Debug, Clone, Default)]
pub(crate) struct NotDispatched {
    pub reason: String,
}

/// The pure input to the run-status verdict (verdict.go's `verdictInput`), composed from `/state`,
/// `/runs/{id}`, and `/issues/{id}/history` by `run_status`.
#[derive(Debug, Clone)]
pub(crate) struct VerdictInput {
    pub now: DateTime<Utc>,
    /// A live run whose last event is older than this is stalled. ≤ 0 ⇒ disabled (never stalled),
    /// mirroring the daemon's `stall_timeout_ms` semantics.
    pub stall_threshold: Duration,
    /// The matching row from `/state.running` (`None` ⇒ not currently running).
    pub running: Option<RunningSession>,
    /// The latest run detail from `/runs/{id}` (`None` ⇒ no run known). Its presence with no
    /// running row is what distinguishes "ran and finished" from "never dispatched".
    pub run: Option<RunDetail>,
    /// The reconstructed reason when the issue was never dispatched (`None` ⇒ unknown).
    pub not_dispatched: Option<NotDispatched>,
}

impl VerdictInput {
    /// A bare input for a given clock + threshold (the run-status starting point).
    pub(crate) fn new(now: DateTime<Utc>, stall_threshold: Duration) -> Self {
        Self {
            now,
            stall_threshold,
            running: None,
            run: None,
            not_dispatched: None,
        }
    }
}

/// Composes the inputs into a single [`Status`] (verdict.go's `verdict`). NEVER fabricates a
/// not-dispatched reason.
pub(crate) fn verdict(input: &VerdictInput) -> Status {
    // 1) A DEFINITIVELY-terminal run detail wins over a /state.running row ONLY when both describe
    //    the SAME run (equal run_id, or no running row at all). This closes the race where
    //    /runs/{id} — fetched AFTER the /state snapshot — already shows the queried run finished
    //    while the stale snapshot still lists it running. It must NOT fire for DIFFERENT runs: on
    //    the issue path `run` is the latest-history run while `running` is the active session, so a
    //    terminal OLDER run must never mask a newer active run. `interrupted` is excluded (a
    //    recovered run may be legitimately re-listed running → case 2).
    if let Some(run) = &input.run {
        let same_run = input
            .running
            .as_ref()
            .is_none_or(|r| r.run_id == run.run_id);
        if is_terminal_outcome(&run.outcome) && same_run {
            return terminal_status(run);
        }
    }
    // 2) Currently running ⇒ alive or stalled, from the last-event age.
    if let Some(running) = &input.running {
        return live_verdict(
            input,
            running.run_id,
            &running.issue_identifier,
            &running.last_event_at,
        );
    }
    // 3) A run detail that still reports live (no running row yet, or a race) is alive/stalled too.
    if let Some(run) = &input.run
        && run.live
        && run.outcome == rhapsody_store::OUTCOME_RUNNING
    {
        return live_verdict(input, run.run_id, &run.issue_identifier, &run.last_event_at);
    }
    // 4) Run detail present but neither terminal-authoritative nor live: interrupted, running-but-
    //    not-live, or an unknown outcome value.
    if let Some(run) = &input.run {
        let mut s = Status {
            run_id: run.run_id,
            issue: run.issue_identifier.clone(),
            ..Default::default()
        };
        if run.outcome == rhapsody_store::OUTCOME_INTERRUPTED {
            s.kind = KIND_INTERRUPTED.into();
            s.summary = "interrupted (recovery pending)".into();
        } else if run.outcome == rhapsody_store::OUTCOME_RUNNING {
            // Live flag was false but outcome still "running" — report alive without an age.
            return live_verdict(input, run.run_id, &run.issue_identifier, &run.last_event_at);
        } else {
            // Unknown outcome value — surface it verbatim rather than guessing.
            s.kind = KIND_COMPLETED.into();
            s.outcome = run.outcome.clone();
            s.summary = format!("completed({})", or_unknown(&run.outcome));
        }
        return s;
    }

    // 5) No running row and no run ⇒ not dispatched. Reason is reconstructed or unknown.
    let reason = input
        .not_dispatched
        .as_ref()
        .map(|n| n.reason.as_str())
        .filter(|r| !r.is_empty())
        .unwrap_or("unknown — check daemon logs")
        .to_string();
    Status {
        kind: KIND_NOT_DISPATCHED.into(),
        summary: format!("not-dispatched({reason})"),
        reason,
        ..Default::default()
    }
}

/// Whether an outcome is DEFINITIVELY terminal — a state a recovered run can never re-enter.
/// `interrupted` is intentionally excluded (boot recovery may continue it).
fn is_terminal_outcome(o: &str) -> bool {
    o == rhapsody_store::OUTCOME_COMPLETED
        || o == rhapsody_store::OUTCOME_CONTINUED
        || o == rhapsody_store::OUTCOME_STOPPED
        || o == rhapsody_store::OUTCOME_FAILED
}

/// Maps a definitively-terminal run detail onto its verdict (verdict.go's `terminalStatus`).
fn terminal_status(run: &RunDetail) -> Status {
    let mut s = Status {
        run_id: run.run_id,
        issue: run.issue_identifier.clone(),
        ..Default::default()
    };
    if run.outcome == rhapsody_store::OUTCOME_FAILED {
        s.kind = KIND_FAILED.into();
        s.error = run.error.clone();
        s.summary = format!("failed({})", or_unknown(&run.error));
    } else {
        // completed / continued / stopped
        s.kind = KIND_COMPLETED.into();
        s.outcome = run.outcome.clone();
        s.summary = format!("completed({})", run.outcome);
    }
    s
}

/// Classifies a live run as alive or stalled from its last-event timestamp (verdict.go's
/// `liveVerdict`).
fn live_verdict(input: &VerdictInput, run_id: i64, issue: &str, last_event_at: &str) -> Status {
    let mut s = Status {
        kind: KIND_ALIVE.into(),
        run_id,
        issue: issue.to_string(),
        last_event_at: last_event_at.to_string(),
        ..Default::default()
    };
    let Some(age) = age_of(input.now, last_event_at) else {
        // No parseable last-event timestamp — report alive without an age rather than guessing.
        s.summary = "alive".into();
        return s;
    };
    s.age_seconds = Some(age.num_seconds());
    if input.stall_threshold > Duration::zero() && age > input.stall_threshold {
        s.kind = KIND_STALLED.into();
        s.summary = format!("stalled (last event {} ago)", human_age(age));
    } else {
        s.summary = format!("alive (last event {} ago)", human_age(age));
    }
    s
}

/// Parses an RFC3339 timestamp and returns `now - t`, clamped to ≥ 0. `None` when `ts` is
/// empty/unparseable (verdict.go's `ageOf`). `chrono`'s RFC3339 parser accepts fractional seconds,
/// covering both Go's `time.RFC3339` and `time.RFC3339Nano` in one call.
fn age_of(now: DateTime<Utc>, ts: &str) -> Option<Duration> {
    if ts.is_empty() {
        return None;
    }
    let t = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    let d = now.signed_duration_since(t);
    Some(if d < Duration::zero() {
        Duration::zero()
    } else {
        d
    })
}

/// Formats a duration as Go verdict.go's `humanAge` (rounded to the nearest second).
fn human_age(d: Duration) -> String {
    // Go rounds to the nearest second (half away from zero); d is already ≥ 0 here.
    let secs = (d.num_milliseconds() + 500) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs / 60) % 60)
    }
}

/// `unknown` for an empty string, else the string itself (verdict.go's `orUnknown`).
fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "unknown" } else { s }
}

#[cfg(test)]
mod tests {
    //! Mirror of `$REF/internal/mcpfacade/verdict_test.go` (`TestVerdictTaxonomy`).
    use super::*;
    use chrono::TimeZone;
    use rhapsody_store::{
        OUTCOME_COMPLETED, OUTCOME_CONTINUED, OUTCOME_FAILED, OUTCOME_INTERRUPTED, OUTCOME_STOPPED,
    };

    fn running(run_id: i64, issue: &str, last_event_at: &str) -> RunningSession {
        RunningSession {
            run_id,
            issue_identifier: issue.to_string(),
            last_event_at: last_event_at.to_string(),
            ..Default::default()
        }
    }

    fn run(run_id: i64, issue: &str, outcome: &str) -> RunDetail {
        RunDetail {
            run_id,
            issue_identifier: issue.to_string(),
            outcome: outcome.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn verdict_taxonomy() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap();
        let fresh = (now - Duration::seconds(3)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let old = (now - Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let stall = default_stall_threshold();

        // running + fresh event => alive (age 3)
        let s = verdict(&VerdictInput {
            running: Some(running(7, "INF-1", &fresh)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_ALIVE);
        assert!(s.summary.contains("alive (last event"), "{}", s.summary);
        assert_eq!(s.age_seconds, Some(3));

        // running row + terminal run detail (same id) => terminal wins (not alive)
        let s = verdict(&VerdictInput {
            running: Some(running(7, "INF-1", &fresh)),
            run: Some(run(7, "INF-1", OUTCOME_COMPLETED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_COMPLETED);
        assert!(s.summary.contains("completed(completed)"), "{}", s.summary);

        // terminal older run + running newer run (different ids) => alive (active wins)
        let s = verdict(&VerdictInput {
            running: Some(running(6, "INF-1", &fresh)),
            run: Some(run(5, "INF-1", OUTCOME_COMPLETED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_ALIVE);
        assert!(s.summary.contains("alive"), "{}", s.summary);
        assert_eq!(s.run_id, 6);

        // running row + interrupted run detail => alive (recovery re-listed it)
        let s = verdict(&VerdictInput {
            running: Some(running(7, "INF-1", &fresh)),
            run: Some(run(7, "", OUTCOME_INTERRUPTED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_ALIVE);
        assert!(s.summary.contains("alive"), "{}", s.summary);

        // running + old event => stalled
        let s = verdict(&VerdictInput {
            running: Some(running(7, "INF-1", &old)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_STALLED);
        assert!(s.summary.contains("stalled"), "{}", s.summary);

        // running + stall disabled (threshold 0) => alive even when old
        let s = verdict(&VerdictInput {
            running: Some(running(7, "", &old)),
            ..VerdictInput::new(now, Duration::zero())
        });
        assert_eq!(s.kind, KIND_ALIVE);
        assert!(s.summary.contains("alive"), "{}", s.summary);

        // finished completed => completed(completed)
        let s = verdict(&VerdictInput {
            run: Some(run(7, "", OUTCOME_COMPLETED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_COMPLETED);
        assert!(s.summary.contains("completed(completed)"), "{}", s.summary);
        assert_eq!(s.outcome, OUTCOME_COMPLETED);

        // finished continued => completed(continued)
        let s = verdict(&VerdictInput {
            run: Some(run(0, "", OUTCOME_CONTINUED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_COMPLETED);
        assert!(s.summary.contains("completed(continued)"), "{}", s.summary);

        // finished stopped => completed(stopped)
        let s = verdict(&VerdictInput {
            run: Some(run(0, "", OUTCOME_STOPPED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_COMPLETED);
        assert!(s.summary.contains("completed(stopped)"), "{}", s.summary);

        // finished failed => failed(reason)
        let s = verdict(&VerdictInput {
            run: Some(RunDetail {
                outcome: OUTCOME_FAILED.into(),
                error: "turn timeout".into(),
                ..Default::default()
            }),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_FAILED);
        assert!(s.summary.contains("failed(turn timeout)"), "{}", s.summary);
        assert_eq!(s.error, "turn timeout");

        // finished failed, empty error => failed(unknown)
        let s = verdict(&VerdictInput {
            run: Some(run(0, "", OUTCOME_FAILED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_FAILED);
        assert!(s.summary.contains("failed(unknown)"), "{}", s.summary);

        // interrupted => interrupted (recovery pending)
        let s = verdict(&VerdictInput {
            run: Some(run(0, "", OUTCOME_INTERRUPTED)),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_INTERRUPTED);
        assert!(s.summary.contains("recovery pending"), "{}", s.summary);

        // issue with non-terminal blocker => not-dispatched(blocked-by X)
        let s = verdict(&VerdictInput {
            not_dispatched: Some(NotDispatched {
                reason: "blocked-by INF-9 (In Review)".into(),
            }),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_NOT_DISPATCHED);
        assert!(s.summary.contains("blocked-by INF-9"), "{}", s.summary);
        assert_eq!(s.reason, "blocked-by INF-9 (In Review)");

        // issue PR-linked, no summons => not-dispatched(pr-suppressed)
        let s = verdict(&VerdictInput {
            not_dispatched: Some(NotDispatched {
                reason: "pr-suppressed".into(),
            }),
            ..VerdictInput::new(now, stall)
        });
        assert_eq!(s.kind, KIND_NOT_DISPATCHED);
        assert!(s.summary.contains("pr-suppressed"), "{}", s.summary);

        // unresolvable => not-dispatched(unknown), never fabricated
        let s = verdict(&VerdictInput::new(now, stall));
        assert_eq!(s.kind, KIND_NOT_DISPATCHED);
        assert!(
            s.summary.contains("unknown — check daemon logs"),
            "{}",
            s.summary
        );
        assert_eq!(s.reason, "unknown — check daemon logs");
    }

    // guard: the status JSON stays stable enough to decode (mirrors TestStatusJSONDecodes).
    #[test]
    fn status_json_decodes() {
        let b = serde_json::to_vec(&Status {
            kind: KIND_ALIVE.into(),
            summary: "alive".into(),
            ..Default::default()
        })
        .unwrap();
        let s: Status = serde_json::from_slice(&b).unwrap();
        assert_eq!(s.kind, KIND_ALIVE);
    }
}
