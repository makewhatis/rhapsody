//! dockercleanup — removes the Docker containers, networks and volumes agent runs leave behind
//! (STUDIO-2316). Rhapsody-only; no Go counterpart.
//!
//! Agents start Postgres containers, compose projects and testcontainers inside their runs, and
//! nothing ever removed them: an operator's Mac carried 30 leftover containers (about 0.8 GiB of RAM,
//! some ten days old), and leftover `<ticket>_default` compose networks once exhausted Docker's
//! address pools until `docker network prune` ran. Three pieces close that:
//!
//! 1. **Labelling.** Every dispatched run's env carries `RHAPSODY_RUN_ID` and
//!    `COMPOSE_PROJECT_NAME=rhapsody-<run id>` (`rhapsody_agent::claude::append_compose_env`), and
//!    the turn-1 prompt asks for `--label rhapsody.run=$RHAPSODY_RUN_ID` on any `docker run`
//!    ([`PROMPT_HINT`]).
//! 2. **Workspace cleanup.** The workspace [`Manager`](rhapsody_workspace::Manager) awaits a
//!    [`removal_hook`] whenever an issue's worktree is removed — the point the run's work is over,
//!    not each turn end (a run resumes across turns). It removes every container labelled with one of
//!    that issue's runs or carrying that run's compose project, then the project's networks and
//!    volumes when unused ([`cleanup_runs`]).
//! 3. **The daily reaper** ([`reap`], driven by `ControlHandle::reap_docker`) removes what predates
//!    labelling or a crash orphaned, under rules pinned by [`decide`].
//!
//! Best-effort throughout: a missing `docker` binary or a stopped Docker logs ONCE per pass at debug
//! level and never fails a run, a removal or the daemon.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use regex::Regex;
use rhapsody_config::DockerCleanup;
use rhapsody_store::Store;
use rhapsody_workspace::RemovalHook;

use crate::control_loop::Event;
use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;

/// The container label a run's `docker run` carries: `rhapsody.run=<run id>`.
pub const RUN_LABEL: &str = "rhapsody.run";
/// The label Docker Compose stamps on every container, network and volume of a project.
pub const COMPOSE_LABEL: &str = "com.docker.compose.project";
/// How often the daemon runs the reaper while it is up. Leftovers are a slow leak, so daily is
/// enough; the workspace-cleanup half handles the common case promptly.
pub const DOCKER_REAP_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// The one line the turn-1 prompt carries so an agent labels what it starts.
pub const PROMPT_HINT: &str = "Docker: give any `docker run` the flag `--label rhapsody.run=$RHAPSODY_RUN_ID`, and \
     leave `COMPOSE_PROJECT_NAME` as set, so the daemon can clean up after this run.";
