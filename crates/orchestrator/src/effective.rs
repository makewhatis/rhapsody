//! effective — parity port of Go `internal/orchestrator/effective.go`.
//!
//! Turns a resolved [`Config`] into the live config + built dependencies the control loop schedules
//! against ([`Effective`]), including the resolved per-project routing set ([`ResolvedProject`],
//! Phase 2). It is rebuilt and atomically swapped on reload (upstream §6.2) — that swap is the
//! control loop's concern (O7); O1 provides the builder + the runtime view.
//!
//! Concurrency mapping: Go holds the shared `*workspace.Manager` and the `agent.Runner` /
//! `tracker.Tracker` interface values by pointer; the Rust port holds them behind [`Arc`] so a
//! single-project config can share one tracker client between [`Effective::tracker`] and its lone
//! [`ResolvedProject::tracker`] (pointer identity is asserted by the tests) and every project shares
//! the one workspace manager.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rhapsody_agent::{Runner, claude};
use rhapsody_config::{Config, EffectiveConfig, effective_for, resolve_projects};
use rhapsody_core::normalize_state;
use rhapsody_tracker::{self as tracker, Tracker};
use rhapsody_workspace::{self as workspace, Manager};

use crate::obslog::Store as TranscriptStore;
use crate::{OrchestratorError, ghsummons, liveness};

/// Default freshness window for pool-mode claim comments. Mirrors Go `config.DefaultClaimTTL`
/// (`2 * time.Minute`). Go places the two claim-timing defaults in its `config` package; the Rust
/// config crate did not port them (config's own logic never needed them), so the orchestrator —
/// their first consumer (`build_effective` here, and O2's claim election) — owns them. INF-477.
pub const DEFAULT_CLAIM_TTL: Duration = Duration::from_secs(120);
/// Default base settle wait for pool-mode claims. Mirrors Go `config.DefaultClaimSettleDelay`
/// (`time.Second`).
pub const DEFAULT_CLAIM_SETTLE_DELAY: Duration = Duration::from_secs(1);

/// One runtime routing target: a single Linear slug bound to its own tracker client + materialized
/// effective config (Phase 2). Mirrors Go `resolvedProject`. The shared [`Runner`] and [`Manager`]
/// are referenced (one per backend+root this phase) so workers run with the project's
/// prompt/active-states. `repo` is carried through for Phase 3 (worktrees).
pub struct ResolvedProject {
    pub slug: String,
    /// The stable per-project key shared by every slug fanned out from the same project. The
    /// per-project concurrency cap is counted across the whole group (see the loop's
    /// `running_in_project_group`, O2), so a multi-slug project admits at most `max_concurrent`
    /// agents across all its slugs. `group == slug` for single-slug and legacy single-project modes.
    pub group: String,
    pub repo: String,
    /// The project's display label (defaults to the first slug). Carried for the per-project status
    /// surface (INF-224); does not affect routing.
    pub name: String,
    /// The resolved pause flag, stored INVERTED from config's `enabled` so the zero value (`false`)
    /// means "enabled" — the many test-constructed projects omit it and must default to enabled.
    /// `build_effective` sets `disabled = !rp.enabled`; the poll paths skip a disabled project
    /// (INF-224).
    pub disabled: bool,

    /// Value-copy of the top-level config overlaid with this project's effective knobs (Go `mcfg`).
    /// Its purpose is to construct the per-project [`Runner`] and to carry the project's terminal
    /// states; it is NOT an independently validatable single-project config.
    pub mcfg: Config,
    pub tracker: Arc<dyn Tracker>,

