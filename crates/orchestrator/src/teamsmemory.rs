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
//! * `teams_reinstate` — put one invalidated record back (STUDIO-689).
//! * `teams_roster` — who exists, and what each of them is doing right now.
//! * `teams_post` — append a host-stamped message to the room (STUDIO-653, T6).
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

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_config::memory::{Fact, MemoryBackend, MemoryError, Query, RecallState, Record};
use rhapsody_config::room::{
    AUDIENCE_ROOM, Cursor, Message, OPERATOR_IDENTITY, RoomError, RoomLog,
};
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

/// `GET /api/v1/teams` — the ONE view the dashboard renders (STUDIO-652).
///
/// A superset of [`RosterView`] rather than a replacement for it: `teams_roster` is an agent-facing
/// MCP tool whose payload is a contract, so the dashboard gets its own view instead of growing that
/// one. What it adds over the roster is the two facts an operator needs to read the roster
/// correctly — *how* tickets are assigned (`manager_mode`) and *whether* anything is remembered
/// (`backend`) — which are otherwise invisible in the app.
///
/// `enabled` is always `true` here, because a Teams-off daemon answers this route
/// `teams_disabled` and never reaches this struct. It is serialised anyway so a client reads one
/// unambiguous shape rather than inferring the feature's state from an HTTP status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TeamsView {
    pub enabled: bool,
    /// `off` | `labels` | `labels+model` — the wire spelling `teams.yaml` uses.
    pub manager_mode: String,
    /// Who takes a ticket nothing matched; empty ⇒ run without an identity.
    pub default_identity: String,
    /// `none` | `local` | `hindsight`.
    pub backend: String,
    pub roster: Vec<RosterRow>,
}

/// `GET /api/v1/teams/recall`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecallView {
    pub identity: String,
    /// Which states this answer was allowed to contain: `valid` (the default and
    /// what an agent asks for), `invalidated`, or `all` (STUDIO-689). Echoed
    /// back so a reader can tell "this bank holds no corrections" from "you did
    /// not ask for them".
    pub state: String,
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

/// `GET /api/v1/teams/room` (STUDIO-650, T5).
///
/// A **read-only peek** at the room, and deliberately not a catch-up: it advances no identity's
/// cursor. Cursors belong to hydration, so a mid-run peek by one teammate must never eat another
/// run's catch-up — the ticket's own words, and the reason this shares `LocalRoom::read_since` but
/// never `Cursors::save`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoomView {
    /// Oldest first, bounded by the room's own window.
    pub messages: Vec<Message>,
    /// Log lines that could not be parsed — reported rather than hidden, so "skipped loudly" is
    /// true of the API as well as of the log.
    pub skipped: Vec<String>,
}

/// `POST /api/v1/runs/{id}/post` (STUDIO-653, T6) — the host-stamped message echoed back, so the
/// agent can see exactly what was written on its behalf and to whom.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PostView {
    /// The room log's `file:seq` id for the appended line (§0.11.4).
    pub id: String,
    /// **Host-stamped**: the identity the posting run is wearing, never anything the body carried
    /// (§0.11.4 — "a run cannot supply it").
    pub from: String,
    /// The wire audience: a teammate's name, or [`AUDIENCE_ROOM`] for the room.
    pub to: String,
    /// The host clock at append time, rendered EXACTLY as the log line renders it (RFC 3339,
    /// second precision, `Z`). A `DateTime<Utc>` here would echo sub-second precision the log
    /// never stored, so this response and a later `GET /api/v1/teams/room` would disagree about
    /// the timestamp of the same message.
    pub at: String,
    /// The refs the poster attached (§0.10 — what proves it).
    pub refs: Vec<String>,
    /// How many LIVE runs the direct message also reached in their mailbox, wearing the teammate
    /// wrap. `0` for a room post, and `0` for a direct post whose recipient is not running or whose
    /// mailbox is full — both of which degrade to catch-up with nothing queued and nothing retried
    /// (§0.5). `0` is also what a busy control task answers within
    /// `ControlHandle::record_teams_post`'s bounded wait: this field says what the host could
    /// CONFIRM, never that the room log is missing anything.
    pub delivered: i64,
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