/// Upper bound on one Docker CLI call. A hung Docker Desktop must not wedge a cleanup forever.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(60);
/// How many of an issue's most recent runs a workspace cleanup covers.
const RUNS_PER_ISSUE: i64 = 500;
/// The `docker ps` row format: one container per line, tab-separated.
const PS_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.CreatedAt}}\t{{.Label \"rhapsody.run\"}}\t{{.Label \"com.docker.compose.project\"}}";
/// Networks younger than this are never pruned: a compose project creates its network a moment
/// before it attaches containers, and a prune in that window would fail the agent's `compose up`.
const NETWORK_PRUNE_MIN_AGE: &str = "until=1h";

/// The Docker CLI the cleanup drives. `bin` is `docker` (resolved on `PATH`, as the investigate
/// sandbox does); tests point it at `harness/stubs/fake-docker` and pass that stub's knobs in `env`,
/// so no test mutates the process environment.
#[derive(Debug, Clone)]
pub struct DockerCli {
    pub bin: String,
    pub env: Vec<(String, String)>,
}

impl Default for DockerCli {
    fn default() -> Self {
        // This crate's unit tests reload real workflows and remove real worktrees, which installs
        // and fires the hook; they must never list or remove the host's own containers. A test that
        // wants Docker builds a `DockerCli` on the fake-docker stub explicitly.
        let bin = if cfg!(test) {
            "/nonexistent/rhapsody-unit-test-docker"
        } else {
            "docker"
        };
        Self {
            bin: bin.to_string(),
            env: Vec::new(),
        }
    }
}

impl DockerCli {
    /// Runs one Docker command and returns its stdout, or the reason it failed (spawn error, timeout,
    /// or a non-zero exit with its stderr).
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = match tokio::time::timeout(DOCKER_TIMEOUT, cmd.output()).await {
            Err(_) => return Err(format!("docker {} timed out", args.join(" "))),
            Ok(Err(e)) => return Err(format!("run {}: {e}", self.bin)),
            Ok(Ok(out)) => out,
        };
        if !out.status.success() {
            return Err(format!(
                "docker {}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn list_containers(&self) -> Result<Vec<Container>, String> {
        let out = self
            .run(&["ps", "-a", "--no-trunc", "--format", PS_FORMAT])
            .await?;
        Ok(out.lines().filter_map(parse_container).collect())
    }

    /// `(id or name, compose project)` for every network or volume (`kind` is `network`/`volume`).
    async fn list_labelled(&self, kind: &str, key: &str) -> Result<Vec<(String, String)>, String> {
        let format = format!("{{{{.{key}}}}}\t{{{{.Label \"{COMPOSE_LABEL}\"}}}}");
        let out = self.run(&[kind, "ls", "--format", &format]).await?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (id, project) = l.split_once('\t').unwrap_or((l, ""));
                let id = id.trim();
                (!id.is_empty()).then(|| (id.to_string(), project.trim().to_string()))
            })
            .collect())
    }
}

/// Appends [`PROMPT_HINT`] to a run's turn-1 stack context when `enabled` (Docker cleanup on, and a
/// run that works in a checkout: not a manager or landing run). Disabled leaves it byte-identical.
pub fn with_docker_hint(stack_context: String, enabled: bool) -> String {
    if !enabled {
        return stack_context;
    }
    if stack_context.is_empty() {
        return PROMPT_HINT.to_string();
    }
    format!("{stack_context}\n\n{PROMPT_HINT}")
}

/// One `docker ps -a` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    /// `None` when Docker's `CreatedAt` did not parse; such a container never counts as old.
    pub created: Option<DateTime<Utc>>,
    pub run_label: String,
    pub compose_project: String,
}

impl Container {
    /// The run this container belongs to: its `rhapsody.run` label, else a `rhapsody-<id>` compose
    /// project. `None` for an unlabelled container.
    pub fn run_id(&self) -> Option<i64> {
        if let Ok(id) = self.run_label.trim().parse::<i64>()
            && id > 0
        {
            return Some(id);
        }
        compose_run_id(&self.compose_project)
    }
}

/// The run id a `rhapsody-<id>` compose project names, if it is one.
fn compose_run_id(project: &str) -> Option<i64> {
    project
        .strip_prefix(rhapsody_agent::claude::COMPOSE_PROJECT_PREFIX)?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

fn parse_container(line: &str) -> Option<Container> {
    let mut f = line.split('\t');
    let id = f.next()?.trim().to_string();
    let name = f.next().unwrap_or_default().trim().to_string();
    let created = parse_created(f.next().unwrap_or_default());
    let run_label = f.next().unwrap_or_default().trim().to_string();
    let compose_project = f.next().unwrap_or_default().trim().to_string();
    if id.is_empty() {
        return None;
    }
    Some(Container {
        id,
        name,
        created,
        run_label,
        compose_project,
    })
}

/// Parses Docker's `CreatedAt` (`2026-11-12 10:00:00 -0800 PST`): the first three fields carry the
/// instant; the zone abbreviation after them is redundant.
fn parse_created(s: &str) -> Option<DateTime<Utc>> {
    let head: Vec<&str> = s.split_whitespace().take(3).collect();
    if head.len() < 3 {
        return None;
    }
    DateTime::parse_from_str(&head.join(" "), "%Y-%m-%d %H:%M:%S %z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// What the daemon knows about a labelled container's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// The run is live on this daemon right now.
    Live,
    /// The store records the run as ended.
    Ended,
    /// Not live here, and the store cannot say it ended (a crash orphan, another daemon's run, or a
    /// store-disabled daemon). Treated like an unlabelled container's age rule, minus the name test.
    Unknown,
}

/// The reaper's compiled policy.
pub struct ReapPolicy {
    pub max_age: chrono::Duration,
    pub max_age_days: i64,
    pub patterns: Vec<Regex>,
}

impl ReapPolicy {
    /// Compiles the config's policy. An invalid pattern is skipped with a warning (config validation
    /// already refuses one, so this only guards a hand-built config).
    pub fn from_config(cfg: &DockerCleanup) -> Self {
        let patterns = cfg
            .name_patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(pattern = %p, err = %e, "docker cleanup: skipping an invalid name pattern");
                    None
                }
            })
            .collect();
        Self {
            max_age: chrono::Duration::days(cfg.max_age_days.max(1)),
            max_age_days: cfg.max_age_days.max(1),
            patterns,
        }
    }
}

