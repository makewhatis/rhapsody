//! teamsmemory — Rhapsody Teams memory as an **off-loop** daemon surface
//! (STUDIO-645, slice T4; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5, §0.11.7).
//!
//! [`crate::teams`] owns the on-loop half of memory: a sync, local-file recall
//! rendered into the turn-1 prompt. This module owns the other half — the three
//! operations an *agent* performs through MCP, which must not touch the control
//! task at all:
//!
//! * `teams_retain` — write a host-stamped record for the calling run.
//! * `teams_recall` — read an identity's bank.
//! * `teams_invalidate` — mark one record non-valid, with the reason.
//! * `teams_roster` — who exists, and what each of them is doing right now.
//!
//! # Why this is a fourth shared cell, and what keeps it honest
//!
//! `crates/orchestrator/CLAUDE.md` lists three sanctioned off-loop seams
//! (`reads`, `ControlHandle`, `warnings`) and asks that a fourth be documented
//! rather than smuggled in as an ad-hoc `Arc<Mutex<..>>`. This is that fourth
//! seam, and it exists for a specific reason: **retain must never block the
//! control task** (§5.1 — "best-effort, never fatal, never blocking the control
//! task"). Routing retain through `ControlHandle`'s event channel would put an
//! agent-triggered request behind whatever the current tick is doing, which is
//! the head-of-line class the design review already made the T3a/T3b split to
//! avoid.
//!
//! So [`TeamsMemory`] is an `Arc` shared by the orchestrator and the HTTP layer:
//!
//! * The **control task writes** [`bind_run`](TeamsMemory::bind_run) /
//!   [`release_run`](TeamsMemory::release_run) — two `HashMap` operations under a
//!   write lock, no I/O, on the dispatch and run-exit paths it already owns.
//! * The **HTTP task reads** that map and then does its own I/O entirely off the
//!   loop.
//!
//! The lock is never held across an `.await`: every entry point copies what it
//! needs out of the map and drops the guard before touching a backend.
//!
//! # Provenance is stamped here, never accepted
//!
//! §5.1's split is that Rhapsody supplies the evidence and the agent supplies
//! the prose. The `teams_retain` tool takes **`content` and nothing else**; the
//! identity, ticket, run id, `document_id` and commit are resolved here from the
//! run the request names. A run dispatched as `bob` cannot retain as `alice`,
//! and no run can retain against a ticket it is not working — the same
//! host-stamping requirement §0.11.4 puts on the room's `from`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use rhapsody_config::memory::{Fact, MemoryBackend, MemoryError, Query, Record};
use rhapsody_config::teams::Teams;
use serde::Serialize;

/// How long the best-effort `git rev-parse HEAD` that stamps `commit_sha` may
/// take before the record is written without one. A local read of a local
/// repository; if it has not answered in this long, something is wrong with the
/// checkout and the retain should still land.
const COMMIT_SHA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What the host knows about a live run, so a `teams_retain` from it can be
/// stamped rather than trusted. Written by the control task at dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProvenance {
    /// The Teams identity the run was routed to. Empty ⇒ the run is not wearing
    /// an identity and cannot retain.
    pub identity: String,
    /// The ticket identifier the run is working.
    pub ticket: String,
    /// The run's worktree, for the `commit_sha` stamp. Empty when the daemon
    /// cannot name one (a legacy dispatch with no workspace manager).
    pub workspace_dir: String,
}

/// One roster row as `teams_roster` serves it: the configured identity plus the
/// status derived from the runs currently bound to it (§6.7 — "who exists,
/// profile, derived status").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RosterRow {
    pub name: String,
    pub profile: String,
    pub labels: Vec<String>,
    pub bank: String,
    pub max_concurrent: i64,
    /// How many runs are live as this identity right now.
    pub live_runs: i64,
    /// Which tickets those runs are working, sorted for a stable response.
    pub tickets: Vec<String>,
}

/// `GET /api/v1/teams/roster`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RosterView {
    pub backend: String,
    pub roster: Vec<RosterRow>,
}

/// `GET /api/v1/teams/recall`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecallView {
    pub identity: String,
    pub facts: Vec<Fact>,
    /// Record files that could not be read — reported rather than hidden, so
    /// "skipped loudly" is true of the API as well as of the log.
    pub skipped: Vec<String>,
}

