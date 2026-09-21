//! reload — parity port of Go `internal/orchestrator/reload.go` (WORKFLOW.md load + validation +
//! hot-reload semantics).
//!
//! [`reload_from_disk`](Orchestrator::reload_from_disk) loads, decodes, resolves, validates, and
//! builds the effective config, swapping `o.eff` only on FULL success; [`on_reload`](Orchestrator::on_reload)
//! re-reads on a watch event and keeps the last-good config on failure (never crashes);
//! [`start_watch`](Orchestrator::start_watch) watches the workflow file and posts [`Event::Reload`] on
//! change.
//!
//! Deviation from Go: Go's `startWatch` uses `fsnotify` (inotify) for instant change detection. To
//! avoid a new dependency (the P5 plan bounds the dep set), the Rust port polls the file's mtime on a
//! short interval — the OBSERVABLE behavior (config hot-reloads on change) is preserved, with a small
//! (≤ [`WATCH_POLL_INTERVAL`]) detection latency. Recorded in the PR body.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rhapsody_config::workflow::{self, Definition, WorkflowError};
use rhapsody_config::{ConfigError, ValidationError, decode, resolve, validate};

use crate::control_loop::{CancelWait, DEFAULT_RETENTION_DAYS, Event};
use crate::effective::build_effective;
use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;
use crate::warnings::project_warn_inputs;

/// The mtime-poll cadence for the workflow watcher (Go's fsnotify is instant; see the module docs).
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A workflow (re)load failure: any stage of the load → decode → resolve → validate → build pipeline.
/// Mirrors the untyped `error` Go `reloadFromDisk` returns (the Display string is the observable
/// contract at startup / on the config HTTP endpoint).
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    /// `workflow.Load` — the file is missing / unreadable / has malformed front matter.
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    /// `config.Decode` / `config.Resolve` — the front matter decodes/resolves to an invalid config.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// `config.ValidateDispatch` — dispatch preflight validation (missing api_key / slug, unsupported
    /// backend, bad git_flow / workspace_mode).
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// `buildEffective` — building the live deps failed (e.g. an unsupported `agent.backend` the
    /// validate step admits but the backend gate rejects, or a workspace-manager construction error).
    #[error(transparent)]
    Effective(#[from] crate::OrchestratorError),
}

/// The identity of the ACTIVE review budget a reload is switching to (STUDIO-950): the separate
/// review pool when `agent.max_concurrent_reviews` is set, otherwise the shared
/// `max_concurrent_agents` pool. Comparing this before and after a reload is what tells whether a
/// retained capacity hold still describes the pool this daemon schedules reviews from — a hold is a
/// statement about a pool, not about a duration, so a reload that changes the pool refutes it.
///
/// The `bool` distinguishes "separate review pool" from "shared pool" rather than collapsing to a
/// bare number, because the two differ in composition even at equal capacity: in shared mode every
/// running run holds the budget, in separate mode only ticketless reviews do.
fn active_review_budget(eff: &crate::effective::Effective) -> (bool, i64) {
    match eff.max_concurrent_reviews {
        Some(reviews) => (true, reviews),
        None => (false, eff.max_concurrent),
    }
}

/// `filepath.Dir` for the workflow path: the parent directory, or `"."` for a bare filename (matching
/// Go `filepath.Dir`, which never returns an empty string).
fn workflow_dir(path: &str) -> String {
    match Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    }
}

/// Runs the daemon's load pipeline (Decode → Resolve → ValidateDispatch → buildEffective) on a
/// candidate `def` resolved against `workflow_path`'s directory, WITHOUT applying it — the shared
/// engine behind [`Orchestrator::validate_config`] + [`ControlHandle::validate_config`]. Mirrors Go
/// `ValidateConfig`.
fn validate_config_at(workflow_path: &str, def: &Definition) -> Result<(), ReloadError> {
    let cfg = decode(def)?;
    let mut cfg = resolve(cfg, &workflow_dir(workflow_path))?;
    validate(&mut cfg)?;
    build_effective(&cfg)?;
    Ok(())
}