/// The reaper's rule for one container: `Some(reason)` to remove it, `None` to keep it.
///
/// * `k8s_*` containers are never touched (a local Kubernetes runs its pods as these).
/// * A container of a LIVE run is kept. One whose run ENDED is removed. One whose run is neither
///   (crash orphan) is removed only once older than the age limit.
/// * An unlabelled container is removed only when it is older than the age limit AND its name
///   matches one of the ticket-shaped patterns; everything else unlabelled is kept, whatever its age.
pub fn decide(
    c: &Container,
    now: DateTime<Utc>,
    policy: &ReapPolicy,
    run_state: impl Fn(i64) -> RunState,
) -> Option<String> {
    if c.name.starts_with("k8s_") {
        return None;
    }
    let old = c.created.is_some_and(|t| now - t >= policy.max_age);
    if let Some(run) = c.run_id() {
        return match run_state(run) {
            RunState::Live => None,
            RunState::Ended => Some(format!("run {run} has ended")),
            RunState::Unknown if old => Some(format!(
                "run {run} is not live and the container is older than {} days",
                policy.max_age_days
            )),
            RunState::Unknown => None,
        };
    }
    if !old {
        return None;
    }
    let pattern = policy.patterns.iter().find(|p| p.is_match(&c.name))?;
    Some(format!(
        "unlabelled, older than {} days, name matches {}",
        policy.max_age_days,
        pattern.as_str()
    ))
}

/// One removal the reaper made (or, in dry-run, would make).
#[derive(Debug, Clone, PartialEq)]
pub struct Removal {
    pub name: String,
    pub age_days: Option<i64>,
    pub reason: String,
}

/// What one reaper pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReapReport {
    /// Containers removed (or, when `dry_run`, that would have been).
    pub removed: Vec<Removal>,
    pub dry_run: bool,
    /// Why the pass stopped early (Docker missing or not running), if it did.
    pub error: Option<String>,
}

