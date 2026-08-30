//! Rhapsody Teams **the room** — the [`RoomLog`] trait and its `local`
//! implementation (STUDIO-650, slice T5; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.5, §0.10, §0.11.4).
//!
//! # A log, not a bus (§0.5)
//!
//! Identities are durable state, not processes: subscription implies a listener
//! and nobody is home. So the room is an **append-only log read at hydration** —
//! nobody receives, everybody catches up. There is no delivery, no fan-out and
//! no daemon-side push; a post reaches a teammate when that teammate next runs.
//!
//! # Single writer, JSONL, `file:seq` (§0.11.4, pinned)
//!
//! * **The daemon is the single writer.** One daemon per machine (the run lock),
//!   so every post — triage, the unknown-identity fallback, a human, and from T6
//!   a teammate through MCP — is appended by the host. The concurrent-append
//!   problem dissolves rather than being solved with locking.
//! * **Format: append-only JSONL**, one [`Message`] per line, in day-partitioned
//!   files (`<root>/<YYYY-MM-DD>.jsonl`). `seq` is the line index within one
//!   file, [`Cursor`] is `{file, seq}`, and [`Message::id`] is `file:seq` where
//!   `file` is the log file's **stem** (so an id reads `2026-08-29:3`, not
//!   `2026-08-29.jsonl:3`).
//! * **fsync stance, stated: best-effort append.** No fsync, no fallocate, no
//!   temp+rename. The room is **advisory** — the ledger is Linear — so losing
//!   the tail of the log to a crash is acceptable and is not worth an fsync on
//!   every post. A message that never lands costs a teammate one paragraph of
//!   context; the ticket, its label and its history are unaffected.
//! * **A corrupt line is skipped loudly, never fatal.** One unparseable line
//!   never costs a reader the rest of the log; the reason travels out in
//!   [`CaughtUp::skipped`], which is T4's [`Recalled`](crate::memory::Recalled)
//!   pattern reused unchanged (`rhapsody-config` does no logging of its own, so
//!   the caller that owns the log reports it).
//! * **Cursors live in the identity's own state**, never the parity store:
//!   `<banks>/<bank-id>/cursor`, written by [`Cursors`]. A lost cursor re-reads
//!   at most the bounded window ([`MAX_ROOM_WINDOW`]), never the whole log.
//! * **`from` is stamped by the host**, resolved run → dispatched identity. This
//!   module never derives it and there is no code path by which a run can supply
//!   one — the same rule §5.1 puts on a retained record's provenance.
//!
//! # One consequence of day-partitioning, stated
//!
//! A message's log file is chosen from its own `at`, and [`Cursor`] orders files chronologically by
//! their stems. So a message appended with an `at` OLDER than a reader's watermark is one that
//! reader never catches up on. Every writer in this daemon stamps `Utc::now()`, so this needs a
//! clock that went backwards — and when it happens the room loses a paragraph while Linear, the
//! ledger, is unaffected. That is the same trade the no-fsync stance above makes, named here so it
//! is a decision rather than a surprise.
//!
//! # Sync, and why the signature is the proof (§0.10)
//!
//! [`RoomLog`] is **sync**, unlike [`MemoryBackend`](crate::memory::MemoryBackend),
//! and deliberately so: a room read happens at turn 1, on the dispatch path,
//! which runs inline on the single control task. A sync `fn` cannot `.await`, so
//! a room read can never put a network round-trip in front of a turn-1 prompt —
//! the STUDIO-551/BO-59 head-of-line class the adversarial design review
//! (`~/.rhapsody/docs/STUDIO-572-design-review.md`) forbade there. When §0.10's
//! reconsider-when trigger fires and a relay backend appears, it will not be
//! reachable from this trait, which is the point.
//!
//! # Never create anything on read
//!
//! Constructing a [`LocalRoom`] or a [`Cursors`] **names paths only**. A read
//! against a room directory that does not exist is an empty result — not an
//! error and emphatically not a `mkdir`. The room directory appears on the
//! **first append**, and an identity's cursor file is written only after a
//! catch-up that actually returned messages. Teams on but quiet therefore
//! touches no filesystem at all.
//!
//! # Module now, crate later (§0.10)
//!
//! This lives beside [`crate::memory`] rather than in a `crates/room` of its
//! own. The room has exactly one consumer, and extracting an abstraction from a
//! single instance is the mistake STUDIO-594 was cancelled over. The extraction
//! trigger is named and is not this ticket: **the day a second backend or a
//! second consumer appears** — a relay backend would drag a websocket and crypto
//! stack into whichever crate holds it, and those do not belong in `config`.
//! Because [`RoomLog`] is already the seam, that extraction is mechanical.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Where the `local` room log lives under Rhapsody's runtime home (§0.11.4:
/// `~/.rhapsody/teams/room/`). Relative to the same runtime home `teams.yaml`,
/// `teams/profiles/` and `teams/banks/` resolve against.
pub const DEFAULT_ROOM_SUBDIR: &str = "teams/room";

/// The extension of one log file. JSONL, one message per line (§0.11.4).
pub const LOG_EXT: &str = "jsonl";

/// The file an identity's room watermark is stored in, inside that identity's
/// own bank directory (§0.11.4: "cursors live in the identity's own state").
pub const CURSOR_FILE: &str = "cursor";

/// The `to` value of a room-wide post on the wire. Not a legal identity name
/// (`is_label_safe` requires a leading `[a-z]`), so it can never collide with a
/// [`Audience::Direct`] recipient.
pub const AUDIENCE_ROOM: &str = "*";

/// The `from` the daemon stamps on a post the **operator** made — through
/// `POST /api/v1/teams/room` or the dashboard's compose box (STUDIO-661).
///
/// A human post has no run to resolve an identity through (§0.5: "a post not
/// tied to a run … goes to a file log"), so the daemon stamps this reserved name
/// instead. The request carries no `from` field at all, exactly like
/// `teams_retain` and `teams_post` carry none — §0.11.4's "`from` is stamped by
/// the host" holds for the human door too.
///
/// Unlike the manager's `@manager`, this one IS label-safe, so a roster entry
/// could otherwise take it and speak with the operator's voice. That is why it
/// is reserved in [`RESERVED_IDENTITIES`] rather than left to a sigil.
pub const OPERATOR_IDENTITY: &str = "operator";

