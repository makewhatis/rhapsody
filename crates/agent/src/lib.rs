//! rhapsody-agent — parity port of Go `internal/agent` (Symphony v0.4.0).
//!
//! Symphony's pluggable coding-agent abstraction and backends. It is backend-agnostic
//! (upstream §10): this crate ports `agent.go` (the `EventType`/`TurnStatus` value sets, `Usage`,
//! `Event`, `TurnResult`, `Transcript`, and the [`Runner`]/[`Session`] traits), `errors.go` (the
//! typed [`AgentError`] sentinels), `humanize.go` (the UI event humanizer — see [`humanize`]), and
//! the in-process [`fake::Fake`] backend (P5's test double). The `claude` subprocess backend lands
//! in the later P4 tasks (A2/A3).
//!
//! Porting decisions carried across the whole crate:
//! * Go's `type EventType string` / `type TurnStatus string` (whose zero value is the empty string
//!   and whose members are documented string values) map to `&'static str` constants + plain
//!   `String` fields, exactly as `rhapsody-store` ports Go's outcome/claim string enums. This keeps
//!   the "same string values" contract and lets a zero-value [`TurnResult`] carry an empty status,
//!   the parity mirror of Go's `var tr TurnResult; tr.Status == ""`.
//! * Go's `ctx context.Context` first argument becomes async cancellation (implicit; the traits are
//!   async, driven by the tokio orchestrator), so it is dropped from the Rust signatures.
//! * Go's `int` maps to `i64` (matching `rhapsody-store`); Go pointer fields (`*Usage`, `*int`,
//!   `time.Time` used as unset-able) become `Option<…>`.

pub mod fake;
pub mod humanize;

pub use humanize::{LogEntry, humanize_stream_line};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rhapsody_core::Issue;
use tokio::sync::mpsc;

// --- EventType (upstream §10.4) ---------------------------------------------------------------
// Normalized agent events forwarded to the orchestrator. Go models these as `type EventType
// string`; the six documented string values are the wire/observable contract, reproduced verbatim.

/// A backend session became live.
pub const EVENT_SESSION_STARTED: &str = "session_started";
/// A turn finished cleanly.
pub const EVENT_TURN_COMPLETED: &str = "turn_completed";
/// A turn failed.
pub const EVENT_TURN_FAILED: &str = "turn_failed";
/// A mid-turn notification (short summarized payload).
pub const EVENT_NOTIFICATION: &str = "notification";
/// The backend failed to start a session.
pub const EVENT_STARTUP_FAILED: &str = "startup_failed";
/// `EVENT_OPERATOR_MESSAGE` is synthesized LOCALLY by the runner when an operator message is
/// actually written to the live turn's stdin (INF-250). It is NEVER parsed from claude's output; it
/// records the delivery ([`Event::message`] = the operator's text, [`Event::turn`] = the turn it was
/// folded into) so the orchestrator can mark the stored row delivered.
pub const EVENT_OPERATOR_MESSAGE: &str = "operator_message";

// --- TurnStatus -------------------------------------------------------------------------------
// The outcome of a single turn. Go models these as `type TurnStatus string`.

/// The turn completed successfully.
pub const TURN_SUCCEEDED: &str = "succeeded";
/// The turn failed.
pub const TURN_FAILED: &str = "failed";
/// The turn exceeded its timeout.
pub const TURN_TIMED_OUT: &str = "timed_out";

/// `Usage` holds token counts (upstream §13.5).
///
/// `input_tokens`/`output_tokens` are the UNCACHED input/output counts (so the "(in/out)"
/// breakdown stays meaningful). `total_tokens` is the BILLED total = uncached input + output +
/// cache-creation + cache-read; the cache portion is (`total_tokens` − `input_tokens` −
/// `output_tokens`). This doc contract is carried over verbatim from Go — downstream billing math
/// depends on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
}

