//! rhapsodyd library — the pieces the daemon binary composes.
//!
//! The `rhapsodyd` package carries both a binary ([`main`](../main.rs), the daemon entrypoint) and
//! this library of the pieces it assembles — the parity port of Go `cmd/symphony`
//! (`$REF/cmd/symphony/{main,run,mcp}.go`). `rhapsodyd` is the Rhapsody daemon binary (the Rust port
//! of the Go `symphony` daemon); its runtime behavior — including operator-facing stderr diagnostics
//! — stays a faithful clone of the Go daemon, so only the binary's NAME differs.
//!
//! * [`run`] — the daemon boot (`run.go`): flag parsing, the single-instance run-lock, telemetry +
//!   orchestrator + observability-server wiring, the startup banner, the prune scheduler, and graceful
//!   shutdown/drain.
//! * [`mcp`] — the `symphony mcp` subcommand (`mcp.go`): the local MCP facade over stdio (→ `crates/mcp`).
//! * [`runlock`] — the advisory single-instance flock (`run.go`'s lock section).
//! * [`state`] — the httpapi [`rhapsody_httpapi::StateProvider`] adapter over the orchestrator's
//!   off-loop [`rhapsody_orchestrator::ControlHandle`] (Go passes `*Orchestrator` directly; Rust can't
//!   alias the loop's `&mut self`, so the daemon serves HTTP through the cloneable handle).
//! * [`logsource`] — the httpapi [`rhapsody_httpapi::LogSource`] adapter over the telemetry log ring.
//! * [`banner`] — the colorful startup banner (Go `internal/banner`).

pub mod banner;
pub mod bootcfg;
pub mod logsource;
pub mod mcp;
pub mod otel;
pub mod prune;
pub mod run;
pub mod runlock;
pub mod state;

#[cfg(test)]
mod testutil;
