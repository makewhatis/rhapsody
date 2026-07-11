//! rhapsody-httpapi — parity port of Go `internal/httpapi` (the OPTIONAL observability HTTP
//! extension, upstream §13.7): a loopback JSON API + embedded React dashboard, read-only except
//! `/refresh`.
//!
//! # H-lane serial chain
//!
//! This crate is delivered as a serial chain of tickets (H1–H3), each porting a group of the Go
//! package's files onto the one server this crate roots. **This ticket (H1)** ports the server core:
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
//! H2 adds the read handlers + goldens; H3 the write handlers (`/config`, `/refresh`, run actions,
//! messages). Reference: `$REF/internal/httpapi/{server,handlers,responses,web,web_dist_placeholder}.go`
//! and their `*_test.go`.

mod handlers;
mod responses;
mod server;
mod web;

#[cfg(test)]
mod testutil;

pub use server::{Server, SnapshotError, StateProvider, new_handler};