impl ControlHandle {
    /// The daemon's off-loop config-validation for `POST /api/v1/config` — the [`ControlHandle`]
    /// mirror of [`Orchestrator::validate_config`]. Runs the SAME load pipeline against the handle's
    /// workflow path WITHOUT applying it, so the endpoint rejects exactly what a hot-reload would.
    /// Mirrors Go `ValidateConfig`.
    pub fn validate_config(&self, def: &Definition) -> Result<(), ReloadError> {
        validate_config_at(self.workflow_path(), def)
    }
}

impl Orchestrator {
    /// Loads, decodes, resolves, validates, and builds the effective config, swapping `o.eff` only on
    /// full success (upstream §6.2). Mirrors Go `reloadFromDisk`.
    pub(crate) fn reload_from_disk(&mut self) -> Result<(), ReloadError> {
        let def = workflow::load(Path::new(&self.workflow_path))?;
        let cfg = decode(&def)?;
        let mut cfg = resolve(cfg, &workflow_dir(&self.workflow_path))?;
        validate(&mut cfg)?;
        // Stamp the workflow path so the claude backend can inject a `symphony mcp` server pointing at
        // the SAME workflow (INF-473); Decode/Resolve only know the dir.
        cfg.workflow_path = self.workflow_path.clone();
        let eff = build_effective(&cfg)?;

        // STUDIO-948: an enabled dag/graphite scope with `promote_from_states` unset must say so at
        // boot (and on reload) — the safety-critical default treats the operator's ENTIRE backlog as
        // ready work, which is how STUDIO-749 was promoted. Emit toward the freshly-built effective
        // before it moves.
        crate::promote::warn_unset_promote_from_states(&eff);

        // Capture the account-level tracker + resolved key for the read-only Linear endpoints (INF-224)
        // + the warning resolver inputs from the freshly-built effective BEFORE moving it into
        // `self.eff` (so there is no re-borrow / fallible unwrap after the swap).
        let tracker = Arc::clone(&eff.tracker);
        // Every ENABLED project's slug-bound tracker, in the poll loop's own order (STUDIO-671).
        // The top-level `tracker` above is NOT a substitute for these: in the `projects:` config
        // form it is bound to a `tracker.project_slug` that validation allows to be empty, so it
        // sees none of the daemon's work. Paused projects are filtered here for the same reason the
        // poll loop skips them — a project nothing dispatches from has nothing to triage either.
        // Each client's own slug rides along (STUDIO-677) so a WRITE can pick the one bound to a
        // given project; triage, which sweeps all of them, still reads the clients alone.
        let project_trackers: Vec<crate::reads::ProjectTracker> = eff
            .projects
            .iter()
            .filter(|p| !p.disabled)
            .map(|p| crate::reads::ProjectTracker {
                slug: p.slug.clone(),
                tracker: Arc::clone(&p.tracker),
            })
            .collect();
        // The per-project facts the manager's room reader files a review ticket with (STUDIO-678),
        // in the SAME order and under the same filter as the trackers above — the reader indexes
        // one by the other, so an entry that drifted would file into another project's state.
        //
        // `create_state` is `Orchestrator::quorum_create_state`'s answer, resolved here instead of
        // there because this reader holds no orchestrator: the project's FIRST configured active
        // state, per-project ⊕ top-level exactly as every other override resolves.
        let project_facts: Vec<crate::reads::ProjectFacts> = eff
            .projects
            .iter()
            .filter(|p| !p.disabled)
            .map(|p| {
                let project = cfg.projects.iter().find(|c| c.slugs.contains(&p.slug));
                let (pr_owner, pr_repo) = crate::ghsummons::parse_repo(&p.repo).unwrap_or_default();
                crate::reads::ProjectFacts {
                    create_state: rhapsody_config::effective_for(&cfg, project)
                        .active_states
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                    pr_owner,
                    pr_repo,
                }
            })
            .collect();
        // Snapshot the state sets the selection gate filters by, BEFORE `eff` moves into `self`.
        let states = crate::dispatch::DispatchStates {
            active: eff.active_states.clone(),
            terminal: eff.terminal_states.clone(),
            review: eff.review_states.clone(),
            canceled: eff.canceled_states.clone(),
        };
        let inputs = project_warn_inputs(&eff);
        let checker = self.prompt_file_checker_for(&eff);
        // STUDIO-950: the capacity holds name the budget that deferred each round, and that record
        // deliberately outlives the tick that wrote it (a pull request the watcher's rotating cursor
        // did not revisit keeps its hold). So a reload that changes the ACTIVE review budget —
        // setting, changing or removing `agent.max_concurrent_reviews`, or (while the key is unset)
        // changing `max_concurrent_agents` — makes every retained hold a statement about a pool this
        // daemon no longer schedules reviews from. Drop them here, or reconciliation would keep
        // telling an operator the superseded budget still blocks a review after scheduling has
        // already moved it to the new pool, until the pull request is revisited or the TTL expires.
        // A reload that leaves the active review budget alone keeps the holds, which is the whole
        // point of the per-pull-request refresh.
        let old_review_budget = self.eff.as_ref().map(active_review_budget);
        let new_review_budget = active_review_budget(&eff);
        self.eff = Some(eff);
        if old_review_budget != Some(new_review_budget) {
            self.review_capacity_held.clear();
        }
        self.set_reads_target(Arc::clone(&tracker), cfg.tracker.api_key.clone());
        // The project trackers and the same reload's dispatchable-state sets, published TOGETHER
        // under one write (STUDIO-672). Together because the off-loop triage task reads them as a
        // pair and acts on the pair; from this reload rather than re-derived there so triage and
        // the selection gate can never disagree about which tickets are work.
        self.set_reads_triage_snapshot(crate::reads::TriageSnapshot {
            trackers: project_trackers,
            states,
            facts: project_facts,
            summon_token: cfg.tracker.summon_token.clone(),
        });
        // Resolve configured project slugs against Linear + flag any missing repo-relative prompt_file,
        // best-effort and OFF the control task (a no-op on the direct-reload test path where `o.ctx` is
        // nil). INF-277 / INF-279.
        self.refresh_project_warnings(inputs, Some(tracker), checker);

        // Mirror storage.retention_days into the atomic the prune scheduler reads (default 30 when
        // unset). Done on every (re)load so a hot-reloaded value applies.
        let retention = cfg.storage.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
        self.retention_days.store(retention, Ordering::Relaxed);
        // Mark retention as loaded so the prune scheduler's startup cycle stops using the New default.
        self.retention_loaded.store(true, Ordering::Relaxed);
        // Mirror polling.pr_state_interval_ms into the atomic the off-loop review watcher reads
        // (STUDIO-974). Stored on every (re)load so a hot-reloaded cadence applies. `<= 0` falls back
        // to the 120s default rather than a zero-millisecond busy loop.
        let interval = if cfg.polling.pr_state_interval_ms > 0 {
            cfg.polling.pr_state_interval_ms
        } else {
            rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS
        };
        self.pr_state_interval_ms.store(interval, Ordering::Relaxed);
        Ok(())
    }