/// One reaper pass: list every container, remove those [`decide`] condemns, then prune unused
/// networks (older than an hour). `run_state` resolves a labelled container's run; it is async so the
/// production driver can re-check liveness against the CURRENT running set immediately before each
/// removal, not only against the snapshot the pass started from. `dry_run` logs and removes nothing.
pub async fn reap<F, Fut>(
    cli: &DockerCli,
    policy: &ReapPolicy,
    dry_run: bool,
    now: DateTime<Utc>,
    run_state: F,
) -> ReapReport
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = RunState>,
{
    let mut report = ReapReport {
        dry_run,
        ..Default::default()
    };
    let containers = match cli.list_containers().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(err = %e, "docker reaper: docker unavailable; skipping this pass");
            report.error = Some(e);
            return report;
        }
    };
    for c in containers {
        // Resolve each labelled run once, up front, so `decide` stays a pure function.
        let state = match c.run_id() {
            Some(run) => run_state(run).await,
            None => RunState::Unknown,
        };
        let Some(reason) = decide(&c, now, policy, |_| state) else {
            continue;
        };
        let age_days = c.created.map(|t| (now - t).num_days());
        if dry_run {
            tracing::info!(container = %c.name, age_days = ?age_days, reason = %reason, "docker reaper (dry run): would remove container");
        } else if let Err(e) = cli.run(&["rm", "-f", "-v", &c.id]).await {
            tracing::warn!(container = %c.name, err = %e, "docker reaper: container removal failed");
            continue;
        } else {
            tracing::info!(container = %c.name, age_days = ?age_days, reason = %reason, "docker reaper: removed container");
        }
        report.removed.push(Removal {
            name: c.name,
            age_days,
            reason,
        });
    }
    if dry_run {
        tracing::info!("docker reaper (dry run): would prune unused networks");
    } else if let Err(e) = cli
        .run(&["network", "prune", "-f", "--filter", NETWORK_PRUNE_MIN_AGE])
        .await
    {
        tracing::debug!(err = %e, "docker reaper: network prune failed");
    }
    report
}

/// What one workspace cleanup did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CleanupReport {
    /// Names of the containers removed (or, when dry-running, that would have been).
    pub containers: Vec<String>,
    pub error: Option<String>,
}

/// Removes everything `runs` started: containers labelled `rhapsody.run=<id>` or in compose project
/// `rhapsody-<id>`, then that project's networks and volumes. A network or volume still in use makes
/// Docker refuse its removal, which is exactly the "if unused" rule, so those refusals are ignored.
/// The first Docker failure (binary missing, daemon not running) ends the pass with one debug line.
pub async fn cleanup_runs(cli: &DockerCli, runs: &HashSet<i64>, dry_run: bool) -> CleanupReport {
    let mut report = CleanupReport::default();
    if runs.is_empty() {
        return report;
    }
    let containers = match cli.list_containers().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(err = %e, "docker cleanup: docker unavailable; nothing removed");
            report.error = Some(e);
            return report;
        }
    };
    for c in containers {
        if !c.run_id().is_some_and(|r| runs.contains(&r)) {
            continue;
        }
        if dry_run {
            tracing::info!(container = %c.name, "docker cleanup (dry run): would remove the run's container");
        } else if let Err(e) = cli.run(&["rm", "-f", "-v", &c.id]).await {
            tracing::warn!(container = %c.name, err = %e, "docker cleanup: container removal failed");
            continue;
        } else {
            tracing::info!(container = %c.name, "docker cleanup: removed the run's container");
        }
        report.containers.push(c.name);
    }
    for (kind, key) in [("network", "ID"), ("volume", "Name")] {
        let rows = match cli.list_labelled(kind, key).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!(err = %e, kind, "docker cleanup: listing failed");
                continue;
            }
        };
        for (id, project) in rows {
            if !compose_run_id(&project).is_some_and(|r| runs.contains(&r)) {
                continue;
            }
            if dry_run {
                tracing::info!(kind, %id, %project, "docker cleanup (dry run): would remove");
            } else if cli.run(&[kind, "rm", &id]).await.is_ok() {
                tracing::info!(kind, %id, %project, "docker cleanup: removed");
            }
        }
    }
    report
}

/// The hook the workspace manager calls on every removal: resolves the issue's runs from the store
/// and removes what they started. Installed on reload while `docker_cleanup.enabled`.
///
/// The cleanup runs on a task of its own and the hook returns at once. Some removals are awaited
/// on the control task (the terminal-reconcile path), and a Docker Desktop that is slow or hung
/// must not stall scheduling behind it. Nothing depends on the cleanup having finished: the
/// worktree removal proceeds either way, and the daily reaper is the backstop.
pub fn removal_hook(
    store: Arc<dyn Store + Send + Sync>,
    cfg: &DockerCleanup,
    cli: DockerCli,
) -> RemovalHook {
    let dry_run = cfg.dry_run;
    let cli = Arc::new(cli);
    Arc::new(move |identifier: String| {
        let store = Arc::clone(&store);
        let cli = Arc::clone(&cli);
        tokio::spawn(async move {
            cleanup_issue(store, &cli, identifier, dry_run).await;
        });
        Box::pin(std::future::ready(()))
    })
}

