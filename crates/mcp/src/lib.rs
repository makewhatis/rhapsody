//! rhapsody-mcp — parity port of Go `internal/mcpfacade` (the `symphony mcp` local MCP facade,
//! INF-473). A thin, read-mostly server over the daemon's loopback `/api/v1` HTTP API, built on the
//! official `rmcp` SDK. It reads NOTHING from `~/.symphony` or the DB except the daemon's published
//! port (`runtime.json` discovery) — the daemon stays the single source of truth; daemon down ⇒ a
//! clear `daemon_unreachable` error.
//!
//! The always-on read tools (`symphony_state` / `_runs` / `_run` / `_ticket` / `_logs` / `_events` +
//! the derived `_run_status`) ship alongside the config-gated write tools (`symphony_send_message`,
//! default on; `symphony_stop` / `symphony_resume`, opt-in) — the latter registered per the `mcp:`
//! config block (P6-M2).
//!
//! - [`Client`] — the loopback HTTP client (client.go).
//! - [`Facade`] / [`Options`] — the rmcp server + "me" defaults (server.go + writes.go); served by
//!   [`Facade::run_stdio`].
//! - [`resolve_daemon_port`] — runtime.json → `server.port` discovery (mcp.go's `daemonPort`).
//! - [`Status`] — the `symphony_run_status` verdict (verdict.go).

mod client;
mod discovery;
mod server;
mod status;
mod verdict;
mod writes;

#[cfg(test)]
mod testutil;

pub use client::{Client, FacadeError};
pub use discovery::resolve_daemon_port;
pub use server::{Facade, Options, VERSION};
pub use verdict::Status;
