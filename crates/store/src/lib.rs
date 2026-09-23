//! rhapsody-store — parity port of Go `internal/store` (Symphony v0.4.0).
//!
//! This crate is the daemon's durable local history + restart-recovery layer. It persists every
//! run/session/event behind the [`Store`] trait (the port of Go's `Store` interface) over 6
//! tables — `runs`, `events`, `retry_queue`, `claims`, `totals`, `run_messages` — with two
//! implementations: [`Sqlite`] (pure-in-process SQLite via `rusqlite`, WAL mode) and [`Noop`]
//! (the guard-free disabled store used when `storage.path: off`).

use std::collections::HashMap;
use std::path::PathBuf;

mod noop;
mod sqlite;
mod types;

pub use noop::Noop;
pub use sqlite::{DEFAULT_RUN_LIMIT, Sqlite, effective_run_limit};
pub use types::*;

/// The resolved storage mode for the durable history + recovery store.
///
/// Mirrors the three cases Go documents on `config.Storage` (`internal/config/config.go`):
/// `off` disables persistence (a Noop store), `:memory:` is the ephemeral in-memory SQLite,
/// and any other value is an on-disk database path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorePath {
    /// Persistence disabled (`storage.path: off`) — handled by the Noop store (S3).
    Off,
    /// Ephemeral in-memory SQLite (`storage.path: :memory:`).
    InMemory,
    /// On-disk SQLite database at this path.
    Disk(PathBuf),
}

/// Classify a raw `storage.path` string into a [`StorePath`], reproducing Go's
/// `config.Storage.Off()` / `config.Storage.InMemory()` case/whitespace rules exactly:
///
/// * `off` — matched **case-insensitively** after trimming surrounding whitespace
///   (`strings.EqualFold(strings.TrimSpace(path), "off")`).
/// * `:memory:` — matched **case-sensitively** after trimming
///   (`strings.TrimSpace(path) == ":memory:"`).
/// * anything else — an on-disk [`StorePath::Disk`] holding the path **verbatim** (untrimmed),
///   because Go's `orchestrator.openStore` passes the raw config value to `store.Open(path)`.
///
/// `off` is ASCII, so `eq_ignore_ascii_case` is the faithful equivalent of Go's Unicode
/// `EqualFold` here (the only strings that fold to `off` are its ASCII case variants).
pub fn parse_store_path(s: &str) -> StorePath {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("off") {
        StorePath::Off
    } else if trimmed == ":memory:" {
        StorePath::InMemory
    } else {
        StorePath::Disk(PathBuf::from(s))
    }
}

