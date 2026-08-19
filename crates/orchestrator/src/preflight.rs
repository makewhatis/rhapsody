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
//! # The gate
//!
//! [`Orchestrator::on_tick`](crate::orchestrator::Orchestrator) already runs
//! `reconcile → validate() → fetch candidates → dispatch`, and on a `validate()` error it logs, skips
//! dispatch **without claiming anything**, and re-arms the timer — exactly the desired behavior for a
//! dead credential. This module adds the missing *check itself* at that same gate: a credential-liveness
//! probe run right after `validate()`, before any candidate fetch, so a dead credential skips dispatch
//! without ever holding a claim (see the on_tick call site and the "claims nothing" invariant below).
//!
//! # Design (the requirements this satisfies)
//!
//! * **Cache** — `validate()` runs every tick (`polling.interval_ms` is 30s); shelling out that often
//!   is unacceptable. A healthy verdict is cached for [`PROBE_TTL`] (probe at most once per TTL); a dead
//!   verdict is never cached past the tick, so recovery is detected on the very next tick rather than
//!   after waiting out the TTL.
//! * **Non-blocking** — each probe is bounded by [`Orchestrator::probe_timeout`], well under the poll
//!   interval; a hang fails closed ("cannot verify → skip dispatch"), never wedging the loop.
//! * **Backend-aware** — only the configured backend is probed ([`backend_has_probe`]); a backend with
//!   no probe (`codex`, or any future backend) is a clean no-op that never blocks dispatch.
//! * **Never holds a claim** — the check runs before candidate fetch; the existing `retry_queue` /
//!   `claims` behavior is untouched.
//! * **Legible skip** — transitions (healthy→dead, dead→healthy) log loudly; the steady-state dead
//!   repeat is rate-limited ([`DEAD_LOG_INTERVAL`]) so `/api/v1/logs` shows the cause without drowning.
//!   The dead condition also surfaces as a per-project advisory on `/api/v1/projects`
//!   ([`CREDENTIAL_DEAD_WARNING`]).
//!
//! # Same scrubbed environment as the children
//!
//! When `claude.billing_guard` is on, the runner scrubs `CLAUDE_CODE_OAUTH_TOKEN` /
//! `ANTHROPIC_API_KEY` / … from every dispatched child, which then authenticates via the tokenless
//! Keychain path. The probe MUST exercise that SAME credential, or it could report healthy while every
//! child still dies (or the reverse). [`scrub_child_env`] therefore reuses the runner's exact
//! `scrub_env` + `scrubbed_env_vars` + `TRACKER_ENV_VARS` primitives, honoring the effective
//! `billing_guard` (only the per-issue "me" identity is omitted — a probe has no issue).

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
/// this is treated as "cannot verify → skip dispatch" (fail closed). It is a field on the orchestrator
/// ([`Orchestrator::probe_timeout`]) so tests can shrink it; this is the production default.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// While the credential stays dead, the steady-state skip is logged at most once per this window so
/// `/api/v1/logs` shows the cause without a line every 30s forever. Transitions (healthy→dead,
/// dead→healthy) always log loudly regardless of this rate limit.
const DEAD_LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The operator advisory surfaced on each project's `/api/v1/projects` status while the credential
/// probe reports dead (the state-visible half of the "legible skip" requirement).
pub(crate) const CREDENTIAL_DEAD_WARNING: &str =
    "agent credential probe failing — dispatch paused until the login is refreshed";

/// The inputs a credential probe needs, captured from the effective config at probe time so a
/// hot-reloaded command / billing_guard / key is honored. The credential is host-global, so these come
/// from the top-level (legacy) config; per-project claude overrides share the same host login.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    /// The configured agent backend (`claude` / `codex`). Only a backend with a probe is probed.
    pub backend: String,
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
    /// The credential is dead / unverifiable; the string is the operator-facing reason for the skip log.
    Dead(String),
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

/// The scrubbed environment the credential probe runs with: identical to the per-turn scrub the claude
/// runner applies to its children (`runner.rs`) — the tracker vars are ALWAYS dropped (by name and by
/// value) and the billing/routing vars are dropped when the guard is on — MINUS the per-issue "me"
/// identity, which a credential probe has no issue for. Reusing the runner's exact `scrub_env` +
/// `scrubbed_env_vars` + `TRACKER_ENV_VARS` primitives guarantees the probe authenticates via the SAME
/// credential path the dispatched children do.
pub(crate) fn scrub_child_env(
    base_env: &[String],
    billing_guard: bool,
    tracker_api_key: &str,
) -> Vec<String> {
    let drop_names: Vec<&str> = if billing_guard {
        scrubbed_env_vars()
    } else {
        TRACKER_ENV_VARS.to_vec()
    };
    scrub_env(base_env, &drop_names, &[tracker_api_key])
}

