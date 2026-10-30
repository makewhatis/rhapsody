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
//!
//! # Holds are keyed by CREDENTIAL ID, never by project
//!
//! A project's binding can be overridden per teammate profile (STUDIO-1584 review B6), so the
//! credential a run actually uses is not a function of the project alone. A hold is therefore keyed
//! by the credential id that was used, and the dispatch gate resolves the id the run *would* use
//! before consulting it. Keying by project released a profile-bound hold on the next tick (preflight
//! probed the project's own id, which was fine or absent) and re-dispatched the ticket forever.

use std::time::Duration;

use async_trait::async_trait;

pub use rhapsody_agent::onepassword::{
    OP_BIOMETRIC_UNLOCK_DISABLED, OP_BIOMETRIC_UNLOCK_ENABLED_ENV, OP_SERVICE_ACCOUNT_TOKEN_ENV,
};

/// How long a single `op whoami` health probe may run before it is abandoned as an infrastructure
/// fault. Matches the credential preflight's own hard bound (`preflight::PROBE_TIMEOUT`): a probe
/// that never returns must not strand a dispatch.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a recorded probe result stays fresh before the preflight re-probes it. Matches
/// `preflight::PROBE_TTL`: a bound credential is probed off the control loop at most once per
/// window, so a daemon with several projects does not shell out to `op` on every tick.
pub const STATUS_TTL: Duration = Duration::from_secs(5 * 60);

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
/// used. `op` 2.x reports `user_type` for a service account, so it is checked first; the older
/// `type` / `account.type` spellings are kept so a probe against another version still classifies.
fn whoami_account_type(v: &serde_json::Value) -> String {
    for key in ["user_type", "type"] {
        if let Some(t) = v.get(key).and_then(|t| t.as_str()) {
            return t.to_string();
        }
    }
    v.get("account")
        .and_then(|a| a.get("type"))
        .and_then(|t| t.as_str())
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
/// are minted with an expiry but not every CLI version reports it; `None` means "not readable". The
/// ticket's 14-day digest warning is explicitly conditional on the expiry being readable, so an
/// unreadable one is `None` here and never manufactures a warning.
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

/// The reason a run is held when its bound credential cannot be used. Names the binding so the
/// operator knows which credential to fix, never the token. `label` is the project slug (or the
/// teammate whose profile overrode the project) — whichever the hold was recorded against.
pub fn hold_reason(label: &str, health: &OnePasswordHealth) -> Option<String> {
    match health {
        OnePasswordHealth::Rejected(reason) => Some(format!(
            "1Password: token rejected for project {label} ({reason})"
        )),
        OnePasswordHealth::Absent => Some(format!(
            "1Password: no service account bound for project {label}"
        )),
        // Ok and Timeout are not credential failures: a timeout is an infrastructure fault that must
        // not strand the run (mirrors the credential preflight's repeated-timeout rule).
        OnePasswordHealth::Ok { .. } | OnePasswordHealth::Timeout => None,
    }
}

/// A recorded hold on a credential whose bound service account is unusable (STUDIO-1584). Held on
/// the control task until a probe succeeds, so a bad credential HOLDS its runs instead of failing
/// them terminally until the daemon restarts. Keyed by the credential `id` (see the module doc): the
/// `project_slug` is carried only so the operator-facing reason can name where the binding lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnePasswordHold {
    /// The bound credential id whose token was rejected/absent. The map key, never the token.
    pub id: String,
    /// The project slug (or teammate name) the binding came from, for the operator-facing reason.
    pub project_slug: String,
    /// The operator-facing reason (names the binding, never the token).
    pub reason: String,
}

impl OnePasswordHold {
    /// Build a hold from a probe that does not authenticate, or `None` when the health does not
    /// warrant a hold (a healthy credential, or an infrastructure timeout).
    pub fn from_health(
        id: &str,
        label: &str,
        health: &OnePasswordHealth,
    ) -> Option<OnePasswordHold> {
        let reason = hold_reason(label, health)?;
        Some(OnePasswordHold {
            id: id.to_string(),
            project_slug: label.to_string(),
            reason,
        })
    }
}

