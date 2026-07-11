//! SQLite-backed persistence — the ported v0.4.0 schema (DDL + pragmas + open/init).
//!
//! Everything here is a faithful port of Go `internal/store/sqlite.go`'s open path: the
//! [`MIGRATIONS`] DDL and [`SCHEMA_VERSION`] are copied verbatim (they ARE the parity
//! contract — the schema golden asserts their stored form against `harness/fixtures/schema.sql`),
//! and [`Sqlite::open`] applies the same pragmas and the same idempotent `user_version`
//! migration loop. The `Store` trait, CRUD, queries, and retention land in S3.

use crate::{StoreError, StorePath};
use rusqlite::Connection;
use std::path::Path;

/// Current `PRAGMA user_version` — Go's `schemaVersion`. Each bump appends one step to
/// [`MIGRATIONS`]; [`migrate`] applies every step whose index is `>=` the DB's current version.
const SCHEMA_VERSION: i64 = 6;

/// Ordered schema migration steps, copied verbatim from Go's `migrations` slice
/// (`internal/store/sqlite.go`). `MIGRATIONS[i]` advances `user_version` from `i` to `i+1`.
/// The exact DDL text is the parity contract: SQLite canonicalizes each `CREATE` into
/// `sqlite_master`, and the schema golden reassembles that stored form back to the fixture.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [
    // v0 -> v1: initial schema (runs, events, retry_queue, claims, totals + indexes).
    r#"
