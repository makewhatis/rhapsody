//! Typed config model — parity port of the structs in Go `internal/config/config.go`.
//!
//! Two layers, exactly mirroring the Go file:
//!
//! * The public typed [`Config`] (Go `Config`) and its sub-structs are the runtime
//!   view produced by [`crate::decode::decode`] with per-field defaults applied. Go
//!   pointer fields that stay pointers in the typed struct — [`Storage::retention_days`],
//!   [`Server::port`], [`Claude::billing_guard`] — become `Option<T>` here, because the
//!   unset-vs-zero distinction is load-bearing (`retention_days: 0` = keep forever ≠ unset
//!   = 30). Pointers that Go collapses to a concrete value in the typed struct (e.g.
//!   `github_summons`, `otel.enabled`) become plain `bool`.
//! * The private [`Raw`] tree (Go `raw`/`rawProject`/`rawHooks`/`rawClaudeOverride`)
//!   mirrors the front-matter YAML schema field-for-field, one `serde` field per Go
//!   `yaml:"…"` tag. Every default-sensitive field is `Option<T>` so `decode` can tell
//!   "unset" from an explicit zero/false, exactly as the Go pointers do.

// NOTE: typed structs derive `Debug, Clone, PartialEq` (the C1 house convention) — not `Eq`,
// so a future float-bearing field never forces a churny de-derive.
use std::collections::HashMap;

use chrono::Duration;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

/// Default OTLP endpoint when `otel.endpoint` is unset. Empty by design: Rhapsody ships with no
/// fleet-observability hub (the Go daemon defaulted to a company-internal collector — a DIVERGENCE,
/// see README). With `otel.enabled` false by default nothing exports; an operator opting in sets
/// their own collector endpoint.
pub const DEFAULT_OTEL_ENDPOINT: &str = "";

// ---------------------------------------------------------------------------
// Override-knob vocabularies (Go `internal/config/config.go` const blocks)
// ---------------------------------------------------------------------------
//
// The accepted `dependency_mode` / `claim_mode` / `workspace_mode` values, plus the mode-on prompt
// default. Empty means inherit/default at the effective layer ([`crate::projects`]); these literals
// are both the validation allow-list (Go `*ModeValid`) and the resolved default the effective layer
// materializes. Ported verbatim from Go so the two crates agree on the exact tokens.

/// `dependency_mode` DEFAULT: a `blockedBy` edge clears only when the blocker is terminal — the
/// pre-feature daemon byte-for-byte (Go `DependencyModeDisabled`, INF-318).
pub const DEPENDENCY_MODE_DISABLED: &str = "disabled";
/// `dependency_mode` "graphite": unblock a dependent at a review state and dispatch it stacked on the
/// predecessor branch; requires a non-empty `review_states` (Go `DependencyModeGraphite`, INF-318).
pub const DEPENDENCY_MODE_GRAPHITE: &str = "graphite";
/// `dependency_mode` "dag": unblock only when ALL blockers are terminal/merged and dispatch fresh
/// from main (Go `DependencyModeDag`, INF-318).
pub const DEPENDENCY_MODE_DAG: &str = "dag";

/// The canonical repo-relative path of the mode-on run prompt rendered in graphite/dag mode; the
/// effective default for `dep_mode_prompt_file` (Go `DefaultDepModePromptFile`, INF-318). Rebranded
/// to `.rhapsody/` — the TRA-238 divergence from Go v0.4.0's `.symphony/PROMPT.dep_mod.md`. A repo
/// that still ships the legacy `.symphony/PROMPT.dep_mod.md` keeps resolving it via the worker's
/// `.rhapsody`→`.symphony` prompt fallback (see `resolve_prompt_template`).
pub const DEFAULT_DEP_MODE_PROMPT_FILE: &str = ".rhapsody/PROMPT.dep_mod.md";