impl std::fmt::Display for OnePasswordHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The last health probe of one bound credential (STUDIO-1584, AC5). Recorded off the control loop by
/// [`crate::Orchestrator::onepassword_preflight`] so the operator can see WHY a run is held — the
/// account type, the vault list and the expiry a healthy probe reported, or the non-secret reason a
/// rejected one did — without reading the daemon log. Never carries a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnePasswordStatus {
    /// Whether the last probe authenticated.
    pub ok: bool,
    /// The account type `op whoami` reported, if any.
    pub account_type: String,
    /// The vaults the credential can read, if the probe listed them.
    pub vaults: Vec<String>,
    /// The credential's expiry, if the probe exposed one.
    pub expires_at: Option<String>,
    /// A non-secret reason when the probe did not authenticate (empty when `ok`).
    pub reason: String,
    /// When the probe ran (unix seconds), for the freshness TTL.
    pub checked_at_s: i64,
}

impl OnePasswordStatus {
    /// Classify one probe result into a recorded status. The details come from a healthy `op whoami`;
    /// a rejected/absent/timeout probe records only its non-secret reason.
    pub fn from_health(health: &OnePasswordHealth, now_s: i64) -> OnePasswordStatus {
        match health {
            OnePasswordHealth::Ok {
                account_type,
                vaults,
                expires_at,
            } => OnePasswordStatus {
                ok: true,
                account_type: account_type.clone(),
                vaults: vaults.clone(),
                expires_at: expires_at.clone(),
                reason: String::new(),
                checked_at_s: now_s,
            },
            OnePasswordHealth::Rejected(reason) => OnePasswordStatus {
                ok: false,
                account_type: String::new(),
                vaults: Vec::new(),
                expires_at: None,
                reason: reason.clone(),
                checked_at_s: now_s,
            },
            OnePasswordHealth::Absent => OnePasswordStatus {
                ok: false,
                account_type: String::new(),
                vaults: Vec::new(),
                expires_at: None,
                reason: "no service-account token is stored for this id".to_string(),
                checked_at_s: now_s,
            },
            OnePasswordHealth::Timeout => OnePasswordStatus {
                ok: false,
                account_type: String::new(),
                vaults: Vec::new(),
                expires_at: None,
                reason: "the health probe timed out (infrastructure fault)".to_string(),
                checked_at_s: now_s,
            },
        }
    }
}

/// One bound credential's status as `/api/v1/state`'s `onepassword` key renders it (STUDIO-1584,
/// AC5): the last probe's account type/vaults/expiry (or the non-secret reason it failed) plus
/// whether a hold currently stands. Never carries a token. Emitted only when at least one credential
/// is bound and has been probed, so a daemon with no `onepassword:` binding serves a `/api/v1/state`
/// payload byte-identical to the Go daemon's.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OnePasswordStatusRow {
    pub id: String,
    pub ok: bool,
    pub account_type: String,
    pub vaults: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The non-secret reason the last probe did not authenticate (empty when `ok`).
    pub reason: String,
    /// Whether a hold currently refuses runs bound to this credential.
    pub held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<String>,
}

/// Whether an agent's output shows a 1Password CLI auth failure (STUDIO-1584, AC6). The
/// biometric-disabled `op` prints a recognizable signature when no service account is bound or its
/// token was rejected — reproduced against `op` 2.41.0: `[ERROR] … no active session found for
/// account …`, with no Touch ID prompt. Matching is deliberately conservative (a specific phrase, or
/// `1password` beside an auth phrase) so ordinary text is never mistaken for a credential failure.
pub fn op_auth_failure_signature(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("no active session found") || lowered.contains("no account is signed in") {
        return true;
    }
    lowered.contains("1password")
        && (lowered.contains("not signed in")
            || lowered.contains("not currently signed in")
            || lowered.contains("authentication failed"))
}