/// Names no roster identity may take, because the daemon stamps them itself
/// (STUDIO-661).
///
/// * `operator` — [`OPERATOR_IDENTITY`], the human's own voice in the room.
/// * `manager` — the routing function's voice. Triage stamps `@manager` today,
///   which `is_label_safe` already puts out of a roster's reach; `manager` is
///   reserved beside it so the *unsigil'd* spelling cannot be taken either. A
///   teammate literally named `manager` would render as one in every catch-up
///   line ("manager wrote on …") whatever the daemon spells internally, and
///   label-safe is not the same thing as identity-legal.
///
/// Existing configs are unaffected unless they already commit the sin, in which
/// case failing validation loudly is the correct outcome.
pub const RESERVED_IDENTITIES: [&str; 2] = [OPERATOR_IDENTITY, "manager"];

/// The most a single post may store, in bytes. §0.5's room "carries decisions
/// and hand-offs, not chatter"; this is the backstop that keeps a caller from
/// appending a transcript anyway. Content past the cap is truncated, never
/// rejected — a post is best-effort and never fatal to its caller.
pub const MAX_POST_BODY_BYTES: usize = 4000;

/// The most ONE caught-up message contributes to a prompt, in bytes. Applied at
/// **read** time so a log written before a cap change still renders within
/// bounds — the same rule [`MAX_FACT_CONTENT_BYTES`](crate::memory::MAX_FACT_CONTENT_BYTES)
/// follows.
pub const MAX_MESSAGE_BODY_BYTES: usize = 600;

/// The hard ceiling on how many messages one catch-up may return, whatever the
/// caller asks for. §0.5: "bounded read window, **non-negotiable**" — every
/// message read at hydration is turn-1 prompt tokens on every run, forever, so
/// an unbounded room would silently inflate the cost of every ticket.
///
/// This is also what bounds a **lost** cursor (§0.11.4): a deleted or corrupt
/// cursor file re-reads at most this many messages, never the whole log.
pub const MAX_ROOM_WINDOW: usize = 50;

/// The window used when a caller asks for no particular limit. Restated as a
/// FALLBACK rather than a minimum: `limit: 0` must not mean "everything" in one
/// place and "nothing" in another — the same trap
/// [`FALLBACK_TOP_K`](crate::memory::FALLBACK_TOP_K) exists to close.
pub const DEFAULT_ROOM_WINDOW: usize = 20;

/// The most log FILES one read walks before giving up on older history. Files
/// are day-partitioned and visited newest-first, so this drops the OLDEST days —
/// the ones a catch-up would have dropped anyway — and stops a years-old room
/// turning every dispatch into an unbounded directory walk. The analog of
/// [`MAX_BANK_SCAN`](crate::memory::MAX_BANK_SCAN).
pub const MAX_ROOM_FILE_SCAN: usize = 32;

/// Who a message is for (§0.5's "one log, two audiences").
///
/// There is deliberately no `Everyone-except` and no group: the room's whole
/// claim is that it is a log with a `to` field, not a routing table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Audience {
    /// `to: alice` — addressed to one identity. Still appended to the same log:
    /// direct-*to-live* is the mailbox (`agent_send_message`), and this is the
    /// catch-up path a not-currently-running teammate degrades to (§0.5).
    Direct(String),
    /// `to: *` — everybody catches up.
    #[default]
    Room,
}

impl Audience {
    /// The wire form: [`AUDIENCE_ROOM`] or the recipient's name.
    pub fn as_wire(&self) -> &str {
        match self {
            Audience::Direct(name) => name.as_str(),
            Audience::Room => AUDIENCE_ROOM,
        }
    }

    /// Parses the wire form. An empty `to` reads as [`Audience::Room`]: a
    /// message nobody is named on is one everybody catches up on, which is the
    /// safe direction (a lost `to` must not silently hide a decision).
    pub fn from_wire(s: &str) -> Self {
        if s.is_empty() || s == AUDIENCE_ROOM {
            Audience::Room
        } else {
            Audience::Direct(s.to_string())
        }
    }

    /// Whether `reader` catches this message up.
    ///
    /// A room post is visible to everyone **including its author** — the room is
    /// a record, not a delivery bus, and a manager reading back its own routing
    /// decisions is the durable history §0.11.7 asks for. A direct post is
    /// visible only to the identity it names, so an empty `reader` (the
    /// room-wide `teams_room_read` peek, which is not any identity) sees room
    /// posts only.
    pub fn visible_to(&self, reader: &str) -> bool {
        match self {
            Audience::Room => true,
            Audience::Direct(name) => !reader.is_empty() && name == reader,
        }
    }
}

/// One message in the room (§0.10's `Message`, plus §0.11.4's stable `id`).
///
/// `refs` is not decoration: it mirrors §5.1's rule that every retained fact
/// carries a pointer to what proves it, so a room message is **re-groundable at
/// read time** by the same machinery that re-grounds memory (§5.2). A message
/// naming a ticket is rendered with that ticket's current state attached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Message {
    /// `file:seq` — the log file's stem and the line index within it (§0.11.4).
    /// Stamped by [`LocalRoom::append`]; empty on a message being constructed
    /// for append.
    pub id: String,
    /// Who wrote it. **Host-stamped**, never caller-supplied at the tool
    /// boundary (§0.11.4).
    pub from: String,
    /// Who it is for.
    #[serde(serialize_with = "serialize_audience")]
    pub to: Audience,
    /// When the host stamped it. The log has no clock of its own — passing the
    /// time in is what makes an appended message reproducible in a test.
    pub at: DateTime<Utc>,
    /// The prose.
    pub body: String,
    /// Ticket ids, PR urls, commit SHAs — what proves it (§0.10).
    pub refs: Vec<String>,
}

fn serialize_audience<S: serde::Serializer>(a: &Audience, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(a.as_wire())
}

