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
    project_group: String,
    resume_pending: bool,
    issue_id: String,
    team_id: String,
    started: bool,
    queued: bool,
    state_moved: bool,
    fetched: bool,
    next_live_s: i64,
    samples: Vec<Sample>,
    checks: BTreeMap<String, CheckState>,
    no_contract: bool,
    pending: Option<String>,
    evidence_sent: bool,
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
impl Drop for LandingTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Orchestrator {
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
        let queued = self
            .store()
            .load_open_landings()
            .map_err(|e| e.to_string())?
            .iter()
            .any(|r| {
                serde_json::from_str::<Progress>(&r.results_json)
                    .map(|p| landing_group(r, &p) == project_group)
                    .unwrap_or(true)
            });
        let progress = Progress {
            project_group,
            issue_id: run.issue_id,
            team_id: run.team_id,
            queued,
            ..Default::default()
        };
        let id = self
            .store()
            .save_landing(LandingRow {
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
        if !self.eff.as_ref().is_some_and(|e| {
            e.cfg
                .projects
                .iter()
                .any(|p| p.landing.as_ref().is_some_and(|c| c.enabled))
        }) {
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
        let rows = match self.store().load_open_landings() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "landing rows could not be read");
                return;
            }
        };
        let mut work = Vec::new();
        for row in rows {
            let Some(config) = self.landing_config(&row.project_slug) else {
                continue;
            };
            let Some(eff) = &self.eff else {
                continue;
            };
            let Some(project) = eff.project_by_slug(&row.project_slug) else {
                continue;
            };
            let runtime = LandingRuntime {
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
            };
            work.push((runtime, row));
        }
        if work.is_empty() {
            return;
        }
        let mut ctx = self.ctx.clone().unwrap_or_default();
        let guard = self.wg.add();
        self.landing_task = Some(LandingTask(tokio::spawn(async move {
            let _guard = guard;
            tokio::select! {
                _ = ctx.cancelled() => {},
                _ = async {
                    for (runtime, row) in work {
                        let id = row.id;
                        if let Err(err) = runtime.advance(row, now.timestamp()).await {
                            tracing::warn!(landing_id = id, %err, "landing tick failed; durable work retained for retry");
                        }
                    }
                } => {}
            }
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
            .map_err(|e| e.to_string())
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

struct LandingRuntime {
    store: Arc<dyn Store + Send + Sync>,
    tracker: Arc<dyn Tracker>,
    checks: Arc<dyn LandingChecks>,
    config: LandingConfig,
    done_state: String,
    changes_state: String,
    notifications: Option<Arc<crate::breaker::NotificationsState>>,
}
impl LandingRuntime {
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
            LandingPhase::Done
                | LandingPhase::Held
                | LandingPhase::Reopened
                | LandingPhase::AwaitingAcceptance
                | LandingPhase::Reverting
        ) {
            return Ok(());
        }
        let mut progress: Progress = serde_json::from_str(&row.results_json)
            .map_err(|e| format!("invalid landing progress: {e}"))?;
        if progress.pending.is_some() {
            return self.finish(&mut row, &mut progress, now).await;
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
        if !progress.started {
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
            Err(reason) => return self.hold(&mut row, &mut progress, &reason, now).await,
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
        let watch_ended = row.watch_until_s.is_some_and(|end| now >= end);
        // The window is absolute: a tolerated final flap stays in evidence but cannot schedule
        // another sample beyond it. Only reaching the persisted threshold fails the landing.
        for check in &contract.checks {
            let state = progress.checks.entry(check.name.clone()).or_default();
            if state.failures >= self.config.failure_threshold {
                continue;
            }
            if state.ran
                && (watch_ended || check.every.is_none() && state.pass || now < state.next_s)
            {
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
            progress.pending = Some("Reopened".into());
        } else if watch_ended
            && contract
                .checks
                .iter()
                .all(|c| progress.checks.get(&c.name).is_some_and(|s| s.ran))
        {
            progress.pending = Some("Done".into());
        }
        self.save(&mut row, &progress, now)?;
        if progress.pending.is_some() {
            self.finish(&mut row, &mut progress, now).await?;
        }
        Ok(())
    }
    async fn hold(
        &self,
        row: &mut LandingRow,
        progress: &mut Progress,
        reason: &str,
        now: i64,
    ) -> Result<(), String> {
        row.hold_reason = Some(reason.into());
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
        if target == "Done" {
            let description = self
                .tracker
                .fetch_issue_description_by_identifier(&row.ticket)
                .await
                .map_err(|e| e.to_string())?;
            if description.as_deref().is_some_and(has_acceptance) {
                target = "AwaitingAcceptance".into();
                progress.pending = Some(target.clone());
                self.save(row, progress, now)?;
            }
        }
        let marker = format!("<!-- rhapsody:landing:{} -->", row.id);
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
                    let failed = progress
                        .checks
                        .iter()
                        .filter(|(_, s)| s.failures >= self.config.failure_threshold)
                        .filter_map(|(name, _)| {
                            progress.samples.iter().rev().find(|s| &s.name == name)
                        })
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
                if !self.done_state.is_empty() {
                    self.tracker
                        .move_issue_state(&progress.issue_id, &progress.team_id, &self.done_state)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                LandingPhase::Done
            }
            "AwaitingAcceptance" => LandingPhase::AwaitingAcceptance,
            _ => return Err("unknown landing outcome".into()),
        };
        progress.pending = None;
        self.save(row, progress, now)
    }
}
fn cell(text: &str) -> String {
    crate::managerapply::strip_summon_tokens(text)
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}
fn has_acceptance(description: &str) -> bool {
    description
        .split("## Post-deploy acceptance")
        .nth(1)
        .is_some_and(|s| {
            s.split("\n## ")
                .next()
                .unwrap_or("")
                .lines()
                .any(|l| l.trim_start().starts_with("- ["))
        })
}

fn landing_group<'a>(row: &'a LandingRow, progress: &'a Progress) -> &'a str {
    if progress.project_group.is_empty() {
        &row.project_slug
    } else {
        &progress.project_group
    }
}

#[cfg(test)]
mod tests {
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
    fn config() -> LandingConfig {
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
                    store,
                    tracker: tracker.clone(),
                    checks: checks.clone(),
                    config: config(),
                    done_state: done.into(),
                    changes_state: "In Progress".into(),
                    notifications: None,
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
    async fn under_threshold_failure_at_deadline_does_not_extend_watch_window() {
        for final_tick in [10, 30] {
            let mut rig = Rig::new("Done");
            let mut contract = rig.checks.contract.clone().unwrap().unwrap();
            contract.checks[0].every = Some(Duration::from_secs(30));
            let checks = Arc::new(Checks {
                contract: Ok(Some(contract)),
                live: Mutex::new(VecDeque::from([("merge".into(), true)])),
                checks: Mutex::new(VecDeque::from([false, false])),
                calls: Mutex::new(Vec::new()),
            });
            rig.checks = checks.clone();
            rig.runtime.checks = checks;
            rig.tick(0).await;
            assert_eq!(rig.row().phase, LandingPhase::Watching);
            rig.tick(final_tick).await;
            assert_eq!(rig.row().phase, LandingPhase::Done);
            assert_eq!(rig.row().watch_until_s, Some(10));
            let samples = rig.progress().samples;
            assert_eq!(samples.iter().filter(|s| s.name == "health").count(), 1);
            assert!(
                !samples.last().unwrap().pass,
                "tolerated failure stays in evidence"
            );
        }
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

    struct RepoRig {
        _dir: TempDir,
        origin: String,
        runtime: LandingRuntime,
        id: i64,
    }
    fn git(path: &str, args: &[&str]) -> String {
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
            };
        }
        fn new(before: Option<&str>, after: Option<&str>) -> Self {
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
                store,
                tracker,
                checks: Arc::new(RepoChecks {
                    manager: crate::testsupport::mk_workspace(&dir.child("workspaces")),
                    repo_url: origin.clone(),
                    config: config(),
                }),
                config: config(),
                done_state: "Done".into(),
                changes_state: "In Progress".into(),
                notifications: None,
            };
            Self {
                _dir: dir,
                origin,
                runtime,
                id,
            }
        }
        fn row(&self) -> LandingRow {
            self.runtime.store.get_landing(self.id).unwrap().unwrap()
        }
        async fn tick(&self, now: i64) {
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
    }
    impl ScriptedGh {
        fn snapshot(&self, pr: i64) -> PrSnapshot {
            PrSnapshot {
                head_sha: git(&self.origin, &["rev-parse", &format!("feature-{pr}")]),
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
            git(
                &self.origin,
                &["merge", "--squash", &format!("feature-{pr}")],
            );
            git(
                &self.origin,
                &["commit", "--allow-empty", "-m", &format!("merge {pr}")],
            );
            self.merged
                .lock()
                .unwrap()
                .insert(pr, git(&self.origin, &["rev-parse", "HEAD"]));
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
                state: "SUCCESS".into(),
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
        assert!(o.store().load_open_landings().unwrap().is_empty());
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