/// `claim_mode` DEFAULT: fetch only issues assigned to the API-key owner, no claim election — today's
/// behavior exactly (Go `ClaimModeAssignee`, INF-477).
pub const CLAIM_MODE_ASSIGNEE: &str = "assignee";
/// `claim_mode` "pool": fetch UNASSIGNED issues and run the single-claimant claim protocol before
/// dispatch, so many daemons can share one project (Go `ClaimModePool`, INF-477).
pub const CLAIM_MODE_POOL: &str = "pool";

/// `workspace_mode` DEFAULT: a shared per-repo bare mirror with one `git worktree` per issue — the
/// pre-feature daemon byte-for-byte (Go `WorkspaceModeWorktree`, INF-418).
pub const WORKSPACE_MODE_WORKTREE: &str = "worktree";
/// `workspace_mode` "clone": provision each issue as an independent `git clone`, removing the
/// cross-ticket checkout lock at the cost of a full clone per dispatch (Go `WorkspaceModeClone`,
/// INF-418).
pub const WORKSPACE_MODE_CLONE: &str = "clone";

// ---------------------------------------------------------------------------
// Typed runtime model (Go `Config` and friends)
// ---------------------------------------------------------------------------

/// Tracker/polling source config (Go `Tracker`).
#[derive(Debug, Clone, PartialEq)]
pub struct Tracker {
    pub kind: String,
    pub endpoint: String,
    /// Resolved (`$VAR` expanded) in Task 5 / Resolve, not Decode — stored verbatim here.
    pub api_key: String,
    pub project_slug: String,
    /// Path to the JSON issue file when `kind == "file"`; ignored otherwise.
    pub source: String,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    /// Terminal states meaning "cancelled / won't do" (INF-272).
    pub canceled_states: Vec<String>,
    /// Non-active states a ticket is re-engaged from on an `@summon` (empty ⇒ feature OFF).
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub github_summons: bool,
    pub review_promote_state: String,
    pub milestone: String,
    pub labels: Vec<String>,
    pub capabilities: Vec<String>,
    /// `"" | "disabled" | "graphite" | "dag"` — empty ⇒ inherit/default at resolve (INF-318).
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    /// `"" | "assignee" | "pool"` — empty ⇒ inherit/default at resolve (INF-477).
    pub claim_mode: String,
    /// Freshness window for pool-mode claim comments; zero ⇒ orchestrator applies the default.
    pub claim_ttl: Duration,
    /// Base settle wait for pool-mode claims; zero ⇒ orchestrator applies the default.
    pub claim_settle_delay: Duration,
}

/// Polling cadence (Go `Polling`).
#[derive(Debug, Clone, PartialEq)]
pub struct Polling {
    pub interval_ms: i64,
}

/// Workspace root (Go `Workspace`; `root` normalized in Resolve, kept raw here).
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub root: String,
}

/// Lifecycle hook commands (Go `Hooks`).
#[derive(Debug, Clone, PartialEq)]
pub struct Hooks {
    pub after_create: String,
    pub before_run: String,
    pub after_run: String,
    pub before_remove: String,
    pub timeout_ms: i64,
}

/// Agent execution knobs (Go `Agent`).
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub backend: String,
    pub max_concurrent_agents: i64,
    pub max_turns: i64,
    pub max_retry_backoff_ms: i64,
    /// Per-state concurrency caps: keys lowercased, only positive ints kept (upstream §5.3.5).
    pub max_concurrent_agents_by_state: HashMap<String, i64>,
    /// Deprecated no-op since INF-266; still parsed + defaulted for backward compatibility.
    pub handoff_drain_grace_ms: i64,
}

/// Codex backend knobs (Go `Codex`).
#[derive(Debug, Clone, PartialEq)]
pub struct Codex {
    pub command: String,
    pub approval_policy: String,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: String,
    pub turn_timeout_ms: i64,
    pub read_timeout_ms: i64,
    pub stall_timeout_ms: i64,
}

