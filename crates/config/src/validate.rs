//! `validate` — parity port of Go `internal/config.ValidateDispatch`: the scheduler preflight that
//! rejects an unusable config before the daemon dispatches any agent (upstream §6.3).
//!
//! Every rejection is a [`ValidationError`] whose [`Display`](std::fmt::Display) reproduces the Go
//! sentinel STRING byte-for-byte — these surface in the config API's 400 bodies and daemon logs, so
//! the token (`unsupported_tracker_kind`, `invalid_projects`, …) and the full wrapped message are an
//! observable contract, not an implementation detail.
//!
//! # Signature deviation from the plan sketch
//!
//! The C5 plan sketch reads `validate(r: &Resolved)`; the faithful port takes `&mut Resolved`. Go's
//! `ValidateDispatch(c *Config)` normalizes each project slug IN PLACE (`strings.TrimSpace`, stored
//! back on the shared `c.Projects[i].Slugs[j]`) so the downstream `ResolveProjects` matches Linear —
//! and `validate_test.go`'s `TestValidateProjectSlugTrimmedInPlace` asserts exactly that mutation.
//! A `&Resolved` could not reproduce it. The trim lives in `validate` (not `resolve_projects`, which
//! must never mutate its input), so callers run `validate(&mut cfg)?; resolve_projects(&cfg)`, byte-
//! identical to the Go daemon's order.

use std::collections::HashSet;

use chrono::Duration;
use rhapsody_core::normalize_state;

use crate::model::{
    CLAIM_MODE_ASSIGNEE, CLAIM_MODE_POOL, Config, DEPENDENCY_MODE_DAG, DEPENDENCY_MODE_DISABLED,
    DEPENDENCY_MODE_GRAPHITE, WORKSPACE_MODE_CLONE, WORKSPACE_MODE_WORKTREE,
};
use crate::projects::{EffectiveConfig, effective_for};
use crate::resolve::Resolved;

/// A dispatch-preflight rejection (Go `internal/config`'s `Err*` sentinels wrapped via
/// `fmt.Errorf`). Each variant's [`Display`](std::fmt::Display) matches the Go message byte-for-byte;
/// the debug-quoted fields (`{:?}`) mirror Go's `%q` for the ASCII config vocabulary (tracker kinds,
/// backends, modes, slugs, state names), which never contains characters Rust and Go quote
/// differently. Tests match on the variant (mirroring Go's `errors.Is`) and, for the messages the
/// API surfaces, assert the exact string.
#[derive(thiserror::Error, Debug)]
pub enum ValidationError {
    /// `tracker.kind` is neither `linear` nor `file` (Go `ErrUnsupportedTrackerKind`).
    #[error("unsupported_tracker_kind: {0:?}")]
    UnsupportedTrackerKind(String),
    /// `tracker.kind: linear` with no `tracker.api_key` (Go `ErrMissingTrackerAPIKey`).
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    /// Neither a `projects:` block nor the legacy `tracker.project_slug` supplies a slug (Go
    /// `ErrMissingTrackerProjectSlug`).
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    /// `tracker.kind: file` with no `tracker.source` path (Go `ErrMissingTrackerSource`, INF-303).
    #[error("missing_tracker_source")]
    MissingTrackerSource,
    /// `tracker.kind: file` alongside a `projects:` list — the single-project-only file tracker (Go
    /// `ErrFileTrackerMultiProject`, INF-303).
    #[error("file_tracker_multi_project")]
    FileTrackerMultiProject,
    /// `agent.backend` is neither `claude` nor `codex` (Go `ErrUnsupportedAgentBackend`).
    #[error("unsupported_agent_backend: {0:?}")]
    UnsupportedAgentBackend(String),
    /// The selected backend's command is empty; `{0}` is the backend name (Go
    /// `ErrMissingAgentCommand`).
    #[error("missing_agent_command: backend {0:?}")]
    MissingAgentCommand(String),
    /// An unknown `git_flow` value (global or per-project); `{0}` carries the shape-specific tail (Go
    /// `ErrUnsupportedGitFlow`, INF-251).
    #[error("unsupported_git_flow: {0}")]
    UnsupportedGitFlow(String),
    /// An unknown `workspace_mode` value (global or per-project) (Go `ErrUnsupportedWorkspaceMode`,
    /// INF-418).
    #[error("unsupported_workspace_mode: {0}")]
    UnsupportedWorkspaceMode(String),
    /// An unknown `dependency_mode` value (global or per-project) (Go
    /// `ErrUnsupportedDependencyMode`, INF-318).
    #[error("unsupported_dependency_mode: {0}")]
    UnsupportedDependencyMode(String),
    /// An effective `dependency_mode: graphite` with an empty effective `review_states` (Go
    /// `ErrGraphiteRequiresReviewStates`, INF-318).
    #[error("graphite_requires_review_states: {0}")]
    GraphiteRequiresReviewStates(String),
    /// An unknown `claim_mode` value (global or per-project) (Go `ErrUnsupportedClaimMode`,
    /// INF-477).
    #[error("unsupported_claim_mode: {0}")]
    UnsupportedClaimMode(String),
    /// A negative `claim_ttl` / `claim_settle_delay` (Go `ErrInvalidClaimTiming`, INF-477).
    #[error("invalid_claim_timing: {0}")]
    InvalidClaimTiming(String),
    /// A malformed `projects:` block — missing/blank/duplicate slug, missing repo, out-of-range cap
    /// (Go `ErrInvalidProjects`).
    #[error("invalid_projects: {0}")]
    InvalidProjects(String),
    /// `review_promote_state` is not one of the (effective) `active_states` where review-reopen is
    /// enabled (Go `ErrInvalidReviewPromoteState`).
    #[error("invalid_review_promote_state: {0}")]
    InvalidReviewPromoteState(String),
    /// `storage.retention_days` is negative (Go `ErrInvalidStorage`).
    #[error("invalid_storage: {0}")]
    InvalidStorage(String),
    /// `agent.handoff_drain_grace_ms` is negative (Go `ErrInvalidAgent`).
    #[error("invalid_agent: {0}")]
    InvalidAgent(String),
}