impl Message {
    /// A room-wide post from `from`. The `at` is passed in rather than read from
    /// a clock here, for the reason [`Message::at`] gives.
    pub fn room(from: impl Into<String>, at: DateTime<Utc>, body: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            from: from.into(),
            to: Audience::Room,
            at,
            body: body.into(),
            refs: Vec::new(),
        }
    }

    /// A post from `from` addressed by the WIRE form of `to` (STUDIO-653, T6) —
    /// a teammate's name, or `*`/empty for the room, resolved through
    /// [`Audience::from_wire`]. The caller validates the name against the
    /// roster; this only builds the message.
    ///
    /// A direct post is appended to the SAME log a room post is:
    /// [`Audience::Direct`] narrows who catches it up, it does not choose a
    /// different store. So a direct message to a teammate who is not running
    /// reaches them on their next waking, which is §0.5's degradation rather
    /// than a queue.
    pub fn addressed(
        from: impl Into<String>,
        to: &str,
        at: DateTime<Utc>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: String::new(),
            from: from.into(),
            to: Audience::from_wire(to),
            at,
            body: body.into(),
            refs: Vec::new(),
        }
    }

    /// Attaches the refs that prove it (§0.10, §5.1).
    pub fn with_refs<I: IntoIterator<Item = S>, S: Into<String>>(mut self, refs: I) -> Self {
        self.refs = refs.into_iter().map(Into::into).collect();
        self
    }
}

/// A reader's watermark: everything before `{file, seq}` has been caught up on.
///
/// `seq` is the **next unread** line index in `file`, and every log file whose
/// stem sorts before `file` is fully consumed. Stems are ISO dates, so
/// lexicographic order is chronological order and no separate index is needed.
///
/// [`Cursor::default`] — an empty `file`, `seq: 0` — is "never read anything",
/// which is also what an absent or unparseable cursor file yields. That case is
/// bounded by [`MAX_ROOM_WINDOW`] rather than by the cursor, so losing a cursor
/// costs a teammate a bounded re-read and never the whole log (§0.11.4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Cursor {
    /// The log file's stem (`2026-08-29`). Empty ⇒ before the beginning.
    pub file: String,
    /// The next unread line index within `file`.
    pub seq: u64,
}

impl Cursor {
    /// The watermark that has just consumed `msg` — parsed from its `file:seq`
    /// id. `None` when the id is not in that form, which is the only shape
    /// [`LocalRoom`] ever stamps.
    pub fn after(msg: &Message) -> Option<Self> {
        let (file, seq) = parse_id(&msg.id)?;
        Some(Self { file, seq: seq + 1 })
    }

    /// The on-disk form, which is exactly a [`Message::id`]: `file:seq`.
    pub fn as_text(&self) -> String {
        format!("{}:{}", self.file, self.seq)
    }

    /// Parses [`Cursor::as_text`]. Anything else is [`Cursor::default`] —
    /// "never read anything" — because a corrupt watermark must degrade to a
    /// bounded re-read, never to an error that blocks a dispatch.
    pub fn parse(text: &str) -> Self {
        match parse_id(text.trim()) {
            Some((file, seq)) => Self { file, seq },
            None => Self::default(),
        }
    }
}

/// Splits a `file:seq` id. `rsplit_once` rather than `split_once` so a stem that
/// somehow contained a colon still yields the sequence number.
fn parse_id(id: &str) -> Option<(String, u64)> {
    let (file, seq) = id.rsplit_once(':')?;
    if file.is_empty() {
        return None;
    }
    Some((file.to_string(), seq.parse().ok()?))
}

/// What one catch-up produced: the messages, the advanced watermark, and the
/// lines that could not be parsed.
///
/// §0.10's trait sketch returned `(Vec<Message>, Cursor)`. This is that tuple
/// plus `skipped`, for the reason [`Recalled`](crate::memory::Recalled) carries
/// the same field: "a corrupt line is skipped **loudly**, never fatal" is an
/// acceptance criterion, and `rhapsody-config` deliberately does no logging of
/// its own, so the reason has to travel to the caller that owns the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaughtUp {
    /// Oldest first — the order they are rendered in, and the order that makes
    /// "drop the oldest on overflow" a truncation of the front.
    pub messages: Vec<Message>,
    /// The watermark **after the last returned message**, or the caller's own
    /// cursor unchanged when nothing was returned. Returned with the read so
    /// catch-up cannot silently re-read or skip (§0.10).
    pub cursor: Cursor,
    /// `(line id, why)` for each line that could not be parsed.
    pub skipped: Vec<(String, String)>,
}

/// Why a room operation failed. Sentinel prefixes follow
/// [`MemoryError`](crate::memory::MemoryError)'s convention, so a caller can log
/// the reason verbatim.
#[derive(thiserror::Error, Debug)]
pub enum RoomError {
    #[error("room_io_error: {0}")]
    Io(String),
    #[error("room_invalid: {0}")]
    Invalid(String),
}

/// The room log (§0.10's trait, pinned by §0.11.4).
///
/// **Sync by construction** — see the module docs: that is what makes it
/// impossible for a room read to put network I/O in front of a turn-1 prompt,
/// and it is checkable by reading this one signature.
pub trait RoomLog: Send + Sync {
    /// Appends one host-stamped message and returns the `file:seq` id it was
    /// given.
    ///
    /// §0.10's sketch returned `()`; the id is returned because the **host**
    /// mints it (§0.11.4), so a caller that wants to log or echo what it wrote
    /// has no other way to learn it.
    ///
    /// Best-effort at every call site: a failed append is logged and the caller
    /// continues (§0.11.4's advisory-room stance).
    fn append(&self, msg: &Message) -> Result<String, RoomError>;

    /// What is new for `reader` at `cursor`, plus the advanced watermark.
    ///
    /// Returns **at most** `limit` messages (itself clamped to
    /// [`MAX_ROOM_WINDOW`]), the NEWEST ones when more are available, oldest
    /// first. Never creates anything: a room that was never written is an empty
    /// result.
    fn read_since(
        &self,
        reader: &str,
        cursor: &Cursor,
        limit: usize,
    ) -> Result<CaughtUp, RoomError>;
}

/// The JSONL wire record — one line of a log file.
///
/// A separate type from [`Message`] on purpose: `id` is positional (derived from
/// where the line sits, not stored) and `to` is a plain string on the wire, so
/// the file format stays readable by `jq` and by a human with `less`.
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    from: String,
    to: String,
    at: String,
    body: String,
    #[serde(default)]
    refs: Vec<String>,
}