/// Claude backend knobs (Go `Claude`).
#[derive(Debug, Clone, PartialEq)]
pub struct Claude {
    pub command: String,
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub allowed_tools: String,
    pub disallowed_tools: String,
    pub mcp_config: String,
    pub setting_sources: String,
    pub add_dirs: Vec<String>,
    pub turn_timeout_ms: i64,
    pub read_timeout_ms: i64,
    pub stall_timeout_ms: i64,
    pub extra_args: Vec<String>,
    /// Pointer in Go: absent (`None`) means enabled; Decode always fills it (`Some(true)` default).
    pub billing_guard: Option<bool>,
    pub ultracode: bool,
}

/// HTTP server config (Go `Server`). `port` stays optional — unset ⇒ no server / default elsewhere.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub port: Option<i64>,
}

/// Logging config (Go `Logging`; `dir` normalized in Resolve).
#[derive(Debug, Clone, PartialEq)]
pub struct Logging {
    pub dir: String,
}

/// Durable-store config (Go `Storage`). `retention_days` stays a pointer so an explicit
/// `0` ("keep forever") is distinguishable from unset (default 30, applied in Resolve).
#[derive(Debug, Clone, PartialEq)]
pub struct Storage {
    pub path: String,
    pub retention_days: Option<i64>,
}

/// OpenTelemetry export config (Go `Otel`). `enabled` is a plain bool — the presence-pointer
/// default (ON when absent, INF-442) is materialized in Decode.
///
/// `Default` yields the zero value (all fields empty/false) — NOT the config default (which Decode
/// materializes). It exists so `rhapsodyd`'s `run.go`-parity best-effort telemetry resolution can
/// build the synthetic `Otel{protocol:"http", service_name:"symphony", ..}` base when the workflow
/// fails to load, and so its otel-resolution tests can set individual fields on a zero base — exactly
/// as the Go tests build `config.Config{}` and assign `c.Otel.<field>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Otel {
    pub enabled: bool,
    pub endpoint: String,
    pub protocol: String,
    pub service_name: String,
    pub headers: HashMap<String, String>,
    pub insecure: bool,
    pub operator: String,
}

/// `symphony mcp` local-facade config (Go `MCP`, INF-473). Plain bools — the presence-pointer
/// defaults (inject + send_message + handoff ON, stop/resume OFF) are materialized in Decode.
#[derive(Debug, Clone, PartialEq)]
pub struct Mcp {
    pub enabled: bool,
    pub allow_send_message: bool,
    pub allow_stop: bool,
    pub allow_resume: bool,
    /// Registers the `symphony_handoff` write tool (default ON). Rhapsody-only knob, NEW beyond Go
    /// v0.4.0 (TRA-242): the daemon-mediated review handoff that moves the run's ticket to the review
    /// state and cleanly ends the run. It gates the tool's presence in `symphony mcp` (invisible when
    /// off) but is deliberately NOT surfaced in the `GET /api/v1/config` view / config round-trip, so
    /// the config goldens stay byte-identical to Go v0.4.0 (a documented divergence).
    pub allow_handoff: bool,
}

/// The subset of Claude knobs a project may override (Go `ClaudeOverride`). Every field is
/// `Option`/`Vec`: `None`/empty ⇒ inherit the top-level effective value (no default applied here).
/// `Default` (all-`None`/empty) mirrors a Go zero-value `ClaudeOverride{}` for constructing partial
/// overrides field-by-field.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClaudeOverride {
    pub command: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub disallowed_tools: Option<String>,
    pub mcp_config: Option<String>,
    pub setting_sources: Option<String>,
    pub add_dirs: Vec<String>,
    pub turn_timeout_ms: Option<i64>,
    pub read_timeout_ms: Option<i64>,
    pub stall_timeout_ms: Option<i64>,
    pub extra_args: Vec<String>,
    pub billing_guard: Option<bool>,
    pub ultracode: Option<bool>,
}