CREATE TABLE IF NOT EXISTS runs (
  id               INTEGER PRIMARY KEY,
  issue_id         TEXT    NOT NULL DEFAULT '',
  issue_identifier TEXT    NOT NULL DEFAULT '',
  title            TEXT    NOT NULL DEFAULT '',
  attempt          INTEGER NOT NULL DEFAULT 0,
  session_uuid     TEXT    NOT NULL DEFAULT '',
  branch           TEXT    NOT NULL DEFAULT '',
  started_at       TEXT    NOT NULL DEFAULT '',
  ended_at         TEXT    NOT NULL DEFAULT '',
  outcome          TEXT    NOT NULL DEFAULT '',
  turns            INTEGER NOT NULL DEFAULT 0,
  input_tokens     INTEGER NOT NULL DEFAULT 0,
  output_tokens    INTEGER NOT NULL DEFAULT 0,
  total_tokens     INTEGER NOT NULL DEFAULT 0,
  error            TEXT    NOT NULL DEFAULT '',
  transcript_path  TEXT    NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS events (
  id      INTEGER PRIMARY KEY,
  run_id  INTEGER NOT NULL REFERENCES runs(id),
  seq     INTEGER NOT NULL DEFAULT 0,
  at      TEXT    NOT NULL DEFAULT '',
  kind    TEXT    NOT NULL DEFAULT '',
  tool    TEXT    NOT NULL DEFAULT '',
  text    TEXT    NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS retry_queue (
  issue_id   TEXT PRIMARY KEY,
  identifier TEXT    NOT NULL DEFAULT '',
  attempt    INTEGER NOT NULL DEFAULT 0,
  due_at_ms  INTEGER NOT NULL DEFAULT 0,
  error      TEXT    NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS claims (
  issue_id   TEXT PRIMARY KEY,
  state      TEXT NOT NULL DEFAULT '',
  claimed_at TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS totals (
  id              INTEGER PRIMARY KEY CHECK (id = 1),
  input_tokens    INTEGER NOT NULL DEFAULT 0,
  output_tokens   INTEGER NOT NULL DEFAULT 0,
  total_tokens    INTEGER NOT NULL DEFAULT 0,
  seconds_running INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_runs_identifier_started ON runs(issue_identifier, started_at);
CREATE INDEX IF NOT EXISTS idx_runs_outcome            ON runs(outcome);
CREATE INDEX IF NOT EXISTS idx_events_run_seq          ON events(run_id, seq);
CREATE INDEX IF NOT EXISTS idx_events_text             ON events(text);
"#,
    // v1 -> v2: multi-project routing (project_slug/repo columns + project index).
    r#"
ALTER TABLE runs        ADD COLUMN project_slug TEXT NOT NULL DEFAULT '';
ALTER TABLE runs        ADD COLUMN repo         TEXT NOT NULL DEFAULT '';
ALTER TABLE retry_queue ADD COLUMN project_slug TEXT NOT NULL DEFAULT '';
ALTER TABLE claims      ADD COLUMN project_slug TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project_slug);
"#,
    // v2 -> v3: token accounting for no-result turn ends (usage_estimated).
    r#"
ALTER TABLE runs ADD COLUMN usage_estimated INTEGER NOT NULL DEFAULT 0;
"#,
    // v3 -> v4: stop/resume run-action support (team_id on runs).
    r#"
ALTER TABLE runs ADD COLUMN team_id TEXT NOT NULL DEFAULT '';
"#,
    // v4 -> v5: run-outcome taxonomy v2 (rewrite stored outcomes to the six-value set).
    r#"
UPDATE runs SET outcome='continued' WHERE outcome='succeeded';
UPDATE runs SET outcome='completed' WHERE outcome='handoff';
UPDATE runs SET outcome='stopped'   WHERE outcome='canceled';
UPDATE runs SET outcome='failed'    WHERE outcome IN ('timed_out','stalled');
"#,
    // v5 -> v6: operator messages (run_messages table + index).
    r#"
CREATE TABLE IF NOT EXISTS run_messages (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL,
  body          TEXT    NOT NULL,
  created_at_ms INTEGER NOT NULL,
  status        TEXT    NOT NULL DEFAULT 'sent',
  delivered_turn INTEGER
);
CREATE INDEX IF NOT EXISTS idx_run_messages_run ON run_messages(run_id, id);
"#,
];

/// SQLite-backed durable history + recovery store (the parity port of Go's `sqliteStore`).
///
/// A single owned [`Connection`] is intentional: it serializes all access through one handle,
/// mirroring Go's `db.SetMaxOpenConns(1)` (WAL is kept for crash-safety + committed-read
/// visibility, not for read/write concurrency). The full `Store` method surface arrives in S3.
pub struct Sqlite {
    // The store handle. Written by `open` (and read by the tests + every `Store` trait method
    // that lands in S3), but this S2 slice ships only `open`, so production code does not yet
    // read it. `allow(dead_code)` marks that intentional, temporary state — removed in S3 when
    // the query/CRUD methods start reading it. (See the PR body.)
    #[allow(dead_code)]
    conn: Connection,
}

impl Sqlite {
    /// Open (creating if needed) the SQLite database for `path`, apply the busy-timeout / WAL /
    /// foreign-keys pragmas, and run the idempotent migrations — the port of Go `store.Open`.
    ///
    /// [`StorePath::Off`] has no SQLite representation (Go routes it to the Noop store, which
    /// lands in S3), so it returns [`StoreError::Disabled`] rather than opening anything.
    pub fn open(path: StorePath) -> Result<Sqlite, StoreError> {
        let mut conn = match path {
            StorePath::Off => return Err(StoreError::Disabled),
            StorePath::InMemory => Connection::open_in_memory()?,
            StorePath::Disk(dbpath) => {
                ensure_parent_dir(&dbpath)?;
                Connection::open(&dbpath)?
            }
        };
        // Pragmas ported from sqlite.go's DSN (`busy_timeout(5000)` / `journal_mode(WAL)` /
        // `foreign_keys(ON)`). busy_timeout guards rare writer-vs-writer contention; WAL keeps
        // committed data crash-safe and readable; foreign_keys keeps events tied to runs.
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Sqlite { conn })
    }
}

/// Create the database file's parent directory (mode `0o700`) when it is named and absent — the
/// port of Go `store.Open`'s `os.MkdirAll(filepath.Dir(path), 0o700)`. Bare/relative names
/// (`filepath.Dir` == "." — e.g. `symphony.db`) and the current directory are skipped: they are
/// already present. Without this, a first run whose parent dir is absent fails with `CANTOPEN`
/// and the daemon silently loses persistence.
fn ensure_parent_dir(path: &Path) -> Result<(), StoreError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() && dir != Path::new(".") => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(dir)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Apply every pending migration step inside a transaction, advancing `PRAGMA user_version`
/// one step at a time — the port of Go `migrate`. Idempotent: on an up-to-date DB the loop runs
/// zero times, so re-opening a Go-written v6 database never re-applies a step.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for v in current..SCHEMA_VERSION {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATIONS[v as usize])?;
        // user_version does not accept a bound parameter; `v + 1` is a trusted constant.
        tx.execute_batch(&format!("PRAGMA user_version = {};", v + 1))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_store_path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique, freshly-created scratch directory under the system temp dir. Avoids a
    /// tempfile dependency; uniqueness comes from the pid + a per-process atomic counter.
    fn scratch_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rhapsody-store-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Reassemble the live schema the way `sqlite3 .schema` (which produced the fixture) does.
    ///
    /// Two documented normalizations reconcile a live `sqlite_master` with the committed text:
    /// `sql IS NOT NULL` drops the implicit PRIMARY KEY / UNIQUE auto-indexes (they carry no
    /// DDL), and `name NOT LIKE 'sqlite_%'` drops SQLite-internal bookkeeping — here the
    /// `sqlite_sequence` table the AUTOINCREMENT `run_messages` PK creates, a reserved-namespace
    /// object that is not application schema and is absent from the fixture (excluding it is
    /// standard, not a loosened assertion).
    ///
    /// `IF NOT EXISTS` needs no stripping: SQLite already canonicalizes it out of the stored
    /// `sql`. `ORDER BY rowid` preserves creation order, exactly as the fixture was captured.
    fn schema_dump(store: &Sqlite) -> String {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT sql FROM sqlite_master \
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
                 ORDER BY rowid",
            )
            .expect("prepare schema query");
        let dump: String = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query schema")
            .map(|sql| format!("{};\n", sql.expect("schema row")))
            .collect();
        dump
    }

    // S2 golden: opening `:memory:` and applying the ported migrations must produce a schema
    // byte-identical to the committed `harness/fixtures/schema.sql` (captured from Go v0.4.0).
    #[test]
    fn schema_matches_committed_golden() {
        let store = Sqlite::open(StorePath::InMemory).expect("open in-memory");
        assert_eq!(
            schema_dump(&store),
            harness_fixtures::load("schema.sql"),
            "reassembled schema must be byte-identical to harness/fixtures/schema.sql"
        );
    }

    // S2 round-trip gate: the committed database written by the Go daemon opens under the Rust
    // store and its rows are readable. We open a throwaway COPY, not the committed fixture:
    // `Sqlite::open` applies `journal_mode=WAL`, which rewrites the file header and spawns
    // `-wal`/`-shm` sidecars — opening the fixture in place would mutate it and dirty the tree.
    // (S3's round-trip test uses the same copy pattern before writing back.)
    #[test]
    fn opens_go_written_database_and_reads_rows() {
        let src = harness_fixtures::fixtures_dir().join("db/go-daemon.db");
        let scratch = scratch_dir();
        let db = scratch.join("go-daemon.db");
        std::fs::copy(&src, &db).expect("copy fixture db");

        let store = Sqlite::open(StorePath::Disk(db)).expect("open go-written db");
        let runs: i64 = store
            .conn
            .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
            .expect("count runs");
        assert!(runs > 0, "expected the Go daemon's run row(s), got {runs}");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    // Migrating an already-current (v6) database is a no-op: opening the Go-written fixture (a
    // second time, via a fresh copy) must not re-run a step (an ALTER re-apply would error).
    #[test]
    fn reopening_current_database_is_idempotent() {
        let src = harness_fixtures::fixtures_dir().join("db/go-daemon.db");
        let scratch = scratch_dir();
        let db = scratch.join("go-daemon.db");
        std::fs::copy(&src, &db).expect("copy fixture db");

        // Open twice in sequence; the second open sees user_version already at 6.
        let _first = Sqlite::open(StorePath::Disk(db.clone())).expect("first open");
        let second = Sqlite::open(StorePath::Disk(db)).expect("reopen current db");
        let version: i64 = second
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, SCHEMA_VERSION, "schema version must stay at 6");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    // `off` has no SQLite store; Sqlite::open must reject it (the daemon uses Noop instead).
    #[test]
    fn open_off_is_rejected() {
        assert!(matches!(
            Sqlite::open(parse_store_path("off")),
            Err(StoreError::Disabled)
        ));
    }
}
