//! `onepassword` — the daemon-side 1Password service-account source and health probe (STUDIO-1584).
//! Rhapsody-only; no Go counterpart.
//!
//! The daemon never stores a service-account token itself. The desktop app owns the system credential
//! store and delivers each bound token to this process over the same authenticated, secret-safe
//! channel the provider credentials use; this module is the daemon-side seam that resolves a bound
//! **id** to a token at dispatch, and probes the credential's health with `op whoami` off the control
//! loop.
//!
//! Two invariants hold everywhere here:
//!
//! * a token is only ever handed to [`rhapsody_agent::Session::set_onepassword_token`], which puts it
//!   in the agent subprocess env and nowhere else — never logged, recorded, or returned by an API;
//! * the biometric-disable variable is set on every run (see `rhapsody_agent::onepassword`), so an
//!   `op` call with no bound token fails fast instead of prompting.

use std::time::Duration;

use async_trait::async_trait;

pub use rhapsody_agent::onepassword::{
    OP_BIOMETRIC_UNLOCK_DISABLED, OP_BIOMETRIC_UNLOCK_ENABLED_ENV, OP_SERVICE_ACCOUNT_TOKEN_ENV,
};

/// How long a single `op whoami` health probe may run before it is abandoned as an infrastructure
/// fault. Matches the credential preflight's own hard bound (`preflight::PROBE_TIMEOUT`): a probe
/// that never returns must not strand a dispatch.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The default `op` command the probe runs. Overridable so tests can point it at a fake script.
pub const DEFAULT_OP_COMMAND: &str = "op";

/// The outcome of resolving a bound credential's token at dispatch.
#[derive(Debug)]
pub enum OnePasswordRead {
    /// The token is present; hand it to the run's agent env.
    Present(String),
    /// The project binds an id but no token is stored for it.
    Absent,
    /// The credential owner refused or is unavailable; the string is a non-secret reason.
    Unavailable(String),
}

/// Resolves a bound 1Password credential id to its service-account token. The production
/// implementation reads over the authenticated desktop channel; tests inject a fake.
#[async_trait]
pub trait OnePasswordSource: Send + Sync {
    async fn read(&self, id: &str) -> OnePasswordRead;
}

/// The classified result of an `op whoami` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnePasswordHealth {
    /// The credential authenticated. `account_type`/`vaults`/`expires_at` are whatever `op whoami`
    /// reported; a field it does not expose is empty/`None` rather than guessed.
    Ok {
        account_type: String,
        vaults: Vec<String>,
        expires_at: Option<String>,
    },
    /// The token was rejected (an auth failure, a non-zero exit). Holds the run.
    Rejected(String),
    /// No token was bound for the project.
    Absent,
    /// The probe did not finish within its bound — an infrastructure fault, never a credential
    /// failure (mirrors the credential preflight's timeout rule).
    Timeout,
}

/// Runs `op whoami --format=json` with a hard timeout and classifies the result. `token` is the
/// bound service-account token (or `None` to probe without one — the probe still disables biometric
/// unlock, so it fails fast rather than prompting). The child is killed on drop and given the
/// 1Password env, so it can never fall back to the operator's desktop integration.
pub async fn probe_whoami(
    command: &str,
    token: Option<&str>,
    timeout: Duration,
) -> OnePasswordHealth {
    let mut cmd = tokio::process::Command::new(command);
    cmd.arg("whoami").arg("--format=json");
    cmd.kill_on_drop(true);
    cmd.env(
        OP_BIOMETRIC_UNLOCK_ENABLED_ENV,
        OP_BIOMETRIC_UNLOCK_DISABLED,
    );
    match token {
        Some(t) if !t.is_empty() => {
            cmd.env(OP_SERVICE_ACCOUNT_TOKEN_ENV, t);
        }
        _ => {
            cmd.env_remove(OP_SERVICE_ACCOUNT_TOKEN_ENV);
        }
    }
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        // The process could not be spawned at all (no `op` on PATH): treat as an infra fault, the
        // same class as a timeout, so a missing CLI never masquerades as a rejected credential.
        Ok(Err(_)) => return OnePasswordHealth::Timeout,
        Err(_) => return OnePasswordHealth::Timeout,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return OnePasswordHealth::Rejected(redact_reason(&stderr));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout) else {
        // A zero exit with unparseable output is a successful auth whose details we could not read;
        // report Ok with empty details rather than inventing a rejection.
        return OnePasswordHealth::Ok {
            account_type: String::new(),
            vaults: Vec::new(),
            expires_at: None,
        };
    };
    OnePasswordHealth::Ok {
        account_type: whoami_account_type(&value),
        vaults: whoami_vaults(&value),
        expires_at: whoami_expiry(&value),
    }
}