    pub active_states: HashSet<String>,
    pub terminal_states: HashSet<String>,
    /// This project's normalized required-label set (match-ANY, case-insensitive); resolved from the
    /// project's effective labels (per-project override, else inherited global). Empty ⇒ no filter.
    pub labels: HashSet<String>,
    /// This project's default capability names (registry keys), in config order — order matters for
    /// rendering, so it is a `Vec`, not a set. Prepended (registry-rendered) to a dispatched agent's
    /// turn-1 prompt, additively unioned with the ticket's `rhapsody:*` labels (BO-12).
    pub capabilities: Vec<String>,
    /// The per-project normalized cancel-type set (INF-272); a state classifies as cancel-type only
    /// when it is ALSO in `terminal_states`.
    pub canceled_states: HashSet<String>,
    /// The per-project set of normalized review-state names; empty when the feature is off.
    pub review_states: HashSet<String>,
    pub per_state_limits: HashMap<String, i64>,
    /// Per-project cap; falls back to the global cap when unset.
    pub max_concurrent: i64,
    pub prompt_tmpl: String,
    /// When non-empty, WINS over `prompt_tmpl`: the worker reads it per-run.
    pub prompt_file: String,
    pub stall_timeout: Duration,
    /// The project's effective git-workflow policy (`""`/`"any"` ⇒ no enforcement, `"graphite"` ⇒
    /// the worktree bootstrap injects the guard hook before spawn; INF-251).
    pub git_flow: String,
    /// The project's effective workspace-provisioning policy (`"worktree"` | `"clone"`; always
    /// non-empty post-resolve). INF-418.
    pub workspace_mode: String,
    /// The project's effective DAG-orchestration policy (`"disabled"` | `"graphite"` | `"dag"`;
    /// always non-empty post-resolve). `dep_mode_prompt_file` is the mode-on prompt path. INF-318.
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    /// The project's effective ticket-claim policy (`"assignee"` | `"pool"`; always non-empty
    /// post-resolve). INF-477.
    pub claim_mode: String,
    /// The project's effective claude model (`mcfg.claude.model`). A bounded telemetry label.
    pub model: String,

    /// Mirrors this project's `tracker.github_summons` flag. `gh_owner`/`gh_repo` are parsed once
    /// from `repo` at build time (empty when `repo` is absent or not a GitHub remote). The poll-side
    /// enrichment is gated on `github_summons && gh_source.is_some()`, so all three default to their
    /// zero values for test-constructed projects, leaving the poll path unchanged. AIE-299.
    pub github_summons: bool,
    pub gh_owner: String,
    pub gh_repo: String,

    pub agent: Arc<dyn Runner>,
    pub workspace: Arc<Manager>,
}

/// The live config + built dependencies the loop schedules against (Go `effective`). Rebuilt and
/// atomically swapped on reload (upstream §6.2).
pub struct Effective {
    pub cfg: Config,
    pub tracker: Arc<dyn Tracker>,
    pub workspace: Arc<Manager>,
    pub agent: Arc<dyn Runner>,
    pub prompt_tmpl: String,
    pub active_states: HashSet<String>,
    pub terminal_states: HashSet<String>,
    /// The top-level normalized cancel-type set (INF-272).
    pub canceled_states: HashSet<String>,
    /// The top-level normalized review-state set (empty ⇒ feature off); `summon_token` is the
    /// comment-body token that re-engages a review ticket; `review_promote_state` is the active
    /// state a summoned ticket is moved to before dispatch.
    pub review_states: HashSet<String>,
    pub summon_token: String,
    pub review_promote_state: String,
    /// The top-level/default normalized required-label set (match-ANY, case-insensitive). Empty ⇒
    /// no label filter. Per-project sets live on [`ResolvedProject::labels`]; this is the fallback
    /// for the legacy single-project path.
    pub labels: HashSet<String>,
    /// The top-level/default capability names (registry keys), in config order — the fallback for the
    /// legacy single-project path, mirroring `labels`. Per-project sets live on
    /// [`ResolvedProject::capabilities`]. Order matters for rendering, so it is a `Vec` (BO-12).
    pub capabilities: Vec<String>,
    pub per_state_limits: HashMap<String, i64>,
    pub max_concurrent: i64,
    pub prompt_file: String,
    pub git_flow: String,
    /// The top-level/legacy effective workspace-provisioning policy (`"worktree"` | `"clone"`;
    /// always non-empty post-resolve). INF-418.
    pub workspace_mode: String,
    /// The GitHub label name the post-run labeler adds to every PR in a run's stack (default
    /// `"symphony"`; blank/absent inherits the default — there is no config disable). Daemon-wide.
    /// AIE-301.
    pub pr_label: String,
    /// The top-level/legacy effective DAG policy (always non-empty post-resolve);
    /// `dep_mode_prompt_file` is the legacy mode-on prompt path. INF-318.
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    /// The top-level/legacy effective ticket-claim policy (`"assignee"` | `"pool"`; always
    /// non-empty post-resolve). `claim_ttl` / `claim_settle_delay` are the pool-mode election timing
    /// knobs, materialized to [`DEFAULT_CLAIM_TTL`] / [`DEFAULT_CLAIM_SETTLE_DELAY`] when unset.
    /// INF-477.
    pub claim_mode: String,
    pub claim_ttl: Duration,
    pub claim_settle_delay: Duration,
    pub max_turns: i64,
    pub max_retry_backoff_ms: i64,
    pub poll_interval: Duration,
    pub stall_timeout: Duration,
    pub cpu_sampler: Arc<dyn liveness::Sampler>,
    pub log_dir: String,
    pub transcripts: Arc<TranscriptStore>,