/// One multi-project entry (Go `Project`). Override fields keep their `Option`/`Vec` optionality
/// so `ResolveProjects` (Task 5) can distinguish inherit-from-top-level from an explicit value.
/// `Default` (all-empty/`None`) mirrors a Go zero-value `Project{}`, so tests and callers can build
/// one setting only the fields they need (`Project { slugs, ..Default::default() }`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Project {
    pub name: String,
    pub repo: String,
    pub slugs: Vec<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub canceled_states: Vec<String>,
    pub review_states: Vec<String>,
    pub prompt: String,
    pub prompt_file: String,
    pub claude: Option<ClaudeOverride>,
    pub hooks: Option<Hooks>,
    pub max_concurrent_agents: Option<i64>,
    pub milestone: String,
    pub labels: Vec<String>,
    pub capabilities: Vec<String>,
    pub git_flow: String,
    pub workspace_mode: String,
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    pub claim_mode: String,
    /// Per-project pause flag; `None` ⇒ enabled (default applied at resolve, INF-224).
    pub enabled: Option<bool>,
}

/// The typed runtime view of a workflow (Go `Config`, upstream §4.1.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub tracker: Tracker,
    pub polling: Polling,
    pub workspace: Workspace,
    pub hooks: Hooks,
    pub agent: Agent,
    pub codex: Codex,
    pub claude: Claude,
    pub server: Server,
    pub logging: Logging,
    pub otel: Otel,
    pub mcp: Mcp,
    pub storage: Storage,

    /// Default repo for the single-project form; `projects` takes precedence when present.
    pub repo: String,
    pub projects: Vec<Project>,

    /// Trimmed prompt body (raw; default applied at render time).
    pub prompt_template: String,
    /// When non-empty, wins over `prompt_template` (read at run time).
    pub prompt_file: String,
    /// Dir containing WORKFLOW.md, for relative resolution — set in Resolve (empty after Decode).
    pub workflow_dir: String,
    /// Path to the started WORKFLOW.md — stamped by the orchestrator (empty after Decode).
    pub workflow_path: String,

    /// Global git-workflow policy: `"" (== "any")` or `"graphite"` (INF-251).
    pub git_flow: String,
    /// Global workspace-provisioning policy: `"" (== "worktree")` or `"clone"` (INF-418).
    pub workspace_mode: String,
    /// GitHub label the post-run labeler adds; defaults to `"symphony"` in Decode (AIE-301).
    pub pr_label: String,
}

