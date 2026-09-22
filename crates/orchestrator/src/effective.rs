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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rhapsody_agent::{Harness, HarnessId, HarnessKnobs, HarnessSpec, claude, opencode};
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
    /// The project's effective set of backlog-state names auto-promote may promote FROM (STUDIO-948),
    /// normalized (case/whitespace-folded). EMPTY ⇒ unset: every backlog-type state is promotable —
    /// the safety-critical default byte-identical to pre-948 behavior. Rhapsody-only.
    pub promote_from_states: HashSet<String>,
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

    pub agent: Arc<dyn Harness>,
    /// One runner per implemented `agent.backend`, so a routed teammate whose profile names a
    /// `harness` runs on that one while every other teammate keeps [`Self::agent`] (STUDIO-902).
    /// Always populated; [`Self::agent`] remains the configured backend's runner and is what every
    /// dispatch that names no harness uses, which is what keeps this additive.
    pub agents: BTreeMap<String, Arc<dyn Harness>>,
    pub workspace: Arc<Manager>,
}

/// The live config + built dependencies the loop schedules against (Go `effective`). Rebuilt and
/// atomically swapped on reload (upstream §6.2).
pub struct Effective {
    pub cfg: Config,
    pub tracker: Arc<dyn Tracker>,
    pub workspace: Arc<Manager>,
    pub agent: Arc<dyn Harness>,
    /// One runner per implemented `agent.backend`, so a routed teammate whose profile names a
    /// `harness` runs on that one while every other teammate keeps [`Self::agent`] (STUDIO-902).
    /// Always populated; [`Self::agent`] remains the configured backend's runner and is what every
    /// dispatch that names no harness uses, which is what keeps this additive.
    pub agents: BTreeMap<String, Arc<dyn Harness>>,
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
    /// The SEPARATE global budget for ticketless review runs (STUDIO-950), or `None` when the
    /// operator never set `agent.max_concurrent_reviews` — in which case the review watcher draws
    /// the shared `max_concurrent` pool exactly as it did before the key existed. A non-positive
    /// value is normalized to `None` here, so every reader can treat `Some(n)` as `n > 0`.
    pub max_concurrent_reviews: Option<i64>,
    pub prompt_file: String,
    pub git_flow: String,
    /// The top-level/legacy effective workspace-provisioning policy (`"worktree"` | `"clone"`;
    /// always non-empty post-resolve). INF-418.
    pub workspace_mode: String,
    /// The GitHub label name the post-run labeler adds to every PR in a run's stack (default
    /// `"rhapsody"`; blank/absent inherits the default — there is no config disable). Daemon-wide.
    /// AIE-301.
    pub pr_label: String,
    /// The top-level/legacy effective DAG policy (always non-empty post-resolve);
    /// `dep_mode_prompt_file` is the legacy mode-on prompt path. INF-318.
    pub dependency_mode: String,
    pub dep_mode_prompt_file: String,
    /// The top-level/legacy set of backlog-state names auto-promote may promote FROM (STUDIO-948),
    /// normalized. EMPTY ⇒ unset: every backlog-type state is promotable (the pre-948 default).
    /// Per-project sets live on [`ResolvedProject::promote_from_states`]. Rhapsody-only.
    pub promote_from_states: HashSet<String>,
    /// The top-level/legacy effective ticket-claim policy (`"assignee"` | `"pool"`; always
    /// non-empty post-resolve). `claim_ttl` / `claim_settle_delay` are the pool-mode election timing
    /// knobs, materialized to [`DEFAULT_CLAIM_TTL`] / [`DEFAULT_CLAIM_SETTLE_DELAY`] when unset.
    /// INF-477.
    pub claim_mode: String,
    pub claim_ttl: Duration,
    pub claim_settle_delay: Duration,
    pub max_turns: i64,
    /// The per-run token ceiling (STUDIO-967), `0` when unset. A positive value stops a run that has
    /// accumulated this many billed tokens within the turn it is in; `0` bounds nothing. The reader
    /// ([`crate::agentupdate`]) gates on `> 0`, so an unset key is byte-identical to before it.
    pub max_run_tokens: i64,
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

/// Builds a [`Runner`] from a [`HarnessSpec`] (STUDIO-900; design record
/// `~/.rhapsody/docs/pluggable-harnesses-design.md` §3/§9). The injectable seam that lets
/// [`build_effective_with_runner`] construct one runner per resolved project (and the top-level
/// legacy runner) while tests assert which spec each project receives. Mirrors Go's
/// `runnerFactory func(claude.Config) agent.Runner`, generalized: the factory used to be typed to
/// Claude's own config (design §1.2's "the seam cannot construct a non-Claude runner"); it now
/// takes the harness-agnostic spec, though `"claude"` remains the only backend
/// [`runner_for_backend`] implements — see that function's doc.
pub type RunnerFactory<'a> = &'a dyn Fn(HarnessSpec) -> Arc<dyn Harness>;

/// The production seam: build the runner the spec names. Mirrors Go `defaultRunnerFactory`
/// (`claude.New`), generalized over [`HarnessKnobs`]'s variants.
///
/// Matched exhaustively with no wildcard arm on purpose: a third harness must stop this function
/// compiling rather than silently resolve to claude. (Until STUDIO-902 there was one variant and
/// this destructured it directly, for the same reason.)
fn default_runner_factory(spec: HarnessSpec) -> Arc<dyn Harness> {
    match spec.knobs {
        HarnessKnobs::Claude(cc) => Arc::new(claude::Runner::new(cc)),
        HarnessKnobs::Opencode(oc) => Arc::new(opencode::Runner::new(oc)),
    }
}

/// Maps a (materialized) [`Config`] onto an [`Runner`] via the named backend, returning
/// [`OrchestratorError::UnsupportedBackend`] for any backend this build does not implement. Both the
/// top-level legacy runner and every per-project runner route through this single switch.
/// `"claude"` and — since STUDIO-902 — `"opencode"` are implemented; `"codex"` is recognized by
/// config validation and still has no runner, which is the split this module's top-of-file doc and
/// `implemented_backends_are_known_harness_names` below keep deliberate rather than accidental.
/// Mirrors Go `runnerForBackend`.
fn runner_for_backend(
    cfg: &Config,
    new_runner: RunnerFactory<'_>,
) -> Result<Arc<dyn Harness>, OrchestratorError> {
    match cfg.agent.backend.as_str() {
        "claude" | "opencode" => Ok(new_runner(harness_spec_from_cfg(cfg))),
        other => Err(OrchestratorError::UnsupportedBackend(other.to_string())),
    }
}

/// The `agent.backend` names [`runner_for_backend`] can actually build. A superset of what any one
/// installation uses: every one of these gets a runner in [`runners_by_harness`] so a teammate
/// profile can name one (STUDIO-902). `codex` is absent because no runner exists for it — which is
/// the same "recognized by config, not implemented here" split `rhapsody_config::HARNESS_NAMES`
/// documents, and `implemented_backends_are_known_harness_names` pins.
pub(crate) const IMPLEMENTED_BACKENDS: &[&str] = &["claude", "opencode"];

/// Whether THIS build can actually run `name` — the membership test `spawn_worker` makes against
/// a resolved project's runner pool (built from [`IMPLEMENTED_BACKENDS`]) when a teammate profile
/// names a harness (STUDIO-902). Exposed so `rhapsodyd teams show` can report a harness the
/// dispatcher would silently fall back from, rather than printing it as though it would run
/// (STUDIO-903). `""` is not implemented on purpose: an empty harness means "inherit
/// `agent.backend`", which dispatch resolves before it ever asks this question.
pub fn harness_is_implemented(name: &str) -> bool {
    IMPLEMENTED_BACKENDS.contains(&name)
}

/// Builds one runner per [`IMPLEMENTED_BACKENDS`] entry, so a dispatch can SELECT a harness without
/// constructing anything (STUDIO-902).
///
/// Every runner is built, not only the configured one, because construction is pure — `Runner::new`
/// for both backends just materializes defaults — while doing it at dispatch would mean holding the
/// `Config` and the factory past the reload that produced them. ⚠️ Building an opencode runner on an
/// installation that never configured one is harmless precisely because nothing is provisioned
/// until `start_session`, which is also where a missing credential is refused.
///
/// This is deliberately SELECTION over a fixed set, not slice 4's resolution chain: see
/// `rhapsody_config::profiles`'s module doc for exactly what is deferred.
fn runners_by_harness(
    cfg: &Config,
    new_runner: RunnerFactory<'_>,
) -> BTreeMap<String, Arc<dyn Harness>> {
    let mut out = BTreeMap::new();
    for name in IMPLEMENTED_BACKENDS {
        let mut c = cfg.clone();
        (*name).clone_into(&mut c.agent.backend);
        out.insert((*name).to_string(), new_runner(harness_spec_from_cfg(&c)));
    }
    out
}

/// Wraps [`claude_config_from_cfg`]'s (untouched) mapping in a [`HarnessSpec`] (STUDIO-900).
/// `model`/`provider` are `None`: Claude's equivalent values already live inside the `knobs` block
/// this builds, and nothing here resolves them independently yet — that is slice 4's dispatch-time
/// resolution chain (design §4.1), deliberately not done by this slice (see `crates/agent/src/harness.rs`'s
/// module doc). Because this is the ONLY place a [`HarnessSpec`] is built, and it wraps the exact
/// [`claude::Config`] the pre-STUDIO-900 code passed straight to [`claude::Runner::new`], the argv
/// [`claude::args::build_args`] produces from it is unchanged.
fn harness_spec_from_cfg(cfg: &Config) -> HarnessSpec {
    if cfg.agent.backend == "opencode" {
        return HarnessSpec {
            harness: HarnessId::Opencode,
            model: None,
            provider: None,
            knobs: HarnessKnobs::Opencode(opencode_config_from_cfg(cfg)),
        };
    }
    HarnessSpec {
        harness: HarnessId::Claude,
        model: None,
        provider: None,
        knobs: HarnessKnobs::Claude(claude_config_from_cfg(cfg)),
    }
}

/// Maps a [`Config`]'s `opencode`/tracker/workspace knobs onto an [`opencode::Config`] (STUDIO-902),
/// the counterpart of [`claude_config_from_cfg`] and built from the same single mapping for both
/// the top-level and per-project runners.
///
/// ⚠️ Note what is NOT carried across from the claude mapping: there is no `billing_guard` (Claude's
/// guard forces subscription billing off an `apiKeySource` signal opencode does not emit, and this
/// backend exists to bill a different provider on purpose) and no `permission_mode`/`allowed_tools`
/// (opencode has one `--auto` approval boolean instead). `tracker_api_key` IS carried: withholding
/// the Linear credential from the agent is not a billing decision.
fn opencode_config_from_cfg(cfg: &Config) -> opencode::Config {
    opencode::Config {
        command: cfg.opencode.command.clone(),
        model: cfg.opencode.model.clone(),
        variant: cfg.opencode.variant.clone(),
        agent: cfg.opencode.agent.clone(),
        auto_approve: cfg.opencode.auto_approve,
        workspace_root: cfg.workspace.root.clone(),
        turn_timeout: Duration::from_millis(cfg.opencode.turn_timeout_ms.max(0) as u64),
        extra_args: cfg.opencode.extra_args.clone(),
        tracker_api_key: cfg.tracker.api_key.clone(),
        inject_mcp: cfg.mcp.enabled,
        daemon_bin: daemon_bin_path(),
        workflow_path: cfg.workflow_path.clone(),
        state_root: cfg.opencode.state_root.clone(),
        auth_source: cfg.opencode.auth_source.clone(),
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
        // binary is the running daemon's own path; the workflow path lets the child `<daemon> mcp`
        // resolve the SAME server.port.
        inject_mcp: cfg.mcp.enabled,
        daemon_bin: daemon_bin_path(),
        workflow_path: cfg.workflow_path.clone(),
    }
}

/// Returns the running daemon binary's absolute path (the injected MCP server's `command`). On the
/// rare failure it logs and returns `""` so injection is skipped rather than pointing at a bad
/// command. Mirrors Go `symphonyBinPath` (`os.Executable`).
fn daemon_bin_path() -> String {
    match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "mcp injection: could not resolve daemon binary path; skipping injection"
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

/// The stall timeout of the backend a config NAMES, not of claude unconditionally.
///
/// ⚠️ This function exists because the pluggable-harnesses design predicted the exact bug it fixes.
/// §1.4 ("The stall-timeout gate is latent, not live") records that the old code read
/// `cfg.claude.stall_timeout_ms` behind an `if backend == "claude"` with a `Duration::ZERO` else —
/// unreachable while claude was the only implemented backend, and, in the record's own words, "it
/// becomes one the instant a second backend is admitted: stall detection would silently be off for
/// it." STUDIO-902 is the ticket that admits one. The per-project path had the mirror-image defect
/// (§1.4's second paragraph): it read `rp.eff.claude.stall_timeout_ms` unconditionally, so an
/// opencode project would have been governed by claude's number rather than its own.
///
/// Claude's value is unchanged: `materialize_config` assigns `c.claude = eff.claude`, so the
/// per-project arm below reads the same field the old line did.
///
/// An unimplemented backend keeps `Duration::ZERO`, which `runner_for_backend` rejects before this
/// is ever reached. A harness-agnostic liveness signal is design §7.3 / slice 6; this only stops
/// the existing subprocess-shaped one from silently not applying.
///
/// This is the per-CONFIG half. A run whose teammate profile names a harness the config's own
/// backend is not resolves per RUN — see [`stall_timeout_for_harness`], which
/// `Orchestrator::stall_timeout_for` consults first.
fn stall_timeout_for(cfg: &Config) -> Duration {
    let ms = stall_timeout_ms_for(cfg, &cfg.agent.backend).unwrap_or(0);
    Duration::from_millis(ms.max(0) as u64)
}

/// The stall-timeout knob of one named backend, or `None` when this build implements no such
/// backend. The single place the knob's per-backend location is spelled, so the config-level and
/// run-level resolutions below cannot drift apart.
fn stall_timeout_ms_for(cfg: &Config, backend: &str) -> Option<i64> {
    match backend {
        "claude" => Some(cfg.claude.stall_timeout_ms),
        "opencode" => Some(cfg.opencode.stall_timeout_ms),
        _ => None,
    }
}

/// The stall timeout of the harness a RUN was DISPATCHED on, when that harness differs from the
/// backend its config names — `None` otherwise, which leaves the precomputed
/// [`Effective::stall_timeout`] / [`ResolvedProject::stall_timeout`] in force.
///
/// ⚠️ [`stall_timeout_for`] resolves per CONFIG, which is only the whole answer while every run of a
/// config shares its backend. STUDIO-902 admits a second harness one PROFILE at a time
/// ([`Effective::agents`]), so the headline arrangement is `agent.backend: claude` with ONE
/// teammate's profile naming `harness: opencode` — and there the run's harness and its config's
/// backend legitimately disagree. Resolving only per config would then govern that opencode run by
/// `claude.stall_timeout_ms` and leave `opencode.stall_timeout_ms` consulted by nothing: the same
/// "knob that silently does nothing" design §1.4 warned about, one level down. Both default to
/// 300000, so this changes nothing until an operator tunes one — which is exactly the operator who
/// would be misled.
///
/// An unrecognized harness name yields `None`, the same skip-don't-refuse posture
/// `Loop::spawn_worker` takes when a profile names a harness this build has no runner for: the run
/// is still perfectly runnable on the default, so it keeps the default's liveness window rather than
/// losing stall detection to a typo.
pub(crate) fn stall_timeout_for_harness(cfg: &Config, harness: &str) -> Option<Duration> {
    if harness.is_empty() || harness == cfg.agent.backend {
        return None;
    }
    let ms = stall_timeout_ms_for(cfg, harness)?;
    Some(Duration::from_millis(ms.max(0) as u64))
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
    let top_agents = runners_by_harness(cfg, new_runner);

    // The top-level (legacy/nil-rp) stall timeout; per-project ones are computed the same way from
    // each project's materialized config below.
    let stall = stall_timeout_for(cfg);

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
        let project_agents = runners_by_harness(&mcfg, new_runner);
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
            promote_from_states: normalize_set(&rp.eff.promote_from_states),
            claim_mode: rp.eff.claim_mode.clone(),
            model: rp.eff.claude.model.clone(),
            stall_timeout: stall_timeout_for(&mcfg),
            mcfg,
            agent: project_runner,
            agents: project_agents,
            workspace: Arc::clone(&wm),
        });
    }

    Ok(Effective {
        tracker: Arc::clone(&tr),
        workspace: Arc::clone(&wm),
        agent: runner,
        agents: top_agents,
        prompt_tmpl: cfg.prompt_template.clone(),
        prompt_file: cfg.prompt_file.clone(),
        git_flow: cfg.git_flow.clone(),
        workspace_mode: top_eff.workspace_mode.clone(),
        pr_label: cfg.pr_label.clone(),
        dependency_mode: top_eff.dependency_mode.clone(),
        dep_mode_prompt_file: top_eff.dep_mode_prompt_file.clone(),
        promote_from_states: normalize_set(&top_eff.promote_from_states),
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
        // STUDIO-950: a positive value opts in; absent/zero/negative keep the shared pool (D2's
        // global half, opt-in by construction so an existing install schedules identically).
        max_concurrent_reviews: cfg.agent.max_concurrent_reviews.filter(|n| *n > 0),
        max_turns: cfg.agent.max_turns,
        // STUDIO-967: verbatim; the reader treats `0` (the default) as unlimited.
        max_run_tokens: cfg.agent.max_run_tokens,
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

    /// STUDIO-950: `agent.max_concurrent_reviews` reaches `Effective` as an opt-in review pool, and
    /// a non-positive value normalizes to `None` so every reader can treat `Some(n)` as `n > 0`.
    #[test]
    fn build_effective_carries_max_concurrent_reviews() {
        let absent = decode_cfg(CLAUDE_WF, "x");
        assert_eq!(
            build_effective(&absent)
                .expect("build")
                .max_concurrent_reviews,
            None,
            "absent ⇒ the shared `max_concurrent_agents` budget"
        );

        let set = decode_cfg(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nagent:\n  max_concurrent_agents: 4\n  max_concurrent_reviews: 2\n",
            "x",
        );
        assert_eq!(
            build_effective(&set).expect("build").max_concurrent_reviews,
            Some(2)
        );

        let zero = decode_cfg(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nagent:\n  max_concurrent_reviews: 0\n",
            "x",
        );
        assert_eq!(
            build_effective(&zero)
                .expect("build")
                .max_concurrent_reviews,
            None,
            "≤ 0 is unset, not a zero-slot pool that would starve every review"
        );
    }

    /// STUDIO-967: `agent.max_run_tokens` reaches `Effective` verbatim, and `0` (the default) stays
    /// `0` — the reader treats it as unlimited, so it must NOT be normalized into a finite bound.
    #[test]
    fn build_effective_carries_max_run_tokens() {
        assert_eq!(
            build_effective(&decode_cfg(CLAUDE_WF, "x"))
                .expect("build")
                .max_run_tokens,
            0,
            "absent ⇒ unlimited"
        );

        let set = decode_cfg(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nagent:\n  max_run_tokens: 2500000\n",
            "x",
        );
        assert_eq!(
            build_effective(&set).expect("build").max_run_tokens,
            2_500_000
        );
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

    // STUDIO-948: the config→gate seam end to end. DECODE keeps the operator's spelling verbatim;
    // the FIFTH GATE matches on `normalize_state`. This pins that `build_effective` folds the
    // configured value with `normalize_set` on BOTH the top-level and per-project paths. Drop either
    // `normalize_set(...)` and an operator's `Backlog` stays `"Backlog"`, matches no normalized issue
    // state, and dag silently promotes zero tickets — with no warning, because the boot WARN only
    // fires when the key is UNSET. The promote tests assign already-lowercased sets directly, so this
    // is the only test that pins the folding of a value that came from YAML.
    #[test]
    fn build_effective_normalizes_promote_from_states() {
        const WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  dependency_mode: dag
  promote_from_states: [Backlog]
projects:
  - slugs: [proj]
    promote_from_states: [  Staged  ]
agent:
  backend: claude
claude:
  command: claude
";
        let cfg = decode_cfg(WF, "body");
        let eff = build_effective(&cfg).expect("build_effective");
        assert!(
            eff.promote_from_states.contains("backlog"),
            "top-level must be normalized (got {:?})",
            eff.promote_from_states
        );
        assert!(
            !eff.promote_from_states.contains("Backlog"),
            "the raw spelling must not survive, or the normalized issue state never matches (got {:?})",
            eff.promote_from_states
        );
        let p = eff
            .projects
            .iter()
            .find(|p| p.slug == "proj")
            .expect("resolved project");
        assert!(
            p.promote_from_states.contains("staged"),
            "per-project override must be normalized too (got {:?})",
            p.promote_from_states
        );
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
        // ⚠️ Since STUDIO-902 the factory is called for the ALTERNATE harness too — `build_effective`
        // pre-builds one runner per implemented backend so a teammate profile can select one
        // (`runners_by_harness`). Only the claude specs are collected here; the opencode ones are
        // asserted to be well-formed and then built, so this test keeps measuring the thing it was
        // written to measure (which claude::Config each project's runner gets) rather than
        // accidentally measuring the new pre-build.
        let factory = |spec: HarnessSpec| -> Arc<dyn Harness> {
            match spec.knobs {
                HarnessKnobs::Claude(cc) => {
                    assert_eq!(spec.harness, HarnessId::Claude);
                    got_configs.borrow_mut().push(cc.clone());
                    Arc::new(claude::Runner::new(cc))
                }
                HarnessKnobs::Opencode(oc) => {
                    assert_eq!(spec.harness, HarnessId::Opencode);
                    Arc::new(opencode::Runner::new(oc))
                }
            }
        };
        let eff = build_effective_with_runner(&cfg, &factory).expect("build_effective");

        assert_eq!(eff.projects.len(), 2, "expected 2 resolved projects");
        assert_eq!(
            got_configs.borrow().len(),
            // One per `runner_for_backend` call (top-level + per project) PLUS one per
            // `runners_by_harness` call, which builds a claude runner for every one of those too.
            (eff.projects.len() + 1) * 2,
            "factory: top-level + one per resolved project, each also pre-built by harness"
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
        // ⚠️ Since STUDIO-902 the factory is called for the ALTERNATE harness too — `build_effective`
        // pre-builds one runner per implemented backend so a teammate profile can select one
        // (`runners_by_harness`). Only the claude specs are collected here; the opencode ones are
        // asserted to be well-formed and then built, so this test keeps measuring the thing it was
        // written to measure (which claude::Config each project's runner gets) rather than
        // accidentally measuring the new pre-build.
        let factory = |spec: HarnessSpec| -> Arc<dyn Harness> {
            match spec.knobs {
                HarnessKnobs::Claude(cc) => {
                    assert_eq!(spec.harness, HarnessId::Claude);
                    got_configs.borrow_mut().push(cc.clone());
                    Arc::new(claude::Runner::new(cc))
                }
                HarnessKnobs::Opencode(oc) => {
                    assert_eq!(spec.harness, HarnessId::Opencode);
                    Arc::new(opencode::Runner::new(oc))
                }
            }
        };
        let eff = build_effective_with_runner(&cfg, &factory).expect("build_effective");
        assert_eq!(eff.projects.len(), 1, "expected exactly 1 resolved project");
        assert_eq!(
            got_configs.borrow().len(),
            // Top-level + single project, each built twice: once by `runner_for_backend` and once
            // by STUDIO-902's `runners_by_harness` pre-build (see the factory comment above).
            4,
            "factory: top-level + single project, each also pre-built by harness"
        );
        for cc in got_configs.borrow().iter() {
            assert_eq!(cc.command, "claude", "runner built with top-level command");
            assert_eq!(cc.model, "", "runner built with top-level (empty) model");
        }
    }

    /// STUDIO-902: every implemented backend gets a runner at effective-build time, on the
    /// top-level effective AND on every resolved project, so a routed teammate's profile can name
    /// one without anything being constructed at dispatch.
    ///
    /// ⚠️ Also pins the property that keeps this additive: `agent` — what every dispatch naming no
    /// harness uses — stays the CONFIGURED backend's runner, not an arbitrary member of the pool.
    #[test]
    fn every_implemented_backend_gets_a_prebuilt_runner() {
        let cfg = decode_cfg(CLAUDE_WF, "Do {{ issue.identifier }}.");
        let eff = build_effective(&cfg).expect("build_effective");

        for name in IMPLEMENTED_BACKENDS {
            assert!(
                eff.agents.contains_key(*name),
                "top-level pool is missing {name}: {:?}",
                eff.agents.keys().collect::<Vec<_>>()
            );
            for p in &eff.projects {
                assert!(
                    p.agents.contains_key(*name),
                    "project {} is missing {name}",
                    p.slug
                );
            }
        }
        assert!(
            !eff.agents.contains_key("codex"),
            "codex is recognized by config but has no runner; it must not appear here"
        );
        assert_eq!(eff.agents.len(), IMPLEMENTED_BACKENDS.len());
    }

    /// ⚠️ Design §1.4, made live by STUDIO-902. The stall timeout must come from the backend the
    /// config NAMES. Before this, an opencode installation got `Duration::ZERO` at the top level —
    /// stall detection silently off — and its projects got CLAUDE's number, which is a different
    /// wrong answer from the same cause.
    ///
    /// Both arms are asserted with DIFFERENT values so neither can pass by coincidence.
    #[test]
    fn stall_timeout_follows_the_configured_backend() {
        const WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo]
agent:
  backend: opencode
claude:
  stall_timeout_ms: 111000
opencode:
  command: /abs/opencode
  stall_timeout_ms: 222000
";
        let cfg = decode_cfg(WF, "body");
        let eff = build_effective(&cfg).expect("build_effective");
        assert_eq!(
            eff.stall_timeout,
            Duration::from_millis(222_000),
            "an opencode installation must use opencode's stall timeout, not ZERO and not claude's"
        );
        for p in &eff.projects {
            assert_eq!(
                p.stall_timeout,
                Duration::from_millis(222_000),
                "project {} read the wrong backend's stall timeout",
                p.slug
            );
        }

        // And claude's own value is untouched by the change.
        let mut claude_cfg = cfg.clone();
        claude_cfg.agent.backend = "claude".to_string();
        let eff = build_effective(&claude_cfg).expect("build_effective");
        assert_eq!(eff.stall_timeout, Duration::from_millis(111_000));
        for p in &eff.projects {
            assert_eq!(p.stall_timeout, Duration::from_millis(111_000));
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

    /// `runner_for_backend` hardcodes `"claude"` as the one backend it can build a [`Runner`]
    /// for. That name must stay a member of the shared harness registry
    /// (`rhapsody_config::HARNESS_NAMES`, STUDIO-893 §1.3/§4.3) — the registry is what `validate`
    /// consults to decide which names even reach `build_effective`, so if `claude` ever dropped
    /// out of it, config validation would reject every config before this function's one working
    /// arm could run. The coupling this pins is by convention, not by a shared symbol: if
    /// `runner_for_backend`'s one implemented arm were ever renamed away from the literal
    /// `"claude"`, this assertion would keep passing on the registry's `"claude"` entry while the
    /// renamed arm silently stopped matching it — the behavioural tests above (`build_effective`
    /// selecting a claude runner) are what would actually catch that.
    #[test]
    fn implemented_backends_are_known_harness_names() {
        for name in ["claude", "opencode"] {
            assert!(
                rhapsody_config::HARNESS_NAMES.contains(&name),
                "runner_for_backend implements {name:?}, so it must remain in the shared registry"
            );
        }
    }

    /// [`harness_is_implemented`] is the arbiter `teams show` consults to decide whether to mark a
    /// profile's harness (STUDIO-903), so it must agree with what the dispatch pool actually
    /// builds — otherwise the report could mark a runnable harness unavailable, or pass `codex`
    /// off as one that will run. `""` is deliberately not implemented: it is the inherit sentinel,
    /// resolved to `agent.backend` before dispatch ever consults the pool.
    #[test]
    fn harness_is_implemented_matches_the_dispatch_pool() {
        for name in IMPLEMENTED_BACKENDS {
            assert!(harness_is_implemented(name), "{name:?} has a runner");
        }
        assert!(!harness_is_implemented("codex"), "codex has no runner");
        assert!(
            !harness_is_implemented(""),
            "the empty name is the inherit sentinel"
        );
    }

    /// STUDIO-978: the console derives a run's event fidelity and steering from
    /// `rhapsody_agent::harness_id_for_name`, which is a SECOND string→harness map living in the
    /// agent crate. It must cover every harness this crate's dispatch pool builds, or a newly
    /// added backend would silently render as "unknown" (no fidelity, composer shown) while
    /// actually running. Pin the two together at the pool's own edge so adding a backend to
    /// `IMPLEMENTED_BACKENDS` without the name map reds here rather than drifting in production.
    #[test]
    fn the_name_map_covers_every_implemented_backend() {
        for name in IMPLEMENTED_BACKENDS {
            assert!(
                rhapsody_agent::harness_id_for_name(name).is_some(),
                "{name:?} is in the dispatch pool but the console's name map cannot address it"
            );
        }
        assert_eq!(rhapsody_agent::harness_id_for_name("codex"), None);
        assert_eq!(rhapsody_agent::harness_id_for_name(""), None);
    }

    /// `validate`'s `UnsupportedAgentBackend` check used to hardcode its own notion of "which
    /// backend names exist" (design record §1.3); STUDIO-893 makes it read
    /// `rhapsody_config::HARNESS_NAMES` instead. This is a SOURCE pin, not a behavioural one: a
    /// black-box test cannot distinguish "reads the shared registry" from "hardcodes its own copy
    /// that happens to agree today." The needle is assembled at run time on purpose: `include_str!`
    /// pulls in this file too, so a needle spelled as a literal here would match its own source
    /// (the idiom `run.rs`/`runmerge.rs` already use for wiring no run-time assertion can reach).
    ///
    /// `runner_for_backend` in THIS file is deliberately not pinned the same way, and does not
    /// appear in the scanned source: per this module's top-of-file doc, "recognized by config"
    /// and "implemented by this build" are different questions on purpose (Go's own reference
    /// keeps `ValidateDispatch` and `runnerForBackend` separate too), so there is no production
    /// reference to the registry here for a source pin to find. The regression that matters on
    /// this side — that the one name this function implements never falls out of the registry
    /// `validate` reads — is `implemented_backend_is_a_known_harness_name` above instead.
    #[test]
    fn config_validator_reads_the_shared_harness_registry() {
        let needle = format!("{}_{}", "HARNESS", "NAMES");

        let validate_src = include_str!("../../config/src/validate.rs");
        assert!(
            validate_src.contains(&needle),
            "config::validate no longer references the shared harness registry"
        );
    }
}