/// Runs the scheduler preflight checks on a resolved config (Go `ValidateDispatch`, upstream §6.3).
///
/// Takes `&mut` because it normalizes each `projects[].slugs[]` in place (trims surrounding
/// whitespace) so the downstream [`resolve_projects`](crate::projects::resolve_projects) matches
/// Linear — see the module docs. The check order mirrors Go exactly (it is observable: the first
/// failure wins).
pub fn validate(config: &mut Resolved) -> Result<(), ValidationError> {
    match config.tracker.kind.as_str() {
        "linear" => {
            if config.tracker.api_key.is_empty() {
                return Err(ValidationError::MissingTrackerApiKey);
            }
            // Either the multi-project block supplies slugs OR the legacy single-project
            // tracker.project_slug is set. Empty in both => the missing-slug error.
            if config.projects.is_empty() && config.tracker.project_slug.is_empty() {
                return Err(ValidationError::MissingTrackerProjectSlug);
            }
        }
        "file" => {
            // The file-backed stub tracker needs only the source path — no api_key/slug (INF-303).
            if config.tracker.source.is_empty() {
                return Err(ValidationError::MissingTrackerSource);
            }
            // Single-project only (v1): a `projects:` list would race write-backs over one file.
            if !config.projects.is_empty() {
                return Err(ValidationError::FileTrackerMultiProject);
            }
        }
        other => {
            return Err(ValidationError::UnsupportedTrackerKind(other.to_string()));
        }
    }
    if config.agent.backend != "claude" && config.agent.backend != "codex" {
        return Err(ValidationError::UnsupportedAgentBackend(
            config.agent.backend.clone(),
        ));
    }
    validate_git_flow(config)?;
    validate_workspace_mode(config)?;
    validate_dependency_mode(config)?;
    validate_review_states_for_graphite(config)?;
    validate_claim_mode(config)?;
    validate_projects(config)?;
    validate_review_promote(config)?;
    if let Some(days) = config.storage.retention_days
        && days < 0
    {
        return Err(ValidationError::InvalidStorage(
            "storage.retention_days must be >= 0 (0 = keep forever)".to_string(),
        ));
    }
    if config.agent.handoff_drain_grace_ms < 0 {
        return Err(ValidationError::InvalidAgent(
            "agent.handoff_drain_grace_ms must be >= 0 (0 = no drain)".to_string(),
        ));
    }
    let cmd = if config.agent.backend == "codex" {
        &config.codex.command
    } else {
        &config.claude.command
    };
    if cmd.is_empty() {
        return Err(ValidationError::MissingAgentCommand(
            config.agent.backend.clone(),
        ));
    }
    Ok(())
}

/// Reports whether `s` is an accepted `git_flow` value. Empty is accepted at the global level
/// (`== "any"`/no enforcement) and per-project (`== inherit`) (Go `gitFlowValid`).
fn git_flow_valid(s: &str) -> bool {
    s.is_empty() || s == "any" || s == "graphite"
}

/// Rejects an unknown `git_flow` on the global knob or any per-project override (Go
/// `validateGitFlow`, INF-251).
fn validate_git_flow(config: &Config) -> Result<(), ValidationError> {
    if !git_flow_valid(&config.git_flow) {
        return Err(ValidationError::UnsupportedGitFlow(format!(
            "{:?} (want \"any\" or \"graphite\")",
            config.git_flow
        )));
    }
    for (i, p) in config.projects.iter().enumerate() {
        if !git_flow_valid(&p.git_flow) {
            return Err(ValidationError::UnsupportedGitFlow(format!(
                "project {i}: {:?} (want \"any\" or \"graphite\")",
                p.git_flow
            )));
        }
    }
    Ok(())
}

/// Reports whether `s` is an accepted `workspace_mode` value (Go `workspaceModeValid`, INF-418).
fn workspace_mode_valid(s: &str) -> bool {
    s.is_empty() || s == WORKSPACE_MODE_WORKTREE || s == WORKSPACE_MODE_CLONE
}