/// `POST /api/v1/teams/reinstate` (STUDIO-689) — the mirror of
/// [`InvalidateView`], one field shorter.
///
/// There is no `reason` because a reinstate clears the stored one: the record
/// goes back to being indistinguishable from one that was never corrected, which
/// is what makes §5.3's "nothing is deleted" worth anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReinstateView {
    pub identity: String,
    pub fact_id: String,
    /// `false` ⇒ the record was already valid (a no-op, not a failure).
    pub reinstated: bool,
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

impl From<RoomError> for TeamsMemoryError {
    fn from(e: RoomError) -> Self {
        match e {
            RoomError::Invalid(m) => TeamsMemoryError::Invalid(m),
            RoomError::Io(m) => TeamsMemoryError::Backend(m),
        }
    }
}

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
    /// The room `teams_room_read` serves (STUDIO-650, T5). `None` when there is no on-disk runtime
    /// home to anchor `~/.rhapsody/teams/room/` to, in which case the endpoint answers as an empty
    /// room rather than an error: a room nobody has posted to and a room that cannot exist read
    /// the same, and neither is a failure.
    room: Option<Arc<dyn RoomLog>>,
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
            room: None,
        }
    }

    /// Attaches the room `teams_room_read` serves (STUDIO-650, T5). Creates nothing: a
    /// [`LocalRoom`](rhapsody_config::room::LocalRoom) names paths only, and this endpoint only
    /// ever reads.
    pub fn with_room(mut self, room: Arc<dyn RoomLog>) -> Self {
        self.room = Some(room);
        self
    }

    /// `GET /api/v1/teams/room` — the newest posts in the room, bounded (STUDIO-650, T5).
    ///
    /// **Read-only in the strongest sense: it advances NO identity's cursor.** Catch-up belongs to
    /// hydration, where the composer earns the watermark from what it actually rendered; a tool
    /// read that advanced a cursor would let a mid-run peek eat another run's catch-up and silently
    /// hide a hand-off from the teammate it was addressed to. That is why this reads from
    /// [`Cursor::default`] every time and never touches `Cursors`.
    ///
    /// The reader is deliberately the empty string — a room-wide peek is not any identity, so it
    /// sees room-audience posts and no direct ones (T6 is where teammate direct posts start to
    /// exist at all).
    pub fn room(&self, limit: usize) -> Result<RoomView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        // No room configured reads as an empty one: a room nobody has posted to and a room that
        // cannot exist are the same answer, and neither is a failure.
        let Some(room) = self.room.as_ref() else {
            return Ok(RoomView::default());
        };
        let got = room.read_since("", &Cursor::default(), limit)?;
        Ok(RoomView {
            messages: got.messages,
            skipped: got
                .skipped
                .into_iter()
                .map(|(line, why)| format!("{line}: {why}"))
                .collect(),
        })
    }

    /// `POST /api/v1/runs/{id}/post` — the room's WRITE side (STUDIO-653, T6; §0.5, §0.10,
    /// §0.11.4).
    ///
    /// **Everything unforgeable about a post is decided here.** The agent supplies `body`, an
    /// optional `to` and optional `refs`; the host resolves `run_id` → the identity that run was
    /// dispatched as and stamps it as `from`, stamps `at` from its own clock, and appends through
    /// [`LocalRoom::append`](rhapsody_config::room::LocalRoom::append) — the single writer, which
    /// also owns the body cap. There is no argument, and no body key, by which a run can name
    /// itself something else, and a run wearing no identity is never bound and so cannot post at
    /// all.
    ///
    /// `to` absent or [`AUDIENCE_ROOM`] is the room. Any other value must name a roster member:
    /// an unknown name is [`TeamsMemoryError::Invalid`] pointing at `teams_roster`, **never** a
    /// silent downgrade to a room post — a message the author believed was private must not
    /// quietly become public.
    ///
    /// This does the ROOM half only. The best-effort mirrors — the `teams.message` timeline row
    /// and the teammate-wrapped delivery into a live recipient's mailbox — need loop-owned state
    /// and live in [`crate::teamspost`], applied by the caller after this returns.
    pub fn post_for_run(
        &self,
        run_id: i64,
        body: &str,
        to: &str,
        refs: &[String],
        now: DateTime<Utc>,
    ) -> Result<PostView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(TeamsMemoryError::Invalid(
                "body is required: say what you want the team to know".to_string(),
            ));
        }
        // The binding is the whole authorisation: no entry (or an entry with no identity, which
        // `bind_run` refuses to store) ⇒ there is nobody to attribute the post to, and guessing is
        // exactly the forgery §0.11.4 rules out.
        let Some(prov) = self.read_runs().get(&run_id).cloned() else {
            return Err(TeamsMemoryError::NotRunning);
        };
        let to = to.trim();
        if !(to.is_empty() || to == AUDIENCE_ROOM)
            && !self.teams.roster.iter().any(|i| i.name == to)
        {
            return Err(TeamsMemoryError::Invalid(format!(
                "no teammate named `{to}` is on this roster: call teams_roster for the names, or                  omit `to` to post to the whole room"
            )));
        }
        // A post needs somewhere to land. Unlike a READ — where no room configured is an empty
        // room, because a room nobody posted to reads the same — a write that cannot be recorded
        // must say so rather than report success over a message that went nowhere.
        let Some(room) = self.room.as_ref() else {
            return Err(TeamsMemoryError::Backend(
                "the team room has no on-disk home on this daemon, so there is nowhere to post"
                    .to_string(),
            ));
        };
        let msg = Message::addressed(&prov.identity, to, now, body).with_refs(refs.iter().cloned());
        let id = room.append(&msg)?;
        Ok(PostView {
            id,
            from: prov.identity,
            to: msg.to.as_wire().to_string(),
            // The log's own rendering, so this view and a later room read agree byte for byte.
            at: msg.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            refs: msg.refs,
            delivered: 0,
        })
    }

    /// `POST /api/v1/teams/room` — the room's HUMAN door (STUDIO-661; §0.5, §0.11.4).
    ///
    /// The operator has no run, so there is no identity to resolve: the daemon stamps
    /// [`OPERATOR_IDENTITY`] as `from`, and the request carries no `from` field at all — exactly
    /// like `teams_retain` and `teams_post` carry none. That is what makes the name unforgeable
    /// here, and it is why [`RESERVED_IDENTITIES`](rhapsody_config::room::RESERVED_IDENTITIES)
    /// keeps a roster entry from wearing it.
    ///
    /// **Room-wide only in v1.** There is deliberately no `to`: direct-to-a-live-run from the
    /// operator already exists as the operator-message mailbox
    /// (`POST /api/v1/runs/{id}/message`), which is the *authoritative* channel, and an async
    /// direct note to a sleeping teammate is an unproven need. If it is ever wanted, it is an
    /// additive `to` on this same body — the log already carries the field.
    ///
    /// **No timeline row, and no mirrors.** §0.10's resolution writes a `teams.message` events row
    /// as the *run's* timeline record; an operator post is not run-scoped, `events.run_id` is
    /// `NOT NULL REFERENCES runs(id)`, and inventing a run to hang it on would be a lie in the
    /// ledger. §0.5 named this case precisely — "a post not tied to a run … goes to a file log" —
    /// so the file log is the whole of it. Nothing is delivered either: §0.2's room never
    /// dispatches, and a live teammate catches this up on its next turn like everyone else.
    pub fn post_as_operator(
        &self,
        body: &str,
        refs: &[String],
        now: DateTime<Utc>,
    ) -> Result<PostView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(TeamsMemoryError::Invalid(
                "body is required: say what you want the team to know".to_string(),
            ));
        }
        // Same call `post_for_run` makes: a write that cannot be recorded must say so rather than
        // report success over a message that went nowhere.
        let Some(room) = self.room.as_ref() else {
            return Err(TeamsMemoryError::Backend(
                "the team room has no on-disk home on this daemon, so there is nowhere to post"
                    .to_string(),
            ));
        };
        let msg = Message::room(OPERATOR_IDENTITY, now, body).with_refs(refs.iter().cloned());
        let id = room.append(&msg)?;
        Ok(PostView {
            id,
            from: OPERATOR_IDENTITY.to_string(),
            to: msg.to.as_wire().to_string(),
            // The log's own rendering, so this view and a later room read agree byte for byte.
            at: msg.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            refs: msg.refs,
            // Nothing was delivered anywhere: an operator post is room-wide, and the room is a log
            // rather than a bus (§0.5).
            delivered: 0,
        })
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

    /// `GET /api/v1/teams` — the roster plus the two settings that make it legible
    /// (STUDIO-652). Built ON TOP of [`roster`](TeamsMemory::roster), so the dashboard and the
    /// `teams_roster` tool can never report different derived status.
    pub fn overview(&self) -> Result<TeamsView, TeamsMemoryError> {
        let roster = self.roster()?;
        Ok(TeamsView {
            enabled: self.teams.enabled,
            manager_mode: manager_mode_name(&self.teams),
            default_identity: self.teams.manager.default_identity.clone(),
            backend: roster.backend,
            roster: roster.roster,
        })
    }

    /// Recalls an identity's memory for a free-text `query` (§6.7's
    /// `teams_recall {identity, query}` — the memory-first path, no live turn).
    ///
    /// `state` is the wire spelling of [`RecallState`] — empty or `valid` for
    /// what an agent may see, `invalidated` for the corrections alone, `all` for
    /// the bank as it is on disk (STUDIO-689). Every caller that predates the
    /// parameter passes `""` and keeps the old answer exactly.
    pub async fn recall(
        &self,
        identity: &str,
        query: &str,
        state: &str,
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
        // Loud rather than lenient: a mistyped filter served with the valid
        // records reads as a bank nobody has ever corrected.
        let Some(state) = RecallState::parse(state) else {
            return Err(TeamsMemoryError::Invalid(format!(
                "state {state:?} is not one of valid, invalidated, all"
            )));
        };
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
            // An EMPTY query is not a search that matches nothing — it is "what does this
            // teammate remember", the question the dashboard's memory listing asks before anyone
            // can notice a wrong fact and invalidate it (§5.2.3, STUDIO-652). Still bounded by
            // `recall_top_k`: browse widens what matches, never how much comes back.
            browse: query.trim().is_empty(),
            state,
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
            state: state.as_str().to_string(),
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

    /// Puts one invalidated record back into recall (STUDIO-689) — §5.3's
    /// reversal, reachable from the same surface the invalidate was made on.
    ///
    /// There is no `reason` argument, deliberately: a reinstate *drops* the
    /// stored reason with the correction it explained, so the record is again
    /// indistinguishable from one that was never invalidated. That asymmetry
    /// with [`invalidate`](TeamsMemory::invalidate) is the point — a correction
    /// has to be justified, undoing one restores the original and justifies
    /// nothing new.
    pub async fn reinstate(
        &self,
        identity: &str,
        fact_id: &str,
    ) -> Result<ReinstateView, TeamsMemoryError> {
        if !self.enabled() {
            return Err(TeamsMemoryError::Disabled);
        }
        let (identity, fact_id) = (identity.trim(), fact_id.trim());
        if identity.is_empty() || fact_id.is_empty() {
            return Err(TeamsMemoryError::Invalid(
                "identity and fact_id are required".to_string(),
            ));
        }
        let reinstated = self.backend.revalidate(identity, fact_id).await?;
        Ok(ReinstateView {
            identity: identity.to_string(),
            fact_id: fact_id.to_string(),
            reinstated,
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

/// The configured manager mode's name, for the overview view — the `teams.yaml` wire spelling,
/// so what the app shows is what an operator would type into the file.
fn manager_mode_name(teams: &Teams) -> String {
    match teams.manager.mode {
        rhapsody_config::teams::ManagerMode::Off => "off",
        rhapsody_config::teams::ManagerMode::Labels => "labels",
        rhapsody_config::teams::ManagerMode::LabelsModel => "labels+model",
    }
    .to_string()
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

    /// The same memory, with a real [`LocalRoom`](rhapsody_config::room::LocalRoom) attached —
    /// what the composition root builds, and what the room's read and write sides both need.
    fn with_room(dir: &TempDir, teams: Arc<Teams>) -> TeamsMemory {
        local(dir, teams).with_room(Arc::new(rhapsody_config::room::LocalRoom::new(
            dir.child("room"),
        )))
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

    /// **The dashboard's one view** (STUDIO-652): the roster plus the two settings that make it
    /// legible. The derived status is the roster's own, not a second computation of it.
    #[tokio::test]
    async fn overview_reports_the_roster_plus_manager_mode_and_backend() {
        let dir = TempDir::new();
        let teams = Arc::new(Teams {
            enabled: true,
            manager: rhapsody_config::teams::Manager {
                mode: rhapsody_config::teams::ManagerMode::LabelsModel,
                default_identity: "alice".to_string(),
                ..rhapsody_config::teams::Manager::default()
            },
            roster: vec![ident("alice"), ident("bob")],
            ..Teams::disabled()
        });
        let mem = local(&dir, teams);
        mem.bind_run(7, bound("alice", "MT-9"));
        mem.bind_run(8, bound("alice", "MT-4"));

        let view = mem.overview().expect("overview");
        assert!(view.enabled);
        assert_eq!(view.manager_mode, "labels+model");
        assert_eq!(view.default_identity, "alice");
        assert_eq!(view.backend, "local");
        assert_eq!(view.roster.len(), 2);
        assert_eq!(view.roster[0].name, "alice");
        assert_eq!(view.roster[0].bank, "agent-alice");
        assert_eq!(view.roster[0].live_runs, 2);
        assert_eq!(view.roster[0].tickets, vec!["MT-4", "MT-9"], "sorted");
        assert_eq!(view.roster[1].live_runs, 0, "bob is idle");
        assert_eq!(
            mem.roster().expect("roster").roster,
            view.roster,
            "the overview must not compute derived status a second way"
        );
    }

    /// Teams off ⇒ the overview is `teams_disabled`, exactly like every other Teams surface.
    #[tokio::test]
    async fn overview_is_disabled_when_teams_is_off() {
        let mem = TeamsMemory::new(Arc::new(Teams::disabled()), Arc::new(NoneBackend));
        assert_eq!(mem.overview().expect_err("off"), TeamsMemoryError::Disabled);
    }

    /// **An empty query lists the bank** (STUDIO-652, §5.2.3): the browse the dashboard's memory
    /// panel needs before anyone can notice a wrong fact. A non-empty query still searches.
    #[tokio::test]
    async fn an_empty_recall_query_browses_the_whole_bank() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-9"));
        for content in ["the mirror lock is per-repo", "goldens are recaptured only"] {
            mem.retain_for_run(7, content, now()).await.expect("retain");
        }

        let browsed = mem.recall("alice", "   ", "").await.expect("recall");
        assert_eq!(
            browsed.facts.len(),
            2,
            "an empty query lists everything the bank holds: {browsed:?}"
        );

        let searched = mem.recall("alice", "goldens", "").await.expect("recall");
        assert_eq!(searched.facts.len(), 1, "a real query still searches");
        assert_eq!(searched.facts[0].content, "goldens are recaptured only");
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
        let got = mem.recall("bob", "mirror lock", "").await.expect("recall");
        assert_eq!(got.facts.len(), 1);
        assert_eq!(got.facts[0].content, "the mirror lock is per-repo");
        assert!(
            mem.recall("alice", "mirror lock", "")
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
            mem.recall("alice", "follow-up", "")
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

    /// STUDIO-689: the correction is undoable through the daemon, not only in
    /// the file format — and the record that comes back is the original, with
    /// the reason it was invalidated for dropped.
    #[tokio::test]
    async fn reinstate_puts_an_invalidated_fact_back_into_recall() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-1"));
        let id = mem
            .retain_for_run(7, "MT-1 needs a follow-up", now())
            .await
            .expect("retain")
            .id;
        mem.invalidate("alice", &id, "the follow-up shipped in MT-2")
            .await
            .expect("invalidate");

        let view = mem.reinstate("alice", &id).await.expect("reinstate");
        assert!(view.reinstated);
        assert_eq!(view.fact_id, id);
        let back = mem.recall("alice", "follow-up", "").await.expect("recall");
        assert_eq!(back.facts.len(), 1, "the fact is recalled again");
        assert_eq!(
            back.facts[0].reason, "",
            "the reason goes with the correction it explained"
        );

        // Twice is a no-op, not a failure — the mirror of invalidate's.
        let again = mem.reinstate("alice", &id).await.expect("reinstate");
        assert!(!again.reinstated);

        assert!(matches!(
            mem.reinstate("alice", "   ").await,
            Err(TeamsMemoryError::Invalid(_))
        ));
        assert!(matches!(
            mem.reinstate("alice", "20260101T000000Z-run-9").await,
            Err(TeamsMemoryError::NotFound(_))
        ));
    }

    /// STUDIO-689: an invalidated record is listable on request, so the
    /// operator UI can show a correction made in an earlier session — while the
    /// DEFAULT stays valid-only, which is the state an agent may see.
    #[tokio::test]
    async fn recall_serves_invalidated_records_only_when_the_state_asks() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        mem.bind_run(7, bound("alice", "MT-1"));
        let kept = mem
            .retain_for_run(7, "the mirror lock is per repo", now())
            .await
            .expect("retain")
            .id;
        let dropped = mem
            .retain_for_run(7, "the mirror lock is global", now())
            .await
            .expect("retain")
            .id;
        mem.invalidate("alice", &dropped, "MT-2 measured it per repo")
            .await
            .expect("invalidate");

        let ids = |view: &RecallView| {
            let mut out: Vec<String> = view.facts.iter().map(|f| f.id.clone()).collect();
            out.sort();
            out
        };

        let default = mem.recall("alice", "", "").await.expect("recall");
        assert_eq!(default.state, "valid");
        assert_eq!(
            ids(&default),
            vec![kept.clone()],
            "the default is unchanged"
        );

        let corrections = mem
            .recall("alice", "", "invalidated")
            .await
            .expect("recall");
        assert_eq!(corrections.state, "invalidated");
        assert_eq!(ids(&corrections), vec![dropped.clone()]);
        assert_eq!(
            corrections.facts[0].reason, "MT-2 measured it per repo",
            "the reason travels with the listed record"
        );

        let all = mem.recall("alice", "", "all").await.expect("recall");
        assert_eq!(all.state, "all");
        let mut want = vec![kept, dropped];
        want.sort();
        assert_eq!(ids(&all), want);

        // Loud, not lenient: a mistyped filter answered with the valid records
        // would read as a bank nobody has ever corrected.
        assert!(matches!(
            mem.recall("alice", "", "Invalidated").await,
            Err(TeamsMemoryError::Invalid(_))
        ));
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
            let got = mem.recall("alice", query, "").await.expect("recall");
            assert_eq!(
                got.facts.len(),
                1,
                "recalling by ticket {query:?} must find the fact, got {got:?}"
            );
        }
        // A short query that names nothing still matches nothing — offering the
        // query as a ticket must not turn recall into "return everything".
        assert!(
            mem.recall("alice", "zz-1", "")
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
            mem.recall("alice", "q", "").await,
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
            mem.recall("alice", "vanishes", "")
                .await
                .expect("recall")
                .facts
                .is_empty()
        );
        assert_eq!(mem.roster().expect("roster").backend, "none");
    }

    /// The room's HUMAN door (STUDIO-661): an operator post lands in the log with `from:
    /// "operator"` — the reserved name the daemon stamps because there is no run to resolve an
    /// identity through — and it reads back through the same room view every other post does.
    #[tokio::test]
    async fn an_operator_post_is_stamped_operator_and_reads_back_room_wide() {
        let dir = TempDir::new();
        let mem = with_room(&dir, teams_on(vec![ident("alice")]));

        let view = mem
            .post_as_operator(
                "  prefer the retry queue for STUDIO-6xx  ",
                &["STUDIO-661".to_string()],
                now(),
            )
            .expect("operator post");
        assert_eq!(view.from, OPERATOR_IDENTITY);
        assert_eq!(view.to, AUDIENCE_ROOM, "v1 is room-wide only");
        assert_eq!(view.refs, vec!["STUDIO-661".to_string()]);
        assert_eq!(view.delivered, 0, "the room is a log, not a bus");
        assert!(!view.id.is_empty(), "the log stamps a file:seq id");

        let read = mem.room(0).expect("room reads back");
        assert_eq!(read.messages.len(), 1, "{read:?}");
        let m = &read.messages[0];
        assert_eq!(m.from, OPERATOR_IDENTITY);
        assert_eq!(m.to.as_wire(), AUDIENCE_ROOM);
        assert_eq!(
            m.body, "prefer the retry queue for STUDIO-6xx",
            "the body is trimmed, exactly as a teammate's is"
        );
        assert_eq!(m.id, view.id, "the echoed id is the log's own");
        assert_eq!(
            m.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            view.at,
            "the echoed timestamp renders exactly as the log stored it"
        );
    }

    /// An empty body is a `bad_request`, not an empty line in the log: the acceptance criterion,
    /// and the same rule `post_for_run` applies.
    #[tokio::test]
    async fn an_empty_operator_post_is_refused() {
        let dir = TempDir::new();
        let mem = with_room(&dir, teams_on(vec![ident("alice")]));
        for body in ["", "   \n\t "] {
            assert!(
                matches!(
                    mem.post_as_operator(body, &[], now()),
                    Err(TeamsMemoryError::Invalid(_))
                ),
                "an empty body must be refused: {body:?}"
            );
        }
        assert!(
            mem.room(0).expect("room").messages.is_empty(),
            "a refused post writes nothing"
        );
    }

    /// Teams off ⇒ `teams_disabled`, like every other Teams entry point — and nothing is created
    /// on disk, because the refusal happens before the room is ever touched (§2.4).
    #[tokio::test]
    async fn an_operator_post_is_disabled_when_teams_is_off() {
        let dir = TempDir::new();
        let room_dir = dir.child("room");
        let mem = local(&dir, Arc::new(Teams::disabled()))
            .with_room(Arc::new(rhapsody_config::room::LocalRoom::new(&room_dir)));
        assert_eq!(
            mem.post_as_operator("hello", &[], now()),
            Err(TeamsMemoryError::Disabled)
        );
        assert!(
            !std::path::Path::new(&room_dir).exists(),
            "a disabled daemon creates no room directory"
        );
    }

    /// A daemon with no on-disk room says so rather than reporting success over a message that
    /// went nowhere. The READ side answers an empty room in the same situation, deliberately: a
    /// room nobody posted to reads the same, but a write that cannot land is not a success.
    #[tokio::test]
    async fn an_operator_post_with_no_room_is_a_backend_error() {
        let dir = TempDir::new();
        let mem = local(&dir, teams_on(vec![ident("alice")]));
        assert!(matches!(
            mem.post_as_operator("hello", &[], now()),
            Err(TeamsMemoryError::Backend(_))
        ));
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
            mem.recall("alice", "recorded anyway", "")
                .await
                .expect("recall")
                .facts
                .len(),
            1
        );
    }
}