/// The AC6 advisory for a run whose agent output shows a 1Password auth failure. Names the project,
/// and the bound credential id when one is bound, never a token. `None` when `text` shows no
/// signature, so the ordinary path adds nothing.
pub fn op_auth_failure_advisory(
    project_slug: &str,
    credential_id: Option<&str>,
    text: &str,
) -> Option<String> {
    if !op_auth_failure_signature(text) {
        return None;
    }
    Some(match credential_id {
        Some(id) if !id.is_empty() => {
            format!("1Password: token rejected for project {project_slug} (credential {id})")
        }
        _ => format!("1Password: no service account bound for project {project_slug}"),
    })
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
    /// Record a hold for the credential it names (STUDIO-1584). Loop-confined: written by the
    /// worker-exit path when a dispatch is refused for an unusable credential. Keyed by the
    /// credential id, so a hold only blocks dispatches that would resolve to the same credential.
    pub(crate) fn record_onepassword_hold(&mut self, hold: OnePasswordHold) {
        if hold.id.is_empty() {
            return;
        }
        self.onepassword_holds.insert(hold.id.clone(), hold);
    }

    /// The credential id a dispatch of `issue` in `project_slug` would resolve, mirroring
    /// `spawn_worker`: a routed teammate's profile `onepassword:` override wins, else the project's
    /// own binding. `None` when neither binds one (the default — a project gets no token).
    pub(crate) fn onepassword_id_for_dispatch(
        &self,
        identity: &str,
        project_slug: &str,
    ) -> Option<String> {
        if let Some(id) = self.profile_onepassword_override(identity) {
            return Some(id);
        }
        self.eff
            .as_ref()
            .and_then(|eff| eff.project_by_slug(project_slug))
            .and_then(|p| p.onepassword.clone())
    }

    /// The hold that would refuse a dispatch of `issue` for `identity` in `project_slug`, keyed by
    /// the credential the run would actually use. `None` when the resolved credential is not held
    /// (including when nothing is bound, or a DIFFERENT teammate's credential is the held one).
    pub(crate) fn onepassword_hold_for_dispatch(
        &self,
        identity: &str,
        project_slug: &str,
    ) -> Option<&OnePasswordHold> {
        let id = self.onepassword_id_for_dispatch(identity, project_slug)?;
        self.onepassword_holds.get(&id)
    }

    /// Every distinct credential id the config binds — each project's `onepassword:` plus each
    /// roster profile's — paired with a label (the project slug, or the teammate name) for the
    /// operator-facing reason. Used by the preflight to probe and record each bound credential.
    pub(crate) fn bound_onepassword_ids(&self) -> Vec<(String, String)> {
        let mut out: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        if let Some(eff) = &self.eff {
            for p in &eff.projects {
                if let Some(id) = &p.onepassword {
                    out.entry(id.clone()).or_insert_with(|| p.slug.clone());
                }
            }
        }
        if let (Some(teams), Some(dir)) = (self.teams.as_ref(), self.teams_profiles_dir.as_ref()) {
            for ident in &teams.roster {
                if let Ok(profile) = rhapsody_config::profiles::resolve(dir, &ident.profile)
                    && !profile.onepassword.is_empty()
                {
                    out.entry(profile.onepassword.clone())
                        .or_insert_with(|| ident.name.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    /// The reconciliation sweep's feed for held 1Password credentials (STUDIO-1584, AC6): one
    /// `CredentialInfrastructure` divergence per held credential, so an unusable credential shows up
    /// in `/api/v1/state`'s `review_divergence` and the per-project advisory instead of an
    /// unexplained stall. Empty on a healthy daemon, which keeps the wire payload — and the golden —
    /// unchanged.
    pub(crate) fn onepassword_human_feed(&self) -> Vec<crate::reviewreconcile::Divergence> {
        let holds =
            self.onepassword_holds
                .values()
                .map(|hold| crate::reviewreconcile::Divergence {
                    pr: format!("1Password credential for project {}", hold.project_slug),
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
                });
        // AC6: a ticket run whose agent output showed a 1Password auth failure. The unbound-project
        // case — which no dispatch-time hold can catch, because nothing is bound — lands here, so it
        // reads as a named credential failure rather than an unexplained one.
        let in_run = self
            .onepassword_inrun_failures
            .iter()
            .map(
                |(project_slug, reason)| crate::reviewreconcile::Divergence {
                    pr: format!("1Password credential for project {project_slug}"),
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
                    reason: reason.clone(),
                },
            );
        holds.chain(in_run).collect()
    }

    /// Re-probe every bound credential off the control loop and RELEASE the hold when a probe
    /// authenticates (STUDIO-1584). Mirrors `credential_preflight`: the probes run concurrently as
    /// tasks and the control task awaits the join, so a probe that never returns is bounded by
    /// [`PROBE_TIMEOUT`] rather than stranding the daemon. A timeout is an infrastructure fault and
    /// keeps the hold; only a healthy `op whoami` releases it. Each probe result is recorded as a
    /// [`OnePasswordStatus`] (AC5) so the operator can see the account type, vaults and expiry, or
    /// the non-secret reason, without reading the log.
    ///
    /// A hold whose credential is no longer bound by ANY project or profile is dropped here: nothing
    /// would ever probe it again, so leaving it would block that credential's runs forever.
    pub(crate) async fn onepassword_preflight(&mut self) {
        // Publish the current expiry advisories for the morning digest (AC5) on every pass, so a
        // credential that drifts inside the warning window is picked up without waiting for a probe.
        if let Some(reports) = &self.lead_reports {
            reports.set_onepassword_expiring(self.onepassword_expiry_advisories());
        }
        // Prune in-run advisories for projects a reload removed, so a deleted project's stale AC6
        // advisory cannot linger in the reconciliation feed forever.
        if let Some(eff) = &self.eff {
            let slugs: std::collections::HashSet<&str> =
                eff.projects.iter().map(|p| p.slug.as_str()).collect();
            self.onepassword_inrun_failures
                .retain(|slug, _| slugs.contains(slug.as_str()));
        }
        let Some(source) = self.onepassword_source.clone() else {
            // No desktop channel: a bound id can never resolve, so nothing can be probed and no
            // status can change. A hold can only stand until a token is stored.
            return;
        };
        let bound = self.bound_onepassword_ids();
        let bound_ids: std::collections::HashSet<String> =
            bound.iter().map(|(id, _)| id.clone()).collect();
        self.onepassword_holds
            .retain(|id, _| bound_ids.contains(id));
        self.onepassword_status
            .retain(|id, _| bound_ids.contains(id));
        let now_s = (self.now)().timestamp();
        let due: Vec<(String, String)> = bound
            .into_iter()
            .filter(|(id, _)| {
                self.onepassword_status
                    .get(id)
                    .is_none_or(|s| now_s - s.checked_at_s >= STATUS_TTL.as_secs() as i64)
            })
            .collect();
        if due.is_empty() {
            return;
        }
        let command = self.onepassword_op_command.clone();
        let mut tasks = tokio::task::JoinSet::new();
        for (id, label) in due {
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
                (id, label, health)
            });
        }
        let now = (self.now)();
        while let Some(result) = tasks.join_next().await {
            let Ok((id, label, health)) = result else {
                continue;
            };
            self.onepassword_status
                .insert(id.clone(), OnePasswordStatus::from_health(&health, now_s));
            match &health {
                OnePasswordHealth::Ok {
                    account_type,
                    expires_at,
                    ..
                } => {
                    if self.onepassword_holds.remove(&id).is_some() {
                        tracing::warn!(
                            credential_id = %id,
                            project_slug = %label,
                            "1Password: the bound credential recovered; releasing the hold"
                        );
                        self.schedule_tick(Duration::ZERO);
                    }
                    if account_type_looks_writable(account_type) {
                        tracing::warn!(
                            credential_id = %id,
                            project_slug = %label,
                            account_type = %account_type,
                            "1Password: the bound credential is not a read-only service account"
                        );
                    }
                    if let Some(expires_at) = expires_at
                        && let Some(true) = expiry_within_warning(expires_at, now)
                    {
                        tracing::warn!(
                            credential_id = %id,
                            project_slug = %label,
                            expires_at = %expires_at,
                            "1Password: the bound service-account credential expires within {} \
                             days; mint a replacement",
                            EXPIRY_WARN_DAYS
                        );
                    }
                }
                OnePasswordHealth::Timeout => {
                    tracing::warn!(
                        credential_id = %id,
                        project_slug = %label,
                        "1Password: the health probe timed out; keeping the hold (infrastructure \
                         fault, not a credential failure)"
                    );
                }
                OnePasswordHealth::Rejected(_) | OnePasswordHealth::Absent => {
                    if let Some(hold) = OnePasswordHold::from_health(&id, &label, &health) {
                        self.onepassword_holds.insert(id.clone(), hold);
                    }
                }
            }
        }
    }

    /// The non-secret advisories for bound credentials within [`EXPIRY_WARN_DAYS`] of expiry (or
    /// already past), one per credential, for the morning digest (STUDIO-1584, AC5). Empty when no
    /// credential has a readable near expiry — the ticket's warning is conditional on the expiry
    /// being readable, so an unreadable one is never a warning.
    pub(crate) fn onepassword_expiry_advisories(&self) -> Vec<String> {
        let now = (self.now)();
        self.onepassword_status
            .iter()
            .filter_map(|(id, status)| {
                let expires_at = status.expires_at.as_deref()?;
                match expiry_within_warning(expires_at, now) {
                    Some(true) => Some(format!("credential {id} expires {expires_at}")),
                    _ => None,
                }
            })
            .collect()
    }

    /// The bound credentials' recorded probe status, one row per credential, ordered by id, for
    /// `/api/v1/state`'s `onepassword` key (STUDIO-1584, AC5). Empty when nothing has been probed —
    /// a daemon with no `onepassword:` binding — which keeps the wire payload (and the golden)
    /// unchanged.
    pub(crate) fn onepassword_status_rows(&self) -> Vec<OnePasswordStatusRow> {
        self.onepassword_status
            .iter()
            .map(|(id, status)| OnePasswordStatusRow {
                id: id.clone(),
                ok: status.ok,
                account_type: status.account_type.clone(),
                vaults: status.vaults.clone(),
                expires_at: status.expires_at.clone(),
                reason: status.reason.clone(),
                held: self.onepassword_holds.contains_key(id),
                hold_reason: self.onepassword_holds.get(id).map(|h| h.reason.clone()),
            })
            .collect()
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
            "#!/usr/bin/env bash\necho '{\"user_type\":\"SERVICE_ACCOUNT\",\"vaults\":[\"Studio49\"],\"expires_at\":\"2027-01-01T00:00:00Z\"}'\n",
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

    /// `op` 2.x reports `user_type`; the older `type` spelling must still classify.
    #[tokio::test]
    async fn a_whoami_that_reports_the_legacy_type_key_still_classifies() {
        let dir = tmp();
        let script = fake_op(
            &dir,
            "#!/usr/bin/env bash\necho '{\"type\":\"SERVICE_ACCOUNT\"}'\n",
        );
        let health = probe_whoami(&script, Some("ops_x"), PROBE_TIMEOUT).await;
        assert_eq!(
            health,
            OnePasswordHealth::Ok {
                account_type: "SERVICE_ACCOUNT".into(),
                vaults: Vec::new(),
                expires_at: None,
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

    #[test]
    fn a_status_records_the_probe_details_or_the_reason() {
        let ok = OnePasswordStatus::from_health(
            &OnePasswordHealth::Ok {
                account_type: "SERVICE_ACCOUNT".into(),
                vaults: vec!["Studio49".into()],
                expires_at: Some("2027-01-01T00:00:00Z".into()),
            },
            100,
        );
        assert!(ok.ok);
        assert_eq!(ok.account_type, "SERVICE_ACCOUNT");
        assert_eq!(ok.vaults, vec!["Studio49".to_string()]);
        assert_eq!(ok.checked_at_s, 100);

        let rejected =
            OnePasswordStatus::from_health(&OnePasswordHealth::Rejected("bad".into()), 101);
        assert!(!rejected.ok);
        assert_eq!(rejected.reason, "bad");
        assert!(rejected.account_type.is_empty());
    }

    // --- Hold + release (STUDIO-1584 review B3/B6) ----------------------------------------------

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
        o.record_onepassword_hold(OnePasswordHold {
            id: id.to_string(),
            project_slug: "studio-infra".to_string(),
            reason: "1Password: token rejected for project studio-infra (stale)".into(),
        });
        (o, dir)
    }

    #[tokio::test]
    async fn a_successful_probe_releases_the_hold() {
        let (mut o, dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\necho '{\"user_type\":\"SERVICE_ACCOUNT\"}'\n",
        );
        assert!(o.onepassword_holds.contains_key("studio-infra-readonly"));
        o.onepassword_preflight().await;
        assert!(
            !o.onepassword_holds.contains_key("studio-infra-readonly"),
            "a healthy probe must release the hold"
        );
        // The probe's details are recorded even though the hold is released (AC5).
        let status = o
            .onepassword_status
            .get("studio-infra-readonly")
            .expect("a status is recorded");
        assert!(status.ok);
        assert_eq!(status.account_type, "SERVICE_ACCOUNT");
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
            .onepassword_holds
            .get("studio-infra-readonly")
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
            .onepassword_holds
            .get("studio-infra-readonly")
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
            o.onepassword_holds.contains_key("studio-infra-readonly"),
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
            o.onepassword_holds.contains_key("studio-infra-readonly"),
            "with no source there is nothing to probe, so the hold stands"
        );
    }

    #[tokio::test]
    async fn a_hold_for_a_credential_that_is_no_longer_bound_is_dropped() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\necho '{\"user_type\":\"SERVICE_ACCOUNT\"}'\n",
        );
        // A reload removes the binding: nothing can probe it, so the hold must not linger.
        o.eff.as_mut().unwrap().projects[0].onepassword = None;
        o.onepassword_preflight().await;
        assert!(
            !o.onepassword_holds.contains_key("studio-infra-readonly"),
            "a hold for a credential that is no longer bound must be dropped"
        );
    }

    /// STUDIO-1584 review B6: a PROFILE-bound credential's hold must survive a preflight tick even
    /// when the project binds nothing. The old project-keyed preflight probed the project's own id
    /// (none here), dropped the hold, and the ticket re-dispatched and recorded a failed run every
    /// poll. The hold is keyed by the credential id, so it is probed directly and only a healthy
    /// probe releases it.
    #[tokio::test]
    async fn a_profile_bound_hold_survives_a_preflight_when_the_project_binds_nothing() {
        use crate::testsupport::{empty_effective, empty_resolved_project};
        use rhapsody_config::teams::{Identity, Teams};
        let profile_dir = tmp();
        std::fs::write(
            profile_dir.join("worker.md"),
            "---\nextends: none\nonepassword: kid-coder-readonly\n---\nWorker\n",
        )
        .unwrap();
        let mut eff = empty_effective(Arc::new(rhapsody_tracker::fake::Fake::new()));
        let mut proj = empty_resolved_project(
            "studio-infra",
            Arc::new(rhapsody_tracker::fake::Fake::new()),
        );
        proj.onepassword = None; // the project binds NOTHING
        eff.projects = vec![proj];
        let mut o = crate::Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: true,
            roster: vec![Identity {
                name: "alice".into(),
                profile: "worker".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        o.teams_profiles_dir = Some(profile_dir.clone());

        // alice's run used the profile-bound credential and was refused.
        o.record_onepassword_hold(OnePasswordHold {
            id: "kid-coder-readonly".into(),
            project_slug: "alice".into(),
            reason: "1Password: token rejected for project alice".into(),
        });
        // The gate resolves alice's override; bob inherits the project's (absent) binding.
        assert!(
            o.onepassword_hold_for_dispatch("alice", "studio-infra")
                .is_some(),
            "alice's profile-bound credential is held"
        );
        assert!(
            o.onepassword_hold_for_dispatch("bob", "studio-infra")
                .is_none(),
            "a hold on alice's credential must not block a teammate whose credential is fine"
        );

        // The two REAL dispatch gates — not just the id-resolution helper — must resolve the
        // profile override and refuse alice while bob is admitted. This pins the gate wiring the
        // review's M2+M3 mutations removed (B6's dispatch half), which the helper-only assertions
        // above left unpinned.
        use crate::testsupport::{issue as ts_issue, running_entry};
        let mut alice_issue = ts_issue("1", "MT-1", "Todo");
        alice_issue.labels = Some(vec!["rhapsody:@alice".into()]);
        assert_eq!(
            o.planned_identity(&alice_issue, &o.teammate_load())
                .as_deref(),
            Some("alice"),
            "the fixture must route to alice, or these gate assertions pin nothing"
        );
        assert!(
            !o.limit_dispatch_ready(&alice_issue, "studio-infra"),
            "the select gate must refuse alice's held profile-bound credential (review B6)"
        );
        let mut re = running_entry(alice_issue.clone(), "studio-infra", "studio-infra");
        re.model_override.identity = "alice".into();
        let mut engine = None;
        assert!(
            !o.gate_limit_dispatch(&mut re, &mut engine, None),
            "the final dispatch gate must refuse alice's held profile-bound credential (review B6)"
        );

        // A preflight tick probes the HELD id (not the project's), finds it still rejected, keeps it.
        let op_dir = tmp();
        let script = fake_op(
            &op_dir,
            "#!/usr/bin/env bash\necho '[ERROR] authentication failed' >&2\nexit 1\n",
        );
        let mut map = std::collections::HashMap::new();
        map.insert("kid-coder-readonly".to_string(), "ops_sentinel".to_string());
        o.onepassword_source = Some(Arc::new(MapOnePasswordSource::new(map)));
        o.onepassword_op_command = script;
        o.onepassword_preflight().await;
        assert!(
            o.onepassword_holds.contains_key("kid-coder-readonly"),
            "the profile-bound hold must survive the preflight (review B6)"
        );
        assert!(
            o.onepassword_hold_for_dispatch("alice", "studio-infra")
                .is_some(),
            "the hold still refuses alice's dispatch"
        );
        std::fs::remove_dir_all(&profile_dir).ok();
        std::fs::remove_dir_all(&op_dir).ok();
    }

    // --- In-run auth-failure advisory (STUDIO-1584 review B5, AC6) ------------------------------

    #[test]
    fn an_op_auth_signature_is_recognized_and_named_without_a_token() {
        // The exact shape `op` 2.41.0 prints with biometric unlock disabled and no token.
        let text = "[ERROR] 2026/10/30 12:00:00 no active session found for account studio49";
        assert!(op_auth_failure_signature(text));
        let advisory = op_auth_failure_advisory("kid-coder", None, text).expect("unbound advisory");
        assert_eq!(
            advisory,
            "1Password: no service account bound for project kid-coder"
        );
        let bound = op_auth_failure_advisory("studio-infra", Some("studio-infra-readonly"), text)
            .expect("bound advisory");
        assert!(
            bound.contains("token rejected for project studio-infra"),
            "{bound}"
        );
        assert!(!bound.contains("ops_"), "never a token: {bound}");
    }

    #[test]
    fn ordinary_text_is_never_a_1password_failure() {
        assert!(!op_auth_failure_signature("the build succeeded"));
        assert!(!op_auth_failure_signature("no session found in the cache"));
        assert!(op_auth_failure_advisory("p", None, "all good").is_none());
    }

    #[test]
    fn a_hold_appears_in_the_reconciliation_feed() {
        let (mut o, _dir) = orch_holding(
            "studio-infra-readonly",
            Some("ops_sentinel"),
            "#!/usr/bin/env bash\nexit 0\n",
        );
        let feed = o.onepassword_human_feed();
        assert_eq!(feed.len(), 1, "one divergence per held credential");
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
