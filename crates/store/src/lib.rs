//! rhapsody-store — parity port of Go `internal/store` (Symphony v0.4.0).
//!
//! This phase (P2 · S2) lands the persistence foundation: the storage-path modes and the
//! SQLite schema/open path. The full `Store` trait, CRUD, queries, retention, and the `Noop`
//! implementation arrive in S3.

use std::path::PathBuf;

mod sqlite;

pub use sqlite::Sqlite;

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
