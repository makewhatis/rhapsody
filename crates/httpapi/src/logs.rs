//! logs — the daemon process-log surface the Logs settings tab reads. Parity port of the `LogSource`
//! interface + `telemetry.LogEntry` wire shape of Go `$REF/internal/httpapi/handlers_logs.go` (which
//! is satisfied by `telemetry.LogBuffer`).
//!
//! The concrete log ring (`telemetry.LogBuffer`) lands with the telemetry lane (T1); until then this
//! crate owns the [`LogSource`] trait + the [`LogEntry`] wire type it yields (exactly as H1 owns
//! [`crate::StateProvider`] without the orchestrator implementor). T1's `LogBuffer` implements
//! [`LogSource`]; F1 wires the live buffer into [`crate::new_handler`].

use std::collections::BTreeMap;

use serde::Serialize;
use tokio::sync::broadcast;

/// One retained daemon-log line on the wire (`GET /api/v1/logs` + each SSE frame). Mirrors Go
/// `telemetry.LogEntry` (`{seq, time, level, msg, attrs?}`): `attrs` is omitted when empty (Go's
/// `omitempty`), and `time` is a pre-formatted RFC3339 string (Go serializes `time.Time` as RFC3339).
/// [`Clone`] is required so a [`broadcast`] subscriber can receive owned copies.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Per-process monotonic sequence (resets each process; the stream's epoch signals a reset).
    pub seq: u64,
    /// RFC3339 timestamp.
    pub time: String,
    pub level: String,
    pub msg: String,
    /// Flattened, group-qualified, string-rendered attributes. Omitted when empty (Go `omitempty`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
}

/// The daemon's in-memory process-log ring, exposed to the HTTP layer. Mirrors Go's `LogSource`
/// interface (satisfied by `telemetry.LogBuffer`). A `None` source is tolerated end-to-end: `/logs`
/// returns `[]` and `/logs/stream` holds open emitting only heartbeats, so a daemon (or test) without
/// a buffer still answers 200 rather than 500.
pub trait LogSource: Send + Sync {
    /// The retained entries, oldest first. Mirrors Go `Snapshot`.
    fn snapshot(&self) -> Vec<LogEntry>;
    /// A subscription to subsequent entries. Dropping the receiver unsubscribes — the Rust idiom for
    /// Go `Subscribe`'s returned cancel func. Mirrors Go `Subscribe`.
    fn subscribe(&self) -> broadcast::Receiver<LogEntry>;
    /// Identifies this process's stream (seq resets per process); the stream emits it so the client can
    /// detect a restart and reset its de-dup watermark. Mirrors Go `Epoch`.
    fn epoch(&self) -> u64;
}
