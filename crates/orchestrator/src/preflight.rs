//! preflight — dispatch credential-liveness probe (BO-59; Rhapsody-only, no Go v0.4.0 counterpart).
//!
//! # The incident this closes
//!
//! An expired agent credential (e.g. a stale Claude OAuth login) makes every dispatched run die in
//! ~1 second at 0 tokens on the same verbatim `OAuth session expired` error. The control loop cannot
//! tell that infrastructure fault from a retryable ticket fault, so it claims a ticket → dispatches →
//! dies → retries every ~5 minutes, unattended, burning a dead run every ~5 min until the credential
//! recovers on its own. Zero-token 1-second failures are the signature of an infra fault, not a ticket
//! fault.
//!
//! # Scoped admission (STUDIO-1144)
//!
//! Probe verdicts are keyed by account, harness and effective command/environment policy. Healthy
//! answers cache for five minutes; definite credential failures hold only that context before claim
//! or slot admission. Unknown answers (including timeouts) permit dispatch and retry next tick.
//! Supported contexts probe concurrently under one timeout window. Three consecutive timeouts report
//! an infrastructure fault in the log and human feed; account holds/reasons are cache-only ledger reads.
//!
//! # Same scrubbed environment as the children
//!
//! When `claude.billing_guard` is on, the runner scrubs `CLAUDE_CODE_OAUTH_TOKEN` /
//! `ANTHROPIC_API_KEY` / … from every dispatched child, which then authenticates via the tokenless
//! Keychain path. The probe MUST exercise that SAME credential, or it could report healthy while every
//! child still dies (or the reverse). [`scrub_child_env`] therefore reuses the runner's exact
//! `scrub_env` + `scrubbed_env_vars` + `TRACKER_ENV_VARS` primitives, honoring the effective
//! `billing_guard` (only the per-issue "me" identity is omitted — a probe has no issue).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rhapsody_agent::claude::{
    TRACKER_ENV_VARS, billing_guard_enabled, scrub_env, scrubbed_env_vars, split_command,
};

use crate::orchestrator::Orchestrator;

/// The liveness probe prompt: `claude -p 'reply with exactly: OK'`. Exit 0 with `OK` on stdout means
/// the agent credential is live (verified working on the host on 2026-08-19).
pub(crate) const PROBE_PROMPT: &str = "reply with exactly: OK";

/// The expected probe reply token on stdout.
const PROBE_OK: &str = "OK";

/// How long a HEALTHY verdict is trusted before re-probing — the cache TTL (requirement: probe at most
/// once per TTL). A DEAD verdict is never cached past the tick, so recovery is detected on the next tick
/// rather than after waiting the full TTL.
pub(crate) const PROBE_TTL: Duration = Duration::from_secs(5 * 60);

/// The default per-probe timeout: well under the 30s poll interval. A probe that does not answer within
/// this is treated as unknown and allows dispatch. It is a field on the orchestrator
/// ([`Orchestrator::probe_timeout`]) so tests can shrink it; this is the production default.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// While the credential stays dead, the steady-state skip is logged at most once per this window so
/// `/api/v1/logs` shows the cause without a line every 30s forever. Transitions (healthy→dead,
/// dead→healthy) always log loudly regardless of this rate limit.
const DEAD_LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The operator advisory surfaced on each project's `/api/v1/projects` status while the credential
/// probe reports dead (the state-visible half of the "legible skip" requirement).
pub(crate) const CREDENTIAL_DEAD_WARNING: &str = "an agent account has a definite credential failure — see /api/v1/accounts for the probe reason";

/// The inputs a credential probe needs, captured from the effective config at probe time so a
/// hot-reloaded command / billing_guard / key is honored. Never debug-print this request: commands
/// and tracker credentials can contain secrets.
#[derive(Clone)]
pub struct ProbeRequest {
    /// The configured agent backend (`claude` / `codex`). Only a backend with a probe is probed.
    pub backend: String,
    /// Non-secret ledger account id, not a credential value.
    pub account: String,
    /// The claude command (default `claude`), shell-split into name+args like the runner.
    pub command: String,
    /// The EFFECTIVE billing guard (already resolved via `billing_guard_enabled`); it selects which env
    /// vars are scrubbed, so the probe authenticates via the SAME path the dispatched child does.
    pub billing_guard: bool,
    /// The resolved tracker (Linear) credential, withheld from the probe's env by value — exactly as
    /// the runner withholds it from children (design §15.5).
    pub tracker_api_key: String,
}

/// A credential-liveness verdict.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// The credential is live (probe exited 0 with `OK`).
    Healthy,
    /// A definite expired, missing or rejected credential. Reasons must be non-secret.
    Dead(String),
    /// Infrastructure/protocol failures cannot establish expiry and do not hold dispatch.
    Unknown(String),
}

/// The injectable credential-liveness probe seam (BO-59). Production installs [`ClaudeCredentialProbe`];
/// tests inject a fake. Object-safe async via `async-trait` — the same idiom the `Tracker` /
/// `SummonSource` traits use.
#[async_trait]
pub trait CredentialProbe: Send + Sync {
    /// Probes the backend credential named by `req`. MUST NOT claim anything and MUST NOT block
    /// indefinitely — the caller additionally bounds it with [`Orchestrator::probe_timeout`].
    async fn probe(&self, req: &ProbeRequest) -> ProbeOutcome;
}

/// Whether a backend has a credential probe. A backend with no probe (`codex`, or any future backend)
/// is a clean no-op that never blocks dispatch.
pub(crate) fn backend_has_probe(backend: &str) -> bool {
    backend == "claude"
}

/// The gh credentials a MANAGER child must never see (STUDIO-1014, design §4.5). The host serves
/// every `gh` read for the manager, so the run has no business holding a GitHub token: dropping
/// them here is the "cannot use it to act" half of the boundary.
pub(crate) const MANAGER_GH_ENV_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];

/// The scrubbed environment the credential probe runs with: identical to the per-turn scrub the claude
/// runner applies to its children (`runner.rs`) — the tracker vars are ALWAYS dropped (by name and by
/// value) and the billing/routing vars are dropped when the guard is on — MINUS the per-issue "me"
/// identity, which a credential probe has no issue for. Reusing the runner's exact `scrub_env` +
/// `scrubbed_env_vars` + `TRACKER_ENV_VARS` primitives guarantees the probe authenticates via the SAME
/// credential path the dispatched children do.
///
/// `manager_role` additionally drops [`MANAGER_GH_ENV_VARS`] (STUDIO-1014, §4.5): the manager has no
/// `gh`, so no GitHub token may reach its environment. `false` for every non-manager caller keeps
/// the existing env byte-identical.
pub(crate) fn scrub_child_env(
    base_env: &[String],
    billing_guard: bool,
    tracker_api_key: &str,
    manager_role: bool,
) -> Vec<String> {
    let mut drop_names: Vec<&str> = if billing_guard {
        scrubbed_env_vars()
    } else {
        TRACKER_ENV_VARS.to_vec()
    };
    if manager_role {
        drop_names.extend(MANAGER_GH_ENV_VARS);
    }
    scrub_env(base_env, &drop_names, &[tracker_api_key])
}

/// The current process environment as `KEY=VALUE` strings (mirrors the runner's `base_env` capture).
/// Shared with [`crate::triage`], whose model turn scrubs the same environment for the same reason.
pub(crate) fn process_env() -> Vec<String> {
    std::env::vars_os()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect()
}

/// The production credential probe: shells out `claude -p 'reply with exactly: OK'` through the scrubbed
/// child environment and reports live iff it exits 0 with `OK` on stdout. Stateless — it reads every
/// input from the [`ProbeRequest`] the control task builds from the live effective config, so a
/// hot-reloaded command / billing_guard / key is honored on the next probe.
#[derive(Debug, Default, Clone)]
pub struct ClaudeCredentialProbe;