/// Rejects an unknown `workspace_mode` on the global knob or any per-project override (Go
/// `validateWorkspaceMode`, INF-418).
fn validate_workspace_mode(config: &Config) -> Result<(), ValidationError> {
    if !workspace_mode_valid(&config.workspace_mode) {
        return Err(ValidationError::UnsupportedWorkspaceMode(format!(
            "{:?} (want {WORKSPACE_MODE_WORKTREE:?} or {WORKSPACE_MODE_CLONE:?})",
            config.workspace_mode
        )));
    }
    for (i, p) in config.projects.iter().enumerate() {
        if !workspace_mode_valid(&p.workspace_mode) {
            return Err(ValidationError::UnsupportedWorkspaceMode(format!(
                "project {i}: {:?} (want {WORKSPACE_MODE_WORKTREE:?} or {WORKSPACE_MODE_CLONE:?})",
                p.workspace_mode
            )));
        }
    }
    Ok(())
}

/// Reports whether `s` is an accepted `dependency_mode` value (Go `dependencyModeValid`, INF-318).
fn dependency_mode_valid(s: &str) -> bool {
    s.is_empty()
        || s == DEPENDENCY_MODE_DISABLED
        || s == DEPENDENCY_MODE_GRAPHITE
        || s == DEPENDENCY_MODE_DAG
}

/// Rejects an unknown `dependency_mode` on the global knob or any per-project override (Go
/// `validateDependencyMode`, INF-318).
fn validate_dependency_mode(config: &Config) -> Result<(), ValidationError> {
    if !dependency_mode_valid(&config.tracker.dependency_mode) {
        return Err(ValidationError::UnsupportedDependencyMode(format!(
            "{:?} (want \"disabled\", \"graphite\" or \"dag\")",
            config.tracker.dependency_mode
        )));
    }
    for (i, p) in config.projects.iter().enumerate() {
        if !dependency_mode_valid(&p.dependency_mode) {
            return Err(ValidationError::UnsupportedDependencyMode(format!(
                "project {i}: {:?} (want \"disabled\", \"graphite\" or \"dag\")",
                p.dependency_mode
            )));
        }
    }
    Ok(())
}

/// Fails when a scope whose EFFECTIVE `dependency_mode` is `graphite` has an empty effective
/// `review_states` (Go `graphite`-arm of `validateReviewStatesForGraphite`). `dag`/`disabled` are
/// exempt; `review_states` is never auto-defaulted.
fn graphite_ok(label: &str, eff: &EffectiveConfig) -> Result<(), ValidationError> {
    if eff.dependency_mode == DEPENDENCY_MODE_GRAPHITE && eff.review_states.is_empty() {
        return Err(ValidationError::GraphiteRequiresReviewStates(format!(
            "{label} has dependency_mode \"graphite\" but no review_states (graphite unblocks at review_states ∪ terminal_states)"
        )));
    }
    Ok(())
}

/// Enforces the graphite⊕review_states rule using the same presence-based resolution as the
/// orchestrator ([`effective_for`]), so inherited graphite + inherited review_states passes while
/// graphite with no review_states anywhere fails (Go `validateReviewStatesForGraphite`, INF-318).
fn validate_review_states_for_graphite(config: &Config) -> Result<(), ValidationError> {
    if config.projects.is_empty() {
        // Legacy/single-project: the top-level effective is what the daemon runs against.
        return graphite_ok("project", &effective_for(config, None));
    }
    for (i, p) in config.projects.iter().enumerate() {
        let label = if !p.name.is_empty() {
            format!("project {:?}", p.name)
        } else if !p.slugs.is_empty() {
            format!("project {:?}", p.slugs[0])
        } else {
            format!("project {i}")
        };
        graphite_ok(&label, &effective_for(config, Some(p)))?;
    }
    Ok(())
}

/// Reports whether `s` is an accepted `claim_mode` value (Go `claimModeValid`, INF-477).
fn claim_mode_valid(s: &str) -> bool {
    s.is_empty() || s == CLAIM_MODE_ASSIGNEE || s == CLAIM_MODE_POOL
}

/// Rejects an unknown `claim_mode` on the global knob or any per-project override, and a negative
/// `claim_ttl` / `claim_settle_delay` (Go `validateClaimMode`, INF-477).
fn validate_claim_mode(config: &Config) -> Result<(), ValidationError> {
    if !claim_mode_valid(&config.tracker.claim_mode) {
        return Err(ValidationError::UnsupportedClaimMode(format!(
            "{:?} (want \"assignee\" or \"pool\")",
            config.tracker.claim_mode
        )));
    }
    for (i, p) in config.projects.iter().enumerate() {
        if !claim_mode_valid(&p.claim_mode) {
            return Err(ValidationError::UnsupportedClaimMode(format!(
                "project {i}: {:?} (want \"assignee\" or \"pool\")",
                p.claim_mode
            )));
        }
    }
    if config.tracker.claim_ttl < Duration::zero() {
        return Err(ValidationError::InvalidClaimTiming(
            "claim_ttl must be >= 0 (0 = use default)".to_string(),
        ));
    }
    if config.tracker.claim_settle_delay < Duration::zero() {
        return Err(ValidationError::InvalidClaimTiming(
            "claim_settle_delay must be >= 0 (0 = use default)".to_string(),
        ));
    }
    Ok(())
}