/// `POST /api/v1/runs/{id}/retain` — the host-stamped provenance echoed back, so
/// the agent can see exactly what was recorded on its behalf.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetainView {
    pub id: String,
    pub identity: String,
    pub ticket: String,
    pub run_id: String,
    pub document_id: String,
    pub commit_sha: String,
}

/// `POST /api/v1/teams/invalidate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InvalidateView {
    pub identity: String,
    pub fact_id: String,
    /// `false` ⇒ the record was already invalidated (a no-op, not a failure).
    pub invalidated: bool,
    pub reason: String,
}

/// Why a Teams memory request could not be served. The HTTP layer maps each
/// variant to a status; the MCP tool surfaces the body verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamsMemoryError {
    /// Teams is off, or the daemon has no Teams runtime at all. Every `teams_*`
    /// route is removed from the MCP router in that case, so an agent should
    /// never see this — it is the daemon-side backstop for a direct HTTP call.
    Disabled,
    /// No live run with that id, or the run is not wearing an identity — so
    /// there is no identity to attribute the record to. A retain cannot be
    /// re-pointed at some other identity to make it succeed.
    NotRunning,
    /// The named record does not exist in that identity's bank.
    NotFound(String),
    /// The request itself is malformed (an unusable identity, an empty body).
    Invalid(String),
    /// The backend failed.
    Backend(String),
}