/// Removes what every run of `identifier` started. The store read runs on the blocking pool.
async fn cleanup_issue(
    store: Arc<dyn Store + Send + Sync>,
    cli: &DockerCli,
    identifier: String,
    dry_run: bool,
) -> CleanupReport {
    let lookup =
        tokio::task::spawn_blocking(move || store.issue_history(&identifier, "", RUNS_PER_ISSUE))
            .await;
    let runs: HashSet<i64> = match lookup {
        Ok(Ok(rows)) => rows.iter().map(|r| r.id).filter(|id| *id > 0).collect(),
        Ok(Err(e)) => {
            tracing::debug!(err = %e, "docker cleanup: run lookup failed; nothing removed");
            return CleanupReport::default();
        }
        Err(_) => return CleanupReport::default(),
    };
    cleanup_runs(cli, &runs, dry_run).await
}

/// The race-free snapshot one reaper pass starts from. Built on the control task.
pub struct DockerReapPlan {
    /// `None` while no config is loaded or `docker_cleanup.enabled` is false: a no-op pass.
    pub cfg: Option<DockerCleanup>,
    pub live_run_ids: HashSet<i64>,
}

impl Orchestrator {
    /// Builds the reaper plan. MUST run on the control task (reads `self.eff` and `self.running`).
    pub fn build_docker_reap_plan(&self) -> DockerReapPlan {
        let cfg = self
            .eff
            .as_ref()
            .map(|e| e.cfg.docker_cleanup.clone())
            .filter(|c| c.enabled);
        let live_run_ids = self
            .running
            .values()
            .map(|re| re.run_id)
            .filter(|id| *id > 0)
            .collect();
        DockerReapPlan { cfg, live_run_ids }
    }

    /// Installs (or clears) the Docker cleanup hook on every workspace manager of the effective
    /// config. Called on each reload, so toggling `docker_cleanup.enabled` takes effect live.
    pub(crate) fn install_docker_removal_hook(&self) {
        let Some(eff) = self.eff.as_ref() else {
            return;
        };
        let hook = eff.cfg.docker_cleanup.enabled.then(|| {
            removal_hook(
                Arc::clone(&self.store),
                &eff.cfg.docker_cleanup,
                DockerCli::default(),
            )
        });
        eff.workspace.set_removal_hook(hook.clone());
        for p in &eff.projects {
            p.workspace.set_removal_hook(hook.clone());
        }
    }
}

