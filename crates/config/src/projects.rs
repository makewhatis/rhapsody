//! `projects` — parity port of Go `internal/config/projects.go`: the per-project override overlay
//! (`EffectiveConfig`/`effectiveOf`/`EffectiveFor`) and the routing-target fan-out
//! (`ResolvedProject`/`ResolveProjects`).
//!
//! # What this stage does
//!
//! [`effective_for`] (Go `EffectiveFor`, a thin wrapper over the file-local `effectiveOf`) layers a
//! project's *set* overrides on top of the top-level defaults — presence-based: a non-empty value
//! wins, an empty one inherits — and materializes the mode defaults (`dependency_mode` → `disabled`,
//! `workspace_mode` → `worktree`, `claim_mode` → `assignee`, `dep_mode_prompt_file` → the canonical
//! path) LAST, so they apply to both the per-project and the `p == None` (legacy/top-level) paths.
//! Those mode defaults live HERE, not in [`crate::resolve`] — `resolve.go` never touches them (see
//! the resolve module docs).
//!
//! [`resolve_projects`] (Go `ResolveProjects`) fans each configured project's slugs out into one
//! [`ResolvedProject`] per slug (sharing repo/name/enabled/group/effective), or synthesizes a single
//! project for `tracker.project_slug` in the legacy form. It never mutates its input.
//!
//! # Signature deviations from the plan sketch
//!
//! The C5 plan sketch reads `resolve_projects(r: &Resolved) -> Vec<Project>`; the faithful port
//! returns `Vec<`[`ResolvedProject`]`>` — Go's `ResolveProjects` returns `[]ResolvedProject`, a
//! distinct type carrying the fanned slug, group key, resolved name/enabled, repo, and the
//! [`EffectiveConfig`] overlay. Returning the raw [`Project`] would drop every one of those; the
//! projects tests assert on `rp.Slug`/`rp.Group`/`rp.Name`/`rp.Enabled`/`rp.Repo`/`rp.Eff.*`, so
//! `ResolvedProject` is the contract. Go's `ResolveProjects(nil)` nil-guard (return an empty slice)
//! has no analogue: a Rust `&Resolved` is never null.

use crate::model::{
    CLAIM_MODE_ASSIGNEE, Claude, ClaudeOverride, Config, DEFAULT_DEP_MODE_PROMPT_FILE,
    DEPENDENCY_MODE_DISABLED, Hooks, Project, WORKSPACE_MODE_WORKTREE,
};
use crate::resolve::Resolved;

/// The per-project overlay of the OVERRIDABLE knobs only (Go `EffectiveConfig`).
///
/// Shared knobs (tracker auth/endpoint/kind, polling, `workspace.root`, server, otel, logging,
/// `agent.max_turns`/`max_retry_backoff_ms`/`max_concurrent_agents_by_state`) are NOT here;
/// consumers read those from the top-level [`Config`] directly.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    /// Non-empty after resolve.
    pub active_states: Vec<String>,
    /// Non-empty after resolve.
    pub terminal_states: Vec<String>,
    /// Cancel-type terminal subset (INF-272); non-empty after resolve.
    pub canceled_states: Vec<String>,
    /// May be empty (the review-summon feature is OFF for this scope).
    pub review_states: Vec<String>,
    pub prompt: String,
    /// When non-empty, WINS over `prompt` at run time (read per-run from disk).
    pub prompt_file: String,
    /// Full value copy of the effective Claude knobs.
    pub claude: Claude,
    pub hooks: Hooks,
    /// `0` ⇒ no per-project cap (only the global ceiling applies).
    pub max_concurrent_agents: i64,
    /// `""` ⇒ no milestone filter for this project.
    pub milestone: String,
    /// Empty ⇒ no label filter for this project.
    pub labels: Vec<String>,
    /// `""` ⇒ `"any"` (no enforcement); `"graphite"` ⇒ inject the guard hook.
    pub git_flow: String,
    /// Resolved workspace-provisioning policy: always non-empty after resolve (`"worktree"` default).
    pub workspace_mode: String,
    /// Resolved DAG-orchestration policy: always non-empty after resolve (`"disabled"` default).
    pub dependency_mode: String,
    /// Resolved mode-on prompt path (default [`DEFAULT_DEP_MODE_PROMPT_FILE`]).
    pub dep_mode_prompt_file: String,
    /// Resolved ticket-claim policy: always non-empty after resolve (`"assignee"` default).
    pub claim_mode: String,
}

