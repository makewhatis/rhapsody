//! config_view — the WRITE side of the `/api/v1/config` endpoint: the POST request DTOs and the
//! decode→apply→encode machinery that patches the typed Settings edit onto the on-disk config.
//! Parity port of the write half of `$REF/internal/httpapi/config_view.go` (`configJSON` +
//! `definitionFromRequest`/`applyTypedConfig`/`projectFromJSON`/`classifyConfigError`), INF-224.
//!
//! The READ side (the GET/POST *response* body) is `rhapsody_config::effective_json::render`, which
//! the handler REUSES rather than reimplementing (the plan's byte-parity rule) — so only the request
//! DTOs and the base⊕edit merge live here.
//!
//! # Deviation from Go: unknown-field handling
//!
//! Go decodes the POST body with `dec.DisallowUnknownFields()`. The Rust DTOs instead declare only
//! the fields the POST *reads* and let serde ignore the rest — so the display-only echo fields the
//! GET view emits (`generated_at`, `global.tracker.api_key_set`, each project's `effective` +
//! `workspace_mode_recommended`) round-trip without being declared as ignored (which would be dead
//! code the workspace lint forbids). This is behaviourally identical on every mirrored test (none
//! sends an unknown field expecting a 400); the only difference is that a genuinely unknown key is
//! silently ignored rather than 400'd — an accepted, documented simplification.

use serde::{Deserialize, Deserializer};

use rhapsody_config::workflow::{Definition, load};
use rhapsody_config::{ClaudeOverride, Config, Project, ValidationError, decode, encode};

use crate::responses::FieldError;
use crate::server::ConfigValidateError;

/// Deserialize a value that may arrive as JSON `null` (Go marshals an empty `[]string`/map as `null`
/// via `slice_or_null`) into its `Default`. Composes with container `#[serde(default)]`, which covers
/// the absent-key case; this covers the present-but-null case.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// POST request DTOs (Go configJSON + friends; DESERIALIZE side only)
// ---------------------------------------------------------------------------