/// Validates the optional multi-project block and normalizes each slug in place (Go
/// `validateProjects`). No-op in legacy single-project mode. Requires: each project has ≥1 slug;
/// every slug is non-empty and globally unique; a repo is present when neither the project nor the
/// top-level repo is set; a per-project cap (if set) is within `0..=global` (0 = no per-project cap).
fn validate_projects(config: &mut Config) -> Result<(), ValidationError> {
    if config.projects.is_empty() {
        return Ok(());
    }
    // Hoist the top-level reads so the per-project mutable borrow of `config.projects` below does not
    // conflict with borrowing `config.agent` / `config.repo`.
    let global_max = config.agent.max_concurrent_agents;
    let top_repo_empty = config.repo.is_empty();
    let mut seen: HashSet<String> = HashSet::with_capacity(config.projects.len());
    for (i, p) in config.projects.iter_mut().enumerate() {
        if p.slugs.is_empty() {
            return Err(ValidationError::InvalidProjects(format!(
                "project {i}: at least one slug required"
            )));
        }
        for slug in p.slugs.iter_mut() {
            let s = slug.trim().to_string();
            if s.is_empty() {
                return Err(ValidationError::InvalidProjects(format!(
                    "project {i}: slug must be non-empty"
                )));
            }
            if seen.contains(&s) {
                return Err(ValidationError::InvalidProjects(format!(
                    "duplicate project slug: {s:?}"
                )));
            }
            seen.insert(s.clone());
            *slug = s; // normalize stored value so resolve_projects matches Linear
        }
        if p.repo.is_empty() && top_repo_empty {
            return Err(ValidationError::InvalidProjects(format!(
                "project {i}: repo is empty and no top-level repo is set"
            )));
        }
        if let Some(n) = p.max_concurrent_agents
            && (n < 0 || n > global_max)
        {
            return Err(ValidationError::InvalidProjects(format!(
                "project {i}: max_concurrent_agents must be 0..{global_max}"
            )));
        }
    }
    Ok(())
}

/// Reports whether `promote` (already normalized) equals some `active` state under
/// [`normalize_state`] (Go `inActive`).
fn state_in(active: &[String], promote: &str) -> bool {
    active
        .iter()
        .any(|s| normalize_state(s).as_str() == promote)
}

/// Formats a `&[String]` the way Go's `fmt` `%v` renders a `[]string`: space-separated elements in
/// square brackets, no quotes, no commas (e.g. `[Todo In Progress]`, `[]`).
fn go_slice(v: &[String]) -> String {
    let mut s = String::from("[");
    for (i, e) in v.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(e);
    }
    s.push(']');
    s
}

