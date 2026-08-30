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

pub mod capabilities;
pub mod decode;
pub mod effective_json;
pub mod encode;
pub mod memory;
pub mod model;
pub mod profiles;
pub mod projects;
pub mod prompt;
pub mod resolve;
pub mod room;
pub mod teams;
pub mod validate;
pub mod workflow;

pub use decode::{ConfigError, decode};
pub use encode::{encode, go_duration_string};
pub use model::{
    Agent, CLAIM_MODE_ASSIGNEE, CLAIM_MODE_POOL, Claude, ClaudeOverride, Codex, Config,
    DEFAULT_DEP_MODE_PROMPT_FILE, DEFAULT_OTEL_ENDPOINT, DEPENDENCY_MODE_DAG,
    DEPENDENCY_MODE_DISABLED, DEPENDENCY_MODE_GRAPHITE, Hooks, Logging, Mcp, Otel, Polling,
    Project, Server, Storage, Tracker, WORKSPACE_MODE_CLONE, WORKSPACE_MODE_WORKTREE, Workspace,
};
pub use projects::{EffectiveConfig, ResolvedProject, effective_for, resolve_projects};
pub use resolve::{Resolved, resolve};
pub use validate::{ValidationError, validate};