/// `Event` is one normalized agent update.
///
/// `event_type` is Go's `Type EventType` (one of the `EVENT_*` constants); it is named `event_type`
/// because `type` is a Rust keyword. `timestamp` is Go's `time.Time`, modeled as
/// `Option<DateTime<Utc>>` so a zero-value `Event` is `Default`-constructible (`None` = unset); the
/// stream-json parser (A2) and runner (A3) fill it. `usage` is Go's `*Usage` pointer — present only
/// on usage-bearing events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub event_type: String,
    pub timestamp: Option<DateTime<Utc>>,
    /// subprocess pid (0 if unknown)
    pub pid: i64,
    /// short summarized payload
    pub message: String,
    /// present on usage-bearing events
    pub usage: Option<Usage>,
    /// `turn` is the 1-based turn number the event belongs to; set on `EVENT_OPERATOR_MESSAGE` so
    /// the orchestrator can record which turn an operator message was delivered into (INF-250).
    /// Zero on events that don't carry a turn.
    pub turn: i64,
}

/// `TurnResult` summarizes a completed turn.
///
/// `status` is Go's `Status TurnStatus` (one of the `TURN_*` constants; empty on a zero value).
/// `usage` is a value (Go's `Usage`, not a pointer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnResult {
    pub status: String,
    pub usage: Usage,
    /// `result_text` is the TAIL (last 4KB) of the final result text the agent emitted on this
    /// turn; empty on no-result exits. The hand-off marker lives at the END of the message, so the
    /// head is dropped on overflow. (taxonomy v2, INF-272)
    pub result_text: String,
}

/// `Transcript` captures an agent run's raw I/O for local logging (design spec §3).
///
/// Either field may be `None`. Passing `None` for the whole transcript (Go's `nil *Transcript`)
/// disables transcript capture. Go's `io.Writer` fields become boxed [`std::io::Write`] trait
/// objects — the runner (A3) writes the raw protocol stream one line per event, mirroring Go's
/// synchronous writes.
pub struct Transcript {
    /// raw protocol stream (e.g. stream-json), one line per event
    pub stdout: Option<Box<dyn std::io::Write + Send>>,
    /// agent diagnostics
    pub stderr: Option<Box<dyn std::io::Write + Send>>,
}

/// `Session` is one live coding-agent conversation for one issue. The thread stays logically alive
/// across continuation turns (upstream §7.1, §10.3).
///
/// The trait is `Send + Sync` so the P5 orchestrator can hold a `Box<dyn Session>` across `.await`
/// points and share it between tasks.
#[async_trait]
pub trait Session: Send + Sync {
    /// Returns `"<thread_id>-<turn_id>"` for the most recent turn (upstream §10.2).
    fn id(&self) -> String;

    /// Returns the stable backend thread/session id (empty before turn 1).
    fn thread_id(&self) -> String;