impl std::fmt::Display for TeamsMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TeamsMemoryError::Disabled => f.write_str("teams is not enabled on this daemon"),
            TeamsMemoryError::NotRunning => {
                f.write_str("no live run with that id is wearing a teams identity")
            }
            TeamsMemoryError::NotFound(m)
            | TeamsMemoryError::Invalid(m)
            | TeamsMemoryError::Backend(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for TeamsMemoryError {}

impl From<MemoryError> for TeamsMemoryError {
    fn from(e: MemoryError) -> Self {
        match e {
            MemoryError::NotFound(m) => TeamsMemoryError::NotFound(m),
            MemoryError::Invalid(m) => TeamsMemoryError::Invalid(m),
            MemoryError::Io(m) => TeamsMemoryError::Backend(m),
        }
    }
}

/// The shared Teams memory runtime: the loaded config, the backend, and the
/// live run → identity binding the control task maintains.
pub struct TeamsMemory {
    teams: Arc<Teams>,
    backend: Arc<dyn MemoryBackend>,
    /// How the backend resolves an identity to its bank id, so the roster view
    /// reports what the STORE actually uses rather than re-deriving it and
    /// risking disagreement (an identity with a roster `bank:` override would
    /// otherwise be reported one way and written another).
    bank_ids: HashMap<String, String>,
    /// run id → what the host knows about that run. Written by the control task
    /// at dispatch / run exit; read by the HTTP task. Never held across an
    /// `.await`.
    runs: RwLock<HashMap<i64, RunProvenance>>,
}

impl TeamsMemory {
    /// Builds the runtime over a loaded config and a constructed backend.
    /// Creates nothing: with `backend: none` (or Teams off) the backend is a
    /// no-op and the filesystem is never touched.
    pub fn new(teams: Arc<Teams>, backend: Arc<dyn MemoryBackend>) -> Self {
        let bank_ids = teams
            .roster
            .iter()
            .map(|i| {
                let bank = if i.bank.is_empty() {
                    format!("{}{}", teams.memory.bank_prefix, i.name)
                } else {
                    i.bank.clone()
                };
                (i.name.clone(), bank)
            })
            .collect();
        Self {
            teams,
            backend,
            bank_ids,
            runs: RwLock::new(HashMap::new()),
        }
    }

    /// The identity → bank-id map the backend was built with, so the composition
    /// root can hand the SAME resolution to `LocalBank::with_bank_overrides`.
    pub fn bank_ids(&self) -> &HashMap<String, String> {
        &self.bank_ids
    }

    /// Whether Teams is on. Every entry point below refuses when it is not, and
    /// the MCP facade removes the tools entirely (§6.7).
    pub fn enabled(&self) -> bool {
        self.teams.enabled
    }

    /// Binds a live run to what the host knows about it, so a later
    /// `teams_retain` can be stamped. Called by the control task at dispatch; a
    /// run with no identity is not bound at all, and so cannot retain.
    ///
    /// `pub` for the control task and for the HTTP layer's tests, which drive
    /// the real runtime rather than a canned provider result. It is a pure map
    /// write — no I/O, no validation of the identity against the roster: the
    /// caller is the host, and the host is the only thing that may say who a run
    /// is.
    pub fn bind_run(&self, run_id: i64, prov: RunProvenance) {
        if run_id == 0 || prov.identity.is_empty() {
            return;
        }
        self.write_runs().insert(run_id, prov);
    }

    /// Releases a finished run's binding. Called by the control task at run exit
    /// and at terminate, so the roster's derived status is live and a completed
    /// run cannot keep retaining.
    pub fn release_run(&self, run_id: i64) {
        if run_id == 0 {
            return;
        }
        self.write_runs().remove(&run_id);
    }

    /// The roster, with each identity's live runs derived from the bindings.
    pub fn roster(&self) -> Result<RosterView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let bound: Vec<RunProvenance> = self.read_runs().values().cloned().collect();
        let roster = self
            .teams
            .roster
            .iter()
            .map(|i| {
                let mut tickets: Vec<String> = bound
                    .iter()
                    .filter(|p| p.identity == i.name)
                    .map(|p| p.ticket.clone())
                    .collect();
                tickets.sort();
                RosterRow {
                    name: i.name.clone(),
                    profile: i.profile.clone(),
                    labels: i.labels.clone(),
                    bank: self.bank_ids.get(&i.name).cloned().unwrap_or_default(),
                    max_concurrent: i.max_concurrent,
                    live_runs: tickets.len() as i64,
                    tickets,
                }
            })
            .collect();
        Ok(RosterView {
            backend: backend_name(&self.teams),
            roster,
        })
    }

    /// Recalls an identity's memory for a free-text `query` (§6.7's
    /// `teams_recall {identity, query}` — the memory-first path, no live turn).
    pub async fn recall(
        &self,
        identity: &str,
        query: &str,
    ) -> Result<RecallView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let identity = identity.trim();
        if identity.is_empty() {
            return Err(TeamsMemoryError::Invalid(
                "identity is required".to_string(),
            ));
        }
        // The query is offered BOTH ways, and it has to be.
        //
        // As a title it contributes its 4+-character tokens, which is what a
        // prose query ("the mirror lock") needs. But as a title ALONE, the most
        // obvious use of this tool silently failed: `"MT-9"` splits into `mt`
        // and `9`, both under the token threshold, so recalling by ticket
        // identifier returned nothing at all — empty, with no error to notice,
        // while the fact sat in the bank. Offering it as `ticket` too lets an
        // exact identifier match score outright.
        //
        // This cannot broaden recall into "return everything": `ticket` only
        // scores on an exact `f.ticket` match or a whole-string containment, so
        // a prose query that names no ticket adds nothing through this field.
        let q = Query {
            ticket: query.trim().to_string(),
            labels: Vec::new(),
            title: query.to_string(),
            top_k: usize::try_from(self.teams.memory.recall_top_k).unwrap_or(0),
        };
        let recalled = self.backend.recall(identity, &q).await?;
        for (file, why) in &recalled.skipped {
            tracing::warn!(
                identity = %identity,
                file = %file,
                reason = %why,
                "teams memory: skipping an unreadable bank record (recall continues without it)"
            );
        }
        Ok(RecallView {
            identity: identity.to_string(),
            facts: recalled.facts,
            skipped: recalled.skipped.into_iter().map(|(f, _)| f).collect(),
        })
    }

    /// Marks one record non-valid with its reason (§5.3), reversibly.
    pub async fn invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<InvalidateView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let (identity, fact_id, reason) = (identity.trim(), fact_id.trim(), reason.trim());
        if identity.is_empty() || fact_id.is_empty() {
            return Err(TeamsMemoryError::Invalid(
                "identity and fact_id are required".to_string(),
            ));
        }
        if reason.is_empty() {
            // §5.3 stores the reason; an invalidation with no reason is the
            // Go client's exact mistake (the measured 400), and it makes the
            // correction unreadable to whoever finds it later.
            return Err(TeamsMemoryError::Invalid("reason is required".to_string()));
        }
        let invalidated = self.backend.invalidate(identity, fact_id, reason).await?;
        Ok(InvalidateView {
            identity: identity.to_string(),
            fact_id: fact_id.to_string(),
            invalidated,
            reason: reason.to_string(),
        })
    }

    /// Retains a record for a live run, stamping every provenance field itself
    /// (§5.1). The agent supplies `content` and nothing else.
    pub async fn retain_for_run(
        &self,
        run_id: i64,
        content: &str,
        now: DateTime<Utc>,
    ) -> Result<RetainView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let content = content.trim();
        if content.is_empty() {
            return Err(TeamsMemoryError::Invalid("content is required".to_string()));
        }
        // Copy the binding out and drop the guard: the commit read and the
        // backend write below are `.await`s, and this lock is also taken by the
        // control task on the dispatch path.
        let Some(prov) = self.read_runs().get(&run_id).cloned() else {
            return Err(TeamsMemoryError::NotRunning);
        };
        let commit_sha = head_commit(&prov.workspace_dir).await;
        let rec = Record {
            identity: prov.identity.clone(),
            document_id: format!("run-{run_id}"),
            ticket: prov.ticket.clone(),
            commit_sha: commit_sha.clone(),
            // `pr` needs a `gh` round-trip, which is network I/O; it is left
            // empty and named as deferred rather than fetched here.
            pr: String::new(),
            run_id: run_id.to_string(),
            at: now,
            content: content.to_string(),
        };
        let id = self.backend.retain(&rec).await?;
        Ok(RetainView {
            id,
            identity: rec.identity,
            ticket: rec.ticket,
            run_id: rec.run_id,
            document_id: rec.document_id,
            commit_sha,
        })
    }

    fn read_runs(&self) -> std::sync::RwLockReadGuard<'_, HashMap<i64, RunProvenance>> {
        self.runs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_runs(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<i64, RunProvenance>> {
        self.runs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The configured backend's name, for the roster view.
fn backend_name(teams: &Teams) -> String {
    match teams.memory.backend {
        rhapsody_config::teams::MemoryBackend::None => "none",
        rhapsody_config::teams::MemoryBackend::Local => "local",
        rhapsody_config::teams::MemoryBackend::Hindsight => "hindsight",
    }
    .to_string()
}

/// Best-effort `git rev-parse HEAD` in a run's worktree — §5.1's "`commit_sha`
/// … from the workspace".
///
/// **Local only, and off the control task.** It reads a local repository, is
/// bounded by [`COMMIT_SHA_TIMEOUT`], and runs on the HTTP task that serves the
/// retain. Any failure — no worktree, no git, a timeout — yields an empty
/// `commit_sha` and the record is written anyway: a retain is best-effort and
/// never fatal (§5.1), and a record with no commit is far better than a run that
/// failed to record what it learned.
async fn head_commit(workspace_dir: &str) -> String {
    if workspace_dir.is_empty() {
        return String::new();
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(workspace_dir)
        .arg("rev-parse")
        .arg("HEAD")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(COMMIT_SHA_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => out,
        Ok(Ok(_)) | Ok(Err(_)) => return String::new(),
        Err(_) => {
            tracing::warn!(
                dir = %workspace_dir,
                "teams retain: `git rev-parse HEAD` timed out; storing the record without a commit"
            );
            return String::new();
        }
    };
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_config::memory::{DEFAULT_BANKS_SUBDIR, LocalBank, NoneBackend};
    use rhapsody_config::teams::{Identity, Teams};

    use crate::testsupport::TempDir;

    fn teams_on(roster: Vec<Identity>) -> Arc<Teams> {
        Arc::new(Teams {
            enabled: true,
            roster,
            ..Teams::disabled()
        })
    }

    fn ident(name: &str) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            labels: vec!["rust".to_string()],
            ..Identity::default()
        }
    }

    fn local(dir: &TempDir, teams: Arc<Teams>) -> TeamsMemory {
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        TeamsMemory::new(teams, Arc::new(bank))
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000, 0).expect("timestamp")
    }

    fn bound(identity: &str, ticket: &str) -> RunProvenance {
        RunProvenance {
            identity: identity.to_string(),
            ticket: ticket.to_string(),
            workspace_dir: String::new(),
        }
    }

    /// **The anti-forgery property.** The tool takes `content` and nothing else:
    /// identity, ticket, run id and `document_id` are all resolved from the run
    /// the request names, so a run dispatched as bob cannot retain as alice
    /// (§5.1, §0.11.4).
    #[tokio::test]
    async fn retain_stamps_provenance_from_the_run_not_the_caller() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice"), ident("bob")]));
        mem.bind_run(7, bound("bob", "MT-9"));

        let view = mem
            .retain_for_run(7, "  the mirror lock is per-repo  ", now())
            .await
            .expect("retain");
        assert_eq!(view.identity, "bob", "the identity comes from the run");
        assert_eq!(view.ticket, "MT-9", "the ticket comes from the run");
        assert_eq!(view.run_id, "7");
        assert_eq!(view.document_id, "run-7", "§5.1's document_id shape");

        // And it really landed in bob's bank, not alice's.
        let got = mem.recall("bob", "mirror lock").await.expect("recall");
        assert_eq!(got.facts.len(), 1);
        assert_eq!(got.facts[0].content, "the mirror lock is per-repo");
        assert!(
            mem.recall("alice", "mirror lock")
                .await
                .expect("recall")
                .facts
                .is_empty(),
            "a retain must not reach another identity's bank"
        );
    }

    /// A run that is not bound — finished, never dispatched, or dispatched
    /// without an identity — cannot retain. There is no identity to attribute
    /// the record to, and guessing one would be the forgery the design forbids.
    #[tokio::test]
    async fn an_unbound_run_cannot_retain() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        assert_eq!(
            mem.retain_for_run(7, "anything", now()).await,
            Err(TeamsMemoryError::NotRunning)
        );

        mem.bind_run(7, bound("alice", "MT-1"));
        mem.retain_for_run(7, "recorded", now())
            .await
            .expect("retain");
        mem.release_run(7);
        assert_eq!(
            mem.retain_for_run(7, "too late", now()).await,
            Err(TeamsMemoryError::NotRunning),
            "a released run must not keep retaining"
        );
    }

    /// A run with no identity is never bound at all, so `bind_run` is the gate
    /// rather than a later check that could be forgotten.
    #[tokio::test]
    async fn a_run_with_no_identity_is_never_bound() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("", "MT-1"));
        mem.bind_run(0, bound("alice", "MT-1"));
        assert_eq!(
            mem.retain_for_run(7, "x", now()).await,
            Err(TeamsMemoryError::NotRunning)
        );
        assert_eq!(mem.roster().expect("roster").roster[0].live_runs, 0);
    }

    /// The roster reports who exists AND what each is doing, derived from the
    /// live bindings (§6.7).
    #[tokio::test]
    async fn the_roster_derives_status_from_live_runs() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice"), ident("bob")]));
        mem.bind_run(1, bound("alice", "MT-1"));
        mem.bind_run(2, bound("alice", "MT-2"));
        mem.bind_run(3, bound("bob", "MT-3"));

        let view = mem.roster().expect("roster");
        assert_eq!(view.backend, "local");
        assert_eq!(view.roster.len(), 2);
        assert_eq!(view.roster[0].name, "alice");
        assert_eq!(view.roster[0].profile, "swe");
        assert_eq!(view.roster[0].bank, "agent-alice", "the derived bank id");
        assert_eq!(view.roster[0].live_runs, 2);
        assert_eq!(view.roster[0].tickets, vec!["MT-1", "MT-2"]);
        assert_eq!(view.roster[1].live_runs, 1);

        mem.release_run(1);
        assert_eq!(mem.roster().expect("roster").roster[0].live_runs, 1);
    }

    /// Invalidate stores the reason and takes the record out of recall,
    /// reversibly (§5.3) — and REFUSES a reasonless invalidation, which is the
    /// exact shape of the Go client's measured 400.
    #[tokio::test]
    async fn invalidate_requires_a_reason_and_removes_the_fact_from_recall() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-1"));
        let id = mem
            .retain_for_run(7, "MT-1 needs a follow-up", now())
            .await
            .expect("retain")
            .id;

        assert!(matches!(
            mem.invalidate("alice", &id, "   ").await,
            Err(TeamsMemoryError::Invalid(_))
        ));

        let view = mem
            .invalidate("alice", &id, "the follow-up shipped in MT-2")
            .await
            .expect("invalidate");
        assert!(view.invalidated);
        assert_eq!(view.reason, "the follow-up shipped in MT-2");
        assert!(
            mem.recall("alice", "follow-up")
                .await
                .expect("recall")
                .facts
                .is_empty(),
            "an invalidated fact leaves recall"
        );

        // Twice is a no-op, not a failure.
        let again = mem
            .invalidate("alice", &id, "still wrong")
            .await
            .expect("invalidate");
        assert!(!again.invalidated);
    }

    /// **Querying by ticket identifier must work** — it is the most obvious use
    /// of `teams_recall`, and the one an agent reaches for first.
    ///
    /// It did not: the scorer only counts title tokens of 4+ characters, so
    /// `"MT-9"` splits into `mt` and `9`, both below the threshold, and the tool
    /// returned NOTHING while the fact sat right there in the bank. Silently
    /// empty, with no error to notice. The query is therefore offered as the
    /// ticket too, which an exact `f.ticket` match scores outright.
    #[tokio::test]
    async fn recall_by_ticket_identifier_finds_the_fact() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-9"));
        mem.retain_for_run(7, "the mirror lock is per-repo", now())
            .await
            .expect("retain");

        for query in ["MT-9", "mt-9", "MT-9 mirror lock"] {
            let got = mem.recall("alice", query).await.expect("recall");
            assert_eq!(
                got.facts.len(),
                1,
                "recalling by ticket {query:?} must find the fact, got {got:?}"
            );
        }
        // A short query that names nothing still matches nothing — offering the
        // query as a ticket must not turn recall into "return everything".
        assert!(
            mem.recall("alice", "zz-1")
                .await
                .expect("recall")
                .facts
                .is_empty(),
            "an unrelated ticket query must still recall nothing"
        );
    }

    /// A missing record is a NotFound, distinguishable from a backend failure so
    /// the HTTP layer can answer 404 rather than 500.
    #[tokio::test]
    async fn invalidating_an_unknown_record_is_not_found() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        assert!(matches!(
            mem.invalidate("alice", "20260101T000000Z-run-1", "why")
                .await,
            Err(TeamsMemoryError::NotFound(_))
        ));
    }

    /// Teams OFF refuses every entry point. The MCP facade removes the tools
    /// entirely, so this is the daemon-side backstop for a direct HTTP call.
    #[tokio::test]
    async fn teams_off_refuses_every_entry_point() {
        let dir = TempDir::new();
        let mem = local(&dir, Arc::new(Teams::disabled()));
        mem.bind_run(7, bound("alice", "MT-1"));
        assert_eq!(mem.roster(), Err(TeamsMemoryError::Disabled));
        assert_eq!(
            mem.recall("alice", "q").await,
            Err(TeamsMemoryError::Disabled)
        );
        assert_eq!(
            mem.invalidate("alice", "id", "why").await,
            Err(TeamsMemoryError::Disabled)
        );
        assert_eq!(
            mem.retain_for_run(7, "x", now()).await,
            Err(TeamsMemoryError::Disabled)
        );
    }

    /// `backend: none` accepts the calls and stores nothing — §5.4's "routing
    /// and profiles with no memory" — without any special-casing above the
    /// trait.
    #[tokio::test]
    async fn the_none_backend_stores_nothing_but_never_errors() {
        let mut teams = Teams {
            enabled: true,
            roster: vec![ident("alice")],
            ..Teams::disabled()
        };
        teams.memory.backend = rhapsody_config::teams::MemoryBackend::None;
        let mem = TeamsMemory::new(Arc::new(teams), Arc::new(NoneBackend));
        mem.bind_run(7, bound("alice", "MT-1"));

        mem.retain_for_run(7, "vanishes", now())
            .await
            .expect("retain");
        assert!(
            mem.recall("alice", "vanishes")
                .await
                .expect("recall")
                .facts
                .is_empty()
        );
        assert_eq!(mem.roster().expect("roster").backend, "none");
    }

    /// An empty or whitespace-only body is refused rather than stored: a record
    /// with no content is pure turn-1 cost forever.
    #[tokio::test]
    async fn an_empty_retain_is_refused() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-1"));
        assert!(matches!(
            mem.retain_for_run(7, "   \n ", now()).await,
            Err(TeamsMemoryError::Invalid(_))
        ));
    }

    /// `commit_sha` is best-effort: a run whose worktree is not a git repository
    /// still retains, with an empty commit rather than a failed record (§5.1's
    /// "never fatal").
    #[tokio::test]
    async fn a_missing_worktree_still_retains_without_a_commit() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-1".to_string(),
                workspace_dir: dir.child("no-such-worktree"),
            },
        );
        let view = mem
            .retain_for_run(7, "recorded anyway", now())
            .await
            .expect("retain");
        assert_eq!(view.commit_sha, "");
        assert_eq!(
            mem.recall("alice", "recorded anyway")
                .await
                .expect("recall")
                .facts
                .len(),
            1
        );
    }
}