/// `local` — the append-only JSONL room under `<root>/` (§0.10's resolution:
/// "the file log **is** the room's own store").
///
/// The file log was chosen over an `events`-backed room on one sufficient leg
/// (§0.11.3 struck the other): `crates/config` does not depend on
/// `rhapsody-store`, and the room lives in `config` beside memory, so an
/// `events`-backed implementation here would invert a dependency graph this port
/// has kept clean throughout. `events` still gets a `teams.message` **timeline**
/// row written by the host — that is T6's, since no teammate post exists until
/// then.
#[derive(Debug, Clone)]
pub struct LocalRoom {
    root: PathBuf,
}

impl LocalRoom {
    /// Names a room root. **Creates nothing** — not the root, not a log file.
    /// The first [`append`](LocalRoom::append) does that.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory the log files sit in. Naming it does not create it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The log file `at` belongs to: one file per UTC day, stem `YYYY-MM-DD`.
    ///
    /// Day partitioning is what keeps [`append`](LocalRoom::append)'s line count
    /// and [`read_since`](LocalRoom::read_since)'s scan bounded by a day of
    /// traffic rather than by the room's whole history, and it makes
    /// [`Cursor::file`] chronologically ordered by plain string comparison.
    fn file_stem(at: DateTime<Utc>) -> String {
        at.format("%Y-%m-%d").to_string()
    }

    fn path_for(&self, stem: &str) -> PathBuf {
        self.root.join(format!("{stem}.{LOG_EXT}"))
    }

    /// The log file stems present, oldest first. An absent root is an empty
    /// list, never an error and never a `mkdir`.
    fn stems(&self) -> Result<Vec<String>, RoomError> {
        let suffix = format!(".{LOG_EXT}");
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            // The room has never been written. Not an error, and emphatically
            // not a reason to create it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(RoomError::Io(format!(
                    "read room {}: {e}",
                    self.root.display()
                )));
            }
        };
        let mut out: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `strip_suffix`, NOT `trim_end_matches`: the latter strips EVERY
            // trailing repetition, so a stray `2026-08-29.jsonl.jsonl` would
            // report the stem `2026-08-29` and collide with the real day's file.
            if let Some(stem) = name.strip_suffix(&suffix)
                && !stem.is_empty()
            {
                out.push(stem.to_string());
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// How many lines `path` already holds — the `seq` the next append gets.
    /// An absent file is zero.
    fn line_count(path: &Path) -> Result<u64, RoomError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(text.lines().count() as u64),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(RoomError::Io(format!("read log {}: {e}", path.display()))),
        }
    }

    /// Appends one message. See [`RoomLog::append`].
    ///
    /// **This is the only method here that creates anything**, and it creates
    /// the room directory on the way — §0.11.4's "the room dir appears on the
    /// first append". The body is truncated to [`MAX_POST_BODY_BYTES`] rather
    /// than rejected.
    pub fn append(&self, msg: &Message) -> Result<String, RoomError> {
        if msg.from.is_empty() {
            return Err(RoomError::Invalid(
                "a room message must name who wrote it (`from` is host-stamped)".to_string(),
            ));
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|e| RoomError::Io(format!("create room {}: {e}", self.root.display())))?;
        let stem = Self::file_stem(msg.at);
        let path = self.path_for(&stem);
        let seq = Self::line_count(&path)?;
        let wire = Wire {
            from: msg.from.clone(),
            to: msg.to.as_wire().to_string(),
            at: msg.at.to_rfc3339_opts(SecondsFormat::Secs, true),
            body: crate::memory::truncate_bytes(&msg.body, MAX_POST_BODY_BYTES),
            refs: msg.refs.clone(),
        };
        let mut line = serde_json::to_string(&wire)
            .map_err(|e| RoomError::Invalid(format!("encode message: {e}")))?;
        line.push('\n');
        // Append, best-effort: no fsync (module docs — the room is advisory and
        // the ledger is Linear). `append(true)` is what makes the single-writer
        // daemon's writes ordered without a lock file.
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| RoomError::Io(format!("open log {}: {e}", path.display())))?;
        // A torn tail — a previous write cut short by a crash, which the no-fsync stance above
        // explicitly accepts — leaves the file without its final newline. Appending straight onto
        // it would splice this message into the broken one and lose BOTH: the corrupt line is
        // skipped loudly on read, and this good message would be inside it. One byte separates
        // them, so the damage stays confined to the line that was already lost.
        if needs_leading_newline(&path) {
            line.insert(0, '\n');
        }
        f.write_all(line.as_bytes())
            .map_err(|e| RoomError::Io(format!("append log {}: {e}", path.display())))?;
        Ok(format!("{stem}:{seq}"))
    }

    /// The catch-up for `reader`. See [`RoomLog::read_since`].
    ///
    /// Walks log files **newest first** so the bounded window costs a scan
    /// proportional to what it returns, not to the room's history: once `limit`
    /// visible messages are in hand, no older file is opened at all.
    pub fn read_since(
        &self,
        reader: &str,
        cursor: &Cursor,
        limit: usize,
    ) -> Result<CaughtUp, RoomError> {
        let limit = effective_limit(limit);
        let mut out = CaughtUp {
            cursor: cursor.clone(),
            ..CaughtUp::default()
        };
        let stems = self.stems()?;
        // Everything strictly before the cursor's file is already consumed. ISO
        // stems make this a plain string comparison.
        let mut pending: Vec<&String> = stems
            .iter()
            .filter(|s| cursor.file.is_empty() || s.as_str() >= cursor.file.as_str())
            .collect();
        // Oldest days first fall off the scan cap, which is the same direction
        // the window itself drops them.
        if pending.len() > MAX_ROOM_FILE_SCAN {
            pending.drain(..pending.len() - MAX_ROOM_FILE_SCAN);
        }

        for stem in pending.iter().rev() {
            if out.messages.len() >= limit {
                break;
            }
            let path = self.path_for(stem);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    // One unreadable day never costs the reader the others.
                    out.skipped
                        .push((format!("{stem}:*"), format!("read log: {e}")));
                    continue;
                }
            };
            // Only the cursor's own file has partially-consumed lines.
            let skip = if *stem == &cursor.file { cursor.seq } else { 0 };
            let mut batch: Vec<Message> = Vec::new();
            for (idx, line) in text.lines().enumerate() {
                if (idx as u64) < skip {
                    continue;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let id = format!("{stem}:{idx}");
                match parse_line(&id, line) {
                    Ok(msg) => {
                        if msg.to.visible_to(reader) {
                            batch.push(msg);
                        }
                    }
                    // "Skipped LOUDLY, never fatal": the line is reported, the
                    // rest of the file is read anyway (T4's `Recalled.skipped`).
                    Err(why) => out.skipped.push((id, why)),
                }
            }
            // Newest file first, so this batch is older than what is already
            // held: prepend, then drop the oldest back down to the window.
            batch.append(&mut out.messages);
            out.messages = batch;
            if out.messages.len() > limit {
                out.messages.drain(..out.messages.len() - limit);
            }
        }

        // Advance the watermark to just past what is actually being handed over
        // — never past a message the caller never saw.
        if let Some(last) = out.messages.last()
            && let Some(c) = Cursor::after(last)
        {
            out.cursor = c;
        }
        Ok(out)
    }
}

