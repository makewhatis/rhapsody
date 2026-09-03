//! teamsknow — the manager's **team-scoped knowledge read accessor** (STUDIO-729, slice 1 of the
//! answering-manager design record `~/.rhapsody/docs/answering-manager-design.md`, §9.5).
//! **No Go v0.4.0 counterpart:** Teams is Rhapsody-only and never seeded, so nothing here is
//! golden-checked.
//!
//! # What this is for
//!
//! The manager today knows exactly one thing — [`EarsCycle::issues`](crate::teamsears::EarsCycle),
//! the tickets its own project trackers returned. That is why *"what was the result of
//! STUDIO-725?"* got silence: a ticket that reached a terminal state has fallen out of the cycle,
//! and nothing else on the ears path can reach the run that worked it. Slice 3 will let the model
//! turn answer such a question; this slice builds the **only surface it may read**.
//!
//! # Team-scoped by construction, not by care
//!
//! §9.1 named the fatal hole: the store is **one per daemon**. [`RunFilter`] and
//! [`Store::issue_history`] filter by `project` and nothing else — no notion of a Rhapsody team
//! exists down there — so a bare `issue_history("RHAP-42")` on a two-team daemon hands team A a run
//! that belongs to team B. STUDIO-668 pins "no cross-team anything", and a room is an
//! unauthenticated shared log, so that leak would be permanent the moment it was posted.
//!
//! The answer is not a check at the call site. A [`Knowledge`] cannot be built without a
//! [`TeamScope`], every read filters through it, and **every returned row is re-checked against it
//! after the store answers** — the store's `project` filter is treated as an optimisation, never as
//! the guard, because the empty slug legitimately means *no filter at all* (`RunFilter::project`'s
//! own contract, and the shape a legacy `tracker:` config still writes). A key that resolves
//! off-team returns nothing at all: no row, no field, no "exists but not yours". Slice 2 turns that
//! nothing into the operator-facing degradation line.
//!
//! # The fact-source allowlist (§9.4)
//!
//! Four sources, and no fifth: the projected [`RunFact`] subset of a [`RunSummary`], recall
//! [`Fact::content`], the cycle's [`Issue`] fields, and the [`RoomLog`]. **No config struct is a
//! fact source.** That is enforced by the constructor rather than by a rule: nothing here accepts a
//! [`Teams`](rhapsody_config::teams::Teams), a `Memory` or a tracker — the caller hands in a
//! project-slug set, an identity set and an already-resolved bank map, none of which can carry an
//! `api_key`, an endpoint or a tracker credential.
//!
//! # The projection is the leak guard (§9.3, ANS-FIELD-LEAK)
//!
//! [`RunSummary`] carries `error`, `transcript_path`, `session_uuid`, `branch` and `repo`. Those are
//! console fields — the console is authenticated and the room is not — so the accessor never returns
//! a `RunSummary` at all. It returns [`RunFact`], which has four fields and cannot grow one by
//! accident: a reviewer adding a field to `RunSummary` changes nothing here.
//!
//! # Recall is pinned, roster-scoped, and refuses a shared bank (§9.3, ANS-MEM-SCOPE)
//!
//! Three separate guards, because they fail differently:
//!
//! 1. **Pinned to [`RecallState::Valid`]** on the way in *and* filtered on the way out. A
//!    corrected fact must never reach the room, and a backend is a trait — [`Query::state`] is a
//!    request, not a promise, and the remote `hindsight` backend is on the far side of a network.
//! 2. **Roster-scoped:** an identity outside this team's roster recalls nothing. A bank belongs to
//!    exactly one identity and an identity to exactly one team (STUDIO-668 §B.3), so the roster set
//!    *is* the memory partition.
//! 3. **No shared-bank cross-read:** a roster `bank:` override
//!    ([`LocalBank::with_bank_overrides`](rhapsody_config::memory::LocalBank::with_bank_overrides))
//!    lets two identities name the same bank id. If the other claimant is outside this team, the
//!    override would be a cross-team read wearing an in-team identity's name, so the recall is
//!    refused outright rather than filtered afterwards.
//!
//! # Errors are values, and an absence is not an error
//!
//! Every method returns a [`Result`]: a store or bank failure is the caller's to log and degrade
//! from, not this module's to swallow. An off-team, unknown or unrecorded identifier is `Ok` with
//! nothing in it — "I have no record of that" and "the read failed" are different answers and the
//! manager must be able to tell them apart.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rhapsody_config::memory::{Fact, MemoryBackend, MemoryError, Query, RecallState, STATE_VALID};
use rhapsody_config::room::{Cursor, Message, RoomError, RoomLog};
use rhapsody_core::Issue;
use rhapsody_store::{RunFilter, RunSummary, Store, StoreError};

use crate::teams::IDENTITY_LABEL_PREFIX;

/// The most history rows one gather may pull, per project slug. §9.3's ANS-BUDGET-TRUNC bounds the
/// GATHER as well as the prompt: a ticket in a retry loop has dozens of runs and an answer needs
/// the newest few, so the cap is here rather than only at the render.
pub const MAX_HISTORY_ROWS: i64 = 20;

/// The most roster identities one [`Knowledge::recall_team`] may fan out over. A recall is a
/// directory scan per identity, and the manager answers on the triage cycle's budget.
pub const MAX_RECALL_IDENTITIES: usize = 8;