    /// The resolved routing set built once per reload. Single-project mode resolves to exactly one
    /// entry whose `tracker` == the legacy top-level [`Effective::tracker`] and whose fields equal
    /// the top-level effective. The loop takes the multi-project path only when this is populated;
    /// test-injected effectives leave it empty to hit the legacy single-tracker path unchanged.
    pub projects: Vec<ResolvedProject>,
}

impl Effective {
    /// Returns the resolved project for a slug, or `None` if the slug is no longer configured (e.g.
    /// after a hot-reload removed it). Mirrors Go `effective.projectBySlug`.
    pub fn project_by_slug(&self, slug: &str) -> Option<&ResolvedProject> {
        self.projects.iter().find(|p| p.slug == slug)
    }

    /// Returns the latest transcript path for `identifier` without requiring callers (e.g. the API
    /// snapshot, O4) to touch [`crate::obslog`] directly. Mirrors Go `effective.transcriptsLatest`;
    /// the Rust `transcripts` handle is always built, so there is no nil-store fallback branch.
    pub fn transcripts_latest(&self, identifier: &str) -> String {
        self.transcripts.latest_path(identifier)
    }
}

/// Builds an [`Runner`] from a [`claude::Config`]. The injectable seam that lets
/// [`build_effective_with_runner`] construct one runner per resolved project (and the top-level
/// legacy runner) while tests assert which [`claude::Config`] each project receives. Mirrors Go's
/// `runnerFactory func(claude.Config) agent.Runner`.
pub type RunnerFactory<'a> = &'a dyn Fn(claude::Config) -> Arc<dyn Runner>;

/// The production seam: build a claude runner. Mirrors Go `defaultRunnerFactory` (`claude.New`).
fn default_runner_factory(cc: claude::Config) -> Arc<dyn Runner> {
    Arc::new(claude::Runner::new(cc))
}

/// Maps a (materialized) [`Config`] onto an [`Runner`] via the named backend, returning
/// [`OrchestratorError::UnsupportedBackend`] for any backend this build does not implement. Both the
/// top-level legacy runner and every per-project runner route through this single switch. Today
/// only `"claude"` is implemented. Mirrors Go `runnerForBackend`.
fn runner_for_backend(
    cfg: &Config,
    new_runner: RunnerFactory<'_>,
) -> Result<Arc<dyn Runner>, OrchestratorError> {
    match cfg.agent.backend.as_str() {
        "claude" => Ok(new_runner(claude_config_from_cfg(cfg))),
        other => Err(OrchestratorError::UnsupportedBackend(other.to_string())),
    }
}

/// Maps a [`Config`]'s claude/tracker/workspace knobs onto a [`claude::Config`]. Both the top-level
/// legacy runner and every per-project runner are built from this single mapping, so a
/// single-project config (whose materialized per-project claude equals the top-level claude)
/// produces a per-project runner byte-for-byte identical to the top-level runner. Mirrors Go
/// `claudeConfigFromCfg`.
fn claude_config_from_cfg(cfg: &Config) -> claude::Config {
    claude::Config {
        command: cfg.claude.command.clone(),
        model: cfg.claude.model.clone(),
        effort: cfg.claude.effort.clone(),
        permission_mode: cfg.claude.permission_mode.clone(),
        allowed_tools: cfg.claude.allowed_tools.clone(),
        disallowed_tools: cfg.claude.disallowed_tools.clone(),
        mcp_config: cfg.claude.mcp_config.clone(),
        setting_sources: cfg.claude.setting_sources.clone(),
        add_dirs: cfg.claude.add_dirs.clone(),
        workspace_root: cfg.workspace.root.clone(),
        turn_timeout: Duration::from_millis(cfg.claude.turn_timeout_ms.max(0) as u64),
        extra_args: cfg.claude.extra_args.clone(),
        billing_guard: cfg.claude.billing_guard,
        ultracode: cfg.claude.ultracode,
        tracker_api_key: cfg.tracker.api_key.clone(),
        // MCP injection into the dispatched agent (INF-473, default-on via `cfg.mcp.enabled`). The
        // binary is the running daemon's own path; the workflow path lets the child `symphony mcp`
        // resolve the SAME server.port.
        inject_mcp: cfg.mcp.enabled,
        symphony_bin: symphony_bin_path(),
        workflow_path: cfg.workflow_path.clone(),
    }
}

/// Returns the running daemon binary's absolute path (the injected `symphony` MCP server's
/// command). On the rare failure it logs and returns `""` so injection is skipped rather than
/// pointing at a bad command. Mirrors Go `symphonyBinPath` (`os.Executable`).
fn symphony_bin_path() -> String {
    match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "mcp injection: could not resolve symphony binary path; skipping injection"
            );
            String::new()
        }
    }
}