    /// Runs one turn with the given prompt, forwarding events to `on_event`. The returned
    /// `(TurnResult, Option<AgentError>)` mirrors Go's `(TurnResult, error)`: a `Some` error means
    /// the turn failed/timed out, and the `TurnResult` STILL carries the status (both are
    /// meaningful, so this is a tuple, not a `Result` that would drop one on error).
    ///
    /// `attempt` is Go's `*int` (the optional retry-attempt number). `messages` is the per-run
    /// operator mailbox (INF-250): while the turn is in flight the runner writes each received
    /// string to the agent's held-open stdin (folded into the ongoing turn at the next step
    /// boundary) and synthesizes an `EVENT_OPERATOR_MESSAGE`. `None` is valid and never yields —
    /// zero-cost for backends/tests with no mailbox. The SAME channel is passed across continuation
    /// turns, so a message that arrives between turns is drained at the start of the next turn.
    async fn run_turn(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> (TurnResult, Option<AgentError>);

    /// Releases any backend resources held by the session. Per-turn backends (like Claude, which
    /// spawns a fresh process per turn) may implement this as a no-op, since no persistent process
    /// outlives a turn.
    async fn stop(&self) -> Result<(), AgentError>;
}

/// `Runner` starts a coding-agent session bound to a workspace and issue.
#[async_trait]
pub trait Runner: Send + Sync {
    /// Prepares a session whose subprocess(es) run with the given absolute workspace path as cwd
    /// (upstream §10.1, §10.2). A `None` transcript disables local raw-output logging.
    async fn start_session(
        &self,
        workspace_path: &str,
        issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError>;
}

/// Typed agent error categories (upstream §10.6, §14.1) — the parity mirror of Go `errors.go`.
///
/// Each unit variant's `Display` reproduces the Go sentinel's exact text (e.g.
/// `ErrAgentNotFound = errors.New("agent_not_found")`), which is the observable contract callers
/// match on. [`AgentError::Other`] is the carrier for arbitrary/wrapped failures — the mirror of
/// Go's `errors.New`/`fmt.Errorf` values (a test-injected fake error, a backend transport failure).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// executable missing / not launchable
    #[error("agent_not_found")]
    AgentNotFound,
    #[error("agent_command_invalid")]
    InvalidCommand,
    #[error("startup_failed")]
    StartupFailed,
    #[error("turn_failed")]
    TurnFailed,
    #[error("turn_timeout")]
    TurnTimeout,
    /// Returned when the billing guard fails: the first system/init event did not report
    /// `apiKeySource == "none"`, so the agent would bill the metered API.
    #[error("billing_guard_failed")]
    BillingGuard,
    /// An arbitrary/wrapped failure carrying its message (Go's opaque `errors.New`/`fmt.Errorf`).
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `agent.TestEnumsAndZeroValues` (agent_test.go). Go asserts the constants are
    // non-empty; we assert their exact string VALUES (the "same string values" contract) for all
    // six event types + three turn statuses, then the zero-value shapes.
    #[test]
    fn enums_and_zero_values() {
        assert_eq!(EVENT_SESSION_STARTED, "session_started");
        assert_eq!(EVENT_TURN_COMPLETED, "turn_completed");
        assert_eq!(EVENT_TURN_FAILED, "turn_failed");
        assert_eq!(EVENT_NOTIFICATION, "notification");
        assert_eq!(EVENT_STARTUP_FAILED, "startup_failed");
        assert_eq!(EVENT_OPERATOR_MESSAGE, "operator_message");

        assert_eq!(TURN_SUCCEEDED, "succeeded");
        assert_eq!(TURN_FAILED, "failed");
        assert_eq!(TURN_TIMED_OUT, "timed_out");

        // Event.usage is None on a zero value (Go's `*Usage` nil).
        let ev = Event::default();
        assert!(
            ev.usage.is_none(),
            "Event.usage should be None on zero value"
        );

        // TurnResult zero status is the empty string (Go's `tr.Status == ""`).
        let tr = TurnResult::default();
        assert!(
            tr.status.is_empty(),
            "TurnResult zero status should be empty"
        );
    }

    // The typed sentinels' Display text is the observable contract; assert it byte-for-byte against
    // the Go `errors.New(...)` strings.
    #[test]
    fn agent_error_display_matches_go_sentinels() {
        assert_eq!(AgentError::AgentNotFound.to_string(), "agent_not_found");
        assert_eq!(
            AgentError::InvalidCommand.to_string(),
            "agent_command_invalid"
        );
        assert_eq!(AgentError::StartupFailed.to_string(), "startup_failed");
        assert_eq!(AgentError::TurnFailed.to_string(), "turn_failed");
        assert_eq!(AgentError::TurnTimeout.to_string(), "turn_timeout");
        assert_eq!(AgentError::BillingGuard.to_string(), "billing_guard_failed");
        assert_eq!(AgentError::Other("boom".into()).to_string(), "boom");
    }
}