    /// Runs the SAME validation pipeline as a hot-reload (Decode → Resolve → ValidateDispatch →
    /// buildEffective) on a candidate definition WITHOUT applying it, so the config HTTP endpoint can
    /// reject anything the daemon would reject on reload. Does not mutate orchestrator state. Mirrors
    /// Go `ValidateConfig`.
    pub fn validate_config(&self, def: &Definition) -> Result<(), ReloadError> {
        validate_config_at(&self.workflow_path, def)
    }

    /// Re-runs dispatch preflight validation on the current effective config (upstream §6.3). A test-
    /// injected effective whose source config is already-valid is treated as valid. Mirrors Go
    /// `validate` (whose nil-cfg guard maps to "already validated at build"; the Rust `Effective`
    /// always carries a resolved cfg, so we re-validate a clone — the mutation the validator performs
    /// is discarded, only the pass/fail verdict matters). Mirrors Go `validate`.
    pub(crate) fn validate(&self) -> Result<(), ValidationError> {
        match self.eff.as_ref() {
            Some(eff) => {
                let mut cfg = eff.cfg.clone();
                validate(&mut cfg)
            }
            None => Ok(()),
        }
    }

    /// Re-reads the workflow on a watch event; on failure it keeps the last-good config and logs
    /// (never crashes, upstream §6.2). Rebuilds the github-summons source from the freshly-swapped
    /// `o.eff` so a hot-reloaded flag / summon_token takes effect without a restart. Mirrors Go
    /// `onReload`.
    pub(crate) fn on_reload(&mut self) {
        if let Err(e) = self.reload_from_disk() {
            tracing::error!(err = %e, "workflow reload failed; keeping last-good config");
            return;
        }
        self.gh_source = self.new_github_summon_source();
        tracing::info!(path = %self.workflow_path, "workflow reloaded");
    }