/// One routing target: a single Linear slug → repo + effective config (Go `ResolvedProject`). A
/// multi-slug [`Project`] fans out to one `ResolvedProject` per slug (all sharing repo/name/enabled/
/// group/eff).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProject {
    pub slug: String,
    /// Display label (defaults to the first slug when unset). Shared by every slug fanned out from
    /// the same [`Project`].
    pub name: String,
    /// Resolved pause flag: `false` means the project is paused and must be skipped by the polling
    /// loop. Unset in config defaults to `true`.
    pub enabled: bool,
    /// A stable per-project key shared by every slug fanned out from the same [`Project`], so the
    /// per-project concurrency cap (`eff.max_concurrent_agents`) is enforced across the whole group,
    /// not per slug. For the legacy/synthetic single-project path `group == slug ==
    /// tracker.project_slug`.
    pub group: String,
    /// `""` ⇒ legacy hook-populated workspace.
    pub repo: String,
    pub eff: EffectiveConfig,
}

/// Returns the resolved-project set for a fully-defaulted [`Resolved`] config (call AFTER `decode` +
/// `resolve`, Go `ResolveProjects`). Never mutates `config`; always returns a (possibly empty)
/// vector. With `projects:` set it fans each project's slugs out in declaration order; otherwise it
/// synthesizes one project for `tracker.project_slug`.
pub fn resolve_projects(config: &Resolved) -> Vec<ResolvedProject> {
    let mut out: Vec<ResolvedProject> = Vec::new();
    if !config.projects.is_empty() {
        for p in &config.projects {
            if p.slugs.is_empty() {
                continue;
            }
            let repo = if p.repo.is_empty() {
                config.repo.clone()
            } else {
                p.repo.clone()
            };
            let eff = effective_of(config, Some(p));
            // Stable per-project group key shared by every slug in this project, so the per-project
            // concurrency cap is counted across the whole group (not per slug).
            let group = p.slugs[0].clone();
            // Name defaults to the first slug when unset; enabled defaults to true when the key is
            // absent (None). Both are per-project and shared by every fanned slug.
            let name = if p.name.is_empty() {
                p.slugs[0].clone()
            } else {
                p.name.clone()
            };
            let enabled = p.enabled.unwrap_or(true);
            for slug in &p.slugs {
                out.push(ResolvedProject {
                    slug: slug.clone(),
                    name: name.clone(),
                    enabled,
                    group: group.clone(),
                    repo: repo.clone(),
                    eff: eff.clone(),
                });
            }
        }
        return out;
    }
    // Legacy / single-project: one synthetic project for tracker.project_slug. It is always enabled
    // (the legacy tracker.project_slug form has no pause flag; pausing is only expressible via the
    // `projects:` form).
    out.push(ResolvedProject {
        slug: config.tracker.project_slug.clone(),
        name: config.tracker.project_slug.clone(),
        enabled: true,
        group: config.tracker.project_slug.clone(),
        repo: config.repo.clone(),
        eff: effective_of(config, None),
    });
    out
}

/// Returns the resolved overridable knobs for project `project` layered on `config`'s top-level
/// defaults (Go `EffectiveFor`). `None` yields the pure top-level effective config (used for the
/// synthesized single-project case, so its effective equals the legacy behavior). Exported so the
/// config API can render each agent's effective (inherited ∪ override) values without
/// re-implementing the merge (INF-224).
pub fn effective_for(config: &Config, project: Option<&Project>) -> EffectiveConfig {
    effective_of(config, project)
}