/// The most room posts one gather may pull. [`RoomLog::read_since`] clamps to
/// [`MAX_ROOM_WINDOW`](rhapsody_config::room::MAX_ROOM_WINDOW) on its own; this is the accessor's
/// own, tighter bound on what a single answer may be composed from.
pub const MAX_ROOM_FACTS: usize = 20;

/// Why a knowledge read failed. Sentinel prefixes follow
/// [`TeamsMemoryError`](crate::teamsmemory::TeamsMemoryError)'s convention, so a caller can log the
/// reason verbatim.
#[derive(thiserror::Error, Debug)]
pub enum KnowledgeError {
    #[error("knowledge_store_error: {0}")]
    Store(String),
    #[error("knowledge_memory_error: {0}")]
    Memory(String),
    #[error("knowledge_room_error: {0}")]
    Room(String),
}

impl From<StoreError> for KnowledgeError {
    fn from(e: StoreError) -> Self {
        KnowledgeError::Store(e.to_string())
    }
}

impl From<MemoryError> for KnowledgeError {
    fn from(e: MemoryError) -> Self {
        KnowledgeError::Memory(e.to_string())
    }
}

impl From<RoomError> for KnowledgeError {
    fn from(e: RoomError) -> Self {
        KnowledgeError::Room(e.to_string())
    }
}

/// **The projected run — the ONLY run fields that may reach a room reply** (§9.3, ANS-FIELD-LEAK).
///
/// Four fields, deliberately. `error` is an agent string that routinely carries a path or a stack;
/// `transcript_path` and `session_uuid` name on-disk artefacts; `repo` and `branch` describe the
/// daemon's checkout. All five are console-only, and the console is authenticated while the room is
/// a plain shared log. For an error body the answer points the operator at the console.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFact {
    /// The ticket the run worked — `RunSummary::issue_identifier`.
    pub key: String,
    /// `completed` / `failed` / `interrupted` / … , verbatim from the store.
    pub outcome: String,
    /// RFC3339, empty while the run is still going.
    pub ended_at: String,
    /// The teammate who wore the ticket, from its `rhapsody:@<name>` label — empty when the ticket
    /// is no longer in the cycle (the `runs` table has no identity column; the routing decision
    /// lives in the label) or when the label names somebody off this team's roster.
    pub identity: String,
}

/// **The projected cycle ticket.** The tracker's own fields, minus the description: a description is
/// unbounded prose that no answer needs and that would dominate a bounded facts block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueFact {
    pub key: String,
    pub title: String,
    pub state: String,
    /// The roster identity wearing this ticket, or empty.
    pub identity: String,
}

/// **The team's identity, reconstructed for every read** (§9.1).
///
/// Built from three things the caller already has and none of which can carry a credential: the
/// resolved project slugs the team owns, the team's roster identity names, and the DAEMON-WIDE
/// identity → bank-id map ([`TeamsMemory::bank_ids`](crate::teamsmemory::TeamsMemory::bank_ids)).
///
/// The bank map is taken whole rather than derived here for the reason
/// [`bank_id_for`](rhapsody_config::memory::bank_id_for) exists: a second copy of the
/// `<prefix><name>`-unless-overridden rule is exactly how the roster's override ends up honoured by
/// one reader and ignored by another. It must be the same resolution the backend was built with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamScope {
    projects: BTreeSet<String>,
    identities: BTreeSet<String>,
    /// In-team identity → its bank id. An identity absent from the caller's map is absent here too,
    /// and so can never be recalled from: an unresolvable bank is not a readable one.
    banks: BTreeMap<String, String>,
    /// Bank ids claimed by an identity OUTSIDE this team — the ones a `bank:` override must not be
    /// allowed to reach.
    foreign_banks: BTreeSet<String>,
    /// Linear team UUIDs (`RunSummary::team_id`) this team's work may come from. **Empty is the
    /// normal case and means "do not gate on it"** — `team_id` is the LINEAR team, orthogonal to the
    /// Rhapsody team↔project binding, and a Rhapsody team's projects may legitimately span Linear
    /// teams. §9.1 names it as a second drop condition, so it is available; it is never the primary
    /// guard, which is the project slug.
    linear_teams: BTreeSet<String>,
}

impl TeamScope {
    /// The scope for the team owning `projects`, whose roster is `identities`.
    ///
    /// `banks` is the daemon-wide identity → bank-id map, and every identity in it that is NOT in
    /// `identities` is a **foreign claimant** whose bank this team may not read.
    ///
    /// A bank two identities of the SAME team share is not foreign and is not refused: §9.3's rule
    /// is about crossing a team boundary, and identities partition along team lines (STUDIO-668
    /// §B.3), so a deliberately shared team bank is inside the partition it names.
    pub fn new<P, I>(projects: P, identities: I, banks: &HashMap<String, String>) -> TeamScope
    where
        P: IntoIterator,
        P::Item: Into<String>,
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let projects: BTreeSet<String> = projects.into_iter().map(Into::into).collect();
        let identities: BTreeSet<String> = identities.into_iter().map(Into::into).collect();

        let mut mine: BTreeMap<String, String> = BTreeMap::new();
        let mut foreign: BTreeSet<String> = BTreeSet::new();
        for (identity, bank) in banks {
            if identities.contains(identity) {
                mine.insert(identity.clone(), bank.clone());
            } else {
                foreign.insert(bank.clone());
            }
        }