impl RoomLog for LocalRoom {
    fn append(&self, msg: &Message) -> Result<String, RoomError> {
        LocalRoom::append(self, msg)
    }

    fn read_since(
        &self,
        reader: &str,
        cursor: &Cursor,
        limit: usize,
    ) -> Result<CaughtUp, RoomError> {
        LocalRoom::read_since(self, reader, cursor, limit)
    }
}

/// Whether `path` ends mid-line, so the next append must start a fresh one. An absent or empty
/// file needs nothing; an unreadable one is treated as intact, because guessing a newline into a
/// file we cannot read would be its own corruption.
fn needs_leading_newline(path: &Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => !bytes.is_empty() && bytes.last() != Some(&b'\n'),
        Err(_) => false,
    }
}

/// The window one read may return: the caller's ask, with `0` meaning
/// [`DEFAULT_ROOM_WINDOW`] and everything clamped to [`MAX_ROOM_WINDOW`].
fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_ROOM_WINDOW.min(MAX_ROOM_WINDOW)
    } else {
        limit.min(MAX_ROOM_WINDOW)
    }
}

/// Parses one JSONL line into a [`Message`] carrying the positional `id`.
fn parse_line(id: &str, line: &str) -> Result<Message, String> {
    let wire: Wire = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if wire.from.is_empty() {
        return Err("message has no `from`".to_string());
    }
    let at = DateTime::parse_from_rfc3339(&wire.at)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| format!("bad `at`: {e}"))?;
    Ok(Message {
        id: id.to_string(),
        from: wire.from,
        to: Audience::from_wire(&wire.to),
        at,
        // Capped at READ time, so a log written before a cap change still
        // renders within bounds.
        body: crate::memory::truncate_bytes(&wire.body, MAX_MESSAGE_BODY_BYTES),
        refs: wire.refs,
    })
}

/// Where each identity's room watermark is stored: `<root>/<bank-id>/cursor`
/// (§0.11.4 — "cursors live in the identity's own state, never the parity
/// store").
///
/// The bank id is resolved by **exactly** [`LocalBank`](crate::memory::LocalBank)'s
/// rule — `bank_prefix` + name, with the roster's per-identity `bank:` override
/// winning — so a teammate's cursor lands beside that teammate's records instead
/// of in a second, differently-named directory. The two resolutions are pinned
/// against each other by `cursor_dir_matches_the_memory_bank_dir`, because they
/// are separate code (a cursor must work with `memory.backend: none`, where
/// there is no `LocalBank` at all).
#[derive(Debug, Clone)]
pub struct Cursors {
    root: PathBuf,
    bank_prefix: String,
    banks: HashMap<String, String>,
}

