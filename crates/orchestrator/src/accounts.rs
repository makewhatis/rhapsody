//! Per-account limit ledger (STUDIO-1123). Rhapsody-only; no Go counterpart.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rhapsody_agent::ratelimit::{LimitObs, LimitStatus};
use serde::Serialize;

pub fn account_for(harness: &str, model: &str, oauth: bool) -> String {
    match harness {
        "claude" if oauth => "claude-subscription".into(),
        "claude" => "anthropic".into(),
        "opencode" => match model.split_once('/') {
            Some(("openai", _)) if oauth => "chatgpt-subscription".into(),
            Some((provider, _)) => provider.into(),
            None => String::new(),
        },
        _ => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct WindowView {
    pub window: String,
    pub utilization: f64,
    pub resets_at_s: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct AccountView {
    pub account: String,
    pub windows: Vec<WindowView>,
    pub status: String,
    pub using_credits: bool,
    pub last_seen_s: i64,
    pub source: String,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    pub detection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub today_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_kind: Option<String>,
}

#[derive(Default)]
pub struct AccountLedger {
    state: Mutex<LedgerState>,
}

#[derive(Default)]
struct LedgerState {
    accounts: BTreeMap<String, AccountState>,
    runs: BTreeMap<String, String>,
}

#[derive(Default)]
struct AccountState {
    windows: BTreeMap<String, WindowState>,
    last_seen_s: i64,
    source: String,
    probe_failure: Option<String>,
}

struct WindowState {
    view: WindowView,
    status: LimitStatus,
    using_credits: bool,
    source: String,
}

fn expired(window: &WindowView, now_s: i64) -> bool {
    window.resets_at_s > 0 && window.resets_at_s <= now_s
}

fn severity(status: LimitStatus) -> u8 {
    match status {
        LimitStatus::Allowed => 0,
        LimitStatus::Warning => 1,
        LimitStatus::Rejected => 2,
    }
}

impl AccountLedger {
    pub fn observe(&self, account: &str, obs: LimitObs) {
        if account.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = obs
            .windows
            .iter()
            .filter(|w| {
                !w.window.is_empty()
                    && w.resets_at_s >= 0
                    && w.utilization.is_finite()
                    && (0.0..=1.0).contains(&w.utilization)
            })
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
            .map(|w| w.window.clone());
        if target.is_none() {
            return;
        }
        let account = state.accounts.entry(account.into()).or_default();
        let mut accepted = false;
        for window in obs.windows {
            if window.window.is_empty()
                || !window.utilization.is_finite()
                || !(0.0..=1.0).contains(&window.utilization)
                || window.resets_at_s < 0
            {
                continue;
            }
            let status = if target.as_deref() == Some(&window.window) {
                obs.status
            } else {
                LimitStatus::Allowed
            };
            let using_credits = target.as_deref() == Some(&window.window) && obs.using_credits;
            match account.windows.get_mut(&window.window) {
                Some(old)
                    if window.resets_at_s > 0 && old.view.resets_at_s > window.resets_at_s =>
                {
                    continue;
                }
                Some(old)
                    if window.resets_at_s == 0
                        && expired(&old.view, obs.observed_at_s)
                        && obs.observed_at_s >= account.last_seen_s =>
                {
                    old.view.utilization = window.utilization;
                    old.view.resets_at_s = 0;
                    old.status = status;
                    old.using_credits = using_credits;
                    old.source = obs.source.into();
                }
                Some(old)
                    if window.resets_at_s == 0 || old.view.resets_at_s == window.resets_at_s =>
                {
                    old.view.utilization = old.view.utilization.max(window.utilization);
                    if severity(status) > severity(old.status) {
                        old.status = status;
                        old.source = obs.source.into();
                    }
                    if obs.observed_at_s >= account.last_seen_s {
                        old.using_credits = using_credits;
                    }
                }
                _ => {
                    account.windows.insert(
                        window.window.clone(),
                        WindowState {
                            view: WindowView {
                                window: window.window,
                                utilization: window.utilization,
                                resets_at_s: window.resets_at_s,
                            },
                            status,
                            using_credits,
                            source: obs.source.into(),
                        },
                    );
                }
            }
            accepted = true;
        }
        if accepted && obs.observed_at_s >= account.last_seen_s {
            account.last_seen_s = obs.observed_at_s;
            account.source = obs.source.into();
            if obs.source == "probe" {
                account.probe_failure = None;
            }
        }
    }

    pub fn snapshot(&self, now_s: i64) -> Vec<AccountView> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .accounts
            .iter()
            .map(|(name, account)| {
                let active = state.runs.values().any(|run_account| run_account == name);
                let mut status = LimitStatus::Allowed;
                let mut using_credits = false;
                let windows = account
                    .windows
                    .values()
                    .map(|w| {
                        let mut view = w.view.clone();
                        if expired(&view, now_s) {
                            view.utilization = 0.0;
                        } else {
                            if severity(w.status) > severity(status) {
                                status = w.status;
                            }
                            using_credits |= w.using_credits;
                        }
                        view
                    })
                    .collect();
                AccountView {
                    level: None,
                    today_usd: None,
                    cost_kind: None,
                    account: name.clone(),
                    windows,
                    status: if account.windows.is_empty() {
                        "unknown"
                    } else {
                        match status {
                            LimitStatus::Allowed => "allowed",
                            LimitStatus::Warning => "warning",
                            LimitStatus::Rejected => "rejected",
                        }
                    }
                    .into(),
                    using_credits,
                    last_seen_s: account.last_seen_s,
                    source: account.source.clone(),
                    stale: account.probe_failure.is_some()
                        || ((!active || name == "chatgpt-subscription")
                            && (account.windows.is_empty()
                                || now_s.saturating_sub(account.last_seen_s) >= 1800)),
                    stale_reason: account.probe_failure.clone(),
                    // The passive WHAM probe was measured successfully. The detection capability
                    // stays probe even when the latest observation is a stream rejection.
                    detection: if name == "chatgpt-subscription" {
                        "probe"
                    } else if name == "claude-subscription" {
                        "stream"
                    } else if account.source == "probe" {
                        "probe"
                    } else {
                        "budget"
                    }
                    .into(),
                }
            })
            .collect()
    }

    pub fn tightest(&self, account: &str, now_s: i64) -> Option<WindowView> {
        self.snapshot(now_s)
            .into_iter()
            .find(|view| view.account == account)?
            .windows
            .into_iter()
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }

    pub fn probe_failed(&self, reason: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let account = state
            .accounts
            .entry("chatgpt-subscription".into())
            .or_default();
        account.probe_failure = Some(reason.into());
        if account.source.is_empty() {
            account.source = "probe".into();
        }
    }

    /// Idempotent per-run activity binding; no lock is held across I/O or awaits.
    pub fn bind_run(&self, run: &str, account: &str) {
        if run.is_empty() || account.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accounts.entry(account.into()).or_default();
        state.runs.insert(run.into(), account.into());
    }

    pub fn release_run(&self, run: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs
            .remove(run);
    }

    pub fn account_for_run(&self, run: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs
            .get(run)
            .cloned()
    }

    /// Operator-owned budget windows are replaced when their cap changes; stream/probe windows
    /// keep L1's monotonic same-window rule and are never cleared by a workflow edit.
    pub fn forget_budget(&self, account: &str) {
        if let Some(account) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accounts
            .get_mut(account)
        {
            account.windows.retain(|_, w| w.source != "budget");
        }
    }

    /// A forbidden credit observation holds the account until this window's reset, even if a
    /// later stream line stops reporting overage. Same-window status monotonicity owns the latch.
    pub fn reject_until_reset(&self, account: &str, now_s: i64) {
        if let Some(window) = self.tightest(account, now_s) {
            self.observe(
                account,
                LimitObs {
                    status: LimitStatus::Rejected,
                    windows: vec![rhapsody_agent::ratelimit::WindowObs {
                        window: window.window,
                        utilization: window.utilization,
                        resets_at_s: window.resets_at_s,
                    }],
                    using_credits: false,
                    source: "stream",
                    observed_at_s: now_s,
                },
            );
        }
    }

    fn observe_run(&self, run: &str, obs: LimitObs) {
        let account = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runs
            .get(run)
            .cloned();
        if let Some(account) = account {
            self.observe(&account, obs);
        }
    }
}

pub(crate) fn run_key(issue_id: &str, started_at: chrono::DateTime<chrono::Utc>) -> String {
    format!("{issue_id}:{}", started_at.to_rfc3339())
}

impl crate::Orchestrator {
    /// Dispatch gates must not hide work that arrived while they were closed. This read-only
    /// pass observes the queue without selecting, claiming, enriching or priming decision ledgers.
    /// Successful dispatch passes already observe their candidate set and need no extra reads.
    pub(crate) async fn refresh_gated_chatgpt_queue(&mut self) {
        let Some(eff) = self.eff.as_ref() else {
            return;
        };
        let trackers: Vec<_> = if eff.projects.is_empty() {
            vec![(String::new(), eff.tracker.clone())]
        } else {
            eff.projects
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| (p.slug.clone(), p.tracker.clone()))
                .collect()
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut complete = !trackers.is_empty();
        let mut queued = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (project, tracker) in trackers {
            match tokio::time::timeout_at(deadline, tracker.fetch_candidate_issues()).await {
                Ok(Ok(issues)) => {
                    queued.extend(
                        issues
                            .into_iter()
                            .filter(|i| seen.insert(i.id.clone()))
                            .map(|i| (i, project.clone())),
                    );
                }
                Ok(Err(error)) => {
                    complete = false;
                    tracing::warn!(%project, %error, "limit: gated queue observation failed");
                }
                Err(_) => {
                    complete = false;
                    tracing::warn!("limit: gated queue observation timed out");
                    break;
                }
            }
        }
        self.record_chatgpt_queue(
            queued
                .iter()
                .map(|(issue, project)| (issue, project.as_str())),
            complete,
        );
    }

    pub(crate) fn chatgpt_probe_path(&self, boot: bool) -> Option<std::path::PathBuf> {
        let openai_run = |run: &crate::RunningEntry| {
            let (harness, model) =
                self.resolved_harness_model(&run.harness, &run.model_override, &run.project_slug);
            harness == "opencode" && model.starts_with("openai/")
        };
        if !boot
            && !self.chatgpt_queued
            && !self.running.values().any(openai_run)
            && !self
                .limit_policy
                .suspended
                .values()
                .any(|s| openai_run(&s.run))
        {
            return None;
        }
        let source = &self.eff.as_ref()?.cfg.opencode.auth_source;
        Some(if source.is_empty() {
            rhapsody_agent::opencode::state::default_auth_source()
        } else {
            source.into()
        })
    }

    /// Called before selection discards candidates held by capacity, labels, budgets or limits.
    pub(crate) fn record_chatgpt_queue<'a>(
        &mut self,
        mut issues: impl Iterator<Item = (&'a rhapsody_core::Issue, &'a str)>,
        complete: bool,
    ) {
        let queued = issues.any(|(issue, project)| {
            let run = self.limit_projection(issue, project);
            let (harness, model) =
                self.resolved_harness_model(&run.harness, &run.model_override, project);
            harness == "opencode" && model.starts_with("openai/")
        });
        if complete || queued {
            self.chatgpt_queued = queued;
        }
    }
    pub(crate) fn bind_account(
        &mut self,
        issue_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        account: &str,
    ) {
        if self
            .running
            .get(issue_id)
            .is_some_and(|re| re.started_at == started_at)
        {
            if !account.is_empty()
                && let Some(re) = self.running.get_mut(issue_id)
            {
                re.pricing.account = account.into();
            }
            self.accounts
                .bind_run(&run_key(issue_id, started_at), account);
        }
    }

    pub(crate) fn observe_account(
        &mut self,
        issue_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        obs: LimitObs,
    ) {
        if self
            .running
            .get(issue_id)
            .is_some_and(|re| re.started_at == started_at)
        {
            self.accounts
                .observe_run(&run_key(issue_id, started_at), obs);
            self.enforce_limits();
        }
    }
}

impl crate::ControlHandle {
    pub async fn chatgpt_probe_path(&self, boot: bool) -> Option<std::path::PathBuf> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(crate::control_loop::Event::ChatgptProbePath { boot, reply })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub fn record_chatgpt_probe(&self, result: Result<LimitObs, &'static str>) {
        let _ = self
            .events
            .send(crate::control_loop::Event::ChatgptProbeResult(result));
    }
    /// Read-only ledger plus today's stored costs and configured levels. No credential reads,
    /// network or control round-trip; raw policy snapshots never acquire reporting fields.
    pub fn accounts(&self, now_s: i64) -> Vec<AccountView> {
        let cfg = self
            .reads
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limits
            .clone();
        let since = chrono::DateTime::from_timestamp(now_s, 0)
            .map(crate::budget::local_day_start_at)
            .unwrap_or_default();
        let costs = self.store.turn_spend_since(&since);
        self.accounts
            .snapshot(now_s)
            .into_iter()
            .map(|mut view| {
                view.level = Some(
                    crate::limitreport::level_label(crate::limitpolicy::level(
                        &self.accounts,
                        &view.account,
                        &cfg,
                        now_s,
                    ))
                    .into(),
                );
                view.cost_kind = Some(
                    if matches!(
                        view.account.as_str(),
                        "claude-subscription" | "chatgpt-subscription"
                    ) {
                        "api_equivalent"
                    } else {
                        "usd"
                    }
                    .into(),
                );
                if self.store.usd_accounting_available()
                    && let Ok(rows) = &costs
                {
                    let mut total = Some(0.0);
                    for row in rows.iter().filter(|r| r.account == view.account) {
                        total = total
                            .zip(row.usd.filter(|v| v.is_finite() && *v >= 0.0))
                            .map(|(a, b)| a + b);
                    }
                    view.today_usd = total.filter(|v| v.is_finite());
                }
                view
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_agent::ratelimit::{LimitStatus, WindowObs};

    #[test]
    fn probe_failure_stays_stale_during_active_work_until_success() {
        let ledger = AccountLedger::default();
        let mut usage = obs(0.31, 5000, 1000);
        usage.source = "probe";
        ledger.observe("chatgpt-subscription", usage.clone());
        ledger.bind_run("active", "chatgpt-subscription");
        ledger.probe_failed("usage_unauthorized");
        let view = &ledger.snapshot(1100)[0];
        assert!(view.stale);
        assert_eq!(view.stale_reason.as_deref(), Some("usage_unauthorized"));
        assert_eq!(view.windows[0].utilization, 0.31);
        ledger.observe("chatgpt-subscription", obs(0.5, 5000, 1200));
        assert!(
            ledger.snapshot(1200)[0].stale,
            "stream traffic cannot repair a failed probe"
        );
        usage.observed_at_s = 1300;
        ledger.observe("chatgpt-subscription", usage);
        assert!(!ledger.snapshot(1300)[0].stale);
        assert!(ledger.snapshot(1300)[0].stale_reason.is_none());
        assert!(
            ledger.snapshot(3100)[0].stale,
            "live work cannot keep probe data fresh forever"
        );
        let empty = AccountLedger::default();
        empty.probe_failed("usage_timeout");
        assert_eq!(empty.snapshot(1000)[0].status, "unknown");
        assert!(empty.snapshot(1000)[0].stale);
    }

    #[test]
    fn queued_and_held_openai_work_keeps_probe_admitted_without_a_runner() {
        let mut o = crate::Orchestrator::new("not-read.md");
        let mut eff = crate::testsupport::empty_effective(std::sync::Arc::new(
            rhapsody_tracker::fake::Fake::default(),
        ));
        eff.cfg.agent.backend = "opencode".into();
        eff.cfg.opencode.model = "openai/test".into();
        eff.cfg.opencode.auth_source = "/test/operator/auth.json".into();
        o.eff = Some(eff);
        assert!(o.chatgpt_probe_path(false).is_none());
        assert!(o.chatgpt_probe_path(true).is_some());
        let issue = rhapsody_core::Issue {
            id: "queued".into(),
            labels: Some(vec!["rhapsody:human".into()]),
            ..Default::default()
        };
        o.record_chatgpt_queue(std::iter::once((&issue, "")), true);
        assert_eq!(
            o.chatgpt_probe_path(false),
            Some("/test/operator/auth.json".into())
        );
        o.record_chatgpt_queue(std::iter::empty(), false);
        assert!(
            o.chatgpt_probe_path(false).is_some(),
            "failed board reads cannot erase queued work"
        );
        o.record_chatgpt_queue(std::iter::empty(), true);
        assert!(o.chatgpt_probe_path(false).is_none());
    }

    #[test]
    fn plan_thresholds_and_operator_budget_wall_remain_separate() {
        use crate::limitpolicy::{Level, level};
        let ledger = AccountLedger::default();
        let mut plan = obs(0.91, 5000, 1000);
        plan.source = "probe";
        ledger.observe("chatgpt-subscription", plan.clone());
        let mut budget = obs(0.96, 5000, 1000);
        budget.source = "budget";
        budget.windows[0].window = "daily".into();
        ledger.observe("openai", budget.clone());
        let cfg = rhapsody_config::Limits::default();
        assert_eq!(
            level(&ledger, "chatgpt-subscription", &cfg, 1100),
            Level::StopNew
        );
        assert_eq!(level(&ledger, "openai", &cfg, 1100), Level::Ok);
        plan.windows[0].utilization = 0.96;
        ledger.observe("chatgpt-subscription", plan);
        assert_eq!(
            level(&ledger, "chatgpt-subscription", &cfg, 1100),
            Level::Handoff
        );
        budget.status = LimitStatus::Rejected;
        budget.windows[0].utilization = 1.0;
        ledger.observe("openai", budget);
        let view = ledger
            .snapshot(1100)
            .into_iter()
            .find(|a| a.account == "openai")
            .unwrap();
        assert_eq!(view.source, "budget");
        assert_eq!(view.detection, "budget");
        assert_eq!(
            level(&ledger, "chatgpt-subscription", &cfg, 1100),
            Level::Handoff
        );
        assert_eq!(level(&ledger, "openai", &cfg, 1100), Level::Wall);
        let mut o = crate::Orchestrator::new("not-read.md");
        o.now = Box::new(|| chrono::DateTime::from_timestamp(1100, 0).unwrap());
        o.accounts = std::sync::Arc::new(ledger);
        let mut healthy_plan = obs(0.31, 6000, 1100);
        healthy_plan.source = "probe";
        o.accounts.observe("chatgpt-subscription", healthy_plan);
        assert_eq!(
            level(&o.accounts, "chatgpt-subscription", &cfg, 1100),
            Level::Ok
        );
        assert!(!o.account_usable("chatgpt-subscription"));
        let mut eff = crate::testsupport::empty_effective(std::sync::Arc::new(
            rhapsody_tracker::fake::Fake::default(),
        ));
        eff.cfg.limits.credits = "manager_urgent".into();
        o.eff = Some(eff);
        o.limit_policy
            .credit_approvals
            .insert("ticket".into(), ("chatgpt-subscription".into(), 5000));
        assert!(
            !o.credit_approved("ticket", "chatgpt-subscription"),
            "credit permission cannot bypass the independent budget wall"
        );
    }

    #[tokio::test]
    async fn queued_openai_work_behind_dispatch_gates_keeps_probe_admitted() {
        use crate::testsupport::{empty_effective, empty_resolved_project, issue};
        use rhapsody_tracker::fake::Fake;
        use std::sync::Arc;

        struct DeadCredential;
        #[async_trait::async_trait]
        impl crate::preflight::CredentialProbe for DeadCredential {
            async fn probe(
                &self,
                _: &crate::preflight::ProbeRequest,
            ) -> crate::preflight::ProbeOutcome {
                crate::preflight::ProbeOutcome::Dead("test login unavailable".into())
            }
        }

        for gate in ["validation", "drain", "credential"] {
            for projects in [false, true] {
                let mut candidate = issue("queued", "MT-1", "Todo");
                candidate.labels = Some(vec![
                    "rhapsody:harness/opencode".into(),
                    "rhapsody:model/openai/test".into(),
                    "rhapsody:human".into(),
                ]);
                let mut o = crate::Orchestrator::new("not-read.md");
                let mut eff = empty_effective(Arc::new(Fake::new()));
                eff.poll_interval = std::time::Duration::from_secs(3600);
                eff.cfg.opencode.auth_source = "/test/operator/auth.json".into();
                if projects {
                    eff.projects = vec![empty_resolved_project("test", eff.tracker.clone())];
                }
                o.eff = Some(eff);
                match gate {
                    "validation" => o.eff.as_mut().unwrap().cfg.tracker.api_key.clear(),
                    "drain" => {
                        o.drain
                            .arm(chrono::Utc::now(), crate::drain::DrainReason::Operator);
                    }
                    _ => o.set_credential_probe(Arc::new(DeadCredential)),
                }
                // Work arrives after boot while the dispatch gate remains closed.
                for (queued, failed) in
                    [(false, false), (true, false), (false, true), (false, false)]
                {
                    let mut tracker = Fake::new();
                    if queued {
                        tracker.candidates = vec![candidate.clone()];
                    }
                    if failed {
                        tracker.candidates_err = Some(rhapsody_tracker::TrackerError::Other(
                            "test board unavailable".into(),
                        ));
                    }
                    let tracker = Arc::new(tracker);
                    let eff = o.eff.as_mut().unwrap();
                    if projects {
                        eff.projects[0].tracker = tracker;
                    } else {
                        eff.tracker = tracker;
                    }
                    o.on_tick().await;
                    if let Some(timer) = o.tick_timer.take() {
                        timer.abort();
                    }
                    assert_eq!(
                        o.chatgpt_probe_path(false).is_some(),
                        queued || failed,
                        "queue observation must survive {gate} (projects={projects}, queued={queued}, failed={failed})"
                    );
                    assert!(o.running.is_empty());
                    assert!(o.claimed.is_empty());
                    assert!(o.retry_attempts.is_empty());
                    assert!(o.preparing.is_empty());
                    assert!(!o.human_holds.labelled_and_primed().1);
                }
            }
        }
    }

    #[tokio::test]
    async fn gated_queue_observation_keeps_partial_reads_and_skips_disabled_projects() {
        use crate::testsupport::{empty_effective, empty_resolved_project, issue};
        use rhapsody_tracker::fake::Fake;
        use std::sync::Arc;

        let mut openai = Fake::new();
        openai.candidates = vec![issue("queued", "MT-1", "Todo")];
        let openai = Arc::new(openai);
        let empty = Arc::new(Fake::new());
        let mut failed = Fake::new();
        failed.candidates_err = Some(rhapsody_tracker::TrackerError::Other("unavailable".into()));
        let failed = Arc::new(failed);
        let mut eff = empty_effective(empty.clone());
        eff.cfg.agent.backend = "opencode".into();
        let mut project = empty_resolved_project("openai", openai.clone());
        project.mcfg.opencode.model = "openai/test".into();
        let mut disabled = empty_resolved_project("disabled", openai.clone());
        disabled.disabled = true;
        eff.projects = vec![project, empty_resolved_project("failed", failed), disabled];
        let mut o = crate::Orchestrator::new("not-read.md");
        o.eff = Some(eff);
        o.refresh_gated_chatgpt_queue().await;
        assert!(o.chatgpt_probe_path(false).is_some());
        assert_eq!(
            openai.candidate_calls(),
            1,
            "disabled project must not be read"
        );
        o.eff.as_mut().unwrap().projects[0].tracker = empty.clone();
        o.refresh_gated_chatgpt_queue().await;
        assert!(
            o.chatgpt_probe_path(false).is_some(),
            "a partial board cannot clear queued work"
        );
        o.eff.as_mut().unwrap().projects[1].tracker = empty;
        o.refresh_gated_chatgpt_queue().await;
        assert!(o.chatgpt_probe_path(false).is_none());
    }

    #[test]
    fn chatgpt_wall_and_probe_share_window_generations() {
        let ledger = AccountLedger::default();
        let bytes = include_bytes!("../../agent/testdata/limits/chatgpt-usage.json");
        let first = rhapsody_agent::opencode::limits::parse_usage(bytes, 1791384678).unwrap();
        ledger.observe("chatgpt-subscription", first);
        let wall = rhapsody_agent::ratelimit::parse_opencode_limit(&serde_json::json!({"type":"error", "timestamp":1791384700000i64, "error":{"data":{"statusCode":429}}})).unwrap();
        ledger.observe("chatgpt-subscription", wall);
        assert_eq!(ledger.snapshot(1791384700)[0].status, "rejected");
        // Boundary input derived from the measurement, not another live capture: a NEW reset
        // generation must clear the old wall instead of leaving an unmatchable generic window.
        let mut next: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        next["rate_limit"]["primary_window"]["used_percent"] = serde_json::json!(10);
        next["rate_limit"]["primary_window"]["reset_at"] = serde_json::json!(1792553415);
        ledger.observe(
            "chatgpt-subscription",
            rhapsody_agent::opencode::limits::parse_usage(
                &serde_json::to_vec(&next).unwrap(),
                1791948616,
            )
            .unwrap(),
        );
        let view = ledger.snapshot(1791948616);
        assert_eq!(view[0].status, "allowed");
        assert_eq!(view[0].windows.len(), 1);
        assert_eq!(view[0].windows[0].utilization, 0.10);
        assert_eq!(view[0].detection, "probe");
    }

    #[test]
    fn stale_run_bindings_and_limits_cannot_change_a_redispatched_account() {
        use crate::orchestrator::RunningEntry;
        let mut o = crate::Orchestrator::new("not-read.md");
        let old = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        let current = chrono::DateTime::from_timestamp(2000, 0).unwrap();
        let mut re = RunningEntry::empty(rhapsody_core::Issue {
            id: "ticket".into(),
            ..Default::default()
        });
        re.started_at = current;
        o.running.insert("ticket".into(), re);
        o.bind_account("ticket", current, "chatgpt-subscription");
        o.bind_account("ticket", old, "claude-subscription");
        o.observe_account("ticket", old, obs(1.0, 5000, 2200));
        assert_eq!(o.accounts.snapshot(2200).len(), 1);
        assert!(o.accounts.snapshot(2200)[0].windows.is_empty());
        o.observe_account("ticket", current, obs(0.31, 5000, 2200));
        assert_eq!(o.accounts.snapshot(2200)[0].windows[0].utilization, 0.31);
        o.running.remove("ticket");
        o.accounts.release_run(&run_key("ticket", current));
        o.bind_account("ticket", current, "claude-subscription");
        o.observe_account("ticket", current, obs(1.0, 5000, 2300));
        assert_eq!(o.accounts.snapshot(2300)[0].windows[0].utilization, 0.31);
    }

    fn obs(utilization: f64, reset: i64, seen: i64) -> LimitObs {
        LimitObs {
            status: LimitStatus::Allowed,
            windows: vec![WindowObs {
                window: "five_hour".into(),
                utilization,
                resets_at_s: reset,
            }],
            using_credits: false,
            source: "stream",
            observed_at_s: seen,
        }
    }

    #[test]
    fn account_for_table() {
        for (harness, model, oauth, want) in [
            ("claude", "claude-opus-5-5", true, "claude-subscription"),
            ("claude", "claude-opus-5-5", false, "anthropic"),
            (
                "opencode",
                "openai/gpt-6.1-sol",
                true,
                "chatgpt-subscription",
            ),
            ("opencode", "openai/gpt-6.1-sol", false, "openai"),
            (
                "opencode",
                "fireworks-ai/deepseek-v4p1-flash",
                false,
                "fireworks-ai",
            ),
            ("opencode", "anthropic/claude-opus-5-5", false, "anthropic"),
        ] {
            assert_eq!(account_for(harness, model, oauth), want);
        }
    }

    #[test]
    fn older_event_never_regresses_same_window() {
        let ledger = AccountLedger::default();
        ledger.observe("claude-subscription", obs(0.95, 5000, 1200));
        ledger.observe("claude-subscription", obs(0.25, 5000, 1100));
        ledger.observe("claude-subscription", obs(0.35, 5000, 1300));
        assert_eq!(
            ledger
                .tightest("claude-subscription", 1400)
                .unwrap()
                .utilization,
            0.95
        );
        ledger.observe("claude-subscription", obs(0.1, 10000, 5100));
        ledger.observe("claude-subscription", obs(1.0, 5000, 5200));
        let current = ledger.tightest("claude-subscription", 5300).unwrap();
        assert_eq!((current.utilization, current.resets_at_s), (0.1, 10000));
    }

    #[test]
    fn window_past_reset_counts_as_reset() {
        let ledger = AccountLedger::default();
        let mut observation = obs(1.0, 5000, 1000);
        observation.status = LimitStatus::Rejected;
        ledger.observe("claude-subscription", observation);
        assert_eq!(
            ledger
                .tightest("claude-subscription", 4999)
                .unwrap()
                .utilization,
            1.0
        );
        assert_eq!(
            ledger
                .tightest("claude-subscription", 5000)
                .unwrap()
                .utilization,
            0.0
        );
        assert_eq!(ledger.snapshot(5000)[0].status, "allowed");
        ledger.observe("unknown-reset", obs(1.0, 0, 1000));
        assert_eq!(
            ledger
                .tightest("unknown-reset", 100000)
                .unwrap()
                .utilization,
            1.0
        );
    }

    #[test]
    fn stale_after_30min_without_active_run() {
        let ledger = AccountLedger::default();
        ledger.observe("claude-subscription", obs(0.95, 10000, 1000));
        assert!(!ledger.snapshot(2799)[0].stale);
        assert!(ledger.snapshot(2800)[0].stale);
        ledger.bind_run("first", "claude-subscription");
        ledger.bind_run("second", "claude-subscription");
        assert!(!ledger.snapshot(5000)[0].stale);
        ledger.release_run("first");
        ledger.release_run("first");
        assert!(!ledger.snapshot(5000)[0].stale);
        ledger.release_run("second");
        assert!(ledger.snapshot(5000)[0].stale);
        assert_eq!(
            ledger
                .tightest("claude-subscription", 5000)
                .unwrap()
                .utilization,
            0.95
        );
    }

    #[test]
    fn an_unknown_reset_rejection_after_expiry_is_not_erased() {
        let ledger = AccountLedger::default();
        ledger.observe("claude-subscription", obs(0.9, 5000, 1000));
        let mut wall = obs(1.0, 0, 5100);
        wall.status = LimitStatus::Rejected;
        ledger.observe("claude-subscription", wall);
        assert_eq!(ledger.snapshot(5200)[0].status, "rejected");
        assert_eq!(
            ledger
                .tightest("claude-subscription", 5200)
                .unwrap()
                .resets_at_s,
            0
        );
    }

    #[test]
    fn credits_are_latest_observation_not_a_same_window_latch() {
        let ledger = AccountLedger::default();
        let mut credit = obs(1.0, 5000, 1000);
        credit.using_credits = true;
        ledger.observe("claude-subscription", credit.clone());
        assert!(ledger.snapshot(1100)[0].using_credits);
        ledger.observe("claude-subscription", obs(1.0, 5000, 1200));
        ledger.observe("claude-subscription", credit);
        assert!(!ledger.snapshot(1300)[0].using_credits);
        assert_eq!(ledger.snapshot(1300)[0].last_seen_s, 1200);
    }

    #[test]
    fn unobserved_accounts_are_unknown_and_invalid_numbers_do_not_poison_json() {
        let ledger = AccountLedger::default();
        for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            ledger.observe("claude-subscription", obs(value, 5000, 1000));
        }
        assert!(ledger.snapshot(1100).is_empty());
        ledger.bind_run("run", "chatgpt-subscription");
        let snapshot = ledger.snapshot(5000);
        assert_eq!(snapshot[0].status, "unknown");
        assert_eq!(snapshot[0].detection, "probe");
        assert!(snapshot[0].windows.is_empty());
        assert!(ledger.tightest("chatgpt-subscription", 5000).is_none());
        serde_json::to_string(&snapshot).expect("finite API view");
    }
}
