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
