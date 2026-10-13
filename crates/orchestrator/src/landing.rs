//! Opt-in merged-change verification (STUDIO-1249); no Go counterpart.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rhapsody_config::landing::{LandingConfig, LandingContract, NamedCheck, parse_contract};
pub use rhapsody_store::{LandingPhase, LandingRow};
use rhapsody_store::{RunFilter, Store};
use rhapsody_tracker::Tracker;
use rhapsody_workspace::Manager;
use serde::{Deserialize, Serialize};

use crate::landingcheck::{CheckOutcome, DEFAULT_COMMAND_TIMEOUT, run_check, run_live_probe};
use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Disabled,
    Admitted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Progress {
    held_from: Option<String>,
    contract_hold: bool,
    phase_since_s: Option<i64>,
    evidence_epoch: u32,
    skipped: bool,
    acceptance_items: Vec<String>,
    acceptance_run: Option<i64>,
    acceptance_done: bool,
    project_group: String,
    resume_pending: bool,
    issue_id: String,
    team_id: String,
    started: bool,
    queued: bool,
    state_moved: bool,
    merge_pending: bool,
    fetched: bool,
    next_live_s: i64,
    samples: Vec<Sample>,
    checks: BTreeMap<String, CheckState>,
    no_contract: bool,
    pending: Option<String>,
    evidence_sent: bool,
    revert_landing: Option<i64>,
    revert_opened_at_s: Option<i64>,
    revert_merge_refusal: Option<String>,
    revert_pr_notified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sample {
    name: String,
    observed: String,
    pass: bool,
    at: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CheckState {
    failures: u32,
    next_s: i64,
    ran: bool,
    pass: bool,
}

/// An owned off-loop sweep. Drop aborts it before the daemon's workspace dependencies disappear.
pub(crate) struct LandingTask(tokio::task::JoinHandle<()>);

pub enum LandingCommand {
    Action(LandingAction),
    Acceptance {
        run_id: i64,
        text: String,
        failed: bool,
    },
}
pub struct LandingRequest {
    pub id: i64,
    pub command: LandingCommand,
    pub reply: Option<tokio::sync::oneshot::Sender<LandingActionOutcome>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LandingRun {
    pub row: LandingRow,
    pub prompt: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LandingsView {
    pub landings: Vec<LandingView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LandingView {
    pub id: i64,
    pub project_slug: String,
    pub ticket: String,
    pub pr: String,
    pub phase: String,
    pub phase_since_s: Option<i64>,
    pub phase_age_s: Option<i64>,
    pub first_failing_check: Option<String>,
    pub hold_reason: Option<String>,
    pub revert_of: Option<i64>,
    pub pausing_auto_merge: Vec<String>,
}

pub fn is_landing_key(key: &str) -> bool {
    key.starts_with("land:")
}
fn land_key(row: &LandingRow) -> String {
    format!("land:{}/{}#{}", row.owner, row.repo, row.pr)
}

impl crate::stop::ControlHandle {
    pub async fn landings(&self) -> Result<LandingsView, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(crate::control_loop::Event::LandingsList { reply: tx })
            .map_err(|e| e.to_string())?;
        let mut ctx = self.ctx.clone();
        tokio::select! { result = rx => result.map_err(|e| e.to_string())?, _ = ctx.cancelled() => Err("daemon is stopping".into()) }
    }
    pub async fn landing_action(&self, id: i64, action: LandingAction) -> LandingActionOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(crate::control_loop::Event::LandingRequest(LandingRequest {
                id,
                command: LandingCommand::Action(action),
                reply: Some(tx),
            }))
            .is_err()
        {
            return LandingActionOutcome::Failed("control loop is unavailable".into());
        }
        let mut ctx = self.ctx.clone();
        tokio::select! {
            result = rx => result.unwrap_or_else(|_| LandingActionOutcome::Failed("landing task is unavailable".into())),
            _ = ctx.cancelled() => LandingActionOutcome::Failed("daemon is stopping".into()),
        }
    }

    async fn dispatch_landing(&self, run: LandingRun) -> Result<Option<i64>, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(crate::control_loop::Event::LandingDispatch { run, reply: tx })
            .map_err(|e| e.to_string())?;
        let mut ctx = self.ctx.clone();
        tokio::select! {
            result = rx => result.map_err(|e| e.to_string()),
            _ = ctx.cancelled() => Err("daemon is stopping".into()),
        }
    }
}
impl Drop for LandingTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Orchestrator {
    pub(crate) fn teardown_landing_worktree(&self, run: &LandingRun) {
        let Some(project) = self
            .eff
            .as_ref()
            .and_then(|e| e.project_by_slug(&run.row.project_slug))
        else {
            return;
        };
        let workspace = project.workspace.clone();
        let repo = project.repo.clone();
        let slug = project.slug.clone();
        let key = land_key(&run.row);
        let guard = self.wg.add();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(error) = workspace.remove_worktree(&repo, &slug, &key).await {
                tracing::warn!(%error, "landing worktree cleanup failed after cancellation");
            }
        });
    }
    pub(crate) fn landings_view(&self, now: i64) -> Result<LandingsView, String> {
        let watches = self
            .store()
            .load_live_review_watch()
            .map_err(|e| e.to_string())?;
        let mut landings = Vec::new();
        for row in self.store().list_landings().map_err(|e| e.to_string())? {
            let progress: Progress =
                serde_json::from_str(&row.results_json).map_err(|e| e.to_string())?;
            let open = !matches!(
                row.phase,
                LandingPhase::Done | LandingPhase::Held | LandingPhase::Reopened
            );
            let group = self
                .eff
                .as_ref()
                .and_then(|e| e.project_by_slug(&row.project_slug))
                .map(|p| &p.group);
            let mut paused = std::collections::BTreeSet::new();
            if open {
                for watch in &watches {
                    if !watch.open
                        || (watch.key.owner.eq_ignore_ascii_case(&row.owner)
                            && watch.key.repo.eq_ignore_ascii_case(&row.repo)
                            && (watch.key.number == row.pr
                                || Some(watch.key.number) == row.revert_pr))
                    {
                        continue;
                    }
                    let pr = PrCoord::new(&watch.key.owner, &watch.key.repo, watch.key.number);
                    if self.review_auto_merge_for_repo(&watch.key.owner, &watch.key.repo)
                        && group.is_some_and(|group| self.landing_lock_groups(&pr).contains(group))
                    {
                        paused.insert(pr.to_string());
                    }
                }
            }
            let first_failing_check = progress
                .samples
                .iter()
                .find(|sample| {
                    progress
                        .checks
                        .get(&sample.name)
                        .is_some_and(|check| check.ran && !check.pass)
                })
                .map(|sample| sample.name.clone());
            landings.push(LandingView {
                id: row.id,
                project_slug: row.project_slug,
                ticket: row.ticket,
                pr: format!("{}/{}#{}", row.owner, row.repo, row.pr),
                phase: row.phase.as_str().into(),
                phase_since_s: progress.phase_since_s,
                phase_age_s: progress
                    .phase_since_s
                    .map(|at| now.saturating_sub(at).max(0)),
                first_failing_check,
                hold_reason: row.hold_reason,
                revert_of: row.revert_of,
                pausing_auto_merge: paused.into_iter().collect(),
            });
        }
        Ok(LandingsView { landings })
    }
    pub(crate) fn dispatch_landing(&mut self, run: LandingRun) -> Option<i64> {
        if self.drain.is_draining() {
            return None;
        }
        let row = match self.store().get_landing(run.row.id) {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(landing_id = run.row.id, %error, "landing dispatch could not read its row");
                return None;
            }
        };
        let progress: Progress = match serde_json::from_str(&row.results_json) {
            Ok(progress) => progress,
            Err(error) => {
                tracing::warn!(landing_id = row.id, %error, "landing dispatch progress is invalid");
                return None;
            }
        };
        if row.phase != LandingPhase::AwaitingAcceptance
            || progress.acceptance_items.is_empty()
            || row.revert_of.is_some()
        {
            return None;
        }
        let key = land_key(&row);
        if let Some(existing) = self.running.get(&key) {
            return Some(existing.run_id);
        }
        if self.claimed.contains(&key) {
            return None;
        }
        let eff = self.eff.as_ref()?;
        let project = eff.project_by_slug(&row.project_slug)?;
        if (eff.max_concurrent > 0 && self.implementation_pool_holders() >= eff.max_concurrent)
            || (project.max_concurrent > 0
                && self
                    .running
                    .values()
                    .filter(|re| re.project_group == project.group)
                    .count() as i64
                    >= project.max_concurrent)
        {
            return None;
        }
        let route = crate::retry::DispatchRoute {
            slug: project.slug.clone(),
            group: project.group.clone(),
            repo: project.repo.clone(),
            model: project.mcfg.claude.model.clone(),
            workspace_mode: project.workspace_mode.clone(),
        };
        let issue = rhapsody_core::Issue {
            id: key.clone(),
            identifier: key.clone(),
            title: format!("Landing acceptance {}/{}#{}", row.owner, row.repo, row.pr),
            description: Some(run.prompt.clone()),
            ..Default::default()
        };
        self.pending_landing.insert(key.clone(), run);
        self.dispatch_issue(issue, None, Some(route), String::new());
        self.pending_landing.remove(&key);
        self.running
            .get(&key)
            .map(|re| re.run_id)
            .filter(|id| *id > 0)
    }

    pub(crate) fn reserve_landing_merge(
        &self,
        rows: &[&rhapsody_store::ReviewWatchRow],
        pr: &PrCoord,
        now: i64,
    ) -> Result<(), String> {
        let Some(eff) = &self.eff else {
            return Ok(());
        };
        let Some(project) = eff.projects.iter().find(|p| {
            self.landing_config(&p.slug).is_some()
                && crate::ghsummons::parse_repo(&p.repo).is_some_and(|(o, r)| {
                    o.eq_ignore_ascii_case(&pr.owner) && r.eq_ignore_ascii_case(&pr.repo)
                })
        }) else {
            return Ok(());
        };
        let ticket = rows
            .iter()
            .find_map(|r| rhapsody_store::origin_ticket(&r.introduced_by))
            .unwrap_or("");
        let run = if ticket.is_empty() {
            None
        } else {
            self.store()
                .list_issue_runs(RunFilter {
                    issue: ticket.into(),
                    limit: 1,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
        };
        let slug = run
            .as_ref()
            .map(|r| r.project_slug.clone())
            .filter(|s| {
                self.landing_config(s).is_some()
                    && eff
                        .project_by_slug(s)
                        .is_some_and(|p| p.group == project.group)
            })
            .unwrap_or_else(|| project.slug.clone());
        let group = eff
            .cfg
            .projects
            .iter()
            .find(|p| p.slugs.contains(&slug))
            .and_then(|p| p.slugs.first())
            .cloned()
            .unwrap_or_else(|| slug.clone());
        let progress = Progress {
            project_group: group,
            merge_pending: true,
            phase_since_s: Some(now),
            ..Default::default()
        };
        let id = self
            .store()
            .save_landing(LandingRow {
                project_slug: slug,
                owner: pr.owner.clone(),
                repo: pr.repo.clone(),
                pr: pr.number,
                ticket: ticket.into(),
                results_json: serde_json::to_string(&progress).map_err(|e| e.to_string())?,
                updated_at_s: now,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
        if id == 0 {
            return Err("landing requires enabled persistence".into());
        }
        Ok(())
    }

    pub(crate) fn cancel_landing_merge(&self, pr: &PrCoord) -> Result<(), String> {
        if self.landing_merge_groups(pr).is_empty() && !self.landing_merges.contains_key(pr) {
            return Ok(());
        }
        for row in self
            .store()
            .load_open_landings()
            .map_err(|e| e.to_string())?
        {
            if row.owner.eq_ignore_ascii_case(&pr.owner)
                && row.repo.eq_ignore_ascii_case(&pr.repo)
                && row.pr == pr.number
                && serde_json::from_str::<Progress>(&row.results_json)
                    .is_ok_and(|p| p.merge_pending)
            {
                self.store()
                    .delete_landing_reservation(row.id)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn landing_config(&self, slug: &str) -> Option<LandingConfig> {
        self.eff
            .as_ref()?
            .cfg
            .projects
            .iter()
            .find(|p| p.slugs.iter().any(|s| s == slug))?
            .landing
            .clone()
            .filter(|c| c.enabled)
    }

    /// Admission is local and durable BEFORE retiring the merge observation. An empty merge SHA
    /// is a pending GitHub read, not the PR head (which is wrong for a squash merge).
    pub(crate) fn admit_landing(
        &self,
        rows: &[rhapsody_store::ReviewWatchRow],
        pr: &PrCoord,
        now: i64,
    ) -> Result<Admission, String> {
        if !self.eff.as_ref().is_some_and(|e| {
            e.projects.iter().any(|p| {
                self.landing_config(&p.slug).is_some()
                    && crate::ghsummons::parse_repo(&p.repo).is_some_and(|(o, r)| {
                        o.eq_ignore_ascii_case(&pr.owner) && r.eq_ignore_ascii_case(&pr.repo)
                    })
            })
        }) {
            return Ok(Admission::Disabled);
        }
        // A recovery's parent already owns admission. Its off-loop poll resolves the actual merge
        // SHA and creates the child. Never let a watch origin finish the original ticket instead.
        if self
            .store()
            .load_open_landings()
            .map_err(|e| e.to_string())?
            .iter()
            .any(|r| {
                r.phase == LandingPhase::Reverting
                    && r.revert_pr == Some(pr.number)
                    && r.owner.eq_ignore_ascii_case(&pr.owner)
                    && r.repo.eq_ignore_ascii_case(&pr.repo)
            })
        {
            return Ok(Admission::Admitted);
        }
        let Some(ticket) = rows
            .iter()
            .filter(|r| crate::reviewwatch::row_is(r, pr))
            .find_map(|r| rhapsody_store::origin_ticket(&r.introduced_by))
        else {
            return Ok(Admission::Disabled);
        };
        let run = self
            .store()
            .list_issue_runs(RunFilter {
                issue: ticket.into(),
                limit: 1,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?
            .into_iter()
            .next();
        let Some(run) = run else {
            return Ok(Admission::Disabled);
        };
        let Some(config) = self.landing_config(&run.project_slug) else {
            return Ok(Admission::Disabled);
        };
        if run.issue_id.is_empty() || run.team_id.is_empty() {
            return Err("landing ticket has no tracker identity".into());
        }
        let project_group = self
            .eff
            .as_ref()
            .and_then(|e| {
                e.cfg
                    .projects
                    .iter()
                    .find(|p| p.slugs.contains(&run.project_slug))
            })
            .and_then(|p| p.slugs.first())
            .cloned()
            .unwrap_or_else(|| run.project_slug.clone());
        let open = self
            .store()
            .load_open_landings()
            .map_err(|e| e.to_string())?;
        let reservation = open.iter().find(|r| {
            r.owner.eq_ignore_ascii_case(&pr.owner)
                && r.repo.eq_ignore_ascii_case(&pr.repo)
                && r.pr == pr.number
                && serde_json::from_str::<Progress>(&r.results_json).is_ok_and(|p| p.merge_pending)
        });
        let queued = open.iter().any(|r| {
            if reservation.is_some_and(|pending| r.id >= pending.id) {
                return false;
            }
            serde_json::from_str::<Progress>(&r.results_json)
                .map(|p| landing_group(r, &p) == project_group)
                .unwrap_or(true)
        });
        let progress = Progress {
            project_group,
            issue_id: run.issue_id,
            team_id: run.team_id,
            queued,
            phase_since_s: Some(now),
            ..Default::default()
        };
        let id = self
            .store()
            .save_landing(LandingRow {
                id: reservation.map(|r| r.id).unwrap_or(0),
                project_slug: run.project_slug,
                owner: pr.owner.clone(),
                repo: pr.repo.clone(),
                pr: pr.number,
                ticket: ticket.into(),
                deploy_deadline_s: deadline(now, config.deploy_timeout),
                results_json: serde_json::to_string(&progress).map_err(|e| e.to_string())?,
                updated_at_s: now,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
        if id == 0 {
            return Err("landing requires enabled persistence".into());
        }
        Ok(Admission::Admitted)
    }

    /// Only dependency snapshots and local reads run on control. Git, HTTP, commands and tracker
    /// effects belong to the owned task. Deterministic checks continue while draining.
    pub(crate) async fn landing_tick(&mut self, now: DateTime<Utc>) {
        if self.landing_requests.is_empty()
            && !self.eff.as_ref().is_some_and(|e| {
                e.cfg
                    .projects
                    .iter()
                    .any(|p| p.landing.as_ref().is_some_and(|c| c.enabled))
            })
        {
            return;
        }
        if self
            .landing_task
            .as_ref()
            .is_some_and(|t| !t.0.is_finished())
        {
            return;
        }
        self.landing_task = None;
        let mut rows = match self.store().load_open_landings() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "landing rows could not be read");
                for request in std::mem::take(&mut self.landing_requests) {
                    self.landing_operator_pending.remove(&request.id);
                    if let Some(reply) = request.reply {
                        let _ = reply.send(LandingActionOutcome::Failed(e.to_string()));
                    }
                }
                return;
            }
        };
        let requests = std::mem::take(&mut self.landing_requests);
        let mut pending = BTreeMap::<i64, Vec<LandingRequest>>::new();
        for request in requests {
            match self.store().get_landing(request.id) {
                Ok(Some(row)) => {
                    if !rows.iter().any(|r| r.id == row.id) {
                        rows.push(row);
                    }
                    pending.entry(request.id).or_default().push(request);
                }
                result => {
                    self.landing_operator_pending.remove(&request.id);
                    if let Some(reply) = request.reply {
                        let outcome = match result {
                            Ok(None) => LandingActionOutcome::NotFound,
                            Err(e) => LandingActionOutcome::Failed(e.to_string()),
                            _ => LandingActionOutcome::Failed("landing unavailable".into()),
                        };
                        let _ = reply.send(outcome);
                    }
                }
            }
        }
        let mut work = Vec::new();
        for row in rows {
            let Some(config) = self.landing_config(&row.project_slug) else {
                self.landing_operator_pending.remove(&row.id);
                for request in pending.remove(&row.id).unwrap_or_default() {
                    if let Some(reply) = request.reply {
                        let _ = reply.send(LandingActionOutcome::Refused(
                            "landing is disabled for this project".into(),
                        ));
                    }
                }
                continue;
            };
            let Some(eff) = &self.eff else {
                continue;
            };
            let Some(project) = eff.project_by_slug(&row.project_slug) else {
                self.landing_operator_pending.remove(&row.id);
                for request in pending.remove(&row.id).unwrap_or_default() {
                    if let Some(reply) = request.reply {
                        let _ = reply.send(LandingActionOutcome::Refused(
                            "the landing project is unavailable".into(),
                        ));
                    }
                }
                continue;
            };
            let runtime = LandingRuntime {
                control: Some(self.control()),
                store: self.store.clone(),
                tracker: project.tracker.clone(),
                checks: Arc::new(RepoChecks {
                    manager: project.workspace.clone(),
                    repo_url: project.repo.clone(),
                    config: config.clone(),
                }),
                config,
                done_state: self
                    .teams
                    .as_ref()
                    .and_then(|t| t.review_done_state())
                    .unwrap_or("")
                    .into(),
                changes_state: self
                    .teams
                    .as_ref()
                    .and_then(|t| t.review_changes_state())
                    .unwrap_or("In Progress")
                    .into(),
                notifications: Some(self.notifications.clone()),
                reverter: Some(Arc::new(crate::landingrevert::Reverter::production(
                    project.workspace.clone(),
                    project.repo.clone(),
                    self.review_auto_merge_for_repo(&row.owner, &row.repo),
                    self.control(),
                    self.human_holds.clone(),
                    self.teams
                        .as_ref()
                        .map(|t| {
                            t.roster
                                .iter()
                                .take(t.review.effective_reviewers())
                                .map(|i| i.name.clone())
                                .collect()
                        })
                        .unwrap_or_default(),
                ))),
            };
            let requests = pending.remove(&row.id).unwrap_or_default();
            work.push((runtime, row, requests));
        }
        if work.is_empty() {
            return;
        }
        let mut ctx = self.ctx.clone().unwrap_or_default();
        let guard = self.wg.add();
        let events = self.events.clone();
        self.landing_task = Some(LandingTask(tokio::spawn(async move {
            let _guard = guard;
            tokio::select! {
                _ = ctx.cancelled() => {},
                _ = async {
                    for (runtime, row, requests) in work {
                        let id = row.id;
                        for request in requests {
                            let operator = matches!(request.command, LandingCommand::Action(_));
                            let outcome = match request.command {
                                LandingCommand::Action(action) => runtime.action(id, action, now.timestamp()).await,
                                LandingCommand::Acceptance { run_id, text, failed } => match runtime.acceptance_finished(id, run_id, &text, failed, now.timestamp()).await {
                                    Ok(()) => LandingActionOutcome::Applied,
                                    Err(err) => LandingActionOutcome::Failed(err),
                                },
                            };
                            if let Some(reply) = request.reply { let _ = reply.send(outcome); }
                            else if let LandingActionOutcome::Failed(err) = outcome { tracing::warn!(landing_id = id, %err, "landing completion failed"); }
                            if operator { let _ = events.send(crate::control_loop::Event::LandingActionFinished { id }); }
                        }
                        let current = match runtime.store.get_landing(id) { Ok(Some(row)) => row, Ok(None) => continue, Err(error) => { tracing::warn!(landing_id = id, %error, "landing row could not be refreshed"); continue; } };
                        if let Err(err) = runtime.advance(current, now.timestamp()).await {
                            tracing::warn!(landing_id = id, %err, "landing tick failed; durable work retained for retry");
                        }
                    }
                } => {}
            }
            let _ = events.send(crate::control_loop::Event::LandingWake);
        })));
    }
}

fn deadline(now: i64, duration: Duration) -> i64 {
    now.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX).max(1))
}

#[async_trait::async_trait]
trait LandingChecks: Send + Sync {
    async fn merge_sha(&self, row: &LandingRow) -> Result<String, String> {
        crate::ghsummons::GH::new("", None)
            .landing_merge_sha(&row.owner, &row.repo, row.pr)
            .await
    }
    async fn prepare(&self, row: &LandingRow) -> Result<Option<LandingContract>, String>;
    /// The final bool records whether an unknown-object fetch was attempted.
    async fn live(
        &self,
        row: &LandingRow,
        contract: &LandingContract,
        fetch: bool,
    ) -> Result<(String, bool, bool), String>;
    async fn check(&self, row: &LandingRow, check: &NamedCheck) -> CheckOutcome;
}

struct RepoChecks {
    manager: Arc<Manager>,
    repo_url: String,
    config: LandingConfig,
}
impl RepoChecks {
    fn key(row: &LandingRow) -> String {
        format!("land:{}-{}-{}", row.owner, row.repo, row.pr)
    }
    fn path(&self, row: &LandingRow) -> String {
        self.manager.path_for(&self.repo_url, &Self::key(row))
    }
    fn env(&self) -> Result<Vec<(String, String)>, String> {
        let Some(path) = &self.config.env_file else {
            return Ok(Vec::new());
        };
        let text = std::fs::read_to_string(path)
            .map_err(|_| "landing env_file could not be read".to_string())?;
        let mut env = Vec::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else {
                return Err("invalid landing env_file assignment".into());
            };
            if key.is_empty()
                || !key.bytes().enumerate().all(|(i, c)| {
                    c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
            {
                return Err("invalid landing env_file key".into());
            }
            env.push((key.into(), value.trim().trim_matches(['\'', '"']).into()));
        }
        Ok(env)
    }
}
#[async_trait::async_trait]
impl LandingChecks for RepoChecks {
    async fn prepare(&self, row: &LandingRow) -> Result<Option<LandingContract>, String> {
        if crate::ghsummons::parse_repo(&self.repo_url).is_some_and(|(owner, repo)| {
            !owner.eq_ignore_ascii_case(&row.owner) || !repo.eq_ignore_ascii_case(&row.repo)
        }) {
            return Err("landing repository no longer matches the merged pull request".into());
        }
        self.manager
            .ensure_review_worktree(
                &self.repo_url,
                &row.project_slug,
                &Self::key(row),
                row.pr,
                &row.merge_sha,
            )
            .await
            .map_err(|e| e.to_string())?;
        let (after, before) = self
            .manager
            .landing_contract_exists(&self.repo_url, &row.merge_sha)
            .await
            .map_err(|e| e.to_string())?;
        if !after {
            return if before {
                Err("landing contract removed by this change".into())
            } else {
                Ok(None)
            };
        }
        // Checkout filters/hooks can rewrite worktree bytes. Read the pinned commit's blob after
        // provisioning so neither a hook nor a mutable default branch can replace its expectations.
        let blob = self
            .manager
            .read_blob(&self.repo_url, &row.merge_sha, ".rhapsody/landing.yaml")
            .await
            .map_err(|e| format!("landing contract: {e}"))?;
        if blob.symlink {
            return Err("landing contract must be a regular file".into());
        }
        parse_contract(&blob.content)
            .map(Some)
            .map_err(|e| format!("landing contract invalid: {e}"))
    }
    async fn live(
        &self,
        row: &LandingRow,
        contract: &LandingContract,
        fetch: bool,
    ) -> Result<(String, bool, bool), String> {
        let env = self.env()?;
        let revision = run_live_probe(
            &contract.live,
            Path::new(&self.path(row)),
            &env,
            contract.live_timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT),
        )
        .await?;
        let (contains, fetched) = self
            .manager
            .landing_revision_contains(&self.repo_url, &row.merge_sha, &revision, fetch)
            .await
            .map_err(|e| e.to_string())?;
        Ok((revision, contains, fetched))
    }
    async fn check(&self, row: &LandingRow, check: &NamedCheck) -> CheckOutcome {
        match self.env() {
            Ok(env) => {
                run_check(
                    &check.spec,
                    Path::new(&self.path(row)),
                    &env,
                    check.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT),
                )
                .await
            }
            Err(observed) => CheckOutcome {
                pass: false,
                observed,
                duration: Duration::ZERO,
            },
        }
    }
}

pub(crate) struct LandingRuntime {
    control: Option<crate::stop::ControlHandle>,
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    pub(crate) tracker: Arc<dyn Tracker>,
    checks: Arc<dyn LandingChecks>,
    config: LandingConfig,
    done_state: String,
    changes_state: String,
    pub(crate) notifications: Option<Arc<crate::breaker::NotificationsState>>,
    pub(crate) reverter: Option<Arc<crate::landingrevert::Reverter>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingAction {
    Retry,
    Skip,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandingActionOutcome {
    Applied,
    NotFound,
    Refused(String),
    Failed(String),
}

impl LandingRuntime {
    async fn acceptance(
        &self,
        row: &mut LandingRow,
        progress: &mut Progress,
        now: i64,
    ) -> Result<(), String> {
        if progress.acceptance_items.is_empty() {
            // L2 parked this phase before acceptance fields existed. Empty defaults are not
            // evidence that its ticket had no checklist; recover it before spending or passing.
            let issue = self
                .tracker
                .fetch_issue_by_identifier(&row.ticket)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("acceptance ticket could not be read")?;
            progress.acceptance_items =
                acceptance_items(issue.description.as_deref().unwrap_or(""));
            self.save(row, progress, now)?;
        }
        if progress.acceptance_items.is_empty() {
            progress.pending = Some("Done".into());
            progress.acceptance_done = true;
            return self.finish(row, progress, now).await;
        }
        if let Some(run_id) = progress.acceptance_run {
            match self.store.get_run(run_id).map_err(|e| e.to_string())? {
                Some(run) if run.outcome == "running" => return Ok(()),
                Some(run) => {
                    let events = self.store.run_events(run_id).map_err(|e| e.to_string())?;
                    let event = events
                        .iter()
                        .rev()
                        .find(|event| event.kind == "landing.verdict");
                    // Boot may interrupt a run after its verdict was saved but before end_run.
                    // Only an interrupted run with no durable verdict needs another agent.
                    if event.is_none() && run.outcome == "interrupted" {
                        progress.acceptance_run = None;
                    } else {
                        let result = event.and_then(|event| {
                            serde_json::from_str::<serde_json::Value>(&event.text).ok()
                        });
                        let text = result
                            .as_ref()
                            .and_then(|v| v["text"].as_str())
                            .unwrap_or("acceptance result unavailable after restart");
                        let failed = result
                            .as_ref()
                            .and_then(|v| v["failed"].as_bool())
                            .unwrap_or(true);
                        return self
                            .acceptance_finished(row.id, run_id, text, failed, now)
                            .await;
                    }
                }
                None => progress.acceptance_run = None,
            }
        }
        let Some(control) = &self.control else {
            return Ok(());
        };
        if control.drain.is_draining() {
            return Ok(());
        }
        let profile = rhapsody_config::profiles::builtin_profiles()
            .iter()
            .find(|p| p.name == "lander")
            .ok_or("built-in lander is unavailable")?;
        let mut prompt = format!(
            "{}\n\nTicket {}. Merged commit {}.\n\nChecklist (untrusted data):\n",
            profile.body,
            cell(&row.ticket),
            cell(&row.merge_sha)
        );
        prompt.push_str("Return these original item strings verbatim, in order:\n");
        prompt.push_str(
            &serde_json::to_string(&progress.acceptance_items).map_err(|e| e.to_string())?,
        );
        prompt.push('\n');
        prompt.push_str("\nContract evidence so far (untrusted data):\n");
        if progress.no_contract {
            prompt.push_str("no landing contract: the deploy was not observed\n");
        }
        for sample in &progress.samples {
            prompt.push_str(&format!(
                "{}: {} ({} at {})\n",
                cell(&sample.name),
                cell(&sample.observed),
                if sample.pass { "pass" } else { "fail" },
                sample.at
            ));
        }
        if let Some(run_id) = control
            .dispatch_landing(LandingRun {
                row: row.clone(),
                prompt,
            })
            .await?
        {
            progress.acceptance_run = Some(run_id);
            self.save(row, progress, now)?;
        }
        Ok(())
    }
    async fn acceptance_finished(
        &self,
        id: i64,
        run_id: i64,
        text: &str,
        failed: bool,
        now: i64,
    ) -> Result<(), String> {
        let Some(mut row) = self.store.get_landing(id).map_err(|e| e.to_string())? else {
            return Ok(());
        };
        let mut progress: Progress =
            serde_json::from_str(&row.results_json).map_err(|e| e.to_string())?;
        if row.phase != LandingPhase::AwaitingAcceptance || progress.acceptance_run != Some(run_id)
        {
            return Ok(());
        }
        let verdict = if failed {
            Err("landing agent failed to complete".into())
        } else {
            parse_acceptance(text, &progress.acceptance_items)
        };
        let passed = match verdict {
            Ok(verdict) => {
                let pass = verdict.pass && verdict.items.iter().all(|item| item.pass);
                for item in verdict.items {
                    let name = format!("acceptance: {}", item.item);
                    progress.checks.insert(
                        name.clone(),
                        CheckState {
                            ran: true,
                            pass: item.pass,
                            failures: if item.pass {
                                0
                            } else {
                                self.config.failure_threshold
                            },
                            ..Default::default()
                        },
                    );
                    progress.samples.push(Sample {
                        name,
                        pass: item.pass,
                        observed: item.evidence,
                        at: now,
                    });
                }
                pass
            }
            Err(reason) => {
                progress.checks.insert(
                    "acceptance".into(),
                    CheckState {
                        ran: true,
                        pass: false,
                        failures: self.config.failure_threshold,
                        ..Default::default()
                    },
                );
                progress.samples.push(Sample {
                    name: "acceptance".into(),
                    observed: reason,
                    pass: false,
                    at: now,
                });
                false
            }
        };
        progress.acceptance_done = true;
        progress.evidence_sent = false;
        progress.evidence_epoch = progress.evidence_epoch.saturating_add(1);
        progress.pending = Some(
            if passed {
                "Done"
            } else if self.config.auto_revert {
                "Reverting"
            } else {
                "Reopened"
            }
            .into(),
        );
        self.save(&mut row, &progress, now)?;
        self.finish(&mut row, &mut progress, now).await
    }
    async fn action(&self, id: i64, action: LandingAction, now: i64) -> LandingActionOutcome {
        match self.apply_action(id, action, now).await {
            Ok(outcome) => outcome,
            Err(error) => LandingActionOutcome::Failed(error),
        }
    }

    async fn apply_action(
        &self,
        id: i64,
        action: LandingAction,
        now: i64,
    ) -> Result<LandingActionOutcome, String> {
        if let Some(file) = (self.tracker.as_ref() as &dyn std::any::Any)
            .downcast_ref::<rhapsody_tracker::file::Tracker>()
        {
            file.enable_landing_comments();
        }
        let Some(mut row) = self.store.get_landing(id).map_err(|e| e.to_string())? else {
            return Ok(LandingActionOutcome::NotFound);
        };
        let mut progress: Progress =
            serde_json::from_str(&row.results_json).map_err(|e| e.to_string())?;
        let refused = |reason: &str| Ok(LandingActionOutcome::Refused(reason.into()));
        if progress.merge_pending {
            return refused("the merge has not been observed yet");
        }
        match action {
            LandingAction::Retry => {
                if row.phase != LandingPhase::Held {
                    return refused("retry requires Held");
                }
                if progress.contract_hold {
                    return refused("the merged commit has a contract problem; skip it instead");
                }
                let phase = match progress.held_from.as_deref() {
                    Some("WaitingForDeploy") => LandingPhase::WaitingForDeploy,
                    Some("Watching") => LandingPhase::Watching,
                    Some("Reverting") => LandingPhase::Reverting,
                    Some("AwaitingAcceptance") => LandingPhase::AwaitingAcceptance,
                    _ => return refused("the phase held from is unknown"),
                };
                if self
                    .store
                    .load_open_landings()
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|other| {
                        if other.id == id || row.revert_of == Some(other.id) {
                            return false;
                        }
                        serde_json::from_str::<Progress>(&other.results_json)
                            .map(|p| landing_group(other, &p) == landing_group(&row, &progress))
                            .unwrap_or(true)
                    })
                {
                    return refused("another landing holds the project lock");
                }
                row.phase = phase;
                row.hold_reason = None;
                progress.phase_since_s = Some(now);
                progress.pending = None;
                progress.fetched = false;
                progress.next_live_s = now;
                progress.evidence_sent = false;
                progress.evidence_epoch = progress.evidence_epoch.saturating_add(1);
                progress.skipped = false;
                if phase == LandingPhase::AwaitingAcceptance {
                    progress.acceptance_run = None;
                    progress.acceptance_done = false;
                }
                match phase {
                    LandingPhase::WaitingForDeploy => {
                        row.deploy_deadline_s = deadline(now, self.config.deploy_timeout)
                    }
                    LandingPhase::Watching => {
                        row.watch_until_s = Some(deadline(now, self.config.watch));
                        progress.checks.clear();
                    }
                    LandingPhase::Reverting => progress.revert_opened_at_s = Some(now),
                    _ => {}
                }
                if let Some(parent_id) = row.revert_of
                    && let Some(mut parent) = self
                        .store
                        .get_landing(parent_id)
                        .map_err(|e| e.to_string())?
                {
                    let mut parent_progress: Progress =
                        serde_json::from_str(&parent.results_json).map_err(|e| e.to_string())?;
                    if parent.phase == LandingPhase::Held
                        && parent_progress.held_from.as_deref() == Some("Reverting")
                        && parent_progress.revert_landing == Some(id)
                    {
                        parent.phase = LandingPhase::Reverting;
                        parent.hold_reason = None;
                        parent_progress.phase_since_s = Some(now);
                        parent_progress.pending = None;
                        self.save(&mut parent, &parent_progress, now)?;
                    }
                }
                self.save(&mut row, &progress, now)?;
            }
            LandingAction::Skip => {
                if !matches!(
                    row.phase,
                    LandingPhase::WaitingForDeploy
                        | LandingPhase::Watching
                        | LandingPhase::AwaitingAcceptance
                        | LandingPhase::Held
                ) {
                    return refused(
                        "skip requires WaitingForDeploy, Watching, AwaitingAcceptance or Held; use stop while Reverting",
                    );
                }
                progress.skipped = true;
                progress.evidence_sent = false;
                progress.evidence_epoch = progress.evidence_epoch.saturating_add(1);
                progress.pending = Some("Done".into());
                self.save(&mut row, &progress, now)?;
                self.finish(&mut row, &mut progress, now).await?;
            }
            LandingAction::Stop => {
                if row.phase != LandingPhase::Reverting {
                    return refused("stop requires Reverting");
                }
                if progress.revert_landing.is_some() {
                    return refused("the revert PR has already merged");
                }
                let Some(pr) = row.revert_pr else {
                    return refused("no pending revert PR has been opened");
                };
                let reverter = self
                    .reverter
                    .as_ref()
                    .ok_or("landing revert dependencies are unavailable")?;
                let snapshot = reverter
                    .github
                    .snapshot(&row, pr)
                    .await
                    .map_err(|e| e.to_string())?;
                if snapshot.status == crate::ghsummons::PrStatus::Merged {
                    return refused("the revert PR has already merged");
                }
                if let Err(error) = reverter.github.stop(&row, pr).await {
                    if matches!(error, crate::landingrevert::RevertError::Merged)
                        || reverter
                            .github
                            .snapshot(&row, pr)
                            .await
                            .is_ok_and(|snapshot| {
                                snapshot.status == crate::ghsummons::PrStatus::Merged
                            })
                    {
                        return refused("the revert PR has already merged");
                    }
                    return Err(error.to_string());
                }
                self.hold(&mut row, &mut progress, "revert stopped by operator", now)
                    .await?;
            }
        }
        Ok(LandingActionOutcome::Applied)
    }
    fn save(&self, row: &mut LandingRow, progress: &Progress, now: i64) -> Result<(), String> {
        row.results_json = serde_json::to_string(progress).map_err(|e| e.to_string())?;
        row.updated_at_s = now;
        self.store
            .save_landing(row.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    async fn advance(&self, mut row: LandingRow, now: i64) -> Result<(), String> {
        if let Some(file) = (self.tracker.as_ref() as &dyn std::any::Any)
            .downcast_ref::<rhapsody_tracker::file::Tracker>()
        {
            file.enable_landing_comments();
        }
        if matches!(
            row.phase,
            LandingPhase::Done | LandingPhase::Held | LandingPhase::Reopened
        ) {
            return Ok(());
        }
        let mut progress: Progress = serde_json::from_str(&row.results_json)
            .map_err(|e| format!("invalid landing progress: {e}"))?;
        // A persisted admission marker precedes GitHub's merge. It owns the lock, but cannot
        // move a ticket or observe a deploy until the watcher has confirmed the merge.
        if progress.merge_pending {
            return Ok(());
        }
        if progress.pending.is_some() {
            return self.finish(&mut row, &mut progress, now).await;
        }
        if row.phase == LandingPhase::AwaitingAcceptance {
            return self.acceptance(&mut row, &mut progress, now).await;
        }
        if row.phase == LandingPhase::Reverting {
            return self.revert(&mut row, &mut progress, now).await;
        }
        if !progress.state_moved {
            if !self.config.state.is_empty() {
                self.tracker
                    .move_issue_state(&progress.issue_id, &progress.team_id, &self.config.state)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            progress.state_moved = true;
            self.save(&mut row, &progress, now)?;
        }
        if !progress.started && row.revert_of.is_none() {
            // A human's second merge waits in the queue without consuming its deploy timeout.
            // An immediately admitted first landing retains the deadline set on admission.
            if self
                .store
                .load_open_landings()
                .map_err(|e| e.to_string())?
                .iter()
                .any(|r| {
                    r.id < row.id
                        && serde_json::from_str::<Progress>(&r.results_json)
                            .map(|p| landing_group(r, &p) == landing_group(&row, &progress))
                            .unwrap_or(true)
                })
            {
                return Ok(());
            }
            if progress.queued {
                row.deploy_deadline_s = deadline(now, self.config.deploy_timeout);
            }
            progress.started = true;
            self.save(&mut row, &progress, now)?;
        }
        if row.merge_sha.is_empty() {
            row.merge_sha = match self.checks.merge_sha(&row).await {
                Ok(sha) => sha,
                Err(_) if now >= row.deploy_deadline_s => {
                    return self
                        .hold(
                            &mut row,
                            &mut progress,
                            "deploy timeout: the merged commit could not be resolved",
                            now,
                        )
                        .await;
                }
                Err(err) => return Err(err),
            };
            self.save(&mut row, &progress, now)?;
        }
        let contract = match self.checks.prepare(&row).await {
            Ok(contract) => contract,
            Err(reason) => {
                progress.contract_hold = reason.starts_with("landing contract invalid:")
                    || reason == "landing contract removed by this change"
                    || reason == "landing contract must be a regular file";
                return self.hold(&mut row, &mut progress, &reason, now).await;
            }
        };
        let Some(contract) = contract else {
            progress.no_contract = true;
            progress.pending = Some("Done".into());
            self.save(&mut row, &progress, now)?;
            return self.finish(&mut row, &mut progress, now).await;
        };
        if row.phase == LandingPhase::WaitingForDeploy && now >= row.deploy_deadline_s {
            return self
                .hold(
                    &mut row,
                    &mut progress,
                    "deploy timeout: the merged revision was not observed",
                    now,
                )
                .await;
        }
        if row.phase == LandingPhase::WaitingForDeploy {
            if now < progress.next_live_s {
                return Ok(());
            }
            progress.next_live_s =
                deadline(now, contract.live_every.unwrap_or(Duration::from_secs(30)));
            match self.checks.live(&row, &contract, !progress.fetched).await {
                Ok((observed, live, fetched)) => {
                    progress.fetched |= fetched;
                    progress.samples.push(Sample {
                        name: "live".into(),
                        observed,
                        pass: live,
                        at: now,
                    });
                    if live {
                        row.phase = LandingPhase::Watching;
                        progress.phase_since_s = Some(now);
                        row.watch_until_s = Some(deadline(now, self.config.watch));
                    }
                }
                Err(observed) => progress.samples.push(Sample {
                    name: "live".into(),
                    observed,
                    pass: false,
                    at: now,
                }),
            }
            self.save(&mut row, &progress, now)?;
            if row.phase != LandingPhase::Watching {
                return Ok(());
            }
        }
        // A crash can occur after recording the threshold sample but before saving the pending
        // outcome. A later passing sample must never erase that already-failed landing.
        let mut failed = progress
            .checks
            .values()
            .any(|s| s.failures >= self.config.failure_threshold);
        for check in &contract.checks {
            let state = progress.checks.entry(check.name.clone()).or_default();
            if state.failures >= self.config.failure_threshold {
                continue;
            }
            if state.ran && (check.every.is_none() && state.pass || now < state.next_s) {
                continue;
            }
            let outcome = self.checks.check(&row, check).await;
            state.ran = true;
            state.pass = outcome.pass;
            state.failures = if outcome.pass {
                0
            } else {
                state.failures.saturating_add(1)
            };
            state.next_s = deadline(now, check.every.unwrap_or(Duration::from_secs(30)));
            failed |= state.failures >= self.config.failure_threshold;
            progress.samples.push(Sample {
                name: check.name.clone(),
                observed: outcome.observed,
                pass: outcome.pass,
                at: now,
            });
            self.save(&mut row, &progress, now)?;
        }
        if failed {
            if row.revert_of.is_some() {
                return self.hold(&mut row, &mut progress, "revert landing failed; no second automatic revert; operator attention required", now).await;
            }
            progress.pending = Some(
                if self.config.auto_revert {
                    "Reverting"
                } else {
                    "Reopened"
                }
                .into(),
            );
        } else if row.watch_until_s.is_some_and(|end| now >= end)
            && contract.checks.iter().all(|c| {
                progress
                    .checks
                    .get(&c.name)
                    .is_some_and(|s| s.ran && s.pass)
            })
        {
            progress.pending = Some("Done".into());
        }
        self.save(&mut row, &progress, now)?;
        if progress.pending.is_some() {
            self.finish(&mut row, &mut progress, now).await?;
        }
        Ok(())
    }
    async fn revert(
        &self,
        row: &mut LandingRow,
        progress: &mut Progress,
        now: i64,
    ) -> Result<(), String> {
        if row.revert_of.is_some() {
            return self
                .hold(
                    row,
                    progress,
                    "revert landing failed; no second automatic revert",
                    now,
                )
                .await;
        }
        if let Some(id) = progress.revert_landing {
            let child = self
                .store
                .get_landing(id)
                .map_err(|e| e.to_string())?
                .ok_or("revert landing disappeared")?;
            match child.phase {
                LandingPhase::Done => {
                    progress.pending = Some("Reopened".into());
                    progress.evidence_sent = false;
                    self.save(row, progress, now)?;
                    return self.finish(row, progress, now).await;
                }
                LandingPhase::Held => {
                    return self
                        .hold(
                            row,
                            progress,
                            &format!(
                                "revert landing failed: {}; operator attention required",
                                child.hold_reason.as_deref().unwrap_or("unknown failure")
                            ),
                            now,
                        )
                        .await;
                }
                _ => return Ok(()),
            }
        }
        let Some(reverter) = &self.reverter else {
            return Err("landing revert dependencies are unavailable".into());
        };
        if row.revert_pr.is_none() {
            let failed = failed_samples(progress, self.config.failure_threshold)
                .into_iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        CheckOutcome {
                            pass: false,
                            observed: s.observed.clone(),
                            duration: Duration::ZERO,
                        },
                    )
                })
                .collect::<Vec<_>>();
            match reverter.open_revert(row, &failed).await {
                Ok(pr) => {
                    row.revert_pr = Some(pr);
                    self.save(row, progress, now)?;
                }
                Err(crate::landingrevert::RevertError::Conflict(reason)) => return self.hold(row, progress, &format!("git revert conflicts: {reason}; no PR opened; operator attention required"), now).await,
                Err(err) => return Err(err.to_string()),
            }
        }
        let pr = row.revert_pr.ok_or("revert PR was not recorded")?;
        if !progress.revert_pr_notified {
            self.notify_reverting(row, progress);
            progress.revert_pr_notified = true;
            self.save(row, progress, now)?;
        }
        let snapshot = reverter
            .github
            .snapshot(row, pr)
            .await
            .map_err(|e| e.to_string())?;
        if progress.revert_opened_at_s.is_none() {
            // GitHub's creation time also reconciles a crash after opening the PR but before saving.
            progress.revert_opened_at_s = Some(snapshot.opened_at_s);
            self.save(row, progress, now)?;
        }
        match snapshot.status {
            crate::ghsummons::PrStatus::Closed => {
                return self
                    .hold(
                        row,
                        progress,
                        "revert PR closed without merging; operator attention required",
                        now,
                    )
                    .await;
            }
            crate::ghsummons::PrStatus::Merged => {
                if !(7..=64).contains(&snapshot.merge_sha.len())
                    || !snapshot.merge_sha.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err("merged revert has no valid merge SHA".into());
                }
                let child_progress = Progress {
                    issue_id: progress.issue_id.clone(),
                    team_id: progress.team_id.clone(),
                    project_group: progress.project_group.clone(),
                    state_moved: true,
                    started: true,
                    phase_since_s: Some(now),
                    ..Default::default()
                };
                let id = self
                    .store
                    .save_landing(LandingRow {
                        owner: row.owner.clone(),
                        repo: row.repo.clone(),
                        project_slug: row.project_slug.clone(),
                        ticket: row.ticket.clone(),
                        pr,
                        merge_sha: snapshot.merge_sha,
                        revert_of: Some(row.id),
                        deploy_deadline_s: deadline(now, self.config.deploy_timeout),
                        results_json: serde_json::to_string(&child_progress)
                            .map_err(|e| e.to_string())?,
                        updated_at_s: now,
                        ..Default::default()
                    })
                    .map_err(|e| e.to_string())?;
                if id == 0 {
                    return Err("revert landing requires persistence".into());
                }
                progress.revert_landing = Some(id);
                self.save(row, progress, now)
            }
            crate::ghsummons::PrStatus::Open => {
                if progress
                    .revert_opened_at_s
                    .is_some_and(|opened| now >= deadline(opened, self.config.deploy_timeout))
                {
                    let timeout = rhapsody_config::go_duration_string(
                        chrono::Duration::from_std(self.config.deploy_timeout)
                            .unwrap_or(chrono::Duration::MAX),
                    );
                    let reason = format!(
                        "revert PR #{pr} not merged within {timeout}; last merge-gate refusal: {}",
                        progress
                            .revert_merge_refusal
                            .as_deref()
                            .unwrap_or("not yet attempted")
                    );
                    return self.hold(row, progress, &reason, now).await;
                }
                let result = self.merge_revert(row, pr, snapshot, reverter).await;
                match result {
                    Ok(Some(reason)) => {
                        tracing::debug!(
                            landing_id = row.id,
                            reason,
                            "revert merge gates refused; waiting"
                        );
                        progress.revert_merge_refusal = Some(reason);
                    }
                    Err(err) => {
                        progress.revert_merge_refusal = Some(err.clone());
                        self.save(row, progress, now)?;
                        return Err(err);
                    }
                    Ok(None) => {}
                }
                self.save(row, progress, now)
            }
        }
    }
    async fn merge_revert(
        &self,
        row: &LandingRow,
        pr: i64,
        snapshot: crate::landingrevert::RevertSnapshot,
        reverter: &crate::landingrevert::Reverter,
    ) -> Result<Option<String>, String> {
        if self.config.revert_review {
            reverter.introduce_review(row, pr).await?;
        }
        if !reverter.auto_merge {
            return Ok(Some("auto-merge is disabled".into()));
        }
        let (held, primed) = reverter.holds.labelled_and_primed();
        if !primed {
            return Ok(Some("human holds have not been read".into()));
        }
        if held.contains(&row.ticket.to_ascii_lowercase()) {
            return Ok(Some("the ticket has a human-hold label".into()));
        }
        let watches = self
            .store
            .load_live_review_watch()
            .map_err(|e| e.to_string())?;
        let mine: Vec<_> = watches
            .iter()
            .filter(|w| {
                w.key.number == pr
                    && w.key.owner.eq_ignore_ascii_case(&row.owner)
                    && w.key.repo.eq_ignore_ascii_case(&row.repo)
            })
            .collect();
        let approved_by = match crate::automerge::revert_merge_verdict(
            row,
            snapshot.labelled,
            self.config.revert_review,
            &mine,
            &snapshot.head,
        ) {
            Ok(approved_by) => approved_by,
            Err(refusal) => return Ok(Some(refusal.why().into())),
        };
        let plan = crate::automerge::AutoMergePlan {
            pr: PrCoord::new(&row.owner, &row.repo, pr),
            head: snapshot.head,
            approved_by,
            manager_approval: None,
        };
        match crate::runautomerge::perform_auto_merge(&plan, &reverter.merges).await {
            crate::runautomerge::AutoMergeOutcome::Failed(err) => return Err(err),
            crate::runautomerge::AutoMergeOutcome::Declined(reason)
            | crate::runautomerge::AutoMergeOutcome::Held(reason) => {
                return Ok(Some(reason.into()));
            }
            _ => {}
        }
        Ok(None)
    }
    fn notify_reverting(&self, row: &LandingRow, progress: &Progress) {
        if let Some(notifications) = &self.notifications {
            let failed = failed_samples(progress, self.config.failure_threshold);
            let first = failed
                .first()
                .map(|s| format!("{}: {}", cell(&s.name), cell(&s.observed)))
                .unwrap_or_else(|| "acceptance failed".into());
            let body = match row.revert_pr {
                Some(pr) => format!("{first}; revert PR #{pr}"),
                None => format!("{first}; opening a revert PR"),
            };
            notifications.push(
                Utc::now(),
                "Landing failed: reverting".into(),
                body,
                row.ticket.clone(),
                format!(
                    "{}/{}#{}",
                    row.owner,
                    row.repo,
                    row.revert_pr.unwrap_or(row.pr)
                ),
            );
        }
    }
    async fn hold(
        &self,
        row: &mut LandingRow,
        progress: &mut Progress,
        reason: &str,
        now: i64,
    ) -> Result<(), String> {
        row.hold_reason = Some(reason.into());
        progress.held_from = Some(row.phase.as_str().into());
        if row.phase == LandingPhase::Reverting {
            progress.evidence_sent = false;
        }
        progress.pending = Some("Held".into());
        self.save(row, progress, now)?;
        self.finish(row, progress, now).await
    }
    async fn finish(
        &self,
        row: &mut LandingRow,
        progress: &mut Progress,
        now: i64,
    ) -> Result<(), String> {
        let mut target = progress
            .pending
            .clone()
            .ok_or("landing has no pending outcome")?;
        if target == "Done"
            && row.revert_of.is_none()
            && !progress.skipped
            && !progress.acceptance_done
        {
            let description = self
                .tracker
                .fetch_issue_description_by_identifier(&row.ticket)
                .await
                .map_err(|e| e.to_string())?;
            if description.as_deref().is_some_and(has_acceptance) {
                progress.acceptance_items = acceptance_items(description.as_deref().unwrap_or(""));
                target = "AwaitingAcceptance".into();
                progress.pending = Some(target.clone());
                self.save(row, progress, now)?;
            }
        }
        if target == "AwaitingAcceptance" {
            row.phase = LandingPhase::AwaitingAcceptance;
            progress.phase_since_s = Some(now);
            progress.pending = None;
            return self.save(row, progress, now);
        }
        let marker = if progress.evidence_epoch > 0 {
            format!(
                "<!-- rhapsody:landing:{}:{}:{target} -->",
                row.id, progress.evidence_epoch
            )
        } else if row.phase == LandingPhase::Reverting {
            format!("<!-- rhapsody:landing:{}:{target} -->", row.id)
        } else {
            format!("<!-- rhapsody:landing:{} -->", row.id)
        };
        if !progress.evidence_sent {
            // A successful comment followed by a crash before the local save is reconciled by its
            // stable marker, rather than posting the same evidence again after restart.
            let comments = self
                .tracker
                .list_comments(&progress.issue_id)
                .await
                .map_err(|e| e.to_string())?;
            if !comments.iter().any(|c| c.body.contains(&marker)) {
                let mut body = format!(
                    "{marker}\n## Landing: {target}\n\nname | observed | pass/fail | at\n--- | --- | --- | ---\n"
                );
                for sample in &progress.samples {
                    body.push_str(&format!(
                        "{} | {} | {} | {}\n",
                        cell(&sample.name),
                        cell(&sample.observed),
                        if sample.pass { "pass" } else { "fail" },
                        sample.at
                    ));
                }
                if progress.no_contract {
                    body.push_str("\nno landing contract: the deploy was not observed\n");
                }
                if progress.skipped {
                    body.push_str("\nOperator evidence: skipped by operator (checks were not asserted passed).\n");
                }
                if progress.acceptance_done {
                    body.push_str("\n## Post-deploy acceptance\n");
                    for item in &progress.acceptance_items {
                        let name = format!("acceptance: {item}");
                        let sample = progress.samples.iter().rev().find(|s| s.name == name);
                        body.push_str(&format!(
                            "- [{}] {} — {}\n",
                            if sample.is_some_and(|s| s.pass) {
                                "x"
                            } else {
                                " "
                            },
                            cell(item),
                            sample
                                .map(|s| cell(&s.observed))
                                .unwrap_or_else(|| "not verified".into())
                        ));
                    }
                }
                if let Some(reason) = &row.hold_reason {
                    body.push_str(&format!("\nHeld: {}\n", cell(reason)));
                }
                self.tracker
                    .create_comment(&progress.issue_id, &body)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            progress.evidence_sent = true;
            self.save(row, progress, now)?;
        }
        row.phase = match target.as_str() {
            "Held" => {
                if let Some(notifications) = &self.notifications {
                    notifications.push(
                        Utc::now(),
                        "Landing held".into(),
                        row.hold_reason.clone().unwrap_or_default(),
                        row.ticket.clone(),
                        format!("{}/{}#{}", row.owner, row.repo, row.pr),
                    );
                }
                tracing::warn!(landing_id = row.id, ticket = %row.ticket, reason = ?row.hold_reason, "landing Held; operator attention required");
                LandingPhase::Held
            }
            "Reopened" => {
                let issue = self
                    .tracker
                    .fetch_issue_by_identifier(&row.ticket)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("landing ticket could not be read for feedback")?;
                let description = issue.description.unwrap_or_default();
                let feedback_marker = format!("<!-- rhapsody:landing-feedback:{} -->", row.id);
                if !description.contains(&feedback_marker) {
                    let failed = failed_samples(progress, self.config.failure_threshold)
                        .into_iter()
                        .map(|s| format!("- {}: {}", cell(&s.name), cell(&s.observed)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.tracker.update_issue_description(&progress.issue_id, &format!("{feedback_marker}\n## REVIEW FEEDBACK\nLanding checks failed:\n{failed}\n\n{description}"))
                        .await.map_err(|e| e.to_string())?;
                }
                self.tracker
                    .move_issue_state(&progress.issue_id, &progress.team_id, &self.changes_state)
                    .await
                    .map_err(|e| e.to_string())?;
                progress.resume_pending = true;
                LandingPhase::Reopened
            }
            "Done" => {
                if row.revert_of.is_none() && !self.done_state.is_empty() {
                    self.tracker
                        .move_issue_state(&progress.issue_id, &progress.team_id, &self.done_state)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                LandingPhase::Done
            }
            "AwaitingAcceptance" => LandingPhase::AwaitingAcceptance,
            "Reverting" => {
                self.notify_reverting(row, progress);
                LandingPhase::Reverting
            }
            _ => return Err("unknown landing outcome".into()),
        };
        progress.pending = None;
        progress.phase_since_s = Some(now);
        self.save(row, progress, now)
    }
}
pub(crate) fn cell(text: &str) -> String {
    crate::managerapply::strip_summon_tokens(text)
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}
fn failed_samples(progress: &Progress, threshold: u32) -> Vec<&Sample> {
    progress
        .checks
        .iter()
        .filter(|(_, state)| state.failures >= threshold)
        .filter_map(|(name, _)| progress.samples.iter().rev().find(|s| &s.name == name))
        .collect()
}
fn has_acceptance(description: &str) -> bool {
    !acceptance_items(description).is_empty()
}

fn acceptance_items(description: &str) -> Vec<String> {
    let mut section = false;
    let mut fenced = false;
    let mut items = Vec::new();
    for line in description.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if line == "## Post-deploy acceptance" {
            section = true;
            continue;
        }
        if section && line.starts_with('#') {
            break;
        }
        if section {
            let item = ["- [ ] ", "- [x] ", "- [X] "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix));
            if let Some(item) = item.filter(|item| !item.trim().is_empty()) {
                items.push(item.trim().into());
            }
        }
    }
    items
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceVerdict {
    pass: bool,
    items: Vec<AcceptanceItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceItem {
    item: String,
    pass: bool,
    evidence: String,
}

fn parse_acceptance(text: &str, expected: &[String]) -> Result<AcceptanceVerdict, String> {
    let marker = "```rhapsody-landing-verdict\n";
    if text.matches(marker).count() != 1 {
        return Err("acceptance requires one structured verdict".into());
    }
    let block = text
        .split_once(marker)
        .and_then(|(_, rest)| rest.split_once("```"))
        .map(|(block, _)| block)
        .ok_or("acceptance verdict fence is incomplete")?;
    let verdict: AcceptanceVerdict =
        serde_json::from_str(block).map_err(|e| format!("invalid acceptance verdict: {e}"))?;
    if expected.is_empty()
        || verdict.items.len() != expected.len()
        || verdict
            .items
            .iter()
            .zip(expected)
            .any(|(item, expected)| &item.item != expected || item.evidence.trim().is_empty())
    {
        return Err("acceptance must report every checklist item in order with evidence".into());
    }
    if verdict.pass != verdict.items.iter().all(|item| item.pass) {
        return Err("acceptance verdict contradicts its checklist".into());
    }
    Ok(verdict)
}

fn landing_group<'a>(row: &'a LandingRow, progress: &'a Progress) -> &'a str {
    if progress.project_group.is_empty() {
        &row.project_slug
    } else {
        &progress.project_group
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ghsummons::*;
    use crate::testsupport::{TempDir, empty_effective};
    use rhapsody_config::landing::parse_contract;
    use rhapsody_store::{Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Checks {
        contract: Result<Option<LandingContract>, String>,
        live: Mutex<VecDeque<(String, bool)>>,
        checks: Mutex<VecDeque<bool>>,
        calls: Mutex<Vec<bool>>,
    }
    #[async_trait::async_trait]
    impl LandingChecks for Checks {
        async fn merge_sha(&self, _: &LandingRow) -> Result<String, String> {
            Err("merge lookup unavailable".into())
        }
        async fn prepare(&self, _: &LandingRow) -> Result<Option<LandingContract>, String> {
            self.contract.clone()
        }
        async fn live(
            &self,
            _: &LandingRow,
            _: &LandingContract,
            fetch: bool,
        ) -> Result<(String, bool, bool), String> {
            self.calls.lock().unwrap().push(fetch);
            let (revision, live) = self
                .live
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(("unknown".into(), false));
            Ok((revision, live, !live && fetch))
        }
        async fn check(&self, _: &LandingRow, _: &NamedCheck) -> CheckOutcome {
            let pass = self.checks.lock().unwrap().pop_front().unwrap_or(true);
            CheckOutcome {
                pass,
                observed: if pass { "healthy" } else { "status=503" }.into(),
                duration: Duration::ZERO,
            }
        }
    }
    struct Rig {
        runtime: LandingRuntime,
        checks: Arc<Checks>,
        tracker: Arc<Fake>,
        id: i64,
    }
    pub(crate) fn config() -> LandingConfig {
        LandingConfig {
            enabled: true,
            auto_revert: true,
            deploy_timeout: Duration::from_secs(30),
            watch: Duration::from_secs(10),
            failure_threshold: 2,
            revert_review: false,
            state: String::new(),
            env_file: None,
        }
    }
    impl Rig {
        fn new(done: &str) -> Self {
            let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
            let tracker = Arc::new(Fake::new());
            let checks = Arc::new(Checks {
                contract: Ok(Some(parse_contract("version: 1\nlive: {command: [version], every: 1s}\nchecks:\n- name: health\n  command: [health]\n  every: 1s\n").unwrap())),
                live: Mutex::new(VecDeque::from([("merge".into(), true)])),
                checks: Mutex::new(VecDeque::new()), calls: Mutex::new(Vec::new()),
            });
            let progress = Progress {
                issue_id: "ID-1".into(),
                team_id: "TEAM".into(),
                ..Progress::default()
            };
            let id = store
                .save_landing(LandingRow {
                    project_slug: "app".into(),
                    owner: "o".into(),
                    repo: "r".into(),
                    pr: 1,
                    ticket: "APP-1".into(),
                    merge_sha: "merge".into(),
                    deploy_deadline_s: 30,
                    results_json: serde_json::to_string(&progress).unwrap(),
                    ..LandingRow::default()
                })
                .unwrap();
            Self {
                runtime: LandingRuntime {
                    control: None,
                    store,
                    tracker: tracker.clone(),
                    checks: checks.clone(),
                    config: LandingConfig {
                        auto_revert: false,
                        ..config()
                    },
                    done_state: done.into(),
                    changes_state: "In Progress".into(),
                    notifications: None,
                    reverter: None,
                },
                checks,
                tracker,
                id,
            }
        }
        async fn tick(&self, now: i64) {
            let row = self.runtime.store.get_landing(self.id).unwrap().unwrap();
            self.runtime.advance(row, now).await.unwrap();
        }
        fn row(&self) -> LandingRow {
            self.runtime.store.get_landing(self.id).unwrap().unwrap()
        }
        fn progress(&self) -> Progress {
            serde_json::from_str(&self.row().results_json).unwrap()
        }
    }

    #[tokio::test]
    async fn l2_awaiting_acceptance_does_not_pass_when_the_ticket_is_unreadable() {
        let rig = Rig::new("Done");
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        rig.runtime.store.save_landing(row.clone()).unwrap();
        assert!(rig.runtime.advance(row, 1).await.is_err());
        assert_eq!(rig.row().phase, LandingPhase::AwaitingAcceptance);
        assert!(rig.tracker.move_calls().is_empty());
        assert!(rig.tracker.create_comment_calls().is_empty());
    }

    #[tokio::test]
    async fn l2_awaiting_acceptance_row_recovers_its_checklist_instead_of_passing_unchecked() {
        let mut rig = Rig::new("Done");
        let mut tracker = Fake::new();
        tracker.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            description: Some("## Post-deploy acceptance\n- [ ] live UI is healthy".into()),
            ..Default::default()
        }];
        rig.runtime.tracker = Arc::new(tracker);
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        // L2 stored only the phase; it predates all acceptance_* Progress fields.
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::AwaitingAcceptance);
        assert_eq!(rig.progress().acceptance_items, vec!["live UI is healthy"]);
        assert!(
            rig.runtime
                .tracker
                .list_comments("ID-1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn land_worker_completes_and_restart_consumes_its_durable_verdict_once() {
        let rig = RepoRig::new(None, None);
        let mut o = enabled_orchestrator_for_view(rig.runtime.store.clone());
        let root = rig._dir.child("acceptance-workers");
        let workspace = crate::testsupport::mk_workspace(&root);
        let verdict = "```rhapsody-landing-verdict\n{\"pass\":true,\"items\":[{\"item\":\"live UI is healthy\",\"pass\":true,\"evidence\":\"GET /healthz returned 200\"}]}\n```";
        let result = serde_json::json!({"type":"result", "subtype":"success", "is_error":false, "session_id":"s", "result":verdict});
        let script = rig._dir.child("lander.sh");
        std::fs::write(&script, format!("#!/bin/bash\nread -r first\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}}' '{result}'\n")).unwrap();
        let project = &mut o.eff.as_mut().unwrap().projects[0];
        project.repo = rig.origin.clone();
        project.workspace = workspace.clone();
        project.agents.insert(
            "claude".into(),
            Arc::new(rhapsody_agent::claude::Runner::new(
                rhapsody_agent::claude::Config {
                    command: format!("bash {script}"),
                    workspace_root: root,
                    turn_timeout: Duration::from_secs(5),
                    ..Default::default()
                },
            )),
        );
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        let mut progress: Progress = serde_json::from_str(&row.results_json).unwrap();
        progress.no_contract = true;
        progress.acceptance_items = vec!["live UI is healthy".into()];
        rig.runtime.save(&mut row, &progress, 1).unwrap();
        let run_id = o
            .dispatch_landing(LandingRun {
                row: row.clone(),
                prompt: "verify live UI is healthy".into(),
            })
            .unwrap();
        progress.acceptance_run = Some(run_id);
        rig.runtime.save(&mut row, &progress, 1).unwrap();
        let mut rx = o.events_rx.lock().unwrap().take().unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(crate::control_loop::Event::WorkerExit(exit)) = rx.recv().await {
                    break exit;
                }
            }
        })
        .await
        .unwrap();
        assert!(!exit.failed, "{}", exit.err_msg);
        assert_eq!(exit.verdict_text.as_deref(), Some(verdict));
        o.on_worker_exit(exit);
        assert!(!o.claimed.contains("land:o/r#1"));
        assert!(o.retry_attempts.is_empty());
        assert_eq!(
            rig.runtime.store.get_run(run_id).unwrap().unwrap().outcome,
            "completed"
        );
        // Drop the volatile completion request, as a daemon restart would. Only the ledger remains.
        o.landing_requests.clear();
        rig.tick(2).await;
        rig.tick(3).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
        let comments = rig.runtime.tracker.list_comments("ID-1").await.unwrap();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].body.contains("- [x] live UI is healthy"));
        assert!(!Path::new(&workspace.path_for(&rig.origin, "land:o/r#1")).exists());
    }

    #[tokio::test]
    async fn interrupted_run_with_durable_verdict_is_consumed() {
        for passed in [true, false] {
            let mut rig = Rig::new("Done");
            let mut tracker = Fake::new();
            tracker.candidates = vec![rhapsody_core::Issue {
                id: "ID-1".into(),
                identifier: "APP-1".into(),
                description: Some("## Post-deploy acceptance\n- [ ] live UI is healthy".into()),
                ..Default::default()
            }];
            rig.tracker = Arc::new(tracker);
            rig.runtime.tracker = rig.tracker.clone();
            let run_id = rig
                .runtime
                .store
                .start_run(rhapsody_store::RunStart {
                    issue_identifier: "land:o/r#1".into(),
                    ..Default::default()
                })
                .unwrap();
            let mut row = rig.row();
            row.phase = LandingPhase::AwaitingAcceptance;
            let mut progress = rig.progress();
            progress.acceptance_items = vec!["live UI is healthy".into()];
            progress.acceptance_run = Some(run_id);
            rig.runtime.save(&mut row, &progress, 1).unwrap();
            let text = format!(
                "```rhapsody-landing-verdict\n{{\"pass\":{passed},\"items\":[{{\"item\":\"live UI is healthy\",\"pass\":{passed},\"evidence\":\"GET /healthz returned {}\"}}]}}\n```",
                if passed { 200 } else { 503 }
            );
            rig.runtime
                .store
                .append_events(
                    run_id,
                    &[rhapsody_store::EventRow {
                        seq: 1,
                        kind: "landing.verdict".into(),
                        text: serde_json::json!({"text": text, "failed": false}).to_string(),
                        ..Default::default()
                    }],
                )
                .unwrap();
            // Crash after the verdict write but before end_run: boot interrupts the run.
            assert_eq!(rig.runtime.store.mark_running_interrupted().unwrap(), 1);
            rig.tick(2).await;
            rig.tick(3).await;
            assert_eq!(
                rig.row().phase,
                if passed {
                    LandingPhase::Done
                } else {
                    LandingPhase::Reopened
                }
            );
            let comments = rig.tracker.create_comment_calls();
            assert_eq!(comments.len(), 1, "recovered evidence is posted once");
            assert!(comments[0].body.contains(if passed {
                "returned 200"
            } else {
                "returned 503"
            }));
            assert_eq!(
                rig.runtime
                    .store
                    .list_runs(RunFilter::default())
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn interrupted_run_without_durable_verdict_allows_acceptance_retry() {
        let rig = Rig::new("Done");
        let run_id = rig
            .runtime
            .store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "land:o/r#1".into(),
                ..Default::default()
            })
            .unwrap();
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        let mut progress = rig.progress();
        progress.acceptance_items = vec!["live UI is healthy".into()];
        progress.acceptance_run = Some(run_id);
        rig.runtime.save(&mut row, &progress, 1).unwrap();
        rig.runtime.store.mark_running_interrupted().unwrap();
        rig.runtime
            .acceptance(&mut row, &mut progress, 2)
            .await
            .unwrap();
        assert!(progress.acceptance_run.is_none());
        assert_eq!(rig.row().phase, LandingPhase::AwaitingAcceptance);
        assert!(rig.tracker.move_calls().is_empty());
        assert!(rig.tracker.create_comment_calls().is_empty());
    }

    #[tokio::test]
    async fn checklist_dispatches_land_run_with_read_only_profile() {
        let rig = Rig::new("Done");
        let mut o = enabled_orchestrator_for_view(rig.runtime.store.clone());
        o.spawn = Some(Box::new(|_, _, re| {
            assert_eq!(re.harness, "claude");
            assert!(re.landing.is_some());
            assert!(re.capabilities_section.is_empty());
        }));
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        let mut progress = rig.progress();
        progress.acceptance_items = vec!["dashboard shows activities".into()];
        rig.runtime.save(&mut row, &progress, 1).unwrap();
        let run = LandingRun {
            row,
            prompt: "dashboard shows activities; health: status=200".into(),
        };
        assert!(o.dispatch_landing(run.clone()).is_some());
        let re = o.running.get("land:o/r#1").unwrap();
        assert_eq!(re.issue.description.as_deref(), Some(run.prompt.as_str()));
        let run_id = re.run_id;
        assert_eq!(o.running.len(), 1);
        assert_eq!(o.dispatch_landing(run), Some(run_id));
    }

    #[tokio::test]
    async fn drain_pauses_land_dispatch_but_not_deterministic_checks() {
        let rig = Rig::new("Done");
        let mut o = enabled_orchestrator_for_view(rig.runtime.store.clone());
        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        let mut progress = rig.progress();
        progress.acceptance_items = vec!["item".into()];
        rig.runtime.save(&mut row, &progress, 1).unwrap();
        assert_eq!(
            o.dispatch_landing(LandingRun {
                row,
                prompt: "item".into()
            }),
            None
        );
        assert!(o.running.is_empty());
        // Same armed signal, normal deterministic runtime; the drain never gates its checks.
        let mut row = rig.row();
        row.phase = LandingPhase::WaitingForDeploy;
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(2).await;
        assert_eq!(rig.row().phase, LandingPhase::Watching);
        assert!(rig.progress().samples.iter().any(|s| s.name == "health"));
    }

    #[tokio::test]
    async fn landings_api_names_the_prs_a_landing_is_pausing() {
        let rig = Rig::new("Done");
        let mut orchestrator = enabled_orchestrator_for_view(rig.runtime.store.clone());
        orchestrator
            .store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                key: rhapsody_store::ReviewWatchKey {
                    owner: "o".into(),
                    repo: "r".into(),
                    number: 7,
                    reviewer: "alice".into(),
                },
                open: true,
                ..Default::default()
            })
            .unwrap();
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        let mut progress = rig.progress();
        progress.phase_since_s = Some(5);
        rig.runtime.save(&mut row, &progress, 50).unwrap();
        let view = orchestrator.landings_view(65).unwrap();
        assert_eq!(view.landings[0].pausing_auto_merge, vec!["o/r#7"]);
        assert_eq!(view.landings[0].phase_age_s, Some(60));
        orchestrator.eff.as_mut().unwrap().cfg.projects[0].landing = None;
        assert!(
            orchestrator
                .landing_merge_gate(&PrCoord::new("o", "r", 7))
                .is_err()
        );
        assert_eq!(
            orchestrator.landings_view(65).unwrap().landings[0].pausing_auto_merge,
            vec!["o/r#7"],
            "disabling landing does not release the durable project lock"
        );
    }

    fn enabled_orchestrator_for_view(store: Arc<dyn Store + Send + Sync>) -> Orchestrator {
        let mut o = Orchestrator::new("");
        o.store = store;
        let mut eff = empty_effective(Arc::new(Fake::new()));
        let mut p = crate::testsupport::empty_resolved_project("app", Arc::new(Fake::new()));
        p.repo = "https://github.com/o/r.git".into();
        eff.projects.push(p);
        eff.cfg.projects.push(rhapsody_config::Project {
            slugs: vec!["app".into()],
            repo: "https://github.com/o/r.git".into(),
            landing: Some(config()),
            ..Default::default()
        });
        o.eff = Some(eff);
        let mut teams = rhapsody_config::teams::Teams::disabled();
        teams.enabled = true;
        teams.review.auto_merge = true;
        teams.review.mode = rhapsody_config::teams::ReviewMode::Ticketless;
        o.teams = Some(teams);
        o
    }

    #[tokio::test]
    async fn retry_keeps_revert_linkage_and_never_opens_a_second_revert() {
        let mut rig = RepoRig::new(None, Some(GOOD));
        let gh = crate::landingrevert::tests::Github::new();
        rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh.clone()));
        let mut row = rig.row();
        row.phase = LandingPhase::Reverting;
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(5).await;
        rig.tick(35).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert_eq!(
            rig.runtime.action(rig.id, LandingAction::Retry, 100).await,
            LandingActionOutcome::Applied
        );
        rig.tick(101).await;
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        assert_eq!(rig.row().revert_pr, Some(2));
        assert_eq!(gh.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retry_revert_child_reopens_parent_reverting() {
        let rig = Rig::new("Done");
        let mut parent = rig.row();
        parent.phase = LandingPhase::Held;
        let mut child = parent.clone();
        child.id = 0;
        child.pr = 2;
        child.revert_of = Some(parent.id);
        let child_progress = Progress {
            held_from: Some("Watching".into()),
            ..rig.progress()
        };
        child.results_json = serde_json::to_string(&child_progress).unwrap();
        let child_id = rig.runtime.store.save_landing(child).unwrap();
        let mut progress = rig.progress();
        progress.held_from = Some("Reverting".into());
        progress.revert_landing = Some(child_id);
        rig.runtime.save(&mut parent, &progress, 5).unwrap();
        assert_eq!(
            rig.runtime
                .action(child_id, LandingAction::Retry, 100)
                .await,
            LandingActionOutcome::Applied
        );
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        assert_eq!(rig.progress().revert_landing, Some(child_id));
        assert_eq!(
            rig.runtime
                .store
                .get_landing(child_id)
                .unwrap()
                .unwrap()
                .revert_of,
            Some(parent.id)
        );
    }

    #[tokio::test]
    async fn stop_closes_unmerged_revert_and_holds() {
        let mut rig = RepoRig::new(None, Some(GOOD));
        let gh = crate::landingrevert::tests::Github::new();
        rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh.clone()));
        let mut row = rig.row();
        row.phase = LandingPhase::Reverting;
        row.revert_pr = Some(2);
        rig.runtime.store.save_landing(row).unwrap();
        assert_eq!(
            rig.runtime.action(rig.id, LandingAction::Stop, 10).await,
            LandingActionOutcome::Applied
        );
        assert_eq!(gh.state.lock().unwrap().0, PrStatus::Closed);
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert_eq!(
            rig.row().hold_reason.as_deref(),
            Some("revert stopped by operator")
        );
    }

    #[tokio::test]
    async fn stop_refused_after_revert_merged() {
        for racing in [false, true] {
            let mut rig = RepoRig::new(None, Some(GOOD));
            let gh = crate::landingrevert::tests::Github::new();
            if racing {
                gh.merged_during_stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                gh.state.lock().unwrap().0 = PrStatus::Merged;
            }
            rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh.clone()));
            let mut row = rig.row();
            row.phase = LandingPhase::Reverting;
            row.revert_pr = Some(2);
            rig.runtime.store.save_landing(row).unwrap();
            assert!(matches!(
                rig.runtime.action(rig.id, LandingAction::Stop, 10).await,
                LandingActionOutcome::Refused(_)
            ));
            assert_eq!(rig.row().phase, LandingPhase::Reverting);
            assert_eq!(gh.state.lock().unwrap().0, PrStatus::Merged);
        }
    }

    #[tokio::test]
    async fn no_checklist_means_no_landing_agent_dispatch() {
        let rig = Rig::new("Done");
        rig.tick(0).await;
        rig.tick(10).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
        assert!(rig.progress().acceptance_items.is_empty());
        assert!(rig.progress().acceptance_run.is_none());
        assert!(
            rig.runtime
                .store
                .list_issue_runs(RunFilter {
                    issue: "land:o/r#1".into(),
                    ..Default::default()
                })
                .unwrap()
                .is_empty()
        );
        let mut o = enabled_orchestrator_for_view(rig.runtime.store.clone());
        o.spawn = Some(Box::new(|_, _, _| {}));
        let mut row = rig.row();
        row.phase = LandingPhase::AwaitingAcceptance;
        rig.runtime.store.save_landing(row.clone()).unwrap();
        assert_eq!(
            o.dispatch_landing(LandingRun {
                row,
                prompt: "no checklist".into()
            }),
            None
        );
        assert!(o.running.is_empty());
    }

    #[tokio::test]
    async fn unmet_acceptance_item_fails_the_landing() {
        let mut rig = Rig::new("Done");
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            description: Some(
                "## Post-deploy acceptance\n- [ ] dashboard shows activities\n".into(),
            ),
            ..Default::default()
        }];
        rig.runtime.tracker = Arc::new(fake);
        rig.runtime.config.auto_revert = true;
        rig.tick(0).await;
        rig.tick(10).await;
        assert_eq!(rig.row().phase, LandingPhase::AwaitingAcceptance);
        let mut progress = rig.progress();
        progress.acceptance_run = Some(42);
        let mut row = rig.row();
        rig.runtime.save(&mut row, &progress, 10).unwrap();
        rig.runtime.acceptance_finished(rig.id, 42, r#"```rhapsody-landing-verdict
{"pass":false,"items":[{"item":"dashboard shows activities","pass":false,"evidence":"GET /activities returned zero"}]}
```"#, false, 11).await.unwrap();
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        assert!(
            rig.progress()
                .samples
                .iter()
                .any(|s| s.name == "acceptance: dashboard shows activities"
                    && !s.pass
                    && s.observed.contains("zero"))
        );
    }

    #[test]
    fn acceptance_requires_exact_checklist_and_evidence() {
        let items = vec!["dashboard shows activities".into()];
        for text in [
            "passed",
            r#"```rhapsody-landing-verdict
{"pass":true,"items":[]}
```"#,
            r#"```rhapsody-landing-verdict
{"pass":true,"items":[{"item":"dashboard shows activities","pass":true,"evidence":""}]}
```"#,
        ] {
            assert!(parse_acceptance(text, &items).is_err());
        }
    }

    #[tokio::test]
    async fn retry_held_deploy_timeout_restores_waiting_with_fresh_deadline() {
        let rig = Rig::new("Done");
        rig.tick(30).await;
        assert_eq!(
            rig.progress().held_from.as_deref(),
            Some("WaitingForDeploy")
        );
        assert_eq!(
            rig.runtime.action(rig.id, LandingAction::Retry, 100).await,
            LandingActionOutcome::Applied
        );
        assert_eq!(rig.row().phase, LandingPhase::WaitingForDeploy);
        assert_eq!(rig.row().deploy_deadline_s, 130);
        assert_eq!(rig.row().hold_reason, None);
        assert!(!rig.progress().fetched);
        assert_eq!(rig.progress().phase_since_s, Some(100));
    }

    #[tokio::test]
    async fn retry_refused_when_project_lock_taken_or_contract_problem() {
        let rig = Rig::new("Done");
        rig.tick(30).await;
        let mut sibling = rig.row();
        sibling.id = 0;
        sibling.pr = 2;
        sibling.phase = LandingPhase::Watching;
        let sibling_id = rig.runtime.store.save_landing(sibling).unwrap();
        assert!(matches!(
            rig.runtime.action(rig.id, LandingAction::Retry, 100).await,
            LandingActionOutcome::Refused(_)
        ));
        let mut sibling = rig.runtime.store.get_landing(sibling_id).unwrap().unwrap();
        sibling.phase = LandingPhase::Done;
        rig.runtime.store.save_landing(sibling).unwrap();
        let mut progress = rig.progress();
        progress.contract_hold = true;
        let mut row = rig.row();
        rig.runtime.save(&mut row, &progress, 30).unwrap();
        assert!(matches!(
            rig.runtime.action(rig.id, LandingAction::Retry, 100).await,
            LandingActionOutcome::Refused(_)
        ));
        assert_eq!(rig.row().phase, LandingPhase::Held);
    }

    #[tokio::test]
    async fn skip_marks_done_with_operator_evidence() {
        let rig = Rig::new("Done");
        assert_eq!(
            rig.runtime.action(rig.id, LandingAction::Skip, 5).await,
            LandingActionOutcome::Applied
        );
        assert_eq!(rig.row().phase, LandingPhase::Done);
        let comments = rig.tracker.create_comment_calls();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].body.contains("skipped by operator"));
        assert!(!comments[0].body.contains("skipped by operator | pass"));
        assert_eq!(rig.tracker.move_calls()[0].state_name, "Done");
    }

    #[tokio::test]
    async fn actions_return_not_found_and_refuse_wrong_phases() {
        let rig = Rig::new("Done");
        for action in [
            LandingAction::Retry,
            LandingAction::Skip,
            LandingAction::Stop,
        ] {
            assert_eq!(
                rig.runtime.action(999, action, 1).await,
                LandingActionOutcome::NotFound
            );
            for phase in [
                LandingPhase::WaitingForDeploy,
                LandingPhase::Watching,
                LandingPhase::AwaitingAcceptance,
                LandingPhase::Reverting,
                LandingPhase::Done,
                LandingPhase::Held,
                LandingPhase::Reopened,
            ] {
                let allowed = match action {
                    LandingAction::Retry => phase == LandingPhase::Held,
                    LandingAction::Skip => matches!(
                        phase,
                        LandingPhase::WaitingForDeploy
                            | LandingPhase::Watching
                            | LandingPhase::AwaitingAcceptance
                            | LandingPhase::Held
                    ),
                    LandingAction::Stop => phase == LandingPhase::Reverting,
                };
                if allowed {
                    continue;
                }
                let mut row = rig.row();
                row.phase = phase;
                rig.runtime.store.save_landing(row).unwrap();
                assert!(
                    matches!(
                        rig.runtime.action(rig.id, action, 1).await,
                        LandingActionOutcome::Refused(_)
                    ),
                    "{action:?} {phase:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn waits_until_live_probe_reports_merge_or_descendant() {
        for revision in ["merge", "descendant"] {
            let rig = Rig::new("Done");
            *rig.checks.live.lock().unwrap() =
                VecDeque::from([("old".into(), false), (revision.into(), true)]);
            rig.tick(0).await;
            assert_eq!(rig.row().phase, LandingPhase::WaitingForDeploy);
            rig.tick(1).await;
            assert_eq!(rig.row().phase, LandingPhase::Watching);
            assert_eq!(rig.row().watch_until_s, Some(11));
        }
    }
    #[tokio::test]
    async fn unknown_reported_sha_fetches_once_then_counts_not_live() {
        let rig = Rig::new("Done");
        rig.checks.live.lock().unwrap().clear();
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::WaitingForDeploy);
        assert_eq!(*rig.checks.calls.lock().unwrap(), vec![true, false]);
    }
    #[tokio::test]
    async fn deploy_timeout_holds_and_never_reverts() {
        let rig = Rig::new("Done");
        rig.tick(30).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert!(rig.row().hold_reason.unwrap().contains("deploy timeout"));
        assert_eq!(rig.row().revert_pr, None);
        assert_eq!(rig.tracker.create_comment_calls().len(), 1);
    }
    #[tokio::test]
    async fn flapping_check_under_threshold_passes() {
        let rig = Rig::new("Done");
        *rig.checks.checks.lock().unwrap() = VecDeque::from([false, true, false, true]);
        for now in [0, 1, 2, 3, 10] {
            rig.tick(now).await;
        }
        assert_eq!(rig.row().phase, LandingPhase::Done);
        assert!(rig.progress().samples.iter().any(|s| !s.pass));
    }
    #[tokio::test]
    async fn under_threshold_failure_at_deadline_resolves_on_the_next_sample() {
        for recovers in [true, false] {
            let mut rig = Rig::new("Done");
            let mut fake = Fake::new();
            fake.candidates = vec![rhapsody_core::Issue {
                id: "ID-1".into(),
                identifier: "APP-1".into(),
                ..Default::default()
            }];
            rig.tracker = Arc::new(fake);
            rig.runtime.tracker = rig.tracker.clone();
            let mut contract = rig.checks.contract.clone().unwrap().unwrap();
            contract.checks[0].every = Some(Duration::from_secs(30));
            let checks = Arc::new(Checks {
                contract: Ok(Some(contract)),
                live: Mutex::new(VecDeque::from([("merge".into(), true)])),
                checks: Mutex::new(VecDeque::from([false, recovers])),
                calls: Mutex::new(Vec::new()),
            });
            rig.checks = checks.clone();
            rig.runtime.checks = checks;
            rig.tick(0).await;
            assert_eq!(rig.row().phase, LandingPhase::Watching);
            rig.tick(10).await;
            rig.tick(29).await;
            assert_eq!(rig.row().phase, LandingPhase::Watching);
            assert_eq!(
                rig.progress()
                    .samples
                    .iter()
                    .filter(|s| s.name == "health")
                    .count(),
                1
            );
            rig.tick(30).await;
            assert_eq!(
                rig.row().phase,
                if recovers {
                    LandingPhase::Done
                } else {
                    LandingPhase::Reopened
                }
            );
            assert_eq!(rig.row().watch_until_s, Some(10));
            let samples = rig.progress().samples;
            assert_eq!(samples.iter().filter(|s| s.name == "health").count(), 2);
        }
    }
    #[tokio::test]
    async fn failure_with_auto_revert_enters_reverting_before_reopening() {
        let mut rig = Rig::new("Done");
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            ..Default::default()
        }];
        rig.runtime.tracker = Arc::new(fake);
        rig.runtime.config.auto_revert = true;
        *rig.checks.checks.lock().unwrap() = VecDeque::from([false, false]);
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        assert!(!rig.runtime.store.subject_resume_pending("APP-1").unwrap());
    }
    #[tokio::test]
    async fn failed_revert_landing_holds_and_never_reverts_again() {
        let mut rig = Rig::new("Done");
        rig.runtime.notifications = Some(Arc::new(crate::breaker::NotificationsState::default()));
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            ..Default::default()
        }];
        rig.runtime.tracker = Arc::new(fake);
        rig.runtime.config.auto_revert = true;
        let mut row = rig.row();
        row.revert_of = Some(99);
        rig.runtime.store.save_landing(row).unwrap();
        *rig.checks.checks.lock().unwrap() = VecDeque::from([false, false]);
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert_eq!(rig.row().revert_pr, None);
        assert!(
            rig.row()
                .hold_reason
                .unwrap()
                .contains("revert landing failed")
        );
        assert!(
            rig.runtime.tracker.list_comments("ID-1").await.unwrap()[0]
                .body
                .contains("health")
        );
        assert!(!rig.runtime.store.subject_resume_pending("APP-1").unwrap());
        rig.tick(2).await;
        assert_eq!(
            rig.runtime.notifications.as_ref().unwrap().pending().len(),
            1
        );
    }
    #[tokio::test]
    async fn sustained_failure_reopens_with_review_feedback() {
        let mut rig = Rig::new("Done");
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            description: Some("Original acceptance".into()),
            ..Default::default()
        }];
        rig.tracker = Arc::new(fake);
        rig.runtime.tracker = rig.tracker.clone();
        *rig.checks.checks.lock().unwrap() = VecDeque::from([false, false]);
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Reopened);
        assert_eq!(rig.row().revert_pr, None);
        let descriptions = rig.tracker.description_update_calls();
        assert_eq!(descriptions.len(), 1);
        assert!(descriptions[0].1.contains("REVIEW FEEDBACK"));
        assert!(descriptions[0].1.contains("health: status=503"));
        assert!(descriptions[0].1.ends_with("Original acceptance"));
        assert_eq!(rig.tracker.move_calls()[0].state_name, "In Progress");
    }
    #[tokio::test]
    async fn auto_revert_false_reopens_without_revert() {
        let mut rig = Rig::new("Done");
        rig.runtime.config.auto_revert = false;
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            ..Default::default()
        }];
        rig.runtime.tracker = Arc::new(fake);
        *rig.checks.checks.lock().unwrap() = VecDeque::from([false, false]);
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Reopened);
        assert_eq!(rig.row().revert_pr, None);
    }
    #[tokio::test]
    async fn revert_pr_not_merged_by_deadline_holds_and_notifies() {
        for (gate, refusal) in [
            ("disabled", "auto-merge is disabled"),
            ("human", "the ticket has a human-hold label"),
            ("unprimed", "human holds have not been read"),
            ("review", "no reviewer is watching this pull request"),
            ("ci", "a check is failing or has not finished"),
        ] {
            let mut rig = RepoRig::new(None, Some(GOOD));
            let gh = crate::landingrevert::tests::Github::new();
            let mut reverter = crate::landingrevert::tests::reverter(&rig, gh.clone());
            let deps = Arc::get_mut(&mut reverter).unwrap();
            deps.auto_merge = gate != "disabled";
            if gate == "human" {
                deps.holds.note_human_label("APP-1");
            } else if gate == "unprimed" {
                deps.holds = Arc::new(crate::dispatch::HumanHoldLedger::default());
            } else if gate == "ci" {
                let checks = Arc::new(ScriptedGh {
                    origin: rig.origin.clone(),
                    merged: Mutex::new(BTreeMap::new()),
                    calls: Mutex::new(Vec::new()),
                    branches: Mutex::new(BTreeMap::from([(2, "feature".into())])),
                    app: None,
                    ci_green: std::sync::atomic::AtomicBool::new(false),
                });
                *gh.state.lock().unwrap() =
                    (PrStatus::Open, checks.snapshot(2).head_sha, String::new());
                deps.merges = Arc::new(crate::runautomerge::AutoMergeDeps {
                    prs: checks.clone(),
                    mergestate: checks.clone(),
                    policy: checks.clone(),
                    updater: checks.clone(),
                    checks: checks.clone(),
                    merger: checks,
                    allow: HeadAllowlist::none(),
                    approvals: None,
                    ledger: Arc::new(crate::runautomerge::AutoMergeLedger::default()),
                });
            }
            rig.runtime.config.revert_review = gate == "review";
            rig.runtime.reverter = Some(reverter.clone());
            let mut row = rig.row();
            row.phase = LandingPhase::Reverting;
            rig.runtime.store.save_landing(row).unwrap();
            rig.tick(5).await;
            assert_eq!(rig.row().revert_pr, Some(2));
            rig.tick(20).await;
            assert_eq!(rig.row().phase, LandingPhase::Reverting);
            rig.restart();
            let persisted: serde_json::Value =
                serde_json::from_str(&rig.row().results_json).unwrap();
            assert_eq!(persisted["revert_opened_at_s"], 5);
            assert_eq!(persisted["revert_merge_refusal"], refusal);
            rig.runtime.reverter = Some(reverter);
            rig.runtime.notifications =
                Some(Arc::new(crate::breaker::NotificationsState::default()));
            rig.tick(34).await;
            assert_eq!(rig.row().phase, LandingPhase::Reverting);
            rig.tick(35).await;
            assert_eq!(rig.row().phase, LandingPhase::Held, "{gate}");
            let reason =
                format!("revert PR #2 not merged within 30s; last merge-gate refusal: {refusal}");
            assert_eq!(rig.row().hold_reason.as_deref(), Some(reason.as_str()));
            let progress: serde_json::Value =
                serde_json::from_str(&rig.row().results_json).unwrap();
            assert_eq!(progress["revert_opened_at_s"], 5);
            let notices = rig.runtime.notifications.as_ref().unwrap().pending();
            assert_eq!(
                notices.len(),
                1,
                "no repeated Reverting notice after restart"
            );
            assert_eq!(notices[0].title, "Landing held");
            assert_eq!(notices[0].body, reason);
            assert_eq!(gh.state.lock().unwrap().0, PrStatus::Open);
            assert_eq!(gh.opened.lock().unwrap().len(), 1);
            assert!(
                rig.runtime
                    .tracker
                    .list_comments("ID-1")
                    .await
                    .unwrap()
                    .iter()
                    .any(|c| c.body.contains(&reason))
            );
            rig.tick(100).await;
            assert_eq!(
                rig.runtime.notifications.as_ref().unwrap().pending().len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn reconciled_revert_uses_original_opening_time_but_merged_pr_still_lands() {
        for status in [PrStatus::Open, PrStatus::Merged] {
            let mut rig = RepoRig::new(None, Some(GOOD));
            let gh = crate::landingrevert::tests::Github::new();
            *gh.existing.lock().unwrap() = Some(2);
            *gh.state.lock().unwrap() = (
                status,
                rig.row().merge_sha.clone(),
                rig.row().merge_sha.clone(),
            );
            rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh.clone()));
            let mut row = rig.row();
            row.phase = LandingPhase::Reverting;
            rig.runtime.store.save_landing(row).unwrap();
            // Crash after GitHub creation, before saving its PR number/time. First resumed tick
            // is already past createdAt + deploy_timeout, not a fresh thirty-second window.
            rig.tick(100).await;
            assert_eq!(rig.row().revert_pr, Some(2));
            assert!(gh.opened.lock().unwrap().is_empty());
            let progress: Progress = serde_json::from_str(&rig.row().results_json).unwrap();
            assert_eq!(progress.revert_opened_at_s, Some(5));
            if status == PrStatus::Open {
                assert_eq!(rig.row().phase, LandingPhase::Held);
                assert_eq!(
                    rig.row().hold_reason.as_deref(),
                    Some(
                        "revert PR #2 not merged within 30s; last merge-gate refusal: not yet attempted"
                    )
                );
                assert_eq!(progress.revert_landing, None);
            } else {
                assert_eq!(rig.row().phase, LandingPhase::Reverting);
                let child = rig
                    .runtime
                    .store
                    .get_landing(progress.revert_landing.unwrap())
                    .unwrap()
                    .unwrap();
                assert_eq!(child.revert_of, Some(rig.id));
                assert_eq!(child.deploy_deadline_s, 130);
            }
        }
    }

    #[tokio::test]
    async fn entering_reverting_pushes_an_operator_notification() {
        let mut rig = RepoRig::new(
            None,
            Some(
                "version: 1\nlive: {command: [git, rev-parse, HEAD]}\nchecks:\n- name: health\n  command: ['false']\n  every: 1s\n",
            ),
        );
        rig.runtime.config.auto_revert = true;
        rig.runtime.notifications = Some(Arc::new(crate::breaker::NotificationsState::default()));
        let gh = crate::landingrevert::tests::Github::new();
        rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh));
        rig.tick(0).await;
        assert!(
            rig.runtime
                .notifications
                .as_ref()
                .unwrap()
                .pending()
                .is_empty()
        );
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        let notices = rig.runtime.notifications.as_ref().unwrap().pending();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Landing failed: reverting");
        assert!(notices[0].body.contains("health"));
        assert_eq!(notices[0].ticket, "APP-1");
        rig.tick(5).await;
        let notices = rig.runtime.notifications.as_ref().unwrap().pending();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[1].title, "Landing failed: reverting");
        assert!(notices[1].body.contains("health"));
        assert!(notices[1].body.contains("revert PR #2"));
        assert_eq!(notices[1].pr, "o/r#2");
        rig.tick(6).await;
        assert_eq!(
            rig.runtime.notifications.as_ref().unwrap().pending().len(),
            2
        );
    }

    #[tokio::test]
    async fn revert_gets_its_own_landing_and_original_reopens_after_it_passes() {
        let mut rig = RepoRig::new(Some(GOOD), Some(GOOD));
        let gh = crate::landingrevert::tests::Github::new();
        *gh.state.lock().unwrap() = (
            PrStatus::Merged,
            rig.row().merge_sha.clone(),
            rig.row().merge_sha.clone(),
        );
        *gh.existing.lock().unwrap() = Some(2);
        rig.runtime.reverter = Some(crate::landingrevert::tests::reverter(&rig, gh));
        let mut row = rig.row();
        row.phase = LandingPhase::Reverting;
        let mut progress: Progress = serde_json::from_str(&row.results_json).unwrap();
        progress.checks.insert(
            "health".into(),
            CheckState {
                failures: 2,
                ..Default::default()
            },
        );
        progress.samples.push(Sample {
            name: "health".into(),
            observed: "status=503".into(),
            pass: false,
            at: 0,
        });
        row.results_json = serde_json::to_string(&progress).unwrap();
        rig.runtime.store.save_landing(row).unwrap();
        rig.runtime
            .tracker
            .update_issue_description("ID-1", "## Post-deploy acceptance\n- [ ] original feature")
            .await
            .unwrap();
        // A human merge queued before the revert must not keep recovery behind its own parent.
        let queued_id = rig
            .runtime
            .store
            .save_landing(LandingRow {
                pr: 3,
                results_json: serde_json::to_string(&Progress {
                    queued: true,
                    issue_id: "ID-1".into(),
                    team_id: "TEAM".into(),
                    ..Default::default()
                })
                .unwrap(),
                id: 0,
                phase: LandingPhase::WaitingForDeploy,
                ..rig.row()
            })
            .unwrap();
        rig.tick(1).await;
        let child = rig
            .runtime
            .store
            .load_open_landings()
            .unwrap()
            .into_iter()
            .find(|r| r.revert_of == Some(rig.id))
            .expect("revert's own landing");
        assert_eq!(child.pr, 2);
        assert_eq!(child.merge_sha, rig.row().merge_sha);
        rig.runtime
            .advance(
                rig.runtime.store.get_landing(queued_id).unwrap().unwrap(),
                2,
            )
            .await
            .unwrap();
        assert_eq!(
            rig.runtime
                .store
                .get_landing(queued_id)
                .unwrap()
                .unwrap()
                .watch_until_s,
            None
        );
        let revert = rig.runtime.reverter.clone();
        rig.restart();
        rig.runtime.reverter = revert;
        rig.runtime.advance(child.clone(), 2).await.unwrap();
        rig.runtime
            .advance(
                rig.runtime.store.get_landing(child.id).unwrap().unwrap(),
                12,
            )
            .await
            .unwrap();
        assert_eq!(
            rig.runtime
                .store
                .get_landing(child.id)
                .unwrap()
                .unwrap()
                .phase,
            LandingPhase::Done
        );
        assert_eq!(
            rig.runtime
                .tracker
                .fetch_issue_by_identifier("APP-1")
                .await
                .unwrap()
                .unwrap()
                .state,
            "In Review",
            "the recovery must not finish the original ticket"
        );
        rig.tick(13).await;
        assert_eq!(rig.row().phase, LandingPhase::Reopened);
        let ticket = rig
            .runtime
            .tracker
            .fetch_issue_by_identifier("APP-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ticket.state, "In Progress");
        assert!(ticket.description.unwrap().contains("health: status=503"));
        assert!(rig.runtime.store.subject_resume_pending("APP-1").unwrap());
    }
    #[test]
    fn watching_a_merged_revert_cannot_finish_the_original_ticket() {
        let (mut o, tracker) = orch(true);
        o.store()
            .save_landing(LandingRow {
                project_slug: "app".into(),
                owner: "o".into(),
                repo: "r".into(),
                pr: 1,
                phase: LandingPhase::Reverting,
                revert_pr: Some(2),
                ..Default::default()
            })
            .unwrap();
        let mut watch = watch_row();
        watch.key.number = 2;
        o.store().save_review_watch(watch).unwrap();
        let mut observation = merged_observation();
        observation.pr.number = 2;
        assert!(o.handle_review_sweep(&[observation]).done.is_empty());
        assert!(tracker.move_calls().is_empty());
        assert_eq!(o.store().load_open_landings().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn threshold_recorded_before_crash_still_reopens_without_another_check() {
        let mut rig = Rig::new("Done");
        let mut fake = Fake::new();
        fake.candidates = vec![rhapsody_core::Issue {
            id: "ID-1".into(),
            identifier: "APP-1".into(),
            ..Default::default()
        }];
        rig.tracker = Arc::new(fake);
        rig.runtime.tracker = rig.tracker.clone();
        rig.tick(0).await;
        let mut row = rig.row();
        let mut progress = rig.progress();
        progress.checks.get_mut("health").unwrap().failures = 2;
        progress.checks.get_mut("health").unwrap().pass = false;
        progress.checks.get_mut("health").unwrap().next_s = 100;
        progress.samples.push(Sample {
            name: "health".into(),
            pass: false,
            observed: "status=503".into(),
            at: 1,
        });
        row.results_json = serde_json::to_string(&progress).unwrap();
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(2).await;
        assert_eq!(rig.row().phase, LandingPhase::Reopened);
    }
    #[tokio::test]
    async fn unresolved_merge_lookup_is_retried_then_held_at_deploy_deadline() {
        let rig = Rig::new("Done");
        let mut row = rig.row();
        row.merge_sha.clear();
        rig.runtime.store.save_landing(row).unwrap();
        assert!(rig.runtime.advance(rig.row(), 0).await.is_err());
        assert_eq!(rig.row().phase, LandingPhase::WaitingForDeploy);
        rig.tick(30).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
    }
    #[tokio::test]
    async fn pass_posts_one_evidence_comment_and_moves_to_done_state() {
        let rig = Rig::new("Done");
        rig.tick(0).await;
        rig.tick(10).await;
        rig.tick(11).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
        let comments = rig.tracker.create_comment_calls();
        assert_eq!(comments.len(), 1);
        assert!(
            comments[0]
                .body
                .contains("name | observed | pass/fail | at")
        );
        assert!(comments[0].body.contains("healthy"));
        assert_eq!(rig.tracker.move_calls()[0].state_name, "Done");
    }
    #[tokio::test]
    async fn pass_without_done_state_posts_evidence_and_leaves_state() {
        let rig = Rig::new("");
        rig.tick(0).await;
        rig.tick(10).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
        assert_eq!(rig.tracker.create_comment_calls().len(), 1);
        assert!(rig.tracker.move_calls().is_empty());
    }
    #[tokio::test]
    async fn restart_mid_watch_resumes_from_stored_deadlines_without_double_comment() {
        let mut rig = RepoRig::new(
            None,
            Some(
                "version: 1\nlive: {command: [git, rev-parse, HEAD]}\nchecks:\n- name: health\n  command: [echo, healthy]\n  every: 1s\n",
            ),
        );
        rig.tick(0).await;
        rig.tick(4).await;
        let before_comment = rig.row();
        rig.restart();
        rig.tick(10).await;
        assert_eq!(rig.row().watch_until_s, Some(10));
        assert_eq!(rig.row().phase, LandingPhase::Done);
        // Simulate the crash boundary after tracker accepted the comment but before the local
        // outcome save. Both the store and file adapter are reopened, not Arc-cloned.
        let mut progress: Progress = serde_json::from_str(&before_comment.results_json).unwrap();
        progress.pending = Some("Done".into());
        let mut interrupted = before_comment;
        interrupted.results_json = serde_json::to_string(&progress).unwrap();
        rig.runtime.store.save_landing(interrupted).unwrap();
        rig.restart();
        rig.tick(11).await;
        assert_eq!(
            rig.runtime
                .tracker
                .list_comments("ID-1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(rig.row().phase, LandingPhase::Done);
    }

    fn orch(enabled: bool) -> (Orchestrator, Arc<Fake>) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.cfg.projects = vec![rhapsody_config::model::Project {
            slugs: vec!["app".into()],
            landing: enabled.then(config),
            ..Default::default()
        }];
        let mut project = crate::testsupport::empty_resolved_project("app", tracker.clone());
        project.repo = "https://github.com/o/r.git".into();
        eff.projects = vec![project];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(rhapsody_config::teams::Teams {
            enabled: true,
            roster: vec![rhapsody_config::teams::Identity {
                name: "bob".into(),
                ..Default::default()
            }],
            review: rhapsody_config::teams::Review {
                mode: rhapsody_config::teams::ReviewMode::Ticketless,
                done_state: "Done".into(),
                ..Default::default()
            },
            ..rhapsody_config::teams::Teams::disabled()
        });
        o.set_store(Arc::new(Sqlite::open(StorePath::InMemory).unwrap()));
        o.store()
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "APP-1".into(),
                issue_id: "ID-1".into(),
                team_id: "TEAM".into(),
                project_slug: "app".into(),
                ..Default::default()
            })
            .unwrap();
        (o, tracker)
    }
    fn watch_row() -> rhapsody_store::ReviewWatchRow {
        rhapsody_store::ReviewWatchRow {
            key: rhapsody_store::ReviewWatchKey {
                owner: "o".into(),
                repo: "r".into(),
                number: 1,
                reviewer: "bob".into(),
            },
            introduced_by: "handoff:APP-1".into(),
            open: true,
            ..Default::default()
        }
    }
    #[tokio::test]
    async fn landing_disabled_review_done_behaviour_is_byte_identical() {
        let (mut o, tracker) = orch(false);
        o.store().save_review_watch(watch_row()).unwrap();
        let report = o.handle_review_sweep(&[merged_observation()]);
        assert_eq!(report.done.len(), 1);
        let plan = report.done.into_iter().next().unwrap();
        o.control().finish_review_ticket(plan).await;
        let calls = tracker.move_calls();
        assert_eq!(
            serde_json::json!({"issue": calls[0].issue_id, "team": calls[0].team_id,
            "state": calls[0].state_name})
            .to_string(),
            "{\"issue\":\"ID-1\",\"state\":\"Done\",\"team\":\"TEAM\"}"
        );
        assert!(o.store().load_open_landings().unwrap().is_empty());
    }
    #[test]
    fn merge_with_landing_enabled_creates_row_and_does_not_move_ticket() {
        let (mut o, tracker) = orch(true);
        o.store().save_review_watch(watch_row()).unwrap();
        let report = o.handle_review_sweep(&[merged_observation()]);
        assert!(report.done.is_empty());
        assert_eq!(o.store().load_open_landings().unwrap().len(), 1);
        assert!(tracker.move_calls().is_empty());
    }
    fn merged_observation() -> crate::prstate::PrObservation {
        crate::prstate::PrObservation {
            pr: PrCoord::new("o", "r", 1),
            lookup: crate::ghsummons::PrLookup::Found(crate::ghsummons::PrSnapshot {
                head_sha: "head-not-merge".into(),
                status: crate::ghsummons::PrStatus::Merged,
                is_draft: Some(false),
                merged_at: None,
                head_repo: "o/r".into(),
                merge_state: String::new(),
            }),
            unchanged_from: Vec::new(),
            head_patch_id: String::new(),
        }
    }
    #[test]
    fn second_merge_queues_behind_first_landing() {
        let (o, _) = orch(true);
        let row = watch_row();
        o.admit_landing(std::slice::from_ref(&row), &PrCoord::new("o", "r", 1), 0)
            .unwrap();
        let mut second = row;
        second.key.number = 2;
        o.admit_landing(&[second], &PrCoord::new("o", "r", 2), 5)
            .unwrap();
        let rows = o.store().load_open_landings().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            serde_json::from_str::<Progress>(&rows[1].results_json)
                .unwrap()
                .queued
        );
        assert_eq!(
            o.store()
                .open_landing_for_project("app")
                .unwrap()
                .unwrap()
                .pr,
            1
        );
    }

    #[tokio::test]
    async fn merge_to_admission_window_blocks_a_second_auto_merge() {
        let dir = TempDir::new();
        let store =
            Arc::new(Sqlite::open(StorePath::Disk(dir.child("landing.db").into())).unwrap());
        let (mut o, _) = orch(true);
        o.set_store(store.clone());
        o.teams.as_mut().unwrap().review.auto_merge = true;
        o.human_holds.begin_pass(true);
        let mut observations = Vec::new();
        for pr in [1, 2] {
            let mut watch = watch_row();
            watch.key.number = pr;
            watch.status = rhapsody_store::REVIEW_STATUS_APPROVED.into();
            watch.last_reviewed_sha = "head".into();
            o.store().save_review_watch(watch).unwrap();
            let mut obs = merged_observation();
            obs.pr.number = pr;
            if let PrLookup::Found(snap) = &mut obs.lookup {
                snap.status = PrStatus::Open;
                snap.head_sha = "head".into();
            }
            observations.push(obs);
        }
        let plans = o.handle_review_sweep(&observations).merge;
        assert_eq!(plans.len(), 1);
        // Before invoking GitHub, admission must already survive loss of the control process.
        let reopened =
            Arc::new(Sqlite::open(StorePath::Disk(dir.child("landing.db").into())).unwrap());
        let marker = reopened
            .open_landing_for_project("app")
            .unwrap()
            .expect("durable merge-time lock");
        let rig = Rig::new("Done");
        let runtime = LandingRuntime {
            store: reopened.clone(),
            ..rig.runtime
        };
        runtime.advance(marker.clone(), i64::MAX).await.unwrap();
        assert_eq!(
            reopened
                .get_landing(marker.id)
                .unwrap()
                .unwrap()
                .watch_until_s,
            None
        );
        assert!(rig.tracker.move_calls().is_empty());
        assert!(rig.tracker.create_comment_calls().is_empty());
        assert!(rig.checks.calls.lock().unwrap().is_empty());
        let (mut restarted, _) = orch(true);
        restarted.set_store(reopened);
        restarted.teams.as_mut().unwrap().review.auto_merge = true;
        restarted.human_holds.begin_pass(true);
        assert!(
            restarted
                .handle_review_sweep(&[observations[1].clone()])
                .merge
                .is_empty()
        );
        assert_eq!(restarted.store().load_open_landings().unwrap().len(), 1);
    }

    pub(crate) struct RepoRig {
        pub(crate) _dir: TempDir,
        pub(crate) origin: String,
        pub(crate) runtime: LandingRuntime,
        id: i64,
    }
    pub(crate) fn git(path: &str, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.org")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.org")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    impl RepoRig {
        fn restart(&mut self) {
            self.runtime = LandingRuntime {
                control: None,
                store: Arc::new(
                    Sqlite::open(StorePath::Disk(self._dir.child("landing.db").into())).unwrap(),
                ),
                tracker: Arc::new(rhapsody_tracker::file::new(
                    rhapsody_tracker::file::Config {
                        source: self._dir.child("issues.json"),
                        ..Default::default()
                    },
                )),
                checks: Arc::new(RepoChecks {
                    manager: crate::testsupport::mk_workspace(&self._dir.child("workspaces")),
                    repo_url: self.origin.clone(),
                    config: self.runtime.config.clone(),
                }),
                config: self.runtime.config.clone(),
                done_state: self.runtime.done_state.clone(),
                changes_state: self.runtime.changes_state.clone(),
                notifications: None,
                reverter: None,
            };
        }
        pub(crate) fn new(before: Option<&str>, after: Option<&str>) -> Self {
            Self::build(before, after, true)
        }
        fn build(before: Option<&str>, after: Option<&str>, admitted: bool) -> Self {
            let dir = TempDir::new();
            let origin = dir.child("origin");
            std::fs::create_dir(&origin).unwrap();
            git(&origin, &["init", "-b", "main"]);
            let commit = |contract: Option<&str>, message: &str| {
                let path = Path::new(&origin).join(".rhapsody/landing.yaml");
                if let Some(contract) = contract {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, contract).unwrap();
                } else if path.exists() {
                    std::fs::remove_file(path).unwrap();
                }
                git(&origin, &["add", "-A"]);
                git(&origin, &["commit", "--allow-empty", "-m", message]);
            };
            commit(before, "before");
            git(&origin, &["checkout", "-b", "feature"]);
            commit(after, "feature");
            git(&origin, &["checkout", "main"]);
            git(&origin, &["merge", "--no-ff", "feature", "-m", "merge"]);
            let sha = git(&origin, &["rev-parse", "HEAD"]);
            let tracker_path = dir.child("issues.json");
            std::fs::write(&tracker_path, r#"{"issues":[{"id":"ID-1","identifier":"APP-1","team_id":"TEAM","title":"ship","state":"In Review","description":"original acceptance"}]}"#).unwrap();
            let tracker = Arc::new(rhapsody_tracker::file::new(
                rhapsody_tracker::file::Config {
                    source: tracker_path,
                    ..Default::default()
                },
            ));
            let store =
                Arc::new(Sqlite::open(StorePath::Disk(dir.child("landing.db").into())).unwrap());
            let progress = Progress {
                issue_id: "ID-1".into(),
                team_id: "TEAM".into(),
                ..Default::default()
            };
            let id = if admitted {
                store
                    .save_landing(LandingRow {
                        project_slug: "app".into(),
                        owner: "o".into(),
                        repo: "r".into(),
                        pr: 1,
                        ticket: "APP-1".into(),
                        merge_sha: sha,
                        deploy_deadline_s: 30,
                        results_json: serde_json::to_string(&progress).unwrap(),
                        ..Default::default()
                    })
                    .unwrap()
            } else {
                0
            };
            let runtime = LandingRuntime {
                control: None,
                store,
                tracker,
                checks: Arc::new(RepoChecks {
                    manager: crate::testsupport::mk_workspace(&dir.child("workspaces")),
                    repo_url: origin.clone(),
                    config: config(),
                }),
                config: LandingConfig {
                    auto_revert: false,
                    ..config()
                },
                done_state: "Done".into(),
                changes_state: "In Progress".into(),
                notifications: None,
                reverter: None,
            };
            Self {
                _dir: dir,
                origin,
                runtime,
                id,
            }
        }
        pub(crate) fn row(&self) -> LandingRow {
            self.runtime.store.get_landing(self.id).unwrap().unwrap()
        }
        pub(crate) async fn tick(&self, now: i64) {
            self.runtime.advance(self.row(), now).await.unwrap();
        }
    }
    const GOOD: &str = "version: 1\nlive: {command: [git, rev-parse, HEAD]}\nchecks: []\n";

    #[tokio::test]
    async fn broken_contract_in_merged_commit_holds_with_parse_error() {
        let rig = RepoRig::new(Some(GOOD), Some("version: 1\nlive: {htp: wrong}\n"));
        // The current branch now fixes it; the landing must still read the merged commit's error.
        std::fs::write(Path::new(&rig.origin).join(".rhapsody/landing.yaml"), GOOD).unwrap();
        git(&rig.origin, &["add", "-A"]);
        git(&rig.origin, &["commit", "-m", "fix after merge"]);
        rig.tick(0).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert!(rig.row().hold_reason.unwrap().contains("live.htp"));
        assert_eq!(rig.row().revert_pr, None);
    }
    #[tokio::test]
    async fn checkout_hook_cannot_replace_the_merged_contract() {
        let mut rig = RepoRig::new(Some(GOOD), Some("version: 1\nlive: {htp: wrong}\n"));
        let manager = Arc::new(
            Manager::new(rhapsody_workspace::Config {
                root: rig._dir.child("hook-workspaces"),
                hooks: rhapsody_workspace::HookScripts {
                    after_create: format!("printf '%s' '{}' > .rhapsody/landing.yaml", GOOD),
                    ..Default::default()
                },
                hook_timeout: Duration::from_secs(30),
            })
            .unwrap(),
        );
        rig.runtime.checks = Arc::new(RepoChecks {
            manager,
            repo_url: rig.origin.clone(),
            config: config(),
        });
        rig.tick(0).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert!(
            rig.row()
                .hold_reason
                .as_deref()
                .is_some_and(|r| r.contains("live.htp")),
            "{:?}",
            rig.row()
        );
        let worktree = crate::testsupport::mk_workspace(&rig._dir.child("hook-workspaces"))
            .path_for(&rig.origin, &RepoChecks::key(&rig.row()));
        assert_eq!(
            std::fs::read_to_string(Path::new(&worktree).join(".rhapsody/landing.yaml")).unwrap(),
            GOOD,
            "the hook must actually run and replace the checkout for this regression to be meaningful"
        );
    }
    #[tokio::test]
    async fn contract_deleted_by_the_merge_holds() {
        let rig = RepoRig::new(Some(GOOD), None);
        rig.tick(0).await;
        assert_eq!(rig.row().phase, LandingPhase::Held);
        assert_eq!(
            rig.row().hold_reason.as_deref(),
            Some("landing contract removed by this change")
        );
        assert_eq!(rig.row().revert_pr, None);
    }
    #[tokio::test]
    async fn no_contract_file_skips_checks_and_still_completes() {
        for checklist in [false, true] {
            let rig = RepoRig::new(None, None);
            if checklist {
                rig.runtime
                    .tracker
                    .update_issue_description(
                        "ID-1",
                        "## Post-deploy acceptance\n- [ ] see the change",
                    )
                    .await
                    .unwrap();
            }
            rig.tick(0).await;
            assert_eq!(
                rig.row().phase,
                if checklist {
                    LandingPhase::AwaitingAcceptance
                } else {
                    LandingPhase::Done
                }
            );
            assert_eq!(rig.row().watch_until_s, None);
            let progress: Progress = serde_json::from_str(&rig.row().results_json).unwrap();
            assert!(progress.samples.is_empty());
            if checklist {
                assert!(
                    rig.runtime
                        .tracker
                        .list_comments("ID-1")
                        .await
                        .unwrap()
                        .is_empty()
                );
                let mut progress = progress;
                progress.acceptance_run = Some(42);
                let mut row = rig.row();
                rig.runtime.save(&mut row, &progress, 0).unwrap();
                rig.runtime.acceptance_finished(rig.id, 42, "```rhapsody-landing-verdict\n{\"pass\":true,\"items\":[{\"item\":\"see the change\",\"pass\":true,\"evidence\":\"GET /change shows it\"}]}\n```", false, 1).await.unwrap();
                assert_eq!(rig.row().phase, LandingPhase::Done);
            }
            let comments = rig.runtime.tracker.list_comments("ID-1").await.unwrap();
            assert_eq!(comments.len(), 1);
            assert!(
                comments[0]
                    .body
                    .contains("no landing contract: the deploy was not observed")
            );
        }
    }

    #[tokio::test]
    async fn no_contract_does_not_wait_for_a_deploy_even_after_deadline() {
        let rig = RepoRig::new(None, None);
        rig.tick(100).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
    }
    #[tokio::test]
    async fn queued_landing_gets_its_own_window_and_moves_to_landing_while_waiting() {
        let mut rig = Rig::new("Done");
        rig.runtime.config.state = "Landing".into();
        rig.tick(0).await;
        let mut second = rig.row();
        second.id = 0;
        second.pr = 2;
        second.phase = LandingPhase::WaitingForDeploy;
        second.watch_until_s = None;
        second.results_json = serde_json::to_string(&Progress {
            issue_id: "ID-2".into(),
            team_id: "TEAM".into(),
            queued: true,
            ..Default::default()
        })
        .unwrap();
        let id = rig.runtime.store.save_landing(second).unwrap();
        rig.runtime
            .advance(rig.runtime.store.get_landing(id).unwrap().unwrap(), 100)
            .await
            .unwrap();
        assert_eq!(
            rig.tracker.move_calls().last().unwrap().state_name,
            "Landing"
        );
        assert_eq!(
            rig.runtime
                .store
                .get_landing(id)
                .unwrap()
                .unwrap()
                .watch_until_s,
            None
        );
        rig.tick(100).await;
        rig.checks
            .live
            .lock()
            .unwrap()
            .push_back(("merge".into(), true));
        rig.runtime
            .advance(rig.runtime.store.get_landing(id).unwrap().unwrap(), 100)
            .await
            .unwrap();
        let second = rig.runtime.store.get_landing(id).unwrap().unwrap();
        assert_eq!(second.phase, LandingPhase::Watching);
        assert_eq!(second.deploy_deadline_s, 130);
        assert_eq!(second.watch_until_s, Some(110));
    }
    #[tokio::test]
    async fn root_commit_without_contract_is_not_a_deletion() {
        let rig = RepoRig::new(None, None);
        let mut row = rig.row();
        row.merge_sha = git(&rig.origin, &["rev-list", "--max-parents=0", "HEAD"]);
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(0).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
    }
    #[tokio::test]
    async fn landing_state_set_moves_ticket_on_merge_unset_leaves_it() {
        for state in ["", "Landing"] {
            let mut rig = RepoRig::new(Some(GOOD), Some(GOOD));
            rig.runtime.config.state = state.into();
            rig.tick(0).await;
            let ticket = rig
                .runtime
                .tracker
                .fetch_issue_by_identifier("APP-1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                ticket.state,
                if state.is_empty() { "In Review" } else { state }
            );
            assert_eq!(rig.row().phase, LandingPhase::Watching);
        }
    }
    struct App {
        address: String,
        state: Arc<Mutex<(String, bool)>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for App {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl App {
        async fn new() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(Mutex::new((
                String::from("0000000000000000000000000000000000000000"),
                true,
            )));
            let values = state.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    let version = String::from_utf8_lossy(&buf[..n]).starts_with("GET /version ");
                    let (sha, healthy) = values.lock().unwrap().clone();
                    let (status, body) = if version {
                        (200, sha)
                    } else {
                        (if healthy { 200 } else { 503 }, "health".into())
                    };
                    let response = format!(
                        "HTTP/1.1 {status} test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Self {
                address,
                state,
                task,
            }
        }
    }
    struct ScriptedGh {
        origin: String,
        merged: Mutex<BTreeMap<i64, String>>,
        calls: Mutex<Vec<i64>>,
        branches: Mutex<BTreeMap<i64, String>>,
        app: Option<Arc<Mutex<(String, bool)>>>,
        ci_green: std::sync::atomic::AtomicBool,
    }
    impl ScriptedGh {
        fn branch(&self, pr: i64) -> String {
            self.branches
                .lock()
                .unwrap()
                .get(&pr)
                .cloned()
                .unwrap_or_else(|| format!("feature-{pr}"))
        }
        fn snapshot(&self, pr: i64) -> PrSnapshot {
            PrSnapshot {
                head_sha: git(&self.origin, &["rev-parse", &self.branch(pr)]),
                status: if self.merged.lock().unwrap().contains_key(&pr) {
                    PrStatus::Merged
                } else {
                    PrStatus::Open
                },
                is_draft: Some(false),
                head_repo: "o/r".into(),
                merge_state: "CLEAN".into(),
                merged_at: None,
            }
        }
        fn merge_branch(&self, pr: i64) {
            git(&self.origin, &["merge", "--squash", &self.branch(pr)]);
            git(
                &self.origin,
                &["commit", "--allow-empty", "-m", &format!("merge {pr}")],
            );
            self.merged
                .lock()
                .unwrap()
                .insert(pr, git(&self.origin, &["rev-parse", "HEAD"]));
            if let Some(app) = &self.app {
                *app.lock().unwrap() = (
                    git(&self.origin, &["rev-parse", "HEAD"]),
                    git(&self.origin, &["show", "HEAD:health"]) == "healthy",
                );
            }
        }
    }
    #[async_trait::async_trait]
    impl PrStateSource for ScriptedGh {
        async fn pr_state(&self, _: &str, _: &str, pr: i64, _: &HeadAllowlist) -> PrStateResult {
            Ok(PrLookup::Found(self.snapshot(pr)))
        }
    }
    #[async_trait::async_trait]
    impl MergeStateSource for ScriptedGh {
        async fn merge_state(&self, _: &str, _: &str, _: i64) -> MergeStateResult {
            Ok("CLEAN".into())
        }
    }
    #[async_trait::async_trait]
    impl PrChecksSource for ScriptedGh {
        async fn pr_checks(&self, _: &str, _: &str, _: i64) -> PrChecksResult {
            Ok(vec![CheckRun {
                name: "test".into(),
                state: if self.ci_green.load(std::sync::atomic::Ordering::Relaxed) {
                    "SUCCESS"
                } else {
                    "FAILURE"
                }
                .into(),
            }])
        }
    }
    #[async_trait::async_trait]
    impl BranchUpdateSource for ScriptedGh {
        async fn allows_branch_update(&self, _: &str, _: &str) -> BranchUpdateResult {
            Ok(false)
        }
    }
    #[async_trait::async_trait]
    impl BranchUpdater for ScriptedGh {
        async fn update_branch(&self, _: &str, _: &str, _: i64) -> MergeResult {
            Err("unexpected branch update".into())
        }
    }
    #[async_trait::async_trait]
    impl MergeSource for ScriptedGh {
        async fn merge_pr(
            &self,
            _: &str,
            _: &str,
            pr: i64,
            method: MergeMethod,
            auto: bool,
            head: Option<&str>,
        ) -> MergeResult {
            assert_eq!(method, MergeMethod::Squash);
            assert!(!auto);
            assert_eq!(head, Some(self.snapshot(pr).head_sha.as_str()));
            self.calls.lock().unwrap().push(pr);
            self.merge_branch(pr);
            Ok("merged".into())
        }
    }
    struct ScriptedChecks {
        repo: RepoChecks,
        gh: Arc<ScriptedGh>,
    }

    #[async_trait::async_trait]
    impl crate::landingrevert::RevertGithub for ScriptedGh {
        async fn original(
            &self,
            _: &LandingRow,
        ) -> Result<(String, String), crate::landingrevert::RevertError> {
            Ok(("break health".into(), "main".into()))
        }
        async fn find(
            &self,
            _: &LandingRow,
            _: &str,
        ) -> Result<Option<i64>, crate::landingrevert::RevertError> {
            Ok(self
                .branches
                .lock()
                .unwrap()
                .contains_key(&99)
                .then_some(99))
        }
        async fn open(
            &self,
            _: &LandingRow,
            branch: &str,
            _: &str,
            title: &str,
            body: &str,
        ) -> Result<i64, crate::landingrevert::RevertError> {
            assert_eq!(title, "revert: break health (landing failed: health)");
            assert!(body.contains("status=503"));
            self.branches.lock().unwrap().insert(99, branch.into());
            Ok(99)
        }
        async fn snapshot(
            &self,
            _: &LandingRow,
            pr: i64,
        ) -> Result<crate::landingrevert::RevertSnapshot, crate::landingrevert::RevertError>
        {
            let snap = self.snapshot(pr);
            Ok(crate::landingrevert::RevertSnapshot {
                head: snap.head_sha,
                opened_at_s: 0,
                status: snap.status,
                labelled: true,
                merge_sha: self
                    .merged
                    .lock()
                    .unwrap()
                    .get(&pr)
                    .cloned()
                    .unwrap_or_default(),
            })
        }
    }

    #[tokio::test]
    async fn file_tracker_http_app_failed_merge_revert_restores_health_and_reopens() {
        use crate::reviewwatch::{ControlWatchSink, ReviewWatchSink};
        let app = App::new().await;
        let contract = format!(
            "version: 1\nlive:\n  http: {{url: '{}/version', status: 200}}\n  every: 1s\nchecks:\n- name: health\n  http: {{url: '{}/healthz', status: 200}}\n  every: 1s\n",
            app.address, app.address
        );
        let mut rig = RepoRig::build(Some(&contract), Some(&contract), false);
        git(&rig.origin, &["reset", "--hard", "HEAD^1"]);
        std::fs::write(Path::new(&rig.origin).join("health"), "healthy\n").unwrap();
        git(&rig.origin, &["add", "-A"]);
        git(&rig.origin, &["commit", "-m", "healthy base"]);
        git(&rig.origin, &["checkout", "-b", "feature-1"]);
        std::fs::write(Path::new(&rig.origin).join("health"), "broken\n").unwrap();
        git(&rig.origin, &["add", "-A"]);
        git(&rig.origin, &["commit", "-m", "break health"]);
        git(&rig.origin, &["checkout", "main"]);
        let gh = Arc::new(ScriptedGh {
            origin: rig.origin.clone(),
            merged: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            branches: Mutex::new(BTreeMap::new()),
            app: Some(app.state.clone()),
            ci_green: std::sync::atomic::AtomicBool::new(true),
        });
        let merges = Arc::new(crate::runautomerge::AutoMergeDeps {
            prs: gh.clone(),
            mergestate: gh.clone(),
            policy: gh.clone(),
            updater: gh.clone(),
            checks: gh.clone(),
            merger: gh.clone(),
            allow: HeadAllowlist::none(),
            approvals: None,
            ledger: Arc::new(crate::runautomerge::AutoMergeLedger::default()),
        });
        let (mut o, _) = orch(true);
        o.teams.as_mut().unwrap().review.auto_merge = true;
        o.human_holds.begin_pass(true);
        o.set_store(rig.runtime.store.clone());
        o.now = Box::new(|| DateTime::from_timestamp(0, 0).unwrap());
        o.store()
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "APP-1".into(),
                issue_id: "ID-1".into(),
                team_id: "TEAM".into(),
                project_slug: "app".into(),
                ..Default::default()
            })
            .unwrap();
        let mut watch = watch_row();
        watch.status = rhapsody_store::REVIEW_STATUS_APPROVED.into();
        watch.last_reviewed_sha = gh.snapshot(1).head_sha;
        o.store().save_review_watch(watch).unwrap();
        let observation = || crate::prstate::PrObservation {
            pr: PrCoord::new("o", "r", 1),
            lookup: PrLookup::Found(gh.snapshot(1)),
            unchanged_from: Vec::new(),
            head_patch_id: String::new(),
        };
        let plan = o.handle_review_sweep(&[observation()]).merge.remove(0);
        let sink = ControlWatchSink::new(o.control()).with_auto_merge(merges.clone());
        let mut events = o.take_events_rx().unwrap();
        sink.merge(plan).await;
        o.drive_event(events.recv().await.unwrap()).await;
        o.handle_review_sweep(&[observation()]);
        rig.id = o.store().load_open_landings().unwrap()[0].id;
        rig.runtime.config.auto_revert = true;
        let repo = RepoChecks {
            manager: crate::testsupport::mk_workspace(&rig._dir.child("workspaces")),
            repo_url: rig.origin.clone(),
            config: rig.runtime.config.clone(),
        };
        rig.runtime.checks = Arc::new(ScriptedChecks {
            repo,
            gh: gh.clone(),
        });
        rig.runtime.reverter = Some(Arc::new(crate::landingrevert::Reverter {
            manager: crate::testsupport::mk_workspace(&rig._dir.child("reverts")),
            repo_url: rig.origin.clone(),
            github: gh.clone(),
            merges,
            auto_merge: true,
            control: None,
            reviewers: Vec::new(),
            holds: o.human_holds.clone(),
        }));
        assert!(
            !app.state.lock().unwrap().1,
            "merging the broken code breaks /healthz"
        );
        rig.tick(0).await;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Reverting);
        gh.ci_green
            .store(false, std::sync::atomic::Ordering::Relaxed);
        rig.tick(2).await;
        assert_eq!(rig.row().revert_pr, Some(99));
        assert_eq!(
            *gh.calls.lock().unwrap(),
            vec![1],
            "a labelled recovery must not merge with red CI"
        );
        assert!(!app.state.lock().unwrap().1);
        gh.ci_green
            .store(true, std::sync::atomic::Ordering::Relaxed);
        rig.tick(3).await;
        assert_eq!(
            *gh.calls.lock().unwrap(),
            vec![1, 99],
            "revert passes the actual green-CI merge path"
        );
        assert!(
            app.state.lock().unwrap().1,
            "the merged git revert restores /healthz"
        );
        rig.tick(4).await;
        let child = o
            .store()
            .load_open_landings()
            .unwrap()
            .into_iter()
            .find(|r| r.revert_of == Some(rig.id))
            .unwrap();
        rig.runtime.advance(child.clone(), 4).await.unwrap();
        rig.runtime
            .advance(o.store().get_landing(child.id).unwrap().unwrap(), 14)
            .await
            .unwrap();
        assert_eq!(
            o.store().get_landing(child.id).unwrap().unwrap().phase,
            LandingPhase::Done
        );
        rig.tick(15).await;
        assert_eq!(rig.row().phase, LandingPhase::Reopened);
        let ticket = rig
            .runtime
            .tracker
            .fetch_issue_by_identifier("APP-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ticket.state, "In Progress");
        assert!(
            ticket
                .description
                .as_deref()
                .unwrap()
                .contains("REVIEW FEEDBACK")
        );
        assert!(
            ticket
                .description
                .as_deref()
                .unwrap()
                .contains("health: status=503")
        );
        let mut linked = ticket;
        linked.linked_pr = true;
        assert!(
            !o.pr_suppressed(&linked),
            "the author is eligible without a summon"
        );
        assert!(o.store().subject_resume_pending("APP-1").unwrap());
        o.eff.as_mut().unwrap().workspace =
            crate::testsupport::mk_workspace(&rig._dir.child("author"));
        o.eff.as_mut().unwrap().tracker = rig.runtime.tracker.clone();
        o.dispatch_issue(linked, None, None, String::new());
        assert!(
            o.running.contains_key("ID-1"),
            "the reopened author is dispatched through the real admission path"
        );
        assert!(
            !o.store().subject_resume_pending("APP-1").unwrap(),
            "one author admission only"
        );
    }
    #[async_trait::async_trait]
    impl LandingChecks for ScriptedChecks {
        async fn merge_sha(&self, row: &LandingRow) -> Result<String, String> {
            self.gh
                .merged
                .lock()
                .unwrap()
                .get(&row.pr)
                .cloned()
                .ok_or("not merged".into())
        }
        async fn prepare(&self, row: &LandingRow) -> Result<Option<LandingContract>, String> {
            self.repo.prepare(row).await
        }
        async fn live(
            &self,
            row: &LandingRow,
            contract: &LandingContract,
            fetch: bool,
        ) -> Result<(String, bool, bool), String> {
            self.repo.live(row, contract, fetch).await
        }
        async fn check(&self, row: &LandingRow, check: &NamedCheck) -> CheckOutcome {
            self.repo.check(row, check).await
        }
    }

    #[tokio::test]
    async fn file_tracker_http_app_merge_watches_to_done_then_failure_reopens() {
        use crate::reviewwatch::{ControlWatchSink, ReviewWatchSink};
        let app = App::new().await;
        let contract = format!(
            "version: 1\nlive:\n  http: {{url: '{}/version', status: 200}}\n  every: 1s\nchecks:\n- name: health\n  http: {{url: '{}/healthz', status: 200}}\n  every: 1s\n",
            app.address, app.address
        );
        let mut rig = RepoRig::build(None, Some(&contract), false);
        // Start with two approved but unmerged branches, rather than inserting landing rows.
        git(&rig.origin, &["reset", "--hard", "HEAD^1"]);
        git(&rig.origin, &["branch", "feature-1", "feature"]);
        git(&rig.origin, &["checkout", "-b", "feature-2", "feature"]);
        git(
            &rig.origin,
            &["commit", "--allow-empty", "-m", "second change"],
        );
        git(&rig.origin, &["checkout", "main"]);
        let gh = Arc::new(ScriptedGh {
            origin: rig.origin.clone(),
            merged: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            branches: Mutex::new(BTreeMap::new()),
            app: None,
            ci_green: std::sync::atomic::AtomicBool::new(true),
        });
        let (mut o, _) = orch(true);
        o.teams.as_mut().unwrap().review.auto_merge = true;
        o.human_holds.begin_pass(true);
        o.set_store(rig.runtime.store.clone());
        o.now = Box::new(|| DateTime::from_timestamp(0, 0).unwrap());
        o.store()
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "APP-1".into(),
                issue_id: "ID-1".into(),
                team_id: "TEAM".into(),
                project_slug: "app".into(),
                ..Default::default()
            })
            .unwrap();
        for pr in [1, 2] {
            let mut row = watch_row();
            row.key.number = pr;
            row.status = rhapsody_store::REVIEW_STATUS_APPROVED.into();
            row.last_reviewed_sha = git(&rig.origin, &["rev-parse", &format!("feature-{pr}")]);
            o.store().save_review_watch(row).unwrap();
        }
        let mut events = o.take_events_rx().unwrap();
        let sink = ControlWatchSink::new(o.control()).with_auto_merge(Arc::new(
            crate::runautomerge::AutoMergeDeps {
                prs: gh.clone(),
                mergestate: gh.clone(),
                policy: gh.clone(),
                updater: gh.clone(),
                checks: gh.clone(),
                merger: gh.clone(),
                allow: HeadAllowlist::none(),
                approvals: None,
                ledger: Arc::new(crate::runautomerge::AutoMergeLedger::default()),
            },
        ));
        let observation = |pr| crate::prstate::PrObservation {
            pr: PrCoord::new("o", "r", pr),
            lookup: crate::ghsummons::PrLookup::Found(gh.snapshot(pr)),
            unchanged_from: Vec::new(),
            head_patch_id: String::new(),
        };
        let report = o.handle_review_sweep(&[observation(1), observation(2)]);
        assert_eq!(
            report.merge.len(),
            1,
            "real merge admission serializes the project"
        );
        assert_eq!(o.store().load_open_landings().unwrap().len(), 1);
        assert!(
            serde_json::from_str::<Progress>(
                &o.store().load_open_landings().unwrap()[0].results_json
            )
            .unwrap()
            .merge_pending
        );
        sink.merge(report.merge[0].clone()).await;
        o.drive_event(events.recv().await.unwrap()).await;
        assert_eq!(*gh.calls.lock().unwrap(), vec![1]);
        assert!(o.handle_review_sweep(&[observation(2)]).merge.is_empty());
        // GitHub's merged observation creates the durable lock, before any ticket move.
        o.handle_review_sweep(&[observation(1)]);
        rig.id = o.store().load_open_landings().unwrap()[0].id;
        rig.runtime.checks = Arc::new(ScriptedChecks {
            repo: RepoChecks {
                manager: crate::testsupport::mk_workspace(&rig._dir.child("workspaces")),
                repo_url: rig.origin.clone(),
                config: config(),
            },
            gh: gh.clone(),
        });
        rig.tick(0).await;
        assert_eq!(
            rig.row().phase,
            LandingPhase::WaitingForDeploy,
            "{:?}",
            rig.row()
        );
        app.state.lock().unwrap().0 = rig.row().merge_sha;
        rig.tick(1).await;
        assert_eq!(rig.row().phase, LandingPhase::Watching);
        assert!(o.handle_review_sweep(&[observation(2)]).merge.is_empty());

        // Human merges bypass auto-merge and queue behind the live landing. They consume no
        // deploy/watch window while queued, even when a tick arrives after their original deadline.
        git(&rig.origin, &["checkout", "-b", "feature-3"]);
        git(
            &rig.origin,
            &["commit", "--allow-empty", "-m", "human change"],
        );
        git(&rig.origin, &["checkout", "main"]);
        gh.merge_branch(3);
        let mut human = watch_row();
        human.key.number = 3;
        o.store().save_review_watch(human).unwrap();
        o.handle_review_sweep(&[observation(3)]);
        let queued = o.store().load_open_landings().unwrap()[1].clone();
        rig.runtime.advance(queued.clone(), 100).await.unwrap();
        assert_eq!(
            o.store()
                .get_landing(queued.id)
                .unwrap()
                .unwrap()
                .watch_until_s,
            None
        );
        rig.tick(100).await;
        assert_eq!(rig.row().phase, LandingPhase::Done);
        assert_eq!(
            rig.runtime
                .tracker
                .fetch_issue_by_identifier("APP-1")
                .await
                .unwrap()
                .unwrap()
                .state,
            "Done"
        );
        let comments = rig.runtime.tracker.list_comments("ID-1").await.unwrap();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].body.contains("status=200"));

        assert!(o.handle_review_sweep(&[observation(2)]).merge.is_empty());
        app.state.lock().unwrap().0 = gh.merged.lock().unwrap()[&3].clone();
        rig.runtime
            .advance(o.store().get_landing(queued.id).unwrap().unwrap(), 100)
            .await
            .unwrap();
        assert_eq!(
            o.store()
                .get_landing(queued.id)
                .unwrap()
                .unwrap()
                .watch_until_s,
            Some(110)
        );
        rig.runtime
            .advance(o.store().get_landing(queued.id).unwrap().unwrap(), 110)
            .await
            .unwrap();
        assert_eq!(
            o.store().get_landing(queued.id).unwrap().unwrap().phase,
            LandingPhase::Done
        );
        o.now = Box::new(|| DateTime::from_timestamp(111, 0).unwrap());
        let report = o.handle_review_sweep(&[observation(2)]);
        assert_eq!(
            report.merge.len(),
            1,
            "terminal landings release auto-merge admission"
        );
        sink.merge(report.merge[0].clone()).await;
        o.drive_event(events.recv().await.unwrap()).await;
        assert_eq!(*gh.calls.lock().unwrap(), vec![1, 2]);
        o.handle_review_sweep(&[observation(2)]);
        let id = o.store().load_open_landings().unwrap()[0].id;
        *app.state.lock().unwrap() = (gh.merged.lock().unwrap()[&2].clone(), false);
        rig.runtime
            .advance(rig.runtime.store.get_landing(id).unwrap().unwrap(), 111)
            .await
            .unwrap();
        rig.runtime
            .advance(rig.runtime.store.get_landing(id).unwrap().unwrap(), 112)
            .await
            .unwrap();
        assert_eq!(
            rig.runtime.store.get_landing(id).unwrap().unwrap().phase,
            LandingPhase::Reopened
        );
        let ticket = rig
            .runtime
            .tracker
            .fetch_issue_by_identifier("APP-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ticket.state, "In Progress");
        assert!(ticket.description.unwrap().contains("health: status=503"));
    }

    #[tokio::test]
    async fn production_landing_tick_runs_off_loop_and_continues_during_drain() {
        let rig = RepoRig::new(None, Some(GOOD));
        let (mut o, tracker) = orch(true);
        let mut project = crate::testsupport::empty_resolved_project("app", tracker);
        project.tracker = rig.runtime.tracker.clone();
        project.repo = rig.origin.clone();
        project.workspace = crate::testsupport::mk_workspace(&rig._dir.child("workspaces"));
        o.eff.as_mut().unwrap().projects = vec![project];
        o.set_store(rig.runtime.store.clone());
        o.drain.arm(Utc::now(), crate::drain::DrainReason::Operator);
        for now in [0, 10] {
            o.landing_tick(DateTime::from_timestamp(now, 0).unwrap())
                .await;
            // Only owned dependency snapshots were taken by landing_tick. The task, not control,
            // does the real git/check/tracker writes; join it here to make the assertion deterministic.
            (&mut o.landing_task.as_mut().unwrap().0).await.unwrap();
        }
        assert_eq!(rig.row().phase, LandingPhase::Done);
        assert_eq!(
            rig.runtime
                .tracker
                .list_comments("ID-1")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
