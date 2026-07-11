//! rhapsody-httpapi — parity port of Go `internal/httpapi` (the OPTIONAL observability HTTP
//! extension, upstream §13.7): a loopback JSON API + embedded React dashboard, read-only except
//! `/refresh`.
//!
//! # H-lane serial chain
//!
//! This crate is delivered as a serial chain of tickets (H1–H3), each porting a group of the Go
//! package's files onto the one server this crate roots.
//!
//! **H1** ported the server core:
//!
//! * [`server`] — the [`StateProvider`] interface the handlers read, the mux/route table
//!   ([`new_handler`]), and the loopback [`Server`] listener wrapper (`$REF/…/server.go`).
//! * [`responses`] — the response-envelope plumbing: `writeJSON`/`writeError` + the healthz/error
//!   DTOs (`$REF/…/{handlers,responses}.go`).
//! * [`handlers`] — `/healthz` and `GET /api/v1/state`. The `/state` wire view is O4's
//!   [`rhapsody_orchestrator::snapshot_json`], which the handler REUSES rather than reimplementing
//!   (the plan's byte-parity rule) — the analog of the config handler reusing `effective_json`.
//! * [`web`] — the `rust-embed` dashboard embed + SPA fallback (`$REF/…/{web,web_dist_placeholder}.go`).
//!
//! **This ticket (H2)** adds every READ handler + its goldens (`$REF/…/{handlers_history,
//! handlers_projects,handlers_linear,handlers_logs,history,responses_history}.go`):
//!
//! * [`handlers_history`] — history, issue history, run detail, run events, run transcript, event
//!   search, metrics; [`responses_history`] their wire views; [`history`] the read-only
//!   [`HistoryStore`] narrowing of `rhapsody_store::Store`.
//! * [`handlers_projects`] / [`handlers_linear`] — per-project live status + the read-only Linear
//!   proxy (projects picker + connected-as identity).
//! * [`handlers_logs`] / [`logs`] — the process-log ring snapshot + SSE stream and the [`LogSource`]
//!   interface (the concrete `telemetry.LogBuffer` implementor lands with the telemetry lane T1).
//!
//! H3 adds the write handlers (`/config`, `/refresh`, run actions, messages). The route registration
//! + method-agnostic 405 semantics live in [`server`]'s `build_router`.

mod handlers;
mod handlers_history;
mod handlers_linear;
mod handlers_logs;
mod handlers_projects;
mod history;
mod logs;
mod responses;
mod responses_history;
mod server;
mod web;

#[cfg(test)]
mod goldens;
#[cfg(test)]
mod testutil;

pub use history::HistoryStore;
pub use logs::{LogEntry, LogSource};
pub use server::{Server, SnapshotError, StateProvider, new_handler};