/// Overlays a project's set overrides on top of the top-level defaults (Go `effectiveOf`). `None`
/// yields the pure top-level effective config.
fn effective_of(config: &Config, project: Option<&Project>) -> EffectiveConfig {
    let mut eff = EffectiveConfig {
        active_states: config.tracker.active_states.clone(),
        terminal_states: config.tracker.terminal_states.clone(),
        canceled_states: config.tracker.canceled_states.clone(),
        review_states: config.tracker.review_states.clone(),
        prompt: config.prompt_template.clone(),
        prompt_file: config.prompt_file.clone(),
        // Value copy of the top-level Claude.
        claude: config.claude.clone(),
        hooks: config.hooks.clone(),
        // NOT seeded from the global agent cap: 0 => no per-project cap unless a project overrides.
        max_concurrent_agents: 0,
        milestone: config.tracker.milestone.clone(),
        labels: config.tracker.labels.clone(),
        git_flow: config.git_flow.clone(),
        // workspace_mode / dependency_mode / dep_mode_prompt_file / claim_mode seed from the global
        // value; a per-project override (below) wins, and the defaults are materialized last.
        workspace_mode: config.workspace_mode.clone(),
        dependency_mode: config.tracker.dependency_mode.clone(),
        dep_mode_prompt_file: config.tracker.dep_mode_prompt_file.clone(),
        claim_mode: config.tracker.claim_mode.clone(),
    };
    if let Some(p) = project {
        apply_project_overrides(&mut eff, config, p);
    }
    // Materialize the dependency_mode default LAST so it applies to both the p==None (legacy/
    // top-level) and per-project paths. The default is the literal "disabled" — NO git_flow coupling
    // (git_flow must never influence this). dep_mode_prompt_file defaults to the canonical path.
    if eff.dependency_mode.is_empty() {
        eff.dependency_mode = DEPENDENCY_MODE_DISABLED.to_string();
    }
    if eff.dep_mode_prompt_file.is_empty() {
        eff.dep_mode_prompt_file = DEFAULT_DEP_MODE_PROMPT_FILE.to_string();
    }
    // Materialize the workspace_mode default LAST (same as dependency_mode) so an empty global + empty
    // override resolves to "worktree" — byte-identical to today's behavior. NO dependency_mode
    // coupling (the clone-for-stacking nudge is a UI recommendation, not a resolver default) (INF-418).
    if eff.workspace_mode.is_empty() {
        eff.workspace_mode = WORKSPACE_MODE_WORKTREE.to_string();
    }
    // Materialize the claim_mode default LAST so an empty global + empty override resolves to
    // "assignee" — byte-identical to today's assignee-locked behavior (INF-477).
    if eff.claim_mode.is_empty() {
        eff.claim_mode = CLAIM_MODE_ASSIGNEE.to_string();
    }
    eff
}

/// Overlays a project's set overrides onto `eff` (Go `applyProjectOverrides`; presence-based: a
/// non-empty value wins, empty inherits). Split out of [`effective_of`] so the
/// default-materialization tail runs for both the `p == None` and per-project paths.
fn apply_project_overrides(eff: &mut EffectiveConfig, config: &Config, p: &Project) {
    if !p.active_states.is_empty() {
        eff.active_states = p.active_states.clone();
    }
    if !p.terminal_states.is_empty() {
        eff.terminal_states = p.terminal_states.clone();
    }
    if !p.canceled_states.is_empty() {
        eff.canceled_states = p.canceled_states.clone();
    }
    if !p.review_states.is_empty() {
        eff.review_states = p.review_states.clone();
    }
    if !p.prompt.is_empty() {
        eff.prompt = p.prompt.clone();
    }
    if !p.prompt_file.is_empty() {
        eff.prompt_file = p.prompt_file.clone();
    }
    if let Some(h) = &p.hooks {
        eff.hooks = h.clone();
    }
    if let Some(n) = p.max_concurrent_agents {
        eff.max_concurrent_agents = n;
    }
    if !p.milestone.is_empty() {
        eff.milestone = p.milestone.clone();
    }
    if !p.labels.is_empty() {
        eff.labels = p.labels.clone();
    }
    if !p.git_flow.is_empty() {
        eff.git_flow = p.git_flow.clone();
    }
    if !p.workspace_mode.is_empty() {
        eff.workspace_mode = p.workspace_mode.clone();
    }
    if !p.dependency_mode.is_empty() {
        eff.dependency_mode = p.dependency_mode.clone();
    }
    if !p.dep_mode_prompt_file.is_empty() {
        eff.dep_mode_prompt_file = p.dep_mode_prompt_file.clone();
    }
    if !p.claim_mode.is_empty() {
        eff.claim_mode = p.claim_mode.clone();
    }
    if let Some(ov) = &p.claude {
        eff.claude = apply_claude_override(config.claude.clone(), ov);
    }
}