/// The current process environment as `KEY=VALUE` strings (mirrors the runner's `base_env` capture).
fn process_env() -> Vec<String> {
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
            Err(e) => {
                return ProbeOutcome::Dead(format!(
                    "invalid claude command {:?}: {e}",
                    req.command
                ));
            }
        };
        let env = scrub_child_env(&process_env(), req.billing_guard, &req.tracker_api_key);

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
        // Reap the child if the caller's timeout drops this future mid-probe (fail-closed cleanup).
        cmd.kill_on_drop(true);

        match cmd.output().await {
            Ok(out) => classify_probe(
                out.status.success(),
                out.status.code(),
                &out.stdout,
                &out.stderr,
            ),
            Err(e) => ProbeOutcome::Dead(format!("could not launch claude probe: {e}")),
        }
    }
}

/// Derives the liveness verdict from a completed probe process (pure, so the core "exit 0 with `OK` =
/// live, else dead" contract is unit-tested without spawning a real `claude`). Live iff the process
/// exited 0 AND printed the `OK` reply on stdout; otherwise dead, with an operator-facing reason built
/// from the exit code + a trimmed stderr tail (the OAuth-expired incident ends on a verbatim stderr
/// line). Taking primitives (not a `std::process::Output`, whose `ExitStatus` has no portable test
/// constructor) keeps it directly testable.
fn classify_probe(success: bool, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> ProbeOutcome {
    if success && stdout_is_ok(stdout) {
        ProbeOutcome::Healthy
    } else {
        ProbeOutcome::Dead(probe_failure_reason(code, stderr))
    }
}

/// Whether the probe's stdout carries the `OK` reply — the exact token on some line (trimmed), NOT a
/// substring (so a banner like `OKAY`/`not ok` never passes).
fn stdout_is_ok(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|l| l.trim() == PROBE_OK)
}

/// A concise operator-facing reason for a failed probe: the exit status plus a trimmed tail of stderr.
fn probe_failure_reason(code: Option<i32>, stderr: &[u8]) -> String {
    let code = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let stderr = String::from_utf8_lossy(stderr);
    let trimmed = stderr.trim();
    let n = trimmed.chars().count();
    let tail: String = trimmed.chars().skip(n.saturating_sub(400)).collect();
    if tail.is_empty() {
        format!("claude probe exited {code} without OK")
    } else {
        format!("claude probe exited {code}: {tail}")
    }
}

/// The cached credential-probe verdict for the dispatch preflight. Control-task-owned (only on_tick's
/// [`Orchestrator::credential_preflight`] mutates it), so it needs no lock.
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

impl Orchestrator {
    /// Installs the production credential-liveness probe (BO-59). The daemon calls this once at startup;
    /// without it the credential preflight is a no-op and dispatch is byte-identical to the pre-feature
    /// behavior (the default for tests and any non-production build).
    pub fn set_credential_probe(&mut self, probe: Arc<dyn CredentialProbe>) {
        self.cred_probe = Some(probe);
    }

    /// Builds the probe request from the current effective config (top-level / legacy path). `None` when
    /// no config is loaded. The credential is host-global, so the top-level claude command +
    /// billing_guard + tracker key are used (per-project claude overrides share the same host login).
    fn probe_request(&self) -> Option<ProbeRequest> {
        let eff = self.eff.as_ref()?;
        Some(ProbeRequest {
            backend: eff.cfg.agent.backend.clone(),
            command: eff.cfg.claude.command.clone(),
            billing_guard: billing_guard_enabled(eff.cfg.claude.billing_guard),
            tracker_api_key: eff.cfg.tracker.api_key.clone(),
        })
    }

    /// Whether the credential probe currently reports the backend dead — read by `project_statuses` to
    /// surface [`CREDENTIAL_DEAD_WARNING`] on `/api/v1/projects` while dispatch is paused.
    pub(crate) fn credential_probe_dead(&self) -> bool {
        self.probe_cache.as_ref().is_some_and(|c| !c.healthy)
    }