// ---------------------------------------------------------------------------
// Raw front-matter model (Go `raw` and friends)
// ---------------------------------------------------------------------------
//
// (De)serialization mirror of the YAML front matter — the SAME tree serves both directions,
// exactly as Go reuses its `raw` struct for `Decode` (unmarshal) and `Encode` (marshal). On the
// read path every field is `#[serde(default)]` (container-level) so an absent key lands on its
// zero value, like yaml.v3 unmarshal into a struct. On the write path (Task C6 `encode`) the
// derived `Serialize` emits EVERY field (no `skip_serializing_if`, mirroring the Go `raw` tags
// which carry no `omitempty`); `encode::prune_empty` does all the trimming afterwards. Default-
// sensitive fields are `Option<T>` (Go's `*int`/`*bool`) so `decode` can tell unset from an
// explicit zero/false and `encode` can drop an unset knob (null) while keeping an explicit
// zero/false. Unknown keys are ignored (no `deny_unknown_fields`), matching yaml.v3.

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Raw {
    pub tracker: RawTracker,
    pub polling: RawPolling,
    pub workspace: RawWorkspace,
    pub hooks: RawHooks,
    pub agent: RawAgent,
    pub codex: RawCodex,
    pub claude: RawClaude,
    pub server: RawServer,
    pub logging: RawLogging,
    pub storage: RawStorage,
    pub otel: RawOtel,
    pub mcp: RawMcp,
    pub repo: String,
    pub prompt_file: String,
    pub git_flow: String,
    pub workspace_mode: String,
    pub pr_label: String,
    pub projects: Vec<RawProject>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawTracker {
    pub kind: String,
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    pub source: String,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub canceled_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub github_summons: Option<bool>,
    pub review_promote_state: String,
    pub milestone: String,
    pub labels: Vec<String>,
    pub capabilities: Vec<String>,
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    pub claim_mode: String,
    pub claim_ttl: String,
    pub claim_settle_delay: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawPolling {
    pub interval_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawWorkspace {
    pub root: String,
}

/// Mirrors both Go `raw.Hooks` (top level) and `rawHooks` (per project) — identical schema.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawHooks {
    pub after_create: String,
    pub before_run: String,
    pub after_run: String,
    pub before_remove: String,
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawAgent {
    pub backend: String,
    pub max_concurrent_agents: Option<i64>,
    pub max_turns: Option<i64>,
    pub max_retry_backoff_ms: Option<i64>,
    pub handoff_drain_grace_ms: Option<i64>,
    /// `map[string]any` in Go — kept as raw YAML values; `normalize_state_map` extracts ints.
    pub max_concurrent_agents_by_state: HashMap<String, Value>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawCodex {
    pub command: String,
    pub approval_policy: String,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: String,
    pub turn_timeout_ms: Option<i64>,
    pub read_timeout_ms: Option<i64>,
    pub stall_timeout_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawClaude {
    pub command: String,
    pub model: String,
    pub effort: String,
    pub permission_mode: String,
    pub allowed_tools: String,
    pub disallowed_tools: String,
    pub mcp_config: String,
    pub setting_sources: String,
    pub add_dirs: Vec<String>,
    pub turn_timeout_ms: Option<i64>,
    pub read_timeout_ms: Option<i64>,
    pub stall_timeout_ms: Option<i64>,
    pub extra_args: Vec<String>,
    pub billing_guard: Option<bool>,
    pub ultracode: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawServer {
    pub port: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawLogging {
    pub dir: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawStorage {
    pub path: String,
    pub retention_days: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawOtel {
    pub enabled: Option<bool>,
    pub endpoint: String,
    pub protocol: String,
    pub service_name: String,
    pub headers: HashMap<String, String>,
    pub insecure: bool,
    pub operator: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawMcp {
    pub enabled: Option<bool>,
    pub allow_send_message: Option<bool>,
    pub allow_stop: Option<bool>,
    pub allow_resume: Option<bool>,
    /// `symphony_handoff` toggle (TRA-242, default ON). `Option<bool>` so `decode` tells an explicit
    /// opt-out from unset; NEW beyond Go v0.4.0 (no yaml tag on the Go raw struct — Rhapsody-only).
    pub allow_handoff: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawProject {
    pub name: String,
    pub repo: String,
    pub slugs: Vec<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    pub canceled_states: Vec<String>,
    pub review_states: Vec<String>,
    pub prompt: String,
    pub prompt_file: String,
    pub git_flow: String,
    pub workspace_mode: String,
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    pub claim_mode: String,
    pub claude: Option<RawClaudeOverride>,
    pub hooks: Option<RawHooks>,
    pub max_concurrent_agents: Option<i64>,
    pub milestone: String,
    pub labels: Vec<String>,
    pub capabilities: Vec<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct RawClaudeOverride {
    pub command: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub disallowed_tools: Option<String>,
    pub mcp_config: Option<String>,
    pub setting_sources: Option<String>,
    pub add_dirs: Vec<String>,
    pub turn_timeout_ms: Option<i64>,
    pub read_timeout_ms: Option<i64>,
    pub stall_timeout_ms: Option<i64>,
    pub extra_args: Vec<String>,
    pub billing_guard: Option<bool>,
    pub ultracode: Option<bool>,
}
