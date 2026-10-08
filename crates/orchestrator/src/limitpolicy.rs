//! Account limit decisions (STUDIO-1126). No Go counterpart; scheduling is control-task-owned.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use rhapsody_config::{Limits, profiles::EngineSpec};
use serde::Serialize;

use crate::accounts::{AccountLedger, WindowView};
use crate::{Orchestrator, RunningEntry};

pub const HANDOFF_LIMIT_MARKER: &str = "HANDOFF: limit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warn,
    StopNew,
    Handoff,
    Wall,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct LimitTicket {
    pub ticket: String,
    pub identity: String,
    pub fallback: Vec<EngineSpec>,
    pub healthy: Vec<bool>,
    pub mid_review: bool,
    #[serde(default)]
    pub engine_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_note: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct LimitItem {
    pub account: String,
    pub windows: Vec<WindowView>,
    pub tickets: Vec<LimitTicket>,
    pub credits_policy: String,
    #[serde(default)]
    pub resets_at_s: i64,
    #[serde(default)]
    pub budgets: BTreeMap<String, rhapsody_config::ProviderBudget>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manager_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Box<crate::managerlimits::LimitDecision>>,
    #[serde(default)]
    pub manager_runs: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub enum HandoffOutcome {
    Park { resume_at_s: i64 },
    Switch { engine: usize },
    ManagerItem(LimitItem),
}

pub fn level(ledger: &AccountLedger, account: &str, cfg: &Limits, now_s: i64) -> Level {
    let Some(view) = ledger
        .snapshot(now_s)
        .into_iter()
        .find(|a| a.account == account)
    else {
        return Level::Ok;
    };
    let t = cfg.for_account(account);
    if account == "openai" && view.source == "budget" {
        return if view.status == "rejected" {
            Level::Wall
        } else {
            Level::Ok
        };
    }
    if cfg.credits == "always" && t.warn == 100.0 && t.stop_new == 100.0 && t.handoff == 100.0 {
        return Level::Ok;
    }
    if view.status == "rejected" {
        return Level::Wall;
    }
    if view.using_credits {
        return match cfg.credits.as_str() {
            "always" | "daily_cap" => Level::Ok,
            _ => Level::Wall,
        };
    }
    let percent = ledger
        .tightest(account, now_s)
        .map_or(0.0, |w| w.utilization * 100.0);
    if percent >= 100.0 {
        Level::Wall
    } else if percent >= t.handoff {
        Level::Handoff
    } else if percent >= t.stop_new {
        Level::StopNew
    } else if percent >= t.warn || view.status == "warning" {
        Level::Warn
    } else {
        Level::Ok
    }
}

pub fn decide(
    ledger: &AccountLedger,
    item: LimitItem,
    cfg: &Limits,
    now_s: i64,
    fallback: Option<usize>,
) -> HandoffOutcome {
    if let Some(window) = ledger.tightest(&item.account, now_s)
        && window.window != "seven_day"
        && window.resets_at_s > now_s
        && window.resets_at_s.saturating_sub(now_s) <= cfg.wait_max_minutes.saturating_mul(60)
    {
        return HandoffOutcome::Park {
            resume_at_s: window.resets_at_s.saturating_add(120),
        };
    }
    fallback.map_or(HandoffOutcome::ManagerItem(item), |engine| {
        HandoffOutcome::Switch { engine }
    })
}

#[derive(Default)]
pub(crate) struct LimitPolicy {
    pub report_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::limitreport::Report>>,
    pub reported_states: BTreeMap<String, (Level, bool)>,
    pub deadlines: HashMap<String, i64>,
    pub suspended: HashMap<String, Suspended>,
    pub items: Vec<LimitItem>,
    pub docs_dir: Option<PathBuf>,
    pub credit_notified: HashMap<String, i64>,
    pub holds: BTreeMap<String, String>,
    pub credit_spent: HashMap<(String, i64), f64>,
    pub cost_baselines: HashMap<(i64, i64), f64>,
    pub resume_sessions: HashMap<String, String>,
    pub pinned_engines: HashMap<String, crate::dispatch::DispatchEngine>,
    pub fed_budgets: BTreeMap<String, rhapsody_config::ProviderBudget>,
    pub manager_cases: HashMap<String, LimitItem>,
    pub limited_managers: HashMap<String, chrono::DateTime<chrono::Utc>>,
    pub pending_reassignments: HashMap<String, chrono::DateTime<chrono::Utc>>,
    pub decisions: Vec<crate::managerlimits::LimitDecision>,
    pub credit_approvals: HashMap<String, (String, i64)>,
}

pub(crate) struct Suspended {
    pub run: RunningEntry,
    pub outcome: HandoffOutcome,
    pub note: Option<PathBuf>,
    pub worker_finished: bool,
    pub account: Option<crate::accounts::AccountView>,
}

/// Stored on the existing retry queue only for limit stops. Ordinary retry rows stay unchanged.
#[derive(Serialize, serde::Deserialize)]
struct ResumeRecord {
    issue: rhapsody_core::Issue,
    identity: String,
    harness: String,
    model: String,
    effort: String,
    engine_index: usize,
    session: String,
    attempt: i64,
    project: String,
    repo: String,
    group: String,
    brokered: bool,
    stack_context: String,
    review: Option<crate::review::ReviewRun>,
    outcome: HandoffOutcome,
    note: Option<PathBuf>,
    account: Option<crate::accounts::AccountView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credit_approval: Option<(String, i64)>,
    #[serde(default)]
    reassignment_pending: bool,
}

pub(crate) const RETRY_LIMIT_PREFIX: &str = "limit continuation: ";

impl Orchestrator {
    fn account_level(&self, account: &str, cfg: &Limits, now: i64) -> Level {
        let plan = level(&self.accounts, account, cfg, now);
        // Keep reporting the real plan independently; its workers still obey the operator's
        // runaway budget wall, even when credit spending or plan thresholds are disabled.
        if account == "chatgpt-subscription" && self.openai_budget_rejected(now) {
            Level::Wall
        } else {
            plan
        }
    }

    pub(crate) fn openai_budget_rejected(&self, now: i64) -> bool {
        self.accounts
            .snapshot(now)
            .iter()
            .any(|a| a.account == "openai" && a.source == "budget" && a.status == "rejected")
    }
    pub(crate) fn limits_config(&self) -> Limits {
        self.eff
            .as_ref()
            .map(|e| e.cfg.limits.clone())
            .unwrap_or_default()
    }

    pub(crate) fn account_usable(&self, account: &str) -> bool {
        let cfg = self.limits_config();
        let now = (self.now)().timestamp();
        if cfg.credits == "daily_cap"
            && self
                .accounts
                .snapshot(now)
                .iter()
                .any(|a| a.account == account && a.using_credits)
        {
            let spent = self.credit_spent(account, now);
            if spent >= cfg.credits_daily_usd {
                return false;
            }
        }
        self.account_level(account, &cfg, now) < Level::StopNew
    }

    pub(crate) fn engine_list(&self, identity: &str, primary: EngineSpec) -> Vec<EngineSpec> {
        let mut list = vec![primary];
        if let Some(teams) = &self.teams
            && let Some(ident) = teams.roster.iter().find(|i| i.name == identity)
            && let Some(dir) = &self.teams_profiles_dir
        {
            match rhapsody_config::profiles::resolve(dir, &ident.profile) {
                Ok(profile) => list.extend(profile.fallback),
                Err(error) => {
                    tracing::warn!(identity, %error, "limit: fallback profile could not be read")
                }
            }
        }
        list
    }

    pub(crate) fn engine_usable(&self, spec: &EngineSpec, project: &str) -> bool {
        if !crate::effective::harness_is_implemented(&spec.harness) {
            return false;
        }
        let pricing = self.run_pricing_for(
            &spec.harness,
            &rhapsody_agent::ModelOverride {
                model: spec.model.clone(),
                ..Default::default()
            },
            project,
        );
        !self.credential_probe_held(&spec.harness, project)
            && self.account_usable(&pricing.account)
            && self.usd_budget_hold(&pricing).is_none()
    }

    /// Resolve the engine without composing the persona or advancing the room cursor. A refused
    /// selection must not consume a teammate's unread messages or the queue's last slot.
    pub(crate) fn limit_projection(
        &self,
        issue: &rhapsody_core::Issue,
        project: &str,
    ) -> RunningEntry {
        let mut run = RunningEntry::empty(issue.clone());
        run.project_slug = project.into();
        run.identity = self
            .planned_identity(issue, &self.teammate_load())
            .unwrap_or_default();
        if let Some(profile) = self
            .teams
            .as_ref()
            .and_then(|t| t.roster.iter().find(|i| i.name == run.identity))
            .and_then(|i| {
                self.teams_profiles_dir
                    .as_ref()
                    .and_then(|dir| rhapsody_config::profiles::resolve(dir, &i.profile).ok())
            })
        {
            run.harness = profile.harness;
            run.pricing.account = profile.provider;
            run.model_override = rhapsody_agent::ModelOverride {
                identity: run.identity.clone(),
                model: profile.model,
                effort: profile.effort,
            };
        }
        if self.config_defines_any_provider() {
            if let Some(eff) = &self.eff {
                let cfg = eff.project_by_slug(project).map_or(&eff.cfg, |p| &p.mcfg);
                if run.pricing.account.is_empty() {
                    run.pricing.account = cfg.agent.provider.clone();
                }
            }
            if let Ok(labels) = rhapsody_config::routing::parse_ticket_selection(
                issue.labels.as_deref().unwrap_or(&[]),
            ) {
                if let Some(harness) = labels.harness {
                    run.harness = harness;
                }
                if let Some(model) = labels.model {
                    run.model_override.model = model;
                }
                if let Some(provider) = labels.provider {
                    run.pricing.account = provider;
                }
            }
        } else {
            run.pricing.account.clear();
        }
        run.review = self.pending_review.get(&issue.id).cloned();
        if run.review.is_some()
            && let Some(teams) = &self.teams
        {
            let harness = self.effective_harness(&run.harness);
            if let rhapsody_config::teams::ReviewModelChoice::Use(model) =
                teams.review_model_for(&harness, &self.configured_backend())
            {
                run.model_override.model = model.into();
            }
        }
        run
    }

    pub(crate) fn limit_dispatch_ready(&self, issue: &rhapsody_core::Issue, project: &str) -> bool {
        let now = (self.now)().timestamp();
        if !self.has_credential_holds()
            && !self.accounts.snapshot(now).iter().any(|a| {
                level(&self.accounts, &a.account, &self.limits_config(), now) >= Level::StopNew
            })
        {
            return true;
        }
        let run = self.limit_projection(issue, project);
        let pricing = self.run_pricing_for(&run.harness, &run.model_override, project);
        let account = if run.pricing.account.is_empty() {
            &pricing.account
        } else {
            &run.pricing.account
        };
        if !self.credential_probe_held(&run.harness, project) && self.account_usable(account) {
            return true;
        }
        let (harness, model) =
            self.resolved_harness_model(&run.harness, &run.model_override, project);
        if self
            .engine_list(
                &run.identity,
                EngineSpec {
                    harness,
                    model,
                    effort: run.model_override.effort,
                },
            )
            .iter()
            .skip(1)
            .any(|e| self.engine_usable(e, project))
        {
            return true;
        }
        let reset = self
            .accounts
            .tightest(account, now)
            .map_or(0, |w| w.resets_at_s);
        self.note_usd_budget_hold(crate::budget::BudgetHeld {
            subject: issue.identifier.clone(),
            title: issue.title.clone(),
            project: project.into(),
            provider: account.clone(),
            reason: self
                .credential_probe_reason(&run.harness, project)
                .map_or_else(
                    || format!("waiting: {account} limit, resets {reset}"),
                    |reason| format!("waiting: {account} credential — {reason}"),
                ),
            ..Default::default()
        });
        false
    }

    pub(crate) fn reviewer_limit_account(
        &self,
        teams: &rhapsody_config::teams::Teams,
        identity: &str,
    ) -> String {
        let profile = teams
            .roster
            .iter()
            .find(|i| i.name == identity)
            .and_then(|i| {
                self.teams_profiles_dir
                    .as_ref()
                    .and_then(|dir| rhapsody_config::profiles::resolve(dir, &i.profile).ok())
            });
        let harness = self.effective_harness(profile.as_ref().map_or("", |p| p.harness.as_str()));
        // Explicit providers are stable account ids, even when the model names another provider.
        // The preparation path selects profile > global; without a registry it stays native.
        if self.config_defines_any_provider() {
            let provider = profile.as_ref().map_or("", |p| p.provider.as_str());
            if !provider.is_empty() {
                return provider.into();
            }
            if let Some(eff) = &self.eff
                && !eff.cfg.agent.provider.is_empty()
            {
                return eff.cfg.agent.provider.clone();
            }
        }
        let mut model = rhapsody_agent::ModelOverride {
            model: profile.map(|p| p.model).unwrap_or_default(),
            ..Default::default()
        };
        if let rhapsody_config::teams::ReviewModelChoice::Use(value) =
            teams.review_model_for(&harness, &self.configured_backend())
        {
            model.model = value.into();
        }
        self.run_pricing_for(&harness, &model, "").account
    }

    /// The last dispatch gate, before claim/start/spawn. Review callers check before watch writes.
    pub(crate) fn gate_limit_dispatch(
        &mut self,
        re: &mut RunningEntry,
        supplied: &mut Option<crate::dispatch::DispatchEngine>,
        account: Option<&str>,
    ) -> bool {
        let pricing = self.run_pricing_for(&re.harness, &re.model_override, &re.project_slug);
        let account = account.unwrap_or(&pricing.account);
        if let Some(mut held) = self.usd_budget_hold(&pricing)
            && (held.reason.contains("cannot be enforced") || held.reason.starts_with("no price"))
        {
            held.subject = re.issue.identifier.clone();
            held.title = re.issue.title.clone();
            held.project = re.project_slug.clone();
            self.note_usd_budget_hold(held);
            return false;
        }
        if self.run_credential_probe_reason(re).is_none()
            && (self.account_usable(account) || self.credit_approved(&re.issue.id, account))
        {
            self.limit_policy.holds.remove(&re.issue.identifier);
            if self
                .budget_ledger
                .get(&re.issue.identifier, self.budget_hold_ttl())
                .is_some_and(|h| {
                    h.reason.starts_with("waiting:")
                        && (h.reason.contains(" limit") || h.reason.contains(" credential"))
                })
            {
                self.release_budget_hold(&re.issue.identifier);
            }
            return true;
        }
        if re.review.is_none()
            && !crate::managerrun::is_manager_key(&re.issue.id)
            && supplied.is_none()
        {
            let (harness, model) =
                self.resolved_harness_model(&re.harness, &re.model_override, &re.project_slug);
            let list = self.engine_list(
                &re.identity,
                EngineSpec {
                    harness,
                    model,
                    effort: re.model_override.effort.clone(),
                },
            );
            if let Some(index) = list
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(i, e)| self.engine_usable(e, &re.project_slug).then_some(i))
            {
                let engine = crate::dispatch::DispatchEngine {
                    index,
                    spec: list[index].clone(),
                    handoff_note: None,
                };
                re.engine_index = index;
                re.harness = engine.spec.harness.clone();
                re.model_override.model = engine.spec.model.clone();
                re.model_override.effort = engine.spec.effort.clone();
                re.model = engine.spec.model.clone();
                re.engine = Some(engine.clone());
                re.brokered = false; // dropping the primary prepared spec revokes its custody
                *supplied = Some(engine);
                self.limit_policy.holds.remove(&re.issue.identifier);
                self.release_budget_hold(&re.issue.identifier);
                return true;
            }
        }
        let reset = self
            .accounts
            .tightest(account, (self.now)().timestamp())
            .map_or(0, |w| w.resets_at_s);
        let reason = self.run_credential_probe_reason(re).map_or_else(
            || format!("waiting: {account} limit, resets {reset}"),
            |reason| format!("waiting: {account} credential — {reason}"),
        );
        if self
            .limit_policy
            .holds
            .insert(re.issue.identifier.clone(), reason.clone())
            .as_ref()
            != Some(&reason)
        {
            tracing::warn!(ticket = %re.issue.identifier, %reason, "limit: holding dispatch");
        }
        false
    }

    pub(crate) fn enforce_limits(&mut self) {
        self.feed_budget_windows();
        let now = (self.now)().timestamp();
        let cfg = self.limits_config();
        let views = self.accounts.snapshot(now);
        self.limit_policy
            .credit_spent
            .retain(|(_, date), _| *date == day(now));
        if cfg.credits == "daily_cap" {
            for view in views.iter().filter(|v| v.using_credits) {
                let key = (view.account.clone(), day(now));
                if !self.limit_policy.credit_spent.contains_key(&key) {
                    let spent = self.credit_spent(&view.account, now);
                    self.limit_policy.credit_spent.insert(key, spent);
                }
            }
        }
        let mut stops = Vec::new();
        let mut wakes = Vec::new();
        for (id, re) in &self.running {
            let account = self
                .accounts
                .account_for_run(&crate::accounts::run_key(id, re.started_at))
                .unwrap_or_else(|| re.pricing.account.clone());
            let using = views
                .iter()
                .any(|v| v.account == account && v.using_credits);
            let spent = if using && cfg.credits == "daily_cap" {
                self.credit_spent(&account, now)
            } else {
                0.0
            };
            let credit_wall = using
                && ((cfg.credits == "never"
                    || (cfg.credits == "manager_urgent" && !self.credit_approved(id, &account)))
                    || (cfg.credits == "daily_cap" && spent >= cfg.credits_daily_usd));
            let lvl = self.account_level(&account, &cfg, now);
            let approved = self.credit_approved(id, &account);
            let provider_rejected = views
                .iter()
                .any(|v| v.account == account && v.status == "rejected")
                || (account == "chatgpt-subscription" && self.openai_budget_rejected(now));
            if credit_wall || (lvl == Level::Wall && (!approved || provider_rejected)) {
                if credit_wall && cfg.credits != "manager_urgent" {
                    self.accounts.reject_until_reset(&account, now);
                }
                stops.push((id.clone(), account));
                continue;
            }
            if lvl >= Level::Handoff && !approved {
                if let Some(deadline) = self.limit_policy.deadlines.get(id) {
                    if now >= *deadline {
                        stops.push((id.clone(), account));
                    }
                } else {
                    let window = self.accounts.tightest(&account, now);
                    let pct = window.as_ref().map_or(100.0, |w| w.utilization * 100.0);
                    let reset = window.map_or(0, |w| w.resets_at_s);
                    let time = (reset > 0)
                        .then(|| chrono::DateTime::from_timestamp(reset, 0))
                        .flatten()
                        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
                        .unwrap_or_else(|| "unknown".into());
                    let message = format!(
                        "Your account `{account}` is at `{pct:.0}`% (resets `{time}`). Finish the step you're on, push your work in progress, then update your progress file (`~/.rhapsody/docs/{}-progress.md`) with a handoff note: what's done, what's next, open questions, anything half-finished. End with `HANDOFF: limit`.",
                        re.issue.identifier
                    );
                    // Even a full mailbox gets a deadline: an undeliverable warning cannot spend forever.
                    let admitted = if re.run_id == 0 {
                        self.deliver_to_mailbox(re, &message).1
                    } else {
                        let result = self.send_run_message(re.run_id, &message);
                        !result.full && !result.not_running
                    };
                    if !admitted {
                        tracing::warn!(
                            run_id = re.run_id,
                            "limit: handoff message could not be admitted"
                        );
                    }
                    self.limit_policy.deadlines.insert(
                        id.clone(),
                        now.saturating_add(cfg.handoff_grace_minutes.saturating_mul(60)),
                    );
                    wakes.push((id.clone(), cfg.handoff_grace_minutes.saturating_mul(60_000)));
                }
            }
        }
        // One spend event per ACCOUNT per day, including a default-never stop.
        for view in views.iter().filter(|v| v.using_credits) {
            if self.limit_policy.credit_notified.get(&view.account) == Some(&day(now)) {
                continue;
            }
            match self
                .store
                .account_credit_notified(&view.account, &day_start(now))
            {
                Ok(true) => {
                    self.limit_policy
                        .credit_notified
                        .insert(view.account.clone(), day(now));
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "limit: could not read credit notification history")
                }
            }
            self.limit_policy
                .credit_notified
                .insert(view.account.clone(), day(now));
            self.send_limit_report(crate::limitreport::Report::Push {
                account: view.account.clone(),
                title: format!("{} credits in use", view.account),
                body: format!(
                    "{} is spending credits today (policy: {}).",
                    view.account, cfg.credits
                ),
            });
            if let Some(re) = self
                .running
                .values_mut()
                .find(|r| r.pricing.account == view.account)
            {
                re.event_seq += 1;
                let row = rhapsody_store::EventRow {
                    seq: re.event_seq,
                    at: crate::persist::rfc3339((self.now)()),
                    kind: "limit.credit_spend".into(),
                    tool: String::new(),
                    text: serde_json::json!({"account":view.account, "policy":cfg.credits})
                        .to_string(),
                };
                if let Err(error) = self.store.append_events(re.run_id, &[row]) {
                    tracing::warn!(%error, "limit: credit event could not be recorded");
                }
            }
        }
        for (id, delay) in wakes {
            self.arm_retry_timer(&format!("limit:{id}"), delay);
        }
        self.report_account_transitions();
        for (id, account) in stops {
            if let Some(re) = self.running.get(&id)
                && !re.cancel.is_armed()
            {
                tracing::warn!(
                    run_id = re.run_id,
                    "limit: refusing to report an unarmed run stopped"
                );
                continue;
            }
            if let Some(re) = self.terminate(&id) {
                self.finish_limit_stop(re, &account, self.spawn.is_some());
            }
        }
        // Off-loop consumers of reviewer selection read the same live exclusions as the quorum.
        if let Some(teams) = &self.teams {
            let exclusions = self.reviewer_exclusions(teams);
            self.reads
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reviewer_exclusions = exclusions;
        }
    }

    pub(crate) fn finish_limit_stop(
        &mut self,
        re: RunningEntry,
        account: &str,
        worker_finished: bool,
    ) {
        let now = (self.now)().timestamp();
        let cfg = self.limits_config();
        self.limit_policy.deadlines.remove(&re.issue.id);
        self.limit_policy
            .cost_baselines
            .retain(|(run, _), _| *run != re.run_id);
        self.persist_end_run(
            &re,
            rhapsody_store::OUTCOME_LIMIT,
            &format!("{account} limit"),
        );
        self.persist_totals();
        if crate::managerrun::is_manager_key(&re.issue.id)
            && self.continue_limited_manager(&re, worker_finished)
        {
            return;
        }
        self.claimed.insert(re.issue.id.clone());
        self.completed.remove(&re.issue.id);
        self.limit_policy.credit_approvals.remove(&re.issue.id);
        if let Some(review) = &re.review
            && let Some(attempt) = self.review_attempts.get_mut(&review.watch_key())
        {
            attempt.pending_exit = false;
        }
        let (harness, model) =
            self.resolved_harness_model(&re.harness, &re.model_override, &re.project_slug);
        let list = self.engine_list(
            &re.identity,
            EngineSpec {
                harness,
                model,
                effort: re.model_override.effort.clone(),
            },
        );
        // Never go back to primary or revisit an earlier fallback during this piece of work.
        let next = list
            .iter()
            .enumerate()
            .skip(re.engine_index.saturating_add(1))
            .find_map(|(i, e)| self.engine_usable(e, &re.project_slug).then_some(i));
        let mut item = LimitItem {
            account: account.into(),
            windows: self
                .accounts
                .snapshot(now)
                .into_iter()
                .find(|v| v.account == account)
                .map(|v| v.windows)
                .unwrap_or_default(),
            tickets: vec![LimitTicket {
                ticket: re.issue.identifier.clone(),
                identity: re.identity.clone(),
                fallback: list.clone(),
                healthy: list
                    .iter()
                    .map(|e| self.engine_usable(e, &re.project_slug))
                    .collect(),
                mid_review: re.review.is_some()
                    || self.eff.as_ref().is_some_and(|eff| {
                        eff.review_states
                            .contains(&rhapsody_core::normalize_state(&re.issue.state))
                    }),
                engine_index: re.engine_index,
                handoff_note: None,
            }],
            credits_policy: cfg.credits.clone(),
            resets_at_s: self
                .accounts
                .tightest(account, now)
                .map_or(0, |w| w.resets_at_s),
            budgets: self
                .eff
                .as_ref()
                .map(|e| e.cfg.budgets.clone())
                .unwrap_or_default(),
            manager_status: String::new(),
            proposal: None,
            manager_runs: 0,
        };
        // Manager runs have no teammate profile or ticket workspace, and their credential/config
        // directory is deliberately removed on exit. L5 owns their ordered engine fallback; a
        // limited manager therefore goes to the human feed rather than pretending it can park.
        let note = match self.write_limit_note(&re, account) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, ticket = %re.issue.identifier, "limit: note could not be written; holding for a human");
                None
            }
        };
        for ticket in &mut item.tickets {
            ticket.handoff_note = note.clone();
        }
        let outcome = if note.is_none() || crate::managerrun::is_manager_key(&re.issue.id) {
            HandoffOutcome::ManagerItem(item.clone())
        } else {
            decide(&self.accounts, item.clone(), &cfg, now, next)
        };
        if matches!(outcome, HandoffOutcome::ManagerItem(_)) {
            self.queue_limit_item(item);
        }
        let delay = match outcome {
            HandoffOutcome::Park { resume_at_s } => {
                Some(resume_at_s.saturating_sub(now).saturating_mul(1000))
            }
            HandoffOutcome::Switch { .. } => Some(0),
            HandoffOutcome::ManagerItem(_) => None,
        };
        self.report_limit_handoff(&re, account, &outcome, note.as_deref());
        let id = re.issue.id.clone();
        self.limit_policy.suspended.insert(
            re.issue.id.clone(),
            Suspended {
                run: re,
                outcome,
                note,
                worker_finished,
                account: self
                    .accounts
                    .snapshot(now)
                    .into_iter()
                    .find(|a| a.account == account),
            },
        );
        self.persist_limit_resume(&id);
        if let Some(delay) = delay {
            self.arm_retry_timer(&format!("limit:{id}"), delay);
        }
    }

    fn write_limit_note(&self, re: &RunningEntry, account: &str) -> std::io::Result<PathBuf> {
        let dir = self
            .limit_policy
            .docs_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rhapsody/docs")))
            .ok_or_else(|| std::io::Error::other("no runtime home for limit note"))?;
        std::fs::create_dir_all(&dir)?;
        let ticket = rhapsody_workspace::sanitize_key(&re.issue.identifier);
        let progress = bounded_file(&dir.join(format!("{ticket}-progress.md")), false)
            .unwrap_or_else(|e| format!("Progress file unavailable: {e}"));
        let transcript = if re.transcript_path.is_empty() {
            "No transcript recorded.".into()
        } else {
            bounded_file(&PathBuf::from(&re.transcript_path), true)
                .unwrap_or_else(|e| format!("Transcript unavailable: {e}"))
        };
        let path = dir.join(format!("{ticket}-limit-handoff.md"));
        let text = format!(
            "# Limit handoff: {}\n\nAccount: {account}\nIdentity: {}\nEngine: {} / {} (index {})\nSession: {}\n\n## Last progress file (data)\n{progress}\n\n## Transcript tail (data)\n{transcript}\n\n## Recent events (data)\n{}\n\nHANDOFF: limit\n",
            re.issue.identifier,
            re.identity,
            re.harness,
            re.model,
            re.engine_index,
            re.thread_id,
            re.recent_events
                .iter()
                .map(|e| format!("{}: {}", e.event, e.message))
                .collect::<Vec<_>>()
                .join("\n")
        );
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let temporary = path.with_extension(format!("md.{}.{}.tmp", std::process::id(), re.run_id));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            std::fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        Ok(path)
    }

    pub(crate) async fn resume_limit(&mut self, id: &str) {
        if self.running.contains_key(id) {
            self.limit_policy.suspended.remove(id);
            return;
        }
        let Some(suspended) = self.limit_policy.suspended.get(id) else {
            return;
        };
        if !suspended.worker_finished {
            return;
        }
        match suspended.outcome {
            HandoffOutcome::Park { resume_at_s } if (self.now)().timestamp() < resume_at_s => {
                return;
            }
            HandoffOutcome::ManagerItem(_) => return,
            _ => {}
        }
        if self.drain.is_draining() {
            return;
        }
        let old = suspended.run.clone();
        let Some(eff) = &self.eff else {
            return;
        };
        let project = eff.project_by_slug(&old.project_slug);
        if (!old.project_slug.is_empty() && project.is_none())
            || project.is_some_and(|p| p.disabled)
        {
            self.drop_limit(id);
            return;
        }
        let tracker = project.map_or_else(|| eff.tracker.clone(), |p| p.tracker.clone());
        let parked = if let Some(review) = &old.review {
            match self.store.get_review_watch(&review.watch_key()) {
                Ok(Some(row))
                    if row.open
                        && row.requested_sha == review.head_sha
                        && self.review_generation(&review.watch_key()) == review.generation =>
                {
                    Some(old.issue.clone())
                }
                Ok(_) => None,
                Err(error) => {
                    tracing::warn!(%error, "limit: review eligibility unreadable; keeping parked");
                    return;
                }
            }
        } else {
            match tracker.fetch_parked_issue(id).await {
                Ok(c) => c,
                Err(error) => {
                    tracing::warn!(%error, "limit: eligibility unreadable; keeping parked");
                    return;
                }
            }
        };
        let Some(iss) = parked else {
            self.drop_limit(id);
            return;
        };
        let route = self
            .eff
            .as_ref()
            .and_then(|e| e.project_by_slug(&old.project_slug))
            .map(|p| crate::retry::DispatchRoute {
                slug: p.slug.clone(),
                group: p.group.clone(),
                repo: p.repo.clone(),
                model: p.model.clone(),
                workspace_mode: p.workspace_mode.clone(),
            });
        let Some(eff) = &self.eff else {
            return;
        };
        let empty = HashSet::new();
        if (old.review.is_none()
            && !crate::eligible(
                &iss,
                &self.running_id_set(),
                &empty,
                &crate::prepare::eligibility_gate_for(eff, route.as_ref()),
            ))
            || self
                .planned_identity(&iss, &self.teammate_load())
                .unwrap_or_default()
                != old.identity
        {
            self.drop_limit(id);
            return;
        }
        let capacity = if old.review.is_some() {
            crate::global_slots(
                eff.max_concurrent_reviews.unwrap_or(eff.max_concurrent),
                self.review_pool_holders(),
            )
        } else {
            crate::global_slots(eff.max_concurrent, self.implementation_pool_holders())
        };
        let project_full =
            project.is_some_and(|p| self.running_in_project_group(&p.group) >= p.max_concurrent);
        if capacity <= 0 || project_full {
            return;
        }
        let Some(suspended) = self.limit_policy.suspended.remove(id) else {
            return;
        };
        let primary = EngineSpec {
            harness: self.effective_harness(&old.harness),
            model: old.model.clone(),
            effort: old.model_override.effort.clone(),
        };
        let list = self.engine_list(&old.identity, primary.clone());
        let (index, spec, resume) = match suspended.outcome {
            HandoffOutcome::Park { .. } => (old.engine_index, primary, old.thread_id.clone()),
            HandoffOutcome::Switch { engine } => match list.get(engine) {
                Some(spec) => (engine, spec.clone(), String::new()),
                None => {
                    self.limit_policy.suspended.insert(id.into(), suspended);
                    return;
                }
            },
            HandoffOutcome::ManagerItem(_) => {
                self.limit_policy.suspended.insert(id.into(), suspended);
                return;
            }
        };
        if !self.engine_usable(&spec, &old.project_slug)
            && !self.credit_approved(
                id,
                &suspended
                    .account
                    .as_ref()
                    .map(|a| a.account.clone())
                    .unwrap_or_default(),
            )
        {
            self.limit_policy.suspended.insert(id.into(), suspended);
            return;
        }
        let engine = crate::dispatch::DispatchEngine {
            index,
            spec,
            handoff_note: suspended.note.clone(),
        };
        self.limit_policy.resume_sessions.insert(id.into(), resume);
        if old.brokered && matches!(suspended.outcome, HandoffOutcome::Park { .. }) {
            if !self.preparation_enabled() {
                tracing::warn!(ticket = %old.issue.identifier, "limit: brokered resume requires provider preparation; keeping parked");
                self.limit_policy.suspended.insert(id.into(), suspended);
                return;
            }
            if let (Some(review), Some(route)) = (old.review.clone(), route.clone()) {
                let target = crate::prepare::PreparedTarget::Review {
                    issue: iss,
                    run: Box::new(review),
                    route,
                    commit: None,
                };
                self.begin_preparation(target, true);
            } else {
                self.dispatch_or_prepare(
                    iss,
                    Some(old.retry_attempt),
                    route,
                    old.stack_context.clone(),
                );
            }
            if !self.running.contains_key(id) && !self.preparing.contains(id) {
                self.limit_policy.suspended.insert(id.into(), suspended);
            }
            return;
        }
        if let Some(review) = &old.review {
            self.pending_review.insert(id.into(), review.clone());
        }
        let dispatched = self.dispatch_issue_with_engine(
            iss,
            Some(old.retry_attempt),
            route,
            old.stack_context.clone(),
            engine,
        );
        if !matches!(dispatched, Ok(true)) {
            self.limit_policy.suspended.insert(id.into(), suspended);
        }
    }

    fn drop_limit(&mut self, id: &str) {
        self.limit_policy.credit_approvals.remove(id);
        if let Some(s) = self.limit_policy.suspended.remove(id) {
            self.claimed.remove(id);
            self.persist_release(&s.run.issue.identifier);
            self.limit_policy.items.retain(|item| {
                !item
                    .tickets
                    .iter()
                    .any(|t| t.ticket == s.run.issue.identifier)
            });
        }
    }

    pub(crate) fn persist_limit_resume(&self, id: &str) {
        let Some(s) = self.limit_policy.suspended.get(id) else {
            return;
        };
        let r = &s.run;
        let record = ResumeRecord {
            issue: r.issue.clone(),
            identity: r.identity.clone(),
            harness: self.effective_harness(&r.harness),
            model: r.model.clone(),
            effort: r.model_override.effort.clone(),
            engine_index: r.engine_index,
            session: r.thread_id.clone(),
            attempt: r.retry_attempt,
            project: r.project_slug.clone(),
            repo: r.project_repo.clone(),
            group: r.project_group.clone(),
            brokered: r.brokered,
            stack_context: r.stack_context.clone(),
            review: r.review.clone(),
            outcome: s.outcome.clone(),
            note: s.note.clone(),
            account: s.account.clone(),
            credit_approval: self.limit_policy.credit_approvals.get(id).cloned(),
            reassignment_pending: self.limit_policy.pending_reassignments.contains_key(id),
        };
        match serde_json::to_string(&record) {
            Ok(json) => {
                let due = match s.outcome {
                    HandoffOutcome::Park { resume_at_s } => resume_at_s,
                    _ => (self.now)().timestamp(),
                };
                self.persist_retry(
                    &r.issue.identifier,
                    r.retry_attempt,
                    due.saturating_mul(1000),
                    &format!("{RETRY_LIMIT_PREFIX}{json}"),
                    &r.project_slug,
                );
            }
            Err(error) => {
                tracing::warn!(%error, ticket = %r.issue.identifier, "limit: resume metadata could not be recorded")
            }
        }
    }

    pub(crate) fn restore_limit_retry(&mut self, key: &str, text: &str) -> Option<String> {
        let json = text.strip_prefix(RETRY_LIMIT_PREFIX)?;
        match serde_json::from_str::<ResumeRecord>(json) {
            Ok(mut record) => {
                if let HandoffOutcome::ManagerItem(item) = &mut record.outcome
                    && (record.reassignment_pending || item.manager_status == "reassign pending")
                {
                    item.manager_status =
                        "reassign interrupted: inspect labels before resuming".into();
                }
                if let Some(account) = &record.account
                    && account.source != "budget"
                {
                    self.accounts.observe(
                        &account.account,
                        rhapsody_agent::ratelimit::LimitObs {
                            status: match account.status.as_str() {
                                "rejected" => rhapsody_agent::ratelimit::LimitStatus::Rejected,
                                "warning" => rhapsody_agent::ratelimit::LimitStatus::Warning,
                                _ => rhapsody_agent::ratelimit::LimitStatus::Allowed,
                            },
                            windows: account
                                .windows
                                .iter()
                                .map(|w| rhapsody_agent::ratelimit::WindowObs {
                                    window: w.window.clone(),
                                    utilization: w.utilization,
                                    resets_at_s: w.resets_at_s,
                                })
                                .collect(),
                            using_credits: account.using_credits,
                            source: if account.source == "probe" {
                                "probe"
                            } else {
                                "stream"
                            },
                            observed_at_s: account.last_seen_s,
                        },
                    );
                }
                let mut run = RunningEntry::empty(record.issue);
                run.identity = record.identity;
                run.harness = record.harness;
                run.model = record.model.clone();
                run.model_override = rhapsody_agent::ModelOverride {
                    identity: run.identity.clone(),
                    model: record.model,
                    effort: record.effort,
                };
                run.engine_index = record.engine_index;
                run.thread_id = record.session;
                run.retry_attempt = record.attempt;
                run.project_slug = record.project;
                run.project_repo = record.repo;
                run.project_group = record.group;
                run.brokered = record.brokered;
                run.stack_context = record.stack_context;
                run.review = record.review;
                let id = run.issue.id.clone();
                if let Some(approval) = record.credit_approval {
                    self.limit_policy
                        .credit_approvals
                        .insert(id.clone(), approval);
                }
                self.clear_retry(key);
                self.claimed.remove(key);
                self.claimed.insert(id.clone());
                if let HandoffOutcome::ManagerItem(item) = &record.outcome {
                    self.queue_limit_item(item.clone());
                }
                self.limit_policy.suspended.insert(
                    id.clone(),
                    Suspended {
                        run,
                        outcome: record.outcome,
                        note: record.note,
                        worker_finished: true,
                        account: record.account,
                    },
                );
                Some(id)
            }
            Err(error) => {
                tracing::warn!(%error, "limit: corrupt resume metadata; holding for a human");
                None
            }
        }
    }

    pub(crate) async fn recover_limit_retry(&mut self, key: &str, text: &str) {
        if let Some(id) = self.restore_limit_retry(key, text) {
            self.resume_limit(&id).await;
        }
    }

    pub(crate) async fn resume_due_limits(&mut self) {
        let ids: Vec<_> = self.limit_policy.suspended.keys().cloned().collect();
        for id in ids {
            self.resume_limit(&id).await;
        }
    }

    pub(crate) fn feed_budget_windows(&mut self) {
        let Some(eff) = &self.eff else {
            return;
        };
        for (account, old) in &self.limit_policy.fed_budgets {
            if eff.cfg.budgets.get(account).is_none_or(|new| {
                new.daily_usd != old.daily_usd || new.daily_tokens != old.daily_tokens
            }) {
                self.accounts.forget_budget(account);
            }
        }
        self.limit_policy.fed_budgets = eff.cfg.budgets.clone();
        if !eff
            .cfg
            .budgets
            .values()
            .any(|b| b.daily_usd > 0.0 || b.daily_tokens > 0)
        {
            return;
        }
        let now = (self.now)();
        let local = now.with_timezone(&chrono::Local);
        use chrono::TimeZone;
        let midnight = local
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|t| chrono::Local.from_local_datetime(&t).earliest());
        let Some(start) = midnight else {
            return;
        };
        let Some(tomorrow) = local
            .date_naive()
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
        else {
            return;
        };
        let since = start.to_rfc3339();
        let tokens = self.store.tokens_by_provider(&since).unwrap_or_default();
        for (account, budget) in &eff.cfg.budgets {
            if budget.daily_usd <= 0.0 && budget.daily_tokens <= 0 {
                continue;
            }
            let usd = if budget.daily_usd > 0.0 {
                crate::budget::daily_spend_usd(self.store(), account, &since) / budget.daily_usd
            } else {
                0.0
            };
            // Unknown live harness-only cost is a dispatch refusal in L2, not evidence that the
            // account spent its whole allowance. Do not latch a fictitious wall into the ledger.
            if !usd.is_finite() {
                continue;
            }
            let token = if budget.daily_tokens > 0 {
                tokens
                    .iter()
                    .find(|t| &t.provider == account)
                    .map_or(0, |t| t.total_tokens) as f64
                    / budget.daily_tokens as f64
            } else {
                0.0
            };
            let utilization = usd.max(token).clamp(0.0, 1.0);
            self.accounts.observe(
                account,
                rhapsody_agent::ratelimit::LimitObs {
                    status: if utilization >= 1.0 {
                        rhapsody_agent::ratelimit::LimitStatus::Rejected
                    } else {
                        rhapsody_agent::ratelimit::LimitStatus::Allowed
                    },
                    windows: vec![rhapsody_agent::ratelimit::WindowObs {
                        window: "daily".into(),
                        utilization,
                        resets_at_s: tomorrow.timestamp(),
                    }],
                    using_credits: false,
                    source: "budget",
                    observed_at_s: now.timestamp(),
                },
            );
        }
    }

    pub(crate) fn observe_credit_cost(&mut self, id: &str, ev: &rhapsody_agent::Event) {
        let Some(re) = self.running.get(id) else {
            return;
        };
        let now = (self.now)().timestamp();
        let account = re.pricing.account.clone();
        let Some(cost) = ev.cost_usd.filter(|v| v.is_finite() && *v >= 0.0) else {
            if matches!(
                ev.event_type.as_str(),
                rhapsody_agent::EVENT_TURN_COMPLETED | rhapsody_agent::EVENT_TURN_FAILED
            ) && self.limits_config().credits == "daily_cap"
                && self
                    .accounts
                    .snapshot(now)
                    .iter()
                    .any(|v| v.account == account && v.using_credits)
            {
                tracing::warn!(%account, "limit: the harness omitted overage cost; daily_cap cannot be enforced, refusing credits");
                if let Some(re) = self.running.get_mut(id) {
                    re.event_seq += 1;
                    let row = rhapsody_store::EventRow {
                        seq: re.event_seq,
                        at: crate::persist::rfc3339((self.now)()),
                        kind: "limit.credit_cost_unknown".into(),
                        tool: String::new(),
                        text: serde_json::json!({"account":account}).to_string(),
                    };
                    if let Err(error) = self.store.append_events(re.run_id, &[row]) {
                        tracing::warn!(%error, "limit: unknown credit cost could not be recorded");
                    }
                }
                self.limit_policy
                    .credit_spent
                    .insert((account, day(now)), f64::INFINITY);
            }
            return;
        };
        let key = (re.run_id, re.turn_count.max(1));
        let previous = self
            .limit_policy
            .cost_baselines
            .insert(key, cost)
            .unwrap_or(0.0);
        if self
            .accounts
            .snapshot(now)
            .iter()
            .any(|v| v.account == account && v.using_credits)
        {
            let delta = (cost - previous).max(0.0);
            if delta > 0.0 {
                let cached = self.credit_spent(&account, now);
                if let Some(re) = self.running.get_mut(id) {
                    re.event_seq += 1;
                    let row = rhapsody_store::EventRow {
                        seq: re.event_seq,
                        at: crate::persist::rfc3339((self.now)()),
                        kind: "limit.credit_cost".into(),
                        tool: String::new(),
                        text: serde_json::json!({"account":account,"usd":delta}).to_string(),
                    };
                    let spent = match self.store.append_events(re.run_id, &[row]) {
                        Ok(()) => cached + delta,
                        Err(error) => {
                            tracing::warn!(%error, "limit: credit estimate could not be recorded; refusing further credits");
                            f64::INFINITY
                        }
                    };
                    self.limit_policy
                        .credit_spent
                        .insert((account, day(now)), spent);
                }
            }
        }
    }

    fn credit_spent(&self, account: &str, now_s: i64) -> f64 {
        if !self.store.usd_accounting_available() {
            return f64::INFINITY;
        }
        self.limit_policy.credit_spent.get(&(account.into(), day(now_s))).copied().unwrap_or_else(|| {
            match self.store.account_credit_spend(account, &day_start(now_s)) {Ok(usd) if usd.is_finite() && usd >= 0.0 => usd, Ok(_) => f64::INFINITY, Err(error) => {tracing::warn!(%error, account, "limit: credit spend unreadable; refusing credits"); f64::INFINITY}}
        })
    }

    pub(crate) fn limit_holds(&self) -> Vec<String> {
        let mut holds = self
            .limit_policy
            .holds
            .values()
            .cloned()
            .collect::<Vec<_>>();
        holds.extend(
            self.budget_ledger
                .held(self.budget_hold_ttl())
                .into_iter()
                .filter(|h| h.reason.starts_with("waiting:") && h.reason.contains(" limit"))
                .map(|h| format!("{}: {}", h.subject, h.reason)),
        );
        holds
    }

    pub(crate) fn request_limit_resume(&mut self, id: &str) -> bool {
        // A handled refusal must retain the claim: Resume finalize otherwise releases it to
        // ordinary dispatch while off-loop label writes are still changing the identity.
        if self.limit_policy.pending_reassignments.contains_key(id) {
            tracing::warn!(
                issue_id = id,
                "limit: Resume refused while reassignment is pending"
            );
            return true;
        }
        let Some(s) = self.limit_policy.suspended.get(id) else {
            return false;
        };
        let run = s.run.clone();
        let primary = EngineSpec {
            harness: self.effective_harness(&run.harness),
            model: run.model.clone(),
            effort: run.model_override.effort.clone(),
        };
        let list = self.engine_list(&run.identity, primary.clone());
        let outcome = if self.engine_usable(&primary, &run.project_slug) {
            Some(HandoffOutcome::Park {
                resume_at_s: (self.now)().timestamp(),
            })
        } else {
            list.iter()
                .enumerate()
                .skip(run.engine_index.saturating_add(1))
                .find_map(|(engine, spec)| {
                    self.engine_usable(spec, &run.project_slug)
                        .then_some(HandoffOutcome::Switch { engine })
                })
        };
        if let Some(outcome) = outcome {
            if let Some(s) = self.limit_policy.suspended.get_mut(id) {
                s.outcome = outcome;
            }
            self.remove_limit_item_ticket(&run.issue.identifier);
            self.persist_limit_resume(id);
            self.arm_retry_timer(&format!("limit:{id}"), 0);
        }
        self.claimed.insert(id.into());
        true
    }

    fn queue_limit_item(&mut self, item: LimitItem) {
        if let Some(existing) = self
            .limit_policy
            .items
            .iter_mut()
            .find(|i| i.account == item.account)
        {
            existing.windows = item.windows;
            existing.credits_policy = item.credits_policy;
            existing.resets_at_s = item.resets_at_s;
            existing.budgets = item.budgets;
            existing.manager_status = item.manager_status;
            existing.proposal = item.proposal;
            existing.manager_runs = item.manager_runs;
            for ticket in item.tickets {
                existing.tickets.retain(|t| t.ticket != ticket.ticket);
                existing.tickets.push(ticket);
            }
        } else {
            self.limit_policy.items.push(item);
        }
    }

    pub(crate) fn remove_limit_item_ticket(&mut self, ticket: &str) {
        for item in &mut self.limit_policy.items {
            item.tickets.retain(|t| t.ticket != ticket);
        }
        self.limit_policy
            .items
            .retain(|item| !item.tickets.is_empty());
    }
}

