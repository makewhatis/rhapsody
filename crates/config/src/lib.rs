//! rhapsody-config — parity port of Go `internal/config`.
//!
//! `internal/workflow` and `internal/prompt` become modules of this crate (P0
//! crate layout): [`workflow`] ports the WORKFLOW.md front-matter loader + save
//! (Go `internal/workflow`); [`prompt`] ports strict Liquid rendering of the
//! prompt body (Go `internal/prompt`, Task C7).
//!
//! [`model`] holds the typed [`Config`] (Go `Config`) plus the private raw
//! front-matter schema; [`decode`] ports Go `config.Decode` — a workflow
//! [`Definition`](workflow::Definition) into a defaulted (but not yet resolved or
//! validated) [`Config`].

pub mod decode;
pub mod model;
pub mod prompt;
pub mod resolve;
pub mod workflow;

pub use decode::{ConfigError, decode};
pub use model::{
    Agent, Claude, ClaudeOverride, Codex, Config, DEFAULT_OTEL_ENDPOINT, Hooks, Logging, Mcp, Otel,
    Polling, Project, Server, Storage, Tracker, Workspace,
};
pub use resolve::{Resolved, resolve};