/// Materializes the pool-mode claim TTL: the configured value when positive, else
/// [`DEFAULT_CLAIM_TTL`]. Mirrors Go `claimTTLOrDefault` (`d <= 0 → default`). Defined here (the
/// effective builder is its first caller) rather than in `claim.rs` (O2), which reuses it.
pub(crate) fn claim_ttl_or_default(d: chrono::Duration) -> Duration {
    let ms = d.num_milliseconds();
    if ms <= 0 {
        DEFAULT_CLAIM_TTL
    } else {
        Duration::from_millis(ms as u64)
    }
}

/// Materializes the pool-mode claim settle delay. Mirrors Go `claimSettleOrDefault`.
pub(crate) fn claim_settle_or_default(d: chrono::Duration) -> Duration {
    let ms = d.num_milliseconds();
    if ms <= 0 {
        DEFAULT_CLAIM_SETTLE_DELAY
    } else {
        Duration::from_millis(ms as u64)
    }
}

/// Returns a value-copy of `top` with `eff`'s project overrides overlaid (active/terminal/canceled
/// states, prompt, claude, hooks, cap). Its sole purpose is to construct the per-project [`Runner`]
/// and to carry the project's terminal states; `tracker.project_slug` is intentionally left as the
/// top-level value (routing is keyed by `rp.slug` elsewhere). Mirrors Go `materializeConfig`.
fn materialize_config(top: &Config, eff: &EffectiveConfig) -> Config {
    let mut c = top.clone();
    c.tracker.active_states = eff.active_states.clone();
    c.tracker.terminal_states = eff.terminal_states.clone();
    c.tracker.canceled_states = eff.canceled_states.clone();
    c.prompt_template = eff.prompt.clone();
    c.claude = eff.claude.clone();
    c.hooks = eff.hooks.clone();
    if eff.max_concurrent_agents > 0 {
        c.agent.max_concurrent_agents = eff.max_concurrent_agents;
    }
    // The materialized config describes a single project's effective view; clear the multi-project
    // list so the per-project runner is never built against the top-level project fan-out.
    c.projects = Vec::new();
    c
}

/// Normalizes a state slice into a set, lowercasing/trimming each entry. Mirrors Go `normalizeSet`
/// (whose `map[string]bool` is a set — the Rust port uses [`HashSet`]).
fn normalize_set(states: &[String]) -> HashSet<String> {
    states.iter().map(|s| normalize_state(s)).collect()
}

/// Constructs the live deps from a resolved [`Config`] using the production runner factory
/// (`claude.New`). Mirrors Go `buildEffective`.
pub fn build_effective(cfg: &Config) -> Result<Effective, OrchestratorError> {
    build_effective_with_runner(cfg, &default_runner_factory)
}