/// Enforces that, wherever review-reopen is enabled (`review_states` non-empty), the configured
/// `review_promote_state` is one of the (effective) `active_states` — else reconcile would terminate
/// the promoted worker on the next tick (Go `validateReviewPromote`). Case-insensitive; a no-op when
/// the feature is off.
fn validate_review_promote(config: &Config) -> Result<(), ValidationError> {
    let promote = normalize_state(&config.tracker.review_promote_state);
    // Top-level (single-project / default) scope.
    if !config.tracker.review_states.is_empty()
        && !state_in(&config.tracker.active_states, &promote)
    {
        return Err(ValidationError::InvalidReviewPromoteState(format!(
            "review_promote_state {:?} must be one of active_states {}",
            config.tracker.review_promote_state,
            go_slice(&config.tracker.active_states)
        )));
    }
    // Per-project scope: a project with review enabled (own or inherited) must promote into its own
    // effective active_states (own override, else top-level).
    for (i, p) in config.projects.iter().enumerate() {
        let review = if p.review_states.is_empty() {
            &config.tracker.review_states
        } else {
            &p.review_states
        };
        if review.is_empty() {
            continue; // feature off for this project
        }
        let active = if p.active_states.is_empty() {
            &config.tracker.active_states
        } else {
            &p.active_states
        };
        if !state_in(active, &promote) {
            return Err(ValidationError::InvalidReviewPromoteState(format!(
                "project {i}: review_promote_state {:?} must be one of active_states {}",
                config.tracker.review_promote_state,
                go_slice(active)
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::model::Project;
    use crate::resolve::resolve;
    use crate::workflow::{Definition, YamlMap};

    /// Decode a YAML front-matter string + prompt body into a [`Config`] (mirrors Go `decode`).
    fn cfg_from(front: &str, body: &str) -> Config {
        let config: YamlMap = if front.trim().is_empty() {
            YamlMap::new()
        } else {
            serde_yaml_ng::from_str(front).expect("front matter must parse")
        };
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        decode(&def).expect("decode should succeed")
    }

    /// Decode + resolve (mirrors Go `decodeResolveStorage`), for the storage-retention cases that
    /// exercise the resolved config.
    fn decode_resolve(front: &str, body: &str) -> Config {
        let config: YamlMap = serde_yaml_ng::from_str(front).expect("front matter must parse");
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        resolve(decode(&def).expect("decode"), "/wf").expect("resolve")
    }

    /// A minimal valid single-project config (mirrors Go `valid()`): linear tracker with an api_key
    /// and slug; backend/commands come from decode defaults (`claude`, `codex app-server`).
    fn valid() -> Config {
        cfg_from(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\n",
            "",
        )
    }

    /// The file-tracker analogue of [`valid`] (mirrors Go `validFile()`): `kind: file` needs only a
    /// source path.
    fn valid_file() -> Config {
        cfg_from("tracker:\n  kind: file\n  source: /tmp/smoke.json\n", "")
    }

    /// The `base()` config the mode-validation tests build on (Go `base()` in the *_mode tests):
    /// linear tracker (api_key/slug/one active state), backend+command, a top-level repo, cap 1.
    fn base_mode() -> Config {
        cfg_from(
            concat!(
                "tracker:\n  kind: linear\n  api_key: k\n  project_slug: s\n  active_states:\n    - Todo\n",
                "agent:\n  backend: claude\n  max_concurrent_agents: 1\n",
                "claude:\n  command: claude\n",
                "repo: \"git@github.com:o/r.git\"\n",
            ),
            "",
        )
    }

    // ---- validate_test.go mirrors ----

    // Mirrors Go `TestValidateOK`.
    #[test]
    fn validate_ok() {
        assert!(validate(&mut valid()).is_ok());
    }

    // Mirrors Go `TestValidateUnsupportedKind`.
    #[test]
    fn unsupported_kind() {
        let mut c = valid();
        c.tracker.kind = "jira".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedTrackerKind(_)));
        assert_eq!(err.to_string(), "unsupported_tracker_kind: \"jira\"");
    }

    // Mirrors Go `TestValidateMissingAPIKey`.
    #[test]
    fn missing_api_key() {
        let mut c = valid();
        c.tracker.api_key = String::new();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::MissingTrackerApiKey));
        assert_eq!(err.to_string(), "missing_tracker_api_key");
    }

    // Mirrors Go `TestValidateMissingProjectSlug`.
    #[test]
    fn missing_project_slug() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::MissingTrackerProjectSlug));
        assert_eq!(err.to_string(), "missing_tracker_project_slug");
    }

    // Mirrors Go `TestValidateFileKindOK`.
    #[test]
    fn file_kind_ok() {
        assert!(validate(&mut valid_file()).is_ok());
    }

    // Mirrors Go `TestValidateFileKindMissingSource`.
    #[test]
    fn file_kind_missing_source() {
        let mut c = valid_file();
        c.tracker.source = String::new();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::MissingTrackerSource));
        assert_eq!(err.to_string(), "missing_tracker_source");
    }

    // Mirrors Go `TestValidateFileKindDoesNotRequireAPIKeyOrSlug`.
    #[test]
    fn file_kind_does_not_require_api_key_or_slug() {
        let mut c = valid_file();
        c.tracker.api_key = String::new();
        c.tracker.project_slug = String::new();
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateFileKindRejectsMultiProject`.
    #[test]
    fn file_kind_rejects_multi_project() {
        let mut c = valid_file();
        c.projects = vec![Project {
            repo: "git@github.com:o/r.git".to_string(),
            slugs: vec!["a".to_string()],
            ..Default::default()
        }];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::FileTrackerMultiProject));
        assert_eq!(err.to_string(), "file_tracker_multi_project");
    }

    // Mirrors Go `TestValidateNegativeHandoffDrainGrace`.
    #[test]
    fn negative_handoff_drain_grace() {
        let mut c = valid();
        c.agent.handoff_drain_grace_ms = -1;
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidAgent(_)));
        assert_eq!(
            err.to_string(),
            "invalid_agent: agent.handoff_drain_grace_ms must be >= 0 (0 = no drain)"
        );
    }

    // Mirrors Go `TestValidateZeroHandoffDrainGraceOK`.
    #[test]
    fn zero_handoff_drain_grace_ok() {
        let mut c = valid();
        c.agent.handoff_drain_grace_ms = 0; // drain disabled — valid
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateMissingClaudeCommand`.
    #[test]
    fn missing_claude_command() {
        let mut c = valid();
        c.claude.command = String::new();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::MissingAgentCommand(_)));
        assert_eq!(err.to_string(), "missing_agent_command: backend \"claude\"");
    }

    // Mirrors Go `TestValidateReviewPromoteStateNotActive`.
    #[test]
    fn review_promote_state_not_active() {
        let mut c = valid();
        c.tracker.active_states = vec!["Todo".to_string(), "In Progress".to_string()];
        c.tracker.review_states = vec!["In Review".to_string()];
        c.tracker.review_promote_state = "In Review".to_string(); // NOT active → would wedge
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidReviewPromoteState(_)));
        assert_eq!(
            err.to_string(),
            "invalid_review_promote_state: review_promote_state \"In Review\" must be one of active_states [Todo In Progress]"
        );
    }

    // Mirrors Go `TestValidateReviewPromoteStateActiveCaseInsensitive`.
    #[test]
    fn review_promote_state_active_case_insensitive() {
        let mut c = valid();
        c.tracker.active_states = vec!["Todo".to_string(), "In Progress".to_string()];
        c.tracker.review_states = vec!["In Review".to_string()];
        c.tracker.review_promote_state = "in progress".to_string(); // case-insensitive match
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateReviewPromoteIgnoredWhenFeatureOff`.
    #[test]
    fn review_promote_ignored_when_feature_off() {
        let mut c = valid();
        c.tracker.active_states = vec!["Todo".to_string()];
        c.tracker.review_states = Vec::new(); // feature OFF
        c.tracker.review_promote_state = "Nonexistent".to_string(); // not active, but not validated
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateMissingCodexCommandWhenCodexBackend`.
    #[test]
    fn missing_codex_command_when_codex_backend() {
        let mut c = valid();
        c.agent.backend = "codex".to_string();
        c.codex.command = String::new();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::MissingAgentCommand(_)));
        assert_eq!(err.to_string(), "missing_agent_command: backend \"codex\"");
    }

    // Mirrors Go `TestValidateUnsupportedAgentBackend`.
    #[test]
    fn unsupported_agent_backend() {
        let mut c = valid();
        c.agent.backend = "openai".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedAgentBackend(_)));
        assert_eq!(err.to_string(), "unsupported_agent_backend: \"openai\"");
    }

    // Mirrors Go `TestValidateSupportedAgentBackends`.
    #[test]
    fn supported_agent_backends() {
        for backend in ["claude", "codex"] {
            let mut c = valid();
            c.agent.backend = backend.to_string();
            assert!(validate(&mut c).is_ok(), "backend {backend} should be ok");
        }
    }

    // Mirrors Go `TestValidateMultiProjectOK`.
    #[test]
    fn multi_project_ok() {
        let mut c = valid();
        c.tracker.project_slug = String::new(); // projects supply slugs
        c.agent.max_concurrent_agents = 5;
        c.projects = vec![Project {
            repo: "git@github.com:o/r.git".to_string(),
            slugs: vec!["a".to_string(), "b".to_string()],
            ..Default::default()
        }];
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateMissingSlugWhenNoProjects`.
    #[test]
    fn missing_slug_when_no_projects() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        assert!(matches!(
            validate(&mut c),
            Err(ValidationError::MissingTrackerProjectSlug)
        ));
    }

    // Mirrors Go `TestValidateDuplicateSlug`.
    #[test]
    fn duplicate_slug() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.projects = vec![
            Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                ..Default::default()
            },
            Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                ..Default::default()
            },
        ];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidProjects(_)));
        assert_eq!(
            err.to_string(),
            "invalid_projects: duplicate project slug: \"a\""
        );
    }

    // Mirrors Go `TestValidateProjectMissingSlug`.
    #[test]
    fn project_missing_slug() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.projects = vec![Project {
            repo: "r".to_string(),
            slugs: Vec::new(),
            ..Default::default()
        }];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidProjects(_)));
        assert_eq!(
            err.to_string(),
            "invalid_projects: project 0: at least one slug required"
        );
    }

    // Mirrors Go `TestValidateProjectEmptySlug`.
    #[test]
    fn project_empty_slug() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.projects = vec![Project {
            repo: "r".to_string(),
            slugs: vec!["  ".to_string()],
            ..Default::default()
        }];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidProjects(_)));
        assert_eq!(
            err.to_string(),
            "invalid_projects: project 0: slug must be non-empty"
        );
    }

    // Mirrors Go `TestValidateProjectSlugTrimmedInPlace`.
    #[test]
    fn project_slug_trimmed_in_place() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.projects = vec![Project {
            repo: "r".to_string(),
            slugs: vec![" alpha ".to_string()],
            ..Default::default()
        }];
        assert!(validate(&mut c).is_ok(), "padded slug should validate");
        // Validation normalizes the stored slug in place so resolve_projects later matches Linear.
        assert_eq!(
            c.projects[0].slugs[0], "alpha",
            "slug not normalized in place"
        );
    }

    // Mirrors Go `TestValidateProjectMissingRepo`.
    #[test]
    fn project_missing_repo() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.repo = String::new();
        c.projects = vec![Project {
            repo: String::new(),
            slugs: vec!["a".to_string()],
            ..Default::default()
        }];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidProjects(_)));
        assert_eq!(
            err.to_string(),
            "invalid_projects: project 0: repo is empty and no top-level repo is set"
        );
    }

    // Mirrors Go `TestValidateProjectRepoInheritsTopLevel`.
    #[test]
    fn project_repo_inherits_top_level() {
        let mut c = valid();
        c.tracker.project_slug = String::new();
        c.repo = "git@github.com:o/r.git".to_string();
        c.projects = vec![Project {
            repo: String::new(),
            slugs: vec!["a".to_string()],
            ..Default::default()
        }];
        assert!(validate(&mut c).is_ok());
    }

    // Mirrors Go `TestValidateProjectCapRange`.
    #[test]
    fn project_cap_range() {
        let mk = |cap: i64| -> Config {
            let mut c = valid();
            c.tracker.project_slug = String::new();
            c.agent.max_concurrent_agents = 5;
            c.projects = vec![Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                max_concurrent_agents: Some(cap),
                ..Default::default()
            }];
            c
        };
        assert!(validate(&mut mk(0)).is_ok(), "cap 0 (no per-project cap)");
        let err = validate(&mut mk(-1)).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidProjects(_)));
        assert_eq!(
            err.to_string(),
            "invalid_projects: project 0: max_concurrent_agents must be 0..5"
        );
        assert!(matches!(
            validate(&mut mk(6)),
            Err(ValidationError::InvalidProjects(_))
        ));
        assert!(validate(&mut mk(5)).is_ok(), "cap == global");
        assert!(validate(&mut mk(1)).is_ok(), "cap 1");
    }

    // ---- gitflow_test.go / workspacemode_test.go / dependency_mode_test.go / claim_mode_test.go
    //      validate halves ----

    // Mirrors Go `TestValidateGitFlow`.
    #[test]
    fn validate_git_flow_values() {
        for v in ["", "any", "graphite"] {
            let mut c = base_mode();
            c.git_flow = v.to_string();
            assert!(validate(&mut c).is_ok(), "git_flow={v} should be valid");
        }
        let mut c = base_mode();
        c.git_flow = "gerrit".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedGitFlow(_)));
        assert_eq!(
            err.to_string(),
            "unsupported_git_flow: \"gerrit\" (want \"any\" or \"graphite\")"
        );
        let mut cp = base_mode();
        cp.tracker.project_slug = String::new();
        cp.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            git_flow: "nope".to_string(),
            ..Default::default()
        }];
        let perr = validate(&mut cp).unwrap_err();
        assert!(matches!(perr, ValidationError::UnsupportedGitFlow(_)));
        assert_eq!(
            perr.to_string(),
            "unsupported_git_flow: project 0: \"nope\" (want \"any\" or \"graphite\")"
        );
        let mut ok = base_mode();
        ok.tracker.project_slug = String::new();
        ok.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            git_flow: "graphite".to_string(),
            ..Default::default()
        }];
        assert!(validate(&mut ok).is_ok());
    }

    // Mirrors Go `TestValidateWorkspaceMode`.
    #[test]
    fn validate_workspace_mode_values() {
        for v in ["", "worktree", "clone"] {
            let mut c = base_mode();
            c.workspace_mode = v.to_string();
            assert!(
                validate(&mut c).is_ok(),
                "workspace_mode={v} should be valid"
            );
        }
        let mut c = base_mode();
        c.workspace_mode = "submodule".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedWorkspaceMode(_)));
        assert_eq!(
            err.to_string(),
            "unsupported_workspace_mode: \"submodule\" (want \"worktree\" or \"clone\")"
        );
        let mut cp = base_mode();
        cp.tracker.project_slug = String::new();
        cp.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            workspace_mode: "nope".to_string(),
            ..Default::default()
        }];
        assert!(matches!(
            validate(&mut cp),
            Err(ValidationError::UnsupportedWorkspaceMode(_))
        ));
        let mut ok = base_mode();
        ok.tracker.project_slug = String::new();
        ok.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            workspace_mode: "clone".to_string(),
            ..Default::default()
        }];
        assert!(validate(&mut ok).is_ok());
    }

    // Mirrors Go `TestValidateDependencyMode`.
    #[test]
    fn validate_dependency_mode_values() {
        for v in ["", "disabled", "graphite", "dag"] {
            let mut c = base_mode();
            c.tracker.dependency_mode = v.to_string();
            if v == "graphite" {
                c.tracker.review_states = vec!["In Review".to_string()]; // graphite needs review_states
                c.tracker.review_promote_state = "Todo".to_string(); // valid promote state
            }
            assert!(
                validate(&mut c).is_ok(),
                "dependency_mode={v} should be valid"
            );
        }
        let mut c = base_mode();
        c.tracker.dependency_mode = "nope".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedDependencyMode(_)));
        assert_eq!(
            err.to_string(),
            "unsupported_dependency_mode: \"nope\" (want \"disabled\", \"graphite\" or \"dag\")"
        );
        let mut cp = base_mode();
        cp.tracker.project_slug = String::new();
        cp.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            dependency_mode: "bogus".to_string(),
            ..Default::default()
        }];
        assert!(matches!(
            validate(&mut cp),
            Err(ValidationError::UnsupportedDependencyMode(_))
        ));
        // dep_mode_prompt_file is free-form; any string passes.
        let mut ok = base_mode();
        ok.tracker.dep_mode_prompt_file = "/anything/goes.md".to_string();
        assert!(validate(&mut ok).is_ok());
    }

    // Mirrors Go `TestValidateGraphiteRequiresReviewStates`.
    #[test]
    fn validate_graphite_requires_review_states() {
        // graphite + empty review_states → error naming the project.
        let mut c = base_mode();
        c.tracker.project_slug = String::new();
        c.projects = vec![Project {
            name: "alpha".to_string(),
            slugs: vec!["a-1".to_string()],
            dependency_mode: DEPENDENCY_MODE_GRAPHITE.to_string(),
            ..Default::default()
        }];
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::GraphiteRequiresReviewStates(_)
        ));
        assert_eq!(
            err.to_string(),
            "graphite_requires_review_states: project \"alpha\" has dependency_mode \"graphite\" but no review_states (graphite unblocks at review_states ∪ terminal_states)"
        );

        // graphite + review_states set → ok.
        let mut ok = base_mode();
        ok.tracker.project_slug = String::new();
        ok.tracker.review_promote_state = "Todo".to_string();
        ok.projects = vec![Project {
            name: "alpha".to_string(),
            slugs: vec!["a-1".to_string()],
            dependency_mode: DEPENDENCY_MODE_GRAPHITE.to_string(),
            review_states: vec!["In Review".to_string()],
            ..Default::default()
        }];
        assert!(validate(&mut ok).is_ok());

        // graphite inherited from global, review_states inherited from global → ok.
        let mut inh = base_mode();
        inh.tracker.project_slug = String::new();
        inh.tracker.dependency_mode = DEPENDENCY_MODE_GRAPHITE.to_string();
        inh.tracker.review_states = vec!["In Review".to_string()];
        inh.tracker.review_promote_state = "Todo".to_string();
        inh.projects = vec![Project {
            name: "alpha".to_string(),
            slugs: vec!["a-1".to_string()],
            ..Default::default()
        }];
        assert!(validate(&mut inh).is_ok());

        // dag + empty review_states → ok.
        let mut dag = base_mode();
        dag.tracker.project_slug = String::new();
        dag.projects = vec![Project {
            name: "alpha".to_string(),
            slugs: vec!["a-1".to_string()],
            dependency_mode: DEPENDENCY_MODE_DAG.to_string(),
            ..Default::default()
        }];
        assert!(validate(&mut dag).is_ok());

        // disabled + empty review_states → ok (legacy single-project).
        let mut dis = base_mode();
        assert!(validate(&mut dis).is_ok());

        // Decode must NOT auto-populate review_states for a non-graphite project.
        let dc = cfg_from(
            concat!(
                "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n",
                "repo: \"git@github.com:o/r.git\"\n",
                "projects:\n  - slugs:\n      - a-1\n",
            ),
            "body",
        );
        assert!(
            dc.tracker.review_states.is_empty(),
            "review_states must not be auto-defaulted"
        );
    }

    // Mirrors Go `TestValidateClaimMode`.
    #[test]
    fn validate_claim_mode_values() {
        for v in ["", "assignee", "pool"] {
            let mut c = base_mode();
            c.tracker.claim_mode = v.to_string();
            assert!(validate(&mut c).is_ok(), "claim_mode={v} should be valid");
        }
        let mut c = base_mode();
        c.tracker.claim_mode = "nope".to_string();
        let err = validate(&mut c).unwrap_err();
        assert!(matches!(err, ValidationError::UnsupportedClaimMode(_)));
        assert_eq!(
            err.to_string(),
            "unsupported_claim_mode: \"nope\" (want \"assignee\" or \"pool\")"
        );
        let mut cp = base_mode();
        cp.tracker.project_slug = String::new();
        cp.projects = vec![Project {
            slugs: vec!["a-1".to_string()],
            claim_mode: "bogus".to_string(),
            ..Default::default()
        }];
        assert!(matches!(
            validate(&mut cp),
            Err(ValidationError::UnsupportedClaimMode(_))
        ));
        let mut neg = base_mode();
        neg.tracker.claim_ttl = Duration::seconds(-1);
        let nerr = validate(&mut neg).unwrap_err();
        assert!(matches!(nerr, ValidationError::InvalidClaimTiming(_)));
        assert_eq!(
            nerr.to_string(),
            "invalid_claim_timing: claim_ttl must be >= 0 (0 = use default)"
        );
    }

    // ---- storage_test.go validate halves ----

    // Mirrors Go `TestStorageRetentionZeroKept`.
    #[test]
    fn storage_retention_zero_kept() {
        let mut cfg = decode_resolve(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nstorage:\n  retention_days: 0\n",
            "body",
        );
        assert_eq!(
            cfg.storage.retention_days,
            Some(0),
            "explicit retention_days 0 must be kept"
        );
        assert!(
            validate(&mut cfg).is_ok(),
            "retention_days 0 must validate OK"
        );
    }

    // Mirrors Go `TestStorageRetentionNegativeInvalid`.
    #[test]
    fn storage_retention_negative_invalid() {
        let mut cfg = decode_resolve(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nstorage:\n  retention_days: -1\n",
            "body",
        );
        let err = validate(&mut cfg).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidStorage(_)));
        assert_eq!(
            err.to_string(),
            "invalid_storage: storage.retention_days must be >= 0 (0 = keep forever)"
        );
    }
}