        TeamScope {
            projects,
            identities,
            banks: mine,
            foreign_banks: foreign,
            linear_teams: BTreeSet::new(),
        }
    }

    /// Additionally drops any store row whose `team_id` is not one of these Linear team UUIDs.
    /// Off by default — see [`TeamScope::linear_teams`].
    pub fn with_linear_teams<T>(mut self, teams: T) -> TeamScope
    where
        T: IntoIterator,
        T::Item: Into<String>,
    {
        self.linear_teams = teams.into_iter().map(Into::into).collect();
        self
    }

    /// The project slugs this team owns.
    pub fn projects(&self) -> impl Iterator<Item = &str> {
        self.projects.iter().map(String::as_str)
    }

    /// This team's roster identity names.
    pub fn identities(&self) -> impl Iterator<Item = &str> {
        self.identities.iter().map(String::as_str)
    }

    /// Whether a store row belongs to this team — the drop rule §9.1 requires, applied to every row
    /// the store returns regardless of what filter produced it.
    pub fn admits_run(&self, r: &RunSummary) -> bool {
        if !self.projects.contains(&r.project_slug) {
            return false;
        }
        self.linear_teams.is_empty() || self.linear_teams.contains(&r.team_id)
    }

    /// Whether `identity` is on this team's roster.
    pub fn admits_identity(&self, identity: &str) -> bool {
        self.identities.contains(identity)
    }

    /// Whether `identity`'s bank may be read: it must be on the roster, must resolve to a bank at
    /// all, and that bank must not also be claimed by an identity outside this team.
    pub fn admits_bank(&self, identity: &str) -> bool {
        match self.banks.get(identity) {
            Some(bank) => !self.foreign_banks.contains(bank),
            None => false,
        }
    }
}

/// The read-only knowledge surface, over ONE team's scope.
///
/// Borrowed rather than owning: it is built for the length of one ears pass, over handles the
/// caller already holds, and holding nothing means it can never outlive the cycle whose issue
/// snapshot it reads.
pub struct Knowledge<'a> {
    scope: &'a TeamScope,
    /// The cycle's already-fetched candidate issues — the same validation set every action intent
    /// uses, reused here so a live ticket costs no tracker call.
    issues: &'a [Issue],
    store: &'a (dyn Store + Send + Sync),
    memory: &'a dyn MemoryBackend,
    room: Option<&'a dyn RoomLog>,
}