impl Cursors {
    /// Names a cursor root (the banks directory). **Creates nothing.**
    pub fn new(root: impl Into<PathBuf>, bank_prefix: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            bank_prefix: bank_prefix.into(),
            banks: HashMap::new(),
        }
    }

    /// Honours the roster's per-identity `bank:` overrides, on the same terms
    /// [`LocalBank::with_bank_overrides`](crate::memory::LocalBank::with_bank_overrides)
    /// does: an override that is not label-safe is dropped rather than joined,
    /// because it becomes a directory name.
    pub fn with_bank_overrides<I, K, V>(mut self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (identity, bank) in overrides {
            let (identity, bank) = (identity.into(), bank.into());
            if !bank.is_empty() && crate::teams::is_label_safe(&bank) {
                self.banks.insert(identity, bank);
            }
        }
        self
    }

    /// The directory `identity`'s cursor lives in. Naming it does not create it.
    ///
    /// The charset is re-checked here rather than trusted: this is reachable
    /// from a routed identity name, and a `..` would otherwise escape the root.
    pub fn dir(&self, identity: &str) -> Result<PathBuf, RoomError> {
        if !crate::teams::is_label_safe(identity) {
            return Err(RoomError::Invalid(format!(
                "identity {identity:?} is not label-safe (must match ^[a-z][a-z0-9-]*$)"
            )));
        }
        Ok(match self.banks.get(identity) {
            Some(bank) => self.root.join(bank),
            None => self.root.join(format!("{}{identity}", self.bank_prefix)),
        })
    }

    /// This identity's watermark, TOTAL: an absent, unreadable, or unparseable cursor is all
    /// [`Cursor::default`] — "never read anything" — which is bounded by [`MAX_ROOM_WINDOW`], never
    /// by the log's length. **Creates nothing.**
    ///
    /// Callers that need to REPORT why a present cursor could not be read use
    /// [`Cursors::try_load`]; that is [`Teams::try_load`](crate::teams::Teams::try_load)'s split,
    /// and the catch-up path uses it so an unreadable watermark is loud rather than showing up as
    /// a teammate mysteriously re-reading the same posts every run.
    pub fn load(&self, identity: &str) -> Cursor {
        self.try_load(identity).unwrap_or_default()
    }

    /// [`Cursors::load`] with the reason preserved. An ABSENT cursor is `Ok(Cursor::default())`,
    /// not an error: never having read the room is the starting state, not a failure. A present
    /// cursor that cannot be read is `Err` — the caller logs it and falls back to the bounded
    /// re-read, which is a degradation rather than a fault.
    ///
    /// An unparseable cursor is deliberately `Ok(Cursor::default())` rather than `Err`: garbage in
    /// the file is indistinguishable from a partially-written one, and both mean the same thing to
    /// a reader.
    pub fn try_load(&self, identity: &str) -> Result<Cursor, RoomError> {
        let dir = self.dir(identity)?;
        let path = dir.join(CURSOR_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Cursor::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursor::default()),
            Err(e) => Err(RoomError::Io(format!(
                "read cursor {}: {e}",
                path.display()
            ))),
        }
    }

    /// Stores this identity's watermark, creating the identity's directory.
    ///
    /// **The only method here that creates anything**, and the caller only
    /// reaches it after a catch-up that actually returned messages — which is
    /// what makes "Teams on but quiet touches no filesystem" true (module docs).
    pub fn save(&self, identity: &str, cursor: &Cursor) -> Result<(), RoomError> {
        let dir = self.dir(identity)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| RoomError::Io(format!("create bank {}: {e}", dir.display())))?;
        let path = dir.join(CURSOR_FILE);
        std::fs::write(&path, format!("{}\n", cursor.as_text()))
            .map_err(|e| RoomError::Io(format!("write cursor {}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{DEFAULT_BANKS_SUBDIR, LocalBank};
    use chrono::TimeZone;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .expect("a real instant")
    }

    fn room(dir: &Path) -> LocalRoom {
        LocalRoom::new(dir.join(DEFAULT_ROOM_SUBDIR))
    }

    fn post(r: &LocalRoom, from: &str, day: u32, hour: u32, body: &str) -> String {
        r.append(&Message::room(from, at(day, hour), body))
            .expect("append")
    }

    /// [`Message::addressed`] resolves the WIRE form of `to` (STUDIO-653, T6), so one constructor
    /// serves both audiences and a lost recipient widens rather than hides: `*` and the empty
    /// string are both the room, and any other name is a direct post only that name catches up.
    #[test]
    fn addressed_resolves_the_wire_form_of_to() {
        let msg = Message::addressed("bob", "alice", at(1, 9), "the lock moved");
        assert_eq!(msg.to, Audience::Direct("alice".to_string()));
        assert!(msg.to.visible_to("alice"));
        assert!(!msg.to.visible_to("carol"));
        assert_eq!(msg.from, "bob", "`from` is whatever the HOST passed in");
        assert!(
            msg.id.is_empty(),
            "the id is minted by the append, not here"
        );

        for room_form in ["", AUDIENCE_ROOM] {
            let msg = Message::addressed("bob", room_form, at(1, 9), "news");
            assert_eq!(msg.to, Audience::Room, "to={room_form:?}");
            assert!(msg.to.visible_to("carol"));
        }
    }

    /// §0.11.4 / the ticket's second acceptance bullet: **never create on read**.
    /// Constructing the log names paths only, and a read against a room that was
    /// never written is an empty result — not an error, and emphatically not a
    /// `mkdir`. This is what makes "Teams on but quiet touches no filesystem"
    /// checkable rather than merely intended.
    #[test]
    fn constructing_and_reading_an_absent_room_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        assert!(!r.root().exists());

        let got = r
            .read_since("alice", &Cursor::default(), 0)
            .expect("read an absent room");
        assert_eq!(got, CaughtUp::default());
        assert!(
            !r.root().exists(),
            "a read must not create the room directory"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("tempdir").count(),
            0,
            "a read must create nothing at all"
        );
    }

    /// The room directory appears on the FIRST append and at no other time
    /// (§0.11.4), and the id it hands back is `file:seq` with `seq` the line
    /// index within the day's file.
    #[test]
    fn the_room_appears_on_the_first_append_and_ids_are_file_seq() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());

        assert_eq!(post(&r, "manager", 29, 9, "first"), "2026-08-29:0");
        assert!(r.root().is_dir(), "the first append creates the room");
        assert_eq!(post(&r, "manager", 29, 10, "second"), "2026-08-29:1");
        // A new UTC day starts a new file, so `seq` restarts.
        assert_eq!(post(&r, "manager", 30, 1, "third"), "2026-08-30:0");

        let log = std::fs::read_to_string(r.root().join("2026-08-29.jsonl")).expect("read log");
        assert_eq!(log.lines().count(), 2, "one message per line: {log}");
        assert!(log.lines().all(|l| l.starts_with('{')), "JSONL: {log}");
    }

    /// The round trip: what was appended is what is caught up on, oldest first,
    /// with the host-stamped `from`, the audience and the refs intact.
    #[test]
    fn append_then_catch_up_round_trips_every_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        r.append(
            &Message::room("manager", at(29, 9), "routed STUDIO-650 to alice")
                .with_refs(["STUDIO-650"]),
        )
        .expect("append");

        let got = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(got.messages.len(), 1);
        let m = &got.messages[0];
        assert_eq!(m.id, "2026-08-29:0");
        assert_eq!(m.from, "manager");
        assert_eq!(m.to, Audience::Room);
        assert_eq!(m.at, at(29, 9));
        assert_eq!(m.body, "routed STUDIO-650 to alice");
        assert_eq!(m.refs, vec!["STUDIO-650".to_string()]);
        assert_eq!(
            got.cursor,
            Cursor {
                file: "2026-08-29".to_string(),
                seq: 1
            }
        );
    }

    /// The watermark contract (§0.10: "catch-up cannot silently re-read or
    /// skip"): reading at the returned cursor sees only what arrived since, and
    /// a second read with nothing new returns nothing and leaves the cursor
    /// where it was.
    #[test]
    fn a_second_catch_up_sees_only_news() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        post(&r, "manager", 29, 9, "one");
        post(&r, "manager", 29, 10, "two");

        let first = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(bodies(&first), vec!["one", "two"]);

        let quiet = r.read_since("alice", &first.cursor, 0).expect("read");
        assert!(quiet.messages.is_empty(), "{:?}", quiet.messages);
        assert_eq!(
            quiet.cursor, first.cursor,
            "an empty catch-up moves nothing"
        );

        post(&r, "manager", 30, 1, "three");
        let second = r.read_since("alice", &first.cursor, 0).expect("read");
        assert_eq!(bodies(&second), vec!["three"]);
    }

    /// §0.5's non-negotiable bound, from both directions: an explicit limit is
    /// honoured, `0` falls back to [`DEFAULT_ROOM_WINDOW`], and no caller can
    /// ask for more than [`MAX_ROOM_WINDOW`]. Overflow keeps the NEWEST —
    /// dropping the oldest is the room's overflow rule everywhere (§0.11.6).
    #[test]
    fn the_window_is_bounded_and_keeps_the_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        for i in 0..(MAX_ROOM_WINDOW + 10) {
            post(&r, "manager", 29, 0, &format!("m{i}"));
        }

        let three = r.read_since("alice", &Cursor::default(), 3).expect("read");
        assert_eq!(three.messages.len(), 3);
        assert_eq!(
            bodies(&three),
            vec![
                format!("m{}", MAX_ROOM_WINDOW + 7),
                format!("m{}", MAX_ROOM_WINDOW + 8),
                format!("m{}", MAX_ROOM_WINDOW + 9)
            ]
        );

        let dflt = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(dflt.messages.len(), DEFAULT_ROOM_WINDOW);

        let greedy = r
            .read_since("alice", &Cursor::default(), 10_000)
            .expect("read");
        assert_eq!(
            greedy.messages.len(),
            MAX_ROOM_WINDOW,
            "no caller may exceed the ceiling"
        );
    }

    /// §0.11.4's lost-cursor rule: a deleted cursor re-reads at most the bounded
    /// window, never the whole log. Stated here as the property the CURSOR file
    /// being absent is equivalent to — [`Cursors::load`] returns
    /// [`Cursor::default`], which is exactly the read above.
    #[test]
    fn a_lost_cursor_re_reads_at_most_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        for i in 0..(MAX_ROOM_WINDOW * 3) {
            post(&r, "manager", 29, 0, &format!("m{i}"));
        }
        let got = r
            .read_since("alice", &Cursor::default(), MAX_ROOM_WINDOW)
            .expect("read");
        assert_eq!(got.messages.len(), MAX_ROOM_WINDOW);
        assert_eq!(
            bodies(&got).last().map(String::as_str),
            Some(format!("m{}", MAX_ROOM_WINDOW * 3 - 1).as_str()),
            "the window is the NEWEST slice, not the oldest"
        );
    }

    /// §0.5's two audiences: a room post reaches everybody; a direct post
    /// reaches only the identity it names. The room-wide peek (an empty reader,
    /// which is `teams_room_read`'s) sees room posts only.
    #[test]
    fn direct_messages_reach_only_their_recipient() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        r.append(&Message::room("manager", at(29, 9), "everyone"))
            .expect("append");
        r.append(&Message {
            to: Audience::Direct("alice".to_string()),
            ..Message::room("manager", at(29, 10), "just alice")
        })
        .expect("append");

        assert_eq!(
            bodies(&r.read_since("alice", &Cursor::default(), 0).expect("read")),
            vec!["everyone", "just alice"]
        );
        assert_eq!(
            bodies(&r.read_since("bob", &Cursor::default(), 0).expect("read")),
            vec!["everyone"]
        );
        assert_eq!(
            bodies(&r.read_since("", &Cursor::default(), 0).expect("read")),
            vec!["everyone"],
            "the room-wide peek is not any identity, so it sees no direct posts"
        );
    }

    /// "A corrupt line is skipped **loudly**, never fatal" — T4's
    /// `Recalled.skipped` pattern. One unparseable line costs the reader that
    /// line and nothing else, and the reason travels out for the caller that
    /// owns the log to report.
    #[test]
    fn a_corrupt_line_is_skipped_loudly_and_never_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        post(&r, "manager", 29, 9, "good one");
        let path = r.root().join("2026-08-29.jsonl");
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str("{ this is not json\n");
        text.push_str(
            "{\"from\":\"\",\"to\":\"*\",\"at\":\"2026-08-29T11:00:00Z\",\"body\":\"x\"}\n",
        );
        text.push_str("{\"from\":\"m\",\"to\":\"*\",\"at\":\"not a time\",\"body\":\"y\"}\n");
        std::fs::write(&path, text).expect("write");
        post(&r, "manager", 29, 12, "good two");

        let got = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(bodies(&got), vec!["good one", "good two"]);
        let ids: Vec<&str> = got.skipped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["2026-08-29:1", "2026-08-29:2", "2026-08-29:3"]);
        assert!(
            got.skipped.iter().all(|(_, why)| !why.is_empty()),
            "every skip carries its reason: {:?}",
            got.skipped
        );
    }

    /// A body past the render cap is truncated at READ time, so a log written
    /// before a cap change still renders within bounds.
    #[test]
    fn a_long_body_is_capped_at_read_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        post(&r, "manager", 29, 9, &"x".repeat(MAX_POST_BODY_BYTES * 2));

        let got = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert!(
            got.messages[0].body.len() <= MAX_MESSAGE_BODY_BYTES,
            "rendered body is {} bytes",
            got.messages[0].body.len()
        );
        // …and the STORED body was capped too, so the log cannot grow without
        // bound from one caller pasting a transcript.
        let log = std::fs::read_to_string(r.root().join("2026-08-29.jsonl")).expect("read");
        assert!(
            log.len() < MAX_POST_BODY_BYTES * 2,
            "log = {} bytes",
            log.len()
        );
    }

    /// An append with no `from` is refused: `from` is host-stamped (§0.11.4) and
    /// an unattributed post is a forgery surface, not a degraded message.
    #[test]
    fn an_append_with_no_from_is_refused_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        let err = r
            .append(&Message::room("", at(29, 9), "anonymous"))
            .expect_err("an unattributed post must be refused");
        assert!(err.to_string().starts_with("room_invalid:"), "{err}");
        assert!(!r.root().exists(), "a refused append creates nothing");
    }

    /// The cursor's text form is exactly a message id, so a human reading the
    /// file can match it against a line, and a corrupt one degrades to "never
    /// read anything" rather than to an error that blocks a dispatch.
    #[test]
    fn cursor_text_round_trips_and_garbage_is_the_beginning() {
        let c = Cursor {
            file: "2026-08-29".to_string(),
            seq: 7,
        };
        assert_eq!(c.as_text(), "2026-08-29:7");
        assert_eq!(Cursor::parse("2026-08-29:7\n"), c);
        for junk in ["", "garbage", ":", "2026-08-29:", ":7", "2026-08-29"] {
            assert_eq!(
                Cursor::parse(junk),
                Cursor::default(),
                "{junk:?} must degrade to the beginning"
            );
        }
    }

    /// Cursors are never created on read, and are written only when asked
    /// (§0.11.4: the identity's own state, not the parity store).
    #[test]
    fn cursors_create_nothing_until_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = Cursors::new(dir.path().join(DEFAULT_BANKS_SUBDIR), "agent-");
        assert_eq!(c.load("alice"), Cursor::default());
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("tempdir").count(),
            0,
            "loading an absent cursor creates nothing"
        );

        let want = Cursor {
            file: "2026-08-29".to_string(),
            seq: 3,
        };
        c.save("alice", &want).expect("save");
        assert_eq!(c.load("alice"), want);
        assert_eq!(
            std::fs::read_to_string(
                dir.path()
                    .join(DEFAULT_BANKS_SUBDIR)
                    .join("agent-alice")
                    .join(CURSOR_FILE)
            )
            .expect("read cursor"),
            "2026-08-29:3\n"
        );
    }

    /// An unreadable cursor is reported rather than swallowed, so a teammate re-reading the same
    /// window every run is a log line and not a mystery. An ABSENT one is still not an error:
    /// never having read the room is the starting state.
    #[test]
    fn an_unreadable_cursor_is_reported_but_an_absent_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = Cursors::new(dir.path().join(DEFAULT_BANKS_SUBDIR), "agent-");
        assert_eq!(
            c.try_load("alice").expect("absent is not an error"),
            Cursor::default()
        );

        // A DIRECTORY where the cursor file should be is an unreadable cursor.
        let bank = c.dir("alice").expect("bank dir");
        std::fs::create_dir_all(bank.join(CURSOR_FILE)).expect("mkdir");
        let err = c
            .try_load("alice")
            .expect_err("an unreadable cursor must be reported");
        assert!(err.to_string().starts_with("room_io_error:"), "{err}");
        // …and the total form still degrades to a bounded re-read rather than failing a dispatch.
        assert_eq!(c.load("alice"), Cursor::default());
    }

    /// The cursor home and the memory bank home are resolved by SEPARATE code
    /// (a cursor must work with `memory.backend: none`, where there is no
    /// `LocalBank`), so they are pinned against each other here — including the
    /// roster's `bank:` override. Without this a teammate could end up with its
    /// records in `agent-alice` and its watermark in a second directory.
    #[test]
    fn cursor_dir_matches_the_memory_bank_dir() {
        let root = PathBuf::from("/tmp/banks");
        let overrides = [("alice", "custom-bank"), ("bob", "")];
        let bank = LocalBank::new(&root, "agent-").with_bank_overrides(overrides);
        let cursors = Cursors::new(&root, "agent-").with_bank_overrides(overrides);
        for identity in ["alice", "bob"] {
            assert_eq!(
                cursors.dir(identity).expect("cursor dir"),
                bank.bank_dir(identity).expect("bank dir"),
                "{identity}"
            );
        }
        // Both refuse a name that is not label-safe, for the same reason: it
        // becomes a directory name, so a `..` would escape the root.
        assert!(cursors.dir("../etc").is_err());
        assert!(bank.bank_dir("../etc").is_err());
    }

    /// The scan is bounded by [`MAX_ROOM_FILE_SCAN`] days, and the days it drops
    /// are the OLDEST — the ones the window would have dropped anyway.
    #[test]
    fn the_file_scan_is_bounded_to_the_newest_days() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        std::fs::create_dir_all(r.root()).expect("mkdir");
        // One message per day, more days than the scan cap.
        for day in 0..(MAX_ROOM_FILE_SCAN + 5) {
            let stem = format!("2020-01-{:02}", day + 1);
            std::fs::write(
                r.root().join(format!("{stem}.{LOG_EXT}")),
                format!(
                    "{{\"from\":\"manager\",\"to\":\"*\",\"at\":\"2020-01-01T00:00:00Z\",\"body\":\"d{day}\"}}\n"
                ),
            )
            .expect("write");
        }
        let got = r
            .read_since("alice", &Cursor::default(), MAX_ROOM_WINDOW)
            .expect("read");
        assert_eq!(got.messages.len(), MAX_ROOM_FILE_SCAN);
        assert_eq!(
            bodies(&got).first().map(String::as_str),
            Some("d5"),
            "the five oldest days fall off the scan, not the newest"
        );
    }

    /// A torn tail — the crash the no-fsync stance explicitly accepts — costs the room the line
    /// that was cut short and NOTHING else. Without the leading-newline guard the next append would
    /// splice itself into the broken line and be lost with it.
    #[test]
    fn an_append_after_a_torn_tail_does_not_join_the_broken_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        post(&r, "manager", 29, 9, "before the crash");
        // Simulate a write cut short mid-line: no trailing newline.
        let path = r.root().join("2026-08-29.jsonl");
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str("{\"from\":\"manager\",\"to\":\"*\",\"at\":\"2026-");
        std::fs::write(&path, text).expect("write");

        post(&r, "manager", 29, 12, "after the crash");

        let got = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(
            bodies(&got),
            vec!["before the crash", "after the crash"],
            "the good message after a torn tail must survive"
        );
        assert_eq!(got.skipped.len(), 1, "exactly the torn line is lost");
    }

    /// A file whose name is not `<stem>.jsonl` is not a log file, and a stray
    /// double extension does not collide with the real day's file.
    #[test]
    fn only_jsonl_files_are_log_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = room(dir.path());
        post(&r, "manager", 29, 9, "real");
        std::fs::write(r.root().join("README.md"), "not a log").expect("write");
        std::fs::write(r.root().join(".DS_Store"), "not a log").expect("write");

        let got = r.read_since("alice", &Cursor::default(), 0).expect("read");
        assert_eq!(bodies(&got), vec!["real"]);
        assert!(got.skipped.is_empty(), "{:?}", got.skipped);
    }

    fn bodies(c: &CaughtUp) -> Vec<String> {
        c.messages.iter().map(|m| m.body.clone()).collect()
    }
}