#[async_trait]
impl CredentialProbe for ClaudeCredentialProbe {
    async fn probe(&self, req: &ProbeRequest) -> ProbeOutcome {
        // Defensive: only claude has a probe (the caller already gates on `backend_has_probe`).
        if !backend_has_probe(&req.backend) {
            return ProbeOutcome::Healthy;
        }
        let (name, base_args) = match split_command(&req.command) {
            Ok(v) => v,
            Err(_) => {
                return ProbeOutcome::Unknown("invalid claude probe command".into());
            }
        };
        let env = scrub_child_env(
            &process_env(),
            req.billing_guard,
            &req.tracker_api_key,
            false,
        );

        let mut cmd = tokio::process::Command::new(&name);
        cmd.args(&base_args);
        cmd.arg("-p").arg(PROBE_PROMPT);
        cmd.env_clear();
        for kv in &env {
            if let Some((k, v)) = kv.split_once('=') {
                cmd.env(k, v);
            }
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Reap the child if the caller's timeout drops this future mid-probe.
        cmd.kill_on_drop(true);

        match cmd.output().await {
            Ok(out) => classify_probe(
                out.status.success(),
                out.status.code(),
                &out.stdout,
                &out.stderr,
            ),
            Err(_) => ProbeOutcome::Unknown("could not launch claude credential probe".into()),
        }
    }
}

/// A completed process is healthy only with exit 0 and OK, dead only with an explicit auth failure,
/// otherwise unknown. Never forward raw diagnostics to a human-facing surface.
fn classify_probe(success: bool, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> ProbeOutcome {
    if success && stdout_is_ok(stdout) {
        ProbeOutcome::Healthy
    } else if let Some(reason) = definite_credential_failure(stdout, stderr) {
        ProbeOutcome::Dead(reason.into())
    } else {
        ProbeOutcome::Unknown(probe_failure_reason(code))
    }
}

/// Whether the probe's stdout carries the `OK` reply — the exact token on some line (trimmed), NOT a
/// substring (so a banner like `OKAY`/`not ok` never passes).
fn stdout_is_ok(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|l| l.trim() == PROBE_OK)
}

/// A closed operator-facing reason for a failed probe, without process output.
fn probe_failure_reason(code: Option<i32>) -> String {
    let code = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    format!("claude credential probe exited {code} without OK; credential status unknown")
}

fn definite_credential_failure(stdout: &[u8], stderr: &[u8]) -> Option<&'static str> {
    // Inspect diagnostics, but never publish them: a harness may echo credential contents.
    for bytes in [stdout, stderr] {
        let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
        if text.contains("oauth session expired") || text.contains("token has expired") {
            return Some("claude credential expired; refresh the login");
        }
        if text.contains("not logged in") || text.contains("missing api key") {
            return Some("claude credential missing; log in before dispatch");
        }
        if [
            "api error: 401",
            "apierror: status 401",
            "http 401",
            "status code: 401",
            "\"statuscode\":401",
            "invalid api key",
            "invalid_api_key",
        ]
        .iter()
        .any(|marker| text.contains(marker))
        {
            return Some("claude credential rejected (401/invalid key); refresh the login");
        }
    }
    None
}

/// The cached credential-probe verdict for the dispatch preflight. Control-task-owned, so it needs
/// no lock. A rejected run also invalidates its healthy TTL.
#[derive(Debug, Clone)]
pub struct ProbeCache {
    /// When the last probe ran (read through [`Orchestrator::now`]).
    pub(crate) checked_at: DateTime<Utc>,
    /// The last verdict: `true` = live, `false` = dead/unverifiable.
    pub(crate) healthy: bool,
    /// When the steady-state DEAD skip was last logged, for rate-limiting the repeat. `None` while
    /// healthy (or before the first dead log).
    pub(crate) last_logged_dead_at: Option<DateTime<Utc>>,
}

/// Whether a cached HEALTHY verdict is still fresh (within `ttl`). A dead verdict is never fresh — the
/// preflight always re-probes after a failure.
fn healthy_and_fresh(cache: &ProbeCache, now: DateTime<Utc>, ttl: Duration) -> bool {
    if !cache.healthy {
        return false;
    }
    match chrono::Duration::from_std(ttl) {
        Ok(ttl) => now.signed_duration_since(cache.checked_at) < ttl,
        Err(_) => false, // unrepresentable TTL → don't trust the cache; re-probe
    }
}

/// Whether a fresh DEAD verdict should be logged, and the `last_logged_dead_at` to store next. A
/// transition into dead (no prior cache, or a prior HEALTHY verdict) always logs loudly; a steady-state
/// dead repeat logs at most once per `interval`.
fn dead_log_decision(
    prev: Option<&ProbeCache>,
    now: DateTime<Utc>,
    interval: Duration,
) -> (bool, Option<DateTime<Utc>>) {
    match prev {
        Some(c) if !c.healthy => {
            let due = match (c.last_logged_dead_at, chrono::Duration::from_std(interval)) {
                (Some(t), Ok(win)) => now.signed_duration_since(t) >= win,
                (Some(_), Err(_)) => false,
                (None, _) => true,
            };
            if due {
                (true, Some(now))
            } else {
                (false, c.last_logged_dead_at)
            }
        }
        // No prior cache, or a prior HEALTHY verdict → this is a transition into dead: log loudly.
        _ => (true, Some(now)),
    }
}

/// No secrets in the account/harness coordinates; the command stays private and is never rendered.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProbeKey {
    account: String,
    harness: String,
    command: String,
    billing_guard: bool,
}

impl ProbeRequest {
    fn key(&self) -> ProbeKey {
        ProbeKey {
            account: self.account.clone(),
            harness: self.backend.clone(),
            command: self.command.clone(),
            billing_guard: self.billing_guard,
        }
    }
}

pub(crate) struct CachedProbe {
    cache: ProbeCache,
    held: bool,
    reason: String,
    timeouts: usize,
}

impl Orchestrator {
    #[cfg(test)]
    pub(crate) fn seed_dead_probe(&mut self, harness: &str, project: &str) {
        let req = self
            .probe_request_for(harness, project)
            .expect("probe context");
        self.probe_cache.insert(
            req.key(),
            CachedProbe {
                cache: ProbeCache {
                    checked_at: (self.now)(),
                    healthy: false,
                    last_logged_dead_at: None,
                },
                held: true,
                reason: "expired login".into(),
                timeouts: 0,
            },
        );
    }

    /// Installs the production credential-liveness probe (BO-59). The daemon calls this once at startup;
    /// without it the credential preflight is a no-op and dispatch is byte-identical to the pre-feature
    /// behavior (the default for tests and any non-production build).
    pub fn set_credential_probe(&mut self, probe: Arc<dyn CredentialProbe>) {
        self.cred_probe = Some(probe);
    }

    fn probe_request_for(&self, harness: &str, project: &str) -> Option<ProbeRequest> {
        let eff = self.eff.as_ref()?;
        let cfg = eff.project_by_slug(project).map_or(&eff.cfg, |p| &p.mcfg);
        let backend = self.effective_harness(harness);
        if !backend_has_probe(&backend) {
            return None;
        }
        let billing_guard = billing_guard_enabled(cfg.claude.billing_guard);
        Some(ProbeRequest {
            account: crate::accounts::account_for(&backend, "", billing_guard),
            backend,
            command: cfg.claude.command.clone(),
            billing_guard,
            tracker_api_key: cfg.tracker.api_key.clone(),
        })
    }

    pub(crate) fn credential_probe_held(&self, harness: &str, project: &str) -> bool {
        self.credential_probe_reason(harness, project).is_some()
    }

    pub(crate) fn has_credential_holds(&self) -> bool {
        !self.credential_rejections.is_empty() || self.probe_cache.values().any(|c| c.held)
    }

    pub(crate) fn rejected_credential_reason(&self, harness: &str, account: &str) -> Option<&str> {
        self.credential_rejections
            .get(&(self.effective_harness(harness), account.into()))
            .map(|hold| hold.reason.as_str())
    }

    fn credential_canary_interval_ms(&self) -> i64 {
        self.eff
            .as_ref()
            .map_or(1_800_000, |e| e.cfg.agent.credential_canary_interval_ms)
    }

    /// Existing Claude liveness and ChatGPT usage probes own recovery for their credentials.
    fn credential_has_recovery_probe(harness: &str, account: &str) -> bool {
        backend_has_probe(harness) || (harness == "opencode" && account == "chatgpt-subscription")
    }

    pub(crate) fn credential_canary_due(&self, run: &crate::RunningEntry, account: &str) -> bool {
        let harness = self.effective_harness(&run.harness);
        if crate::review::is_non_tracker_key(&run.issue.id)
            || Self::credential_has_recovery_probe(&harness, account)
            || self.rejected_credential_reason(&harness, account).is_none()
        {
            return false;
        }
        let Some(last) = self
            .credential_canaries
            .get(&(harness.clone(), account.into()))
        else {
            return false;
        };
        (self.now)().signed_duration_since(*last).num_milliseconds()
            >= self.credential_canary_interval_ms()
            && !self
                .running
                .values()
                .any(|r| r.credential_harness == harness && r.pricing.account == account)
    }

    /// Selection is read-only; reserve only in its pass-local set. Final admission consumes time.
    pub(crate) fn issue_credential_canary(
        &self,
        issue: &rhapsody_core::Issue,
        project: &str,
    ) -> Option<(String, String)> {
        if self.credential_rejections.is_empty() {
            return None;
        }
        let run = self.limit_projection(issue, project);
        let pricing = self.run_pricing_for(&run.harness, &run.model_override, project);
        let account = if run.pricing.account.is_empty() {
            &pricing.account
        } else {
            &run.pricing.account
        };
        self.credential_canary_due(&run, account)
            .then(|| (self.effective_harness(&run.harness), account.clone()))
    }

    pub(crate) fn select_credential_canary(
        &self,
        issue: &rhapsody_core::Issue,
        project: &str,
        selected: &mut BTreeSet<(String, String)>,
    ) -> bool {
        self.issue_credential_canary(issue, project)
            .is_none_or(|key| selected.insert(key))
    }