    /// Watches the workflow file and posts [`Event::Reload`] on change, until `ctx` is cancelled.
    /// Returns the watcher task handle (Go returns a stop `func()`; here the task self-terminates on
    /// `ctx` cancel and the handle lets the caller abort it too). A stat failure is non-fatal — the
    /// poll simply keeps trying. Mirrors Go `startWatch` (fsnotify → mtime polling; see the module docs).
    pub(crate) fn start_watch(&self, mut ctx: CancelWait) -> tokio::task::JoinHandle<()> {
        let path = self.workflow_path.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mtime = || {
                std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
            };
            let mut last = mtime();
            let mut interval = tokio::time::interval(WATCH_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = ctx.cancelled() => return,
                    _ = interval.tick() => {
                        let cur = mtime();
                        if cur != last {
                            last = cur;
                            if events.send(Event::Reload).is_err() {
                                return; // the loop is gone
                            }
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::orchestrator::Orchestrator;
    use crate::review::review_key;
    use crate::reviewwatch::CapacityHold;
    use crate::testsupport::{TempDir, capture_events};

    // A full WORKFLOW.md (front matter + prompt body) mirroring Go `effective_test.go`'s `claudeWF`.
    // Go uses `api_key: $ORCH_TEST_KEY` + `t.Setenv`; the Rust port uses a literal key so the test
    // never mutates process-global env (edition 2024's `set_var` is `unsafe` + racy across parallel
    // tests) — the reload assertions (`max_turns`, `validate`, `gh_source`) don't exercise expansion.
    const CLAUDE_WF: &str = "---
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
---
Do {{ issue.identifier }}.
";

    // Mirrors Go `ghenrich_loop_test.go`'s `summonsWF` (github_summons ON), literal key as above.
    const SUMMONS_WF: &str = "---
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  github_summons: true
repo: git@github.com:acme/widget.git
agent:
  backend: claude
claude:
  command: claude
---
Do {{ issue.identifier }}.
";

    /// An ENABLED `dag` mode with `promote_from_states` UNSET — the shape STUDIO-948 must warn about
    /// at boot (an operator running dag chose it, but silence about the unset key is how STUDIO-749
    /// got promoted). The companion DAG_PROMOTE_WF names the key and must be silent.
    const DAG_WF: &str = "---
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  dependency_mode: dag
agent:
  backend: claude
claude:
  command: claude
---
Do {{ issue.identifier }}.
";

    /// Same as [`DAG_WF`] but with the key set — no boot warning.
    const DAG_PROMOTE_WF: &str = "---
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  dependency_mode: dag
  promote_from_states:
    - Backlog
agent:
  backend: claude
claude:
  command: claude
---
Do {{ issue.identifier }}.
";

    /// Writes `body` to a fresh temp `WORKFLOW.md`, returning its path + the owning dir (kept alive
    /// for the test's duration). Mirrors Go `writeWorkflow`.
    fn write_workflow(body: &str) -> (String, TempDir) {
        let dir = TempDir::new();
        let path = dir.child("WORKFLOW.md");
        std::fs::write(&path, body).expect("write workflow");
        (path, dir)
    }

    // Mirrors Go `TestReloadFromDiskValid`.
    #[test]
    fn reload_from_disk_valid() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path);
        o.reload_from_disk().expect("reload");
        assert_eq!(o.eff.as_ref().expect("eff built").max_turns, 7);
        o.validate().expect("validate should pass on a good config");
    }

    // Mirrors Go `TestReloadInvalidKeepsLastGood`.
    #[test]
    fn reload_invalid_keeps_last_good() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        let good_max = o.eff.as_ref().unwrap().max_turns;
        // Overwrite with an invalid workflow (missing project_slug → validation fails).
        std::fs::write(
            &path,
            "---\ntracker:\n  kind: linear\n  api_key: tok\n---\nbody\n",
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_turns,
            good_max,
            "an invalid reload must keep the last-good effective config"
        );
    }

    /// STUDIO-974: the ticketless PR-state watcher's cadence hot-reloads with WORKFLOW.md. Unset ⇒
    /// the historical 120s default (byte-identical to before the key existed); a set value applies
    /// on `on_reload`; a non-positive value falls back to the default rather than a busy loop.
    #[test]
    fn reload_applies_the_pr_state_interval() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        assert_eq!(
            o.current_pr_state_interval_ms(),
            120_000,
            "an install that never writes the key keeps the pinned 120s watcher clock"
        );

        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  interval_ms: 1234",
                "  interval_ms: 1234\n  pr_state_interval_ms: 10000",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.current_pr_state_interval_ms(),
            10000,
            "a hot-reloaded cadence must apply without a restart"
        );

        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  interval_ms: 1234",
                "  interval_ms: 1234\n  pr_state_interval_ms: 0",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.current_pr_state_interval_ms(),
            120_000,
            "a non-positive cadence falls back to the default, not a zero-millisecond loop"
        );
    }

    // Mirrors Go `TestReloadAppliesNewConfig`.
    #[test]
    fn reload_applies_new_config() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        std::fs::write(&path, CLAUDE_WF.replace("max_turns: 7", "max_turns: 11")).unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_turns,
            11,
            "reload should apply the new config"
        );
    }

    /// STUDIO-950: `agent.max_concurrent_reviews` hot-reloads with the rest of WORKFLOW.md. Setting
    /// it, changing it, and removing it all take effect on `on_reload` without a restart — the
    /// knob's whole point is that an operator can tune the review pool live.
    #[test]
    fn reload_applies_a_changed_review_budget() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        assert_eq!(
            o.eff.as_ref().unwrap().max_concurrent_reviews,
            None,
            "absent on the first load ⇒ the shared budget"
        );

        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  max_concurrent_agents: 4\n",
                "  max_concurrent_agents: 4\n  max_concurrent_reviews: 1\n",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_concurrent_reviews,
            Some(1),
            "the key must hot-reload without a restart"
        );

        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  max_concurrent_agents: 4\n",
                "  max_concurrent_agents: 4\n  max_concurrent_reviews: 3\n",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_concurrent_reviews,
            Some(3),
            "a changed value must take effect"
        );

        std::fs::write(&path, CLAUDE_WF).unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_concurrent_reviews,
            None,
            "removing the key returns to the shared budget"
        );
    }

    /// STUDIO-950 (round 11): a capacity hold is a statement about the ACTIVE review budget, so a
    /// reload that changes that budget must refute every retained hold. The holds deliberately
    /// outlive the tick that wrote them (the watcher's rotating cursor may not revisit a held pull
    /// request for several ticks), so without this the reconciliation sweep keeps naming the old
    /// pool — the API, console, advisory and WARN all telling the operator a budget still blocks a
    /// review after scheduling has already moved it to a free pool.
    ///
    /// Mutation check: drop the `old_review_budget != Some(new_review_budget)` guard and every
    /// clearing assertion reds while the companion control test stays green.
    #[test]
    fn reload_clears_a_capacity_hold_when_the_review_budget_changes() {
        let hold = || CapacityHold {
            holders: 4,
            separate: false,
            recorded: chrono::Utc::now(),
        };
        let id = review_key("makewhatis", "rhapsody", 31, "alice");

        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        // A hold recorded against the shared pool ...
        o.review_capacity_held.insert(id.clone(), hold());

        // ... is refuted by a reload that turns on the separate review pool: reviews now draw
        // `max_concurrent_reviews`, which has a free slot, so the old shared-pool hold is superseded.
        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  max_concurrent_agents: 4\n",
                "  max_concurrent_agents: 4\n  max_concurrent_reviews: 1\n",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            o.eff.as_ref().unwrap().max_concurrent_reviews,
            Some(1),
            "the reload must move reviews to their own pool"
        );
        assert!(
            o.review_capacity_held.is_empty(),
            "a reload that changes the active review budget must drop the superseded hold"
        );

        // A reload that changes the separation VALUE also refutes it.
        o.review_capacity_held.insert(id.clone(), hold());
        std::fs::write(
            &path,
            CLAUDE_WF.replace(
                "  max_concurrent_agents: 4\n",
                "  max_concurrent_agents: 4\n  max_concurrent_reviews: 3\n",
            ),
        )
        .unwrap();
        o.on_reload();
        assert!(
            o.review_capacity_held.is_empty(),
            "raising the separate review budget supersedes a hold against the old value"
        );

        // Removing the key returns to the shared pool — also a different active budget.
        o.review_capacity_held.insert(id.clone(), hold());
        std::fs::write(&path, CLAUDE_WF).unwrap();
        o.on_reload();
        assert!(
            o.review_capacity_held.is_empty(),
            "removing the key returns to the shared pool and supersedes a separate-pool hold"
        );
    }

    // STUDIO-671: a `projects:` config with NO top-level `tracker.project_slug` — the shape
    // `config::validate` deliberately accepts, and the shape the daemon that wedged was running.
    // The account-level client is bound to that empty slug, so it is NOT a substitute for the
    // per-project ones: its candidate query filters `project.slugId == ""`, which Linear answers
    // with zero rows and no error. Triage read through it and found nothing, silently, forever.
    const MULTI_PROJECT_WF: &str = "---
