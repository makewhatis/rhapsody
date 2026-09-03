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
//! Treating the store's filter as an optimisation has a second consequence that is easy to miss and
//! is **not** a leak: the SQL `LIMIT` runs before the drop does, so a capped page can arrive full of
//! rows about to be discarded, and a team owning plenty of runs would be told it owns none. The
//! gather therefore pages ([`Knowledge::scan`]) until it holds a page of ADMITTED rows or has read
//! [`MAX_SCAN_ROWS`]. Both halves matter: the drop keeps the answer from saying too much, the fill
//! keeps it from confidently saying nothing.
//!
//! # The fact-source allowlist (§9.4)
//!
//! Slice 1's four: the projected [`RunFact`] subset of a [`RunSummary`], recall [`Fact::content`],
//! the cycle's [`Issue`] fields, and the [`RoomLog`]. Slice 2 adds exactly two more, both named by
//! §9.5: the projected [`ReviewFact`] subset of a watch-set row, and a pull request's newest
//! summoning [`Comment`] — the one source that leaves the daemon, and therefore the one that needs
//! a scope gate the store's `project` filter cannot supply: it is read only for a pull request
//! this team's own watch set says one of its own reviewers is watching. §3.2 scopes the accessor
//! to "the team's own entities — nothing external", and an ungated `gh` leg would spend the
//! daemon's own credential on any repository an operator can name. See [`Knowledge::pr_comment`].
//! **No config struct is a fact source.** That is enforced by the
//! constructor rather than by a rule: nothing here accepts a
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
//! Its fourth field is the run's RECORDED dispatch identity (the `teams.route` event), not the
//! ticket's current label: a reassigned ticket must not rewrite who ran its history, and a wrong
//! name in an unauthenticated room is worse than no name because nobody can tell it is wrong.
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
//!    refused outright rather than filtered afterwards. The comparison is only sound over bank ids
//!    resolved the way the BACKEND resolves them — it drops a non-label-safe override and silently
//!    reads `<prefix><name>` instead, which another identity's override may name — so the scope is
//!    built from ids that went through
//!    [`resolve_bank_id`](rhapsody_config::memory::resolve_bank_id). See [`TeamScope`].
//!
//! What these three do NOT do is validate the roster. A `bank:` override that is not label-safe
//! still loads; it is resolved away here rather than rejected at `Teams::validate`, because making
//! it a config error would turn an existing `teams.yaml` into a disabled Teams runtime.
//!
//! # Errors are values, and an absence is not an error
//!
//! Every method returns a [`Result`]: a store or bank failure is the caller's to log and degrade
//! from, not this module's to swallow. An off-team, unknown or unrecorded identifier is `Ok` with
//! nothing in it — "I have no record of that" and "the read failed" are different answers and the
//! manager must be able to tell them apart.
//!
//! Between those two sits a third answer this module also refuses to blur: **partial**. Every
//! gather reports its own shortfalls rather than returning a bare list: a recall carries the
//! records the backend could not parse and how much of the roster it covered ([`Recall`]), a run
//! history carries whether `limit` left rows behind and whether the scan hit its ceiling
//! ([`Runs`]), and a truncated roster gather logs as well. An answer that is short because a bank
//! is corrupt or because a bound bit must not read as an answer that is short because there is
//! nothing to say.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Duration, SecondsFormat, Utc};
use rhapsody_config::memory::{Fact, MemoryBackend, MemoryError, Query, RecallState, STATE_VALID};
use rhapsody_config::room::{Cursor, Message, RoomError, RoomLog};
use rhapsody_core::Issue;
use rhapsody_store::{EventQuery, ReviewWatchKey, RunFilter, RunSummary, Store, StoreError};

use crate::ghsummons::{SummonHit, SummonSource};
use crate::review::{REVIEW_KEY_PREFIX, review_key};
use crate::teams::{EVENT_ROUTE, IDENTITY_LABEL_PREFIX};
use crate::triage::route_event_identity;

/// The most history rows one gather may pull, per project slug. §9.3's ANS-BUDGET-TRUNC bounds the
/// GATHER as well as the prompt: a ticket in a retry loop has dozens of runs and an answer needs
/// the newest few, so the cap is here rather than only at the render.
pub const MAX_HISTORY_ROWS: i64 = 20;

/// The most store rows one scoped gather may READ, per project slug, while trying to fill a page.
///
/// The store's `project` filter is an optimisation and never the guard (see the module docs), so
/// its SQL `LIMIT` has to bound a **scan**, not the answer. Bounding the answer with it is a false
/// NEGATIVE rather than a leak — a page fills with rows [`TeamScope::admits_run`] is about to drop
/// and the team's own rows never reach it — and §9.3's ANS-BUDGET-TRUNC is explicit that a
/// confidently wrong "I have no record of that" is the failure this design exists to fix. The
/// gather therefore pages until it holds `limit` ADMITTED rows or has read this many.
///
/// The ceiling is what the guarantee stops at, stated plainly: on a box busy enough that this
/// team owns none of the newest 500 rows for a slug, its older runs are still out of reach. That
/// is a bound on a bounded read, not a hole in the drop — nothing off-team can escape either way.
///
/// Reaching it is REPORTED, never silent: [`Runs::scan_exhausted`] carries it to the caller and
/// the scan logs it. A gather that stopped at the ceiling has the shape the fill was added to
/// prevent — an answer confidently shorter than the history it describes — just further out, so
/// it is told rather than absorbed.
pub const MAX_SCAN_ROWS: i64 = 500;

/// One page of that scan. Large enough that the ordinary case — a store whose rows are mostly this
/// team's — is answered in a single query.
const SCAN_PAGE_ROWS: i64 = 100;

/// The most roster identities one [`Knowledge::recall_team`] may fan out over. A recall is a
/// directory scan per identity, and the manager answers on the triage cycle's budget.
pub const MAX_RECALL_IDENTITIES: usize = 8;

/// How many `teams.route` rows one ticket's dispatch-identity lookup reads. A ticket accumulates
/// one per routed dispatch; a hundred is far past any real ticket's retry count, and the query is
/// filtered on the issue identifier — the same bound `triage`'s reconcile uses on the same rows.
const MAX_ROUTE_ROWS: i64 = 100;

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
    /// The teammate this RUN was dispatched as, from its `teams.route` event — empty when the run
    /// was never routed, when its events have been pruned, or when it names somebody off this
    /// team's roster. **Per-run and historical, not the ticket's current assignee:** see
    /// [`Knowledge::dispatch_identities`].
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

/// **What one scoped recall produced** — the facts, what the backend could not read, and how much
/// of the roster the gather covered.
///
/// `skipped` exists here because [`Recalled::skipped`] exists there: a corrupt record file is
/// "skipped **loudly**, never fatal", and `rhapsody-config` deliberately does no logging of its own
/// so that "the reason travels to the caller that owns the log". This module IS that caller.
/// Dropping the list would make the loud part silent — a bank whose records will not parse answers
/// short and reads exactly like a teammate who remembers nothing, which is the one distinction §8
/// says the manager must be able to make.
///
/// The identity counts are §9.3's "showing N of M" captured while M is still known. Slice 3 renders
/// the truncation; it cannot render a total this method threw away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recall {
    /// The VALID facts — roster order for a team-wide gather.
    pub facts: Vec<Fact>,
    /// `(file name, why)` for every record the backend could not read.
    pub skipped: Vec<(String, String)>,
    /// How many identities the gather actually read. Zero when the scope refused the only one.
    pub identities_read: usize,
    /// How many it could have read: the roster for [`Knowledge::recall_team`], one for
    /// [`Knowledge::recall`]. Greater than `identities_read` means the answer is partial.
    pub identities_total: usize,
}

/// **What one scoped run gather produced** — the facts, and the two reasons the list may be
/// shorter than the team's real history.
///
/// A bare `Vec<RunFact>` made three situations byte-identical to the caller: the whole history;
/// the newest `limit` of a longer one; and a page the scan could not fill before it hit
/// [`MAX_SCAN_ROWS`]. The last is [`Recall`]'s problem one notch further out — an answer short
/// because a bound bit, read back into a room reply as if it were the whole history — so the run
/// path reports it for the same reason the recall path does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runs {
    /// The projected runs, newest first.
    pub facts: Vec<RunFact>,
    /// There are in-scope runs this answer does NOT contain, because `limit` bounded it. Exact,
    /// not a guess: the gather asks each slug for one row more than it will return, so "filled the
    /// page" and "there is more behind it" are the same fact rather than two.
    pub capped: bool,
    /// A slug read [`MAX_SCAN_ROWS`] rows without filling the page: the SEARCH stopped before the
    /// store did, so there may be older in-scope runs it never reached. Deliberately weaker than
    /// `capped` — the scan cannot tell a store that ran out exactly at the ceiling from one that
    /// did not, and claiming the stronger thing is the kind of confident wrongness this type
    /// exists to prevent. Distinct from `capped` because the remedy differs: `capped` is answered
    /// by asking for more rows, this is not.
    pub scan_exhausted: bool,
}

/// **The operator-facing line for a gather that resolved nothing** (§3.4, §9.1).
///
/// §3.4 pins the requirement — *"a question naming nothing resolvable still degrades to a helpful
/// 'I don't have a record of that' … never silence (silence is the bug this fixes)"* — and §9.1
/// pins the wording, because the sentence has to be true of all three ways a key resolves to
/// nothing at once: off this team's projects, on them but never run, and never seen anywhere. A
/// line that distinguished them would be the leak [`TeamScope`] exists to prevent.
pub const NO_RECORD: &str = "I have no record of that on this team's projects.";

/// How many runs one [`Knowledge::outcome`] gather projects.
///
/// Deliberately far below [`MAX_HISTORY_ROWS`]: an answer to *"what was the result of X"* is about
/// the LATEST attempt and its couple of predecessors, and every extra row is prompt budget slice 3
/// has to spend before it reaches the closed rules (§9.3, ANS-BUDGET-TRUNC). A ticket with more
/// history than this still reports [`Runs::capped`], so the answer can say so.
pub const MAX_OUTCOME_RUNS: i64 = 5;

/// The most watch-set reviewers ONE bare pull-request coordinate is asked about.
///
/// A key that names its reviewer (`pr:owner/repo#12@alice`) asks about exactly that one. A bare
/// `owner/repo#12` has to fan out, because the watch set is keyed per (PR, reviewer) and there is
/// no query that lists a PR's rows — so the fan-out is over the ROSTER, in roster order, capped
/// here and reported as [`Outcome::reviewers_capped`] when the cap bit.
pub const MAX_REVIEW_REVIEWERS: usize = 8;

/// How far back the PR-comment gather asks GitHub to look, in days.
///
/// It bounds the gather in TIME and only in time. [`SummonSource::summons_since`] passes it as the
/// REST `since` filter, so a repository with years of comments costs no more than a quiet one — but
/// the two endpoints it reads are the repository's whole `issues/comments` and `pulls/comments`
/// streams, paged to the end and matched down to one pull request in this process. The breadth is
/// therefore the repository's month, not the pull request's; a per-PR endpoint
/// (`repos/{owner}/{repo}/issues/{number}/comments`) would make it proportional, and that belongs
/// to [`SummonSource`], which the watcher shares. What keeps the cost off an arbitrary repository
/// is not this constant but the scope gate on [`Knowledge::pr_comment`].
///
/// A month is well past the life of any review round this daemon runs, and a review older than that
/// has a run row in the store anyway — the comment is the colour, never the only fact.
pub const PR_COMMENT_LOOKBACK_DAYS: i64 = 30;

/// The most bytes an operator-supplied identifier may be before it names nothing.
///
/// §9.3 bounds the GATHER, and the key is part of the gather: it becomes a SQL parameter on every
/// probe and it is echoed back as [`Outcome::key`], which slice 3 renders into a room reply. An
/// operator post is untrusted DATA of no fixed length (§0.11.5), so without a cap a pasted essay
/// would travel as an "identifier" all the way into the facts block and crowd out the closed rules
/// the prompt ends with — ANS-BUDGET-TRUNC, reached from the one direction the render-side cap
/// cannot see.
///
/// Over-long is treated as naming NOTHING rather than truncated to something: a clipped identifier
/// is a different identifier, and answering about a different one is the confident wrongness this
/// design exists to stop. 256 bytes is comfortably past every real shape — a Linear key is a
/// dozen, a review key with GitHub's longest legal owner and repository is under 160, and a pull
/// request URL with a discussion fragment is about 200.
pub const MAX_KEY_BYTES: usize = 256;