    /// Due canaries get an oldest-first walk; other work retains the normal dispatch ordering.
    pub(crate) fn credential_canary_cmp(
        a: &rhapsody_core::Issue,
        b: &rhapsody_core::Issue,
        a_due: bool,
        b_due: bool,
    ) -> std::cmp::Ordering {
        b_due.cmp(&a_due).then_with(|| {
            if a_due && b_due {
                // Missing ages sort last, as in the normal dispatcher.
                a.created_at
                    .is_none()
                    .cmp(&b.created_at.is_none())
                    .then_with(|| a.created_at.cmp(&b.created_at))
                    .then_with(|| a.identifier.cmp(&b.identifier))
            } else {
                crate::dispatch::dispatch_cmp(a, b)
            }
        })
    }

    pub(crate) fn projected_credential_rejection(&self, run: &crate::RunningEntry) -> Option<&str> {
        let account = if run.pricing.account.is_empty() {
            self.run_pricing_for(&run.harness, &run.model_override, &run.project_slug)
                .account
        } else {
            run.pricing.account.clone()
        };
        self.rejected_credential_reason(&run.harness, &account)
    }

    pub(crate) fn note_credential_wait(
        &self,
        issue: &rhapsody_core::Issue,
        project: &str,
        account: &str,
        reason: &str,
    ) {
        self.note_usd_budget_hold(crate::budget::BudgetHeld {
            subject: issue.identifier.clone(),
            title: issue.title.clone(),
            project: project.into(),
            provider: account.into(),
            reason: format!("waiting: {account} credential — {reason}"),
            ..Default::default()
        });
    }

    pub(crate) fn record_credential_rejection(&mut self, run: &crate::RunningEntry) {
        let harness = if run.credential_harness.is_empty() {
            self.effective_harness(&run.harness)
        } else {
            run.credential_harness.clone()
        };
        let account = run.pricing.account.clone();
        if account.is_empty() {
            tracing::warn!(
                run_id = run.run_id,
                "rejected login has no account attribution"
            );
            return;
        }
        let command = match harness.as_str() {
            "opencode" => "opencode auth login",
            "claude" => "claude auth login",
            _ => "the harness login command",
        };
        for (key, cached) in &mut self.probe_cache {
            if key.harness == harness && key.account == account {
                cached.cache.healthy = false;
            }
        }
        self.credential_canaries
            .insert((harness.clone(), account.clone()), (self.now)());
        let retry = if Self::credential_has_recovery_probe(&harness, &account) {
            String::new()
        } else {
            let minutes = self.credential_canary_interval_ms() as f64 / 60_000.0;
            format!("; retries automatically every {minutes} minutes")
        };
        self.credential_rejections.insert(
            (harness.clone(), account.clone()),
            crate::accounts::CredentialView {
                account: account.clone(),
                harness: harness.clone(),
                held: true,
                reason: format!("login rejected — run `{command}` as the daemon's user{retry}; clear now with POST /api/v1/accounts/{account}/credentials/{harness}/clear"),
                checked_at_s: (self.now)().timestamp(),
            },
        );
        self.publish_credential_views();
    }

    pub(crate) fn clear_credential_rejection(&mut self, harness: &str, account: &str) -> bool {
        let removed = self
            .credential_rejections
            .remove(&(harness.into(), account.into()))
            .is_some();
        if removed {
            self.credential_canaries
                .remove(&(harness.into(), account.into()));
            // Drop only this credential's display holds, never an independent quota/budget hold.
            for held in self.budget_ledger.held(self.budget_hold_ttl()) {
                if held.provider == account && held.reason.contains("login rejected") {
                    if self
                        .credential_rejections
                        .values()
                        .any(|hold| hold.account == account)
                    {
                        continue;
                    }
                    self.release_budget_hold(&held.subject);
                    self.limit_policy.holds.remove(&held.subject);
                }
            }
            self.publish_credential_views();
            self.schedule_tick(Duration::ZERO);
        }
        removed
    }

    pub(crate) fn operator_clear_credential(&mut self, harness: &str, account: &str) -> bool {
        let before = self.probe_cache.len();
        self.probe_cache.retain(|key, cached| {
            !(key.harness == harness && key.account == account && cached.held)
        });
        let probe_cleared = before != self.probe_cache.len();
        let rejection_cleared = self.clear_credential_rejection(harness, account);
        if probe_cleared && !rejection_cleared {
            self.publish_credential_views();
            self.schedule_tick(Duration::ZERO);
        }
        probe_cleared || rejection_cleared
    }

    fn publish_credential_views(&self) {
        self.accounts.replace_credential_probes(
            self.probe_cache
                .iter()
                .map(|(key, c)| crate::accounts::CredentialView {
                    account: key.account.clone(),
                    harness: key.harness.clone(),
                    held: c.held,
                    reason: c.reason.clone(),
                    checked_at_s: c.cache.checked_at.timestamp(),
                })
                .chain(self.credential_rejections.values().cloned())
                .collect(),
        );
    }

    pub(crate) fn credential_probe_reason(&self, harness: &str, project: &str) -> Option<&str> {
        self.probe_request_for(harness, project)
            .and_then(|r| self.probe_cache.get(&r.key()))
            .filter(|c| c.held)
            .map(|c| c.reason.as_str())
    }

    fn manager_probe_request(&self, harness: &str, project: &str) -> Option<ProbeRequest> {
        let mut req = self.probe_request_for(harness, project)?;
        // Isolated managers use native OAuth even under the worker API-billing escape hatch.
        req.billing_guard = true;
        req.account = "claude-subscription".into();
        Some(req)
    }

    pub(crate) fn manager_credential_probe_reason(
        &self,
        harness: &str,
        project: &str,
    ) -> Option<&str> {
        self.manager_probe_request(harness, project)
            .and_then(|r| self.probe_cache.get(&r.key()))
            .filter(|c| c.held)
            .map(|c| c.reason.as_str())
    }

    pub(crate) fn run_credential_probe_reason(&self, run: &crate::RunningEntry) -> Option<&str> {
        if let Some(reason) = self.projected_credential_rejection(run) {
            return Some(reason);
        }
        if crate::managerrun::is_manager_key(&run.issue.id) {
            self.manager_credential_probe_reason(&run.harness, &run.project_slug)
        } else {
            self.credential_probe_reason(&run.harness, &run.project_slug)
        }
    }

    pub(crate) fn credential_human_feed(&self) -> Vec<crate::reviewreconcile::Divergence> {
        self.probe_cache.iter().enumerate().filter(|(_, (_, c))| c.timeouts >= 3).map(|(index, (key, c))| {
            crate::reviewreconcile::Divergence {
                pr: format!("{} credential probe {} ({})", key.harness, index + 1, key.account),
                kind: crate::reviewreconcile::DivergenceKind::CredentialInfrastructure,
                ticket: String::new(), reviewer: String::new(), stale_secs: 0,
                auto_merge_reason: None, capacity_held: None, capacity_unreadable: None,
                adjudicated_head: String::new(), current_head: String::new(), rounds: 0,
                findings: Vec::new(),
                reason: format!("{} credential probe for {}: infrastructure problem, {} consecutive timeouts; {}. Dispatch continues; retry next tick.", key.harness, key.account, c.timeouts, c.reason),
            }
        }).collect()
    }