/// Renders a summons timestamp in the ONE canonical form [`SummonWatermark::at`] is ever stored in:
/// RFC3339 UTC, seconds precision, `Z` suffix — identical to the format every other timestamp
/// column in this store uses.
///
/// It exists so the comparison "is what I just observed newer than what I remember?" can be made on
/// the STRINGS. Formatting both sides through here makes that comparison both chronological (the
/// form is fixed-width) and exactly round-trip stable — a summons whose source reports sub-second
/// precision renders to the same string every poll, so a stable comment is never rewritten tick
/// after tick just because its stored form lost a fraction of a second.
pub fn format_summon_at(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The error type for store operations. Go's store returns bare `error` values (wrapped with
/// `fmt.Errorf`); Rust makes the failure modes explicit while staying dependency-free.
#[derive(Debug)]
pub enum StoreError {
    /// [`Sqlite::open`] was called with [`StorePath::Off`]. SQLite has no representation for a
    /// disabled store (Go routes `off` to the Noop store, which lands in S3), so this is an
    /// error rather than a silently-empty database.
    Disabled,
    /// Creating the database file's parent directory failed.
    Io(std::io::Error),
    /// An underlying SQLite error (connection open, pragma, or migration).
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Disabled => {
                write!(f, "storage is disabled (path: off); use the Noop store")
            }
            StoreError::Io(e) => write!(f, "store i/o error: {e}"),
            StoreError::Sqlite(e) => write!(f, "sqlite error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Disabled => None,
            StoreError::Io(e) => Some(e),
            StoreError::Sqlite(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// Store is Symphony's persistence + recovery port — the port of Go's `store.Store` interface.
///
/// Implementations must be safe for concurrent use: the write-through methods are called from the
/// orchestrator actor and the event writer, while the read methods are called from the HTTP API.
/// [`Sqlite`] serializes all access through a single owned connection; [`Noop`] is stateless.
///
/// Go returns bare `error`; here every fallible method yields [`StoreError`]. Go's `(row, found,
/// err)` triple for a single lookup becomes `Result<Option<_>, _>`, and Go pointer fields map to
/// [`Option`].
pub trait Store {
    // --- run lifecycle (write-through from the orchestrator actor) ---
    fn start_run(&self, r: RunStart) -> Result<i64, StoreError>;
    fn end_run(&self, run_id: i64, e: RunEnd) -> Result<(), StoreError>;
    fn update_run_progress(&self, run_id: i64, p: RunProgress) -> Result<(), StoreError>;
    fn append_events(&self, run_id: i64, ev: &[EventRow]) -> Result<(), StoreError>;

    // --- restart-recovery ---
    fn save_retry(&self, r: RetryRow) -> Result<(), StoreError>;
    fn delete_retry(&self, issue_id: &str) -> Result<(), StoreError>;
    fn save_claim(&self, issue_id: &str, state: &str, project_slug: &str)
    -> Result<(), StoreError>;
    fn delete_claim(&self, issue_id: &str) -> Result<(), StoreError>;
    fn load_recovery(&self) -> Result<Recovery, StoreError>;
    fn mark_running_interrupted(&self) -> Result<i64, StoreError>;
    fn save_totals(&self, t: Totals) -> Result<(), StoreError>;
    fn load_totals(&self) -> Result<Totals, StoreError>;

    // --- history / queries (read-only, for the HTTP API) ---
    fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError>;
    /// One row per issue — the LATEST run of each `issue_identifier` matching `f`, most-recent
    /// first, paged by ISSUE (`f.limit`/`f.offset` count issues, not runs). The issue-grouped
    /// dashboard list reads this instead of grouping a run-paged fetch, so a single issue in a
    /// retry loop occupies exactly one row and cannot crowd other issues off the page (TRA-320).
    ///
    /// Runs with an empty `issue_identifier` are unattributed and are NOT grouped together: each
    /// stays its own row, matching the client-side grouping this replaces.
    fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError>;
    /// Whole-store token/runtime/run-count totals over the runs that started at or after `since`
    /// (an RFC3339 lower bound on `started_at`, compared as a string exactly like [`RunFilter::since`]).
    /// `now` is the RFC3339 instant an in-flight run's elapsed time is measured against. Backs the
    /// dashboard's header "today" cells, which must never be a fold over one fetched page (TRA-320).
    fn day_totals(&self, since: &str, now: &str) -> Result<DayTotals, StoreError>;
    fn issue_history(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError>;
    /// Every run whose `issue_identifier` is one of `identifiers`, most-recent first, capped at
    /// `limit` (<= 0 ⇒ the default page). One query for a WHOLE set rather than a probe per key.
    ///
    /// This is the review-run half of a ticket's run detail (STUDIO-976): the identifiers are the
    /// `pr:owner/repo#n@reviewer` keys the caller resolved from the watch set, so a ticket can show
    /// the reviews credited to it in time order with its attempts. It is deliberately not a new
    /// notion of which ticket a review belongs to — the caller joins with
    /// `rhapsody_orchestrator::reviewdone::origin_ticket`, the same reader the listing's `review_of`
    /// and the cost ledger use. Rhapsody-only; Go has no review runs.
    fn runs_for_issues(
        &self,
        identifiers: &[String],
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError>;
    /// Returns a single run row by id. `Ok(None)` (not an error) when no such run exists, so the
    /// caller can answer 404 without treating "missing" as an error.
    fn get_run(&self, run_id: i64) -> Result<Option<RunSummary>, StoreError>;
    fn run_events(&self, run_id: i64) -> Result<Vec<EventRow>, StoreError>;
    fn search_events(&self, q: EventQuery) -> Result<Vec<EventHit>, StoreError>;
    /// The `started_at` of the OLDEST run this store still holds, or `Ok(None)` when it holds no
    /// runs at all. RFC3339, compared as a string exactly like [`RunFilter::since`].
    ///
    /// This is the store's **evidence horizon** (STUDIO-672): the earliest instant it can answer a
    /// question about. Before it, "there is no record of X" and "the record of X is gone" are
    /// indistinguishable — [`Store::prune`] deletes ended runs wholesale, and a replaced or
    /// freshly-created database has no rows at any age. A caller that would ACT on an absence (the
    /// Teams identity-label reconcile removes a label when no run wore it) must bound itself to
    /// after this instant; `None` means the store can vouch for nothing and the caller must not act
    /// at all.
    ///
    /// Additive to the Go `store.Store` port: Rhapsody Teams has no Go counterpart, so neither does
    /// the question. It reads the existing `runs` table and adds no column, index or migration.
    fn earliest_run_start(&self) -> Result<Option<String>, StoreError>;
    fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError>;
    /// [`Store::metrics`] decomposed by provider (STUDIO-957): the same day window and project
    /// filter, one row per (`date`, `provider`) rather than per day. A run with no recorded
    /// provenance lands in the empty-provider bucket (LEFT JOIN) so the series still sums to the
    /// undifferentiated total. Rhapsody-only — Go has no provider dimension.
    fn metrics_by_provider(
        &self,
        since_days: i64,
        project: &str,
    ) -> Result<Vec<DayProviderRollup>, StoreError>;

    // --- per-run provenance (STUDIO-909) ---
    // Additive Rhapsody-only surface with no Go counterpart: the frozen reference records nothing
    // about what ran a run. Written once at dispatch and never rewritten, so a later config
    // hot-reload cannot change what a past run says it ran on. Backed by the
    // `rhapsody_run_provenance` table (see the README "Divergences" entry); a run with no row is
    // "unknown", which is why [`Store::run_provenance`] returns `None` rather than a zero value.
    //
    // [`Store::set_run_provenance`] inserts (upserting on `run_id`) unconditionally and does not
    // check that `run_id` names a live `runs` row; callers pass the id [`Store::start_run`]
    // returned.
    fn set_run_provenance(&self, run_id: i64, p: &RunProvenance) -> Result<(), StoreError>;
    /// One run's provenance, or `Ok(None)` when the run predates this feature (or recorded nothing).
    fn run_provenance(&self, run_id: i64) -> Result<Option<RunProvenance>, StoreError>;
    /// Provenance for a PAGE of run ids in one query, keyed by run id. Missing ids are absent from
    /// the map — the same "no answer" [`Store::run_provenance`] returns, batched so a listing never
    /// pays a query per row.
    fn load_run_provenances(
        &self,
        run_ids: &[i64],
    ) -> Result<HashMap<i64, RunProvenance>, StoreError>;
    /// Token totals grouped by recorded provider over the runs that started at or after `since` —
    /// the cost-attribution question this feature exists to answer, scoped to the same window as
    /// [`Store::day_totals`] so the two figures can be read beside each other. One row per distinct
    /// provider, empty provider included, run count descending then provider ascending so the order
    /// is stable.
    ///
    /// The window is the point (STUDIO-909 round 1): a lifetime total rendered under a "today"
    /// heading answers a question nobody asked and cannot be reconciled with `day_totals`.
    fn tokens_by_provider(&self, since: &str) -> Result<Vec<ProviderTokens>, StoreError>;
    /// Every run's tokens summed per (`issue_identifier`, provider), over the WHOLE store — the
    /// ledger behind a ticket's cost on the console (STUDIO-926). Unlike `list_issue_runs` this does
    /// not keep only each key's newest run: a ticket's cost is every run that spent on it. One row
    /// per distinct pair, empty provider included; order is stable (identifier, then provider).
    fn run_costs(&self) -> Result<Vec<RunCostBucket>, StoreError>;

    // --- operator messages (INF-250) ---
    /// Records a new operator message for a run with status "sent" and returns its row id. `body`
    /// is the operator's ORIGINAL (unwrapped) text.
    fn insert_run_message(
        &self,
        run_id: i64,
        body: &str,
        created_at_ms: i64,
    ) -> Result<i64, StoreError>;
    /// Stamps the OLDEST still-"sent" message for `run_id` as "delivered" with the given turn
    /// number (FIFO matches mailbox delivery order). A no-op when no "sent" row exists.
    fn mark_oldest_run_message_delivered(&self, run_id: i64, turn: i64) -> Result<(), StoreError>;
    /// Marks every still-"sent" message for `run_id` as "expired" (called at run end so
    /// undelivered messages don't linger as pending).
    fn expire_run_messages(&self, run_id: i64) -> Result<(), StoreError>;
    /// Returns all messages for a run ordered by id ASC.
    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError>;

    // --- ticketless review watch set (STUDIO-703 / STUDIO-711) ---
    // Additive Rhapsody-only surface with no Go counterpart: the frozen reference has no review
    // feature, so this is new state rather than ported state (README "Divergences"). It is the
    // restart-surviving home for the review watch set, at per-(PR, reviewer) granularity.
    //
    // The two SHA columns are the watcher's whole idempotency, so they are written by EXACTLY the
    // two methods named for the moments the design pins them to, and by nothing else:
    // [`Store::mark_review_requested`] at dispatch (F-DUP) and [`Store::mark_review_completed`] at
    // completion (F-SHA). [`Store::save_review_watch`] deliberately cannot move them on an
    // existing row.
    //
    // No dispatch or watcher logic lives here or calls these yet — this slice is the substrate
    // only. Nothing writes a row unless the Teams-gated review path is active, so on a Teams-off
    // daemon (the shipped default) the table simply stays empty.

    /// Introduces a (PR, reviewer) pair into the watch set, or re-arms one that is already there.
    ///
    /// On a NEW row every field of `w` is stored verbatim. On an EXISTING row (same
    /// [`ReviewWatchKey`]) only `introduced_by`, `status` and `open` are updated — `requested_sha`
    /// and `last_reviewed_sha` are PRESERVED. That asymmetry is the point: re-introducing a PR
    /// must never be able to forget which SHA was dispatched or reviewed, which is exactly how
    /// F-DUP double-dispatches and F-SHA loses a pushed fix.
    fn save_review_watch(&self, w: ReviewWatchRow) -> Result<(), StoreError>;

    /// Records the head SHA a reviewer run was DISPATCHED against and moves the row to
    /// [`REVIEW_STATUS_IN_FLIGHT`]. Never touches `last_reviewed_sha`.
    ///
    /// This is the edge-trigger the watcher gates on: without a persisted requested SHA the
    /// re-review condition stays true on every tick between introduction and first completion
    /// (design §14.1 F-DUP). A no-op when the row is absent.
    fn mark_review_requested(
        &self,
        key: &ReviewWatchKey,
        requested_sha: &str,
    ) -> Result<(), StoreError>;

    /// Records the head SHA a completed review ACTUALLY read, with its terminal `status`
    /// ([`REVIEW_STATUS_REVIEWED`] or [`REVIEW_STATUS_APPROVED`]). Never touches `requested_sha`.
    ///
    /// `reviewed_sha` must be the SHA pinned at checkout, NOT a completion-time re-query: a
    /// re-query records a mid-review push as reviewed and that commit is then never read by
    /// anyone (design §14.1 F-SHA). A no-op when the row is absent.
    fn mark_review_completed(
        &self,
        key: &ReviewWatchKey,
        reviewed_sha: &str,
        status: &str,
    ) -> Result<(), StoreError>;

    /// Records the EVIDENCE the last review that COMPLETED with a verdict left behind (STUDIO-1009;
    /// design record `manager-agent-design.md` §5.4) — the four `last_completed_*` columns beside
    /// [`Store::mark_review_completed`]'s `last_reviewed_sha`/`status`.
    ///
    /// `status` is the same terminal verdict [`Store::mark_review_completed`] takes, so the two
    /// writes that describe one completed round can be one call. The record's `sha` is written to
    /// `last_reviewed_sha` as well, so `last_completed_sha` and `last_reviewed_sha` always describe
    /// the same round. This is the ONLY writer of the four columns, and it is called ONLY for a
    /// review run that completed with a declared verdict — a truncated, dropped, failed or crashed
    /// round calls [`Store::mark_review_truncated`] (or nothing) instead, and must leave them
    /// untouched. A no-op when the row is absent.
    fn record_review_completion(
        &self,
        key: &ReviewWatchKey,
        status: &str,
        completed: &ReviewCompleted,
    ) -> Result<(), StoreError>;

    /// The completed-review record for one (PR, reviewer) row, or `None` when no review of that pair
    /// has completed with a verdict (or the row does not exist).
    fn review_completed(&self, key: &ReviewWatchKey)
    -> Result<Option<ReviewCompleted>, StoreError>;

    /// Records that a reviewer run ENDED without a declared verdict — it either burned its whole
    /// turn budget mid-review (STUDIO-721) or declared a hand-off whose payload was neither
    /// `approved` nor a recognised rejection (STUDIO-894) — by parking `status` at
    /// [`REVIEW_STATUS_TRUNCATED`] and touching NEITHER SHA column.
    ///
    /// Deliberately not a `mark_review_completed` with a third status: that method's contract is to
    /// advance `last_reviewed_sha`, and advancing it here is precisely the bug — the head was read
    /// only partially, so a watcher reading `last_reviewed_sha == head` would consider a partial
    /// review sufficient and never look at that head again. Leaving both SHAs alone keeps the row
    /// non-terminal, which is what re-arms the same head for another round. A no-op when the row is
    /// absent.
    fn mark_review_truncated(&self, key: &ReviewWatchKey) -> Result<(), StoreError>;

    /// Drops one (PR, reviewer) row out of the watch set: clears `open` and parks `status` at
    /// [`REVIEW_STATUS_DROPPED`]. The terminal for Slice 1's `MERGED` / `CLOSED` / gone states.
    /// Both SHAs are left intact as the record of what was reviewed. Idempotent, and a no-op when
    /// the row is absent.
    fn drop_review_watch(&self, key: &ReviewWatchKey) -> Result<(), StoreError>;

    /// Reads back one (PR, reviewer) row, or `Ok(None)` when the pair is not watched.
    fn get_review_watch(&self, key: &ReviewWatchKey) -> Result<Option<ReviewWatchRow>, StoreError>;

    /// The same read for a coordinate whose CASE may not match what was written: `owner`, `repo`
    /// and `reviewer` compare case-INSENSITIVELY, `number` exactly.
    ///
    /// It exists because the three key columns are plain `TEXT` with no `NOCASE` collation, so
    /// [`Store::get_review_watch`] is a byte comparison — correct for the watcher, which only ever
    /// looks a row up with the spelling it wrote, and wrong for a reader handed a coordinate a
    /// PERSON typed. GitHub matches an owner and a repository case-insensitively, and a reviewer is
    /// a roster identity, so `Acme/Rhapsody#12` and `acme/rhapsody#12` are the same pull request to
    /// everyone except this table; a reader that could not see that would answer "no record" about
    /// a pull request the team is actively reviewing.
    ///
    /// This is a read-only counterpart, deliberately NOT a change to the collation of the columns
    /// themselves: the writers match on the primary key's binary collation, and making the read and
    /// the write disagree about what one row is would be worse than the mis-cased read. The row
    /// comes back with the spelling the STORE holds, which is the spelling every other key derived
    /// from it must use. If case-variant duplicates of one coordinate exist — possible, because the
    /// primary key does not collapse them — the (owner, repo, reviewer) order picks one
    /// deterministically rather than reporting an arbitrary row.
    fn find_review_watch(&self, key: &ReviewWatchKey)
    -> Result<Option<ReviewWatchRow>, StoreError>;

    /// The whole watch set in a deterministic order (owner, repo, number, reviewer) — the boot
    /// snapshot restart recovery rebuilds from, the sibling of [`Store::load_recovery`].
    ///
    /// Returns EVERY row, including dropped and closed ones: which of them still deserve watching
    /// is the watcher's rule (Slice 5), not the store's, and folding that filter in here would
    /// hide a row that a later rule cares about.
    fn load_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError>;

    /// Only the rows still worth watching: `open` and not `dropped`. The watcher's hot paths run
    /// this once per tick and once per observation, and they all apply exactly this predicate the
    /// moment they get the rows back — so applying it in SQL costs nothing and stops a retired row
    /// being deserialized forever (STUDIO-727).
    ///
    /// It matters because a retirement is a SOFT delete: [`Store::drop_review_watch`] sets
    /// `status = 'dropped', open = 0` and [`Store::prune`] never touches this table, so the dead
    /// rows are permanent. Callers whose predicate is genuinely broader — retirement, which must
    /// also see a closed-but-undropped row — still use [`Store::load_review_watch`].
    fn load_live_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError>;

    // --- summons watermark (STUDIO-885; no Go counterpart — see [`SummonWatermark`]) ---

    /// Remembers `w` as this ticket's observed summons, replacing any row already there.
    ///
    /// Deliberately a last-write-wins upsert rather than a max-only one: the caller has just read
    /// [`Store::summon_watermark`] to decide whether what it observed is newer, and putting the
    /// same rule in SQL as well would mean two places could disagree about what "newer" means.
    /// The caller is the single-threaded control loop, so the read-then-write is not racing
    /// anything.
    fn record_summon_watermark(&self, w: SummonWatermark) -> Result<(), StoreError>;

    /// This ticket's remembered summons, or `None` when none was ever observed (or the store is
    /// disabled — with persistence off there is nowhere to remember one, so the daemon keeps the
    /// pre-STUDIO-885 behaviour of seeing only what is inside the lookback window right now).
    fn summon_watermark(&self, identifier: &str) -> Result<Option<SummonWatermark>, StoreError>;

    // --- durable review bound (STUDIO-956; no Go counterpart — see [`ReviewBoundRow`]) ---

    /// Records `pr`'s review↔author round counter, in dispatches, creating the row when it is the
    /// first thing known about that pull request and leaving any settled adjudication alone.
    ///
    /// The value is the caller's total rather than an increment. The counter has exactly one
    /// writer — the control task — and it is rehydrated into memory at boot, so the in-memory
    /// figure is authoritative and a last-write-wins column can never drift from it. An increment
    /// in SQL would put the arithmetic in two places and make a dropped write silently permanent.
    fn set_review_rounds(&self, pr: &str, dispatches: i64) -> Result<(), StoreError>;

    /// Records the manager's SETTLED decision about `pr`, creating the row when the counter has
    /// not been written yet and leaving the counter alone when it has.
    ///
    /// Only a settled decision reaches here; see [`ReviewAdjudication`] for why an in-flight marker
    /// is deliberately not durable.
    fn record_review_adjudication(
        &self,
        pr: &str,
        adjudication: &ReviewAdjudication,
    ) -> Result<(), StoreError>;

    /// Forgets the manager's decision about `pr` WITHOUT touching its round counter — the operator's
    /// re-run, which refunds one round and overrides the decision but does not reset the budget. A
    /// no-op when the row is absent.
    fn clear_review_adjudication(&self, pr: &str) -> Result<(), StoreError>;

    /// Forgets everything durable about `pr` — counter and decision both. The terminal for a pull
    /// request that left the watch set (merged, closed, dismissed) and for the operator's
    /// deliberate `POST /api/v1/reviews/clear`, so a pull request that is later re-introduced,
    /// reopened or rebuilt never inherits a spent budget. Idempotent.
    fn clear_review_bound(&self, pr: &str) -> Result<(), StoreError>;

    /// Every durable review bound, in `pr` order — the boot snapshot the round counter and the
    /// adjudication ledger are rehydrated from.
    fn load_review_bounds(&self) -> Result<Vec<ReviewBoundRow>, StoreError>;

    /// Establishes `pr`'s review LOOP GENERATION at 1 if no bound row exists, and does nothing when
    /// one does (STUDIO-1009; design record §5.1). Called at the pull request's FIRST introduction
    /// into the watch set; a handoff re-introduction is a no-op, which is exactly the F9 rule — a
    /// re-introduced row is not a new generation.
    ///
    /// A generation START at 1 (not 0) so a real generation is never confused with the M1 finding
    /// rows' placeholder `0`. Every creation path writes 1 (this one and the counter/adjudication/
    /// evidence-revision upserts, STUDIO-1010), and the v15→v16 migration backfills every existing
    /// row to 1, so a bound row's generation is always `>= 1` and `0` can only mean the daemon holds
    /// no bound row at all.
    fn ensure_review_generation(&self, pr: &str) -> Result<(), StoreError>;

    /// The operator's deliberate reset of `pr`'s review loop (STUDIO-1009; §7.4): INCREMENTS the
    /// loop generation and zeroes the round counter and any settled adjudication in the same write.
    /// This is the `/clear` path's durable effect, replacing the old wholesale delete — the row (and
    /// its generation) must survive a clear so later rounds know a reset happened.
    ///
    /// The row is created at generation 1 when it does not exist, so a clear on a watched pull
    /// request the daemon has never charged still leaves a generation behind.
    fn increment_review_generation(&self, pr: &str) -> Result<(), StoreError>;

    /// Writes `pr`'s evidence revision (§5.2), creating the bound row when it does not exist. Like
    /// [`Store::set_review_rounds`] the value is the caller's total rather than an increment: the
    /// control task is the single writer and holds the last-seen fingerprint in memory, so a
    /// last-write-wins column can never drift.
    fn set_review_evidence_rev(&self, pr: &str, evidence_rev: i64) -> Result<(), StoreError>;

    /// One pull request's durable review bound, or `None` when the daemon holds none for it.
    fn review_bound(&self, pr: &str) -> Result<Option<ReviewBoundRow>, StoreError>;

    // --- durable terminal-move ledger (STUDIO-1007; no Go counterpart — see [`ReviewDoneRow`]) ---

    /// Records `row` as a terminal-state move owed to `row.identifier`'s merged pull request, or
    /// replaces the existing row for that ticket with it.
    ///
    /// A last-write-wins upsert of the WHOLE row, attempts and `next_at` included, because there is
    /// exactly one writer — the off-loop auto-done half on the review watcher's task — so the row it
    /// last wrote is authoritative and no column needs protecting from a second writer. It is
    /// written BEFORE the first move attempt (which is what lets the handoff guard and the
    /// reconciliation sweep see the merge even while the move is still being tried) and rewritten by
    /// each retry with its new attempt count and next due time.
    fn save_review_done(&self, row: ReviewDoneRow) -> Result<(), StoreError>;

    /// Forgets the owed move for one ticket — the move LANDED. Idempotent, and a no-op when the row
    /// is absent: the normal case is that a daemon never had anything to move.
    fn clear_review_done(&self, identifier: &str) -> Result<(), StoreError>;

    /// Every owed terminal move, in `identifier` order — what the bounded retry walks each tick and
    /// what the reconciliation sweep reads to report a merged pull request whose ticket is not
    /// terminal.
    fn load_review_done(&self) -> Result<Vec<ReviewDoneRow>, StoreError>;

    // --- per-run review verdicts (STUDIO-1020; no Go counterpart) -------------------------------
    // A review run's own verdict, keyed by `runs.id` and written ONCE at the run's exit. The watch
    // set's `status` cannot answer this: it holds only the LATEST state per (PR, reviewer), so a
    // ticket with three rounds whose last review approved reads approved all the way back. Backed by
    // the `rhapsody_review_verdicts` table (see the README "Divergences" entry); a run with no row —
    // still running, ended without declaring a verdict, or predating this feature — is "no verdict".

    /// Records `run_id`'s verdict, upserting on the run id. Callers pass one of
    /// [`REVIEW_VERDICT_APPROVED`] / [`REVIEW_VERDICT_CHANGES_REQUESTED`]; a value the console
    /// cannot colour is refused by the caller, not stored.
    fn set_review_verdict(&self, run_id: i64, verdict: &str) -> Result<(), StoreError>;

    /// One run's verdict, or `Ok(None)` when it recorded none.
    fn review_verdict(&self, run_id: i64) -> Result<Option<String>, StoreError>;

    /// The verdicts of a SET of runs in one query, keyed by run id. Missing ids are absent — the
    /// same "no verdict" [`Store::review_verdict`] returns, batched so the run detail's review strip
    /// never pays a query per round.
    fn load_review_verdicts(&self, run_ids: &[i64]) -> Result<HashMap<i64, String>, StoreError>;

    // --- runaway-loop breaker crossings (STUDIO-1026; no Go counterpart — see
    // [`BreakerCrossingRow`]) ------------------------------------------------------------------

    /// How many COMPLETED review runs this daemon has recorded for one pull request — every `runs`
    /// row whose `issue_identifier` is `pr:<owner>/<repo>#<number>@<any reviewer>` with
    /// `outcome = completed`.
    ///
    /// ⚠️ NEVER [`Store::load_review_bounds`]' `dispatches`: that counts rounds the daemon ARMED and
    /// an operator's `clear` resets it. The breaker bounds SPEND, so it counts the runs that
    /// actually HAPPENED. Reads with the caller's explicit empty/whitespace identifiers truncated by
    /// LIKE's own escaping (see [`Sqlite`](crate::Sqlite)); a coordinate with no such runs answers 0.
    fn count_completed_review_runs(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<i64, StoreError>;

    /// How many runs this daemon has recorded for one exact `issue_identifier` — a ticket's
    /// "attempts" for the breaker's notification, counting every author run whatever its outcome.
    /// `0` for an identifier with no runs.
    fn count_runs_for(&self, identifier: &str) -> Result<i64, StoreError>;

    /// A ticket's lifetime token spend per provider, covering BOTH its author runs
    /// (`runs.issue_identifier = ticket`) AND the review runs on its pull request
    /// (`pr:<owner>/<repo>#<number>@*`), joined through `rhapsody_run_provenance.provider`
    /// (STUDIO-1026).
    ///
    /// The per-ticket cap's input. A run with no recorded provider lands in the empty-provider
    /// bucket exactly as [`Store::tokens_by_provider`] reports it, so the figure is never silently
    /// short by the rows a pre-STUDIO-909 daemon wrote.
    fn ticket_spend_by_provider(
        &self,
        ticket: &str,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<Vec<ProviderTokens>, StoreError>;

    /// Records one ticket's breaker crossings, upserting on the ticket identifier. The caller
    /// passes the whole row, so a crossing is persisted as an absolute fact (the highest notified
    /// round count and the providers already notified) rather than an increment.
    fn save_breaker_crossing(&self, row: &BreakerCrossingRow) -> Result<(), StoreError>;

    /// Every persisted breaker crossing, in ticket order — the boot snapshot the crossing ledger is
    /// rehydrated from, so a restart never re-notifies a crossing.
    fn load_breaker_crossings(&self) -> Result<Vec<BreakerCrossingRow>, StoreError>;

    // --- structured review findings (STUDIO-1008; no Go counterpart) -----------------------------
    // One row per finding REVISION a reviewer raised on a pull request (design record
    // `manager-agent-design.md` §5.3). The review path writes a revision at each completed round; a
    // later approving review by the same reviewer resolves that reviewer's open revisions. Nothing
    // writes `dismissed` yet — the manager (a later ticket) is its only writer — but the reopen rule
    // (§6.3) is already defined against it. Backed by `rhapsody_review_finding` (see the README
    // "Divergences" entry).

    /// Records one finding revision. A revision row is IMMUTABLE once written — a duplicate write is
    /// a no-op rather than a rewrite — so the row stays the record of what one completed review said.
    fn save_review_finding(&self, row: ReviewFindingRow) -> Result<(), StoreError>;

    /// Every finding revision recorded for `pr`, in `(reviewer, finding_id, revision)` order. The
    /// writer's read: it folds these to find the latest revision of each scoped finding id before
    /// appending the next one.
    fn load_review_findings(&self, pr: &str) -> Result<Vec<ReviewFindingRow>, StoreError>;

    /// The revisions that are still `open` AND `blocking` for `pr` — the read the manager and the
    /// later tickets consume. Resolved, dismissed and settled revisions, and non-blocking ones, are
    /// filtered in SQL.
    fn open_blocking_findings(&self, pr: &str) -> Result<Vec<ReviewFindingRow>, StoreError>;

    /// Resolves every `open` revision of `reviewer`'s on `pr`/`generation`, recording the approving
    /// review's run id in `resolved_by`. Part of §5.3's resolution rule; a no-op when the reviewer
    /// has nothing open.
    fn resolve_review_findings(
        &self,
        pr: &str,
        generation: i64,
        reviewer: &str,
        resolved_by: &str,
    ) -> Result<(), StoreError>;

    /// Deletes ended runs (and their events/messages/transcripts) older than `retention_days`.
    /// `retention_days <= 0` keeps everything forever (see the sqlite impl).
    fn prune(&self, retention_days: i64) -> Result<(), StoreError>;
    fn close(&self) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_case_insensitive_and_trimmed() {
        // strings.EqualFold(strings.TrimSpace(path), "off")
        for raw in ["off", "OFF", "Off", "oFf", "  off", "off\t", "\n off \n"] {
            assert_eq!(parse_store_path(raw), StorePath::Off, "raw = {raw:?}");
        }
    }

    #[test]
    fn memory_is_case_sensitive_and_trimmed() {
        // strings.TrimSpace(path) == ":memory:" — exact, case-sensitive.
        assert_eq!(parse_store_path(":memory:"), StorePath::InMemory);
        assert_eq!(parse_store_path("  :memory:  "), StorePath::InMemory);
    }

    #[test]
    fn memory_uppercase_is_a_disk_path() {
        // Unlike `off`, the `:memory:` check is case-sensitive, so `:MEMORY:` is a plain path.
        assert_eq!(
            parse_store_path(":MEMORY:"),
            StorePath::Disk(PathBuf::from(":MEMORY:"))
        );
    }

    #[test]
    fn disk_path_is_held_verbatim() {
        // Go passes the raw config value to store.Open — no trimming of the on-disk path.
        assert_eq!(
            parse_store_path("/Users/x/.symphony/symphony.db"),
            StorePath::Disk(PathBuf::from("/Users/x/.symphony/symphony.db"))
        );
        assert_eq!(
            parse_store_path("symphony.db"),
            StorePath::Disk(PathBuf::from("symphony.db"))
        );
    }
}