/// The POST `/api/v1/config` request body (Go `configJSON`). The `global` block's presence selects
/// the typed path over the legacy verbatim-map path.
#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct ConfigPostReq {
    /// The legacy verbatim front-matter map — persisted as-is when `global` is absent.
    config: serde_json::Value,
    /// The prompt body persisted on the legacy path.
    prompt_body: String,
    /// The typed global block; `Some` ⇒ the typed patch path.
    global: Option<GlobalReq>,
    /// The wholesale-replaced agent list (typed path).
    projects: Option<Vec<ProjectReq>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalReq {
    tracker: GlobalTrackerReq,
    polling: GlobalPollingReq,
    agent: GlobalAgentReq,
    claude: GlobalClaudeReq,
    workspace: GlobalWorkspaceReq,
    storage: GlobalStorageReq,
    otel: GlobalOtelReq,
    mcp: GlobalMcpReq,
    server: GlobalServerReq,
    logging: GlobalLoggingReq,
    repo: String,
    #[serde(deserialize_with = "null_default")]
    active_states: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    terminal_states: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    canceled_states: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    review_states: Vec<String>,
    review_promote_state: String,
    summon_token: String,
    github_summons: bool,
    milestone: String,
    #[serde(deserialize_with = "null_default")]
    labels: Vec<String>,
    prompt: String,
    prompt_file: String,
    git_flow: String,
    workspace_mode: String,
    dependency_mode: String,
    dep_mode_prompt_file: String,
    claim_mode: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalTrackerReq {
    kind: String,
    endpoint: String,
    // api_key_set is display-only (the value is never sent); ignored on POST.
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalPollingReq {
    interval_ms: i64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalAgentReq {
    backend: String,
    max_concurrent_agents: i64,
    max_turns: i64,
    max_retry_backoff_ms: i64,
    max_concurrent_agents_by_state: std::collections::HashMap<String, i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalClaudeReq {
    command: String,
    model: String,
    effort: String,
    permission_mode: String,
    billing_guard: bool,
    ultracode: bool,
    turn_timeout_ms: i64,
    read_timeout_ms: i64,
    stall_timeout_ms: i64,
    mcp_config: String,
    extra_args: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalWorkspaceReq {
    root: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalStorageReq {
    path: String,
    retention_days: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalOtelReq {
    enabled: bool,
    endpoint: String,
    protocol: String,
    service_name: String,
    insecure: bool,
    headers: std::collections::HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalMcpReq {
    enabled: bool,
    allow_send_message: bool,
    allow_stop: bool,
    allow_resume: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalServerReq {
    port: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GlobalLoggingReq {
    dir: String,
}

/// One agent's typed payload (Go `projectConfigJSON`, deserialize side). `effective` +
/// `workspace_mode_recommended` are display-only and ignored on POST (see the module note).
#[derive(Deserialize, Default)]
#[serde(default)]
struct ProjectReq {
    name: String,
    slugs: Vec<String>,
    repo: String,
    milestone: String,
    labels: Vec<String>,
    capabilities: Vec<String>,
    /// Presence-pointer: absent ⇒ inherit the default (enabled), `Some(false)` ⇒ paused.
    enabled: Option<bool>,
    active_states: Vec<String>,
    terminal_states: Vec<String>,
    canceled_states: Vec<String>,
    review_states: Vec<String>,
    max_concurrent_agents: Option<i64>,
    prompt: String,
    prompt_file: String,
    overrides: ClaudeOverridesReq,
}

/// The sparse per-project claude override map (Go `claudeOverridesJSON`): a present key overrides, an
/// absent one inherits. Also carries the top-level project knobs surfaced in the overrides block.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ClaudeOverridesReq {
    model: Option<String>,
    effort: Option<String>,
    permission: Option<String>,
    ultracode: Option<bool>,
    turn_timeout_ms: Option<i64>,
    stall_timeout_ms: Option<i64>,
    billing_guard: Option<bool>,
    command: Option<String>,
    git_flow: Option<String>,
    workspace_mode: Option<String>,
    dependency_mode: Option<String>,
    dep_mode_prompt_file: Option<String>,
    claim_mode: Option<String>,
}

impl ConfigPostReq {
    /// Whether the typed path applies (Go `req.Global != nil`): a present `global` block patches the
    /// typed knobs onto the on-disk config; its absence takes the legacy verbatim-map path.
    pub(crate) fn is_typed(&self) -> bool {
        self.global.is_some()
    }

    /// The legacy verbatim [`Definition`] (Go `definitionFromRequest`, `req.Global == nil` branch):
    /// the submitted front-matter map + prompt body, persisted as-is. The JSON `config` object is
    /// re-encoded to the YAML front-matter map `workflow::save` writes.
    pub(crate) fn legacy_definition(&self) -> Definition {
        let config = match serde_yaml_ng::to_value(&self.config) {
            Ok(serde_yaml_ng::Value::Mapping(map)) => map,
            _ => serde_yaml_ng::Mapping::new(),
        };
        Definition {
            config,
            prompt_template: self.prompt_body.clone(),
        }
    }
}

/// Build the typed [`Definition`] to validate + persist (Go `definitionFromRequest`, `req.Global !=
/// nil` branch): load the current on-disk config, decode it to the typed base, patch the typed edit
/// onto it, and re-encode via `config::encode` (so the `api_key` indirection + any unlisted knob are
/// preserved from disk). `Err` (surfaced as 500 `config_unavailable`) carries the failing step's
/// message.
pub(crate) fn build_typed_definition(
    workflow_path: &str,
    req: &ConfigPostReq,
) -> Result<Definition, String> {
    let base_def = load(std::path::Path::new(workflow_path)).map_err(|e| e.to_string())?;
    let base = decode(&base_def).map_err(|e| e.to_string())?;
    encode(&apply_typed_config(&base, req)).map_err(|e| e.to_string())
}

/// Patch the editable typed knobs from a POST body onto the current on-disk config `base` (Go
/// `applyTypedConfig`). The global block overwrites the knobs the typed view owns (`api_key` is
/// preserved from `base`; `project_slug` is cleared so `encode` picks the single-vs-projects form
/// from `projects`); `projects` is replaced WHOLESALE so omitting an entry removes that agent. Knobs
/// the typed view does not surface (other claude fields, hooks, codex, claim timings) are preserved by
/// the `base.clone()` shallow copy / by matching the base project on its slug.
fn apply_typed_config(base: &Config, req: &ConfigPostReq) -> Config {
    let mut out = base.clone();
    let Some(g) = req.global.as_ref() else {
        // Only reached on the typed path (`global` is `Some`); defensively return the base unchanged.
        return out;
    };

    out.tracker.kind = g.tracker.kind.clone();
    out.tracker.endpoint = g.tracker.endpoint.clone();
    // api_key: never sent over the wire; preserved from disk (already on `out` via the clone).
    out.tracker.project_slug = String::new(); // encode derives the form from Projects
    out.tracker.active_states = g.active_states.clone();
    out.tracker.terminal_states = g.terminal_states.clone();
    out.tracker.canceled_states = g.canceled_states.clone();
    out.tracker.review_states = g.review_states.clone();
    out.tracker.review_promote_state = g.review_promote_state.clone();
    out.tracker.summon_token = g.summon_token.clone();
    out.tracker.github_summons = g.github_summons;
    out.tracker.milestone = g.milestone.clone();
    out.tracker.labels = g.labels.clone();
    // tracker.capabilities is NOT surfaced in the typed `global` GET view (BO-10 keeps it out for Go
    // byte-parity — the frozen Go v0.4.0 config goldens have no such field). So, like `pr_label` and
    // the other unexposed global knobs, it is deliberately NOT overwritten here: the `base.clone()`
    // above preserves the on-disk value across a Save, rather than a verbatim round-trip (which cannot
    // echo back a field the view never emitted) silently clearing it. Per-project `capabilities` IS
    // surfaced (projects[] omitempty) and so is round-tripped in `project_from_json`.

    out.polling.interval_ms = g.polling.interval_ms;

    out.agent.backend = g.agent.backend.clone();
    out.agent.max_concurrent_agents = g.agent.max_concurrent_agents;
    out.agent.max_turns = g.agent.max_turns;
    out.agent.max_retry_backoff_ms = g.agent.max_retry_backoff_ms;
    out.agent.max_concurrent_agents_by_state = g.agent.max_concurrent_agents_by_state.clone();

    out.claude.command = g.claude.command.clone();
    out.claude.model = g.claude.model.clone();
    out.claude.effort = g.claude.effort.clone();
    out.claude.permission_mode = g.claude.permission_mode.clone();
    out.claude.mcp_config = g.claude.mcp_config.clone();
    out.claude.turn_timeout_ms = g.claude.turn_timeout_ms;
    out.claude.read_timeout_ms = g.claude.read_timeout_ms;
    out.claude.stall_timeout_ms = g.claude.stall_timeout_ms;
    out.claude.extra_args = g.claude.extra_args.clone();
    out.claude.billing_guard = Some(g.claude.billing_guard);
    out.claude.ultracode = g.claude.ultracode;
    // allowed_tools / disallowed_tools / setting_sources / add_dirs are not in the typed view; the
    // shallow copy retains them from base.

    out.workspace.root = g.workspace.root.clone();
    out.storage.path = g.storage.path.clone();
    out.storage.retention_days = g.storage.retention_days;
    out.otel.enabled = g.otel.enabled;
    out.otel.endpoint = g.otel.endpoint.clone();
    out.otel.protocol = g.otel.protocol.clone();
    out.otel.service_name = g.otel.service_name.clone();
    out.otel.insecure = g.otel.insecure;
    out.otel.headers = g.otel.headers.clone();
    out.mcp.enabled = g.mcp.enabled;
    out.mcp.allow_send_message = g.mcp.allow_send_message;
    out.mcp.allow_stop = g.mcp.allow_stop;
    out.mcp.allow_resume = g.mcp.allow_resume;
    out.server.port = g.server.port;
    out.logging.dir = g.logging.dir.clone();
    out.repo = g.repo.clone();
    out.prompt_template = g.prompt.clone();
    out.prompt_file = g.prompt_file.clone();
    out.git_flow = g.git_flow.clone();
    out.workspace_mode = g.workspace_mode.clone();
    out.tracker.dependency_mode = g.dependency_mode.clone();
    out.tracker.dep_mode_prompt_file = g.dep_mode_prompt_file.clone();
    out.tracker.claim_mode = g.claim_mode.clone();
    // claim_ttl / claim_settle_delay are not in the typed DTO; the clone of base retains them from
    // disk so a Settings save never drops an explicitly-configured pool-mode timing (INF-477).

    out.projects = req
        .projects
        .iter()
        .flatten()
        .map(|pj| project_from_json(base, pj))
        .collect();
    out
}

/// Reconstruct one [`Project`] from its typed payload (Go `projectFromJSON`). The managed claude
/// knobs come from `overrides` (an absent key clears the override); other per-project claude fields
/// and the per-project hooks block are preserved from the matching base project (matched on ANY
/// shared slug, so renaming the primary slug still locates it), so an edit that touches only the
/// managed knobs never drops them.
fn project_from_json(base: &Config, pj: &ProjectReq) -> Project {
    let mut p = Project {
        name: pj.name.clone(),
        slugs: pj.slugs.clone(),
        repo: pj.repo.clone(),
        milestone: pj.milestone.clone(),
        active_states: pj.active_states.clone(),
        terminal_states: pj.terminal_states.clone(),
        canceled_states: pj.canceled_states.clone(),
        review_states: pj.review_states.clone(),
        max_concurrent_agents: pj.max_concurrent_agents,
        prompt: pj.prompt.clone(),
        prompt_file: pj.prompt_file.clone(),
        labels: pj.labels.clone(),
        capabilities: pj.capabilities.clone(),
        ..Project::default()
    };
    // git_flow / workspace_mode / dependency_mode / dep_mode_prompt_file / claim_mode are surfaced in
    // the overrides block but stored on the top-level Project; a nil pointer (absent) clears the
    // override (== inherit).
    if let Some(v) = &pj.overrides.git_flow {
        p.git_flow = v.clone();
    }
    if let Some(v) = &pj.overrides.workspace_mode {
        p.workspace_mode = v.clone();
    }
    if let Some(v) = &pj.overrides.dependency_mode {
        p.dependency_mode = v.clone();
    }
    if let Some(v) = &pj.overrides.dep_mode_prompt_file {
        p.dep_mode_prompt_file = v.clone();
    }
    if let Some(v) = &pj.overrides.claim_mode {
        p.claim_mode = v.clone();
    }
    // enabled:false pauses (stored explicitly); an omitted (None) or true flag leaves it unset
    // (default enabled), so a POST that omits enabled never pauses the agent.
    if pj.enabled == Some(false) {
        p.enabled = Some(false);
    }

    // Match the base project by ANY shared slug (not just the first) to carry forward the unexposed
    // per-project knobs across a rename/reorder of the primary slug.
    let base_project = pj.slugs.iter().find_map(|s| find_project_by_slug(base, s));
    if let Some(bp) = base_project
        && let Some(hooks) = &bp.hooks
    {
        p.hooks = Some(hooks.clone());
    }
    // Seed the override from the base project's claude block (preserving unmanaged knobs), then
    // overwrite ONLY the managed knobs from the DTO (a None clears that managed knob).
    let mut ov = base_project
        .and_then(|bp| bp.claude.clone())
        .unwrap_or_default();
    ov.model = pj.overrides.model.clone();
    ov.effort = pj.overrides.effort.clone();
    ov.permission_mode = pj.overrides.permission.clone();
    ov.ultracode = pj.overrides.ultracode;
    ov.turn_timeout_ms = pj.overrides.turn_timeout_ms;
    ov.stall_timeout_ms = pj.overrides.stall_timeout_ms;
    ov.billing_guard = pj.overrides.billing_guard;
    ov.command = pj.overrides.command.clone();
    if !is_empty_claude_override(&ov) {
        p.claude = Some(ov);
    }
    p
}

/// The base project owning `slug`, or `None`. Used to carry per-project knobs the typed view does not
/// expose across a POST. Mirrors Go `findProjectBySlug`.
fn find_project_by_slug<'a>(c: &'a Config, slug: &str) -> Option<&'a Project> {
    c.projects
        .iter()
        .find(|p| p.slugs.iter().any(|s| s == slug))
}

/// Whether every override field is unset, so an all-inherit project emits no claude block (and stays
/// collapsible to the legacy single-project form). Mirrors Go `isEmptyClaudeOverride`.
fn is_empty_claude_override(o: &ClaudeOverride) -> bool {
    o.command.is_none()
        && o.model.is_none()
        && o.effort.is_none()
        && o.permission_mode.is_none()
        && o.allowed_tools.is_none()
        && o.disallowed_tools.is_none()
        && o.mcp_config.is_none()
        && o.setting_sources.is_none()
        && o.add_dirs.is_empty()
        && o.turn_timeout_ms.is_none()
        && o.read_timeout_ms.is_none()
        && o.stall_timeout_ms.is_none()
        && o.extra_args.is_empty()
        && o.billing_guard.is_none()
        && o.ultracode.is_none()
}

/// Map a validation error to a stable code + best-effort field path so the Settings UI can attach the
/// message to the offending input (Go `classifyConfigError`). Only the structured
/// [`ValidationError`] variants Go classifies get a field path; every other failure (decode / resolve
/// / build-effective, and the three variants Go has no case for) falls back to the generic
/// `invalid_config` with no field path — exactly Go's `default` arm.
pub(crate) fn classify_config_error(err: &ConfigValidateError) -> (&'static str, Vec<FieldError>) {
    let ConfigValidateError::Validation(ve) = err else {
        return ("invalid_config", Vec::new());
    };
    let field = |path: &str| {
        vec![FieldError {
            path: path.to_string(),
            message: ve.to_string(),
        }]
    };
    match ve {
        ValidationError::InvalidReviewPromoteState(_) => (
            "invalid_review_promote_state",
            field("review_promote_state"),
        ),
        ValidationError::UnsupportedTrackerKind(_) => {
            ("unsupported_tracker_kind", field("tracker.kind"))
        }
        ValidationError::MissingTrackerApiKey => {
            ("missing_tracker_api_key", field("tracker.api_key"))
        }
        ValidationError::MissingTrackerProjectSlug => {
            ("missing_tracker_project_slug", field("projects"))
        }
        ValidationError::UnsupportedAgentBackend(_) => {
            ("unsupported_agent_backend", field("agent.backend"))
        }
        ValidationError::UnsupportedGitFlow(_) => ("unsupported_git_flow", field("git_flow")),
        ValidationError::UnsupportedWorkspaceMode(_) => {
            ("unsupported_workspace_mode", field("workspace_mode"))
        }
        ValidationError::UnsupportedDependencyMode(_) => {
            ("unsupported_dependency_mode", field("dependency_mode"))
        }
        ValidationError::UnsupportedClaimMode(_) => ("unsupported_claim_mode", field("claim_mode")),
        ValidationError::GraphiteRequiresReviewStates(_) => {
            ("graphite_requires_review_states", field("review_states"))
        }
        ValidationError::InvalidProjects(_) => ("invalid_projects", field("projects")),
        ValidationError::InvalidStorage(_) => ("invalid_storage", field("storage.retention_days")),
        ValidationError::InvalidAgent(_) => {
            ("invalid_agent", field("agent.handoff_drain_grace_ms"))
        }
        ValidationError::MissingAgentCommand(_) => {
            ("missing_agent_command", field("claude.command"))
        }
        // MissingTrackerSource / FileTrackerMultiProject / InvalidClaimTiming have no Go
        // classifyConfigError case → the generic code, exactly like Go's `default` arm.
        _ => ("invalid_config", Vec::new()),
    }
}