/// [`build_effective`] with an injectable runner factory so tests can observe the per-project
/// [`claude::Config`] each runner is built from. Mirrors Go `buildEffectiveWithRunner`.
///
/// Deviation from Go: Go threads a `*slog.Logger` through this call (used for the workspace manager,
/// the claude config, and the sampler/bin-path warnings). The Rust sibling crates emit their
/// diagnostics via `tracing` rather than a threaded logger, so this port drops the logger parameter
/// and uses `tracing::warn!` for the one build-time diagnostic (the CPU-liveness probe).
pub fn build_effective_with_runner(
    cfg: &Config,
    new_runner: RunnerFactory<'_>,
) -> Result<Effective, OrchestratorError> {
    // Resolve the top-level/legacy effective knobs (dependency_mode, workspace_mode, claim_mode) via
    // the config resolver so their defaults are materialized, exactly as the multi-project path uses
    // `rp.eff` below. Computed up front so the top-level tracker is built with the resolved
    // claim_mode (INF-318 / INF-418 / INF-477).
    let top_eff = effective_for(cfg, None);
    let tr: Arc<dyn Tracker> = Arc::from(tracker::new(tracker::Spec {
        kind: cfg.tracker.kind.clone(),
        endpoint: cfg.tracker.endpoint.clone(),
        api_key: cfg.tracker.api_key.clone(),
        project_slug: cfg.tracker.project_slug.clone(),
        source: cfg.tracker.source.clone(),
        active_states: cfg.tracker.active_states.clone(),
        review_states: cfg.tracker.review_states.clone(),
        summon_token: cfg.tracker.summon_token.clone(),
        milestone: cfg.tracker.milestone.clone(),
        claim_mode: top_eff.claim_mode.clone(),
    }));

    let wm = Arc::new(Manager::new(workspace::Config {
        root: cfg.workspace.root.clone(),
        hooks: workspace::HookScripts {
            after_create: cfg.hooks.after_create.clone(),
            before_run: cfg.hooks.before_run.clone(),
            after_run: cfg.hooks.after_run.clone(),
            before_remove: cfg.hooks.before_remove.clone(),
        },
        hook_timeout: Duration::from_millis(cfg.hooks.timeout_ms.max(0) as u64),
    })?);

    // The top-level runner backs the nil-rp legacy/test path (worker-deps fallback). Per-project
    // runners are built below from each project's effective config, routed through the same
    // `runner_for_backend` switch so both paths share one backend gate.
    let runner = runner_for_backend(cfg, new_runner)?;

    // The top-level (legacy/nil-rp) stall timeout; per-project stall timeouts are computed from each
    // project's materialized claude config below. Only the claude backend carries a stall timeout
    // this phase; `runner_for_backend` already rejected any other backend above.
    let stall = if cfg.agent.backend == "claude" {
        Duration::from_millis(cfg.claude.stall_timeout_ms.max(0) as u64)
    } else {
        Duration::ZERO
    };

    let log_dir = cfg.logging.dir.clone();

    let sampler = liveness::new_sampler();
    if !stall.is_zero() && sampler.group_cpu(std::process::id() as i32).is_none() {
        tracing::warn!(
            ?stall,
            "CPU-based liveness unavailable (no readable /proc); stall detection will not fire"
        );
    }

    // Build the resolved routing set once per reload (Phase 2). Single-project mode resolves to
    // exactly one project whose slug-bound tracker IS the legacy top-level tracker (`tr`) and whose
    // effective fields equal the top-level effective.
    let resolved = resolve_projects(cfg);
    let mut projects = Vec::with_capacity(resolved.len());
    for rp in &resolved {
        // Reuse the already-built top-level client when the slug matches, so single-project mode
        // shares one client (and tests that compare `project.tracker == eff.tracker` hold). The
        // review-state set, the configured milestone AND the effective claim_mode are all part of
        // the client's candidate filter, so each must match too before reusing — otherwise a
        // per-project `pool` override would silently reuse the assignee-mode client and never flip
        // the query to unassigned (INF-477).
        let rtr: Arc<dyn Tracker> = if rp.slug == cfg.tracker.project_slug
            && rp.eff.active_states == cfg.tracker.active_states
            && rp.eff.review_states == cfg.tracker.review_states
            && rp.eff.milestone == cfg.tracker.milestone
            && rp.eff.claim_mode == top_eff.claim_mode
        {
            Arc::clone(&tr)
        } else {
            Arc::from(tracker::new(tracker::Spec {
                kind: cfg.tracker.kind.clone(),
                endpoint: cfg.tracker.endpoint.clone(),
                api_key: cfg.tracker.api_key.clone(),
                project_slug: rp.slug.clone(),
                source: cfg.tracker.source.clone(),
                active_states: rp.eff.active_states.clone(),
                review_states: rp.eff.review_states.clone(),
                summon_token: cfg.tracker.summon_token.clone(),
                milestone: rp.eff.milestone.clone(),
                claim_mode: rp.eff.claim_mode.clone(),
            }))
        };

        let max_conc = if rp.eff.max_concurrent_agents <= 0 {
            cfg.agent.max_concurrent_agents
        } else {
            rp.eff.max_concurrent_agents
        };

        // Build a runner from THIS project's effective config so per-project knobs all reach the
        // spawned process. In single-project mode `mcfg.claude` equals the top-level claude config,
        // so the per-project runner == the top-level runner (backward compat).
        let mcfg = materialize_config(cfg, &rp.eff);
        let project_runner = runner_for_backend(&mcfg, new_runner)?;
        let github_summons = mcfg.tracker.github_summons;

        // github-summons routing (AIE-299): parse owner/repo from the project repo once. Inert when
        // the feature is off (read only under the `github_summons && gh_source` gate).
        let (gh_owner, gh_repo) = ghsummons::parse_repo(&rp.repo).unwrap_or_default();

        projects.push(ResolvedProject {
            slug: rp.slug.clone(),
            group: rp.group.clone(),
            name: rp.name.clone(),
            disabled: !rp.enabled,
            repo: rp.repo.clone(),
            github_summons,
            gh_owner,
            gh_repo,
            tracker: rtr,
            active_states: normalize_set(&rp.eff.active_states),
            terminal_states: normalize_set(&rp.eff.terminal_states),
            labels: normalize_set(&rp.eff.labels),
            capabilities: rp.eff.capabilities.clone(),
            canceled_states: normalize_set(&rp.eff.canceled_states),
            review_states: normalize_set(&rp.eff.review_states),
            per_state_limits: cfg.agent.max_concurrent_agents_by_state.clone(),
            max_concurrent: max_conc,
            prompt_tmpl: rp.eff.prompt.clone(),
            prompt_file: rp.eff.prompt_file.clone(),
            git_flow: rp.eff.git_flow.clone(),
            workspace_mode: rp.eff.workspace_mode.clone(),
            dependency_mode: rp.eff.dependency_mode.clone(),
            dep_mode_prompt_file: rp.eff.dep_mode_prompt_file.clone(),
            claim_mode: rp.eff.claim_mode.clone(),
            model: rp.eff.claude.model.clone(),
            stall_timeout: Duration::from_millis(rp.eff.claude.stall_timeout_ms.max(0) as u64),
            mcfg,
            agent: project_runner,
            workspace: Arc::clone(&wm),
        });
    }

    Ok(Effective {
        tracker: Arc::clone(&tr),
        workspace: Arc::clone(&wm),
        agent: runner,
        prompt_tmpl: cfg.prompt_template.clone(),
        prompt_file: cfg.prompt_file.clone(),
        git_flow: cfg.git_flow.clone(),
        workspace_mode: top_eff.workspace_mode.clone(),
        pr_label: cfg.pr_label.clone(),
        dependency_mode: top_eff.dependency_mode.clone(),
        dep_mode_prompt_file: top_eff.dep_mode_prompt_file.clone(),
        claim_mode: top_eff.claim_mode.clone(),
        claim_ttl: claim_ttl_or_default(cfg.tracker.claim_ttl),
        claim_settle_delay: claim_settle_or_default(cfg.tracker.claim_settle_delay),
        active_states: normalize_set(&cfg.tracker.active_states),
        terminal_states: normalize_set(&cfg.tracker.terminal_states),
        canceled_states: normalize_set(&cfg.tracker.canceled_states),
        review_states: normalize_set(&cfg.tracker.review_states),
        summon_token: cfg.tracker.summon_token.clone(),
        review_promote_state: cfg.tracker.review_promote_state.clone(),
        labels: normalize_set(&cfg.tracker.labels),
        capabilities: cfg.tracker.capabilities.clone(),
        per_state_limits: cfg.agent.max_concurrent_agents_by_state.clone(),
        max_concurrent: cfg.agent.max_concurrent_agents,
        max_turns: cfg.agent.max_turns,
        max_retry_backoff_ms: cfg.agent.max_retry_backoff_ms,
        poll_interval: Duration::from_millis(cfg.polling.interval_ms.max(0) as u64),
        stall_timeout: stall,
        cpu_sampler: sampler,
        log_dir: log_dir.clone(),
        transcripts: Arc::new(TranscriptStore::new(log_dir)),
        cfg: cfg.clone(),
        projects,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use rhapsody_config::workflow::{Definition, YamlMap};
    use rhapsody_config::{CLAIM_MODE_POOL, Config, decode, effective_for, resolve};

    use super::*;

    /// Decode a WORKFLOW.md front matter + body into a resolved [`Config`]. Mirrors Go
    /// `effective_test.go`'s `decodeCfg` (workflow.Load → config.Decode → config.Resolve); the front
    /// matter is parsed directly (as the config crate's own tests do) rather than via a temp file,
    /// and `api_key` uses a literal `tok` instead of `$ORCH_TEST_KEY` — the effective tests never
    /// assert on the key and `$VAR` indirection is covered by the config crate's resolve tests, so
    /// this avoids Rust 2024's `unsafe { set_var }` (mirroring the sibling crates' env-free tests).
    fn decode_cfg(front: &str, body: &str) -> Config {
        let config: YamlMap = serde_yaml_ng::from_str(front).expect("front matter parses");
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        let decoded = decode(&def).expect("decode");
        resolve(decoded, "/tmp/wf").expect("resolve")
    }

    const CLAUDE_WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
polling:
  interval_ms: 1234
agent:
  backend: claude
  max_concurrent_agents: 4
  max_turns: 7
  max_concurrent_agents_by_state:
    In Progress: 2
codex:
  stall_timeout_ms: 0
claude:
  command: claude
  stall_timeout_ms: 5000
";

    // Mirrors Go `TestBuildEffectiveClaude`.
    #[test]
    fn build_effective_claude() {
        let cfg = decode_cfg(CLAUDE_WF, "Do {{ issue.identifier }}.");
        let eff = build_effective(&cfg).expect("build_effective");
        assert_eq!(eff.poll_interval, Duration::from_millis(1234));
        assert_eq!(eff.max_concurrent, 4);
        assert_eq!(eff.max_turns, 7);
        assert_eq!(eff.per_state_limits.get("in progress"), Some(&2));
        assert!(eff.active_states.contains("todo"));
        assert!(eff.active_states.contains("in progress"));
        assert!(eff.terminal_states.contains("done"));
        assert!(eff.terminal_states.contains("canceled"));
        assert_eq!(eff.stall_timeout, Duration::from_millis(5000));
        assert_eq!(eff.prompt_tmpl, "Do {{ issue.identifier }}.");
    }

    // Mirrors Go `TestBuildEffectiveSingleProjectPopulatesLegacyFields`.
    #[test]
    fn single_project_populates_legacy_fields() {
        let cfg = decode_cfg(CLAUDE_WF, "Do {{ issue.identifier }}.");
        let eff = build_effective(&cfg).expect("build_effective");
        assert_eq!(eff.projects.len(), 1, "expected exactly 1 resolved project");
        let p = &eff.projects[0];
        assert_eq!(p.slug, "proj");
        assert!(
            Arc::ptr_eq(&p.tracker, &eff.tracker),
            "single-project tracker should be the same client as the legacy top-level tracker"
        );
        assert_eq!(p.max_concurrent, eff.max_concurrent);
        assert_eq!(p.prompt_tmpl, eff.prompt_tmpl);
        assert_eq!(p.stall_timeout, eff.stall_timeout);
        assert!(p.active_states.contains("todo"));
        assert!(p.active_states.contains("in progress"));
        assert!(p.terminal_states.contains("done"));
        assert_eq!(p.per_state_limits.get("in progress"), Some(&2));
        assert_eq!(p.repo, "", "no repo configured");
    }

    // Mirrors Go `TestBuildEffectiveClaimModeOverrideDistinctClient`: a per-project claim_mode:pool
    // override (global default assignee) must build a DISTINCT tracker client, not reuse the
    // assignee-mode top-level one — else the candidate query never flips to unassigned (INF-477).
    #[test]
    fn claim_mode_override_distinct_client() {
        const WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo]
projects:
  - slugs: [proj]
    claim_mode: pool
agent:
  backend: claude
  max_concurrent_agents: 2
claude:
  command: claude
";
        let cfg = decode_cfg(WF, "body");
        let eff = build_effective(&cfg).expect("build_effective");
        assert_eq!(eff.projects.len(), 1);
        assert!(
            !Arc::ptr_eq(&eff.projects[0].tracker, &eff.tracker),
            "a claim_mode:pool override must build its own tracker client"
        );
        assert_eq!(
            effective_for(&cfg, Some(&cfg.projects[0])).claim_mode,
            CLAIM_MODE_POOL,
        );
    }

    const MULTI_PROJECT_WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  active_states: [Todo, In Progress]
  terminal_states: [Done]
repo: git@github.com:o/top.git
projects:
  - repo: git@github.com:o/r1.git
    slugs: [alpha, beta]
    max_concurrent_agents: 2
    prompt: \"alpha prompt\"
  - slugs: [gamma]
    active_states: [Started]
polling:
  interval_ms: 1000
agent:
  backend: claude
  max_concurrent_agents: 6
claude:
  command: claude
  stall_timeout_ms: 4000
";

    // Mirrors Go `TestBuildEffectiveMultiProject`.
    #[test]
    fn multi_project() {
        let cfg = decode_cfg(MULTI_PROJECT_WF, "top prompt body");
        let eff = build_effective(&cfg).expect("build_effective");
        assert_eq!(eff.projects.len(), 3, "expected alpha,beta,gamma");
        let by_slug: HashMap<&str, &ResolvedProject> =
            eff.projects.iter().map(|p| (p.slug.as_str(), p)).collect();
        for s in ["alpha", "beta", "gamma"] {
            assert!(by_slug.contains_key(s), "missing resolved project {s}");
        }
        // Distinct slug-bound trackers.
        assert!(!Arc::ptr_eq(
            &by_slug["alpha"].tracker,
            &by_slug["beta"].tracker
        ));
        assert!(!Arc::ptr_eq(
            &by_slug["alpha"].tracker,
            &by_slug["gamma"].tracker
        ));
        assert_eq!(by_slug["alpha"].max_concurrent, 2);
        assert_eq!(by_slug["alpha"].prompt_tmpl, "alpha prompt");
        // gamma has no per-project cap => falls back to global 6.
        assert_eq!(by_slug["gamma"].max_concurrent, 6);
        assert!(by_slug["gamma"].active_states.contains("started"));
        assert!(
            !by_slug["gamma"].active_states.contains("todo"),
            "gamma active_states should NOT include todo (overridden)"
        );
        assert_eq!(by_slug["alpha"].repo, "git@github.com:o/r1.git");
        assert_eq!(
            by_slug["gamma"].repo, "git@github.com:o/top.git",
            "gamma repo should inherit top-level"
        );
    }

    // Mirrors Go `TestBuildEffectivePerProjectRunner`: the per-project claude config reaches a
    // distinct runner; the factory is invoked once per resolved project + once for the top-level
    // legacy runner, each with that project's effective `claude.Config`.
    #[test]
    fn per_project_runner() {
        const WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  active_states: [Todo]
  terminal_states: [Done]
repo: git@github.com:o/top.git
projects:
  - slugs: [alpha]
    claude:
      model: sonnet
      billing_guard: true
  - slugs: [gamma]
    claude:
      model: opus
      billing_guard: false
polling:
  interval_ms: 1000
agent:
  backend: claude
  max_concurrent_agents: 6
claude:
  command: claude
  model: top-model
";
        let cfg = decode_cfg(WF, "top prompt body");
        let got_configs: RefCell<Vec<claude::Config>> = RefCell::new(Vec::new());
        let factory = |cc: claude::Config| -> Arc<dyn Runner> {
            got_configs.borrow_mut().push(cc.clone());
            Arc::new(claude::Runner::new(cc))
        };
        let eff = build_effective_with_runner(&cfg, &factory).expect("build_effective");

        assert_eq!(eff.projects.len(), 2, "expected 2 resolved projects");
        assert_eq!(
            got_configs.borrow().len(),
            eff.projects.len() + 1,
            "factory: top-level + one per resolved project"
        );

        let by_model: HashMap<String, claude::Config> = got_configs
            .borrow()
            .iter()
            .map(|cc| (cc.model.clone(), cc.clone()))
            .collect();
        assert!(
            by_model.contains_key("top-model"),
            "top-level runner from top-level model"
        );
        let alpha = by_model.get("sonnet").expect("alpha model sonnet");
        let gamma = by_model.get("opus").expect("gamma model opus");
        assert_eq!(alpha.billing_guard, Some(true));
        assert_eq!(gamma.billing_guard, Some(false));

        let by_slug: HashMap<&str, &ResolvedProject> =
            eff.projects.iter().map(|p| (p.slug.as_str(), p)).collect();
        assert!(
            !Arc::ptr_eq(&by_slug["alpha"].agent, &by_slug["gamma"].agent),
            "distinct projects must get distinct per-project runners"
        );
    }

    // Mirrors Go `TestBuildEffectiveSingleProjectRunnerUsesTopLevel`: the no-override single-project
    // case builds its per-project runner from the TOP-LEVEL claude config (backward compat).
    #[test]
    fn single_project_runner_uses_top_level() {
        let cfg = decode_cfg(CLAUDE_WF, "Do {{ issue.identifier }}.");
        let got_configs: RefCell<Vec<claude::Config>> = RefCell::new(Vec::new());
        let factory = |cc: claude::Config| -> Arc<dyn Runner> {
            got_configs.borrow_mut().push(cc.clone());
            Arc::new(claude::Runner::new(cc))
        };
        let eff = build_effective_with_runner(&cfg, &factory).expect("build_effective");
        assert_eq!(eff.projects.len(), 1, "expected exactly 1 resolved project");
        assert_eq!(
            got_configs.borrow().len(),
            2,
            "factory: top-level + single project"
        );
        for cc in got_configs.borrow().iter() {
            assert_eq!(cc.command, "claude", "runner built with top-level command");
            assert_eq!(cc.model, "", "runner built with top-level (empty) model");
        }
    }

    // Mirrors Go `TestBuildEffectiveCodexUnsupported`.
    #[test]
    fn codex_unsupported() {
        let mut cfg = decode_cfg(CLAUDE_WF, "Do {{ issue.identifier }}.");
        cfg.agent.backend = "codex".to_string();
        // `Effective` does not implement `Debug` (it holds `Arc<dyn Tracker>` etc.), so match the
        // result with `matches!` rather than `expect_err`.
        let res = build_effective(&cfg);
        assert!(
            matches!(res, Err(OrchestratorError::UnsupportedBackend(ref b)) if b == "codex"),
            "codex backend must return UnsupportedBackend(\"codex\")"
        );
    }
}