fn day(now_s: i64) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&day_start(now_s))
        .map(|d| d.timestamp())
        .unwrap_or(now_s.div_euclid(86400).saturating_mul(86400))
}

fn day_start(now_s: i64) -> String {
    chrono::DateTime::from_timestamp(now_s, 0)
        .map(crate::budget::local_day_start_at)
        .unwrap_or_default()
}

fn bounded_file(path: &std::path::Path, tail: bool) -> std::io::Result<String> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    const CAP: u64 = 64 * 1024;
    if tail {
        file.seek(std::io::SeekFrom::Start(len.saturating_sub(CAP)))?;
    }
    let mut bytes = Vec::new();
    file.take(CAP).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{TempDir, empty_effective, issue};
    use rhapsody_agent::ratelimit::{LimitObs, LimitStatus, WindowObs};
    use rhapsody_store::{Sqlite, Store, StorePath};
    use rhapsody_tracker::fake::Fake;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };

    fn observation(pct: f64, reset: i64) -> LimitObs {
        let mut obs = rhapsody_agent::ratelimit::parse_claude_rate_limit(
            include_bytes!("../../agent/testdata/limits/allowed.jsonl")
                .split(|b| *b == b'\n')
                .next()
                .unwrap(),
        )
        .unwrap();
        obs.observed_at_s = 1000;
        obs.windows = vec![WindowObs {
            window: "five_hour".into(),
            utilization: pct,
            resets_at_s: reset,
        }];
        obs
    }

    fn setup(
        fallback: bool,
    ) -> (
        Orchestrator,
        Arc<dyn Store + Send + Sync>,
        Arc<AtomicI64>,
        TempDir,
    ) {
        let dir = TempDir::new();
        let mut tr = Fake::new();
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        tr.candidates = vec![iss];
        let mut o = Orchestrator::new("not-read.md");
        let mut eff = empty_effective(Arc::new(tr));
        eff.max_concurrent = 10;
        eff.active_states = crate::testsupport::active_set();
        o.eff = Some(eff);
        let st: Arc<dyn Store + Send + Sync> = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        o.set_store(st.clone());
        o.spawn = Some(Box::new(|_, _, _| {}));
        let clock = Arc::new(AtomicI64::new(1000));
        let now = clock.clone();
        o.now = Box::new(move || {
            chrono::DateTime::from_timestamp(now.load(Ordering::SeqCst), 0).unwrap()
        });
        let teams = rhapsody_config::teams::Teams {
            enabled: true,
            roster: vec![rhapsody_config::teams::Identity {
                name: "alice".into(),
                profile: "worker".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(dir.child("worker.md"), if fallback {
            "---\nharness: claude\nmodel: opus\nfallback:\n  - {harness: opencode, model: fireworks-ai/model, effort: high}\n---\nWorker\n"
        } else { "---\nharness: claude\nmodel: opus\n---\nWorker\n" }).unwrap();
        o.teams = Some(teams);
        o.teams_profiles_dir = Some(PathBuf::from(&dir.path));
        o.limit_policy.docs_dir = Some(PathBuf::from(&dir.path));
        (o, st, clock, dir)
    }

    fn dispatch(o: &mut Orchestrator) {
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        o.dispatch_issue(iss, Some(2), None, String::new());
        let at = o.running["1"].started_at;
        o.bind_account("1", at, "claude-subscription");
        o.running.get_mut("1").unwrap().thread_id = "session-before-limit".into();
    }

    fn item() -> LimitItem {
        LimitItem {
            account: "claude-subscription".into(),
            windows: vec![],
            tickets: vec![],
            credits_policy: "never".into(),
            resets_at_s: 0,
            budgets: BTreeMap::new(),
            manager_status: String::new(),
            proposal: None,
            manager_runs: 0,
        }
    }

    #[test]
    fn push_on_transition_not_per_run() {
        let (mut o, _, clock, _dir) = setup(true);
        let mut rx = o.open_limit_report_channel();
        dispatch(&mut o);
        let mut second = o.running["1"].clone();
        second.issue.id = "2".into();
        second.issue.identifier = "MT-2".into();
        o.accounts.bind_run(
            &crate::accounts::run_key("2", second.started_at),
            "claude-subscription",
        );
        o.running.insert("2".into(), second);
        o.accounts
            .observe("claude-subscription", observation(0.95, 1200));
        o.enforce_limits();
        let crate::limitreport::Report::Push { body, .. } = rx.try_recv().unwrap() else {
            panic!("push expected")
        };
        assert!(body.contains("95%"), "{body}");
        assert!(body.contains("2 runs handing off"), "{body}");
        assert!(body.contains("2 parked until"), "{body}");
        o.enforce_limits();
        assert!(
            rx.try_recv().is_err(),
            "same state never pushes per run/tick"
        );
        clock.store(1201, Ordering::SeqCst);
        o.enforce_limits();
        let crate::limitreport::Report::Push { body, .. } = rx.try_recv().unwrap() else {
            panic!("reset push expected")
        };
        assert!(body.contains("ok"), "{body}");
    }

    #[test]
    fn credit_spend_push() {
        for policy in ["never", "daily_cap", "manager_urgent", "always"] {
            let (mut o, _, clock, _dir) = setup(false);
            let mut rx = o.open_limit_report_channel();
            o.eff.as_mut().unwrap().cfg.limits.credits = policy.into();
            dispatch(&mut o);
            let mut obs = observation(0.9, 200000);
            obs.using_credits = true;
            o.accounts.observe("claude-subscription", obs);
            o.enforce_limits();
            let pushes: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert_eq!(pushes.iter().filter(|r| matches!(r, crate::limitreport::Report::Push { title, body, .. } if title.contains("credits") && body.contains(policy))).count(), 1);
            o.enforce_limits();
            assert!(rx.try_recv().is_err());
            clock.store(100000, Ordering::SeqCst);
            // A new day's credit spend is a fresh observation, even after `never` latched a wall.
            let mut obs = observation(0.9, 300000);
            obs.observed_at_s = 100000;
            obs.using_credits = true;
            o.accounts.observe("claude-subscription", obs);
            o.enforce_limits();
            assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|r| matches!(r, crate::limitreport::Report::Push { title, .. } if title.contains("credits"))));
        }
    }

    #[tokio::test]
    async fn room_post_per_handoff() {
        use rhapsody_config::room::{Cursor, LocalRoom};
        let (mut o, _, _, dir) = setup(true);
        let mut rx = o.open_limit_report_channel();
        let room = LocalRoom::new(dir.child("room"));
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 9000));
        o.enforce_limits();
        while let Ok(report) = rx.try_recv() {
            crate::limitreport::perform_report(report, Some(&room), &[], None).await;
        }
        let posts = room.read_since("alice", &Cursor::default(), 20).unwrap();
        assert_eq!(posts.messages.len(), 1);
        let body = &posts.messages[0].body;
        for expected in [
            "MT-1",
            "claude",
            "opus",
            "opencode",
            "fireworks-ai/model",
            "MT-1-limit-handoff.md",
        ] {
            assert!(body.contains(expected), "missing {expected}: {body}");
        }
        o.enforce_limits();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tracker_comment_is_tokenless() {
        let (mut o, _, _, _dir) = setup(false);
        let tracker = Arc::new(Fake::new());
        o.eff.as_mut().unwrap().tracker = tracker.clone();
        let mut rx = o.open_limit_report_channel();
        dispatch(&mut o);
        o.running.get_mut("1").unwrap().model = "model @symphony".into();
        o.eff.as_mut().unwrap().summon_token = "#wake".into();
        o.running.get_mut("1").unwrap().identity = "alice #wake".into();
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        while let Ok(report) = rx.try_recv() {
            crate::limitreport::perform_report(report, None, &[], None).await;
        }
        let comments = tracker.create_comment_calls();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].body.contains("resumes"));
        assert!(!crate::reviewnotify::summons_author(
            &comments[0].body,
            "#wake"
        ));
        assert!(
            !comments[0].body.contains('@'),
            "report must never mint a mention"
        );
    }

    #[test]
    fn transition_warn_log_fields() {
        let (mut o, _, _, _dir) = setup(false);
        o.accounts
            .observe("claude-subscription", observation(0.81, 1200));
        let (_, events) = crate::testsupport::capture_events(|| o.enforce_limits());
        let event = events
            .iter()
            .find(|e| e.message.contains("account state transition"))
            .unwrap();
        for field in ["account", "window", "utilization", "reset", "state"] {
            assert!(event.fields.contains_key(field), "missing {field}");
        }
        assert_eq!(event.level, "WARN");
    }

    #[tokio::test]
    async fn stripping_mentions_cannot_create_a_custom_summon() {
        for token in ["foo", "removed", "aba"] {
            let (mut o, _, _, _dir) = setup(false);
            let tracker = Arc::new(Fake::new());
            o.eff.as_mut().unwrap().tracker = tracker.clone();
            o.eff.as_mut().unwrap().summon_token = token.into();
            let mut rx = o.open_limit_report_channel();
            dispatch(&mut o);
            let mut run = o.running["1"].clone();
            run.model = "f@oo rem@oved aababa".into();
            o.report_limit_handoff(
                &run,
                "claude-subscription",
                &HandoffOutcome::Park { resume_at_s: 1320 },
                None,
            );
            crate::limitreport::perform_report(rx.try_recv().unwrap(), None, &[], None).await;
            let calls = tracker.create_comment_calls();
            assert_eq!(calls.len(), 1);
            assert!(
                !crate::reviewnotify::summons_author(&calls[0].body, token),
                "{token}: {}",
                calls[0].body
            );
            assert!(!calls[0].body.contains(token));
        }
    }

    #[tokio::test]
    async fn identity_limit_handoff_reports_the_new_teammate_once() {
        use rhapsody_config::room::{Cursor, LocalRoom};
        let (mut o, store, _, dir) = setup(false);
        let tracker = Arc::new(Fake::new());
        o.eff.as_mut().unwrap().tracker = tracker.clone();
        let mut rx = o.open_limit_report_channel();
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 9000));
        o.enforce_limits();
        while rx.try_recv().is_ok() {}
        // The policy executor has reassigned the ticket and seeded the note; dispatch reports
        // the actual next owner rather than announcing an uncommitted label decision.
        o.limit_policy.suspended.remove("1");
        std::fs::write(
            dir.child("bob.md"),
            "---\nharness: opencode\nmodel: fireworks-ai/model\n---\nWorker\n",
        )
        .unwrap();
        o.teams
            .as_mut()
            .unwrap()
            .roster
            .push(rhapsody_config::teams::Identity {
                name: "bob".into(),
                profile: "bob".into(),
                ..Default::default()
            });
        let mut next = issue("1", "MT-1", "Todo");
        next.labels = Some(vec!["rhapsody:@bob".into()]);
        o.dispatch_issue(next, Some(2), None, String::new());
        assert_eq!(o.running["1"].identity, "bob");
        let report = rx.try_recv().expect("the identity handoff must report");
        let room = LocalRoom::new(dir.child("room"));
        crate::limitreport::perform_report(report, Some(&room), &[], None).await;
        let posts = room.read_since("bob", &Cursor::default(), 20).unwrap();
        assert_eq!(posts.messages.len(), 1);
        for word in [
            "MT-1",
            "alice",
            "bob",
            "fireworks-ai/model",
            "resumes",
            "MT-1-limit-handoff.md",
        ] {
            assert!(posts.messages[0].body.contains(word));
        }
        assert_eq!(tracker.create_comment_calls().len(), 1);
        let text = &tracker.create_comment_calls()[0].body;
        assert!(!text.contains('@'));
        let events = store.run_events(o.running["1"].run_id).unwrap();
        let handoff = events.iter().find(|e| e.kind == "limit.handoff").unwrap();
        assert!(handoff.text.contains("handed_off"));
        o.enforce_limits();
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|r| matches!(r, crate::limitreport::Report::Handoff { .. }))
        );
    }

    #[test]
    fn account_reporting_uses_configured_levels_and_unknown_costs() {
        let (mut o, store, _, _dir) = setup(false);
        dispatch(&mut o);
        let mut limits = o.limits_config();
        limits.thresholds.stop_new = 85.0;
        o.reads.write().unwrap().limits = limits;
        o.accounts
            .observe("claude-subscription", observation(0.86, 5000));
        let at = crate::budget::local_day_start_at((o.now)());
        let mut spend = rhapsody_store::TurnSpend {
            turn: 1,
            at,
            provider: "anthropic".into(),
            account: "claude-subscription".into(),
            usd: Some(1.25),
            ..Default::default()
        };
        store.set_turn_spend(o.running["1"].run_id, &spend).unwrap();
        let view = o.control().accounts(1000).remove(0);
        assert_eq!(view.level.as_deref(), Some("stop-new"));
        assert_eq!(view.cost_kind.as_deref(), Some("api_equivalent"));
        assert_eq!(view.today_usd, Some(1.25));
        spend.usd = None;
        store.set_turn_spend(o.running["1"].run_id, &spend).unwrap();
        assert_eq!(o.control().accounts(1000)[0].today_usd, None);
        assert!(
            o.accounts.snapshot(1000)[0].level.is_none(),
            "reporting never alters raw policy snapshots"
        );
    }

    #[test]
    fn job_report_survives_park_and_switch_and_records_note() {
        for reset in [1200, 9000] {
            let (mut o, store, _, _dir) = setup(true);
            dispatch(&mut o);
            let run_id = o.running["1"].run_id;
            o.accounts
                .observe("claude-subscription", observation(1.0, reset));
            o.enforce_limits();
            let snapshot = crate::snapshot_json::render(&o.build_snapshot());
            let job = &snapshot["limit_jobs"][0];
            assert_eq!(job["ticket"], "MT-1");
            assert!(
                job["note"]
                    .as_str()
                    .unwrap()
                    .ends_with("MT-1-limit-handoff.md")
            );
            if reset == 1200 {
                assert_eq!(job["state"], "parked");
                assert_eq!(job["resume_at_s"], 1320);
            } else {
                assert_eq!(job["state"], "switched");
                assert_eq!(job["model"], "fireworks-ai/model");
            }
            let events = store.run_events(run_id).unwrap();
            assert_eq!(
                events.iter().filter(|e| e.kind == "limit.handoff").count(),
                1
            );
        }
    }

    #[test]
    fn review_b1_credits_do_not_mask_a_rejected_window() {
        for policy in ["daily_cap", "always"] {
            let (mut o, _, _, _dir) = setup(false);
            dispatch(&mut o);
            o.eff.as_mut().unwrap().cfg.limits.credits = policy.into();
            o.eff.as_mut().unwrap().cfg.limits.credits_daily_usd = 10.0;
            let mut credits = observation(1.0, 1200);
            credits.using_credits = true;
            o.accounts.observe("claude-subscription", credits);
            let mut rejected = observation(0.99, 9000);
            rejected.windows[0].window = "seven_day".into();
            rejected.status = LimitStatus::Rejected;
            o.accounts.observe("claude-subscription", rejected);
            assert_eq!(
                level(&o.accounts, "claude-subscription", &o.limits_config(), 1000),
                Level::Wall,
                "{policy} cannot bypass a rejected window"
            );
            assert!(!o.account_usable("claude-subscription"));
            o.enforce_limits();
            assert!(o.running.is_empty());
        }
    }

    #[test]
    fn review_b2_reviewer_routes_by_explicit_provider_not_model_prefix() {
        let (mut o, st, _, dir) = setup(false);
        std::fs::write(
            dir.child("worker.md"),
            "---\nharness: opencode\nprovider: paid\nmodel: openai/model\n---\nReviewer\n",
        )
        .unwrap();
        std::fs::write(
            dir.child("bob.md"),
            "---\nharness: opencode\nprovider: healthy\nmodel: openai/model\n---\nReviewer\n",
        )
        .unwrap();
        for id in ["paid", "healthy"] {
            o.eff.as_mut().unwrap().cfg.providers.insert(
                id.into(),
                rhapsody_config::ProviderDefinition {
                    id: id.into(),
                    protocol: "openai-compatible".into(),
                    base_url: "https://example.test/v1".into(),
                    ..Default::default()
                },
            );
        }
        o.teams
            .as_mut()
            .unwrap()
            .roster
            .push(rhapsody_config::teams::Identity {
                name: "bob".into(),
                profile: "bob".into(),
                ..Default::default()
            });
        o.teams.as_mut().unwrap().quorum.reviewers = 1;
        o.accounts.observe("paid", observation(0.91, 5000));
        let teams = o.teams.as_ref().unwrap();
        assert_eq!(o.reviewer_limit_account(teams, "alice"), "paid");
        assert_eq!(
            crate::quorum::select_reviewers(
                teams,
                "author",
                &HashMap::new(),
                &o.reviewer_exclusions(teams)
            ),
            vec!["bob"]
        );
        let review = crate::review::ReviewRun {
            owner: "owner".into(),
            repo: "repo".into(),
            number: 1,
            reviewer: "alice".into(),
            ..Default::default()
        };
        assert_eq!(
            o.review_projected_pricing(&review.synthetic_issue(), "")
                .account,
            "paid"
        );
        // A model prefix that happens to be limited must not exclude the healthy explicit account.
        o.accounts.observe("openai", observation(0.99, 5000));
        o.accounts
            .observe("chatgpt-subscription", observation(0.99, 5000));
        assert!(!o.reviewer_exclusions(teams).excludes("bob"));
        let watch = review.watch_key();
        let iss = review.synthetic_issue();
        assert!(!o.finish_review_dispatch_prepared(
            review,
            crate::retry::DispatchRoute {
                slug: String::new(),
                group: String::new(),
                repo: String::new(),
                model: String::new(),
                workspace_mode: String::new(),
            },
            iss,
            None,
        ));
        assert!(st.get_review_watch(&watch).unwrap().is_none());
        assert!(o.running.is_empty());
    }

    #[test]
    fn review_b3_note_write_failure_keeps_a_durable_human_suspension() {
        let (mut o, _, _, dir) = setup(false);
        let db = StorePath::Disk(dir.child("limit.db").into());
        let st: Arc<dyn Store + Send + Sync> = Arc::new(Sqlite::open(db.clone()).unwrap());
        o.set_store(st.clone());
        dispatch(&mut o);
        std::fs::write(dir.child("not-a-directory"), "blocked").unwrap();
        o.limit_policy.docs_dir = Some(dir.child("not-a-directory").into());
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        assert!(o.running.is_empty());
        assert!(
            st.load_recovery()
                .unwrap()
                .retries
                .iter()
                .any(|r| r.identifier == "MT-1" && r.error.starts_with(RETRY_LIMIT_PREFIX))
        );
        drop(o);
        drop(st);
        let (mut recovered, _, clock, _dir) = setup(false);
        recovered.set_store(Arc::new(Sqlite::open(db).unwrap()));
        recovered.boot_recovery();
        let suspended = &recovered.limit_policy.suspended["1"];
        assert!(matches!(suspended.outcome, HandoffOutcome::ManagerItem(_)));
        assert_eq!(suspended.run.thread_id, "session-before-limit");
        assert_eq!(suspended.run.retry_attempt, 2);
        assert!(recovered.claimed.contains("1"));
        assert!(recovered.retry_attempts.is_empty());
        assert_eq!(
            recovered.limit_policy.items[0].tickets[0].handoff_note,
            None
        );
        clock.store(1320, Ordering::SeqCst);
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        assert!(recovered.select_dispatch(vec![iss]).is_empty());
    }

    #[test]
    fn review_b4_unknown_credit_cost_survives_reopen_for_the_local_day() {
        let (mut o, _, _, dir) = setup(false);
        let db = StorePath::Disk(dir.child("limit.db").into());
        o.set_store(Arc::new(Sqlite::open(db.clone()).unwrap()));
        dispatch(&mut o);
        o.eff.as_mut().unwrap().cfg.limits.credits = "daily_cap".into();
        o.eff.as_mut().unwrap().cfg.limits.credits_daily_usd = 10.0;
        let mut credits = observation(1.0, 1200);
        credits.using_credits = true;
        o.accounts.observe("claude-subscription", credits);
        o.on_agent_update(crate::AgentUpdate {
            issue_id: "1".into(),
            ev: rhapsody_agent::Event {
                event_type: rhapsody_agent::EVENT_TURN_COMPLETED.into(),
                usage: Some(Default::default()),
                ..Default::default()
            },
        });
        assert!(o.running.is_empty());
        drop(o);
        let (mut recovered, _, clock, _dir) = setup(false);
        recovered.eff.as_mut().unwrap().cfg.limits.credits = "daily_cap".into();
        recovered.eff.as_mut().unwrap().cfg.limits.credits_daily_usd = 10.0;
        recovered.set_store(Arc::new(Sqlite::open(db).unwrap()));
        recovered.boot_recovery();
        // Even after the plan window reset, the day's unknown bill must still refuse credits.
        clock.store(1320, Ordering::SeqCst);
        let mut credits = observation(1.0, 5000);
        credits.observed_at_s = 1320;
        credits.using_credits = true;
        recovered.accounts.observe("claude-subscription", credits);
        assert!(!recovered.account_usable("claude-subscription"));
        assert!(
            recovered
                .credit_spent("claude-subscription", 1320)
                .is_infinite()
        );
        assert_eq!(recovered.credit_spent("other-account", 1320), 0.0);
        let tomorrow = day(1320) + 86400;
        assert_eq!(recovered.credit_spent("claude-subscription", tomorrow), 0.0);
    }

    #[tokio::test]
    async fn parked_recovery_restores_the_hold_before_dispatch_and_the_same_session_after_reset() {
        let (mut o, st, _, _dir) = setup(false);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        let (mut resumed, _, clock, _new_dir) = setup(false);
        resumed.set_store(st);
        resumed.boot_recovery();
        assert_eq!(
            level(
                &resumed.accounts,
                "claude-subscription",
                &Limits::default(),
                1000
            ),
            Level::Wall
        );
        assert!(resumed.limit_policy.suspended.contains_key("1"));
        clock.store(1320, Ordering::SeqCst);
        resumed.resume_limit("1").await;
        assert_eq!(resumed.running["1"].resume_session, "session-before-limit");
        assert_eq!(resumed.running["1"].retry_attempt, 2);
    }

    #[test]
    fn a_removed_or_raised_budget_does_not_leave_its_old_limit_latched() {
        let (mut o, st, _, _dir) = setup(false);
        let run = st
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "COST-1".into(),
                ..Default::default()
            })
            .unwrap();
        st.set_turn_spend(
            run,
            &rhapsody_store::TurnSpend {
                turn: 1,
                at: chrono::DateTime::from_timestamp(900, 0)
                    .unwrap()
                    .to_rfc3339(),
                provider: "fireworks-ai".into(),
                account: "fireworks-ai".into(),
                model: "fireworks-ai/model".into(),
                usd: Some(9.5),
                source: "harness_reported".into(),
                harness_priced: true,
            },
        )
        .unwrap();
        o.eff.as_mut().unwrap().cfg.budgets.insert(
            "fireworks-ai".into(),
            rhapsody_config::ProviderBudget {
                daily_usd: 10.0,
                ..Default::default()
            },
        );
        o.feed_budget_windows();
        assert_eq!(
            level(&o.accounts, "fireworks-ai", &Limits::default(), 1000),
            Level::Handoff
        );
        o.eff
            .as_mut()
            .unwrap()
            .cfg
            .budgets
            .get_mut("fireworks-ai")
            .unwrap()
            .daily_usd = 20.0;
        o.feed_budget_windows();
        assert_eq!(
            level(&o.accounts, "fireworks-ai", &Limits::default(), 1000),
            Level::Ok
        );
        o.eff.as_mut().unwrap().cfg.budgets.clear();
        o.feed_budget_windows();
        assert!(o.accounts.tightest("fireworks-ai", 1000).is_none());
    }

    #[test]
    fn daily_cap_fails_closed_when_the_harness_omits_its_cost() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        o.eff.as_mut().unwrap().cfg.limits.credits = "daily_cap".into();
        o.eff.as_mut().unwrap().cfg.limits.credits_daily_usd = 1.0;
        let mut obs = observation(1.0, 2500);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.on_agent_update(crate::AgentUpdate {
            issue_id: "1".into(),
            ev: rhapsody_agent::Event {
                event_type: rhapsody_agent::EVENT_TURN_COMPLETED.into(),
                usage: Some(Default::default()),
                ..Default::default()
            },
        });
        assert!(o.running.is_empty());
    }

    #[test]
    fn a_limited_candidate_does_not_spend_the_healthy_teammates_only_slot() {
        let (mut o, _, _, dir) = setup(false);
        o.eff.as_mut().unwrap().max_concurrent = 1;
        std::fs::write(
            dir.child("bob.md"),
            "---\nharness: opencode\nmodel: fireworks-ai/model\n---\nReviewer\n",
        )
        .unwrap();
        o.teams
            .as_mut()
            .unwrap()
            .roster
            .push(rhapsody_config::teams::Identity {
                name: "bob".into(),
                profile: "bob".into(),
                ..Default::default()
            });
        o.accounts
            .observe("claude-subscription", observation(0.91, 5000));
        let mut alice = issue("1", "MT-1", "Todo");
        alice.labels = Some(vec!["rhapsody:@alice".into()]);
        let mut bob = issue("2", "MT-2", "Todo");
        bob.labels = Some(vec!["rhapsody:@bob".into()]);
        assert_eq!(
            o.select_dispatch(vec![alice, bob])
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            vec!["2"]
        );
    }

    #[test]
    fn a_limit_observed_during_review_preparation_leaves_no_watch_side_effect() {
        let (mut o, st, _, _dir) = setup(false);
        let review = crate::review::ReviewRun {
            owner: "owner".into(),
            repo: "repo".into(),
            number: 1,
            reviewer: "alice".into(),
            head_sha: "head".into(),
            ..Default::default()
        };
        let key = review.watch_key();
        let issue = review.synthetic_issue();
        o.accounts
            .observe("claude-subscription", observation(0.91, 5000));
        o.finish_review_dispatch_prepared(
            review,
            crate::retry::DispatchRoute {
                slug: String::new(),
                group: String::new(),
                repo: String::new(),
                model: String::new(),
                workspace_mode: String::new(),
            },
            issue,
            None,
        );
        assert!(o.running.is_empty());
        assert!(st.get_review_watch(&key).unwrap().is_none());
        assert!(o.review_attempts.is_empty());
    }

    #[test]
    fn an_isolated_manager_limit_is_a_human_item_not_an_unrestorable_session() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        let mut run = o.running.remove("1").unwrap();
        run.issue.id = "pr:owner/repo#1@manager".into();
        run.issue.identifier = run.issue.id.clone();
        run.identity.clear();
        o.running.insert(run.issue.id.clone(), run);
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        assert!(o.running.is_empty());
        assert!(matches!(
            o.limit_policy.suspended["pr:owner/repo#1@manager"].outcome,
            HandoffOutcome::ManagerItem(_)
        ));
        assert_eq!(o.limit_policy.items.len(), 1);
    }

    #[test]
    fn levels_from_tightest_window() {
        for (pct, want) in [
            (0.79, Level::Ok),
            (0.80, Level::Warn),
            (0.90, Level::StopNew),
            (0.95, Level::Handoff),
            (1.0, Level::Wall),
        ] {
            let ledger = AccountLedger::default();
            let mut obs = observation(pct, 5000);
            obs.windows.push(WindowObs {
                window: "seven_day".into(),
                utilization: 0.2,
                resets_at_s: 9000,
            });
            ledger.observe("a", obs);
            assert_eq!(level(&ledger, "a", &Limits::default(), 1100), want);
        }
    }

    #[test]
    fn per_account_threshold_override() {
        let ledger = AccountLedger::default();
        ledger.observe("a", observation(0.85, 5000));
        let cfg: Limits =
            serde_yaml_ng::from_str("accounts: {a: {thresholds: {stop_new: 85}}}").unwrap();
        assert_eq!(level(&ledger, "a", &cfg, 1100), Level::StopNew);
    }

    #[test]
    fn stop_new_holds_with_reason() {
        let (mut o, _, _, _dir) = setup(false);
        o.accounts
            .observe("claude-subscription", observation(0.91, 5000));
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        o.dispatch_issue(iss, None, None, String::new());
        assert!(o.running.is_empty());
        assert!(
            o.limit_holds()
                .iter()
                .any(|s| s.contains("claude-subscription limit") && s.contains("5000"))
        );
    }

    #[test]
    fn stale_high_with_future_reset_still_holds() {
        let (mut o, _, clock, _dir) = setup(false);
        o.accounts
            .observe("claude-subscription", observation(0.91, 10000));
        clock.store(3000, Ordering::SeqCst);
        assert!(o.accounts.snapshot(3000)[0].stale);
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        o.dispatch_issue(iss, None, None, String::new());
        assert!(o.running.is_empty());
    }

    #[test]
    fn reviewer_routed_to_healthy_account() {
        let (mut o, _, _, dir) = setup(false);
        std::fs::write(
            dir.child("bob.md"),
            "---\nharness: opencode\nmodel: fireworks-ai/model\n---\nReviewer\n",
        )
        .unwrap();
        o.teams
            .as_mut()
            .unwrap()
            .roster
            .push(rhapsody_config::teams::Identity {
                name: "bob".into(),
                profile: "bob".into(),
                ..Default::default()
            });
        o.teams.as_mut().unwrap().quorum.reviewers = 1;
        o.accounts
            .observe("claude-subscription", observation(0.91, 5000));
        let teams = o.teams.as_ref().unwrap();
        assert_eq!(
            crate::quorum::select_reviewers(
                teams,
                "author",
                &HashMap::new(),
                &o.reviewer_exclusions(teams)
            ),
            vec!["bob"]
        );
    }

    #[test]
    fn implementer_dispatches_on_healthy_fallback() {
        let (mut o, _, _, _dir) = setup(true);
        o.accounts
            .observe("claude-subscription", observation(0.91, 5000));
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        o.dispatch_issue(iss, None, None, String::new());
        assert_eq!(o.running["1"].engine_index, 1);
        assert_eq!(o.running["1"].identity, "alice");
    }

    #[test]
    fn handoff_sends_message_via_mailbox() {
        let (mut o, st, _, _dir) = setup(false);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(0.96, 2500));
        o.enforce_limits();
        let msg = o.mailbox_try_recv("1").unwrap();
        assert!(msg.contains("Your account `claude-subscription` is at `96`%"));
        assert!(msg.contains("MT-1-progress.md") && msg.contains(HANDOFF_LIMIT_MARKER));
        assert_eq!(
            st.list_run_messages(o.running["1"].run_id).unwrap().len(),
            1
        );
        o.enforce_limits();
        assert!(o.mailbox_try_recv("1").is_none());
    }

    #[test]
    fn handoff_limit_marker_ends_run_cleanly() {
        let (mut o, st, _, _dir) = setup(false);
        dispatch(&mut o);
        let at = o.running["1"].started_at;
        o.accounts
            .observe("claude-subscription", observation(0.96, 2500));
        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: "1".into(),
            started_at: at,
            last_state: HANDOFF_LIMIT_MARKER.into(),
            failed: false,
            err_msg: String::new(),
            declared_handoff: true,
            review_verdict: None,
            manager_text: None,
            refused: false,
            auth_needed: false,
        });
        assert_eq!(
            st.issue_history("MT-1", "", 10).unwrap()[0].outcome,
            "limit"
        );
        assert!(o.limit_policy.suspended.contains_key("1"));
        assert!(o.retry_attempts.is_empty());
    }

    #[test]
    fn grace_expired_daemon_stops_and_writes_note() {
        let (mut o, st, clock, dir) = setup(false);
        dispatch(&mut o);
        std::fs::write(
            dir.child("MT-1-progress.md"),
            "Committed work; next: finish tests.",
        )
        .unwrap();
        o.accounts
            .observe("claude-subscription", observation(0.96, 2500));
        o.enforce_limits();
        clock.store(1600, Ordering::SeqCst);
        o.enforce_limits();
        assert!(o.running.is_empty());
        assert_eq!(
            st.issue_history("MT-1", "", 10).unwrap()[0].outcome,
            "limit"
        );
        let text =
            std::fs::read_to_string(o.limit_policy.suspended["1"].note.as_ref().unwrap()).unwrap();
        assert!(text.contains("Committed work") && text.contains("next: finish tests"));
    }

    #[test]
    fn wall_without_warning_daemon_writes_note() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        let mut wall = observation(1.0, 2500);
        wall.status = LimitStatus::Rejected;
        o.accounts.observe("claude-subscription", wall);
        o.enforce_limits();
        assert!(o.running.is_empty());
        assert!(
            o.limit_policy.suspended["1"]
                .note
                .as_ref()
                .unwrap()
                .exists()
        );
    }

    #[tokio::test]
    async fn park_when_reset_within_wait_max() {
        let (mut o, _, clock, _dir) = setup(false);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 2500));
        o.enforce_limits();
        assert_eq!(
            o.limit_policy.suspended["1"].outcome,
            HandoffOutcome::Park { resume_at_s: 2620 }
        );
        clock.store(2619, Ordering::SeqCst);
        o.resume_limit("1").await;
        assert!(o.running.is_empty());
        clock.store(2620, Ordering::SeqCst);
        o.resume_limit("1").await;
        assert_eq!(o.running["1"].resume_session, "session-before-limit");
        assert_eq!(o.running["1"].retry_attempt, 2);
    }

    #[test]
    fn seven_day_skips_park() {
        let ledger = AccountLedger::default();
        let mut obs = observation(0.96, 1200);
        obs.windows[0].window = "seven_day".into();
        ledger.observe("claude-subscription", obs);
        assert_eq!(
            decide(&ledger, item(), &Limits::default(), 1000, Some(1)),
            HandoffOutcome::Switch { engine: 1 }
        );
    }

    #[test]
    fn switch_when_far_and_fallback_healthy() {
        let ledger = AccountLedger::default();
        ledger.observe("claude-subscription", observation(0.96, 9000));
        assert_eq!(
            decide(&ledger, item(), &Limits::default(), 1000, Some(1)),
            HandoffOutcome::Switch { engine: 1 }
        );
    }

    #[test]
    fn manager_item_otherwise() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 9000));
        o.enforce_limits();
        assert_eq!(o.limit_policy.items.len(), 1);
        assert_eq!(o.limit_policy.items[0].tickets[0].ticket, "MT-1");
        let value = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(value["limit_items"][0]["account"], "claude-subscription");
    }

    #[test]
    fn both_engines_limited_waits_or_goes_to_manager() {
        let (mut o, _, _, _dir) = setup(true);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 9000));
        o.accounts.observe("fireworks-ai", observation(0.91, 9000));
        o.enforce_limits();
        assert!(matches!(
            o.limit_policy.suspended["1"].outcome,
            HandoffOutcome::ManagerItem(_)
        ));
        assert!(o.running.is_empty());
    }

    #[tokio::test]
    async fn parked_resume_rechecks_eligibility() {
        for change in ["closed", "moved", "relabeled"] {
            let (mut o, _, clock, _dir) = setup(false);
            dispatch(&mut o);
            o.accounts
                .observe("claude-subscription", observation(1.0, 1200));
            o.enforce_limits();
            let mut tr = Fake::new();
            let mut iss = issue(
                "1",
                "MT-1",
                if change == "closed" { "Done" } else { "Todo" },
            );
            iss.labels = Some(vec![
                if change == "relabeled" {
                    "rhapsody:@bob"
                } else {
                    "rhapsody:@alice"
                }
                .into(),
            ]);
            if change != "moved" {
                tr.candidates = vec![iss];
            }
            o.eff.as_mut().unwrap().tracker = Arc::new(tr);
            clock.store(1320, Ordering::SeqCst);
            o.resume_limit("1").await;
            assert!(o.running.is_empty(), "{change}");
            assert!(!o.claimed.contains("1"), "{change}");
            assert!(!o.limit_policy.suspended.contains_key("1"), "{change}");
        }
    }

    #[test]
    fn credits_never_stops_on_overage() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        let mut obs = observation(0.25, 2500);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.enforce_limits();
        assert!(o.running.is_empty());
        assert_eq!(
            level(&o.accounts, "claude-subscription", &Limits::default(), 1001),
            Level::Wall
        );
    }

    #[test]
    fn credits_daily_cap_then_never() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        o.eff.as_mut().unwrap().cfg.limits.credits = "daily_cap".into();
        o.eff.as_mut().unwrap().cfg.limits.credits_daily_usd = 1.0;
        let mut obs = observation(1.0, 2500);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.enforce_limits();
        assert!(o.running.contains_key("1"));
        for (turn, cost) in [(1, 0.6), (2, 0.6)] {
            o.on_agent_update(crate::AgentUpdate {
                issue_id: "1".into(),
                ev: rhapsody_agent::Event {
                    event_type: rhapsody_agent::EVENT_SESSION_STARTED.into(),
                    ..Default::default()
                },
            });
            o.on_agent_update(crate::AgentUpdate {
                issue_id: "1".into(),
                ev: rhapsody_agent::Event {
                    event_type: rhapsody_agent::EVENT_TURN_COMPLETED.into(),
                    turn,
                    cost_usd: Some(cost),
                    usage: Some(Default::default()),
                    ..Default::default()
                },
            });
        }
        o.enforce_limits();
        assert!(o.running.is_empty());
    }

    #[test]
    fn credits_always_runs() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        o.eff.as_mut().unwrap().cfg.limits.credits = "always".into();
        let mut obs = observation(1.0, 2500);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.enforce_limits();
        assert!(o.running.contains_key("1"));
        assert!(o.limit_policy.deadlines.is_empty());
        // A positive credit-spend event is still required even when spending is allowed.
        assert_eq!(o.limit_policy.credit_notified.len(), 1);
    }

    #[test]
    fn credit_spend_notifies_once_per_day() {
        let (mut o, st, clock, _dir) = setup(false);
        dispatch(&mut o);
        o.eff.as_mut().unwrap().cfg.limits.credits = "always".into();
        let run = o.running["1"].run_id;
        let mut obs = observation(0.5, 200000);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.enforce_limits();
        o.enforce_limits();
        let first = st
            .run_events(run)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "limit.credit_spend")
            .count();
        assert_eq!(first, 1);
        clock.store(100000, Ordering::SeqCst);
        o.enforce_limits();
        assert_eq!(
            st.run_events(run)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "limit.credit_spend")
                .count(),
            2
        );
    }

    #[test]
    fn limit_stop_counts_against_nothing() {
        let (mut o, st, _, _dir) = setup(false);
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        assert!(o.retry_attempts.is_empty());
        assert!(o.review_rounds.is_empty());
        assert!(o.escalation_notified.is_empty());
        assert_eq!(o.limit_policy.suspended["1"].run.retry_attempt, 2);
        assert_eq!(
            st.issue_history("MT-1", "", 10).unwrap()[0].outcome,
            "limit"
        );
        assert_eq!(
            st.count_completed_review_runs("owner", "repo", 1).unwrap(),
            0
        );
    }

    #[test]
    fn budget_windows_feed_ledger() {
        let (mut o, st, _, _dir) = setup(false);
        o.eff.as_mut().unwrap().cfg.budgets.insert(
            "fireworks-ai".into(),
            rhapsody_config::ProviderBudget {
                daily_usd: 10.0,
                daily_tokens: 100,
                per_ticket: 0,
            },
        );
        let run = st
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "OTHER-1".into(),
                ..Default::default()
            })
            .unwrap();
        st.set_turn_spend(
            run,
            &rhapsody_store::TurnSpend {
                turn: 1,
                at: chrono::DateTime::from_timestamp(900, 0)
                    .unwrap()
                    .to_rfc3339(),
                provider: "fireworks-ai".into(),
                account: "fireworks-ai".into(),
                model: "fireworks-ai/model".into(),
                usd: Some(9.3),
                source: "harness_reported".into(),
                harness_priced: true,
            },
        )
        .unwrap();
        o.feed_budget_windows();
        assert_eq!(
            level(&o.accounts, "fireworks-ai", &Limits::default(), 1000),
            Level::StopNew
        );
        assert_eq!(o.accounts.snapshot(1000)[0].source, "budget");
        assert!(
            o.accounts
                .tightest("fireworks-ai", 1000)
                .unwrap()
                .resets_at_s
                > 1000
        );
    }

    #[test]
    fn credit_stop_latches_rejected_until_reset() {
        let (mut o, _, _, _dir) = setup(false);
        dispatch(&mut o);
        let mut obs = observation(0.1, 2500);
        obs.using_credits = true;
        o.accounts.observe("claude-subscription", obs);
        o.enforce_limits();
        o.accounts
            .observe("claude-subscription", observation(0.1, 2500));
        assert_eq!(
            level(&o.accounts, "claude-subscription", &Limits::default(), 1100),
            Level::Wall
        );
        assert_eq!(
            level(&o.accounts, "claude-subscription", &Limits::default(), 2500),
            Level::Ok
        );
    }

    #[tokio::test]
    async fn held_retry_keeps_its_attempt_and_rechecks_after_reset() {
        let (mut o, _, clock, _dir) = setup(false);
        dispatch(&mut o);
        let re = o.running["1"].clone();
        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: "1".into(),
            started_at: re.started_at,
            failed: true,
            err_msg: "ordinary failure".into(),
            last_state: "Todo".into(),
            declared_handoff: false,
            review_verdict: None,
            manager_text: None,
            refused: false,
            auth_needed: false,
        });
        let before = o.retry_attempts["1"].attempt;
        o.accounts
            .observe("claude-subscription", observation(0.91, 1500));
        o.on_retry(crate::EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(o.running.is_empty());
        assert_eq!(o.retry_attempts["1"].attempt, before);
        clock.store(1620, Ordering::SeqCst);
        o.on_retry(crate::EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert_eq!(o.running["1"].retry_attempt, before);
    }

    #[tokio::test]
    async fn a_fallback_continuation_does_not_go_back_to_primary() {
        let (mut o, _, clock, _dir) = setup(true);
        o.accounts
            .observe("claude-subscription", observation(0.91, 1100));
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        o.dispatch_issue(iss, None, None, String::new());
        let re = o.running["1"].clone();
        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: "1".into(),
            started_at: re.started_at,
            failed: false,
            err_msg: String::new(),
            last_state: "Todo".into(),
            declared_handoff: false,
            review_verdict: None,
            manager_text: None,
            refused: false,
            auth_needed: false,
        });
        clock.store(1200, Ordering::SeqCst);
        o.on_retry(crate::EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert_eq!(o.running["1"].engine_index, 1);
    }

    #[tokio::test]
    async fn parked_pool_issue_rechecks_the_assigned_issue_in_its_project() {
        let (mut o, _, clock, _dir) = setup(false);
        o.eff.as_mut().unwrap().claim_mode = "pool".into();
        dispatch(&mut o);
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        let mut tr = Fake::new();
        let mut iss = issue("1", "MT-1", "Todo");
        iss.labels = Some(vec!["rhapsody:@alice".into()]);
        tr.by_id.insert("1".into(), iss);
        o.eff.as_mut().unwrap().tracker = Arc::new(tr);
        clock.store(1320, Ordering::SeqCst);
        o.resume_limit("1").await;
        assert_eq!(o.running["1"].resume_session, "session-before-limit");
    }

    #[tokio::test]
    async fn parked_review_resumes_without_charging_a_failed_attempt_or_verdict() {
        let (mut o, st, clock, _dir) = setup(false);
        dispatch(&mut o);
        let review = crate::review::ReviewRun {
            owner: "owner".into(),
            repo: "repo".into(),
            number: 1,
            reviewer: "alice".into(),
            head_sha: "old-head".into(),
            ..Default::default()
        };
        st.save_review_watch(rhapsody_store::ReviewWatchRow {
            key: review.watch_key(),
            status: rhapsody_store::REVIEW_STATUS_IN_FLIGHT.into(),
            open: true,
            requested_sha: "old-head".into(),
            ..Default::default()
        })
        .unwrap();
        o.running.get_mut("1").unwrap().review = Some(review);
        o.accounts
            .observe("claude-subscription", observation(1.0, 1200));
        o.enforce_limits();
        clock.store(1320, Ordering::SeqCst);
        o.eff.as_mut().unwrap().tracker = Arc::new(Fake::new());
        o.resume_limit("1").await;
        assert_eq!(o.running["1"].resume_session, "session-before-limit");
        assert!(o.running["1"].review.is_some());
        assert!(o.review_attempts.is_empty());
        assert!(o.review_rounds.is_empty());
        assert_eq!(
            st.count_completed_review_runs("owner", "repo", 1).unwrap(),
            0
        );
    }
}