impl<'a> Knowledge<'a> {
    /// Binds a scope to the sources it may read. There is no constructor that omits the scope.
    pub fn new(
        scope: &'a TeamScope,
        issues: &'a [Issue],
        store: &'a (dyn Store + Send + Sync),
        memory: &'a dyn MemoryBackend,
    ) -> Knowledge<'a> {
        Knowledge {
            scope,
            issues,
            store,
            memory,
            room: None,
        }
    }

    /// Attaches the team's room. The handle IS the scope — rooms are per team on disk
    /// (`teams/room/<team>/`), so there is nothing further to filter.
    pub fn with_room(mut self, room: &'a dyn RoomLog) -> Knowledge<'a> {
        self.room = Some(room);
        self
    }

    /// The scope every read on this accessor is filtered through.
    pub fn scope(&self) -> &TeamScope {
        self.scope
    }

    /// Every run of `identifier` **on this team's projects**, newest first, projected.
    ///
    /// Empty — not an error — when the key belongs to another team, to no project this team owns,
    /// or to nothing the store has ever seen. Those three are indistinguishable on purpose: a
    /// distinguishable "exists but not yours" is itself the leak.
    pub fn issue_runs(&self, identifier: &str, limit: i64) -> Result<Vec<RunFact>, KnowledgeError> {
        let limit = clamp_rows(limit);
        let mut rows: Vec<RunSummary> = Vec::new();
        for slug in &self.scope.projects {
            rows.extend(self.store.issue_history(identifier, slug, limit)?);
        }
        Ok(self.project_rows(rows, limit))
    }

    /// This team's most recent runs, newest first, projected.
    pub fn recent_runs(&self, limit: i64) -> Result<Vec<RunFact>, KnowledgeError> {
        let limit = clamp_rows(limit);
        let mut rows: Vec<RunSummary> = Vec::new();
        for slug in &self.scope.projects {
            rows.extend(self.store.list_runs(RunFilter {
                project: slug.clone(),
                limit,
                ..RunFilter::default()
            })?);
        }
        Ok(self.project_rows(rows, limit))
    }

    /// The cycle ticket with this identifier, projected. `None` for a key this team's own trackers
    /// did not return — the same validation set every action intent is bounded by.
    pub fn issue(&self, key: &str) -> Option<IssueFact> {
        let iss = self
            .issues
            .iter()
            .find(|i| i.identifier.eq_ignore_ascii_case(key))?;
        Some(IssueFact {
            key: iss.identifier.clone(),
            title: iss.title.clone(),
            state: iss.state.clone(),
            identity: self.wearer(&iss.identifier),
        })
    }

    /// `identity`'s VALID facts matching `q`, bounded by `q.top_k`.
    ///
    /// Empty for an off-roster identity, for one whose bank cannot be resolved, and for one whose
    /// bank another team also claims. [`Query::state`] is overridden to [`RecallState::Valid`] and
    /// the result is filtered again, because a backend is a trait and the request is not a promise.
    pub async fn recall(&self, identity: &str, q: &Query) -> Result<Vec<Fact>, KnowledgeError> {
        if !self.scope.admits_identity(identity) || !self.scope.admits_bank(identity) {
            return Ok(Vec::new());
        }
        let q = Query {
            state: RecallState::Valid,
            ..q.clone()
        };
        let recalled = self.memory.recall(identity, &q).await?;
        Ok(recalled
            .facts
            .into_iter()
            .filter(|f| f.state == STATE_VALID)
            .collect())
    }

    /// The same recall across this team's roster, capped at [`MAX_RECALL_IDENTITIES`] identities.
    pub async fn recall_team(&self, q: &Query) -> Result<Vec<Fact>, KnowledgeError> {
        let mut out: Vec<Fact> = Vec::new();
        for identity in self.scope.identities.iter().take(MAX_RECALL_IDENTITIES) {
            out.extend(self.recall(identity, q).await?);
        }
        Ok(out)
    }

    /// The newest room posts, oldest first, bounded by [`MAX_ROOM_FACTS`].
    ///
    /// **Advances no cursor.** Catch-up is hydration's job and belongs to the identity that earned
    /// it; a read that moved a watermark would let an answer eat a teammate's unread hand-off.
    pub fn room(&self, limit: usize) -> Result<Vec<Message>, KnowledgeError> {
        let Some(room) = self.room else {
            return Ok(Vec::new());
        };
        // Zero is the caller asking for the default, never for one post: that is what
        // `RoomLog::read_since` itself does with a non-positive window, and what `clamp_rows` does
        // with a non-positive row limit. A `clamp(1, _)` here would quietly disagree with both.
        let limit = if limit == 0 {
            MAX_ROOM_FACTS
        } else {
            limit.min(MAX_ROOM_FACTS)
        };
        Ok(room.read_since("", &Cursor::default(), limit)?.messages)
    }

    /// The roster identity wearing `key`'s ticket, or empty. Off-roster labels read as empty for
    /// the reason [`crate::teamsears`] refuses to act on them: §0.11.1 leaves a label the manager
    /// did not author alone, and that includes not reporting its name as a teammate.
    fn wearer(&self, key: &str) -> String {
        self.issues
            .iter()
            .find(|i| i.identifier.eq_ignore_ascii_case(key))
            .and_then(|iss| {
                iss.labels.iter().flatten().find_map(|l| {
                    let name = l.strip_prefix(IDENTITY_LABEL_PREFIX)?;
                    self.scope.admits_identity(name).then(|| name.to_string())
                })
            })
            .unwrap_or_default()
    }

    /// Drops every off-team row, restores the store's newest-first order across the per-slug
    /// queries, bounds the result, and projects it.
    ///
    /// The drop is applied HERE, after the store answered, and not only through
    /// [`RunFilter::project`]: an empty slug means "no project filter" to the store, so a team that
    /// legitimately owns the empty slug (a legacy `tracker:` config) would otherwise be handed every
    /// row on the box.
    fn project_rows(&self, mut rows: Vec<RunSummary>, limit: i64) -> Vec<RunFact> {
        rows.retain(|r| self.scope.admits_run(r));
        // The store orders by (started_at DESC, id DESC); one merged list of per-slug pages has to
        // be put back into that order, and `id` breaks a same-instant tie exactly as SQLite does.
        rows.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        rows.dedup_by_key(|r| r.id);
        rows.truncate(limit.max(0) as usize);
        rows.iter()
            .map(|r| RunFact {
                key: r.issue_identifier.clone(),
                outcome: r.outcome.clone(),
                ended_at: r.ended_at.clone(),
                identity: self.wearer(&r.issue_identifier),
            })
            .collect()
    }
}