impl ControlHandle {
    /// The reaper plan via a control round-trip. `None` when the loop is gone.
    pub async fn docker_reap_plan(&self) -> Option<DockerReapPlan> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events.send(Event::DockerReap { reply: tx }).ok()?;
        rx.await.ok()
    }

    /// Runs one reaper pass OFF the control loop. Each labelled container's run is re-checked
    /// against the CURRENT running set (the scratch janitor's round-trip) before it may be removed,
    /// so a run dispatched after the snapshot is never reaped; the store then says whether a
    /// non-live run ended. A gone loop, or the feature off, is a no-op.
    pub async fn reap_docker(&self) -> ReapReport {
        self.reap_docker_with(&DockerCli::default()).await
    }

    pub(crate) async fn reap_docker_with(&self, cli: &DockerCli) -> ReapReport {
        let Some(plan) = self.docker_reap_plan().await else {
            return ReapReport::default();
        };
        let Some(cfg) = plan.cfg else {
            return ReapReport::default();
        };
        let policy = ReapPolicy::from_config(&cfg);
        let store = Arc::clone(&self.store);
        let live = plan.live_run_ids;
        reap(cli, &policy, cfg.dry_run, Utc::now(), |run| {
            let store = Arc::clone(&store);
            let snapshot_live = live.contains(&run);
            async move {
                if snapshot_live || self.scratch_run_in_use(run).await {
                    return RunState::Live;
                }
                let row = tokio::task::spawn_blocking(move || store.get_run(run)).await;
                match row {
                    Ok(Ok(Some(r))) if !r.ended_at.is_empty() => RunState::Ended,
                    _ => RunState::Unknown,
                }
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    fn stub_path() -> String {
        format!(
            "{}/../../harness/stubs/fake-docker",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    /// A fake Docker CLI with canned listings and an argv log under a fresh temp dir.
    struct Fake {
        dir: TempDir,
        cli: DockerCli,
    }

    impl Fake {
        fn new(ps: &str, networks: &str, volumes: &str) -> Self {
            let dir = TempDir::new();
            for (name, body) in [("ps", ps), ("networks", networks), ("volumes", volumes)] {
                std::fs::write(dir.child(name), body).unwrap();
            }
            let cli = DockerCli {
                bin: stub_path(),
                env: vec![
                    ("FAKE_DOCKER_LOG".into(), dir.child("log")),
                    ("FAKE_DOCKER_PS".into(), dir.child("ps")),
                    ("FAKE_DOCKER_NETWORKS".into(), dir.child("networks")),
                    ("FAKE_DOCKER_VOLUMES".into(), dir.child("volumes")),
                ],
            };
            Self { dir, cli }
        }

        fn log(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.child("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn removed(&self) -> Vec<String> {
            self.log()
                .iter()
                .filter_map(|l| l.strip_prefix("rm -f -v ").map(str::to_string))
                .collect()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-22T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A `docker ps` row created `days` before [`now`].
    fn row(id: &str, name: &str, days: i64, run: &str, project: &str) -> String {
        let created = (now() - chrono::Duration::days(days))
            .format("%Y-%m-%d %H:%M:%S +0000 UTC")
            .to_string();
        format!("{id}\t{name}\t{created}\t{run}\t{project}\n")
    }

    fn policy() -> ReapPolicy {
        ReapPolicy::from_config(&DockerCleanup::default())
    }

    #[test]
    fn the_prompt_hint_is_appended_only_when_enabled() {
        assert_eq!(with_docker_hint("stack".into(), false), "stack");
        assert_eq!(with_docker_hint(String::new(), false), "");
        assert_eq!(with_docker_hint(String::new(), true), PROMPT_HINT);
        assert_eq!(
            with_docker_hint("stack".into(), true),
            format!("stack\n\n{PROMPT_HINT}")
        );
        assert!(PROMPT_HINT.contains("--label rhapsody.run=$RHAPSODY_RUN_ID"));
        assert!(PROMPT_HINT.contains("COMPOSE_PROJECT_NAME"));
    }

    #[test]
    fn parses_docker_created_at_with_its_zone_suffix() {
        let t = parse_created("2026-11-12 10:00:00 -0800 PST").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-11-12T18:00:00+00:00");
        assert!(parse_created("garbage").is_none());
    }

    #[test]
    fn a_container_names_its_run_by_label_or_compose_project() {
        let c = parse_container(&row("a", "x", 1, "42", "")).unwrap();
        assert_eq!(c.run_id(), Some(42));
        let c = parse_container(&row("b", "x", 1, "", "rhapsody-9")).unwrap();
        assert_eq!(c.run_id(), Some(9));
        let c = parse_container(&row("c", "x", 1, "", "studio2297")).unwrap();
        assert_eq!(c.run_id(), None);
    }

    /// The acceptance matrix: a finished run's labelled container and an old ticket-shaped
    /// unlabelled one go; a young unlabelled one, any `k8s_*` one and an active run's stay.
    #[tokio::test]
    async fn the_reaper_removes_only_what_the_rules_condemn() {
        let ps = [
            row("c1", "pg-for-run", 0, "11", ""),
            row("c2", "studio2107-postgres", 8, "", ""),
            row("c3", "studio2300-postgres", 2, "", ""),
            row("c4", "k8s_POD_coredns", 30, "", ""),
            row("c5", "k8s_etcd", 30, "11", ""),
            row("c6", "live-pg", 30, "12", ""),
            row("c7", "rhapsody-12-db-1", 30, "", "rhapsody-12"),
            row("c8", "operator-redis", 60, "", ""),
            row("c9", "rhapsody-11-db-1", 0, "", "rhapsody-11"),
        ]
        .concat();
        let fake = Fake::new(&ps, "", "");
        let state = |run: i64| async move {
            match run {
                11 => RunState::Ended,
                12 => RunState::Live,
                _ => RunState::Unknown,
            }
        };
        let report = reap(&fake.cli, &policy(), false, now(), state).await;
        assert_eq!(report.error, None);
        assert_eq!(fake.removed(), vec!["c1", "c2", "c9"]);
        let names: Vec<&str> = report.removed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["pg-for-run", "studio2107-postgres", "rhapsody-11-db-1"]
        );
        assert_eq!(report.removed[1].age_days, Some(8));
        assert!(report.removed[1].reason.contains("^studio"), "{report:?}");
        assert!(
            fake.log().iter().any(|l| l.starts_with("network prune -f")),
            "unused networks are pruned: {:?}",
            fake.log()
        );
    }

    #[tokio::test]
    async fn an_orphaned_labelled_container_goes_only_once_old() {
        let ps = [row("o1", "a", 8, "50", ""), row("o2", "b", 2, "51", "")].concat();
        let fake = Fake::new(&ps, "", "");
        reap(&fake.cli, &policy(), false, now(), |_| async {
            RunState::Unknown
        })
        .await;
        assert_eq!(fake.removed(), vec!["o1"]);
    }

    #[tokio::test]
    async fn an_operator_pattern_extends_the_allowlist() {
        let ps = row("x1", "scratchpad-pg", 9, "", "");
        let fake = Fake::new(&ps, "", "");
        let mut cfg = DockerCleanup::default();
        reap(
            &fake.cli,
            &ReapPolicy::from_config(&cfg),
            false,
            now(),
            |_| async { RunState::Unknown },
        )
        .await;
        assert!(fake.removed().is_empty());
        cfg.name_patterns.push("^scratchpad".into());
        reap(
            &fake.cli,
            &ReapPolicy::from_config(&cfg),
            false,
            now(),
            |_| async { RunState::Unknown },
        )
        .await;
        assert_eq!(fake.removed(), vec!["x1"]);
    }

    #[tokio::test]
    async fn dry_run_removes_nothing_and_lists_what_it_would() {
        let ps = [
            row("c1", "pg", 0, "11", ""),
            row("c2", "studio2107-postgres", 8, "", ""),
        ]
        .concat();
        let fake = Fake::new(&ps, "", "");
        let report = reap(&fake.cli, &policy(), true, now(), |_| async {
            RunState::Ended
        })
        .await;
        assert!(report.dry_run);
        let names: Vec<&str> = report.removed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["pg", "studio2107-postgres"]);
        let log = fake.log();
        assert!(
            log.iter().all(|l| l.starts_with("ps ")),
            "a dry run only lists: {log:?}"
        );
    }

    #[tokio::test]
    async fn the_reaper_tolerates_docker_missing_or_failing() {
        let missing = DockerCli {
            bin: "/nonexistent/docker".into(),
            env: Vec::new(),
        };
        let report = reap(&missing, &policy(), false, now(), |_| async {
            RunState::Ended
        })
        .await;
        assert!(report.error.is_some() && report.removed.is_empty());

        let mut fake = Fake::new(&row("c1", "pg", 0, "11", ""), "", "");
        fake.cli.env.push(("FAKE_DOCKER_FAIL".into(), "1".into()));
        let report = reap(&fake.cli, &policy(), false, now(), |_| async {
            RunState::Ended
        })
        .await;
        assert!(report.error.is_some());
        assert_eq!(fake.log().len(), 1, "one failed listing, then nothing");
    }

    #[tokio::test]
    async fn cleanup_removes_the_runs_containers_networks_and_volumes() {
        let ps = [
            row("c1", "pg", 0, "7", ""),
            row("c2", "app-db-1", 0, "", "rhapsody-7"),
            row("c3", "other-run", 0, "8", ""),
            row("c4", "studio9999-postgres", 30, "", ""),
        ]
        .concat();
        let fake = Fake::new(
            &ps,
            "n1\trhapsody-7\nn2\tstudio_default\n",
            "v1\trhapsody-7\nv2\t\n",
        );
        let report = cleanup_runs(&fake.cli, &HashSet::from([7]), false).await;
        assert_eq!(report.containers, vec!["pg", "app-db-1"]);
        assert_eq!(fake.removed(), vec!["c1", "c2"]);
        let log = fake.log();
        assert!(log.contains(&"network rm n1".to_string()), "{log:?}");
        assert!(log.contains(&"volume rm v1".to_string()), "{log:?}");
        assert!(
            !log.iter().any(|l| l.contains("n2") || l.contains("v2")),
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_dry_run_removes_nothing() {
        let fake = Fake::new(&row("c1", "pg", 0, "7", ""), "n1\trhapsody-7\n", "");
        let report = cleanup_runs(&fake.cli, &HashSet::from([7]), true).await;
        assert_eq!(report.containers, vec!["pg"]);
        let log = fake.log();
        assert!(
            log.iter()
                .all(|l| l.starts_with("ps ") || l.contains(" ls --format ")),
            "a dry run only lists: {log:?}"
        );
    }

    /// Workspace cleanup through the real hook: the issue's runs come from the store, and the
    /// removal passes the run's label and compose project to Docker. A missing binary or a failing
    /// Docker never fails the workspace removal.
    #[tokio::test]
    async fn the_workspace_removal_hook_cleans_up_the_issues_runs() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).unwrap());
        let run = store
            .start_run(rhapsody_store::RunStart {
                issue_id: "id-1".into(),
                issue_identifier: "MT-1".into(),
                ..Default::default()
            })
            .unwrap();
        let ps = [
            row("c1", "pg", 0, &run.to_string(), ""),
            row("c2", "db-1", 0, "", &format!("rhapsody-{run}")),
            row("c3", "unrelated", 0, "999", ""),
        ]
        .concat();
        let fake = Fake::new(&ps, "", "");
        let root = TempDir::new();
        let mgr = rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
            root: root.path.clone(),
            hooks: Default::default(),
            hook_timeout: Duration::from_secs(5),
        })
        .unwrap();
        mgr.set_removal_hook(Some(removal_hook(
            Arc::clone(&store),
            &DockerCleanup::default(),
            fake.cli.clone(),
        )));
        mgr.remove("", "MT-1").await.unwrap();
        // The hook returns at once and cleans up on its own task; wait for its removals.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fake.removed().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(fake.removed(), vec!["c1", "c2"]);

        // A missing binary and a stopped Docker: the cleanup reports why and removes nothing.
        for cli in [
            DockerCli {
                bin: "/nonexistent/docker".into(),
                env: Vec::new(),
            },
            DockerCli {
                env: vec![("FAKE_DOCKER_FAIL".into(), "1".into())],
                ..fake.cli.clone()
            },
        ] {
            let report = cleanup_issue(Arc::clone(&store), &cli, "MT-1".into(), false).await;
            assert!(
                report.error.is_some() && report.containers.is_empty(),
                "{report:?}"
            );
        }

        // A hung Docker never holds up the workspace removal (some removals run on the control
        // task): the hook returns while the cleanup is still waiting on Docker.
        let slow = DockerCli {
            env: vec![("FAKE_DOCKER_SLEEP_S".into(), "5".into())],
            ..fake.cli.clone()
        };
        mgr.set_removal_hook(Some(removal_hook(
            Arc::clone(&store),
            &DockerCleanup::default(),
            slow,
        )));
        let started = std::time::Instant::now();
        mgr.remove("", "MT-1")
            .await
            .expect("a Docker failure must never fail the workspace removal");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the removal waited on Docker: {:?}",
            started.elapsed()
        );
    }
}