/// Returns `base` with each non-empty field of `ov` applied (Go `applyClaudeOverride`). `None`
/// pointer fields / empty slice fields leave the base value untouched (inherit); a non-empty slice
/// REPLACES. The empty-vs-unset distinction for `add_dirs`/`extra_args` collapses to
/// `is_empty()` (the raw model already decodes them as `Vec`, not `Option`, per C3).
fn apply_claude_override(mut base: Claude, ov: &ClaudeOverride) -> Claude {
    if let Some(v) = &ov.command {
        base.command = v.clone();
    }
    if let Some(v) = &ov.model {
        base.model = v.clone();
    }
    if let Some(v) = &ov.effort {
        base.effort = v.clone();
    }
    if let Some(v) = &ov.permission_mode {
        base.permission_mode = v.clone();
    }
    if let Some(v) = &ov.allowed_tools {
        base.allowed_tools = v.clone();
    }
    if let Some(v) = &ov.disallowed_tools {
        base.disallowed_tools = v.clone();
    }
    if let Some(v) = &ov.mcp_config {
        base.mcp_config = v.clone();
    }
    if let Some(v) = &ov.setting_sources {
        base.setting_sources = v.clone();
    }
    if !ov.add_dirs.is_empty() {
        base.add_dirs = ov.add_dirs.clone();
    }
    if let Some(v) = ov.turn_timeout_ms {
        base.turn_timeout_ms = v;
    }
    if let Some(v) = ov.read_timeout_ms {
        base.read_timeout_ms = v;
    }
    if let Some(v) = ov.stall_timeout_ms {
        base.stall_timeout_ms = v;
    }
    if !ov.extra_args.is_empty() {
        base.extra_args = ov.extra_args.clone();
    }
    if ov.billing_guard.is_some() {
        base.billing_guard = ov.billing_guard;
    }
    if let Some(v) = ov.ultracode {
        base.ultracode = v;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::model::{
        CLAIM_MODE_POOL, DEPENDENCY_MODE_DAG, DEPENDENCY_MODE_GRAPHITE, WORKSPACE_MODE_CLONE,
    };
    use crate::workflow::{Definition, YamlMap};

    /// Decode a YAML front-matter string + prompt body into a [`Config`], mirroring the Go
    /// `decode`/`decodeMap` test helpers (which pass an equivalent `map[string]any`).
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

    /// A fully-defaulted single-project [`Config`] (as decode+resolve would produce), mirroring Go
    /// `baseCfg`. Each test overrides only the fields it exercises.
    fn base_cfg() -> Config {
        cfg_from(
            concat!(
                "tracker:\n",
                "  kind: linear\n",
                "  api_key: tok\n",
                "  project_slug: proj\n",
                "  active_states:\n    - Todo\n    - In Progress\n",
                "  terminal_states:\n    - Done\n    - Canceled\n",
                "  canceled_states:\n    - Canceled\n",
                "agent:\n  backend: claude\n  max_concurrent_agents: 10\n",
                "claude:\n",
                "  command: claude\n",
                "  model: opus\n",
                "  effort: high\n",
                "  allowed_tools: Read,Edit\n",
                "  extra_args:\n    - --foo\n",
                "  billing_guard: true\n",
                "hooks:\n  after_create: echo top\n",
            ),
            "top prompt",
        )
    }

    // ---- projects_test.go mirrors ----

    // Mirrors Go `TestEffectiveMilestoneOverride`.
    #[test]
    fn effective_milestone_override() {
        let mut c = cfg_from(
            "tracker:\n  project_slug: top\n  milestone: top-ms\n  active_states:\n    - Todo\n",
            "",
        );
        c.projects = vec![
            Project {
                slugs: vec!["a".to_string()],
                milestone: "proj-ms".to_string(),
                ..Default::default()
            },
            Project {
                slugs: vec!["b".to_string()],
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        let mut got = std::collections::HashMap::new();
        for rp in &rps {
            got.insert(rp.slug.clone(), rp.eff.milestone.clone());
        }
        assert_eq!(
            got.get("a").map(String::as_str),
            Some("proj-ms"),
            "override"
        );
        assert_eq!(
            got.get("b").map(String::as_str),
            Some("top-ms"),
            "inherited"
        );
    }

    // Mirrors Go `TestResolveProjectsSingleProjectSynthesized`.
    #[test]
    fn single_project_synthesized() {
        let c = base_cfg();
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 1, "expected 1 synthesized project");
        let rp = &rps[0];
        assert_eq!(rp.slug, "proj");
        assert_eq!(rp.group, "proj", "legacy: group == slug == project_slug");
        assert_eq!(rp.repo, "");
        assert_eq!(rp.eff.active_states, c.tracker.active_states);
        assert_eq!(rp.eff.terminal_states, c.tracker.terminal_states);
        assert_eq!(rp.eff.prompt, "top prompt");
        assert_eq!(rp.eff.claude, c.claude);
        assert_eq!(
            rp.eff.max_concurrent_agents, 0,
            "no per-project cap on the synthesized project"
        );
    }

    // Mirrors Go `TestResolveProjectsFanout`.
    #[test]
    fn fanout() {
        let mut c = base_cfg();
        c.repo = "r0".to_string();
        c.projects = vec![
            Project {
                repo: "r1".to_string(),
                slugs: vec!["a".to_string(), "b".to_string()],
                ..Default::default()
            },
            Project {
                slugs: vec!["c".to_string()],
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 3, "expected 3 resolved projects");
        // Every slug fanned out from the same Project shares one stable group key (the project's
        // first slug), so the per-project cap is counted across the whole group.
        let want = [("a", "r1", "a"), ("b", "r1", "a"), ("c", "r0", "c")];
        for (i, (slug, repo, group)) in want.iter().enumerate() {
            assert_eq!(rps[i].slug, *slug, "rps[{i}].slug");
            assert_eq!(rps[i].repo, *repo, "rps[{i}].repo");
            assert_eq!(rps[i].group, *group, "rps[{i}].group");
        }
    }

    // Mirrors Go `TestResolveProjectsClaudeOverrideInheritVsSet`.
    #[test]
    fn claude_override_inherit_vs_set() {
        let mut c = base_cfg();
        c.projects = vec![
            Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                claude: Some(ClaudeOverride {
                    model: Some("sonnet".to_string()),
                    // allowed_tools/effort/extra_args unset => inherit
                    ..Default::default()
                }),
                ..Default::default()
            },
            Project {
                repo: "r".to_string(),
                slugs: vec!["b".to_string()],
                claude: None, // whole block inherits
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        let a = &rps[0].eff.claude;
        assert_eq!(a.model, "sonnet", "overridden");
        assert_eq!(a.effort, "high", "inherited");
        assert_eq!(a.allowed_tools, "Read,Edit", "inherited");
        assert_eq!(a.extra_args, vec!["--foo".to_string()], "inherited");
        assert_eq!(a.billing_guard, Some(true), "inherited true");
        let b = &rps[1].eff.claude;
        assert_eq!(*b, c.claude, "nil override should equal top-level");
    }

    // Mirrors Go `TestResolveProjectsClaudeOverrideOurSpecificFields`.
    #[test]
    fn claude_override_our_specific_fields() {
        let mut c = base_cfg();
        c.projects = vec![Project {
            repo: "r".to_string(),
            slugs: vec!["a".to_string()],
            claude: Some(ClaudeOverride {
                allowed_tools: Some("OnlyRead".to_string()),
                disallowed_tools: Some("Bash".to_string()),
                extra_args: vec!["--bar".to_string(), "--baz".to_string()],
                billing_guard: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let rps = resolve_projects(&c);
        let a = &rps[0].eff.claude;
        assert_eq!(a.allowed_tools, "OnlyRead");
        assert_eq!(a.disallowed_tools, "Bash");
        assert_eq!(a.extra_args, vec!["--bar".to_string(), "--baz".to_string()]);
        assert_eq!(a.billing_guard, Some(false));
    }

    // Mirrors Go `TestResolveProjectsPerProjectStatesAndCap`.
    #[test]
    fn per_project_states_and_cap() {
        let mut c = base_cfg();
        c.projects = vec![
            Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                active_states: vec!["Started".to_string()],
                terminal_states: vec!["Shipped".to_string(), "Abandoned".to_string()],
                canceled_states: vec!["Abandoned".to_string()],
                prompt: "proj prompt".to_string(),
                max_concurrent_agents: Some(2),
                ..Default::default()
            },
            Project {
                repo: "r".to_string(),
                slugs: vec!["b".to_string()],
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        let a = &rps[0].eff;
        assert_eq!(a.active_states, vec!["Started".to_string()]);
        assert_eq!(
            a.terminal_states,
            vec!["Shipped".to_string(), "Abandoned".to_string()]
        );
        assert_eq!(
            a.canceled_states,
            vec!["Abandoned".to_string()],
            "per-project override should win"
        );
        assert_eq!(a.prompt, "proj prompt");
        assert_eq!(a.max_concurrent_agents, 2);
        let b = &rps[1].eff;
        assert_eq!(
            b.active_states, c.tracker.active_states,
            "b active inherits"
        );
        assert_eq!(
            b.canceled_states, c.tracker.canceled_states,
            "b canceled inherits"
        );
        assert_eq!(b.prompt, "top prompt", "b prompt inherits");
        assert_eq!(b.max_concurrent_agents, 0, "no per-project cap");
    }

    // Mirrors Go `TestResolveProjectsHooksOverride`.
    #[test]
    fn hooks_override() {
        let mut c = base_cfg();
        c.projects = vec![
            Project {
                repo: "r".to_string(),
                slugs: vec!["a".to_string()],
                hooks: Some(Hooks {
                    after_create: "echo a".to_string(),
                    before_run: String::new(),
                    after_run: String::new(),
                    before_remove: String::new(),
                    timeout_ms: 5,
                }),
                ..Default::default()
            },
            Project {
                repo: "r".to_string(),
                slugs: vec!["b".to_string()],
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        assert_eq!(rps[0].eff.hooks.after_create, "echo a", "override");
        assert_eq!(rps[1].eff.hooks.after_create, "echo top", "b inherits");
    }

    // Mirrors Go `TestResolveProjectsNameAndEnabledDefaults`.
    #[test]
    fn name_and_enabled_defaults() {
        let mut c = base_cfg();
        c.repo = "r0".to_string();
        c.projects = vec![
            Project {
                slugs: vec!["alpha".to_string(), "alpha2".to_string()],
                repo: "r1".to_string(),
                ..Default::default()
            }, // no name => first slug; enabled None => true
            Project {
                slugs: vec!["beta".to_string()],
                repo: "r2".to_string(),
                name: "Bravo".to_string(),
                enabled: Some(false),
                ..Default::default()
            }, // explicit name + paused
            Project {
                slugs: vec!["gamma".to_string()],
                repo: "r3".to_string(),
                enabled: Some(true),
                ..Default::default()
            }, // explicit enabled true
        ];
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 4, "alpha fans to 2");
        // alpha + alpha2 fan out from one Project: share the default name (first slug) and enabled.
        for i in [0, 1] {
            assert_eq!(rps[i].name, "alpha", "rps[{i}].name default = first slug");
            assert!(rps[i].enabled, "rps[{i}].enabled unset defaults enabled");
        }
        assert_eq!(rps[2].name, "Bravo");
        assert!(!rps[2].enabled, "beta paused");
        assert_eq!(rps[3].name, "gamma");
        assert!(rps[3].enabled);
    }

    // Mirrors Go `TestResolveProjectsLegacySynthNameAndEnabled`.
    #[test]
    fn legacy_synth_name_and_enabled() {
        let c = base_cfg(); // single-project via tracker.project_slug "proj"
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 1);
        assert_eq!(
            rps[0].name, "proj",
            "legacy synth name = tracker.project_slug"
        );
        assert!(rps[0].enabled, "legacy synth project must be enabled");
    }

    // Mirrors Go `TestDecodeProjectNameAndEnabled` (the decode→resolve boundary the name/enabled
    // defaults build on: decode keeps name "" and enabled None for an unset project).
    #[test]
    fn decode_project_name_and_enabled() {
        let c = cfg_from(
            concat!(
                "tracker:\n  kind: linear\n  api_key: \"$X\"\n",
                "repo: \"git@github.com:o/r.git\"\n",
                "projects:\n",
                "  - name: Infra Bot\n    slugs:\n      - s1\n    enabled: false\n",
                "  - slugs:\n      - s2\n",
            ),
            "body",
        );
        assert_eq!(c.projects.len(), 2);
        assert_eq!(c.projects[0].name, "Infra Bot");
        assert_eq!(
            c.projects[0].enabled,
            Some(false),
            "explicit false decodes verbatim"
        );
        // Name default (first slug) and enabled default (true) are applied at resolve_projects, not
        // decode, so the raw decoded project keeps name "" and enabled None.
        assert_eq!(c.projects[1].name, "", "name stays empty at decode");
        assert_eq!(c.projects[1].enabled, None, "enabled stays None at decode");
    }

    // Mirrors Go `TestResolveProjectsNeverMutatesInput` (minus the `ResolveProjects(nil)` nil-guard,
    // which is not representable for a Rust `&Resolved`).
    #[test]
    fn never_mutates_input() {
        let mut c = base_cfg();
        c.repo = "r0".to_string();
        c.projects = vec![Project {
            repo: "r1".to_string(),
            slugs: vec!["a".to_string()],
            claude: Some(ClaudeOverride {
                model: Some("x".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let before = c.clone();
        let _ = resolve_projects(&c);
        let _ = resolve_projects(&c);
        assert_eq!(c, before, "resolve_projects must not mutate its input");
    }

    // ---- review_test.go mirrors (the EffectiveFor/ResolveProjects halves) ----

    // Mirrors Go `TestEffectiveReviewStatesPerProjectOverride`.
    #[test]
    fn effective_review_states_per_project_override() {
        let mut c = base_cfg();
        c.tracker.review_states = vec!["In Review".to_string()];
        c.projects = vec![
            Project {
                slugs: vec!["a".to_string()],
                review_states: vec!["QA".to_string(), "Reviewing".to_string()],
                ..Default::default()
            },
            Project {
                slugs: vec!["b".to_string()],
                ..Default::default()
            },
        ];
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 2);
        let a = rps.iter().find(|rp| rp.slug == "a").expect("slug a");
        let b = rps.iter().find(|rp| rp.slug == "b").expect("slug b");
        assert_eq!(
            a.eff.review_states,
            vec!["QA".to_string(), "Reviewing".to_string()],
            "override"
        );
        assert_eq!(
            b.eff.review_states,
            vec!["In Review".to_string()],
            "inherited top-level"
        );
    }

    // Mirrors Go `TestEffectiveReviewStatesSynthesizedInherits`.
    #[test]
    fn effective_review_states_synthesized_inherits() {
        let mut c = base_cfg();
        c.tracker.review_states = vec!["In Review".to_string()];
        let rps = resolve_projects(&c);
        assert_eq!(rps.len(), 1);
        assert_eq!(rps[0].eff.review_states, vec!["In Review".to_string()]);
    }

    // ---- dependency_mode_test.go mirrors (the EffectiveFor halves) ----

    // Mirrors Go `TestEffectiveDependencyModeDefaultsToDisabled`.
    #[test]
    fn effective_dependency_mode_defaults_to_disabled() {
        // git_flow set to graphite: it must NOT influence the dependency_mode default.
        let c = cfg_from(
            "tracker:\n  project_slug: top\n  active_states:\n    - Todo\ngit_flow: graphite\n",
            "",
        );
        let eff = effective_for(&c, None);
        assert_eq!(
            eff.dependency_mode, DEPENDENCY_MODE_DISABLED,
            "no git_flow coupling"
        );
        assert_eq!(eff.dep_mode_prompt_file, DEFAULT_DEP_MODE_PROMPT_FILE);
    }

    // Mirrors Go `TestEffectiveDependencyModeInherit`.
    #[test]
    fn effective_dependency_mode_inherit() {
        let mut c = cfg_from(
            concat!(
                "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n",
                "  review_states:\n    - In Review\n",
                "  dependency_mode: graphite\n  dep_mode_prompt_file: .symphony/G.md\n",
            ),
            "",
        );
        c.projects = vec![
            Project {
                slugs: vec!["a-1".to_string()],
                dependency_mode: DEPENDENCY_MODE_DAG.to_string(),
                dep_mode_prompt_file: ".symphony/A.md".to_string(),
                ..Default::default()
            }, // explicit override
            Project {
                slugs: vec!["b-1".to_string()],
                ..Default::default()
            }, // inherits global graphite
        ];
        assert_eq!(
            effective_for(&c, Some(&c.projects[0])).dependency_mode,
            DEPENDENCY_MODE_DAG,
            "project[0] own dag"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[0])).dep_mode_prompt_file,
            ".symphony/A.md"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[1])).dependency_mode,
            DEPENDENCY_MODE_GRAPHITE,
            "project[1] inherits global graphite"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[1])).dep_mode_prompt_file,
            ".symphony/G.md",
            "project[1] inherits global dep_mode_prompt_file"
        );
        assert_eq!(
            effective_for(&c, None).dependency_mode,
            DEPENDENCY_MODE_GRAPHITE,
            "explicit global beats disabled default"
        );
    }

    // ---- claim_mode_test.go mirrors (the EffectiveFor halves) ----

    // Mirrors Go `TestEffectiveClaimModeDefaultsToAssignee`.
    #[test]
    fn effective_claim_mode_defaults_to_assignee() {
        let c = cfg_from(
            "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n",
            "",
        );
        assert_eq!(effective_for(&c, None).claim_mode, CLAIM_MODE_ASSIGNEE);
    }

    // Mirrors Go `TestEffectiveClaimModeInherit`.
    #[test]
    fn effective_claim_mode_inherit() {
        let mut c = cfg_from(
            "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n  claim_mode: pool\n",
            "",
        );
        c.projects = vec![
            Project {
                slugs: vec!["a-1".to_string()],
                claim_mode: CLAIM_MODE_ASSIGNEE.to_string(),
                ..Default::default()
            }, // explicit override
            Project {
                slugs: vec!["b-1".to_string()],
                ..Default::default()
            }, // inherits global pool
        ];
        assert_eq!(
            effective_for(&c, Some(&c.projects[0])).claim_mode,
            CLAIM_MODE_ASSIGNEE,
            "project[0] own assignee"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[1])).claim_mode,
            CLAIM_MODE_POOL,
            "project[1] inherits global pool"
        );
        assert_eq!(
            effective_for(&c, None).claim_mode,
            CLAIM_MODE_POOL,
            "explicit global pool"
        );
    }

    // ---- workspacemode_test.go mirrors (the EffectiveFor halves) ----

    // Mirrors Go `TestWorkspaceModeDefaultsToWorktree`.
    #[test]
    fn workspace_mode_defaults_to_worktree() {
        let c = cfg_from(
            "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n",
            "",
        );
        assert_eq!(
            effective_for(&c, None).workspace_mode,
            WORKSPACE_MODE_WORKTREE
        );
    }

    // Mirrors Go `TestWorkspaceModeNotDerivedFromDependencyMode`.
    #[test]
    fn workspace_mode_not_derived_from_dependency_mode() {
        for dm in [DEPENDENCY_MODE_GRAPHITE, DEPENDENCY_MODE_DAG] {
            let c = cfg_from(
                &format!(
                    "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n  review_states:\n    - In Review\n  dependency_mode: {dm}\n"
                ),
                "",
            );
            assert_eq!(
                effective_for(&c, None).workspace_mode,
                WORKSPACE_MODE_WORKTREE,
                "dependency_mode={dm} (top-level): NOT derived"
            );
            let p = Project {
                slugs: vec!["a-1".to_string()],
                dependency_mode: dm.to_string(),
                ..Default::default()
            };
            assert_eq!(
                effective_for(&c, Some(&p)).workspace_mode,
                WORKSPACE_MODE_WORKTREE,
                "dependency_mode={dm} (per-project): NOT derived"
            );
        }
    }

    // Mirrors the EffectiveFor assertions of Go `TestEncodeWorkspaceModeRoundTrip` (an inheriting
    // project sees the global value; an overriding one its own).
    #[test]
    fn effective_workspace_mode_inherit_vs_override() {
        let c = cfg_from(
            concat!(
                "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n",
                "repo: \"git@github.com:o/r.git\"\n",
                "workspace_mode: clone\n",
                "projects:\n",
                "  - slugs:\n      - a-1\n    workspace_mode: worktree\n",
                "  - slugs:\n      - b-1\n",
            ),
            "body",
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[1])).workspace_mode,
            WORKSPACE_MODE_CLONE,
            "inheriting project resolves global clone"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[0])).workspace_mode,
            WORKSPACE_MODE_WORKTREE,
            "overriding project resolves its own worktree"
        );
    }

    // ---- gitflow_test.go mirrors (the EffectiveFor halves) ----

    // Mirrors Go `TestGitFlowDefaultsToEmpty`.
    #[test]
    fn git_flow_defaults_to_empty() {
        let c = cfg_from(
            "tracker:\n  project_slug: top\n  active_states:\n    - Todo\n",
            "",
        );
        assert_eq!(effective_for(&c, None).git_flow, "", "empty == any");
    }

    // Mirrors the EffectiveFor assertions of Go `TestEncodeGitFlowRoundTrip` (an inheriting project
    // sees the global value; an overriding one its own).
    #[test]
    fn effective_git_flow_inherit_vs_override() {
        let c = cfg_from(
            concat!(
                "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n",
                "repo: \"git@github.com:o/r.git\"\n",
                "git_flow: graphite\n",
                "projects:\n",
                "  - slugs:\n      - a-1\n    git_flow: any\n",
                "  - slugs:\n      - b-1\n",
            ),
            "body",
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[1])).git_flow,
            "graphite",
            "inheriting project resolves global git_flow"
        );
        assert_eq!(
            effective_for(&c, Some(&c.projects[0])).git_flow,
            "any",
            "overriding project resolves its own git_flow"
        );
    }
}