    /// The dispatch credential-liveness preflight (BO-59): returns `true` when dispatch may proceed.
    /// Runs at the existing on_tick gate (right after `validate()`), BEFORE any candidate fetch, so a
    /// dead credential skips dispatch WITHOUT claiming anything. Caches a healthy verdict for
    /// [`PROBE_TTL`] (probing at most once per TTL) and re-probes immediately after a failure; bounds
    /// each probe with [`Orchestrator::probe_timeout`] (a hang fails closed); only probes a backend that
    /// has a probe (a probe-less backend never blocks dispatch); and logs transitions loudly while
    /// rate-limiting the steady-state dead repeat.
    pub(crate) async fn credential_preflight(&mut self) -> bool {
        // Seam absent → the feature is off; dispatch unchanged (all existing tests + non-prod builds).
        let Some(probe) = self.cred_probe.clone() else {
            return true;
        };
        let Some(req) = self.probe_request() else {
            return true; // no config loaded (defensive; production always has one after reload)
        };
        // Backend without a probe (codex, …) → clean no-op; never blocks dispatch. Clear any cached
        // verdict so a stale dead reading from a prior claude config can't linger (e.g. after a
        // hot-reload from claude to codex) and surface a false advisory.
        if !backend_has_probe(&req.backend) {
            self.probe_cache = None;
            return true;
        }
        let now = (self.now)();
        // Trust a fresh healthy verdict (cache). A dead verdict is never fresh → re-probe every tick.
        if let Some(cache) = &self.probe_cache
            && healthy_and_fresh(cache, now, PROBE_TTL)
        {
            return true;
        }
        // Probe, bounded by the timeout that fails closed on a hang.
        let outcome = match tokio::time::timeout(self.probe_timeout, probe.probe(&req)).await {
            Ok(o) => o,
            Err(_) => ProbeOutcome::Dead(format!(
                "credential probe did not answer within {:?}; cannot verify — skipping dispatch",
                self.probe_timeout
            )),
        };
        let prev_healthy = self.probe_cache.as_ref().map(|c| c.healthy);
        match outcome {
            ProbeOutcome::Healthy => {
                if prev_healthy == Some(false) {
                    tracing::warn!(
                        "agent credential recovered; dispatch resuming (BO-59 dispatch preflight)"
                    );
                }
                self.probe_cache = Some(ProbeCache {
                    checked_at: now,
                    healthy: true,
                    last_logged_dead_at: None,
                });
                true
            }
            ProbeOutcome::Dead(reason) => {
                let (should_log, last_logged) =
                    dead_log_decision(self.probe_cache.as_ref(), now, DEAD_LOG_INTERVAL);
                if should_log {
                    tracing::error!(
                        reason = %reason,
                        "agent credential probe FAILED; skipping ALL dispatch this tick — an expired \
                         login fails fast instead of burning a run every ~5 min (BO-59 dispatch preflight)"
                    );
                }
                self.probe_cache = Some(ProbeCache {
                    checked_at: now,
                    healthy: false,
                    last_logged_dead_at: last_logged,
                });
                false
            }
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
        let scrubbed = scrub_child_env(&base, true, "");
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
        let scrubbed = scrub_child_env(&base, false, "lin");
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
        // The incident: a NON-ZERO exit is dead even if stdout somehow contained OK, and the reason
        // carries the exit code + the stderr tail an operator needs.
        match classify_probe(
            false,
            Some(1),
            b"OK\n",
            b"OAuth session expired and could not be refreshed\n",
        ) {
            ProbeOutcome::Dead(reason) => {
                assert!(
                    reason.contains("exited 1"),
                    "reason names the exit code: {reason}"
                );
                assert!(
                    reason.contains("OAuth session expired"),
                    "reason carries the stderr tail: {reason}"
                );
            }
            ProbeOutcome::Healthy => panic!("a non-zero exit must be classified dead"),
        }
        // exit 0 but stdout lacks the OK token → dead (a broken / differently-behaving probe).
        assert!(matches!(
            classify_probe(true, Some(0), b"something else\n", b""),
            ProbeOutcome::Dead(_)
        ));
        // killed by a signal (no exit code) with no stderr → dead with a generic reason.
        match classify_probe(false, None, b"", b"") {
            ProbeOutcome::Dead(reason) => {
                assert!(
                    reason.contains("signal") && reason.contains("without OK"),
                    "{reason}"
                );
            }
            ProbeOutcome::Healthy => panic!("a signalled probe must be dead"),
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
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort(); // stop the poll timer on_tick re-arms
        }
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

    // Requirement: a probe that hangs is bounded by its timeout and fails closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn hanging_probe_fails_closed_within_timeout() {
        let (mut o, sink, _calls) = orch_with_probe(true, FakeKind::Hang);
        o.probe_timeout = Duration::from_millis(50);
        let start = std::time::Instant::now();
        drive_tick(&mut o).await;
        let elapsed = start.elapsed();
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a hanging probe must fail closed (skip dispatch)"
        );
        assert!(o.claimed.is_empty(), "a fail-closed skip claims nothing");
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
        o.probe_cache = Some(ProbeCache {
            checked_at: (o.now)(),
            healthy: false,
            last_logged_dead_at: None,
        });
        assert!(
            o.credential_preflight().await,
            "a probe-less backend (codex) must never block dispatch"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a codex backend must not invoke the claude probe at all"
        );
        assert!(
            !o.credential_probe_dead(),
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
        o.probe_cache = Some(ProbeCache {
            checked_at: (o.now)(),
            healthy: false,
            last_logged_dead_at: None,
        });
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