tracker:
  kind: linear
  api_key: tok
  active_states: [Todo]
  terminal_states: [Done]
repo: git@github.com:acme/widget.git
projects:
  - slugs: [558008ab185c]
  - slugs: [beefcafe1234]
  - slugs: [dadfaced0001]
    enabled: false
agent:
  backend: claude
claude:
  command: claude
---
Do {{ issue.identifier }}.
";

    // The reload path must publish every ENABLED project's tracker, not just the account-level one.
    #[test]
    fn reload_publishes_every_enabled_project_tracker() {
        let (path, _dir) = write_workflow(MULTI_PROJECT_WF);
        let mut o = Orchestrator::new(path.clone());
        let control = o.control();
        assert!(
            control.reads_project_trackers().is_none(),
            "no config has loaded yet"
        );
        o.reload_from_disk().expect("reload");

        let eff = o.eff.as_ref().expect("eff built");
        assert_eq!(
            eff.cfg.tracker.project_slug, "",
            "the projects: form supplies the slugs; the top-level one is legitimately empty"
        );
        // The root cause, pinned: the account-level client is a DIFFERENT client from every
        // project's, so reading candidates through it can never see the daemon's work.
        for p in &eff.projects {
            assert!(
                !std::sync::Arc::ptr_eq(&p.tracker, &eff.tracker),
                "project {} must not be served by the slug-less account-level client",
                p.slug
            );
        }

        let published = control.reads_project_trackers().expect("config loaded");
        assert_eq!(
            published.len(),
            2,
            "both enabled projects are published; the paused one is not"
        );
        let enabled: Vec<&crate::effective::ResolvedProject> =
            eff.projects.iter().filter(|p| !p.disabled).collect();
        for (got, want) in published.iter().zip(enabled.iter()) {
            assert!(
                std::sync::Arc::ptr_eq(got, &want.tracker),
                "published trackers must be the projects' own clients, in poll order"
            );
        }

        // A hot-reload that un-pauses the third project republishes it.
        std::fs::write(
            &path,
            MULTI_PROJECT_WF.replace(
                "    enabled: false
",
                "",
            ),
        )
        .unwrap();
        o.on_reload();
        assert_eq!(
            control
                .reads_project_trackers()
                .expect("config loaded")
                .len(),
            3,
            "a reload must republish the live project set"
        );
    }

    // Mirrors Go `TestReloadRebuildsGitHubSource`.
    #[test]
    fn reload_rebuilds_github_source() {
        let (path, _dir) = write_workflow(CLAUDE_WF); // github_summons OFF
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");
        o.gh_source = o.new_github_summon_source(); // mirror Run's startup construction
        assert!(
            o.gh_source.is_none(),
            "gh_source should be None when github_summons is off"
        );
        // Hot-reload to github_summons ON.
        std::fs::write(&path, SUMMONS_WF).unwrap();
        o.on_reload();
        assert!(
            o.gh_source.is_some(),
            "onReload must rebuild gh_source after enabling github_summons"
        );
        // Hot-reload back to OFF — the source must drop again.
        std::fs::write(&path, CLAUDE_WF).unwrap();
        o.on_reload();
        assert!(
            o.gh_source.is_none(),
            "onReload must rebuild gh_source after disabling github_summons"
        );
    }

    /// The control for [`reload_clears_a_capacity_hold_when_the_review_budget_changes`]: a reload
    /// that does NOT change the active review budget keeps the holds, because a pull request the
    /// watcher's cursor did not revisit is still legitimately held and its annotation must not blink.
    ///
    /// Mutation check: clear unconditionally on every reload and this reds.
    #[test]
    fn reload_keeps_a_capacity_hold_when_the_review_budget_is_unchanged() {
        let (path, _dir) = write_workflow(CLAUDE_WF);
        let mut o = Orchestrator::new(path.clone());
        o.reload_from_disk().expect("reload");

        let id = review_key("makewhatis", "rhapsody", 31, "alice");
        o.review_capacity_held.insert(
            id.clone(),
            CapacityHold {
                holders: 4,
                separate: false,
                recorded: chrono::Utc::now(),
            },
        );

        std::fs::write(&path, CLAUDE_WF.replace("max_turns: 7", "max_turns: 11")).unwrap();
        o.on_reload();

        assert_eq!(
            o.eff.as_ref().unwrap().max_turns,
            11,
            "the reload must have applied"
        );
        assert!(
            o.review_capacity_held.contains_key(&id),
            "a reload that leaves the active review budget alone must keep the hold"
        );
    }

    // STUDIO-948: an enabled dag/graphite scope with `promote_from_states` UNSET must say so at boot
    // — one warning naming the key and the risk, in the same style as the INF-277 unmatched-slug
    // advisory. Silence here is how STUDIO-749 was promoted.
    #[test]
    fn reload_warns_when_dag_promote_from_states_unset() {
        let (path, _dir) = write_workflow(DAG_WF);
        let mut o = Orchestrator::new(path);
        let (res, events) = capture_events(|| o.reload_from_disk());
        res.expect("reload");
        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == "WARN" && e.message.contains("promote_from_states is unset"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "exactly one boot warning for the unset key under an enabled dag mode"
        );
        assert_eq!(
            warns[0].fields.get("project_slug").map(String::as_str),
            Some("proj")
        );
        assert_eq!(
            warns[0].fields.get("dependency_mode").map(String::as_str),
            Some("dag")
        );
    }

    // Naming the key silences the warning: the operator has told dag which states are staged work.
    #[test]
    fn reload_does_not_warn_when_promote_from_states_set() {
        let (path, _dir) = write_workflow(DAG_PROMOTE_WF);
        let mut o = Orchestrator::new(path);
        let (res, events) = capture_events(|| o.reload_from_disk());
        res.expect("reload");
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("promote_from_states is unset")),
            "a named promote_from_states must not warn"
        );
    }

    // A disabled-mode daemon promotes nothing, so there is no risk and no warning — the
    // disabled-is-noop invariant's diagnostic half.
    #[test]
    fn reload_does_not_warn_when_dependency_mode_disabled() {
        let (path, _dir) = write_workflow(CLAUDE_WF); // no dependency_mode => disabled
        let mut o = Orchestrator::new(path);
        let (res, events) = capture_events(|| o.reload_from_disk());
        res.expect("reload");
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("promote_from_states is unset")),
            "a disabled-mode daemon must not warn about promote_from_states"
        );
    }
}