    /// Refresh scoped contexts concurrently; no credential verdict returns early from the whole tick.
    pub(crate) async fn credential_preflight(&mut self) {
        let Some(probe) = self.cred_probe.clone() else {
            return;
        };
        let mut requests = BTreeMap::new();
        let projects: Vec<String> = self
            .eff
            .as_ref()
            .map(|e| {
                if e.projects.is_empty() {
                    vec![String::new()]
                } else {
                    e.projects
                        .iter()
                        .filter(|p| !p.disabled)
                        .map(|p| p.slug.clone())
                        .collect()
                }
            })
            .unwrap_or_default();
        // Profiles may choose Claude even when a project's configured primary is OpenCode.
        let mut harnesses = vec![String::new()];
        if let (Some(teams), Some(dir)) = (&self.teams, &self.teams_profiles_dir) {
            for ident in &teams.roster {
                if let Ok(profile) = rhapsody_config::profiles::resolve(dir, &ident.profile) {
                    harnesses.push(profile.harness);
                    harnesses.extend(profile.fallback.into_iter().map(|e| e.harness));
                }
            }
        }
        for project in projects {
            for harness in &harnesses {
                if let Some(req) = self.probe_request_for(harness, &project) {
                    requests.insert(req.key(), req);
                }
            }
            if let Some(teams) = &self.teams {
                for entry in teams.manager.effective_harnesses() {
                    if let Some(req) = self.manager_probe_request(&entry.harness, &project) {
                        requests.insert(req.key(), req);
                    }
                }
            }
        }
        self.probe_cache.retain(|key, _| requests.contains_key(key));
        let now = (self.now)();
        let mut tasks = tokio::task::JoinSet::new();
        let mut pending = BTreeSet::new();
        for (key, req) in requests {
            if self
                .probe_cache
                .get(&key)
                .is_some_and(|c| healthy_and_fresh(&c.cache, now, PROBE_TTL))
            {
                continue;
            }
            let probe = probe.clone();
            let timeout = self.probe_timeout;
            pending.insert(key.clone());
            tasks.spawn(async move {
                let (outcome, timed_out) =
                    match tokio::time::timeout(timeout, probe.probe(&req)).await {
                        Ok(outcome) => (outcome, false),
                        Err(_) => (
                            ProbeOutcome::Unknown(format!(
                                "no answer within {timeout:?}; credential status unknown"
                            )),
                            true,
                        ),
                    };
                (key, outcome, timed_out)
            });
        }
        while let Some(result) = tasks.join_next().await {
            let Ok((key, outcome, timed_out)) = result else {
                tracing::warn!("credential probe task failed; credential status unknown");
                continue;
            };
            pending.remove(&key);
            let prev = self.probe_cache.get(&key);
            let timeouts = if timed_out {
                prev.map_or(1, |c| c.timeouts.saturating_add(1))
            } else {
                0
            };
            let held = matches!(outcome, ProbeOutcome::Dead(_));
            let healthy = matches!(outcome, ProbeOutcome::Healthy);
            if healthy {
                self.clear_credential_rejection(&key.harness, &key.account);
            }
            let prev = self.probe_cache.get(&key);
            let reason = match outcome {
                ProbeOutcome::Healthy => String::new(),
                ProbeOutcome::Dead(reason) | ProbeOutcome::Unknown(reason) => reason,
            };
            let (log, last_logged) = if healthy {
                (false, None)
            } else {
                dead_log_decision(prev.map(|c| &c.cache), now, DEAD_LOG_INTERVAL)
            };
            if prev.is_some_and(|c| c.held || c.timeouts >= 3) && healthy {
                tracing::warn!(account = %key.account, harness = %key.harness, "credential probe recovered; context may dispatch");
            }
            if log || timeouts == 3 || prev.is_some_and(|c| c.held != held) {
                tracing::warn!(account = %key.account, harness = %key.harness, %reason, held, timeouts,
                    "credential probe result; definite failures hold only this context; repeated timeouts are infrastructure faults");
            }
            self.probe_cache.insert(
                key,
                CachedProbe {
                    cache: ProbeCache {
                        checked_at: now,
                        healthy,
                        last_logged_dead_at: last_logged,
                    },
                    held,
                    reason,
                    timeouts,
                },
            );
        }
        // A probe task failing before it returns is unknown too; never keep its previous hold.
        for key in pending {
            self.probe_cache.insert(
                key,
                CachedProbe {
                    cache: ProbeCache {
                        checked_at: now,
                        healthy: false,
                        last_logged_dead_at: Some(now),
                    },
                    held: false,
                    reason: "credential probe task failed; credential status unknown".into(),
                    timeouts: 0,
                },
            );
        }
        self.publish_credential_views();
    }
}

