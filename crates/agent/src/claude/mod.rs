//! claude — the Claude Code agent backend (parity port of Go `internal/agent/claude`).
//!
//! Symphony drives `claude` headlessly as a subprocess and maps its stream-json output to the
//! normalized [`crate`] agent events (design-spec §8; upstream §10). This module ports the pieces
//! that are pure and backend-shaped:
//!
//! * [`args`] — [`Config`] plus CLI argv construction ([`build_args`], [`split_command`]),
//!   byte-compatible with Go `args.go` (flag order is the contract; see `args_test.go`).
//! * [`parse`] — one stream-json line → a normalized [`crate::Event`] ([`classify`]), including the
//!   uncached-vs-billed usage extraction (Go `parse.go`).
//! * [`billing`] — the env scrub + `apiKeySource` billing guard (Go `billing.go`).
//! * [`mcpinject`] — per-workspace `.mcp.json` merge + the `SYMPHONY_*` "me" identity env
//!   (Go `mcpinject.go` + `appendMeEnv`).
//!
//! The subprocess runner itself (Go `runner.go`) lands in the follow-up task A3; it consumes every
//! item this module exposes. Those items are `pub` (not `pub(crate)`) so that sibling module — not
//! yet present — can reference them without tripping `dead_code` under `-D warnings`; Go keeps them
//! package-private, but Rust visibility is not observable behavior, and this avoids a
//! `#[allow(dead_code)]`.

pub mod args;
pub mod billing;
pub mod mcpinject;
pub mod parse;

pub use args::{Config, build_args, split_command};
pub use billing::{
    BILLING_ENV_VARS, TRACKER_ENV_VARS, billing_guard_enabled, billing_guard_ok, scrub_env,
    scrubbed_env_vars,
};
pub use mcpinject::{MERGED_MCP_CONFIG_NAME, append_me_env, inject_symphony_mcp};
pub use parse::{Classified, classify};