/// The account type `op whoami --format=json` reports, read defensively from the shapes the CLI has
/// used (`type` / `account.type`).
fn whoami_account_type(v: &serde_json::Value) -> String {
    v.get("type")
        .and_then(|t| t.as_str())
        .or_else(|| {
            v.get("account")
                .and_then(|a| a.get("type"))
                .and_then(|t| t.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// The vault names a service-account `op whoami` may report (an array of names or of objects with a
/// `name`). Absent on CLI versions that do not list vaults, which yields an empty list.
fn whoami_vaults(v: &serde_json::Value) -> Vec<String> {
    let Some(arr) = v.get("vaults").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            item.as_str().map(str::to_string).or_else(|| {
                item.get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
            })
        })
        .collect()
}

/// A token expiry, if `op whoami` exposes one (`expires_at` / `expiresAt`). Service-account tokens
/// are minted with an expiry but not every CLI version reports it; `None` means "not readable".
fn whoami_expiry(v: &serde_json::Value) -> Option<String> {
    v.get("expires_at")
        .or_else(|| v.get("expiresAt"))
        .and_then(|e| e.as_str())
        .map(str::to_string)
}

/// A short, non-secret reason from the CLI's stderr: the first non-empty line, trimmed and capped.
/// A credential value can never appear here because `op` never echoes the token back.
fn redact_reason(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("1Password CLI exited non-zero");
    let mut reason = line.to_string();
    if reason.len() > 400 {
        reason.truncate(400);
    }
    reason
}

/// The daemon's source backed by an in-memory `id → token` map delivered once from the desktop app.
/// This is the production source when the daemon is spawned by the desktop supervisor; a bare CLI
/// run has no map and resolves every id as [`OnePasswordRead::Absent`]. The map is never written to
/// disk, logged, or recorded.
#[derive(Debug, Default)]
pub struct MapOnePasswordSource {
    tokens: std::collections::HashMap<String, String>,
}

impl MapOnePasswordSource {
    pub fn new(tokens: std::collections::HashMap<String, String>) -> MapOnePasswordSource {
        MapOnePasswordSource { tokens }
    }
}

#[async_trait]
impl OnePasswordSource for MapOnePasswordSource {
    async fn read(&self, id: &str) -> OnePasswordRead {
        match self.tokens.get(id) {
            Some(token) if !token.is_empty() => OnePasswordRead::Present(token.clone()),
            _ => OnePasswordRead::Absent,
        }
    }
}

/// The reason a run is held when its bound credential cannot be used. Names the project so the
/// operator knows which binding to fix, never the token.
pub fn hold_reason(project_slug: &str, health: &OnePasswordHealth) -> Option<String> {
    match health {
        OnePasswordHealth::Rejected(reason) => Some(format!(
            "1Password: token rejected for project {project_slug} ({reason})"
        )),
        OnePasswordHealth::Absent => Some(format!(
            "1Password: no service account bound for project {project_slug}"
        )),
        // Ok and Timeout are not credential failures: a timeout is an infrastructure fault that must
        // not strand the run (mirrors the credential preflight's repeated-timeout rule).
        OnePasswordHealth::Ok { .. } | OnePasswordHealth::Timeout => None,
    }
}

/// A recorded hold on a project whose bound 1Password credential is unusable (STUDIO-1584). Held on
/// the control task until a probe succeeds (or the credential becomes absent-then-present), so a bad
/// credential HOLDS the ticket instead of failing it terminally until the daemon restarts. Carries
/// the last probe's non-secret details so the operator can see WHY without reading the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnePasswordHold {
    /// The operator-facing reason (names the project, never the token).
    pub reason: String,
    /// The probe's account type, if it reported one.
    pub account_type: String,
    /// The vaults the credential can read, if the probe listed them.
    pub vaults: Vec<String>,
    /// The credential's expiry, if the probe exposed one.
    pub expires_at: Option<String>,
}

impl OnePasswordHold {
    /// Build a hold from a probe that does not authenticate, or `None` when the health does not
    /// warrant a hold (a healthy credential, or an infrastructure timeout).
    pub fn from_health(project_slug: &str, health: &OnePasswordHealth) -> Option<OnePasswordHold> {
        let reason = hold_reason(project_slug, health)?;
        let (account_type, vaults, expires_at) = match health {
            OnePasswordHealth::Ok {
                account_type,
                vaults,
                expires_at,
            } => (account_type.clone(), vaults.clone(), expires_at.clone()),
            _ => (String::new(), Vec::new(), None),
        };
        Some(OnePasswordHold {
            reason,
            account_type,
            vaults,
            expires_at,
        })
    }
}

impl std::fmt::Display for OnePasswordHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// How many days before a credential's expiry the operator should be warned (STUDIO-1584). Service
/// accounts are minted with `--expires-in 90d`, so a 14-day warning leaves time to mint a
/// replacement before the current one lapses and starts holding runs.
pub const EXPIRY_WARN_DAYS: i64 = 14;

/// Whether `expires_at` (an RFC3339-ish string `op whoami` reported) is within
/// [`EXPIRY_WARN_DAYS`] of `now`, or already past. `None` when the expiry is unreadable, which is
/// never a warning (an unreadable expiry must not manufacture one).
pub fn expiry_within_warning(expires_at: &str, now: chrono::DateTime<chrono::Utc>) -> Option<bool> {
    let parsed = chrono::DateTime::parse_from_rfc3339(expires_at).ok()?;
    let delta = parsed.with_timezone(&chrono::Utc) - now;
    Some(delta.num_days() <= EXPIRY_WARN_DAYS)
}

/// Whether the probe's account type names a credential with WRITE access. The ticket's out-of-scope
/// note says a binding with write permission should be WARNED about (writes stay human). `op whoami`
/// reports the account type; a service account is read-scoped by construction, so anything that is
/// not a service-account type is flagged for the operator to check.
pub fn account_type_looks_writable(account_type: &str) -> bool {
    let lowered = account_type.to_ascii_lowercase();
    !lowered.is_empty() && !lowered.contains("service")
}

impl crate::Orchestrator {
    /// Record a hold for `project_slug` (STUDIO-1584). Loop-confined: written by the worker-exit
    /// path when a dispatch is refused for an unusable credential.
    pub(crate) fn record_onepassword_hold(&mut self, project_slug: &str, hold: OnePasswordHold) {
        if project_slug.is_empty() {
            return;
        }
        self.onepassword_holds
            .insert(project_slug.to_string(), hold);
    }

    /// The hold on `project_slug`'s bound credential, if any.
    pub(crate) fn onepassword_project_hold(&self, project_slug: &str) -> Option<&OnePasswordHold> {
        self.onepassword_holds.get(project_slug)
    }

    /// The reconciliation sweep's feed for held 1Password credentials (STUDIO-1584, AC6): one
    /// `CredentialInfrastructure` divergence per held project, so an unusable credential shows up in
    /// `/api/v1/state`'s `review_divergence` and the per-project advisory instead of an unexplained
    /// stall. Empty on a healthy daemon, which keeps the wire payload — and the golden — unchanged.
    pub(crate) fn onepassword_human_feed(&self) -> Vec<crate::reviewreconcile::Divergence> {
        self.onepassword_holds
            .iter()
            .map(|(slug, hold)| crate::reviewreconcile::Divergence {
                pr: format!("1Password credential for project {slug}"),
                kind: crate::reviewreconcile::DivergenceKind::CredentialInfrastructure,
                ticket: String::new(),
                reviewer: String::new(),
                stale_secs: 0,
                auto_merge_reason: None,
                capacity_held: None,
                capacity_unreadable: None,
                adjudicated_head: String::new(),
                current_head: String::new(),
                rounds: 0,
                findings: Vec::new(),
                reason: hold.reason.clone(),
            })
            .collect()
    }

    /// Re-probe every held project's bound credential and RELEASE the hold when a probe
    /// authenticates (STUDIO-1584). Mirrors `credential_preflight`: the probes run concurrently as
    /// tasks and the control task awaits the join, so a probe that never returns is bounded by
    /// [`PROBE_TIMEOUT`] rather than stranding the daemon. A timeout is an infrastructure fault and
    /// keeps the hold; only a healthy `op whoami` releases it. A healthy probe also warns when the
    /// credential looks writable or is within [`EXPIRY_WARN_DAYS`] of expiry.
    pub(crate) async fn onepassword_preflight(&mut self) {
        if self.onepassword_holds.is_empty() {
            return;
        }
        let Some(source) = self.onepassword_source.clone() else {
            // No desktop channel: a bound id can never resolve, so the hold can only stand until a
            // token is stored. Nothing to probe.
            return;
        };
        // Snapshot the (project_slug, credential id) pairs for held projects.
        let held: Vec<(String, String)> = self
            .eff
            .as_ref()
            .map(|eff| {
                eff.projects
                    .iter()
                    .filter(|p| self.onepassword_holds.contains_key(&p.slug))
                    .filter_map(|p| p.onepassword.clone().map(|id| (p.slug.clone(), id)))
                    .collect()
            })
            .unwrap_or_default();
        if held.is_empty() {
            return;
        }
        let command = self.onepassword_op_command.clone();
        let mut tasks = tokio::task::JoinSet::new();
        for (slug, id) in held {
            let source = source.clone();
            let command = command.clone();
            tasks.spawn(async move {
                let read = source.read(&id).await;
                let token = match read {
                    OnePasswordRead::Present(t) if !t.is_empty() => Some(t),
                    _ => None,
                };
                let health = match &token {
                    Some(t) => probe_whoami(&command, Some(t), PROBE_TIMEOUT).await,
                    None => OnePasswordHealth::Absent,
                };
                (slug, health)
            });
        }
        let now = chrono::Utc::now();
        while let Some(result) = tasks.join_next().await {
            let Ok((slug, health)) = result else {
                continue;
            };
            match &health {
                OnePasswordHealth::Ok {
                    account_type,
                    expires_at,
                    ..
                } => {
                    self.onepassword_holds.remove(&slug);
                    tracing::warn!(
                        project_slug = %slug,
                        "1Password: the bound credential recovered; releasing the hold"
                    );
                    self.schedule_tick(Duration::ZERO);
                    if account_type_looks_writable(account_type) {
                        tracing::warn!(
                            project_slug = %slug,
                            account_type = %account_type,
                            "1Password: the bound credential is not a read-only service account"
                        );
                    }
                    if let Some(expires_at) = expires_at
                        && let Some(true) = expiry_within_warning(expires_at, now)
                    {
                        tracing::warn!(
                            project_slug = %slug,
                            expires_at = %expires_at,
                            "1Password: the bound service-account credential expires within {} \
                             days; mint a replacement",
                            EXPIRY_WARN_DAYS
                        );
                    }
                }
                OnePasswordHealth::Timeout => {
                    tracing::warn!(
                        project_slug = %slug,
                        "1Password: the health probe timed out; keeping the hold (infrastructure \
                         fault, not a credential failure)"
                    );
                }
                OnePasswordHealth::Rejected(_) | OnePasswordHealth::Absent => {
                    if let Some(hold) = OnePasswordHold::from_health(&slug, &health) {
                        self.onepassword_holds.insert(slug.clone(), hold);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    fn fake_op(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("op");
        let mut f = std::fs::File::create(&path).expect("create fake op");
        f.write_all(body.as_bytes()).expect("write fake op");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string_lossy().into_owned()
    }

    fn tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rd-op-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&p).expect("tmp dir");
        p
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[tokio::test]
    async fn a_successful_whoami_reports_type_vaults_and_expiry() {
        let dir = tmp();
        let script = fake_op(
            &dir,
            "#!/usr/bin/env bash\necho '{\"type\":\"SERVICE_ACCOUNT\",\"vaults\":[\"Studio49\"],\"expires_at\":\"2027-01-01T00:00:00Z\"}'\n",
        );
        let health = probe_whoami(&script, Some("ops_x"), PROBE_TIMEOUT).await;
        assert_eq!(
            health,
            OnePasswordHealth::Ok {
                account_type: "SERVICE_ACCOUNT".into(),
                vaults: vec!["Studio49".into()],
                expires_at: Some("2027-01-01T00:00:00Z".into()),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_a_rejection_with_a_redacted_reason() {
        let dir = tmp();
        let script = fake_op(
            &dir,
            "#!/usr/bin/env bash\necho '[ERROR] authentication failed: invalid token' >&2\nexit 1\n",
        );
        let health = probe_whoami(&script, Some("ops_x"), PROBE_TIMEOUT).await;
        match health {
            OnePasswordHealth::Rejected(reason) => {
                assert!(reason.contains("authentication failed"), "{reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_probe_that_never_returns_times_out_as_an_infra_fault() {
        let dir = tmp();
        let script = fake_op(&dir, "#!/usr/bin/env bash\nsleep 30\n");
        let health = probe_whoami(&script, Some("ops_x"), Duration::from_millis(200)).await;
        assert_eq!(health, OnePasswordHealth::Timeout);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_missing_command_is_an_infra_fault_not_a_rejection() {
        let health = probe_whoami("/nonexistent/op-xyz", Some("ops_x"), PROBE_TIMEOUT).await;
        assert_eq!(health, OnePasswordHealth::Timeout);
    }

    #[test]
    fn hold_reason_names_the_project_and_never_the_token() {
        let rejected = OnePasswordHealth::Rejected("invalid token".into());
        let reason = hold_reason("studio-infra", &rejected).expect("rejected holds");
        assert!(reason.contains("studio-infra"), "{reason}");
        assert!(!reason.contains("ops_"), "must not carry a token: {reason}");

        let absent = hold_reason("studio-infra", &OnePasswordHealth::Absent).expect("absent holds");
        assert!(absent.contains("no service account bound"), "{absent}");

        assert!(
            hold_reason("studio-infra", &OnePasswordHealth::Timeout).is_none(),
            "a probe timeout is an infra fault, not a hold"
        );
        assert!(
            hold_reason(
                "studio-infra",
                &OnePasswordHealth::Ok {
                    account_type: "SERVICE_ACCOUNT".into(),
                    vaults: vec![],
                    expires_at: None,
                }
            )
            .is_none(),
            "a healthy probe must not hold"
        );
    }

    #[tokio::test]
    async fn the_map_source_resolves_present_absent_and_empty() {
        let mut map = std::collections::HashMap::new();
        map.insert("studio-infra".to_string(), "ops_sentinel".to_string());
        map.insert("empty".to_string(), String::new());
        let src = MapOnePasswordSource::new(map);
        match src.read("studio-infra").await {
            OnePasswordRead::Present(t) => assert_eq!(t, "ops_sentinel"),
            other => panic!("expected Present, got {other:?}"),
        }
        assert!(matches!(src.read("empty").await, OnePasswordRead::Absent));
        assert!(matches!(src.read("missing").await, OnePasswordRead::Absent));
    }

    #[test]
    fn expiry_within_warning_only_flags_a_readable_near_expiry() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-30T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // 10 days out: within the 14-day window.
        assert_eq!(
            expiry_within_warning("2026-11-09T00:00:00Z", now),
            Some(true)
        );
        // 40 days out: not yet.
        assert_eq!(
            expiry_within_warning("2026-12-09T00:00:00Z", now),
            Some(false)
        );
        // Unreadable is never a warning.
        assert_eq!(expiry_within_warning("not a date", now), None);
    }

    #[test]
    fn account_type_looks_writable_flags_non_service_accounts() {
        assert!(!account_type_looks_writable("SERVICE_ACCOUNT"));
        assert!(!account_type_looks_writable(""));
        assert!(account_type_looks_writable("INDIVIDUAL"));
        assert!(account_type_looks_writable("BUSINESS"));
    }

    // --- Hold + release (STUDIO-1584 review B3) -------------------------------------------------

    /// An orchestrator with one project bound to `id`, a source holding `token`, and a fake `op`.
    fn orch_holding(
        id: &str,
        token: Option<&str>,
        op_body: &str,
    ) -> (crate::Orchestrator, std::path::PathBuf) {
        use crate::testsupport::{empty_effective, empty_resolved_project};
        let mut eff = empty_effective(Arc::new(rhapsody_tracker::fake::Fake::new()));
        let mut proj = empty_resolved_project(
            "studio-infra",
            Arc::new(rhapsody_tracker::fake::Fake::new()),
        );
        proj.onepassword = Some(id.to_string());
        eff.projects = vec![proj];
        let mut o = crate::Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let mut map = std::collections::HashMap::new();
        if let Some(token) = token {
            map.insert(id.to_string(), token.to_string());
        }
        o.onepassword_source = Some(Arc::new(MapOnePasswordSource::new(map)));
        let dir = tmp();
        let script = fake_op(&dir, op_body);
        o.onepassword_op_command = script;
        o.record_onepassword_hold(
            "studio-infra",
            OnePasswordHold {
                reason: "1Password: token rejected for project studio-infra (stale)".into(),
                account_type: String::new(),
                vaults: Vec::new(),
                expires_at: None,
            },
        );
        (o, dir)
    }

    #[tokio::test]
    async fn a_successful_probe_releases_the_hold() {
        let (mut o, dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\necho '{\"type\":\"SERVICE_ACCOUNT\"}'\n",
        );
        assert!(o.onepassword_project_hold("studio-infra").is_some());
        o.onepassword_preflight().await;
        assert!(
            o.onepassword_project_hold("studio-infra").is_none(),
            "a healthy probe must release the hold"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_rejected_probe_keeps_the_hold() {
        let (mut o, dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\necho '[ERROR] authentication failed' >&2\nexit 1\n",
        );
        o.onepassword_preflight().await;
        let hold = o
            .onepassword_project_hold("studio-infra")
            .expect("a rejected probe keeps the hold");
        assert!(
            hold.reason
                .contains("token rejected for project studio-infra"),
            "{hold}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_absent_credential_keeps_the_hold_with_a_bound_reason() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            None,
            "#!/usr/bin/env bash\nexit 0\n",
        );
        o.onepassword_preflight().await;
        let hold = o
            .onepassword_project_hold("studio-infra")
            .expect("an absent credential keeps the hold");
        assert!(hold.reason.contains("no service account bound"), "{hold}");
    }

    #[tokio::test]
    async fn a_probe_timeout_keeps_the_hold() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\nsleep 30\n",
        );
        o.onepassword_op_command = "/nonexistent/op-xyz".to_string();
        o.onepassword_preflight().await;
        assert!(
            o.onepassword_project_hold("studio-infra").is_some(),
            "an infrastructure timeout must not release the hold"
        );
    }

    #[tokio::test]
    async fn no_source_keeps_the_hold() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\necho '{}'\n",
        );
        o.onepassword_source = None;
        o.onepassword_preflight().await;
        assert!(
            o.onepassword_project_hold("studio-infra").is_some(),
            "with no source there is nothing to probe, so the hold stands"
        );
    }

    #[test]
    fn a_hold_appears_in_the_reconciliation_feed() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
        let feed = o.onepassword_human_feed();
        assert_eq!(feed.len(), 1, "one divergence per held project");
        assert!(
            feed[0].reason.contains("studio-infra"),
            "the divergence names the project: {}",
            feed[0].reason
        );
        assert!(matches!(
            feed[0].kind,
            crate::reviewreconcile::DivergenceKind::CredentialInfrastructure
        ));
        // A healthy daemon feeds nothing, keeping the wire payload unchanged.
        o.onepassword_holds.clear();
        assert!(o.onepassword_human_feed().is_empty());
    }
}