/// The most bytes of ONE pull-request comment body an outcome carries.
///
/// A review comment is agent prose with no length contract at all, and it lands in the same
/// bounded facts block as everything else (§9.3). Clipping here rather than at the render keeps
/// the bound on the GATHER, which is what the design asks for; [`Comment::truncated`] says it
/// happened, so a clipped body is never mistaken for a short one.
pub const MAX_PR_COMMENT_BYTES: usize = 1_000;

/// **A pull-request coordinate an operator's key named**, and the reviewer it named with it.
///
/// The three coordinate fields are what the existing `gh` helpers take; `reviewer` is the fourth
/// component of a review run's own key (`pr:owner/repo#12@alice`) and is empty for a bare
/// coordinate. It is the difference between *"what did alice say"* and *"what happened to this
/// pull request"*, and therefore between one watch-set lookup and a capped roster fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// The reviewing teammate a review key named, or empty for a bare coordinate.
    pub reviewer: String,
}

/// **An operator-supplied identifier, normalized ONCE** — the boundary the STUDIO-729 review
/// required (*"normalize the operator-supplied key ONCE at the boundary (canonical-case) so
/// `issue()` and `issue_runs()` agree"*).
///
/// The two reads disagreed by construction before this type existed. [`Knowledge::issue`] matched
/// the cycle with `eq_ignore_ascii_case`, while [`Knowledge::issue_runs`] handed its argument to
/// the store, whose `issue_identifier = ?` is a case-SENSITIVE SQL comparison on a column with no
/// `NOCASE` collation. An operator asking about `studio-725` was therefore told *"yes, I know that
/// ticket — and it has never been run"*: a confidently wrong answer of exactly the class this
/// design exists to stop, and precisely on the terminal-reach path this slice adds.
///
/// [`Knowledge::key`] is the only thing that builds one, and every read goes through it, so the
/// two reads cannot drift apart again — which also makes it the one place an operator string is
/// bounded before it becomes a query parameter and a rendered fact ([`MAX_KEY_BYTES`]). The canonical spelling is resolved from DATA wherever data
/// exists — a key the cycle knows takes the cycle's own spelling, whatever case it was typed in —
/// and only falls back to a shape rule (`TEAM-123` folds to upper case, the form Linear mints) for
/// a key no source has spelled yet. That fold is a GUESS about a tracker this accessor cannot see,
/// so it is never the last word: see [`Key::probes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Key {
    raw: String,
    canonical: String,
    pr: Option<PrRef>,
}

impl Key {
    /// The spelling every read uses.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// What the operator typed, trimmed.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The pull request this key names, when it names one — the FIRST gate on the `gh` PR-comment
    /// path (§9.5 slice 2: *"gh PR-comment path only when a PR key is present"*), and never the
    /// only one: naming a pull request is not the same as it being this team's, so
    /// [`Knowledge::pr_comment`] also requires the watch set to agree.
    pub fn pr(&self) -> Option<&PrRef> {
        self.pr.as_ref()
    }

    /// Whether the operator named anything at all.
    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// The spellings a store read tries, canonical first — one when the canonical spelling IS what
    /// was typed, two when the shape fold changed it.
    ///
    /// The second probe is what keeps the fold from being a silent guess. `TEAM-123` folded to
    /// upper case is right for Linear, which is the only tracker that mints identifiers this
    /// daemon has ever seen in the wild — but the file tracker's identifiers come out of a
    /// hand-written JSON document and may be spelled any way at all. Probing the raw spelling when
    /// the canonical one found nothing costs one extra bounded scan on the path that was about to
    /// answer [`NO_RECORD`] anyway, and it means the answer follows the store's spelling rather
    /// than this module's opinion of it.
    fn probes(&self) -> impl Iterator<Item = &str> {
        let second = (self.canonical != self.raw).then_some(self.raw.as_str());
        std::iter::once(self.canonical.as_str())
            .chain(second)
            .filter(|s| !s.is_empty())
    }
}

/// **The projected watch-set row — one reviewer's verdict on one pull request.**
///
/// The verdict is `status`, which is one of the `REVIEW_STATUS_*` values the watcher records:
/// `approved` is the reviewer finding nothing, `reviewed` is findings posted, `truncated` is a
/// round that ran out of turns mid-review and therefore is NOT a verdict at all. Reporting those
/// three as one would be the confident wrongness this design exists to stop, so the raw status
/// travels and slice 3 renders it.
///
/// [`ReviewWatchRow`](rhapsody_store::ReviewWatchRow) also carries `requested_sha`,
/// `last_reviewed_sha` and `introduced_by`. None of them is here: §9.3's minimal projection admits
/// a field because an answer needs it, and *"what was the result"* is answered by the verdict, the
/// two teammates and whether the pull request is still open. A SHA and an origin URL are console
/// detail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewFact {
    /// The reviewing teammate. Always on this team's roster — an off-roster row is never read.
    pub reviewer: String,
    /// The teammate whose work is under review, or empty when the row does not know or names
    /// somebody off this team's roster (the §0.11.1 rule [`Knowledge::wearer`] applies to a label).
    pub author: String,
    /// The `REVIEW_STATUS_*` value verbatim — the verdict.
    pub status: String,
    /// Whether the pull request is still open.
    pub open: bool,
    /// The outcome of this reviewer's newest review RUN, or empty when the store has none: the
    /// watch row says what was decided, the run says whether the deciding finished.
    pub outcome: String,
    /// That run's end time, RFC3339, or empty.
    pub ended_at: String,
}

/// **The projected pull-request comment** — the newest summoning comment on the pull request a key
/// named, clipped to [`MAX_PR_COMMENT_BYTES`].
///
/// This is the one fact source §9.4's allowlist did not name, and it is admitted here because
/// §9.5's slice 2 admits it: *"gh PR-comment path only when a PR key is present"*. It is
/// attacker-influenceable prose exactly as recall content and room text are, so it is §9.2 DATA
/// like the rest of the gather — nothing here is trusted, and slice 3 fences all of it identically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comment {
    /// The comment time, RFC3339.
    pub at: String,
    /// The body, clipped to [`MAX_PR_COMMENT_BYTES`] on a character boundary.
    pub body: String,
    /// The body was longer than the clip.
    pub truncated: bool,
}

/// **The answer to "what was the result of X"** — one identifier, gathered across every source the
/// scope admits.
///
/// The whole point of the type is that it reaches a TERMINAL entity. [`Knowledge::issue`] answers
/// only from `cycle.issues`, and a Done ticket has fallen out of that snapshot, which is why
/// STUDIO-725 returned silence (§8). `runs` is the reach: the store remembers a run long after the
/// tracker has stopped listing its ticket, and [`TeamScope`] keeps that reach inside the team.
///
/// Every field can legitimately be empty, and all of them being empty is itself the answer — see
/// [`Outcome::degradation`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The spelling this identifier RESOLVED under: the store's own, when a run row matched, and
    /// otherwise [`Key::canonical`] — which is then a spelling nothing has confirmed, exactly as it
    /// was when the gather began.
    pub key: String,
    /// The live cycle ticket, when this team's own trackers returned one.
    pub issue: Option<IssueFact>,
    /// The team-scoped run history, newest first, bounded by [`MAX_OUTCOME_RUNS`].
    pub runs: Runs,
    /// One per reviewer with a watch-set row for the pull request this key named. Always empty for
    /// a ticket key.
    pub reviews: Vec<ReviewFact>,
    /// The newest summoning comment on that pull request, when a key named one AND a source is
    /// wired.
    pub comment: Option<Comment>,
    /// A PR-comment gather was ATTEMPTED and could not be made — `gh` failed, the token expired,
    /// GitHub was unreachable. Distinct from a `None` comment, which is a pull request nobody has
    /// commented on: the first means the answer is missing something, the second means there is
    /// nothing to miss. False when no source is wired, because then nothing was attempted.
    pub comment_unavailable: bool,
    /// The roster fan-out hit [`MAX_REVIEW_REVIEWERS`]: there may be reviewers of this pull request
    /// this answer does not carry.
    pub reviewers_capped: bool,
}

impl Outcome {
    /// [`NO_RECORD`] when the gather resolved nothing at all, `None` when it resolved something.
    ///
    /// "Nothing" is deliberately strict: a ticket the cycle knows but the store has never run is
    /// NOT a degradation, it is the true and useful answer *"I know that ticket and it has never
    /// been dispatched"*. The degradation is for a key that reached no source — which, for a key
    /// belonging to another team, is exactly what [`TeamScope`] guarantees it reaches.
    ///
    /// "Reached no source" is a claim about the whole gather, so a source that was reached and
    /// FAILED must not read as one that was never asked. It cannot here, and the reason is
    /// structural rather than a check: the only leg that can fail without returning an error is the
    /// `gh` one, and [`Knowledge::pr_comment`] does not take it unless `reviews` is already
    /// non-empty — so [`Outcome::comment_unavailable`] implies `!reviews.is_empty()` implies this
    /// method has already returned `None`. Store and bank failures are never silent: they are
    /// [`KnowledgeError`], and an [`Outcome`] with one of them in it does not exist.
    ///
    /// One case this deliberately does NOT special-case: a bare coordinate whose roster fan-out hit
    /// [`MAX_REVIEW_REVIEWERS`] without finding a watch row still answers [`NO_RECORD`], even
    /// though a reviewer past the cap might hold one. That is [`Runs::scan_exhausted`]'s shape
    /// again — a bound that bit, not a claim that nothing exists — and it is handled the same way:
    /// REPORTED rather than absorbed, on [`Outcome::reviewers_capped`], for slice 3 to render
    /// beside the line. A second degradation string is the wrong fix, because §9.1 pins one wording
    /// precisely so that a key off this team's projects and a key nobody has heard of cannot be
    /// told apart by which sentence comes back.
    pub fn degradation(&self) -> Option<&'static str> {
        let empty = self.issue.is_none()
            && self.runs.facts.is_empty()
            && self.reviews.is_empty()
            && self.comment.is_none();
        empty.then_some(NO_RECORD)
    }
}

