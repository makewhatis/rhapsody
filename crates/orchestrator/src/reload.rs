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

        // Capture the account-level tracker + resolved key for the read-only Linear endpoints (INF-224)
        // + the warning resolver inputs from the freshly-built effective BEFORE moving it into
        // `self.eff` (so there is no re-borrow / fallible unwrap after the swap).
        let tracker = Arc::clone(&eff.tracker);
        // Every ENABLED project's slug-bound tracker, in the poll loop's own order (STUDIO-671).
        // The top-level `tracker` above is NOT a substitute for these: in the `projects:` config
        // form it is bound to a `tracker.project_slug` that validation allows to be empty, so it
        // sees none of the daemon's work. Paused projects are filtered here for the same reason the
        // poll loop skips them — a project nothing dispatches from has nothing to triage either.
        let project_trackers: Vec<Arc<dyn rhapsody_tracker::Tracker>> = eff
            .projects
            .iter()
            .filter(|p| !p.disabled)
            .map(|p| Arc::clone(&p.tracker))
            .collect();
        // Snapshot the state sets the selection gate filters by, BEFORE `eff` moves into `self`.
        let states = crate::dispatch::DispatchStates {
            active: eff.active_states.clone(),
            terminal: eff.terminal_states.clone(),
            review: eff.review_states.clone(),
        };
        let inputs = project_warn_inputs(&eff);
        let checker = self.prompt_file_checker_for(&eff);
        self.eff = Some(eff);
        self.set_reads_target(Arc::clone(&tracker), cfg.tracker.api_key.clone());
        // The project trackers and the same reload's dispatchable-state sets, published TOGETHER
        // under one write (STUDIO-672). Together because the off-loop triage task reads them as a
        // pair and acts on the pair; from this reload rather than re-derived there so triage and
        // the selection gate can never disagree about which tickets are work.
        self.set_reads_triage_snapshot(project_trackers, states);
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
    use crate::testsupport::TempDir;

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
}
