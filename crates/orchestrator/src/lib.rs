//! rhapsody-orchestrator — parity port of Go `internal/orchestrator` (the daemon's heart).
//!
//! # P5 serial ticket chain (O1–O8)
//!
//! The Go orchestrator is one package split across 26 source files; the Rust port mirrors that
//! file split one-to-one and is delivered as a serial chain of tickets (O1–O8), each porting a
//! group of files. Because every file shares the one `Orchestrator` struct, later tickets extend
//! the types this crate roots here (adding fields to [`Orchestrator`]/[`orchestrator::RunningEntry`]
//! and variants to the control-event set) as their behavior lands.
//!
//! **This ticket (O1)** ports the *core state* and the *effective config* view:
//!
//! * [`orchestrator`] — the [`Orchestrator`] scheduling state (running / claimed / retrying /
//!   completed maps, the pending-stack map, token totals, the resolved [`Effective`] config) and
//!   its constructor. The Go orchestrator is loop-confined (only the control task mutates state);
//!   the Rust design keeps that discipline — a single owning task, channels in / channels out.
//! * [`effective`] — [`build_effective`], which turns a resolved [`rhapsody_config::Config`] into
//!   the per-project runtime view ([`Effective`] / [`ResolvedProject`]) the loop schedules against.
//! * [`telemetry_attrs`] — the bounded metric-label builders (the cardinality contract).
//!
//! Three tiny Go helper packages the orchestrator depends on have no dedicated Rust crate
//! (`internal/liveness`, `internal/obslog`, `internal/ghsummons`); they are ported here as
//! orchestrator-internal modules ([`liveness`], [`obslog`], [`ghsummons`]) — the orchestrator is
//! their sole consumer this phase.
//!
//! **Compiling-stub protocol.** Where a method or type owned by a strictly-later ticket is
//! referenced, this crate adds a minimal compiling stub returning
//! [`OrchestratorError::Unimplemented`] (never `todo!()`/`panic!`), tagged with a stub marker
//! naming the owning ticket (the `O<N>` form, per the P5 plan). Each later ticket replaces its own
//! stubs before its PR, and O8's completion gate asserts no such markers remain anywhere under
//! `crates/orchestrator`. This O1 slice — the core state, the effective config view, and the
//! telemetry attrs — compiles standalone and introduces none.

pub mod agentupdate;
pub mod backoff;
pub mod claim;
pub mod concurrency;
// `loop` is a reserved word; the file mirrors Go `loop.go` while the module is `control_loop`.
#[path = "loop.rs"]
pub mod control_loop;
pub mod dispatch;
pub mod effective;
pub mod ghenrich;
pub mod ghsummons;
pub mod handoff;
pub mod issuelog;
pub mod liveness;
pub mod message;
pub mod obslog;
pub mod orchestrator;
pub mod persist;
pub mod promote;
pub mod reads;
pub mod reconcile;
pub mod reconcile_run;
pub mod recovery;
pub mod reload;
pub mod retry;
pub mod select;
pub mod snapshot;
pub mod snapshot_json;
pub mod stop;
pub mod telemetry_attrs;
pub mod warnings;
pub mod worker;
pub mod workspace_gc;

#[cfg(test)]
mod testsupport;

// O8 e2e gate: the INF-303 no-Linear end-to-end suite (real file tracker + real runner + committed
// fake-claude + in-memory store), driving the assembled control pass. Test-only.
#[cfg(test)]
mod filetracker_e2e;

pub use agentupdate::AgentUpdate;
pub use backoff::{CONTINUATION_DELAY_MS, failure_backoff_ms};
pub use concurrency::{global_slots, state_limit};
pub use control_loop::{CancelSignal, CancelWait, Event, WaitGroup};
pub use dispatch::{eligible, sort_for_dispatch};
pub use effective::{Effective, ResolvedProject, build_effective, build_effective_with_runner};
pub use handoff::HandoffResult;
pub use message::RunMessageResult;
pub use orchestrator::{EventRecord, Orchestrator, RetryEntry, RunningEntry, StackHint, Totals};
pub use reads::{Identity, ReadsError, ReadsTarget};
pub use reconcile::{ActionKind, ReconcileAction, reconcile_actions};
pub use reload::ReloadError;
pub use retry::{EvRetry, EvWorkerExit};
pub use snapshot::{
    ProjectStatus, RateLimit, RefreshResult, RetryRow, RunningRow, Snapshot, TokenCounts,
};
pub use stop::{ControlHandle, ResumeResult, StopError, StopResult};
pub use worker::{WorkerDeps, WorkerError, run_agent_attempt};
pub use workspace_gc::WorkspaceGcPlan;

/// Typed orchestrator error categories.
///
/// [`OrchestratorError::UnsupportedBackend`] mirrors Go `ErrUnsupportedBackend`
/// (`agent_backend_unsupported`), the sentinel `build_effective` returns for an unimplemented
/// `agent.backend`. [`OrchestratorError::Unimplemented`] is the compiling-stub carrier used by the
/// P5 ticket chain (see the crate docs). [`OrchestratorError::Workspace`] carries a workspace-manager
/// construction failure verbatim, mirroring Go `buildEffective` returning the `NewManager` error.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// A method or type owned by a strictly-later P5 ticket is referenced but not yet ported. The
    /// message names the owning file + ticket (e.g. `"worker.rs — ticket O3"`). Never a panic.
    #[error("unimplemented: {0}")]
    Unimplemented(String),
    /// `agent_backend_unsupported` — `agent.backend` names a backend this build does not implement
    /// (only `"claude"` is implemented). Mirrors Go `ErrUnsupportedBackend`; the payload is the
    /// offending backend name (Go wraps it as `%w: %q`).
    #[error("agent_backend_unsupported: {0:?}")]
    UnsupportedBackend(String),
    /// A workspace-manager construction failure surfaced from [`build_effective`] (Go returns the
    /// `workspace.NewManager` error verbatim).
    #[error(transparent)]
    Workspace(#[from] rhapsody_workspace::Error),
}
