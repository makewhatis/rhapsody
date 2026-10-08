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
    pub detection: String,
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
                    stale: !active
                        && (account.windows.is_empty()
                            || now_s.saturating_sub(account.last_seen_s) >= 1800),
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
    /// Read-only, in-memory view; no credential reads, network or control round-trip.
    pub fn accounts(&self, now_s: i64) -> Vec<AccountView> {
        self.accounts.snapshot(now_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_agent::ratelimit::{LimitStatus, WindowObs};

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
