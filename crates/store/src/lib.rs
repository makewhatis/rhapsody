//! rhapsody-store — parity port of Go `internal/store` (Symphony v0.4.0).
//!
//! This crate is the daemon's durable local history + restart-recovery layer. It persists every
//! run/session/event behind the [`Store`] trait (the port of Go's `Store` interface) over 6
//! tables — `runs`, `events`, `retry_queue`, `claims`, `totals`, `run_messages` — with two
//! implementations: [`Sqlite`] (pure-in-process SQLite via `rusqlite`, WAL mode) and [`Noop`]
//! (the guard-free disabled store used when `storage.path: off`).

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

    /// Records that a reviewer run ENDED without finishing its round — it burned its whole turn
    /// budget mid-review — by parking `status` at [`REVIEW_STATUS_TRUNCATED`] and touching NEITHER
    /// SHA column (STUDIO-721).
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