impl crate::ControlHandle {
    /// Operator release; no credential contents or provider I/O cross this surface.
    pub async fn clear_credential_hold(
        &self,
        harness: &str,
        account: &str,
    ) -> Result<bool, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(crate::control_loop::Event::ClearCredentialHold {
                harness: harness.into(),
                account: account.into(),
                reply,
            })
            .map_err(|_| "control loop unavailable".to_string())?;
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            result = rx => result.map_err(|_| "control loop unavailable".to_string()),
            _ = lifetime.cancelled() => Err("daemon stopping".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeZone;

    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{
        DispatchedEntries, empty_effective, empty_resolved_project, issue, record_entries, set_of,
    };
    use rhapsody_tracker::fake::Fake;

    fn credential_exit(o: &Orchestrator, id: &str, failed: bool) -> crate::retry::EvWorkerExit {
        crate::retry::EvWorkerExit {
            issue_id: id.into(),
            started_at: o.running[id].started_at,
            last_state: "Done".into(),
            failed,
            auth_needed: failed,
            err_msg: if failed {
                "opencode_auth_needed: login rejected".into()
            } else {
                String::new()
            },
            refused: false,
            onepassword_hold: None,
            declared_handoff: false,
            review_verdict: None,
            verdict_text: None,
        }
    }

    #[tokio::test]
    async fn credential_canary_retries_oldest_once_per_interval_and_recovers() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        for multi in [false, true] {
            let (mut o, sink, _) = orch_with_probe(false, FakeKind::Healthy);
            let store = Arc::new(
                rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).unwrap(),
            );
            o.store = store.clone();
            o.cred_probe = None;
            o.now = Box::new(fixed_now);
            let eff = o.eff.as_mut().unwrap();
            let tracker = Arc::new(Fake::new());
            eff.tracker = tracker.clone();
            eff.cfg.agent.backend = "opencode".into();
            eff.cfg.opencode.model = "fireworks-ai/test-model".into();
            if multi {
                let mut p = empty_resolved_project("p", tracker.clone());
                p.mcfg = eff.cfg.clone();
                p.active_states = eff.active_states.clone();
                p.terminal_states = eff.terminal_states.clone();
                p.max_concurrent = 10;
                let mut other = empty_resolved_project("other", tracker.clone());
                other.mcfg = p.mcfg.clone();
                other.active_states = p.active_states.clone();
                other.terminal_states = p.terminal_states.clone();
                other.max_concurrent = 10;
                eff.projects.push(p);
                eff.projects.push(other);
            }
            let mut oldest = issue("old", "TEST-1", "Todo");
            oldest.created_at = Some(fixed_now() - chrono::Duration::days(2));
            oldest.priority = Some(4);
            let mut newer = issue("new", "TEST-2", "Todo");
            newer.created_at = Some(fixed_now() - chrono::Duration::days(1));
            newer.priority = Some(1); // canaries choose age rather than priority
            o.dispatch_issue(oldest.clone(), None, None, String::new());
            o.on_worker_exit(credential_exit(&o, "old", true));
            let reason = &o.credential_rejections.values().next().unwrap().reason;
            assert!(reason.contains("retries automatically every 30 minutes"));
            assert!(
                reason.contains("POST /api/v1/accounts/fireworks-ai/credentials/opencode/clear")
            );
            let select = |o: &Orchestrator| {
                let candidates = vec![newer.clone(), oldest.clone()];
                if multi {
                    o.select_dispatch_multi(
                        candidates
                            .into_iter()
                            .enumerate()
                            .map(|(i, iss)| crate::select::TaggedIssue { iss, proj: Some(i) })
                            .collect(),
                    )
                    .into_iter()
                    .map(|t| t.iss)
                    .collect::<Vec<_>>()
                } else {
                    o.select_dispatch(candidates)
                }
            };
            o.now = Box::new(|| fixed_now() + chrono::Duration::milliseconds(1_799_999));
            assert!(select(&o).is_empty(), "no early admission");
            o.now = Box::new(|| fixed_now() + chrono::Duration::minutes(30));
            let picks = select(&o);
            assert_eq!(picks.len(), 1, "one due canary, even with spare slots");
            assert_eq!(picks[0].id, "old", "oldest waiting ticket wins");
            o.dispatch_issue(picks[0].clone(), None, None, String::new());
            o.dispatch_issue(newer.clone(), None, None, String::new());
            assert_eq!(o.running.len(), 1, "final gate cannot over-admit");
            assert!(select(&o).is_empty());
            o.on_worker_exit(credential_exit(&o, "old", true));
            assert_eq!(o.credential_rejections.len(), 1);
            assert_eq!(
                o.credential_rejections
                    .values()
                    .next()
                    .unwrap()
                    .checked_at_s,
                (fixed_now() + chrono::Duration::minutes(30)).timestamp()
            );
            assert!(o.retry_attempts.is_empty() && o.claimed.is_empty());
            assert!(o.human_holds.held().is_empty());
            o.now = Box::new(|| fixed_now() + chrono::Duration::minutes(60));
            let picks = select(&o);
            assert_eq!(picks.len(), 1);
            o.dispatch_issue(picks[0].clone(), None, None, String::new());
            o.on_worker_exit(credential_exit(&o, "old", false));
            assert!(o.credential_rejections.is_empty(), "success releases hold");
            assert!(
                o.accounts
                    .snapshot((o.now)().timestamp())
                    .iter()
                    .all(|a| a.credential_probes.iter().all(|p| !p.held))
            );
            assert_eq!(
                sink.lock().unwrap().len(),
                3,
                "initial failure, rejected canary, successful canary"
            );
            use rhapsody_store::Store;
            let runs = store
                .list_runs(rhapsody_store::RunFilter {
                    issue: "TEST-1".into(),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(runs.len(), 3);
            assert_eq!(
                runs.iter()
                    .filter(|r| r.outcome == rhapsody_store::OUTCOME_FAILED)
                    .count(),
                2,
                "one row per rejected attempt"
            );
            assert!(
                tracker.add_label_calls().is_empty(),
                "no human label on either rejection"
            );
        }
    }

    #[tokio::test]
    async fn credential_canary_is_routed_and_prepared_admission_shares_the_interval() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _, _) = orch_with_probe(false, FakeKind::Healthy);
        o.cred_probe = None;
        o.now = Box::new(fixed_now);
        o.eff
            .as_mut()
            .unwrap()
            .cfg
            .agent
            .credential_canary_interval_ms = 60_000;
        let dir = crate::testsupport::TempDir::new();
        std::fs::write(
            dir.child("worker.md"),
            "---\nextends: swe\nharness: opencode\nmodel: fireworks-ai/test-model\n---\nWorker.\n",
        )
        .unwrap();
        let mut teams = rhapsody_config::teams::Teams::disabled();
        teams.enabled = true;
        teams.roster.push(rhapsody_config::teams::Identity {
            name: "worker".into(),
            profile: "worker".into(),
            ..Default::default()
        });
        o.teams = Some(teams);
        o.teams_profiles_dir = Some(dir.child("").into());
        let mut a = issue("a", "TEST-1", "Todo");
        a.labels = Some(vec!["rhapsody:@worker".into()]);
        o.dispatch_issue(a.clone(), None, None, String::new());
        o.on_worker_exit(credential_exit(&o, "a", true));
        o.now = Box::new(|| fixed_now() + chrono::Duration::milliseconds(59_999));
        assert!(o.select_dispatch(vec![a.clone()]).is_empty());
        o.now = Box::new(|| fixed_now() + chrono::Duration::seconds(60));
        assert_eq!(o.select_dispatch(vec![a.clone()]).len(), 1);
        let mut prepared = o.limit_projection(&a, "");
        assert!(
            o.gate_limit_dispatch(&mut prepared, &mut None, Some("fireworks-ai")),
            "prepared spec's explicit provider uses same canary"
        );
        o.dispatch_issue(a, None, None, String::new());
        assert_eq!(o.running["a"].identity, "worker");
        assert_eq!(o.running["a"].credential_harness, "opencode");
        assert!(!o.gate_limit_dispatch(&mut prepared, &mut None, Some("fireworks-ai")));
        o.now = Box::new(|| fixed_now() + chrono::Duration::hours(2));
        assert!(
            !o.gate_limit_dispatch(&mut prepared, &mut None, Some("fireworks-ai")),
            "never overlap an unfinished canary"
        );
        o.running.remove("a");
        assert!(o.credential_canary_due(&prepared, "fireworks-ai"));
        prepared.issue.id = "pr:o/r#1@worker".into();
        assert!(!o.credential_canary_due(&prepared, "fireworks-ai"));
    }

    #[tokio::test]
    async fn credential_canary_never_bypasses_existing_probes() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let dir = crate::testsupport::TempDir::new();
        let auth = dir.child("auth-kind.json");
        // Billing attribution needs only the kind, never an operator login or token.
        std::fs::write(&auth, r#"{"openai":{"type":"oauth"}}"#).unwrap();
        for (harness, account, model) in [
            ("claude", "claude-subscription", ""),
            ("opencode", "chatgpt-subscription", "openai/gpt-test"),
        ] {
            let (mut o, _, _) = orch_with_probe(false, FakeKind::Healthy);
            o.cred_probe = None;
            o.now = Box::new(fixed_now);
            let eff = o.eff.as_mut().unwrap();
            eff.cfg.agent.backend = harness.into();
            eff.cfg.opencode.model = model.into();
            eff.cfg.opencode.auth_source = auth.clone();
            let a = issue("a", "TEST-1", "Todo");
            o.dispatch_issue(a.clone(), None, None, String::new());
            assert_eq!(o.running["a"].pricing.account, account);
            o.on_worker_exit(credential_exit(&o, "a", true));
            o.now = Box::new(|| fixed_now() + chrono::Duration::days(1));
            assert!(o.select_dispatch(vec![a.clone()]).is_empty());
            o.dispatch_issue(a, None, None, String::new());
            assert!(o.running.is_empty(), "probe credentials stay held");
        }
    }

    #[tokio::test]
    async fn login_rejection_waits_even_with_a_healthy_profile_fallback() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _, _) = orch_with_probe(false, FakeKind::Healthy);
        let dir = crate::testsupport::TempDir::new();
        std::fs::write(dir.child("worker.md"), "---\nextends: swe\nharness: opencode\nmodel: fireworks-ai/test-model\nfallback:\n  - harness: opencode\n    model: openai/gpt-test\n---\nWorker.\n").unwrap();
        let mut teams = rhapsody_config::teams::Teams::disabled();
        teams.enabled = true;
        teams.roster.push(rhapsody_config::teams::Identity {
            name: "worker".into(),
            profile: "worker".into(),
            ..Default::default()
        });
        o.teams = Some(teams);
        o.teams_profiles_dir = Some(dir.child("").into());
        let mut a = issue("a", "TEST-1", "Todo");
        a.labels = Some(vec!["rhapsody:@worker".into()]);
        o.dispatch_issue(a.clone(), None, None, String::new());
        let rejection = credential_exit(&o, "a", true);
        o.on_worker_exit(rejection);
        let run = o.limit_projection(&a, "");
        let fallback = o.engine_list(
            "worker",
            rhapsody_config::profiles::EngineSpec {
                harness: "opencode".into(),
                model: "fireworks-ai/test-model".into(),
                effort: String::new(),
            },
        );
        assert_eq!(fallback.len(), 2);
        assert!(o.engine_usable(&fallback[1], ""));
        assert!(
            !o.limit_dispatch_ready(&a, ""),
            "credential rejection never chooses a fallback automatically"
        );
        let mut supplied = None;
        let mut run = run;
        assert!(!o.gate_limit_dispatch(&mut run, &mut supplied, None));
        assert!(supplied.is_none());
    }

    #[tokio::test]
    async fn repeated_rejection_and_success_use_the_dispatch_time_credential() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _, _) = orch_with_probe(false, FakeKind::Healthy);
        o.cred_probe = None;
        let eff = o.eff.as_mut().unwrap();
        eff.cfg.agent.backend = "opencode".into();
        eff.cfg.opencode.model = "fireworks-ai/test-model".into();
        // Three already-live runs can complete after the first rejection has closed admission.
        for id in ["a", "repeat", "success"] {
            o.dispatch_issue(
                issue(id, &format!("TEST-{id}"), "Todo"),
                None,
                None,
                String::new(),
            );
        }
        let first = credential_exit(&o, "a", true);
        o.on_worker_exit(first);
        let before = o
            .credential_rejections
            .values()
            .next()
            .unwrap()
            .checked_at_s;
        o.now = Box::new(move || chrono::DateTime::from_timestamp(before + 60, 0).unwrap());
        // Reloads must not attribute an old run's failure/success to a replacement engine.
        let eff = o.eff.as_mut().unwrap();
        eff.cfg.agent.backend = "claude".into();
        eff.cfg.opencode.model = "openai/gpt-test".into();
        let repeat = credential_exit(&o, "repeat", true);
        o.on_worker_exit(repeat);
        assert_eq!(o.credential_rejections.len(), 1);
        let held = o.credential_rejections.values().next().unwrap();
        assert_eq!(
            (&*held.harness, &*held.account),
            ("opencode", "fireworks-ai")
        );
        assert_eq!(
            held.checked_at_s,
            before + 60,
            "repeat updates the same credential"
        );
        let success = credential_exit(&o, "success", false);
        o.on_worker_exit(success);
        assert!(
            o.credential_rejections.is_empty(),
            "in-flight success releases its original credential"
        );
        assert!(o.retry_attempts.is_empty());
    }

    #[tokio::test]
    async fn unknown_probe_keeps_run_rejection_and_healthy_probe_releases_it() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _, calls) = orch_with_probe(false, FakeKind::Healthy);
        o.credential_preflight().await; // establish a fresh healthy TTL
        o.dispatch_issue(issue("a", "TEST-1", "Todo"), None, None, String::new());
        let rejected = credential_exit(&o, "a", true);
        o.on_worker_exit(rejected);
        o.cred_probe = Some(Arc::new(FakeProbe {
            kind: FakeKind::Hang,
            calls: calls.clone(),
        }));
        o.probe_timeout = Duration::from_millis(1);
        o.credential_preflight().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a rejection invalidates the healthy TTL"
        );
        assert!(
            o.has_credential_holds(),
            "unknown cannot erase a confirmed rejection"
        );
        o.cred_probe = Some(Arc::new(FakeProbe {
            kind: FakeKind::Healthy,
            calls,
        }));
        o.credential_preflight().await;
        assert!(!o.has_credential_holds());
        assert_eq!(
            o.select_dispatch(vec![
                issue("a", "TEST-1", "Todo"),
                issue("b", "TEST-2", "Todo")
            ])
            .len(),
            2
        );
    }

    #[tokio::test]
    async fn existing_chatgpt_usage_probe_releases_only_its_credential() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _, _) = orch_with_probe(false, FakeKind::Healthy);
        let mut run = crate::RunningEntry::empty(issue("a", "TEST-1", "Todo"));
        run.credential_harness = "opencode".into();
        for account in ["chatgpt-subscription", "fireworks-ai"] {
            run.pricing.account = account.into();
            o.record_credential_rejection(&run);
        }
        o.drive_event(crate::control_loop::Event::ChatgptProbeResult(Err(
            "usage_timeout",
        )))
        .await;
        assert_eq!(o.credential_rejections.len(), 2);
        o.drive_event(crate::control_loop::Event::ChatgptProbeResult(Ok(
            rhapsody_agent::ratelimit::LimitObs {
                source: "probe",
                status: rhapsody_agent::ratelimit::LimitStatus::Allowed,
                windows: Vec::new(),
                using_credits: false,
                observed_at_s: (o.now)().timestamp(),
            },
        )))
        .await;
        assert!(
            o.rejected_credential_reason("opencode", "chatgpt-subscription")
                .is_none()
        );
        assert!(
            o.rejected_credential_reason("opencode", "fireworks-ai")
                .is_some()
        );
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 19, 12, 0, 0)
            .single()
            .expect("valid fixed instant")
    }

    fn names_of(env: &[String]) -> Vec<&str> {
        env.iter()
            .map(|kv| kv.split_once('=').map(|(n, _)| n).unwrap_or(kv))
            .collect()
    }

    // --- Requirement 5 env-scrub pin: guard on → no CLAUDE_CODE_OAUTH_TOKEN in the probe env. ------

    #[test]
    fn scrub_child_env_guard_on_drops_oauth_and_billing_and_tracker() {
        let base = vec![
            "PATH=/usr/bin".to_string(),
            "CLAUDE_CODE_OAUTH_TOKEN=secret".to_string(),
            "ANTHROPIC_API_KEY=sk".to_string(),
            "ANTHROPIC_AUTH_TOKEN=tok".to_string(),
            "LINEAR_API_KEY=lin".to_string(),
        ];
        let scrubbed = scrub_child_env(&base, true, "", false);
        let names = names_of(&scrubbed);
        assert!(
            !names.contains(&"CLAUDE_CODE_OAUTH_TOKEN"),
            "guard on must drop the OAuth token so the probe uses the same tokenless path as children"
        );
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(!names.contains(&"ANTHROPIC_AUTH_TOKEN"));
        assert!(
            !names.contains(&"LINEAR_API_KEY"),
            "tracker var always dropped"
        );
        assert!(names.contains(&"PATH"), "unrelated vars survive the scrub");
    }

    #[test]
    fn scrub_child_env_guard_off_keeps_billing_but_still_drops_tracker() {
        let base = vec![
            "CLAUDE_CODE_OAUTH_TOKEN=secret".to_string(),
            "LINEAR_API_KEY=lin".to_string(),
            "MY_CUSTOM_TOKEN=lin".to_string(), // same value as the tracker key → dropped by value
            "KEEP=ok".to_string(),
        ];
        let scrubbed = scrub_child_env(&base, false, "lin", false);
        let names = names_of(&scrubbed);
        assert!(
            names.contains(&"CLAUDE_CODE_OAUTH_TOKEN"),
            "guard off (the API-billing escape hatch) keeps the billing vars"
        );
        assert!(
            !names.contains(&"LINEAR_API_KEY"),
            "the tracker var is always dropped by name, independent of the billing guard"
        );
        assert!(
            !names.contains(&"MY_CUSTOM_TOKEN"),
            "the tracker key is withheld by value even under a custom var name"
        );
        assert!(names.contains(&"KEEP"));
    }

    // STUDIO-1014 §4.5: a manager child additionally loses its GitHub tokens (the host serves gh).
    #[test]
    fn scrub_child_env_manager_drops_gh_tokens() {
        let base = vec![
            "PATH=/usr/bin".to_string(),
            "GH_TOKEN=ghs_x".to_string(),
            "GITHUB_TOKEN=ghp_y".to_string(),
            "CLAUDE_CODE_OAUTH_TOKEN=secret".to_string(),
            "KEEP=ok".to_string(),
        ];
        let scrubbed = scrub_child_env(&base, true, "", true);
        let names = names_of(&scrubbed);
        assert!(
            !names.contains(&"GH_TOKEN"),
            "manager run must not hold GH_TOKEN"
        );
        assert!(
            !names.contains(&"GITHUB_TOKEN"),
            "manager run must not hold GITHUB_TOKEN"
        );
        assert!(
            names.contains(&"KEEP"),
            "unrelated vars still survive the manager scrub"
        );
        // A non-manager caller keeps them (byte-identical to before this ticket).
        let ordinary = scrub_child_env(&base, true, "", false);
        assert!(names_of(&ordinary).contains(&"GH_TOKEN"));
    }

    // --- production verdict derivation: exit code + stdout → verdict (requirement 1's real path) ----

    #[test]
    fn classify_probe_maps_exit_and_stdout_to_verdict() {
        // exit 0 with `OK` on stdout → live.
        assert!(matches!(
            classify_probe(true, Some(0), b"OK\n", b""),
            ProbeOutcome::Healthy
        ));
        // `OK` among other lines still counts.
        assert!(matches!(
            classify_probe(true, Some(0), b"warming up\nOK\n", b""),
            ProbeOutcome::Healthy
        ));
        // The incident: explicit expiry is dead even if stdout somehow contained OK; the reason
        // names the credential failure without echoing the diagnostic.
        match classify_probe(
            false,
            Some(1),
            b"OK\n",
            b"OAuth session expired and could not be refreshed\n",
        ) {
            ProbeOutcome::Dead(reason) => {
                assert!(
                    reason.contains("expired"),
                    "reason names the credential failure: {reason}"
                );
                assert!(
                    !reason.contains("could not be refreshed"),
                    "reason must not echo stderr: {reason}"
                );
            }
            _ => panic!("an explicit OAuth expiry must be classified dead"),
        }
        // exit 0 but stdout lacks OK → unknown (a broken / differently-behaving probe).
        assert!(matches!(
            classify_probe(true, Some(0), b"something else\n", b""),
            ProbeOutcome::Unknown(_)
        ));
        // killed by a signal (no exit code) with no stderr → unknown with a generic reason.
        match classify_probe(false, None, b"", b"") {
            ProbeOutcome::Unknown(reason) => {
                assert!(
                    reason.contains("signal") && reason.contains("without OK"),
                    "{reason}"
                );
            }
            _ => panic!("a signalled probe is unknown, not expired"),
        }
    }

    #[test]
    fn stdout_is_ok_requires_the_exact_token_not_a_substring() {
        assert!(stdout_is_ok(b"OK"));
        assert!(stdout_is_ok(b"OK\n"));
        assert!(stdout_is_ok(b"  OK  \n"));
        assert!(stdout_is_ok(b"blah\nOK\nblah"));
        assert!(!stdout_is_ok(b"OKAY"), "a substring must not pass");
        assert!(!stdout_is_ok(b"not ok"));
        assert!(!stdout_is_ok(b""));
    }

    #[test]
    fn only_definite_auth_errors_hold_and_diagnostics_never_echo_secrets() {
        for diagnostic in [
            "API Error: 401 token=secret-canary",
            "OAuth session expired secret-canary",
            "Not logged in secret-canary",
            "Invalid API key secret-canary",
        ] {
            let ProbeOutcome::Dead(reason) =
                classify_probe(false, Some(1), b"", diagnostic.as_bytes())
            else {
                panic!("explicit credential refusal must hold");
            };
            assert!(!reason.contains("secret-canary"));
        }
        for diagnostic in [
            "network timeout secret-canary",
            "request 401 completed without a response",
        ] {
            let ProbeOutcome::Unknown(reason) =
                classify_probe(false, Some(1), b"", diagnostic.as_bytes())
            else {
                panic!("a transport failure is not an expired login");
            };
            assert!(!reason.contains("secret-canary"));
        }
    }

    #[test]
    fn apierror_status_401_is_a_definite_rejection_on_either_output_stream() {
        let diagnostic = b"APIError: status 401 token=secret-canary";
        for (stdout, stderr) in [
            (diagnostic.as_slice(), b"".as_slice()),
            (b"".as_slice(), diagnostic.as_slice()),
        ] {
            let ProbeOutcome::Dead(reason) = classify_probe(false, Some(1), stdout, stderr) else {
                panic!("an explicit APIError status 401 must hold, not dispatch as unknown");
            };
            assert_eq!(
                reason,
                "claude credential rejected (401/invalid key); refresh the login"
            );
            assert!(!reason.contains("secret-canary"));
        }
    }

    // --- backend gating (requirement 3) ------------------------------------------------------------

    #[test]
    fn backend_has_probe_only_claude() {
        assert!(backend_has_probe("claude"));
        assert!(!backend_has_probe("codex"));
        assert!(!backend_has_probe(""));
        assert!(!backend_has_probe("openai"));
    }

    // --- pure cache + logging helpers --------------------------------------------------------------

    #[test]
    fn healthy_and_fresh_respects_ttl_and_never_trusts_dead() {
        let t0 = fixed_now();
        let ttl = Duration::from_secs(300);
        let fresh = ProbeCache {
            checked_at: t0,
            healthy: true,
            last_logged_dead_at: None,
        };
        assert!(healthy_and_fresh(
            &fresh,
            t0 + chrono::Duration::seconds(60),
            ttl
        ));
        assert!(
            !healthy_and_fresh(&fresh, t0 + chrono::Duration::seconds(301), ttl),
            "a healthy verdict past the TTL is stale"
        );
        let dead = ProbeCache {
            checked_at: t0,
            healthy: false,
            last_logged_dead_at: Some(t0),
        };
        assert!(
            !healthy_and_fresh(&dead, t0, ttl),
            "a dead verdict is never fresh — always re-probe after a failure"
        );
    }

    #[test]
    fn dead_log_decision_logs_transitions_and_rate_limits_steady_state() {
        let t0 = fixed_now();
        let win = Duration::from_secs(300);

        // First-ever probe (no prior cache) that comes back dead → log.
        let (log, last) = dead_log_decision(None, t0, win);
        assert!(log, "the first dead verdict logs loudly");
        assert_eq!(last, Some(t0));

        // Transition from healthy → dead → log.
        let healthy = ProbeCache {
            checked_at: t0,
            healthy: true,
            last_logged_dead_at: None,
        };
        let (log, last) = dead_log_decision(Some(&healthy), t0, win);
        assert!(log, "healthy→dead is a loud transition");
        assert_eq!(last, Some(t0));

        // Steady-state dead within the window → suppress, preserving the last-logged instant.
        let dead = ProbeCache {
            checked_at: t0,
            healthy: false,
            last_logged_dead_at: Some(t0),
        };
        let (log, last) = dead_log_decision(Some(&dead), t0 + chrono::Duration::seconds(60), win);
        assert!(
            !log,
            "a steady-state repeat within the window is suppressed"
        );
        assert_eq!(last, Some(t0), "the last-logged instant is preserved");

        // Steady-state dead past the window → log again, advancing the last-logged instant.
        let later = t0 + chrono::Duration::seconds(301);
        let (log, last) = dead_log_decision(Some(&dead), later, win);
        assert!(log, "past the window the steady-state skip logs again");
        assert_eq!(last, Some(later));
    }

    // --- on_tick integration (requirements 1, 2, 4, 6) ---------------------------------------------

    enum FakeKind {
        Healthy,
        Dead,
        Hang,
    }

    struct FakeProbe {
        kind: FakeKind,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CredentialProbe for FakeProbe {
        async fn probe(&self, _req: &ProbeRequest) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.kind {
                FakeKind::Healthy => ProbeOutcome::Healthy,
                FakeKind::Dead => ProbeOutcome::Dead("expired login".to_string()),
                FakeKind::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!("a hanging probe never resolves")
                }
            }
        }
    }

    /// A legacy-path orchestrator with a fake credential probe and (optionally) one Todo candidate,
    /// wired exactly like the loop.rs on_tick tests. Returns the dispatch sink + the probe call counter.
    fn orch_with_probe(
        with_candidate: bool,
        kind: FakeKind,
    ) -> (Orchestrator, DispatchedEntries, Arc<AtomicUsize>) {
        let mut tr = Fake::new();
        if with_candidate {
            tr.candidates = vec![issue("1", "MT-1", "Todo")];
        }
        let mut eff = empty_effective(Arc::new(tr));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600); // no background ticks; the test drives on_tick
        eff.max_retry_backoff_ms = 300_000;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let calls = Arc::new(AtomicUsize::new(0));
        o.cred_probe = Some(Arc::new(FakeProbe {
            kind,
            calls: Arc::clone(&calls),
        }));
        let sink: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));
        (o, sink, calls)
    }

    async fn drive_tick(o: &mut Orchestrator) {
        // These ticks share tracing callsites with the log-capture test. Serializing every tick
        // keeps an unrelated runtime's empty subscriber from racing its interest-cache rebuild.
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        drive_tick_unlocked(o).await;
    }

    async fn drive_tick_unlocked(o: &mut Orchestrator) {
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort(); // stop the poll timer on_tick re-arms
        }
    }

    struct ScopedProbe {
        timeout: bool,
    }

    #[async_trait]
    impl CredentialProbe for ScopedProbe {
        async fn probe(&self, req: &ProbeRequest) -> ProbeOutcome {
            if req.billing_guard {
                if self.timeout {
                    std::future::pending::<()>().await;
                }
                ProbeOutcome::Dead("expired login".into())
            } else {
                ProbeOutcome::Healthy
            }
        }
    }

    fn mixed_accounts(
        timeout: bool,
    ) -> (Orchestrator, DispatchedEntries, crate::testsupport::TempDir) {
        let dir = crate::testsupport::TempDir::new();
        std::fs::write(
            dir.child("claude.md"),
            "---\nextends: swe\nharness: claude\n---\nClaude.\n",
        )
        .unwrap();
        std::fs::write(
            dir.child("chatgpt.md"),
            "---\nextends: swe\nharness: opencode\nmodel: openai/gpt-test\n---\nOpenCode.\n",
        )
        .unwrap();
        let (mut o, sink, _) = orch_with_probe(false, FakeKind::Healthy);
        let mut teams = rhapsody_config::teams::Teams {
            enabled: true,
            ..rhapsody_config::teams::Teams::disabled()
        };
        let eff = o.eff.as_mut().unwrap();
        eff.cfg.providers.clear();
        for (slug, backend, guard) in [
            ("subscription", "claude", true),
            ("api", "claude", false),
            ("chatgpt", "opencode", true),
        ] {
            let mut tr = Fake::new();
            let mut candidate = issue(slug, slug, "Todo");
            candidate.labels = Some(vec![format!("rhapsody:@{slug}")]);
            tr.candidates = vec![candidate];
            let mut p = empty_resolved_project(slug, Arc::new(tr));
            p.active_states = set_of(&["todo"]);
            p.max_concurrent = 10;
            p.mcfg.providers.clear();
            p.mcfg.claude.billing_guard = Some(guard);
            p.mcfg.opencode.model = "openai/gpt-test".into();
            eff.projects.push(p);
            teams.roster.push(rhapsody_config::teams::Identity {
                name: slug.into(),
                profile: if backend == "opencode" {
                    "chatgpt"
                } else {
                    "claude"
                }
                .into(),
                ..Default::default()
            });
        }
        o.teams = Some(teams);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("")));
        o.cred_probe = Some(Arc::new(ScopedProbe { timeout }));
        o.probe_timeout = Duration::from_millis(10);
        (o, sink, dir)
    }

    #[tokio::test]
    async fn scoped_timeout_other_accounts_still_dispatch() {
        let (mut o, sink, _dir) = mixed_accounts(true);
        drive_tick(&mut o).await;
        let entries = sink.lock().unwrap();
        assert!(entries.iter().any(|e| e.issue.id == "api"));
        assert!(entries.iter().any(|e| e.issue.id == "chatgpt"));
        assert_eq!(
            entries
                .iter()
                .find(|e| e.issue.id == "chatgpt")
                .unwrap()
                .harness,
            "opencode"
        );
    }

    #[tokio::test]
    async fn scoped_timeout_dispatches_affected_work() {
        let (mut o, sink, _dir) = mixed_accounts(true);
        drive_tick(&mut o).await;
        assert!(
            sink.lock()
                .unwrap()
                .iter()
                .any(|e| e.issue.id == "subscription")
        );
    }

    #[tokio::test]
    async fn scoped_expiry_holds_only_affected_account() {
        let (mut o, sink, _dir) = mixed_accounts(false);
        drive_tick(&mut o).await;
        let entries = sink.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.issue.id != "subscription"));
        assert!(!o.claimed.contains("subscription"));
        assert!(o.retry_attempts.is_empty());
        let accounts = o.control().accounts((o.now)().timestamp());
        let held = accounts
            .iter()
            .find(|a| a.account == "claude-subscription")
            .unwrap();
        assert_eq!(held.status, "credential_held");
        assert_eq!(held.level.as_deref(), Some("stop_new"));
        assert!(
            serde_json::to_value(held).unwrap()["probe_reason"]
                .as_str()
                .unwrap()
                .contains("expired")
        );
    }

    #[tokio::test]
    async fn scoped_apierror_status_401_holds_only_affected_account() {
        struct Status401Probe;

        #[async_trait]
        impl CredentialProbe for Status401Probe {
            async fn probe(&self, req: &ProbeRequest) -> ProbeOutcome {
                if req.billing_guard {
                    classify_probe(
                        false,
                        Some(1),
                        b"",
                        b"APIError: status 401 token=secret-canary",
                    )
                } else {
                    ProbeOutcome::Healthy
                }
            }
        }

        let (mut o, sink, _dir) = mixed_accounts(false);
        o.cred_probe = Some(Arc::new(Status401Probe));
        drive_tick(&mut o).await;
        let entries = sink.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.issue.id == "api"));
        assert!(entries.iter().any(|e| e.issue.id == "chatgpt"));
        assert!(entries.iter().all(|e| e.issue.id != "subscription"));
        assert!(!o.claimed.contains("subscription"));
        assert!(o.retry_attempts.is_empty());
        let accounts = o.control().accounts((o.now)().timestamp());
        let held = accounts
            .iter()
            .find(|a| a.account == "claude-subscription")
            .unwrap();
        assert_eq!(held.status, "credential_held");
        assert_eq!(held.level.as_deref(), Some("stop_new"));
        let wire = serde_json::to_value(held).unwrap();
        assert_eq!(
            wire["probe_reason"],
            "claude credential rejected (401/invalid key); refresh the login"
        );
        assert!(!wire.to_string().contains("secret-canary"));
    }

    #[tokio::test]
    async fn scoped_expiry_does_not_hold_another_harness_on_the_same_account() {
        let (mut o, sink, dir) = mixed_accounts(false);
        let eff = o.eff.as_mut().unwrap();
        let api = eff.projects.iter_mut().find(|p| p.slug == "api").unwrap();
        api.mcfg.opencode.model = "anthropic/claude-test".into();
        std::fs::write(dir.child("anthropic.md"), "---\nextends: swe\nharness: opencode\nmodel: anthropic/claude-test\n---\nOpenCode Anthropic.\n").unwrap();
        o.teams
            .as_mut()
            .unwrap()
            .roster
            .iter_mut()
            .find(|i| i.name == "api")
            .unwrap()
            .profile = "anthropic".into();
        let mut claude_api = empty_resolved_project("claude-api", Arc::new(Fake::new()));
        claude_api.mcfg.claude.billing_guard = Some(false);
        eff.projects.push(claude_api);
        // Explicitly reject Claude API auth; OpenCode's own Anthropic login remains independent.
        o.cred_probe = Some(Arc::new(FakeProbe {
            kind: FakeKind::Dead,
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        drive_tick(&mut o).await;
        assert!(sink.lock().unwrap().iter().any(|e| e.issue.id == "api"));
        assert!(o.credential_probe_held("claude", "claude-api"));
    }

    #[tokio::test]
    async fn scoped_command_reload_invalidates_a_healthy_cache() {
        let (mut o, _, calls) = orch_with_probe(false, FakeKind::Healthy);
        drive_tick(&mut o).await;
        o.eff.as_mut().unwrap().cfg.claude.command = "different-command".into();
        drive_tick(&mut o).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(o.probe_cache.len(), 1, "old context must be retired");
    }

    #[tokio::test]
    async fn scoped_repeated_timeouts_report_infrastructure_and_recover() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let (mut o, _, _dir) = mixed_accounts(true);
        for _ in 0..2 {
            drive_tick_unlocked(&mut o).await;
            assert!(o.build_snapshot().review_divergence.is_empty());
        }
        drive_tick_unlocked(&mut o).await;
        let report = o.build_snapshot().review_divergence;
        assert_eq!(report.len(), 1);
        assert!(report[0].reason.contains("claude-subscription"));
        assert!(report[0].reason.contains("claude"));
        assert!(report[0].reason.contains("infrastructure"));
        let wire = crate::snapshot_json::render(&o.build_snapshot());
        assert!(
            wire["review_divergence"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("3 consecutive timeouts")
        );
        assert!(events.lock().unwrap().iter().any(|e| {
            e.level == "WARN"
                && e.message.contains("infrastructure faults")
                && e.fields
                    .get("account")
                    .is_some_and(|a| a == "claude-subscription")
                && e.fields.get("harness").is_some_and(|h| h == "claude")
                && e.fields.get("timeouts").is_some_and(|t| t == "3")
        }));
        o.cred_probe = Some(Arc::new(FakeProbe {
            kind: FakeKind::Healthy,
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        drive_tick_unlocked(&mut o).await;
        assert!(o.build_snapshot().review_divergence.is_empty());
    }

    // Requirement: a probe that fails causes on_tick to SKIP dispatch — without claiming anything.
    #[tokio::test(flavor = "multi_thread")]
    async fn dead_probe_skips_dispatch_without_claiming() {
        let (mut o, sink, calls) = orch_with_probe(true, FakeKind::Dead);
        drive_tick(&mut o).await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a dead credential must skip dispatch"
        );
        assert!(
            o.claimed.is_empty(),
            "a skipped dispatch must claim nothing (no stranded claim wedges the project)"
        );
        assert!(o.running.is_empty(), "a skipped dispatch must start no run");
        assert!(
            o.retry_attempts.is_empty(),
            "a skipped dispatch must not touch the retry queue"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the probe ran once");
    }

    // Requirement: a probe that succeeds leaves dispatch behavior completely unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn healthy_probe_leaves_dispatch_unchanged() {
        let (mut o, sink, calls) = orch_with_probe(true, FakeKind::Healthy);
        drive_tick(&mut o).await;
        assert_eq!(
            sink.lock().expect("dispatch sink").len(),
            1,
            "a live credential dispatches the candidate exactly as before"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // Requirement: N ticks inside the TTL trigger exactly ONE probe invocation.
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_cached_within_ttl_runs_once() {
        let (mut o, _sink, calls) = orch_with_probe(true, FakeKind::Healthy);
        let now = fixed_now();
        o.now = Box::new(move || now); // pin the clock so all ticks fall within the TTL
        drive_tick(&mut o).await;
        drive_tick(&mut o).await;
        drive_tick(&mut o).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "three ticks within the TTL must trigger exactly one probe"
        );
    }

    // Requirement: re-probe immediately after a failure rather than waiting out the TTL.
    #[tokio::test(flavor = "multi_thread")]
    async fn dead_verdict_reprobes_every_tick() {
        let (mut o, _sink, calls) = orch_with_probe(true, FakeKind::Dead);
        let now = fixed_now();
        o.now = Box::new(move || now); // pinned clock: proves it is NOT the TTL forcing the re-probe
        drive_tick(&mut o).await;
        drive_tick(&mut o).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a dead verdict is not cached — each tick re-probes so recovery is detected fast"
        );
    }

    // A hanging probe is bounded and permits dispatch with an unknown credential state.
    #[tokio::test(flavor = "multi_thread")]
    async fn hanging_probe_dispatches_within_timeout() {
        let (mut o, sink, _calls) = orch_with_probe(true, FakeKind::Hang);
        o.probe_timeout = Duration::from_millis(50);
        let start = std::time::Instant::now();
        drive_tick(&mut o).await;
        let elapsed = start.elapsed();
        assert!(
            sink.lock().expect("dispatch sink").len() == 1,
            "a hanging probe is unknown and permits dispatch"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the probe timeout must bound the tick (got {elapsed:?})"
        );
    }

    // Requirement: a non-claude backend does not block dispatch (a probe-less backend is a no-op).
    #[tokio::test(flavor = "multi_thread")]
    async fn non_claude_backend_does_not_block_dispatch() {
        // Even a DEAD fake probe must be bypassed when the configured backend has no probe.
        let (mut o, _sink, calls) = orch_with_probe(false, FakeKind::Dead);
        o.eff.as_mut().expect("eff").cfg.agent.backend = "codex".to_string();
        // Seed a stale dead verdict (as if the backend had just hot-reloaded from claude).
        o.seed_dead_probe("claude", "");
        o.credential_preflight().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a codex backend must not invoke the claude probe at all"
        );
        assert!(
            o.probe_cache.is_empty(),
            "switching to a probe-less backend must clear a stale dead verdict"
        );
    }

    // Requirement 5 (state surface): a dead credential surfaces an operator advisory on the project
    // status (rendered on /api/v1/projects), and none appears while healthy.
    #[test]
    fn dead_credential_surfaces_project_advisory() {
        let tr: Arc<dyn rhapsody_tracker::Tracker> = Arc::new(Fake::new());
        let mut eff = empty_effective(Arc::clone(&tr));
        eff.projects = vec![empty_resolved_project("alpha", Arc::clone(&tr))];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);

        // No probe yet (healthy default) → no advisory.
        let before = o.project_statuses();
        assert_eq!(before.len(), 1);
        assert!(
            before[0]
                .warnings
                .iter()
                .all(|w| w != CREDENTIAL_DEAD_WARNING),
            "a healthy daemon surfaces no credential advisory"
        );

        // Mark the cached verdict dead → the advisory appears on the project status.
        o.seed_dead_probe("claude", "alpha");
        let after = o.project_statuses();
        assert!(
            after[0]
                .warnings
                .iter()
                .any(|w| w == CREDENTIAL_DEAD_WARNING),
            "a dead credential must surface an operator advisory on the project status"
        );
    }
}