/// A non-positive row limit is the caller asking for the default, never for "unbounded" — the same
/// rule [`Query::top_k`] and `memory.recall_top_k` follow.
fn clamp_rows(limit: i64) -> i64 {
    if limit <= 0 {
        MAX_HISTORY_ROWS
    } else {
        limit.min(MAX_HISTORY_ROWS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use rhapsody_config::memory::{
        DEFAULT_BANKS_SUBDIR, LocalBank, MemoryBackend, NoneBackend, Recalled, Record,
        STATE_INVALIDATED,
    };
    use rhapsody_config::room::{LocalRoom, Message};
    use rhapsody_config::teams::{Identity, Memory as MemoryCfg, Teams};
    use rhapsody_store::{Noop, RunEnd, RunStart, Sqlite, StorePath};

    use crate::testsupport::TempDir;

    /// The api_key / endpoint a leak test looks for. Deliberately distinctive.
    const SECRET_KEY: &str = "sk-live-NEVER-IN-THE-ROOM";
    const SECRET_ENDPOINT: &str = "https://hindsight.internal.example/v1";

    fn store() -> Arc<Sqlite> {
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"))
    }

    /// The common shape: a run of `key` on `project`, started at `started`.
    fn run_start(key: &str, project: &str, started: &str) -> RunStart {
        RunStart {
            issue_id: format!("id-{key}"),
            issue_identifier: key.to_string(),
            title: format!("{key} title"),
            branch: format!("symphony/{key}"),
            started_at: started.to_string(),
            project_slug: project.to_string(),
            ..RunStart::default()
        }
    }

    /// Seeds one ENDED run. The tests read it back through the accessor, which is the only surface
    /// under test — the store is real (in-memory SQLite), never a fake that could agree with a bug.
    fn seed(st: &Sqlite, start: RunStart, outcome: &str, end: RunEnd) -> i64 {
        let id = st.start_run(start).expect("start run");
        st.end_run(
            id,
            RunEnd {
                outcome: outcome.to_string(),
                ended_at: "2026-09-01T12:00:00Z".to_string(),
                ..end
            },
        )
        .expect("end run");
        id
    }

    /// The same, for a run with nothing interesting in its console-only fields.
    fn seed_ok(st: &Sqlite, key: &str, project: &str, started: &str, outcome: &str) -> i64 {
        seed(
            st,
            run_start(key, project, started),
            outcome,
            RunEnd::default(),
        )
    }

    fn banks(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The scope a single-project team gets: one slug, one roster, banks derived the way
    /// `TeamsMemory::new` derives them.
    fn scope_of(projects: &[&str], identities: &[&str]) -> TeamScope {
        let derived: HashMap<String, String> = identities
            .iter()
            .map(|n| (n.to_string(), format!("agent-{n}")))
            .collect();
        TeamScope::new(
            projects.iter().map(|s| s.to_string()),
            identities.iter().map(|s| s.to_string()),
            &derived,
        )
    }

    fn issue_with(key: &str, state: &str, labels: &[&str]) -> Issue {
        Issue {
            id: format!("id-{key}"),
            identifier: key.to_string(),
            title: format!("{key} title"),
            state: state.to_string(),
            labels: Some(labels.iter().map(|l| l.to_string()).collect()),
            ..Issue::default()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000, 0).expect("timestamp")
    }

    /// A backend that answers with an INVALIDATED fact whatever [`Query::state`] asks for — the
    /// remote-backend case the pin alone cannot cover.
    struct LyingBank;

    #[async_trait]
    impl MemoryBackend for LyingBank {
        async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
            Ok(String::new())
        }

        async fn recall(&self, identity: &str, _q: &Query) -> Result<Recalled, MemoryError> {
            Ok(Recalled {
                facts: vec![
                    Fact {
                        id: "corrected".into(),
                        identity: identity.to_string(),
                        state: STATE_INVALIDATED.into(),
                        reason: "wrong".into(),
                        content: "the corrected claim".into(),
                        ..Fact::default()
                    },
                    Fact {
                        id: "good".into(),
                        identity: identity.to_string(),
                        state: STATE_VALID.into(),
                        content: "the standing claim".into(),
                        ..Fact::default()
                    },
                ],
                skipped: Vec::new(),
            })
        }

        async fn invalidate(
            &self,
            _identity: &str,
            _fact_id: &str,
            _reason: &str,
        ) -> Result<bool, MemoryError> {
            Ok(false)
        }

        async fn revalidate(&self, _identity: &str, _fact_id: &str) -> Result<bool, MemoryError> {
            Ok(false)
        }
    }

    // --- §9.1 ANS-SCOPE-LEAK -----------------------------------------------------------------

    /// **The ANS-SCOPE-LEAK gate.** One daemon, one store, two teams. Team A asks for team B's
    /// TERMINAL ticket — the exact question ("what was the result of X?") that motivated the whole
    /// design — and gets nothing back: no row, and therefore no field.
    #[test]
    fn a_query_for_another_teams_key_returns_nothing() {
        let st = store();
        seed_ok(&st, "AAA-1", "alpha", "2026-09-01T10:00:00Z", "completed");
        seed(
            &st,
            RunStart {
                session_uuid: "uuid-b".into(),
                transcript_path: "/tmp/b/transcript.jsonl".into(),
                repo: "git@github.com:acme/beta.git".into(),
                team_id: "".into(),
                ..run_start("BBB-2", "beta", "2026-09-01T11:00:00Z")
            },
            "failed",
            RunEnd {
                error: "team B's private failure".into(),
                transcript_path: "/tmp/b/transcript.jsonl".into(),
                ..RunEnd::default()
            },
        );

        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert!(
            k.issue_runs("BBB-2", 0).expect("issue_runs").is_empty(),
            "team A resolved team B's terminal key through the global store"
        );
        let recent = k.recent_runs(0).expect("recent_runs");
        assert_eq!(
            recent.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["AAA-1"],
            "team A's recent runs must contain only its own projects"
        );
        // The whole rendered answer, not just the keys: no field of team B's row may survive.
        let rendered = format!("{recent:?}");
        for leaked in ["BBB-2", "team B's private failure", "uuid-b", "beta"] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} leaked into team A's answer: {rendered}"
            );
        }

        // The same question with the store's own `project` filter NEUTRALISED. A team that owns the
        // legacy empty slug as well as a real one sends `RunFilter::project: ""` — which means "no
        // filter" to the store — so this pass is answered by the accessor's own drop and by nothing
        // else. Without that drop, team B's row comes straight back.
        let scope = scope_of(&["", "alpha"], &["alice"]);
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        assert!(
            k.issue_runs("BBB-2", 0).expect("issue_runs").is_empty(),
            "the accessor leaned on the store's project filter instead of its own drop"
        );
        let recent = k.recent_runs(0).expect("recent_runs");
        assert_eq!(
            recent.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["AAA-1"],
            "an unfiltered store page must still be dropped to the team's own projects"
        );
    }

    /// A team that owns no project reads nothing — the degenerate case that would otherwise become
    /// "no project filter", which is the store's own meaning for an empty `RunFilter::project`.
    #[test]
    fn a_team_owning_no_project_reads_no_run() {
        let st = store();
        seed_ok(&st, "AAA-1", "alpha", "2026-09-01T10:00:00Z", "completed");
        let scope = scope_of(&[], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert!(k.issue_runs("AAA-1", 0).expect("issue_runs").is_empty());
        assert!(k.recent_runs(0).expect("recent_runs").is_empty());
    }

    /// The legacy shape: a team bound to the EMPTY slug sees the unattributed rows and nothing else,
    /// even though the store reads an empty `project` filter as "every row".
    #[test]
    fn a_team_on_the_legacy_empty_slug_sees_only_unattributed_rows() {
        let st = store();
        seed_ok(&st, "LEG-1", "", "2026-09-01T10:00:00Z", "completed");
        seed_ok(&st, "BBB-2", "beta", "2026-09-01T11:00:00Z", "failed");
        let scope = scope_of(&[""], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let recent = k.recent_runs(0).expect("recent_runs");
        assert_eq!(
            recent.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["LEG-1"]
        );
        assert!(k.issue_runs("BBB-2", 0).expect("issue_runs").is_empty());
    }

    /// The optional second drop condition §9.1 names. Off by default; on, it drops a row whose
    /// LINEAR team is not one of the team's, even when the project slug matches.
    #[test]
    fn the_linear_team_gate_drops_a_row_from_another_linear_team() {
        let st = store();
        seed(
            &st,
            RunStart {
                session_uuid: "".into(),
                transcript_path: "".into(),
                repo: "".into(),
                team_id: "linear-a".into(),
                ..run_start("AAA-1", "alpha", "2026-09-01T10:00:00Z")
            },
            "completed",
            RunEnd {
                error: "".into(),
                transcript_path: "".into(),
                ..RunEnd::default()
            },
        );
        seed(
            &st,
            RunStart {
                session_uuid: "".into(),
                transcript_path: "".into(),
                repo: "".into(),
                team_id: "linear-b".into(),
                ..run_start("AAA-2", "alpha", "2026-09-01T11:00:00Z")
            },
            "completed",
            RunEnd {
                error: "".into(),
                transcript_path: "".into(),
                ..RunEnd::default()
            },
        );
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();

        let open = scope_of(&["alpha"], &["alice"]);
        let k = Knowledge::new(&open, &issues, st.as_ref(), &none);
        assert_eq!(k.recent_runs(0).expect("recent_runs").len(), 2);

        let gated = scope_of(&["alpha"], &["alice"]).with_linear_teams(["linear-a"]);
        let k = Knowledge::new(&gated, &issues, st.as_ref(), &none);
        assert_eq!(
            k.recent_runs(0)
                .expect("recent_runs")
                .iter()
                .map(|r| r.key.as_str())
                .collect::<Vec<_>>(),
            vec!["AAA-1"]
        );
    }

    // --- §9.3 ANS-FIELD-LEAK -----------------------------------------------------------------

    /// The projection carries four fields and no fifth: none of `error`, `transcript_path`,
    /// `session_uuid`, `branch` or `repo` can reach a room reply.
    #[test]
    fn the_projected_run_omits_error_transcript_session_and_repo() {
        let st = store();
        seed(
            &st,
            RunStart {
                session_uuid: "5f2c-uuid".into(),
                transcript_path: "/home/david/.rhapsody/transcripts/AAA-1.jsonl".into(),
                repo: "git@github.com:acme/alpha.git".into(),
                team_id: "".into(),
                ..run_start("AAA-1", "alpha", "2026-09-01T10:00:00Z")
            },
            "failed",
            RunEnd {
                error: "panicked at src/lib.rs:42".into(),
                transcript_path: "/home/david/.rhapsody/transcripts/AAA-1.jsonl".into(),
                ..RunEnd::default()
            },
        );
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-1", "Done", &["rhapsody:@alice"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let runs = k.issue_runs("AAA-1", 0).expect("issue_runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0],
            RunFact {
                key: "AAA-1".into(),
                outcome: "failed".into(),
                ended_at: "2026-09-01T12:00:00Z".into(),
                identity: "alice".into(),
            }
        );
        let rendered = format!("{runs:?}");
        for leaked in [
            "panicked at src/lib.rs:42",
            "/home/david/.rhapsody/transcripts/AAA-1.jsonl",
            "5f2c-uuid",
            "git@github.com:acme/alpha.git",
            "symphony/AAA-1",
        ] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} reached the projection: {rendered}"
            );
        }
    }

    /// A `rhapsody:@` label naming somebody who is not on THIS team's roster is not reported as a
    /// teammate — §0.11.1's "a label the manager did not author is left alone" extends to reading.
    #[test]
    fn an_off_roster_label_is_not_reported_as_a_teammate() {
        let st = store();
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![
            issue_with("AAA-1", "In Progress", &["rhapsody:@mallory"]),
            issue_with("AAA-2", "In Progress", &["rhapsody:@alice"]),
        ];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert_eq!(k.issue("AAA-1").expect("issue").identity, "");
        assert_eq!(k.issue("AAA-2").expect("issue").identity, "alice");
        assert!(
            k.issue("BBB-9").is_none(),
            "an off-cycle key resolves to nothing"
        );
    }

    // --- §9.3 ANS-MEM-SCOPE ------------------------------------------------------------------

    /// An invalidated record never reaches an answer — pinned on the way in, and filtered on the
    /// way out so a backend that ignores the pin cannot smuggle one through.
    #[tokio::test]
    async fn recall_never_returns_an_invalidated_record() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        for (doc, content) in [
            ("run-1", "the standing claim"),
            ("run-2", "the wrong claim"),
        ] {
            bank.retain(&Record {
                identity: "alice".into(),
                document_id: doc.into(),
                ticket: "AAA-1".into(),
                run_id: doc.into(),
                at: now(),
                content: content.into(),
                ..Record::default()
            })
            .expect("retain");
        }
        let wrong = bank
            .recall(
                "alice",
                &Query {
                    browse: true,
                    ..Query::default()
                },
            )
            .expect("recall")
            .facts
            .into_iter()
            .find(|f| f.content.contains("wrong"))
            .expect("the wrong fact");
        bank.invalidate("alice", &wrong.id, "corrected")
            .expect("invalidate");

        let scope = scope_of(&["alpha"], &["alice"]);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        // Even asked for ALL states, the accessor pins Valid.
        let facts = k
            .recall(
                "alice",
                &Query {
                    browse: true,
                    state: RecallState::All,
                    ..Query::default()
                },
            )
            .await
            .expect("recall");
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert!(facts[0].content.contains("standing"));

        // And a backend that answers with an invalidated record regardless is filtered anyway.
        let lying = LyingBank;
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &lying);
        let facts = k.recall("alice", &Query::default()).await.expect("recall");
        assert_eq!(
            facts.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["good"],
            "a backend that ignored the state pin was not filtered"
        );
    }

    /// Recall is roster-scoped: an identity this team does not own reads nothing, whatever its bank
    /// holds.
    #[tokio::test]
    async fn recall_refuses_an_identity_outside_the_team() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        bank.retain(&Record {
            identity: "mallory".into(),
            document_id: "run-9".into(),
            at: now(),
            content: "team B's private note".into(),
            ..Record::default()
        })
        .expect("retain");

        let scope = scope_of(&["alpha"], &["alice"]);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        let facts = k
            .recall(
                "mallory",
                &Query {
                    browse: true,
                    ..Query::default()
                },
            )
            .await
            .expect("recall");
        assert!(facts.is_empty(), "{facts:?}");
    }

    /// A roster `bank:` override that points at a bank another team's identity also claims is a
    /// cross-team read wearing an in-team name. It is refused outright.
    #[tokio::test]
    async fn recall_refuses_a_bank_a_foreign_identity_also_claims() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-")
            .with_bank_overrides([("alice", "shared"), ("mallory", "shared")]);
        bank.retain(&Record {
            identity: "mallory".into(),
            document_id: "run-9".into(),
            at: now(),
            content: "team B's private note".into(),
            ..Record::default()
        })
        .expect("retain");

        // The DAEMON-WIDE map: alice is ours, mallory is not, and both name `shared`.
        let daemon = banks(&[("alice", "shared"), ("mallory", "shared")]);
        let scope = TeamScope::new(["alpha"], ["alice"], &daemon);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        assert!(!scope.admits_bank("alice"));
        let facts = k
            .recall(
                "alice",
                &Query {
                    browse: true,
                    ..Query::default()
                },
            )
            .await
            .expect("recall");
        assert!(
            facts.is_empty(),
            "a shared-bank override became a cross-team read: {facts:?}"
        );

        // The same override, with no foreign claimant, still reads normally.
        let daemon = banks(&[("alice", "shared")]);
        let scope = TeamScope::new(["alpha"], ["alice"], &daemon);
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);
        assert!(scope.admits_bank("alice"));
        assert_eq!(
            k.recall(
                "alice",
                &Query {
                    browse: true,
                    ..Query::default()
                }
            )
            .await
            .expect("recall")
            .len(),
            1
        );
    }

    /// Two identities of the SAME team may share one bank: the refusal is about crossing a team
    /// boundary, not about sharing.
    #[tokio::test]
    async fn recall_allows_a_bank_two_teammates_share() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-")
            .with_bank_overrides([("alice", "shared"), ("bob", "shared")]);
        bank.retain(&Record {
            identity: "bob".into(),
            document_id: "run-9".into(),
            at: now(),
            content: "a teammate's note".into(),
            ..Record::default()
        })
        .expect("retain");

        let daemon = banks(&[("alice", "shared"), ("bob", "shared")]);
        let scope = TeamScope::new(["alpha"], ["alice", "bob"], &daemon);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        assert!(scope.admits_bank("alice"));
        assert_eq!(
            k.recall(
                "alice",
                &Query {
                    browse: true,
                    ..Query::default()
                }
            )
            .await
            .expect("recall")
            .len(),
            1,
            "a bank shared inside one team is not a cross-team read"
        );
    }

    /// An identity with no resolvable bank cannot be recalled from — an unresolvable bank is not a
    /// readable one.
    #[tokio::test]
    async fn recall_refuses_an_identity_with_no_resolved_bank() {
        let scope = TeamScope::new(["alpha"], ["alice"], &HashMap::new());
        let st = store();
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        assert!(!scope.admits_bank("alice"));
        assert!(
            k.recall("alice", &Query::default())
                .await
                .expect("recall")
                .is_empty()
        );
    }

    /// The team-wide recall covers the roster and stops there.
    #[tokio::test]
    async fn recall_team_covers_only_the_team_roster() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        for who in ["alice", "bob", "mallory"] {
            bank.retain(&Record {
                identity: who.into(),
                document_id: format!("run-{who}"),
                at: now(),
                content: format!("{who}'s note"),
                ..Record::default()
            })
            .expect("retain");
        }
        let scope = scope_of(&["alpha"], &["alice", "bob"]);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        let facts = k
            .recall_team(&Query {
                browse: true,
                ..Query::default()
            })
            .await
            .expect("recall_team");
        let mut who: Vec<&str> = facts.iter().map(|f| f.identity.as_str()).collect();
        who.sort_unstable();
        assert_eq!(who, vec!["alice", "bob"]);
    }

    // --- §9.4 ANS-CONFIG-NOT-A-FACT ----------------------------------------------------------

    /// **No credential can reach the accessor's output**, because no config struct is a fact source:
    /// the scope is built from a slug set, an identity set and a resolved bank map, and a `Teams` /
    /// `Memory` / tracker struct is not accepted anywhere on this surface.
    #[tokio::test]
    async fn no_credential_can_appear_in_accessor_output() {
        let teams = Teams {
            enabled: true,
            memory: MemoryCfg {
                endpoint: SECRET_ENDPOINT.into(),
                api_key: SECRET_KEY.into(),
                ..MemoryCfg::default()
            },
            roster: vec![Identity {
                name: "alice".into(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        // Exactly what the composition root hands in — names and slugs, never the config.
        let derived: HashMap<String, String> = teams
            .roster
            .iter()
            .map(|i| {
                (
                    i.name.clone(),
                    format!("{}{}", teams.memory.bank_prefix, i.name),
                )
            })
            .collect();
        let scope = TeamScope::new(["alpha"], ["alice"], &derived);

        let st = store();
        seed_ok(&st, "AAA-1", "alpha", "2026-09-01T10:00:00Z", "completed");
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), &teams.memory.bank_prefix);
        bank.retain(&Record {
            identity: "alice".into(),
            document_id: "run-1".into(),
            at: now(),
            content: "a plain observation".into(),
            ..Record::default()
        })
        .expect("retain");
        let room = LocalRoom::new(dir.child("room"));
        room.append(&Message::room("alice", now(), "a plain post"))
            .expect("append");

        let issues = vec![issue_with("AAA-1", "Done", &["rhapsody:@alice"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank).with_room(&room);

        let rendered = format!(
            "{:?}{:?}{:?}{:?}{:?}",
            k.recent_runs(0).expect("recent_runs"),
            k.issue_runs("AAA-1", 0).expect("issue_runs"),
            k.issue("AAA-1"),
            k.recall_team(&Query {
                browse: true,
                ..Query::default()
            })
            .await
            .expect("recall_team"),
            k.room(10).expect("room"),
        );
        for secret in [SECRET_KEY, SECRET_ENDPOINT] {
            assert!(
                !rendered.contains(secret),
                "{secret:?} reached accessor output: {rendered}"
            );
        }
        assert!(rendered.contains("a plain observation"));
        assert!(rendered.contains("a plain post"));
    }

    // --- bounds + degradation ----------------------------------------------------------------

    /// The gather is bounded: a non-positive limit is the default, and no caller can ask for more
    /// than [`MAX_HISTORY_ROWS`].
    #[test]
    fn the_history_gather_is_capped() {
        let st = store();
        for n in 0..(MAX_HISTORY_ROWS + 5) {
            seed_ok(
                &st,
                "AAA-1",
                "alpha",
                &format!("2026-09-01T10:{n:02}:00Z"),
                "completed",
            );
        }
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert_eq!(
            k.issue_runs("AAA-1", 0).expect("issue_runs").len() as i64,
            MAX_HISTORY_ROWS
        );
        assert_eq!(
            k.issue_runs("AAA-1", 1_000).expect("issue_runs").len() as i64,
            MAX_HISTORY_ROWS
        );
        assert_eq!(k.issue_runs("AAA-1", 3).expect("issue_runs").len(), 3);
    }

    /// The room read is bounded and returns the posts oldest-first, and a room that was never
    /// attached reads as empty rather than as a failure.
    #[test]
    fn the_room_read_is_bounded_and_optional() {
        let dir = TempDir::new();
        let room = LocalRoom::new(dir.child("room"));
        for n in 0..(MAX_ROOM_FACTS + 4) {
            room.append(&Message::room("alice", now(), format!("post {n}")))
                .expect("append");
        }
        let scope = scope_of(&["alpha"], &["alice"]);
        let st = store();
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();

        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        assert!(k.room(10).expect("room").is_empty(), "no room attached");

        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_room(&room);
        assert_eq!(k.room(1_000).expect("room").len(), MAX_ROOM_FACTS);
        assert_eq!(k.room(3).expect("room").len(), 3);
        assert_eq!(
            k.room(0).expect("room").len(),
            MAX_ROOM_FACTS,
            "zero is the default window, not a window of one"
        );
    }

    /// Teams off / storage off / memory off: every read is an empty answer, none is an error, and
    /// nothing is created. This is the "no behaviour change" half of the acceptance.
    #[tokio::test]
    async fn every_source_disabled_reads_as_empty() {
        let scope = TeamScope::new(Vec::<String>::new(), Vec::<String>::new(), &HashMap::new());
        let st = Noop;
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, &st, &none);

        assert!(k.issue_runs("AAA-1", 0).expect("issue_runs").is_empty());
        assert!(k.recent_runs(0).expect("recent_runs").is_empty());
        assert!(k.issue("AAA-1").is_none());
        assert!(k.room(10).expect("room").is_empty());
        assert!(
            k.recall_team(&Query::default())
                .await
                .expect("recall_team")
                .is_empty()
        );
    }
}