/// **The team's identity, reconstructed for every read** (§9.1).
///
/// Built from three things the caller already has and none of which can carry a credential: the
/// resolved project slugs the team owns, the team's roster identity names, and the DAEMON-WIDE
/// identity → bank-id map ([`TeamsMemory::bank_ids`](crate::teamsmemory::TeamsMemory::bank_ids)).
///
/// The bank map is taken whole rather than derived here for the reason
/// [`resolve_bank_id`](rhapsody_config::memory::resolve_bank_id) exists: a second copy of the
/// `<prefix><name>`-unless-overridden rule is exactly how the roster's override ends up honoured by
/// one reader and ignored by another. It must be the same resolution the backend was built with —
/// [`TeamsMemory::bank_ids`](crate::teamsmemory::TeamsMemory::bank_ids) is, and a map assembled any
/// other way must resolve each override through `resolve_bank_id` before it reaches here. Comparing
/// a RAW override against another identity's resolved bank id is the shape of the cross-team read
/// this scope exists to refuse: the backend drops a non-label-safe override and silently reads
/// `<prefix><name>`, so the collision is invisible until it is resolved.
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

    /// The ROSTER's spelling of `identity`, resolved case-insensitively, or `None` when this team
    /// has no such teammate.
    ///
    /// The counterpart of [`Knowledge::key`] for the other half of a review key. A reviewer reaches
    /// this accessor from two places whose case nobody controls — an operator typing
    /// `pr:acme/rhapsody#12@Alice`, and a watch row written before the roster's spelling settled —
    /// and both then have to match a `BTreeSet` lookup, a `rhapsody_review_watch.reviewer` column
    /// with no `NOCASE` collation, and a run's own `issue_identifier`. Three exact comparisons in a
    /// row is three chances to answer "no record" about a teammate who is on the roster, so the
    /// spelling is resolved ONCE here and the roster's own is what every one of them is given.
    ///
    /// The roster is the authority precisely because it is the only one of the three that is
    /// configuration rather than data: [`TeamScope::admits_identity`] is what the scope guard is
    /// written against, so an identity admitted under any other spelling would be admitted under a
    /// name the guard never saw.
    pub fn identity(&self, identity: &str) -> Option<&str> {
        self.identities
            .iter()
            .find(|i| i.eq_ignore_ascii_case(identity))
            .map(String::as_str)
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
    pr_comments: Option<&'a dyn SummonSource>,
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
            pr_comments: None,
        }
    }

    /// Attaches the team's room. The handle IS the team scope — rooms are per team on disk
    /// (`teams/room/<team>/`), so no further TEAM filtering is needed or done.
    ///
    /// It is not the whole filter, though: [`Knowledge::room`] reads as the empty identity, which
    /// `Audience::visible_to("")` admits `Audience::Room` for and rejects every `Audience::Direct`
    /// for. So the accessor sees BROADCAST posts only — the safe direction, and the same view
    /// `teams_room_read` serves — but it means a post directed at the manager is not in the fact
    /// set, and an answer about one would be confidently partial. Whether the manager should
    /// instead read as its own identity is slice 2/3's call, and deliberately not taken here.
    pub fn with_room(mut self, room: &'a dyn RoomLog) -> Knowledge<'a> {
        self.room = Some(room);
        self
    }

    /// Attaches the `gh` pull-request comment source — the one leg of the gather that leaves the
    /// daemon, and the reason it is a builder rather than a constructor argument.
    ///
    /// Without it every other read still answers; a caller that has no GitHub credential, or that
    /// must not spawn a process at all, simply does not call this and gets an answer composed from
    /// the daemon's own stores. With it, the leg still only fires for a key that names a pull
    /// request ([`Knowledge::pr_comment`]).
    pub fn with_pr_comments(mut self, src: &'a dyn SummonSource) -> Knowledge<'a> {
        self.pr_comments = Some(src);
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
    pub fn issue_runs(&self, identifier: &str, limit: i64) -> Result<Runs, KnowledgeError> {
        self.runs_of(&self.key(identifier), limit)
    }

    /// This team's most recent runs, newest first, projected.
    pub fn recent_runs(&self, limit: i64) -> Result<Runs, KnowledgeError> {
        let limit = clamp_rows(limit);
        let scan = self.scan(&RunFilter::default(), limit)?;
        self.project_rows(scan, limit)
    }

    /// Reads `base` once per owned project slug, paging until the slug has yielded one row more
    /// than `limit` ADMITTED by the scope or [`MAX_SCAN_ROWS`] have been read.
    ///
    /// The paging is the point. `Store` applies its `LIMIT` in SQL, before this accessor sees a
    /// row, and its `project` filter is weaker than [`TeamScope::admits_run`] in two shapes that
    /// both occur: an empty slug is *no filter at all* to the store (`RunFilter::project`'s own
    /// contract, and what a legacy `tracker:` config writes), and the `linear_teams` gate has no
    /// SQL counterpart on any slug. A single capped query in either shape can return a full page
    /// of rows that are all dropped, so the read has to keep asking rather than accept the page.
    ///
    /// The one row past `limit` is what makes [`Runs::capped`] a fact rather than a guess: a slug
    /// that stops exactly AT `limit` cannot tell "that is all there is" from "I stopped asking".
    /// One extra admitted row costs nothing and answers it.
    ///
    /// [`Store::list_runs`] rather than [`Store::issue_history`] because only the former takes an
    /// `offset`; with `RunFilter::issue` set it is the same query, plus the ability to page it.
    fn scan(&self, base: &RunFilter, limit: i64) -> Result<Scan, KnowledgeError> {
        // One past the answer, so the caller can distinguish a full page from a complete one.
        let want_admitted = limit.max(0).saturating_add(1) as usize;
        let mut out = Scan::default();
        for slug in &self.scope.projects {
            let mut admitted = 0usize;
            let mut offset: i64 = 0;
            while admitted < want_admitted && offset < MAX_SCAN_ROWS {
                // Never zero: `RunFilter::limit <= 0` means "default page" to the store rather than
                // an error, so a zero here would silently ask for 50 (`store::types`). It cannot be
                // zero at these constants; `max(1)` keeps that true by construction rather than by
                // arithmetic, for whoever changes one of them.
                let want = SCAN_PAGE_ROWS.min(MAX_SCAN_ROWS - offset).max(1);
                let page = self.store.list_runs(RunFilter {
                    project: slug.clone(),
                    limit: want,
                    offset,
                    ..base.clone()
                })?;
                let read = page.len() as i64;
                for row in page {
                    if self.scope.admits_run(&row) {
                        admitted += 1;
                        out.rows.push(row);
                    }
                }
                if read < want {
                    break; // the store is exhausted, not the budget
                }
                offset += read;
            }
            if admitted < want_admitted && offset >= MAX_SCAN_ROWS {
                // The ceiling bit before the page was full: this slug's older runs are out of reach
                // on this read. Logged as well as reported, so the bound is visible in the daemon
                // log without a caller having to plumb the flag anywhere.
                out.scan_exhausted = true;
                tracing::info!(
                    slug = %slug,
                    scanned = offset,
                    admitted,
                    limit,
                    "teams knowledge stopped scanning run history at the per-slug ceiling; \
                     older runs on this slug are out of reach for this answer"
                );
            }
        }
        Ok(out)
    }

    /// The cycle ticket with this identifier, projected. `None` for a key this team's own trackers
    /// did not return — the same validation set every action intent is bounded by.
    pub fn issue(&self, key: &str) -> Option<IssueFact> {
        self.issue_of(&self.key(key))
    }

    /// `identity`'s VALID facts matching `q`, bounded by `q.top_k`.
    ///
    /// Empty for an off-roster identity, for one whose bank cannot be resolved, and for one whose
    /// bank another team also claims — and in those three cases `identities_read` is 0, so a
    /// refusal is distinguishable from a bank that is simply empty. [`Query::state`] is overridden
    /// to [`RecallState::Valid`] and the result is filtered again, because a backend is a trait and
    /// the request is not a promise.
    pub async fn recall(&self, identity: &str, q: &Query) -> Result<Recall, KnowledgeError> {
        if !self.scope.admits_identity(identity) || !self.scope.admits_bank(identity) {
            return Ok(Recall {
                identities_total: 1,
                ..Recall::default()
            });
        }
        let q = Query {
            state: RecallState::Valid,
            ..q.clone()
        };
        let recalled = self.memory.recall(identity, &q).await?;
        Ok(Recall {
            facts: recalled
                .facts
                .into_iter()
                .filter(|f| f.state == STATE_VALID)
                .collect(),
            skipped: recalled.skipped,
            identities_read: 1,
            identities_total: 1,
        })
    }

    /// The same recall across this team's roster, capped at [`MAX_RECALL_IDENTITIES`] identities.
    ///
    /// The cap truncates in roster order — [`TeamScope::identities`] is a `BTreeSet`, so the choice
    /// is deterministic rather than arbitrary — and says so: the returned counts carry N of M, and
    /// a truncated gather logs, because a silently partial answer is indistinguishable from a
    /// complete one to the operator reading it.
    pub async fn recall_team(&self, q: &Query) -> Result<Recall, KnowledgeError> {
        let total = self.scope.identities.len();
        if total > MAX_RECALL_IDENTITIES {
            tracing::info!(
                read = MAX_RECALL_IDENTITIES,
                roster = total,
                "teams knowledge recalled from part of the roster; the gather is capped per answer"
            );
        }
        let mut out = Recall {
            identities_total: total,
            ..Recall::default()
        };
        for identity in self.scope.identities.iter().take(MAX_RECALL_IDENTITIES) {
            let one = self.recall(identity, q).await?;
            out.facts.extend(one.facts);
            out.skipped.extend(one.skipped);
            out.identities_read += one.identities_read;
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

    /// **The boundary normalizer** — resolves an operator-supplied identifier ONCE into the [`Key`]
    /// every read below uses. See [`Key`] for why one shared normalization is a requirement rather
    /// than a tidiness.
    pub fn key(&self, raw: &str) -> Key {
        let raw = raw.trim();
        if raw.len() > MAX_KEY_BYTES {
            // Not an identifier by any reading, so it names nothing — and nothing is what the rest
            // of the gather then reads. See [`MAX_KEY_BYTES`].
            return Key::default();
        }
        let raw = raw.to_string();
        let pr = parse_pr_ref(&raw);
        let canonical = if let Some(iss) = self
            .issues
            .iter()
            .find(|i| i.identifier.eq_ignore_ascii_case(&raw))
        {
            // The cycle has spelled it, so there is nothing to guess: the tracker's own spelling is
            // by definition the one the store was written with.
            iss.identifier.clone()
        } else if pr.is_some() {
            // A pull-request coordinate is case-BEARING in all four components — a GitHub owner and
            // repository are matched case-insensitively by GitHub but written as the operator has
            // them, and a reviewer is a roster identity — so folding it would break the review key
            // it has to match in the store.
            raw.clone()
        } else if is_tracker_shaped(&raw) {
            raw.to_ascii_uppercase()
        } else {
            raw.clone()
        };
        Key { raw, canonical, pr }
    }

    /// **The answer to "what was the result of X"** (§9.5 slice 2), gathered under one [`Key`].
    ///
    /// Reaches a TERMINAL entity, which is the gap that made STUDIO-725 return nothing: a ticket
    /// the cycle no longer lists still has run rows, watch-set rows and pull-request comments, and
    /// all three are readable without a single tracker call. Every one of them is filtered through
    /// [`TeamScope`] — the store rows by project (and Linear team, when gated), the watch rows by
    /// roster — so a key belonging to another team resolves to [`Outcome::degradation`] and never
    /// to a row.
    ///
    /// The gather is bounded on every axis §9.3 names: [`MAX_OUTCOME_RUNS`] history rows through
    /// [`Knowledge::scan`]'s own [`MAX_SCAN_ROWS`] ceiling, [`MAX_REVIEW_REVIEWERS`] watch-set
    /// lookups, [`MAX_PR_COMMENT_BYTES`] of comment body over a [`PR_COMMENT_LOOKBACK_DAYS`]
    /// window — and the `gh` path is not merely bounded but GATED twice: no pull request in the
    /// key, no process spawned; no in-roster watch row for that pull request, no process spawned
    /// either. The second gate is the team scope for the one leg [`TeamScope`] cannot reach, and
    /// it is why *every* leg of this gather is scoped and not merely most of them.
    ///
    /// **Not for the control task.** The `gh` leg goes through
    /// [`SummonSource`](crate::ghsummons::SummonSource), whose production implementation shells out
    /// through a synchronous `std::process::Command` and therefore runs to completion in its first
    /// poll. The containment is structural, exactly as [`crate::prstate`]'s is: this method takes
    /// no `Orchestrator`, sends no control event and holds no lock the control task takes, so its
    /// caller drives it from its own task and a stalled `gh` parks that task and nothing else.
    /// A `tokio::time::timeout` around it would be decoration — the future has no await point to
    /// cancel at.
    pub async fn outcome(&self, identifier: &str) -> Result<Outcome, KnowledgeError> {
        let key = self.key(identifier);
        if key.is_empty() {
            return Ok(Outcome::default());
        }
        let issue = self.issue_of(&key);
        let runs = self.runs_of(&key, MAX_OUTCOME_RUNS)?;
        let (reviews, reviewers_capped) = self.reviews_of(&key, &runs)?;
        let (comment, comment_unavailable) = self.pr_comment(&key, &reviews).await;
        Ok(Outcome {
            // The spelling that MATCHED, not the fold that went looking. `runs_of` may resolve on
            // either probe, and echoing the canonical one after the raw one hit would put an
            // identifier no source holds into the facts block — the one place the design says never
            // to fabricate. With nothing matched there is nothing to echo but the fold.
            key: runs
                .facts
                .first()
                .map(|r| r.key.clone())
                .unwrap_or(key.canonical),
            issue,
            runs,
            reviews,
            comment,
            comment_unavailable,
            reviewers_capped,
        })
    }

    /// The cycle ticket this key resolved to. An EXACT comparison, and it is exact precisely
    /// because [`Knowledge::key`] has already taken the cycle's own spelling for any key the cycle
    /// knows — which is what makes this read and [`Knowledge::runs_of`] agree.
    fn issue_of(&self, key: &Key) -> Option<IssueFact> {
        let iss = self.issues.iter().find(|i| i.identifier == key.canonical)?;
        Some(IssueFact {
            key: iss.identifier.clone(),
            title: iss.title.clone(),
            state: iss.state.clone(),
            identity: self.wearer(&iss.identifier),
        })
    }

    /// Every run of this key on this team's projects, newest first, projected — trying the raw
    /// spelling only when the canonical one found nothing (see [`Key::probes`]).
    ///
    /// [`Runs::scan_exhausted`] survives a probe that found nothing: a ceiling that bit on the
    /// first spelling is still a reason the second one's silence may be incomplete, and dropping
    /// the flag would turn a bounded search into a confident "there is nothing".
    fn runs_of(&self, key: &Key, limit: i64) -> Result<Runs, KnowledgeError> {
        let limit = clamp_rows(limit);
        let mut exhausted = false;
        for probe in key.probes() {
            let scan = self.scan(
                &RunFilter {
                    issue: probe.to_string(),
                    ..RunFilter::default()
                },
                limit,
            )?;
            let runs = self.project_rows(scan, limit)?;
            exhausted = exhausted || runs.scan_exhausted;
            if !runs.facts.is_empty() {
                return Ok(Runs {
                    scan_exhausted: exhausted,
                    ..runs
                });
            }
        }
        Ok(Runs {
            scan_exhausted: exhausted,
            ..Runs::default()
        })
    }

    /// The watch-set verdicts for the pull request this key named, and whether the roster fan-out
    /// was capped. Empty and uncapped for a ticket key — a ticket has no watch row.
    ///
    /// **Roster membership IS the team scope here.** A [`ReviewWatchRow`] carries no project slug
    /// and no Linear team, so [`TeamScope::admits_run`] has nothing to bite on; what it does carry
    /// is a reviewer, and an identity belongs to exactly one Rhapsody team (STUDIO-668 §B.3). The
    /// scope is applied by never ASKING about an off-roster reviewer rather than by dropping the
    /// answer afterwards, so another team's verdict is not read and then discarded — it is not read.
    fn reviews_of(
        &self,
        key: &Key,
        gathered: &Runs,
    ) -> Result<(Vec<ReviewFact>, bool), KnowledgeError> {
        let Some(pr) = key.pr() else {
            return Ok((Vec::new(), false));
        };
        // A key that names its reviewer asks about that reviewer and no one else; a bare
        // coordinate has to fan out, because the watch set has no "rows for this PR" query.
        let (reviewers, capped): (Vec<String>, bool) = if pr.reviewer.is_empty() {
            let total = self.scope.identities().count();
            let roster: Vec<String> = self
                .scope
                .identities()
                .take(MAX_REVIEW_REVIEWERS)
                .map(str::to_string)
                .collect();
            let capped = total > MAX_REVIEW_REVIEWERS;
            if capped {
                tracing::info!(
                    read = MAX_REVIEW_REVIEWERS,
                    roster = total,
                    "teams knowledge asked part of the roster for a pull request's verdicts; \
                     the fan-out is capped per answer"
                );
            }
            (roster, capped)
        } else if let Some(known) = self.scope.identity(&pr.reviewer) {
            // The ROSTER's spelling, not the operator's: it is what the scope guard admitted and
            // what the watch-set and run lookups below are given. See [`TeamScope::identity`].
            (vec![known.to_string()], false)
        } else {
            (Vec::new(), false)
        };

        let mut out = Vec::new();
        for reviewer in reviewers {
            // A case-insensitive coordinate read, because the operator typed the owner and the
            // repository and this table stores them case-SENSITIVELY: `Acme/Rhapsody#12` names the
            // same pull request as `acme/rhapsody#12` to GitHub and to the person asking, and an
            // exact read would answer "no record" about a pull request the team is reviewing.
            let row = self.store.find_review_watch(&ReviewWatchKey {
                owner: pr.owner.clone(),
                repo: pr.repo.clone(),
                number: pr.number,
                reviewer: reviewer.clone(),
            })?;
            let Some(row) = row else {
                continue;
            };
            // The reviewer's own review run, which is a store row like any other and therefore
            // project-scoped like any other. One row: an answer wants the latest round.
            //
            // A review KEY is already its own run's identifier, so the gather that produced
            // `gathered` has read exactly these rows — reading them again would be a second
            // bounded scan for an answer already in hand.
            //
            // Built from the STORE's spelling of the coordinate, never the operator's. The run and
            // the watch row are minted from the same `ReviewRun` fields
            // ([`ReviewRun::key`](crate::review::ReviewRun) and its `watch_key`), so the row that
            // just came back names the exact bytes `runs.issue_identifier` holds — and that column
            // is a case-SENSITIVE `=` too, which is the third and last exact match between a typed
            // coordinate and this answer.
            let run_key = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            let newest: Option<RunFact> = if run_key == key.canonical {
                // Byte-identical to what `runs_of` already probed, so its rows ARE this lookup.
                // Only an exact match may take the shortcut: a coordinate whose case the store
                // corrected was probed under the operator's spelling and found nothing.
                gathered.facts.first().cloned()
            } else {
                self.runs_of(&self.key(&run_key), 1)?
                    .facts
                    .into_iter()
                    .next()
            };
            out.push(ReviewFact {
                reviewer,
                // The roster's spelling for the same reason the reviewer's is the roster's, and
                // empty when this team has no such teammate — a row's author is a store string,
                // and §0.11.1 leaves a name this team does not own out of the answer entirely.
                author: self
                    .scope
                    .identity(&row.author)
                    .unwrap_or_default()
                    .to_string(),
                status: row.status.clone(),
                open: row.open,
                outcome: newest
                    .as_ref()
                    .map(|r| r.outcome.clone())
                    .unwrap_or_default(),
                ended_at: newest.map(|r| r.ended_at).unwrap_or_default(),
            });
        }
        Ok((out, capped))
    }

    /// The newest summoning comment on the pull request this key named — the ONLY leg of the
    /// gather that leaves the daemon, and therefore the one with a scope gate of its own.
    ///
    /// Three conditions, all necessary: the key carries a [`PrRef`], a source has been wired with
    /// [`Knowledge::with_pr_comments`], and `reviews` is NON-EMPTY.
    ///
    /// The third is the team scope (§3.2, *"scoped to the team's own entities — nothing
    /// external"*). Every other leg reads the daemon's own store through [`TeamScope`]; this one
    /// spends the daemon's OWN GitHub credential on a repository named in operator text, so
    /// "bounded" is not enough — an ungated leg answers
    /// `some-other-org/private-infra#12` by reading a private repository the team has nothing to do
    /// with and returning it as a positive answer, which slice 3 then renders into an
    /// unauthenticated shared room. That is a confused-deputy read primitive, not a long answer.
    ///
    /// `reviews` is the gate because it is already the roster-scoped resolution of this exact
    /// coordinate: [`Knowledge::reviews_of`] has just asked the watch set which of THIS team's
    /// reviewers are watching this pull request, and a pull request with no in-roster watch row is
    /// not this team's pull request. The watch set is also the one place a PR coordinate is
    /// trusted from — it is written from a handoff's own resolved repository or by an operator
    /// through the authenticated console, never from room text
    /// ([`ReviewWatchRow::introduced_by`](rhapsody_store::ReviewWatchRow), design §14.1 F-SEC) —
    /// so gating on it keeps the untrusted half of the key from selecting the request.
    ///
    /// A failed fetch is not an error the caller has to handle: the rest of the answer is still
    /// true, and refusing to answer at all because GitHub was unreachable would be the silence §3.4
    /// exists to end. It is REPORTED instead, as [`Outcome::comment_unavailable`], so a short
    /// answer is never mistaken for a complete one. Note what the gate does to that flag: nothing
    /// is attempted without a watch row, so `comment_unavailable` now IMPLIES a non-empty
    /// `reviews`, and [`Outcome::degradation`] can never claim "no record" about a key whose only
    /// reached source failed.
    async fn pr_comment(&self, key: &Key, reviews: &[ReviewFact]) -> (Option<Comment>, bool) {
        let (Some(pr), Some(src)) = (key.pr(), self.pr_comments) else {
            return (None, false);
        };
        if reviews.is_empty() {
            tracing::debug!(
                owner = %pr.owner,
                repo = %pr.repo,
                number = pr.number,
                "teams knowledge left a pull request's comments unread: no reviewer on this \
                 team's roster is watching it, so it is not this team's pull request"
            );
            return (None, false);
        }
        let since = Utc::now() - Duration::days(PR_COMMENT_LOOKBACK_DAYS);
        match src.summons_since(&pr.owner, &pr.repo, since).await {
            Ok(by_pr) => (by_pr.get(&pr.number).map(project_comment), false),
            Err(e) => {
                tracing::info!(
                    owner = %pr.owner,
                    repo = %pr.repo,
                    number = pr.number,
                    error = %e,
                    "teams knowledge could not read a pull request's comments; the answer reports \
                     the gap rather than hiding it"
                );
                (None, true)
            }
        }
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
    /// row on the box. [`Knowledge::scan`] applies the same predicate while paging, so that the
    /// page is FILLED with in-scope rows; this second application is what makes the guarantee hold
    /// for any row that reaches the projection, however it got here.
    fn project_rows(&self, scan: Scan, limit: i64) -> Result<Runs, KnowledgeError> {
        let Scan {
            mut rows,
            scan_exhausted,
        } = scan;
        rows.retain(|r| self.scope.admits_run(r));
        // The store orders by (started_at DESC, id DESC); one merged list of per-slug pages has to
        // be put back into that order, and `id` breaks a same-instant tie exactly as SQLite does.
        rows.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        rows.dedup_by_key(|r| r.id);
        // The scan read one admitted row past `limit` per slug, so a merged set that still exceeds
        // `limit` is a MEASURED "there is more", not an assumption that a full page implies one.
        let limit = limit.max(0) as usize;
        let capped = rows.len() > limit;
        rows.truncate(limit);
        // Resolved for the BOUNDED page only, so the lookup cost follows the answer's size.
        let dispatched = self.dispatch_identities(&rows)?;
        Ok(Runs {
            facts: rows
                .iter()
                .map(|r| RunFact {
                    key: r.issue_identifier.clone(),
                    outcome: r.outcome.clone(),
                    ended_at: r.ended_at.clone(),
                    identity: dispatched.get(&r.id).cloned().unwrap_or_default(),
                })
                .collect(),
            capped,
            scan_exhausted,
        })
    }

    /// Which roster identity each of `rows` was DISPATCHED as, keyed by run id, from the
    /// `teams.route` events row every routed dispatch writes ([`EVENT_ROUTE`]) — the same durable
    /// ledger [`crate::triage::StoreIdentityHistory`] reconciles labels against, rather than a
    /// second one.
    ///
    /// **Not the ticket's current `rhapsody:@` label.** A ticket reassigned since the run — a
    /// re-review handed on, a manager reroute — would otherwise attribute every historical run to
    /// whoever wears the label today, and in an unauthenticated room "jimmy ran it" when alice ran
    /// it is a WRONG fact rather than a missing one, which an operator has no way to notice.
    ///
    /// A run reports EMPTY — "cannot tell", never a guess — when it was never routed, when its
    /// events have been pruned, when its ticket has more than [`MAX_ROUTE_ROWS`] dispatches ahead
    /// of it, or when the route names somebody off this team's roster (the same rule
    /// [`Knowledge::wearer`] applies to a label, for the same §0.11.1 reason).
    ///
    /// One query per distinct ticket in the page rather than one per row, and only ever for rows
    /// the scope has already admitted: the events search has no project filter of its own, so it is
    /// never asked a question whose answer could be another team's.
    fn dispatch_identities(
        &self,
        rows: &[RunSummary],
    ) -> Result<HashMap<i64, String>, KnowledgeError> {
        let keys: BTreeSet<&str> = rows
            .iter()
            .map(|r| r.issue_identifier.as_str())
            .filter(|k| !k.is_empty())
            .collect();
        // A ticket's route rows cover every run of it, including runs this page does not carry and
        // the scope may never have admitted. Keeping only the ones being projected is what makes
        // "only for rows the scope has already admitted" true of the data and not just the query.
        let wanted: BTreeSet<i64> = rows.iter().map(|r| r.id).collect();
        let mut out: HashMap<i64, String> = HashMap::new();
        for key in keys {
            let hits = self.store.search_events(EventQuery {
                issue: key.to_string(),
                kind: EVENT_ROUTE.to_string(),
                limit: MAX_ROUTE_ROWS,
                ..EventQuery::default()
            })?;
            // Ordered (run_id DESC, seq DESC), so the first row seen for a run is its LAST routing
            // decision — the one it actually ran under if it was ever re-routed mid-run.
            for hit in hits {
                if !wanted.contains(&hit.run_id) {
                    continue;
                }
                let Some(name) = route_event_identity(&hit.text) else {
                    continue;
                };
                if self.scope.admits_identity(&name) {
                    out.entry(hit.run_id).or_insert(name);
                }
            }
        }
        Ok(out)
    }
}

/// **Reads a pull-request coordinate out of an operator-supplied key**, or `None` when the key
/// names no pull request — which is the whole gate on the `gh` leg of the gather (§9.5 slice 2).
///
/// Three shapes, because three shapes are what an operator has to hand:
///
/// * `pr:owner/repo#12@alice` — a review RUN's own key ([`review_key`]), the spelling the store
///   holds and the one a teammate quotes out of a room line.
/// * `owner/repo#12` — the coordinate the `gh` helpers take.
/// * `https://github.com/owner/repo/pull/12` — what a browser hands you, with any trailing
///   `/files`, `#discussion_r…` or query string ignored.
///
/// The URL form is matched on its `/pull/` segment and its last two path components rather than on
/// its host, because the host is not what makes the lookup safe: nothing here fetches a URL. The
/// coordinate is handed to the same [`SummonSource`](crate::ghsummons::SummonSource) every other
/// caller uses, which is GitHub's API and no one else's, so a coordinate carved out of some other
/// host's URL resolves to that repository on GitHub or to nothing at all.
///
/// Fails closed on anything ambiguous: a missing component, a number that is not a positive
/// integer, an owner or repository containing a path separator.
pub fn parse_pr_ref(raw: &str) -> Option<PrRef> {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix(REVIEW_KEY_PREFIX) {
        // `@` splits from the RIGHT: a reviewer name cannot contain one, and splitting from the
        // left would mangle a coordinate that somehow did.
        let (coord, reviewer) = match rest.rsplit_once('@') {
            Some((c, r)) => (c, r.trim()),
            None => (rest, ""),
        };
        let mut pr = parse_pr_coord(coord)?;
        pr.reviewer = reviewer.to_string();
        return Some(pr);
    }
    if let Some((head, tail)) = s.rsplit_once("/pull/") {
        let number = leading_number(tail)?;
        let mut segments = head.rsplit('/').filter(|seg| !seg.is_empty());
        let repo = segments.next()?;
        let owner = segments.next()?;
        return pr_ref(owner, repo, number);
    }
    parse_pr_coord(s)
}

/// `owner/repo#12` — the bare coordinate, shared by the plain form and the `pr:` key's body.
fn parse_pr_coord(s: &str) -> Option<PrRef> {
    let (slug, num) = s.split_once('#')?;
    let (owner, repo) = slug.split_once('/')?;
    pr_ref(owner, repo, leading_number(num)?)
}

/// The one place a [`PrRef`] is admitted, so every shape fails closed the same way.
fn pr_ref(owner: &str, repo: &str, number: i64) -> Option<PrRef> {
    let (owner, repo) = (owner.trim(), repo.trim());
    if owner.is_empty() || repo.is_empty() || owner.contains('/') || repo.contains('/') {
        return None;
    }
    Some(PrRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        reviewer: String::new(),
    })
}

/// The positive integer a string STARTS with, ignoring whatever follows it — a pull-request URL
/// carries `/files` and a fragment past the number often enough that requiring a clean tail would
/// reject the commonest paste. `None` for a leading non-digit, an empty run, a value that does not
/// fit an `i64`, or a non-positive number.
fn leading_number(s: &str) -> Option<i64> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let n: i64 = digits.parse().ok()?;
    (n > 0).then_some(n)
}

/// Whether a key has the `TEAM-123` shape Linear mints — one alphanumeric run beginning with a
/// letter, a single hyphen, then digits.
///
/// Deliberately STRICT, because the only thing that follows from a match is a case fold. The file
/// tracker's minted `<team_id>-<n>` (`team-1-5`) has a second hyphen and is left alone, and so is
/// anything else this module cannot recognise: an unfolded key is read exactly as typed, which is
/// the behaviour before this slice.
fn is_tracker_shaped(s: &str) -> bool {
    let Some((prefix, num)) = s.rsplit_once('-') else {
        return false;
    };
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && prefix.starts_with(|c: char| c.is_ascii_alphabetic())
        && prefix.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Projects a [`SummonHit`] into the room-safe [`Comment`].
fn project_comment(hit: &SummonHit) -> Comment {
    let (body, truncated) = clip(&hit.body, MAX_PR_COMMENT_BYTES);
    Comment {
        at: hit.at.to_rfc3339_opts(SecondsFormat::Secs, true),
        body,
        truncated,
    }
}

/// Clips to at most `max` BYTES, backing up to a character boundary rather than slicing through a
/// multi-byte character (which would panic, and a comment body is arbitrary UTF-8 from the
/// internet).
fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// A non-positive row limit is the caller asking for the default, never for "unbounded" — the same
/// rule [`Query::top_k`] and `memory.recall_top_k` follow.
/// What one [`Knowledge::scan`] read: the admitted rows, and whether any slug stopped at
/// [`MAX_SCAN_ROWS`] rather than because it had what it needed.
///
/// Internal because the rows are unprojected `RunSummary` — the console-grade fields §9.3 keeps out
/// of the room live on them, and [`Knowledge::project_rows`] is the only thing that may see them.
#[derive(Debug, Default)]
struct Scan {
    rows: Vec<RunSummary>,
    scan_exhausted: bool,
}

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
    use rhapsody_store::{
        EventRow, Noop, REVIEW_STATUS_APPROVED, REVIEW_STATUS_REVIEWED, RunEnd, RunStart, Sqlite,
        StorePath,
    };

    use crate::teamsmemory::TeamsMemory;
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

    /// Records that `run_id` was DISPATCHED as `identity` — the same `teams.route` row
    /// `Orchestrator::route_teams` writes, in the same `identity=<name> reason=<why>` shape.
    fn seed_route(st: &Sqlite, run_id: i64, identity: &str) {
        st.append_events(
            run_id,
            &[EventRow {
                seq: 1,
                at: "2026-09-01T10:00:00Z".into(),
                kind: EVENT_ROUTE.into(),
                tool: String::new(),
                text: format!("identity={identity} reason=label_overlap"),
            }],
        )
        .expect("append route event");
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
            k.issue_runs("BBB-2", 0)
                .expect("issue_runs")
                .facts
                .is_empty(),
            "team A resolved team B's terminal key through the global store"
        );
        let recent = k.recent_runs(0).expect("recent_runs").facts;
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
            k.issue_runs("BBB-2", 0)
                .expect("issue_runs")
                .facts
                .is_empty(),
            "the accessor leaned on the store's project filter instead of its own drop"
        );
        let recent = k.recent_runs(0).expect("recent_runs").facts;
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

        assert!(
            k.issue_runs("AAA-1", 0)
                .expect("issue_runs")
                .facts
                .is_empty()
        );
        assert!(k.recent_runs(0).expect("recent_runs").facts.is_empty());
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

        let recent = k.recent_runs(0).expect("recent_runs").facts;
        assert_eq!(
            recent.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec!["LEG-1"]
        );
        assert!(
            k.issue_runs("BBB-2", 0)
                .expect("issue_runs")
                .facts
                .is_empty()
        );
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
        assert_eq!(k.recent_runs(0).expect("recent_runs").facts.len(), 2);

        let gated = scope_of(&["alpha"], &["alice"]).with_linear_teams(["linear-a"]);
        let k = Knowledge::new(&gated, &issues, st.as_ref(), &none);
        assert_eq!(
            k.recent_runs(0)
                .expect("recent_runs")
                .facts
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
        let run = seed(
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
        seed_route(&st, run, "alice");
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-1", "Done", &["rhapsody:@alice"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let runs = k.issue_runs("AAA-1", 0).expect("issue_runs").facts;
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

    /// **A reassigned ticket does not rewrite its history (STUDIO-729 review).** `RunFact::identity`
    /// is the run's recorded `teams.route` dispatch, so a ticket handed on after a run still
    /// attributes that run to whoever actually ran it — and a run that was never routed says
    /// nothing rather than borrowing today's label.
    #[test]
    fn a_run_is_attributed_to_who_dispatched_it_not_to_todays_label() {
        let st = store();
        let first = seed_ok(&st, "AAA-1", "alpha", "2026-09-01T10:00:00Z", "failed");
        let second = seed_ok(&st, "AAA-1", "alpha", "2026-09-01T11:00:00Z", "completed");
        seed_ok(&st, "AAA-2", "alpha", "2026-09-01T12:00:00Z", "completed");
        seed_route(&st, first, "alice");
        seed_route(&st, second, "jimmy");
        // A route naming somebody this team does not own is not reported as a teammate either.
        let foreign = seed_ok(&st, "AAA-3", "alpha", "2026-09-01T13:00:00Z", "completed");
        seed_route(&st, foreign, "mallory");

        // The ticket now wears jimmy's label; alice's run predates the hand-off.
        let scope = scope_of(&["alpha"], &["alice", "jimmy"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-1", "In Review", &["rhapsody:@jimmy"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let runs = k.issue_runs("AAA-1", 0).expect("issue_runs").facts;
        assert_eq!(
            runs.iter()
                .map(|r| (r.outcome.as_str(), r.identity.as_str()))
                .collect::<Vec<_>>(),
            vec![("completed", "jimmy"), ("failed", "alice")],
            "the current label overwrote a historical run's attribution"
        );
        // The ticket's CURRENT assignee is still the label — that is what a live ticket's
        // `IssueFact` reports, and the two answer different questions.
        assert_eq!(k.issue("AAA-1").expect("issue").identity, "jimmy");

        let by_key = |key: &str| {
            k.issue_runs(key, 0)
                .expect("issue_runs")
                .facts
                .first()
                .map(|r| r.identity.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            by_key("AAA-2"),
            "",
            "an unrouted run must not be attributed"
        );
        assert_eq!(
            by_key("AAA-3"),
            "",
            "an off-roster route must not be reported"
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
        let got = k
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
        assert_eq!(got.facts.len(), 1, "{got:?}");
        assert!(got.facts[0].content.contains("standing"));
        assert_eq!((got.identities_read, got.identities_total), (1, 1));

        // And a backend that answers with an invalidated record regardless is filtered anyway.
        let lying = LyingBank;
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &lying);
        let facts = k
            .recall("alice", &Query::default())
            .await
            .expect("recall")
            .facts;
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
            .expect("recall")
            .facts;
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
            .expect("recall")
            .facts;
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
            .facts
            .len(),
            1
        );
    }

    /// **The dropped-override leak (STUDIO-729 review, BLOCKER 1).** A roster `bank:` that is not
    /// label-safe is DROPPED by the backend, which then reads `<prefix><name>` instead — so a guard
    /// reasoning about the raw override reasons about a string no backend ever opens. Give a
    /// FOREIGN identity a label-safe override naming exactly that fallback and the two teams share
    /// one directory while the raw map shows no collision at all.
    ///
    /// The scope is built from [`TeamsMemory::bank_ids`] — the real producer, not a hand-made map —
    /// because the failure WAS the producer disagreeing with the backend.
    #[tokio::test]
    async fn recall_refuses_a_bank_reached_through_a_dropped_override() {
        let dir = TempDir::new();
        let bank = Arc::new(
            LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-")
                // Not label-safe -> dropped -> alice actually reads `agent-alice`.
                // Label-safe -> honoured -> mallory actually reads `agent-alice` too.
                .with_bank_overrides([("alice", "Not/Safe"), ("mallory", "agent-alice")]),
        );
        bank.retain(&Record {
            identity: "mallory".into(),
            document_id: "run-9".into(),
            at: now(),
            content: "team B's private note".into(),
            ..Record::default()
        })
        .expect("retain");

        let teams = Arc::new(Teams {
            enabled: true,
            roster: vec![
                Identity {
                    name: "alice".into(),
                    bank: "Not/Safe".into(),
                    ..Identity::default()
                },
                Identity {
                    name: "mallory".into(),
                    bank: "agent-alice".into(),
                    ..Identity::default()
                },
            ],
            ..Teams::disabled()
        });
        let runtime = TeamsMemory::new(teams, bank.clone());
        assert_eq!(
            runtime.bank_ids().get("alice").map(String::as_str),
            Some("agent-alice"),
            "the daemon-wide map must report the bank the BACKEND reads, not the raw override"
        );

        let scope = TeamScope::new(["alpha"], ["alice"], runtime.bank_ids());
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), bank.as_ref());

        assert!(
            !scope.admits_bank("alice"),
            "a dropped override let an in-team identity reach a foreign claimant's bank"
        );
        let facts = k
            .recall(
                "alice",
                &Query {
                    browse: true,
                    ..Query::default()
                },
            )
            .await
            .expect("recall")
            .facts;
        assert!(
            facts.is_empty(),
            "recall crossed into another team's bank through a dropped override: {facts:?}"
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
            .facts
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
        let got = k.recall("alice", &Query::default()).await.expect("recall");
        assert!(got.facts.is_empty());
        assert_eq!(
            (got.identities_read, got.identities_total),
            (0, 1),
            "a refused recall must be distinguishable from an empty bank"
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
        let mut who: Vec<&str> = facts.facts.iter().map(|f| f.identity.as_str()).collect();
        who.sort_unstable();
        assert_eq!(who, vec!["alice", "bob"]);
    }

    /// **A record the backend could not read is reported, not swallowed (STUDIO-729 review).**
    /// `rhapsody-config` does no logging of its own precisely so the reason reaches the caller that
    /// owns the log — this module — and a bank of unparseable records must not read as a teammate
    /// who simply remembers nothing.
    #[tokio::test]
    async fn recall_reports_the_records_the_backend_could_not_read() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        bank.retain(&Record {
            identity: "alice".into(),
            document_id: "run-1".into(),
            at: now(),
            content: "a good record".into(),
            ..Record::default()
        })
        .expect("retain");
        let bank_dir = bank.bank_dir("alice").expect("bank dir");
        std::fs::write(bank_dir.join("00000000T000000Z-broken.md"), "not a record")
            .expect("write corrupt");

        let scope = scope_of(&["alpha"], &["alice"]);
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        let q = Query {
            browse: true,
            ..Query::default()
        };
        let got = k.recall("alice", &q).await.expect("recall");
        assert_eq!(got.facts.len(), 1, "the good record still comes back");
        assert_eq!(
            got.skipped.len(),
            1,
            "the unreadable record was swallowed: {got:?}"
        );
        assert_eq!(got.skipped[0].0, "00000000T000000Z-broken.md");

        // …and it survives the team-wide gather too, which is where slice 2 reads it.
        let team = k.recall_team(&q).await.expect("recall_team");
        assert_eq!(team.skipped.len(), 1, "{team:?}");
    }

    /// **The roster cap says so (STUDIO-729 review).** `recall_team` reads the alphabetically first
    /// [`MAX_RECALL_IDENTITIES`], deterministically — but a partial answer that reports itself as
    /// complete is the §9.3 failure, so the counts carry N of M while M is still knowable.
    #[tokio::test]
    async fn a_truncated_team_recall_reports_how_much_it_covered() {
        let dir = TempDir::new();
        let bank = LocalBank::new(dir.child(DEFAULT_BANKS_SUBDIR), "agent-");
        let roster: Vec<String> = (0..(MAX_RECALL_IDENTITIES + 3))
            .map(|n| format!("mate-{n}"))
            .collect();
        for who in &roster {
            bank.retain(&Record {
                identity: who.clone(),
                document_id: format!("run-{who}"),
                at: now(),
                content: format!("{who}'s note"),
                ..Record::default()
            })
            .expect("retain");
        }
        let scope = scope_of(
            &["alpha"],
            &roster.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let st = store();
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &bank);

        let got = k
            .recall_team(&Query {
                browse: true,
                ..Query::default()
            })
            .await
            .expect("recall_team");
        assert_eq!(got.identities_read, MAX_RECALL_IDENTITIES);
        assert_eq!(got.identities_total, roster.len());
        assert_eq!(got.facts.len(), MAX_RECALL_IDENTITIES);
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
            k.recent_runs(0).expect("recent_runs").facts,
            k.issue_runs("AAA-1", 0).expect("issue_runs").facts,
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
            k.issue_runs("AAA-1", 0).expect("issue_runs").facts.len() as i64,
            MAX_HISTORY_ROWS
        );
        assert_eq!(
            k.issue_runs("AAA-1", 1_000)
                .expect("issue_runs")
                .facts
                .len() as i64,
            MAX_HISTORY_ROWS
        );
        assert_eq!(k.issue_runs("AAA-1", 3).expect("issue_runs").facts.len(), 3);
    }

    /// **The page must be FILLED from in-scope rows (STUDIO-729 review, BLOCKER 2).** The store
    /// applies its `LIMIT` before this accessor can drop anything, so whenever the SQL filter is
    /// weaker than [`TeamScope::admits_run`] a capped page arrives full of rows that are all
    /// discarded — and a team that owns plenty of runs is told, confidently, that it owns none.
    ///
    /// Two shapes reach it, and the second is not a legacy edge: the empty slug means "no project
    /// filter" to the store, and the `linear_teams` gate is invisible to SQL on an ordinary slug.
    #[test]
    fn a_capped_page_is_filled_from_in_scope_rows() {
        // --- shape 1: the legacy empty slug, whose SQL filter is no filter at all.
        let st = store();
        for n in 0..MAX_HISTORY_ROWS {
            seed_ok(
                &st,
                "LEG-1",
                "",
                &format!("2026-09-01T10:{n:02}:00Z"),
                "completed",
            );
        }
        // …then MORE than a page of NEWER rows this team does not own, same identifier.
        for n in 0..(MAX_HISTORY_ROWS + 5) {
            seed_ok(
                &st,
                "LEG-1",
                "beta",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "failed",
            );
        }
        let scope = scope_of(&[""], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let runs = k.issue_runs("LEG-1", 0).expect("issue_runs").facts;
        assert_eq!(
            runs.len() as i64,
            MAX_HISTORY_ROWS,
            "the page was truncated to off-team rows before the drop ran"
        );
        assert!(
            runs.iter().all(|r| r.outcome == "completed"),
            "an off-team row survived: {runs:?}"
        );
        let recent = k.recent_runs(0).expect("recent_runs").facts;
        assert_eq!(recent.len() as i64, MAX_HISTORY_ROWS, "{recent:?}");
        assert!(
            recent.iter().all(|r| r.outcome == "completed"),
            "{recent:?}"
        );

        // --- shape 2: an ORDINARY slug, gated on a Linear team the SQL knows nothing about.
        let st = store();
        let seed_team = |key: &str, team: &str, started: &str, outcome: &str| {
            seed(
                &st,
                RunStart {
                    team_id: team.into(),
                    ..run_start(key, "alpha", started)
                },
                outcome,
                RunEnd::default(),
            );
        };
        for n in 0..5 {
            seed_team(
                "AAA-1",
                "linear-a",
                &format!("2026-09-01T10:{n:02}:00Z"),
                "completed",
            );
        }
        for n in 0..(MAX_HISTORY_ROWS + 5) {
            seed_team(
                "AAA-1",
                "linear-b",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "failed",
            );
        }
        let scope = scope_of(&["alpha"], &["alice"]).with_linear_teams(["linear-a"]);
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let runs = k.issue_runs("AAA-1", 0).expect("issue_runs").facts;
        assert_eq!(
            runs.len(),
            5,
            "the Linear-team gate emptied a page it should have filled"
        );
        assert!(runs.iter().all(|r| r.outcome == "completed"), "{runs:?}");
        assert_eq!(k.recent_runs(0).expect("recent_runs").facts.len(), 5);
    }

    /// **A short run history must say WHY it is short (STUDIO-729 review, SF3).** Three situations
    /// were byte-identical to a caller holding a bare `Vec<RunFact>`: the whole history, the newest
    /// `limit` of a longer one, and a page [`Knowledge::scan`] could not fill before it burned
    /// [`MAX_SCAN_ROWS`] on rows the scope drops. The third is the one that stings — a partial run
    /// list read back into a room reply as if it were the whole history — and it is the fill fix
    /// one notch further out, so [`Runs`] reports both bounds.
    #[test]
    fn a_short_run_history_reports_which_bound_shortened_it() {
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let scope = scope_of(&[""], &["alice"]);

        // --- complete: nothing was left behind and nothing was out of reach.
        let st = store();
        for n in 0..3 {
            seed_ok(
                &st,
                "LEG-1",
                "",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "completed",
            );
        }
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        let runs = k.issue_runs("LEG-1", 0).expect("issue_runs");
        assert_eq!(runs.facts.len(), 3);
        assert!(
            !runs.capped,
            "a complete history must not claim to be capped"
        );
        assert!(!runs.scan_exhausted);
        assert!(!k.recent_runs(0).expect("recent_runs").capped);

        // --- capped: the team owns MORE than the answer carries, and is told so. The scan reads
        // one admitted row past `limit` precisely so this is measured rather than inferred from a
        // full page — seeding exactly `MAX_HISTORY_ROWS` below proves it does not over-claim.
        let st = store();
        for n in 0..(MAX_HISTORY_ROWS + 5) {
            seed_ok(
                &st,
                "LEG-1",
                "",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "completed",
            );
        }
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        let runs = k.issue_runs("LEG-1", 0).expect("issue_runs");
        assert_eq!(runs.facts.len() as i64, MAX_HISTORY_ROWS);
        assert!(runs.capped, "a truncated history must say it was truncated");
        assert!(
            !runs.scan_exhausted,
            "nothing was out of reach; it was bounded"
        );
        assert!(k.recent_runs(0).expect("recent_runs").capped);
        // A caller cannot ask its way past the cap — `clamp_rows` bounds every request at
        // `MAX_HISTORY_ROWS` — so an explicit smaller limit is the only one that moves, and it
        // still reports honestly.
        assert!(k.issue_runs("LEG-1", 3).expect("issue_runs").capped);

        let st = store();
        for n in 0..MAX_HISTORY_ROWS {
            seed_ok(
                &st,
                "LEG-1",
                "",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "completed",
            );
        }
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        let runs = k.issue_runs("LEG-1", 0).expect("issue_runs");
        assert_eq!(runs.facts.len() as i64, MAX_HISTORY_ROWS);
        assert!(
            !runs.capped,
            "a history that exactly fills the page is complete, not capped"
        );

        // --- the ceiling: two in-scope runs, then more than MAX_SCAN_ROWS rows the scope drops.
        // The gather comes back with two of them and must not present that as the whole history.
        let st = store();
        for n in 0..2 {
            seed_ok(
                &st,
                "LEG-1",
                "",
                &format!("2026-09-02T10:{n:02}:00Z"),
                "completed",
            );
        }
        for n in 0..(MAX_SCAN_ROWS + 5) {
            let (h, m) = (n / 60, n % 60);
            seed_ok(
                &st,
                "LEG-1",
                "beta",
                &format!("2026-09-01T{h:02}:{m:02}:00Z"),
                "failed",
            );
        }
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);
        let runs = k.issue_runs("LEG-1", 0).expect("issue_runs");
        assert_eq!(
            runs.facts.len(),
            2,
            "the drop still holds: {:?}",
            runs.facts
        );
        assert!(
            runs.facts.iter().all(|r| r.outcome == "completed"),
            "an off-team row escaped: {:?}",
            runs.facts
        );
        assert!(
            runs.scan_exhausted,
            "the scan hit its ceiling and reported a complete history"
        );
        assert!(
            !runs.capped,
            "nothing was left behind by `limit`; the SEARCH ran out, not the page"
        );
        assert!(k.recent_runs(0).expect("recent_runs").scan_exhausted);
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

        assert!(
            k.issue_runs("AAA-1", 0)
                .expect("issue_runs")
                .facts
                .is_empty()
        );
        assert!(k.recent_runs(0).expect("recent_runs").facts.is_empty());
        assert!(k.issue("AAA-1").is_none());
        assert!(k.room(10).expect("room").is_empty());
        assert!(
            k.recall_team(&Query::default())
                .await
                .expect("recall_team")
                .facts
                .is_empty()
        );
    }

    // --- §9.5 slice 2: terminal reach, review verdicts, bounded gather (STUDIO-730) -----------

    /// Records every `summons_since` call so a test can assert the `gh` leg was NOT taken, and
    /// answers one summoning comment for PR 12.
    #[derive(Default)]
    struct RecordingSummons {
        calls: std::sync::Mutex<Vec<(String, String, DateTime<Utc>)>>,
        body: String,
        fail: bool,
    }

    impl RecordingSummons {
        fn with_body(body: &str) -> RecordingSummons {
            RecordingSummons {
                body: body.to_string(),
                ..RecordingSummons::default()
            }
        }

        fn failing() -> RecordingSummons {
            RecordingSummons {
                fail: true,
                ..RecordingSummons::default()
            }
        }

        fn calls(&self) -> Vec<(String, String, DateTime<Utc>)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl SummonSource for RecordingSummons {
        async fn summons_since(
            &self,
            owner: &str,
            repo: &str,
            since: DateTime<Utc>,
        ) -> crate::ghsummons::SummonResult {
            self.calls
                .lock()
                .expect("calls")
                .push((owner.to_string(), repo.to_string(), since));
            if self.fail {
                return Err("gh: HTTP 502".into());
            }
            Ok(HashMap::from([(
                12i64,
                SummonHit {
                    at: DateTime::from_timestamp(1_756_000_000, 0).expect("timestamp"),
                    body: self.body.clone(),
                },
            )]))
        }
    }

    /// Seeds one watch-set row for `pr` at `status`.
    fn seed_watch(st: &Sqlite, number: i64, reviewer: &str, author: &str, status: &str) {
        st.save_review_watch(rhapsody_store::ReviewWatchRow {
            key: ReviewWatchKey {
                owner: "acme".into(),
                repo: "rhapsody".into(),
                number,
                reviewer: reviewer.into(),
            },
            author: author.into(),
            introduced_by: "https://github.com/acme/rhapsody".into(),
            status: status.into(),
            open: true,
            ..rhapsody_store::ReviewWatchRow::default()
        })
        .expect("save review watch");
    }

    /// **The motivating case (§8, STUDIO-725).** A ticket that has reached a terminal state has
    /// fallen out of `cycle.issues`, so `issue()` alone answers nothing — which is precisely why
    /// *"what was the result of STUDIO-725?"* got silence. The store still holds its run, and the
    /// outcome gather reaches it.
    ///
    /// The same question about a ticket on ANOTHER team's project reaches the same store and gets
    /// [`NO_RECORD`] — not the row, not a field of it, and not a different degradation that would
    /// betray that the row exists.
    #[tokio::test]
    async fn a_terminal_ticket_resolves_on_the_team_and_degrades_off_it() {
        let st = store();
        let mine = seed_ok(
            &st,
            "STUDIO-725",
            "alpha",
            "2026-09-01T10:00:00Z",
            "completed",
        );
        seed_route(&st, mine, "alice");
        seed_ok(&st, "RHAP-42", "beta", "2026-09-01T11:00:00Z", "failed");

        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        // The cycle is EMPTY: a terminal ticket is not in it, which is the whole gap.
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let got = k.outcome("STUDIO-725").await.expect("outcome");
        assert_eq!(
            got.degradation(),
            None,
            "a terminal in-team ticket resolves"
        );
        assert_eq!(got.key, "STUDIO-725");
        assert!(got.issue.is_none(), "it is not in the cycle, and says so");
        assert_eq!(
            got.runs.facts,
            vec![RunFact {
                key: "STUDIO-725".into(),
                outcome: "completed".into(),
                ended_at: "2026-09-01T12:00:00Z".into(),
                identity: "alice".into(),
            }]
        );

        let off = k.outcome("RHAP-42").await.expect("outcome");
        assert_eq!(
            off.degradation(),
            Some(NO_RECORD),
            "another team's terminal ticket degrades"
        );
        assert!(off.runs.facts.is_empty(), "and carries no row");
    }

    /// **The STUDIO-729 carry-in.** `issue()` matched case-INSENSITIVELY while `issue_runs()`
    /// handed a case-SENSITIVE SQL `=` its argument, so `studio-725` answered *"I know that ticket
    /// and it has never been run"* — a confidently wrong answer on the exact path this slice adds.
    ///
    /// Both halves are pinned: the lower-case terminal key resolves to the outcome, and the same
    /// lower-case key off the team's projects still degrades. Normalizing must not have widened
    /// the scope while it fixed the case.
    #[tokio::test]
    async fn a_lower_case_terminal_key_resolves_and_still_respects_the_scope() {
        let st = store();
        seed_ok(
            &st,
            "STUDIO-725",
            "alpha",
            "2026-09-01T10:00:00Z",
            "completed",
        );
        seed_ok(&st, "RHAP-42", "beta", "2026-09-01T11:00:00Z", "failed");

        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        for typed in ["studio-725", "Studio-725", "  STUDIO-725  "] {
            let got = k.outcome(typed).await.expect("outcome");
            assert_eq!(
                got.runs.facts.first().map(|r| r.outcome.as_str()),
                Some("completed"),
                "{typed} must resolve to the run the canonical spelling finds"
            );
            assert_eq!(
                got.key, "STUDIO-725",
                "{typed} normalizes once, to one spelling"
            );
        }

        assert_eq!(
            k.outcome("rhap-42").await.expect("outcome").degradation(),
            Some(NO_RECORD),
            "the fold must not reach off-team rows the exact spelling could not"
        );
    }

    /// The two reads agree for a LIVE ticket too, whatever case it was typed in: `issue()` finds
    /// the cycle row and `issue_runs()` finds its runs, rather than one finding and the other not.
    #[test]
    fn the_boundary_normalizes_a_key_once_for_both_reads() {
        let st = store();
        seed_ok(&st, "AAA-7", "alpha", "2026-09-01T10:00:00Z", "completed");
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-7", "In Review", &["rhapsody:@alice"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert!(k.issue("aaa-7").is_some(), "issue() was already case-blind");
        assert_eq!(
            k.issue_runs("aaa-7", 0).expect("issue_runs").facts.len(),
            1,
            "issue_runs() must agree rather than report a known ticket as never run"
        );
    }

    /// A key the fold cannot recognise is read exactly as typed — the file tracker's minted
    /// `<team>-<n>` has a second hyphen, and folding it would invent a spelling no store holds.
    /// The raw probe covers the remaining case: a strict-shaped key a tracker spelled in lower
    /// case still resolves.
    #[tokio::test]
    async fn an_unrecognised_shape_is_read_as_typed() {
        let st = store();
        seed_ok(
            &st,
            "team-1-5",
            "alpha",
            "2026-09-01T10:00:00Z",
            "completed",
        );
        seed_ok(&st, "smk-9", "alpha", "2026-09-01T10:00:00Z", "failed");
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        assert_eq!(k.key("team-1-5").canonical(), "team-1-5");
        assert_eq!(
            k.outcome("team-1-5")
                .await
                .expect("outcome")
                .runs
                .facts
                .len(),
            1
        );
        // Folded to SMK-9, which the store does not hold — the raw probe is what finds it.
        assert_eq!(k.key("smk-9").canonical(), "SMK-9");
        let got = k.outcome("smk-9").await.expect("outcome");
        assert_eq!(
            got.runs.facts.first().map(|r| r.outcome.as_str()),
            Some("failed")
        );
        // …and the answer reports the spelling that MATCHED, not the fold that went looking for
        // it. Echoing "SMK-9" here would put an identifier no source holds into the facts block,
        // which is the one thing the design says an answer may never invent.
        assert_eq!(
            got.key, "smk-9",
            "the answer's key is the store's spelling once the store has spelled it"
        );
        // With nothing matched there is nothing to echo but the fold, and the degradation says so
        // rather than the key implying a record exists.
        let unknown = k.outcome("smk-404").await.expect("outcome");
        assert_eq!(unknown.key, "SMK-404");
        assert_eq!(unknown.degradation(), Some(NO_RECORD));
    }

    /// **The `gh` gate (§9.5 slice 2).** A question naming no pull request must not spawn a
    /// process: not for a live ticket, not for a terminal one, not for a key that resolves to
    /// nothing at all.
    #[tokio::test]
    async fn the_pr_comment_path_is_not_invoked_without_a_pr_key() {
        let st = store();
        seed_ok(&st, "AAA-1", "alpha", "2026-09-01T10:00:00Z", "completed");
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_APPROVED);
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-1", "In Review", &[])];
        let gh = RecordingSummons::with_body("@symphony requested changes");
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        for keyless in ["AAA-1", "STUDIO-725", "the thing we shipped", ""] {
            let got = k.outcome(keyless).await.expect("outcome");
            assert!(got.comment.is_none(), "{keyless} must carry no comment");
            assert!(!got.comment_unavailable, "{keyless} attempted nothing");
        }
        assert!(
            gh.calls().is_empty(),
            "no key named a pull request, so gh must never have been asked: {:?}",
            gh.calls()
        );

        // …and the same accessor DOES take the leg for a pull request this team is watching.
        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert_eq!(
            got.comment.map(|c| c.body),
            Some("@symphony requested changes".to_string())
        );
        assert_eq!(gh.calls().len(), 1, "exactly one call, for the named PR");
        let (owner, repo, since) = gh.calls()[0].clone();
        assert_eq!((owner.as_str(), repo.as_str()), ("acme", "rhapsody"));
        let window = Utc::now() - since;
        assert!(
            window >= Duration::days(PR_COMMENT_LOOKBACK_DAYS)
                && window <= Duration::days(PR_COMMENT_LOOKBACK_DAYS + 1),
            "the gh lookback is bounded to {PR_COMMENT_LOOKBACK_DAYS} days, got {window}"
        );
    }

    /// **The `gh` leg is team-scoped, not merely bounded (§3.2).** It is the one read that leaves
    /// the daemon, and it spends the daemon's OWN credential on a repository named in operator
    /// text — so a coordinate this team is not watching must not reach GitHub at all.
    ///
    /// Ungated, the accessor answers `some-other-org/private-infra#12` by reading a private
    /// repository the team has nothing to do with and returning it as a POSITIVE answer, which
    /// slice 3 renders into an unauthenticated shared room: a confused-deputy read primitive out of
    /// any token-mentioning comment the daemon's token can see. The watch set is the gate because
    /// it is the one place a PR coordinate is trusted from.
    #[tokio::test]
    async fn the_pr_comment_path_is_not_invoked_for_a_pull_request_off_the_team() {
        let st = store();
        // The team watches exactly one pull request, and it is not the one being asked about.
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_APPROVED);
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let gh = RecordingSummons::with_body("@symphony ship it");
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        for off in [
            // Another organisation's private repository, named in operator text.
            "https://github.com/some-other-org/private-infra/pull/12",
            "some-other-org/private-infra#12",
            // The team's OWN repository, but a pull request nobody on the roster watches.
            "acme/rhapsody#99",
            // The right pull request, but the reviewer named is not on this roster.
            "pr:acme/rhapsody#12@mallory",
        ] {
            let got = k.outcome(off).await.expect("outcome");
            assert!(
                got.reviews.is_empty(),
                "{off} resolves no in-roster watcher"
            );
            assert!(got.comment.is_none(), "{off} must carry no comment");
            assert!(
                !got.comment_unavailable,
                "{off} attempted nothing, so there is no gap to report"
            );
            assert_eq!(
                got.degradation(),
                Some(NO_RECORD),
                "{off} reached no source at all"
            );
        }
        assert!(
            gh.calls().is_empty(),
            "no in-roster watch row, so the daemon's credential must never have left: {:?}",
            gh.calls()
        );

        // The gate is the WATCH ROW, not the repository name: the same accessor reads the one
        // pull request the roster is watching.
        let watched = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert_eq!(
            watched.comment.map(|c| c.body),
            Some("@symphony ship it".into())
        );
        assert_eq!(gh.calls().len(), 1);
    }

    /// **A mis-cased pull-request coordinate resolves (STUDIO-729's bug, on this slice's path).**
    ///
    /// Three case-sensitive exact matches stand between what an operator types and this answer:
    /// `rhapsody_review_watch.owner`/`.repo`/`.reviewer` are TEXT with no `NOCASE`, the roster is a
    /// `BTreeSet`, and the review run's own `issue_identifier = ?` is a byte comparison. Miss any
    /// of them and the daemon says "no record" about a pull request the team is ACTIVELY
    /// reviewing — the confident wrongness this design exists to stop.
    #[tokio::test]
    async fn a_mis_cased_pull_request_key_resolves_and_still_respects_the_scope() {
        let st = store();
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_REVIEWED);
        seed_ok(
            &st,
            "pr:acme/rhapsody#12@alice",
            "alpha",
            "2026-09-01T10:00:00Z",
            "completed",
        );
        let scope = scope_of(&["alpha"], &["alice", "bob"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let gh = RecordingSummons::with_body("@symphony requested changes");
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        let expected = ReviewFact {
            reviewer: "alice".into(),
            author: "bob".into(),
            status: REVIEW_STATUS_REVIEWED.into(),
            open: true,
            // The review RUN resolved too, which is the third exact match: it is only reachable
            // through the store's own spelling of the coordinate, never the operator's.
            outcome: "completed".into(),
            ended_at: "2026-09-01T12:00:00Z".into(),
        };
        for typed in [
            "Acme/Rhapsody#12",
            "ACME/RHAPSODY#12",
            "pr:Acme/Rhapsody#12@Alice",
            "https://github.com/Acme/Rhapsody/pull/12",
        ] {
            let got = k.outcome(typed).await.expect("outcome");
            assert_eq!(
                got.reviews,
                vec![expected.clone()],
                "{typed} names the pull request the team is reviewing"
            );
            assert_eq!(got.degradation(), None, "{typed} is not a no-record");
            assert!(
                got.comment.is_some(),
                "{typed} is in scope, so the comment leg runs"
            );
        }

        // …and the case fold is not a way around the scope: a repository this team does not watch
        // stays unreachable however it is spelled.
        for off in ["Other/Repo#12", "OTHER/REPO#12"] {
            let got = k.outcome(off).await.expect("outcome");
            assert_eq!(got.degradation(), Some(NO_RECORD), "{off} is off the team");
        }
        assert_eq!(
            gh.calls().len(),
            4,
            "one call per in-scope spelling and none for an off-team one: {:?}",
            gh.calls()
        );
    }

    /// A pull-request key resolves its reviewers' VERDICTS from the watch set, plus each
    /// reviewer's own review run — and an off-roster reviewer's row is never read, because a watch
    /// row carries no project slug and roster membership is the whole team partition for it.
    #[tokio::test]
    async fn a_pr_key_resolves_the_watch_set_verdicts_inside_the_roster() {
        let st = store();
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_APPROVED);
        seed_watch(&st, 12, "mallory", "bob", REVIEW_STATUS_REVIEWED);
        seed_ok(
            &st,
            "pr:acme/rhapsody#12@alice",
            "alpha",
            "2026-09-01T10:00:00Z",
            "completed",
        );

        let scope = scope_of(&["alpha"], &["alice", "bob"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert_eq!(
            got.reviews,
            vec![ReviewFact {
                reviewer: "alice".into(),
                author: "bob".into(),
                status: REVIEW_STATUS_APPROVED.into(),
                open: true,
                outcome: "completed".into(),
                ended_at: "2026-09-01T12:00:00Z".into(),
            }],
            "mallory is not on this roster, so her verdict is not read"
        );
        assert_eq!(got.degradation(), None);

        // A review KEY names one reviewer, and asks about that one only.
        let one = k
            .outcome("pr:acme/rhapsody#12@alice")
            .await
            .expect("outcome");
        assert_eq!(one.reviews.len(), 1);
        assert_eq!(
            one.runs.facts.first().map(|r| r.key.as_str()),
            Some("pr:acme/rhapsody#12@alice"),
            "the review run itself is the key's own history"
        );

        // …and a review key naming an off-roster reviewer resolves to nothing at all.
        let off = k
            .outcome("pr:acme/rhapsody#12@mallory")
            .await
            .expect("outcome");
        assert!(off.reviews.is_empty());
        assert_eq!(off.degradation(), Some(NO_RECORD));
    }

    /// **The gather is bounded on every axis §9.3 names.** Run rows, the roster fan-out over a
    /// pull request, and the comment body are each capped, and each cap REPORTS itself so a
    /// bounded answer is never read as a complete one.
    #[tokio::test]
    async fn the_outcome_gather_is_bounded() {
        let st = store();
        for n in 0..(MAX_OUTCOME_RUNS + 4) {
            seed_ok(
                &st,
                "AAA-1",
                "alpha",
                &format!("2026-09-01T10:{n:02}:00Z"),
                "completed",
            );
        }
        let roster: Vec<String> = (0..(MAX_REVIEW_REVIEWERS + 3))
            .map(|n| format!("r{n:02}"))
            .collect();
        for r in &roster {
            seed_watch(&st, 12, r, "bob", REVIEW_STATUS_REVIEWED);
        }

        let names: Vec<&str> = roster.iter().map(String::as_str).collect();
        let scope = scope_of(&["alpha"], &names);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let long = "x".repeat(MAX_PR_COMMENT_BYTES * 3);
        let gh = RecordingSummons::with_body(&long);
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        let runs = k.outcome("AAA-1").await.expect("outcome");
        assert_eq!(runs.runs.facts.len() as i64, MAX_OUTCOME_RUNS);
        assert!(runs.runs.capped, "and says there is more behind the cap");

        let pr = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert_eq!(pr.reviews.len(), MAX_REVIEW_REVIEWERS);
        assert!(pr.reviewers_capped, "and says the fan-out was capped");
        let comment = pr.comment.expect("comment");
        assert_eq!(comment.body.len(), MAX_PR_COMMENT_BYTES);
        assert!(comment.truncated, "and says the body was clipped");
        assert_eq!(gh.calls().len(), 1, "one PR, one gh call");
    }

    /// A comment body is clipped on a CHARACTER boundary: the body is arbitrary UTF-8 from the
    /// internet and slicing through a multi-byte character would panic.
    #[tokio::test]
    async fn a_multi_byte_comment_body_is_clipped_without_panicking() {
        let st = store();
        // The comment leg is team-scoped, so it needs a pull request the roster is watching before
        // there is a body to clip at all.
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_APPROVED);
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        // 3 bytes per character, so the cap lands mid-character.
        let body = "€".repeat(MAX_PR_COMMENT_BYTES);
        let gh = RecordingSummons::with_body(&body);
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        let comment = got.comment.expect("comment");
        assert!(comment.truncated);
        assert!(comment.body.len() <= MAX_PR_COMMENT_BYTES);
        assert!(
            comment.body.chars().all(|c| c == '€'),
            "the clip must not have produced a partial character"
        );
    }

    /// A `gh` failure is reported, not swallowed and not fatal: the rest of the answer is still
    /// true, and refusing to answer because GitHub was unreachable is the silence §3.4 ends.
    #[tokio::test]
    async fn a_failed_pr_comment_fetch_is_reported_rather_than_hidden() {
        let st = store();
        seed_watch(&st, 12, "alice", "bob", REVIEW_STATUS_APPROVED);
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let gh = RecordingSummons::failing();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert!(got.comment.is_none());
        assert!(got.comment_unavailable, "the gap is reported");
        assert_eq!(
            got.reviews.len(),
            1,
            "and the rest of the answer still arrived"
        );
        assert_eq!(
            got.degradation(),
            None,
            "a reached-and-failed source is not an absence of records"
        );
    }

    /// **`degradation()` never claims "no record" about a source that was reached and FAILED.**
    ///
    /// The invariant is structural rather than a check, and this pins the shape of it: the only
    /// leg that fails without returning an error is `gh`, and it is not taken unless the watch set
    /// has already produced an in-roster row — so `comment_unavailable` implies a non-empty
    /// `reviews` implies [`Outcome::degradation`] is already `None`.
    ///
    /// With an EMPTY store the failing source is therefore never asked, and [`NO_RECORD`] is then
    /// the true answer rather than a flat assertion of absence in the one case the daemon
    /// demonstrably does not know.
    #[tokio::test]
    async fn a_flat_no_record_is_never_returned_over_a_source_that_failed() {
        let st = store();
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let gh = RecordingSummons::failing();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        assert!(
            !got.comment_unavailable,
            "nothing was attempted, so there is no gap to report"
        );
        assert!(
            gh.calls().is_empty(),
            "and the failing source was not asked"
        );
        assert_eq!(got.degradation(), Some(NO_RECORD));

        // The two are mutually exclusive by construction, over every key this test can reach.
        for probe in ["acme/rhapsody#12", "AAA-1", "pr:acme/rhapsody#12@alice", ""] {
            let got = k.outcome(probe).await.expect("outcome");
            assert!(
                !(got.comment_unavailable && got.degradation().is_some()),
                "{probe} claimed no-record while a source it reached had failed"
            );
        }
    }

    /// A ticket the cycle knows but the store has never run is NOT a degradation — it is the true
    /// answer. The degradation is reserved for a key that reached no source at all.
    #[tokio::test]
    async fn a_known_but_never_run_ticket_is_not_a_degradation() {
        let st = store();
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues = vec![issue_with("AAA-1", "Todo", &["rhapsody:@alice"])];
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let got = k.outcome("AAA-1").await.expect("outcome");
        assert_eq!(got.degradation(), None);
        assert_eq!(got.issue.map(|i| i.state), Some("Todo".to_string()));
        assert!(got.runs.facts.is_empty());
        assert_eq!(
            k.outcome("AAA-2").await.expect("outcome").degradation(),
            Some(NO_RECORD)
        );
    }

    /// **The watch-row projection is a leak guard too (§9.3, ANS-FIELD-LEAK).** A
    /// [`ReviewWatchRow`](rhapsody_store::ReviewWatchRow) carries two head SHAs and the origin it
    /// was introduced from — a repository URL. None of them answers *"what was the result"*, and
    /// the room is an unauthenticated shared log, so none of them may reach [`ReviewFact`]. As with
    /// [`RunFact`], a reviewer adding a column to the row changes nothing here.
    #[tokio::test]
    async fn the_projected_review_omits_the_shas_and_the_origin() {
        let st = store();
        st.save_review_watch(rhapsody_store::ReviewWatchRow {
            key: ReviewWatchKey {
                owner: "acme".into(),
                repo: "rhapsody".into(),
                number: 12,
                reviewer: "alice".into(),
            },
            author: "bob".into(),
            introduced_by: "git@github.com:acme/private-infra.git".into(),
            requested_sha: "1111111111111111111111111111111111111111".into(),
            last_reviewed_sha: "2222222222222222222222222222222222222222".into(),
            status: REVIEW_STATUS_APPROVED.into(),
            open: true,
        })
        .expect("save review watch");

        let scope = scope_of(&["alpha"], &["alice", "bob"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none);

        let got = k.outcome("acme/rhapsody#12").await.expect("outcome");
        let rendered = format!("{got:?}");
        for leaked in [
            "git@github.com:acme/private-infra.git",
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ] {
            assert!(
                !rendered.contains(leaked),
                "{leaked:?} reached the projection: {rendered}"
            );
        }
        assert_eq!(
            got.reviews.first().map(|r| r.status.as_str()),
            Some(REVIEW_STATUS_APPROVED),
            "…while the verdict itself still arrives"
        );
    }

    /// An operator string too long to be an identifier names NOTHING — it does not become a SQL
    /// parameter, it does not reach the `gh` leg, and it is not echoed back as a "key" for slice 3
    /// to render into the facts block. §9.3 bounds the gather, and the key is part of the gather.
    #[tokio::test]
    async fn an_over_long_key_names_nothing() {
        let st = store();
        let scope = scope_of(&["alpha"], &["alice"]);
        let none = NoneBackend;
        let issues: Vec<Issue> = Vec::new();
        let gh = RecordingSummons::with_body("@symphony requested changes");
        let k = Knowledge::new(&scope, &issues, st.as_ref(), &none).with_pr_comments(&gh);

        // Long enough to be a paste, and shaped like a pull-request coordinate so the `gh` leg
        // would fire if the cap did not come first.
        let essay = format!("acme/{}#12", "r".repeat(MAX_KEY_BYTES));
        let key = k.key(&essay);
        assert!(key.is_empty(), "an over-long key resolves to nothing");
        assert!(key.pr().is_none(), "and therefore names no pull request");

        let got = k.outcome(&essay).await.expect("outcome");
        assert_eq!(got.degradation(), Some(NO_RECORD));
        assert!(
            got.key.is_empty(),
            "the operator's paste is not echoed back"
        );
        assert!(gh.calls().is_empty(), "and gh was never asked");

        // …while a key at the cap is still an ordinary key.
        let at_cap = format!("acme/{}#12", "r".repeat(MAX_KEY_BYTES - "acme/#12".len()));
        assert_eq!(at_cap.len(), MAX_KEY_BYTES);
        assert!(k.key(&at_cap).pr().is_some());
    }

    /// The three shapes an operator has to hand, and the ones that must fail closed.
    #[test]
    fn a_pull_request_coordinate_is_read_from_the_three_shapes_that_exist() {
        let want = PrRef {
            owner: "acme".into(),
            repo: "rhapsody".into(),
            number: 12,
            reviewer: String::new(),
        };
        assert_eq!(parse_pr_ref("acme/rhapsody#12"), Some(want.clone()));
        assert_eq!(
            parse_pr_ref("https://github.com/acme/rhapsody/pull/12"),
            Some(want.clone())
        );
        assert_eq!(
            parse_pr_ref("https://github.com/acme/rhapsody/pull/12/files#r1"),
            Some(want.clone())
        );
        assert_eq!(
            parse_pr_ref("pr:acme/rhapsody#12@alice"),
            Some(PrRef {
                reviewer: "alice".into(),
                ..want
            })
        );

        for bad in [
            "",
            "STUDIO-725",
            "acme/rhapsody",
            "acme#12",
            "acme/rhapsody#0",
            "acme/rhapsody#-3",
            "acme/rhapsody#abc",
            "/rhapsody#12",
            "acme/#12",
            "https://github.com/acme/rhapsody/pull/",
        ] {
            assert_eq!(parse_pr_ref(bad), None, "{bad:?} must not name a PR");
        }
    }
}
