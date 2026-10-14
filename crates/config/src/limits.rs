//! Account limit policy configuration (STUDIO-1126); no Go counterpart.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Thresholds {
    pub warn: f64,
    pub stop_new: f64,
    pub handoff: f64,
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_probe_knobs_default_validate_and_round_trip() {
        let defaults = super::Limits::default();
        assert_eq!(defaults.claude_probe_interval_s, 600);
        assert_eq!(defaults.claude_probe_timeout_s, 60);
        for interval in [0, 42] {
            let text = format!(
                "---\nlimits:\n  claude_probe_interval_s: {interval}\n  claude_probe_timeout_s: 7\n---\nprompt"
            );
            let def = crate::workflow::parse(&text).unwrap();
            let cfg = crate::decode::decode(&def).unwrap();
            let encoded = crate::encode::encode(&cfg).unwrap();
            let next = crate::decode::decode(&encoded).unwrap();
            assert_eq!(next.limits.claude_probe_interval_s, interval);
            assert_eq!(next.limits.claude_probe_timeout_s, 7);
        }
        assert!(
            super::Limits {
                claude_probe_timeout_s: 0,
                ..defaults
            }
            .validate()
            .is_err()
        );
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warn: 80.0,
            stop_new: 90.0,
            handoff: 95.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ThresholdOverride {
    pub warn: Option<f64>,
    pub stop_new: Option<f64>,
    pub handoff: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountLimits {
    pub thresholds: ThresholdOverride,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    pub claude_probe_interval_s: u64,
    pub claude_probe_timeout_s: u64,
    pub credits: String,
    pub credits_daily_usd: f64,
    pub wait_max_minutes: i64,
    pub handoff_grace_minutes: i64,
    pub thresholds: Thresholds,
    pub accounts: BTreeMap<String, AccountLimits>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            claude_probe_interval_s: 600,
            claude_probe_timeout_s: 60,
            credits: "never".into(),
            credits_daily_usd: 0.0,
            wait_max_minutes: 30,
            handoff_grace_minutes: 10,
            thresholds: Thresholds::default(),
            accounts: BTreeMap::new(),
        }
    }
}

impl Limits {
    pub fn for_account(&self, account: &str) -> Thresholds {
        let mut t = self.thresholds.clone();
        if let Some(o) = self.accounts.get(account) {
            t.warn = o.thresholds.warn.unwrap_or(t.warn);
            t.stop_new = o.thresholds.stop_new.unwrap_or(t.stop_new);
            t.handoff = o.thresholds.handoff.unwrap_or(t.handoff);
        }
        t
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.claude_probe_timeout_s == 0 {
            return Err("limits.claude_probe_timeout_s: must be positive".into());
        }
        if !matches!(
            self.credits.as_str(),
            "never" | "daily_cap" | "manager_urgent" | "always"
        ) {
            return Err(
                "limits.credits: expected never, daily_cap, manager_urgent or always".into(),
            );
        }
        if !self.credits_daily_usd.is_finite() || self.credits_daily_usd < 0.0 {
            return Err("limits.credits_daily_usd: must be finite and nonnegative".into());
        }
        if self.wait_max_minutes < 0 || self.handoff_grace_minutes < 0 {
            return Err(
                "limits: wait_max_minutes and handoff_grace_minutes must be nonnegative".into(),
            );
        }
        for account in std::iter::once("").chain(self.accounts.keys().map(String::as_str)) {
            let t = self.for_account(account);
            if [t.warn, t.stop_new, t.handoff]
                .iter()
                .any(|v| !v.is_finite() || !(0.0..=100.0).contains(v))
                || t.warn > t.stop_new
                || t.stop_new > t.handoff
            {
                return Err(format!(
                    "limits.accounts.{account}.thresholds: expected 0 <= warn <= stop_new <= handoff <= 100"
                ));
            }
        }
        Ok(())
    }
}
