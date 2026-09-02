//! SQLite-backed persistence — the ported v0.4.0 schema (DDL + pragmas + open/init).
//!
//! Everything here is a faithful port of Go `internal/store/sqlite.go`'s open path: the
//! [`MIGRATIONS`] DDL and [`SCHEMA_VERSION`] are copied verbatim (they ARE the parity
//! contract — the schema golden asserts their stored form against `harness/fixtures/schema.sql`),
//! and [`Sqlite::open`] applies the same pragmas and the same idempotent `user_version`
//! migration loop. The `Store` trait, CRUD, queries, and retention land in S3.
//!
//! # The one divergent schema object, and how the golden still gates the rest (STUDIO-711)
//!
//! Migration steps 7 and 8 (`rhapsody_review_watch` and its `author` column) have no Go
//! counterpart: they are the ticketless
//! PR-review watch set, a feature the frozen v0.4.0 reference does not have. That creates a
//! problem the rest of the schema does not have. `harness/fixtures/schema.sql` is recapturable
//! ONLY from the real Go daemon (`make fixtures`), so it can never be made to contain a table the
//! Go daemon cannot create — a naive new table would turn
//! `schema_matches_committed_golden` permanently red with no honest way to fix it. Hand-editing
//! the golden to add the table would be exactly the drift laundering the parity discipline exists
//! to prevent.
//!
//! **The mechanism** (README, "Divergences"): every Rhapsody-only schema object is NAMED with the
//! `rhapsody_` prefix, and the golden comparison excludes objects by that prefix and nothing else
//! (matched literally — the `_` is `ESCAPE`d rather than left as a LIKE wildcard).
//! The exclusion is therefore a name rule, not a loosened assertion — a Go table can never be
//! named `rhapsody_*`, so the golden keeps gating all 6 ported tables byte-strictly, and a NEW
//! table added without the prefix still turns it red. `divergent_objects_are_gated_by_name_only`
//! asserts that property directly, and pins the divergent object set to exactly one name.
//!
//! A new schema object that is a PORT of Go behaviour must never take the prefix: it belongs in
//! the golden, recaptured with `make fixtures`.

use crate::*;
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Current `PRAGMA user_version` — Go's `schemaVersion`. Each bump appends one step to
/// [`MIGRATIONS`]; [`migrate`] applies every step whose index is `>=` the DB's current version.
///
/// Go v0.4.0 froze at 6. Steps 7 and 8 are Rhapsody-only (the ticketless review watch set, then its
/// `author` column) and are the one documented reason this number is ahead of the reference — see
/// the module doc above.
const SCHEMA_VERSION: i64 = 8;

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
    // v6 -> v7: RHAPSODY-ONLY (no Go counterpart) — the ticketless PR-review watch set, one row
    // per (PR, reviewer). Named with the `rhapsody_` prefix so the schema golden gates it out by
    // name; see this module's doc comment and the README "Divergences" entry.
    //
    // The composite PRIMARY KEY *is* the per-(PR, reviewer) granularity, and its implicit
    // auto-index carries no DDL of its own (`sqlite_master.sql IS NULL`), so no explicit index is
    // added and nothing further reaches the golden comparison.
    r#"
CREATE TABLE IF NOT EXISTS rhapsody_review_watch (
  owner             TEXT    NOT NULL,
  repo              TEXT    NOT NULL,
  number            INTEGER NOT NULL,
  reviewer          TEXT    NOT NULL,
  introduced_by     TEXT    NOT NULL DEFAULT '',
  requested_sha     TEXT    NOT NULL DEFAULT '',
  last_reviewed_sha TEXT    NOT NULL DEFAULT '',
  status            TEXT    NOT NULL DEFAULT 'requested',
  open              INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (owner, repo, number, reviewer)
);
"#,
    // v7 -> v8: the pull request's AUTHOR on each watch row (STUDIO-721). Rhapsody-only, on the
    // Rhapsody-only table, so the golden gates it out by the same name rule.
    //
    // A separate step rather than an edit to the step above: step 7 already shipped, so a database
    // that has run it is at `user_version = 7` and would never re-execute a changed CREATE TABLE.
    // `ADD COLUMN` with a NOT NULL DEFAULT backfills every existing row with the empty string,
    // which the reviewer-selection path reads as "author unknown" and fails closed on.
    r#"
ALTER TABLE rhapsody_review_watch ADD COLUMN author TEXT NOT NULL DEFAULT '';
"#,
];

/// Name prefix carried by every Rhapsody-only schema object, and the ONLY thing that excludes an
/// object from the Go-recaptured schema golden (STUDIO-711). See this module's doc comment for why
/// the exclusion is a name rule rather than a loosened assertion.
#[cfg(test)]
const DIVERGENT_OBJECT_PREFIX: &str = "rhapsody_";

/// The `rhapsody_review_watch` columns, in DDL order — the single shared list for the watch-set
/// queries, read POSITIONALLY by [`map_review_watch`] exactly as [`RUN_COLS`] is by
/// `map_run_summary`.
const REVIEW_WATCH_COLS: &str = "owner, repo, number, reviewer, author, introduced_by, \
     requested_sha, last_reviewed_sha, status, open";

/// The `WHERE` clause selecting exactly one (PR, reviewer) row, with `?1..?4` bound from a
/// [`ReviewWatchKey`] by [`review_watch_key_params`]. Shared by every point read and write so no
/// two of them can ever disagree about what "one row" means.
const REVIEW_WATCH_KEY_WHERE: &str = "owner = ?1 AND repo = ?2 AND number = ?3 AND reviewer = ?4";

/// The four key columns as positional params `?1..?4` for [`REVIEW_WATCH_KEY_WHERE`].
fn review_watch_key_params(key: &ReviewWatchKey) -> [&dyn rusqlite::ToSql; 4] {
    [&key.owner, &key.repo, &key.number, &key.reviewer]
}

/// Scan one `rhapsody_review_watch` row selected with [`REVIEW_WATCH_COLS`] (positional, in DDL
/// order). `open` reads the INTEGER 0/1 column as a bool, exactly like `runs.usage_estimated`.
fn map_review_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewWatchRow> {
    Ok(ReviewWatchRow {
        key: ReviewWatchKey {
            owner: row.get(0)?,
            repo: row.get(1)?,
            number: row.get(2)?,
            reviewer: row.get(3)?,
        },
        author: row.get(4)?,
        introduced_by: row.get(5)?,
        requested_sha: row.get(6)?,
        last_reviewed_sha: row.get(7)?,
        status: row.get(8)?,
        open: row.get(9)?,
    })
}

/// SQLite-backed durable history + recovery store (the parity port of Go's `sqliteStore`).
///
/// A single owned [`Connection`] behind a [`Mutex`] is intentional: it serializes ALL access
/// through one handle, faithfully modeling Go's `db.SetMaxOpenConns(1)` plus its write mutex.
/// (WAL is kept for crash-safety + committed-read visibility, not for read/write concurrency —
/// so full serialization matches the Go store's observable behavior.) The mutex also makes the
/// store `Sync`, so the HTTP API reads and the actor writes can share one `Sqlite` across threads.
pub struct Sqlite {
    conn: Mutex<Connection>,
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
        Ok(Sqlite {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the single connection. A poisoned mutex (a prior holder panicked) is recovered rather
    /// than propagated: the store's own methods never panic while holding the lock, and a SQLite
    /// handle stays usable, so recovering keeps the daemon persistence-alive instead of wedging it.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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

/// Current UTC time as RFC3339 with seconds precision and a `Z` suffix — the port of Go
/// `nowRFC3339()` (`time.Now().UTC().Format(time.RFC3339)`), the format every timestamp column uses.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `now - days` as an RFC3339 cutoff string (UTC, seconds precision), matching Go's
/// `time.Now().UTC().AddDate(0, 0, -days).Format(time.RFC3339)`. In UTC a calendar day is exactly
/// 24h, so day-granularity `Duration` reproduces `AddDate`'s day arithmetic.
///
/// Panic-free on the production path (prune/metrics): `try_days` and `checked_sub_signed` both fall
/// back rather than panic on an absurd (out-of-`TimeDelta`-range) `days`, which a real
/// retention/metrics window never reaches.
fn days_ago_rfc3339(days: i64) -> String {
    let delta = chrono::Duration::try_days(days).unwrap_or_default();
    chrono::Utc::now()
        .checked_sub_signed(delta)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Default page size for history run queries (Go `defaultRunLimit`). Public because the HTTP layer
/// must know the page size that will ACTUALLY be applied in order to report `next_offset` honestly
/// — a caller that sends no `limit` still gets a bounded page, and telling it otherwise loses every
/// row past the first page (TRA-320). Resolve it through [`effective_run_limit`] rather than
/// re-deriving the `<= 0` rule at the call site.
pub const DEFAULT_RUN_LIMIT: i64 = 50;

/// The page size a run query will actually apply for a requested `limit`: the caller's value when
/// positive, else [`DEFAULT_RUN_LIMIT`]. Single source of truth for the default, shared by
/// [`Store::list_runs`]/[`Store::issue_history`]/[`Store::list_issue_runs`] and by the history
/// handler that derives `next_offset` from it (TRA-320).
pub fn effective_run_limit(limit: i64) -> i64 {
    if limit <= 0 { DEFAULT_RUN_LIMIT } else { limit }
}
/// Default limit for the cross-run event search (Go `defaultEventLimit`).
const DEFAULT_EVENT_LIMIT: i64 = 100;

/// The `runs` column list shared by every run-projection query (Go `runCols`). The scan in
/// [`map_run_summary`] reads these columns positionally, so the order is load-bearing.
const RUN_COLS: &str = "id, issue_id, issue_identifier, title, attempt, session_uuid, branch, \
     started_at, ended_at, outcome, turns, input_tokens, output_tokens, total_tokens, \
     error, transcript_path, project_slug, repo, usage_estimated, team_id";

/// Map one `runs` row (selected as [`RUN_COLS`]) to a [`RunSummary`] — the port of Go
/// `queryRuns`'s `rows.Scan`. `usage_estimated` reads the INTEGER 0/1 column as a bool.
fn map_run_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        id: row.get(0)?,
        issue_id: row.get(1)?,
        issue_identifier: row.get(2)?,
        title: row.get(3)?,
        attempt: row.get(4)?,
        session_uuid: row.get(5)?,
        branch: row.get(6)?,
        started_at: row.get(7)?,
        ended_at: row.get(8)?,
        outcome: row.get(9)?,
        turns: row.get(10)?,
        input_tokens: row.get(11)?,
        output_tokens: row.get(12)?,
        total_tokens: row.get(13)?,
        error: row.get(14)?,
        transcript_path: row.get(15)?,
        project_slug: row.get(16)?,
        repo: row.get(17)?,
        usage_estimated: row.get(18)?,
        team_id: row.get(19)?,
    })
}

/// Build the ` WHERE …` clause (empty string when unfiltered) and its bound arguments for a
/// [`RunFilter`], in the column order Go's `ListRuns` appends them. Shared by [`Store::list_runs`]
/// and [`Store::list_issue_runs`] so run-paged and issue-paged listings can never drift on which
/// rows they select — only on how they page (TRA-320).
fn run_filter_where(f: &RunFilter) -> (String, Vec<Value>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    if !f.issue.is_empty() {
        clauses.push("issue_identifier = ?");
        args.push(Value::Text(f.issue.clone()));
    }
    if !f.outcome.is_empty() {
        clauses.push("outcome = ?");
        args.push(Value::Text(f.outcome.clone()));
    }
    if !f.since.is_empty() {
        clauses.push("started_at >= ?");
        args.push(Value::Text(f.since.clone()));
    }
    if !f.project.is_empty() {
        clauses.push("project_slug = ?");
        args.push(Value::Text(f.project.clone()));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (sql, args)
}

/// Escape LIKE wildcards so a user's literal `%` or `_` matches literally (paired with `ESCAPE
/// '\'` in the query) — the port of Go `escapeLike`. Backslash MUST be escaped first so the
/// backslashes introduced by the `%`/`_` rules are not double-escaped.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The non-empty `transcript_path` values of every ended run older than `cutoff` (the same
/// predicate [`Sqlite::prune`]'s DELETE uses) — the port of Go `prunablePaths`. Best-effort: a
/// failed read yields whatever was collected so far and never blocks the row prune.
fn prunable_paths(conn: &Connection, cutoff: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT transcript_path FROM runs \
          WHERE ended_at IS NOT NULL AND ended_at <> '' AND ended_at < ?1 AND transcript_path <> ''",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([cutoff], |row| row.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for p in rows {
        match p {
            Ok(p) => out.push(p),
            Err(_) => return out,
        }
    }
    out
}

impl Sqlite {
    /// Shared run-projection query used by [`Store::list_runs`], [`Store::issue_history`], and
    /// [`Store::get_run`] — the port of Go `queryRuns`.
    fn query_runs(&self, query: &str, args: Vec<Value>) -> Result<Vec<RunSummary>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(query)?;
        let rows = stmt.query_map(params_from_iter(args), map_run_summary)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

impl Store for Sqlite {
    fn start_run(&self, r: RunStart) -> Result<i64, StoreError> {
        let started = if r.started_at.is_empty() {
            now_rfc3339()
        } else {
            r.started_at
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runs
               (issue_id, issue_identifier, title, attempt, session_uuid, branch,
                started_at, outcome, turns, input_tokens, output_tokens, total_tokens,
                error, transcript_path, project_slug, repo, usage_estimated, team_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, 0, 0, '', ?9, ?10, ?11, 0, ?12)",
            params![
                r.issue_id,
                r.issue_identifier,
                r.title,
                r.attempt,
                r.session_uuid,
                r.branch,
                started,
                OUTCOME_RUNNING,
                r.transcript_path,
                r.project_slug,
                r.repo,
                r.team_id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn end_run(&self, run_id: i64, e: RunEnd) -> Result<(), StoreError> {
        let ended = if e.ended_at.is_empty() {
            now_rfc3339()
        } else {
            e.ended_at
        };
        let conn = self.lock();
        // A non-empty transcript_path overwrites the column with the concrete per-run file; an
        // empty value leaves it as set at StartRun (COALESCE/NULLIF keeps the existing value).
        conn.execute(
            "UPDATE runs
                SET outcome = ?1, ended_at = ?2, turns = ?3,
                    input_tokens = ?4, output_tokens = ?5, total_tokens = ?6, error = ?7,
                    usage_estimated = ?8,
                    transcript_path = COALESCE(NULLIF(?9, ''), transcript_path)
              WHERE id = ?10",
            params![
                e.outcome,
                ended,
                e.turns,
                e.input_tokens,
                e.output_tokens,
                e.total_tokens,
                e.error,
                e.usage_estimated,
                e.transcript_path,
                run_id,
            ],
        )?;
        Ok(())
    }

    fn update_run_progress(&self, run_id: i64, p: RunProgress) -> Result<(), StoreError> {
        let conn = self.lock();
        // A non-empty transcript_path overwrites; an empty one leaves it unchanged (see end_run).
        conn.execute(
            "UPDATE runs
                SET turns = ?1, input_tokens = ?2, output_tokens = ?3, total_tokens = ?4,
                    usage_estimated = ?5,
                    transcript_path = COALESCE(NULLIF(?6, ''), transcript_path)
              WHERE id = ?7",
            params![
                p.turns,
                p.input_tokens,
                p.output_tokens,
                p.total_tokens,
                p.usage_estimated,
                p.transcript_path,
                run_id,
            ],
        )?;
        Ok(())
    }

    fn append_events(&self, run_id: i64, ev: &[EventRow]) -> Result<(), StoreError> {
        if ev.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO events (run_id, seq, at, kind, tool, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for e in ev {
                stmt.execute(params![run_id, e.seq, e.at, e.kind, e.tool, e.text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn save_retry(&self, r: RetryRow) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO retry_queue (issue_id, identifier, attempt, due_at_ms, error, project_slug)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(issue_id) DO UPDATE SET
               identifier   = excluded.identifier,
               attempt      = excluded.attempt,
               due_at_ms    = excluded.due_at_ms,
               error        = excluded.error,
               project_slug = excluded.project_slug",
            params![
                r.issue_id,
                r.identifier,
                r.attempt,
                r.due_at_ms,
                r.error,
                r.project_slug,
            ],
        )?;
        Ok(())
    }

    fn delete_retry(&self, issue_id: &str) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM retry_queue WHERE issue_id = ?1",
            params![issue_id],
        )?;
        Ok(())
    }

    fn save_claim(
        &self,
        issue_id: &str,
        state: &str,
        project_slug: &str,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO claims (issue_id, state, claimed_at, project_slug)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(issue_id) DO UPDATE SET
               state        = excluded.state,
               claimed_at   = excluded.claimed_at,
               project_slug = excluded.project_slug",
            params![issue_id, state, now, project_slug],
        )?;
        Ok(())
    }

    fn delete_claim(&self, issue_id: &str) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute("DELETE FROM claims WHERE issue_id = ?1", params![issue_id])?;
        Ok(())
    }

    fn load_recovery(&self) -> Result<Recovery, StoreError> {
        let conn = self.lock();
        let mut rec = Recovery::default();
        {
            let mut stmt = conn.prepare(
                "SELECT issue_id, identifier, attempt, due_at_ms, error, project_slug \
                   FROM retry_queue ORDER BY due_at_ms",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(RetryRow {
                    issue_id: row.get(0)?,
                    identifier: row.get(1)?,
                    attempt: row.get(2)?,
                    due_at_ms: row.get(3)?,
                    error: row.get(4)?,
                    project_slug: row.get(5)?,
                })
            })?;
            for r in rows {
                rec.retries.push(r?);
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT issue_id, state, claimed_at, project_slug FROM claims ORDER BY claimed_at",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(ClaimRow {
                    issue_id: row.get(0)?,
                    state: row.get(1)?,
                    claimed_at: row.get(2)?,
                    project_slug: row.get(3)?,
                })
            })?;
            for c in rows {
                rec.claims.push(c?);
            }
        }
        Ok(rec)
    }

    fn mark_running_interrupted(&self) -> Result<i64, StoreError> {
        let now = now_rfc3339();
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE runs SET outcome = ?1, ended_at = ?2 WHERE outcome = ?3",
            params![OUTCOME_INTERRUPTED, now, OUTCOME_RUNNING],
        )?;
        Ok(n as i64)
    }

    fn save_totals(&self, t: Totals) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO totals (id, input_tokens, output_tokens, total_tokens, seconds_running)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
               input_tokens    = excluded.input_tokens,
               output_tokens   = excluded.output_tokens,
               total_tokens    = excluded.total_tokens,
               seconds_running = excluded.seconds_running",
            params![
                t.input_tokens,
                t.output_tokens,
                t.total_tokens,
                t.seconds_running,
            ],
        )?;
        Ok(())
    }

    fn load_totals(&self) -> Result<Totals, StoreError> {
        let conn = self.lock();
        let res = conn.query_row(
            "SELECT input_tokens, output_tokens, total_tokens, seconds_running FROM totals WHERE id = 1",
            [],
            |row| {
                Ok(Totals {
                    input_tokens: row.get(0)?,
                    output_tokens: row.get(1)?,
                    total_tokens: row.get(2)?,
                    seconds_running: row.get(3)?,
                })
            },
        );
        match res {
            Ok(t) => Ok(t),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Totals::default()), // no totals yet -> zero
            Err(e) => Err(e.into()),
        }
    }

    fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        let (where_sql, mut args) = run_filter_where(&f);
        let limit = effective_run_limit(f.limit);
        let offset = if f.offset < 0 { 0 } else { f.offset };
        let q = format!(
            "SELECT {RUN_COLS} FROM runs{where_sql} \
             ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?"
        );
        args.push(Value::Integer(limit));
        args.push(Value::Integer(offset));
        self.query_runs(&q, args)
    }

    fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        let (where_sql, mut args) = run_filter_where(&f);
        let limit = effective_run_limit(f.limit);
        let offset = if f.offset < 0 { 0 } else { f.offset };
        // ROW_NUMBER() over the per-issue partition keeps only each issue's newest matching run, so
        // LIMIT/OFFSET page over ISSUES. The partition key sends an unattributed run (empty
        // identifier) to a partition of its own — `run:<id>` can never collide with a real Linear
        // identifier — so those rows stay individual instead of collapsing into one synthetic row.
        // Because the kept row IS each partition's newest, ordering the survivors by started_at is
        // already "most recent activity first".
        let q = format!(
            "SELECT {RUN_COLS} FROM (
               SELECT {RUN_COLS}, ROW_NUMBER() OVER (
                        PARTITION BY CASE WHEN issue_identifier = '' THEN 'run:' || id
                                          ELSE 'issue:' || issue_identifier END
                        ORDER BY started_at DESC, id DESC
                      ) AS rn
                 FROM runs{where_sql}
             )
              WHERE rn = 1
              ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?"
        );
        args.push(Value::Integer(limit));
        args.push(Value::Integer(offset));
        self.query_runs(&q, args)
    }

    fn day_totals(&self, since: &str, now: &str) -> Result<DayTotals, StoreError> {
        // Per-run runtime mirrors the dashboard rule this replaces: an in-flight run counts its
        // elapsed time against `now`, a finished one counts ended_at - started_at. strftime yields
        // NULL for an unparseable/empty timestamp, and SUM skips NULLs, so a malformed row
        // contributes 0 seconds rather than poisoning the total (the client also scored it 0).
        // COUNT/SUM run over every matching row in the table — never over a page.
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN outcome = ?1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(
                      max(0, CAST(strftime('%s', CASE WHEN outcome = ?2 THEN ?3 ELSE ended_at END)
                                  AS INTEGER)
                           - CAST(strftime('%s', started_at) AS INTEGER))), 0)
               FROM runs
              WHERE started_at >= ?4",
        )?;
        let totals = stmt.query_row(
            params![OUTCOME_COMPLETED, OUTCOME_RUNNING, now, since],
            |row| {
                Ok(DayTotals {
                    runs: row.get(0)?,
                    completed: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    total_tokens: row.get(4)?,
                    seconds: row.get(5)?,
                })
            },
        )?;
        Ok(totals)
    }

    fn issue_history(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        let limit = effective_run_limit(limit);
        if project.is_empty() {
            let q = format!(
                "SELECT {RUN_COLS} FROM runs WHERE issue_identifier = ? \
                 ORDER BY started_at DESC, id DESC LIMIT ?"
            );
            self.query_runs(
                &q,
                vec![Value::Text(identifier.to_string()), Value::Integer(limit)],
            )
        } else {
            let q = format!(
                "SELECT {RUN_COLS} FROM runs WHERE issue_identifier = ? AND project_slug = ? \
                 ORDER BY started_at DESC, id DESC LIMIT ?"
            );
            self.query_runs(
                &q,
                vec![
                    Value::Text(identifier.to_string()),
                    Value::Text(project.to_string()),
                    Value::Integer(limit),
                ],
            )
        }
    }

    fn get_run(&self, run_id: i64) -> Result<Option<RunSummary>, StoreError> {
        let q = format!("SELECT {RUN_COLS} FROM runs WHERE id = ?");
        let mut runs = self.query_runs(&q, vec![Value::Integer(run_id)])?;
        if runs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(runs.remove(0)))
        }
    }

    fn run_events(&self, run_id: i64) -> Result<Vec<EventRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, at, kind, tool, text FROM events WHERE run_id = ?1 ORDER BY seq, id",
        )?;
        let rows = stmt.query_map([run_id], |row| {
            Ok(EventRow {
                seq: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                tool: row.get(3)?,
                text: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for e in rows {
            out.push(e?);
        }
        Ok(out)
    }

    fn search_events(&self, q: EventQuery) -> Result<Vec<EventHit>, StoreError> {
        let mut where_clauses: Vec<&str> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        if !q.text.is_empty() {
            // ESCAPE '\' makes escape_like's wildcard-escaping effective.
            where_clauses.push("e.text LIKE ? ESCAPE '\\'");
            args.push(Value::Text(format!("%{}%", escape_like(&q.text))));
        }
        if !q.issue.is_empty() {
            where_clauses.push("r.issue_identifier = ?");
            args.push(Value::Text(q.issue.clone()));
        }
        if !q.kind.is_empty() {
            where_clauses.push("e.kind = ?");
            args.push(Value::Text(q.kind.clone()));
        }
        let limit = if q.limit <= 0 {
            DEFAULT_EVENT_LIMIT
        } else {
            q.limit
        };
        let mut query = "SELECT e.run_id, r.issue_identifier, e.seq, e.at, e.kind, e.tool, e.text \
                           FROM events e JOIN runs r ON r.id = e.run_id"
            .to_string();
        if !where_clauses.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&where_clauses.join(" AND "));
        }
        query.push_str(" ORDER BY e.run_id DESC, e.seq DESC LIMIT ?");
        args.push(Value::Integer(limit));

        let conn = self.lock();
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(params_from_iter(args), |row| {
            Ok(EventHit {
                run_id: row.get(0)?,
                issue_identifier: row.get(1)?,
                seq: row.get(2)?,
                at: row.get(3)?,
                kind: row.get(4)?,
                tool: row.get(5)?,
                text: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for h in rows {
            out.push(h?);
        }
        Ok(out)
    }

    fn earliest_run_start(&self) -> Result<Option<String>, StoreError> {
        // Empty `started_at` is the column default, not a real instant, so it is excluded rather
        // than sorted to the front — a single defaulted row would otherwise claim a horizon of
        // "the beginning of time" and vouch for history this store never held.
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT started_at FROM runs WHERE started_at <> '' ORDER BY started_at ASC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError> {
        // started_at is RFC3339; substr(...,1,10) yields the YYYY-MM-DD bucket. sinceDays<=0 => all.
        let mut args: Vec<Value> = Vec::new();
        let mut q = "SELECT substr(started_at, 1, 10) AS day,
                            COUNT(*) AS runs,
                            SUM(CASE WHEN outcome = 'completed' THEN 1 ELSE 0 END) AS completed,
                            SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END) AS failed,
                            COALESCE(SUM(total_tokens), 0) AS total_tokens
                       FROM runs
                      WHERE started_at <> ''"
            .to_string();
        if since_days > 0 {
            q.push_str(" AND started_at >= ?");
            args.push(Value::Text(days_ago_rfc3339(since_days)));
        }
        if !project.is_empty() {
            q.push_str(" AND project_slug = ?");
            args.push(Value::Text(project.to_string()));
        }
        q.push_str(" GROUP BY day ORDER BY day");

        let conn = self.lock();
        let mut stmt = conn.prepare(&q)?;
        let rows = stmt.query_map(params_from_iter(args), |row| {
            Ok(DayRollup {
                date: row.get(0)?,
                runs: row.get(1)?,
                completed: row.get(2)?,
                failed: row.get(3)?,
                total_tokens: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for d in rows {
            out.push(d?);
        }
        Ok(out)
    }

    fn insert_run_message(
        &self,
        run_id: i64,
        body: &str,
        created_at_ms: i64,
    ) -> Result<i64, StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO run_messages (run_id, body, created_at_ms, status) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, body, created_at_ms, RUN_MESSAGE_SENT],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn mark_oldest_run_message_delivered(&self, run_id: i64, turn: i64) -> Result<(), StoreError> {
        let conn = self.lock();
        // The subquery picks the lowest-id "sent" row so delivery marking is FIFO. No-op when none.
        conn.execute(
            "UPDATE run_messages SET status = ?1, delivered_turn = ?2
               WHERE id = (SELECT id FROM run_messages WHERE run_id = ?3 AND status = ?4 ORDER BY id LIMIT 1)",
            params![RUN_MESSAGE_DELIVERED, turn, run_id, RUN_MESSAGE_SENT],
        )?;
        Ok(())
    }

    fn expire_run_messages(&self, run_id: i64) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE run_messages SET status = ?1 WHERE run_id = ?2 AND status = ?3",
            params![RUN_MESSAGE_EXPIRED, run_id, RUN_MESSAGE_SENT],
        )?;
        Ok(())
    }

    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, body, created_at_ms, status, delivered_turn \
               FROM run_messages WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |row| {
            Ok(RunMessage {
                id: row.get(0)?,
                run_id: row.get(1)?,
                body: row.get(2)?,
                created_at_ms: row.get(3)?,
                status: row.get(4)?,
                delivered_turn: row.get(5)?, // Option<i64> from the nullable INTEGER column
            })
        })?;
        let mut out = Vec::new();
        for m in rows {
            out.push(m?);
        }
        Ok(out)
    }

    fn save_review_watch(&self, w: ReviewWatchRow) -> Result<(), StoreError> {
        let conn = self.lock();
        // On INSERT the row is stored verbatim. On CONFLICT only the origin/status/open triple is
        // refreshed — the two SHA columns are deliberately absent from the DO UPDATE SET list, so
        // re-introducing a watched PR structurally cannot forget which SHA was dispatched (F-DUP)
        // or reviewed (F-SHA). They move only via mark_review_requested / mark_review_completed.
        conn.execute(
            "INSERT INTO rhapsody_review_watch
               (owner, repo, number, reviewer, author, introduced_by, requested_sha, last_reviewed_sha, status, open)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(owner, repo, number, reviewer) DO UPDATE SET
               author        = excluded.author,
               introduced_by = excluded.introduced_by,
               status        = excluded.status,
               open          = excluded.open",
            params![
                w.key.owner,
                w.key.repo,
                w.key.number,
                w.key.reviewer,
                w.author,
                w.introduced_by,
                w.requested_sha,
                w.last_reviewed_sha,
                w.status,
                w.open,
            ],
        )?;
        Ok(())
    }

    fn mark_review_requested(
        &self,
        key: &ReviewWatchKey,
        requested_sha: &str,
    ) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            &format!(
                "UPDATE rhapsody_review_watch SET requested_sha = ?5, status = ?6 \
                   WHERE {REVIEW_WATCH_KEY_WHERE}"
            ),
            params![
                key.owner,
                key.repo,
                key.number,
                key.reviewer,
                requested_sha,
                REVIEW_STATUS_IN_FLIGHT,
            ],
        )?;
        Ok(())
    }

    fn mark_review_completed(
        &self,
        key: &ReviewWatchKey,
        reviewed_sha: &str,
        status: &str,
    ) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            &format!(
                "UPDATE rhapsody_review_watch SET last_reviewed_sha = ?5, status = ?6 \
                   WHERE {REVIEW_WATCH_KEY_WHERE}"
            ),
            params![
                key.owner,
                key.repo,
                key.number,
                key.reviewer,
                reviewed_sha,
                status,
            ],
        )?;
        Ok(())
    }

    fn mark_review_truncated(&self, key: &ReviewWatchKey) -> Result<(), StoreError> {
        let conn = self.lock();
        // Only `status` moves: both SHA columns are absent from the SET list for the same reason
        // they are absent from save_review_watch's DO UPDATE, and here the point is sharper — a
        // truncated round read the requested head only partially, so advancing last_reviewed_sha
        // would record a partial review as a complete one.
        conn.execute(
            &format!("UPDATE rhapsody_review_watch SET status = ?5 WHERE {REVIEW_WATCH_KEY_WHERE}"),
            params![
                key.owner,
                key.repo,
                key.number,
                key.reviewer,
                REVIEW_STATUS_TRUNCATED,
            ],
        )?;
        Ok(())
    }

    fn drop_review_watch(&self, key: &ReviewWatchKey) -> Result<(), StoreError> {
        let conn = self.lock();
        // The row is kept, not deleted: both SHAs stay as the record of what was reviewed before
        // the PR merged or closed.
        conn.execute(
            &format!(
                "UPDATE rhapsody_review_watch SET status = ?5, open = 0 \
                   WHERE {REVIEW_WATCH_KEY_WHERE}"
            ),
            params![
                key.owner,
                key.repo,
                key.number,
                key.reviewer,
                REVIEW_STATUS_DROPPED,
            ],
        )?;
        Ok(())
    }

    fn get_review_watch(&self, key: &ReviewWatchKey) -> Result<Option<ReviewWatchRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REVIEW_WATCH_COLS} FROM rhapsody_review_watch \
               WHERE {REVIEW_WATCH_KEY_WHERE}"
        ))?;
        // The composite PRIMARY KEY makes this at most one row; absent is Ok(None), not an error.
        let mut rows = stmt.query_map(&review_watch_key_params(key)[..], map_review_watch)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    fn load_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REVIEW_WATCH_COLS} FROM rhapsody_review_watch \
               ORDER BY owner, repo, number, reviewer"
        ))?;
        let rows = stmt.query_map([], map_review_watch)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    fn prune(&self, retention_days: i64) -> Result<(), StoreError> {
        if retention_days <= 0 {
            return Ok(()); // 0 = keep forever
        }
        let cutoff = days_ago_rfc3339(retention_days);
        let mut conn = self.lock();

        // Collect the concrete per-run transcript files of the to-be-pruned runs BEFORE deleting
        // the rows (the rows are the only index of which files belong to a pruned run).
        let paths = prunable_paths(&conn, &cutoff);

        let tx = conn.transaction()?;
        // Delete events + operator messages of old runs first (FK-safe), then the runs themselves.
        // A run is "old" when it ended before the cutoff (a still-running run has ended_at == '').
        let old_runs =
            "SELECT id FROM runs WHERE ended_at IS NOT NULL AND ended_at <> '' AND ended_at < ?1";
        tx.execute(
            &format!("DELETE FROM events WHERE run_id IN ({old_runs})"),
            params![cutoff],
        )?;
        // run_messages has no FK to runs but is still per-run history (INF-250); prune it too.
        tx.execute(
            &format!("DELETE FROM run_messages WHERE run_id IN ({old_runs})"),
            params![cutoff],
        )?;
        tx.execute(
            "DELETE FROM runs WHERE ended_at IS NOT NULL AND ended_at <> '' AND ended_at < ?1",
            params![cutoff],
        )?;
        tx.commit()?;

        // Best-effort delete the on-disk transcripts of the pruned runs AFTER the rows are gone.
        // A vanished file is tolerated; we never delete a latest.jsonl alias.
        let mut rm_errs: Vec<String> = Vec::new();
        for p in paths {
            if p.is_empty() || p.ends_with("latest.jsonl") {
                continue; // defensive: never remove an alias even if one ever slipped in
            }
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                rm_errs.push(format!("prune transcript {p:?}: {e}"));
            }
        }
        if rm_errs.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Io(std::io::Error::other(rm_errs.join("; "))))
        }
    }

    fn close(&self) -> Result<(), StoreError> {
        // rusqlite closes the connection on Drop, so there is nothing to release here; Close exists
        // for Store-interface parity with Go's `Close() error` and always succeeds. (See PR body.)
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_store_path;
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
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
    ///
    /// A THIRD exclusion, `name NOT LIKE 'rhapsody\_%' ESCAPE '\'`, gates out the Rhapsody-only objects
    /// that the Go daemon cannot create and therefore can never appear in a recaptured golden
    /// (today: `rhapsody_review_watch`, STUDIO-711 — see this module's doc comment and the README
    /// "Divergences" entry). It excludes by NAME only, so it cannot hide drift in any of the 6
    /// ported tables: a Go table is never named `rhapsody_*`, and a new un-prefixed table still
    /// turns this golden red. `divergent_objects_are_gated_by_name_only` asserts that property and
    /// pins the excluded set to exactly the documented object.
    ///
    /// The `_` in the prefix is ESCAPEd: unescaped it is a LIKE single-character wildcard, which
    /// would silently widen the exclusion to any `rhapsody?*` name. Escaped, the SQL is exactly
    /// the `starts_with` rule the README states and `divergent_objects_are_gated_by_name_only`
    /// asserts, so the two can never drift apart.
    fn schema_dump(store: &Sqlite) -> String {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT sql FROM sqlite_master \
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
                   AND name NOT LIKE 'rhapsody\\_%' ESCAPE '\\' \
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
            .lock()
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
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(
            version, SCHEMA_VERSION,
            "a Go-written database must migrate forward to the current schema version and stop"
        );

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

    // ---------------------------------------------------------------------------------------------
    // S3: mirror of `sqlite_test.go` (test-by-test, in file order) + `noop_test.go`'s sibling. The
    // Go tests are the acceptance map for the ported behavior.
    // ---------------------------------------------------------------------------------------------

    /// Fresh in-memory store for a hermetic test (Go `openMem`).
    fn open_mem() -> Sqlite {
        Sqlite::open(StorePath::InMemory).expect("open in-memory")
    }

    /// Fresh file-backed store under a scratch dir (Go `openTemp`) so WAL behavior — and sharing
    /// one store across threads — can be exercised against a real on-disk database.
    fn open_temp() -> Sqlite {
        let dir = scratch_dir();
        Sqlite::open(StorePath::Disk(dir.join("symphony.db"))).expect("open temp")
    }

    /// Create a transcript fixture file (Go `writeFileForTest`).
    fn write_file_for_test(path: &Path) {
        std::fs::write(path, b"{}\n").expect("write transcript fixture");
    }

    /// Whether `path` exists on disk (Go `fileExistsForTest`).
    fn file_exists_for_test(path: &Path) -> bool {
        path.exists()
    }

    // Mirror TestOpenCreatesMissingParentDir: Open must MkdirAll a missing parent dir, else SQLite
    // returns CANTOPEN and the daemon silently loses persistence.
    #[test]
    fn open_creates_missing_parent_dir() {
        let dir = scratch_dir()
            .join("does")
            .join("not")
            .join("exist")
            .join("yet");
        let st = Sqlite::open(StorePath::Disk(dir.join("symphony.db")))
            .expect("Open must create the missing parent dir");
        st.close().expect("close");
        assert!(dir.exists(), "parent dir not created by Open");
    }

    // Mirror TestRunMessagesRoundTrip (INF-250): insert (status "sent"), FIFO delivery of the
    // oldest sent row with its turn, expiry of the rest at run end, and per-run isolation.
    #[test]
    fn run_messages_round_trip() {
        let s = open_mem();

        let id1 = s
            .insert_run_message(7, "check the branch name", 1000)
            .expect("insert 1");
        assert!(id1 > 0, "id1 = {id1}");
        let id2 = s.insert_run_message(7, "second", 2000).expect("insert 2");
        assert!(id2 > id1, "id2 = {id2}, id1 = {id1}");

        // FIFO: the oldest "sent" row (id1) gets the turn stamp.
        s.mark_oldest_run_message_delivered(7, 3)
            .expect("mark delivered");
        let msgs = s.list_run_messages(7).expect("list");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, id1);
        assert_eq!(msgs[0].body, "check the branch name");
        assert_eq!(msgs[0].status, RUN_MESSAGE_DELIVERED);
        assert_eq!(msgs[0].delivered_turn, Some(3));
        assert_eq!(msgs[0].created_at_ms, 1000);
        assert_eq!(msgs[1].status, RUN_MESSAGE_SENT);
        assert_eq!(msgs[1].delivered_turn, None);

        // Run end expires the remaining sent rows; the delivered row is untouched.
        s.expire_run_messages(7).expect("expire");
        let msgs = s.list_run_messages(7).expect("list");
        assert_eq!(msgs[0].status, RUN_MESSAGE_DELIVERED);
        assert_eq!(msgs[0].delivered_turn, Some(3));
        assert_eq!(msgs[1].status, RUN_MESSAGE_EXPIRED);

        // Marking delivered with no remaining sent rows is a no-op.
        s.mark_oldest_run_message_delivered(7, 9)
            .expect("mark no-op");
        let msgs = s.list_run_messages(7).expect("list");
        assert_eq!(msgs[0].delivered_turn, Some(3));
        assert_eq!(msgs[1].status, RUN_MESSAGE_EXPIRED);

        // Other runs are untouched.
        assert!(s.list_run_messages(8).expect("list 8").is_empty());
    }

    // Mirror TestWALEnabled: journal_mode=WAL is active on a file-backed handle.
    #[test]
    fn wal_enabled() {
        let st = open_temp();
        let mode: String = st
            .lock()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal", "journal_mode = {mode}");
    }

    // Mirror TestConcurrentReadDuringWrite: a reader thread queries while a writer thread streams
    // writes. The single-connection mutex (Go's SetMaxOpenConns(1)) serializes them; every write
    // lands and no read errors out.
    #[test]
    fn concurrent_read_during_write() {
        let st = open_temp();
        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-1".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("seed start run");

        const WRITES: i64 = 100;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..WRITES {
                    st.append_events(
                        id,
                        &[EventRow {
                            seq: i,
                            kind: "event".into(),
                            text: format!("e{i}"),
                            ..Default::default()
                        }],
                    )
                    .expect("append");
                }
            });
            scope.spawn(|| {
                for _ in 0..WRITES {
                    st.list_runs(RunFilter {
                        issue: "MT-1".into(),
                        ..Default::default()
                    })
                    .expect("read runs");
                    st.run_events(id).expect("read events");
                }
            });
        });

        let ev = st.run_events(id).expect("final events");
        assert_eq!(
            ev.len(),
            WRITES as usize,
            "concurrent reads must not have blocked writes"
        );
    }

    // Mirror TestOpenMigratesToSchemaVersion: a fresh store is at schemaVersion; re-opening a file
    // store is an idempotent no-op.
    #[test]
    fn open_migrates_to_schema_version() {
        let path = scratch_dir().join("symphony.db");
        let s1 = Sqlite::open(StorePath::Disk(path.clone())).expect("open file");
        s1.close().expect("close");
        let s2 = Sqlite::open(StorePath::Disk(path)).expect("re-open file"); // migrate is a no-op
        s2.close().expect("close");

        let st = open_mem();
        let v: i64 = st
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version");
        assert_eq!(v, SCHEMA_VERSION, "user_version = {v}");
    }

    // Mirror TestMigrateOutcomesV5 (INF-272): a v4 database carrying the OLD outcome strings is
    // rewritten by the v4->v5 step. succeeded->continued, handoff->completed, canceled->stopped,
    // stalled/timed_out->failed; failed/interrupted/running are left untouched.
    #[test]
    fn migrate_outcomes_v5() {
        let path = scratch_dir().join("symphony.db");
        // Build a v4 database by applying the first four migration steps directly, then stamping
        // user_version=4 so Open() runs ONLY the new v4->v5 (and v5->v6) steps.
        {
            let raw = Connection::open(&path).expect("raw open");
            for m in &MIGRATIONS[0..4] {
                raw.execute_batch(m).expect("apply migration");
            }
            raw.execute_batch("PRAGMA user_version = 4;")
                .expect("stamp v4");
            let seed = |id: i64, outcome: &str| {
                raw.execute(
                    "INSERT INTO runs (id, issue_identifier, outcome, started_at) VALUES (?1, ?2, ?3, ?4)",
                    params![id, "MIG", outcome, "2026-01-01T00:00:00Z"],
                )
                .expect("seed");
            };
            seed(1, "succeeded");
            seed(2, "handoff");
            seed(3, "canceled");
            seed(4, "stalled");
            seed(5, "timed_out");
            seed(6, "failed");
            seed(7, "interrupted");
            seed(8, "running");
        } // raw connection dropped/closed here

        // Reopen via Open(): the migrate() layer runs the v4->v5 step.
        let st = Sqlite::open(StorePath::Disk(path)).expect("reopen");
        // id -> EXPECTED post-migration outcome.
        let want: [(i64, &str); 8] = [
            (1, OUTCOME_CONTINUED),   // succeeded
            (2, OUTCOME_COMPLETED),   // handoff
            (3, OUTCOME_STOPPED),     // canceled
            (4, OUTCOME_FAILED),      // stalled
            (5, OUTCOME_FAILED),      // timed_out
            (6, OUTCOME_FAILED),      // failed (untouched)
            (7, OUTCOME_INTERRUPTED), // interrupted (untouched)
            (8, OUTCOME_RUNNING),     // running (untouched)
        ];
        let conn = st.lock();
        for (id, exp) in want {
            let got: String = conn
                .query_row(
                    "SELECT outcome FROM runs WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .expect("query outcome");
            assert_eq!(got, exp, "run {id}: outcome = {got}, want {exp}");
        }
    }

    // Mirror TestRunLifecycleRoundTrip: StartRun -> UpdateRunProgress -> EndRun -> ListRuns keeps
    // every field's wire shape.
    #[test]
    fn run_lifecycle_round_trip() {
        let st = open_mem();
        let id = st
            .start_run(RunStart {
                issue_id: "ID-1".into(),
                issue_identifier: "MT-1".into(),
                title: "do thing".into(),
                attempt: 2,
                started_at: "2026-01-01T00:00:00Z".into(),
                transcript_path: "/logs/MT-1/latest.jsonl".into(),
                project_slug: "alpha".into(),
                repo: "git@x/alpha.git".into(),
                ..Default::default()
            })
            .expect("start run");
        assert!(id > 0, "runID = {id}");
        st.update_run_progress(
            id,
            RunProgress {
                turns: 3,
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
                ..Default::default()
            },
        )
        .expect("progress");
        st.end_run(
            id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: "2026-01-01T00:05:00Z".into(),
                turns: 4,
                input_tokens: 11,
                output_tokens: 22,
                total_tokens: 33,
                ..Default::default()
            },
        )
        .expect("end run");

        let runs = st.list_runs(RunFilter::default()).expect("list");
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.id, id);
        assert_eq!(r.issue_id, "ID-1");
        assert_eq!(r.issue_identifier, "MT-1");
        assert_eq!(r.title, "do thing");
        assert_eq!(r.attempt, 2);
        assert_eq!(r.outcome, OUTCOME_COMPLETED);
        assert_eq!(r.turns, 4);
        assert_eq!(r.input_tokens, 11);
        assert_eq!(r.output_tokens, 22);
        assert_eq!(r.total_tokens, 33);
        assert_eq!(r.transcript_path, "/logs/MT-1/latest.jsonl");
        assert_eq!(r.project_slug, "alpha");
        assert_eq!(r.repo, "git@x/alpha.git");
        assert_eq!(r.ended_at, "2026-01-01T00:05:00Z");
    }

    // Mirror TestUsageEstimatedRoundTrip (INF-208): the floored-estimate flag threads through
    // progress and end into the history projection, and a clean end clears it.
    #[test]
    fn usage_estimated_round_trip() {
        let st = open_mem();
        // A run started fresh is NOT estimated until a floor write marks it.
        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-9".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("start run");
        let r = st.get_run(id).expect("get run").expect("found");
        assert!(!r.usage_estimated, "fresh run must not be estimated");

        // A progress write can flag the live row as estimated (floored).
        st.update_run_progress(
            id,
            RunProgress {
                turns: 1,
                total_tokens: 100,
                usage_estimated: true,
                ..Default::default()
            },
        )
        .expect("progress");
        let r = st.get_run(id).expect("get run").expect("found");
        assert!(
            r.usage_estimated && r.total_tokens == 100,
            "not persisted: {r:?}"
        );

        // EndRun carries the final estimated flag into the history projection.
        st.end_run(
            id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                total_tokens: 412803,
                input_tokens: 139000,
                output_tokens: 8000,
                usage_estimated: true,
                ..Default::default()
            },
        )
        .expect("end run");
        let runs = st.list_runs(RunFilter::default()).expect("list");
        assert_eq!(runs.len(), 1);
        assert!(runs[0].usage_estimated && runs[0].total_tokens == 412803);

        // A clean (authoritative) end clears it back to false.
        let id2 = st
            .start_run(RunStart {
                issue_identifier: "MT-10".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("start run 2");
        st.end_run(
            id2,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                total_tokens: 50,
                usage_estimated: false,
                ..Default::default()
            },
        )
        .expect("end run 2");
        let r2 = st.get_run(id2).expect("get run 2").expect("found");
        assert!(
            !r2.usage_estimated,
            "authoritative run must not be estimated"
        );
    }

    // Mirror TestStartRunFillsStartedAtWhenEmpty: an empty StartedAt is auto-filled with now.
    #[test]
    fn start_run_fills_started_at_when_empty() {
        let st = open_mem();
        st.start_run(RunStart {
            issue_identifier: "MT-2".into(),
            ..Default::default()
        })
        .expect("start run");
        let runs = st.list_runs(RunFilter::default()).expect("list");
        assert_eq!(runs.len(), 1);
        assert!(
            !runs[0].started_at.is_empty(),
            "started_at should be auto-filled"
        );
    }

    // Mirror TestListRunsFiltersAndPaging: issue/outcome/since/project filters, DESC ordering,
    // and limit/offset paging.
    #[test]
    fn list_runs_filters_and_paging() {
        let st = open_mem();
        let seed = |ident: &str, outcome: &str, proj: &str, started: &str| {
            let id = st
                .start_run(RunStart {
                    issue_identifier: ident.into(),
                    started_at: started.into(),
                    project_slug: proj.into(),
                    ..Default::default()
                })
                .expect("start");
            st.end_run(
                id,
                RunEnd {
                    outcome: outcome.into(),
                    ended_at: started.into(),
                    ..Default::default()
                },
            )
            .expect("end");
        };
        seed("MT-1", OUTCOME_COMPLETED, "alpha", "2026-01-01T00:00:00Z");
        seed("MT-2", OUTCOME_FAILED, "beta", "2026-01-02T00:00:00Z");
        seed("MT-1", OUTCOME_COMPLETED, "alpha", "2026-01-03T00:00:00Z");

        // Issue filter.
        let got = st
            .list_runs(RunFilter {
                issue: "MT-1".into(),
                ..Default::default()
            })
            .expect("issue");
        assert_eq!(got.len(), 2, "issue filter");
        // ORDER BY started_at DESC -> newest first.
        assert_eq!(got[0].started_at, "2026-01-03T00:00:00Z", "order");
        // Outcome filter.
        let got = st
            .list_runs(RunFilter {
                outcome: OUTCOME_FAILED.into(),
                ..Default::default()
            })
            .expect("outcome");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].issue_identifier, "MT-2");
        // Since filter.
        let got = st
            .list_runs(RunFilter {
                since: "2026-01-02T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("since");
        assert_eq!(got.len(), 2, "since filter");
        // Project filter.
        let got = st
            .list_runs(RunFilter {
                project: "beta".into(),
                ..Default::default()
            })
            .expect("project");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].issue_identifier, "MT-2");
        // Paging: limit 1, offset 0 vs offset 1.
        let page0 = st
            .list_runs(RunFilter {
                limit: 1,
                offset: 0,
                ..Default::default()
            })
            .expect("page0");
        let page1 = st
            .list_runs(RunFilter {
                limit: 1,
                offset: 1,
                ..Default::default()
            })
            .expect("page1");
        assert_eq!(page0.len(), 1);
        assert_eq!(page1.len(), 1);
        assert_ne!(
            page0[0].id, page1[0].id,
            "paging returned the same row twice"
        );
    }

    // TRA-320 — the paging default is a shared, observable rule, not a private `<= 0` branch
    // duplicated at every call site: the HTTP layer resolves it to report `next_offset` honestly.
    #[test]
    fn effective_run_limit_resolves_the_default() {
        assert_eq!(effective_run_limit(0), DEFAULT_RUN_LIMIT, "0 => default");
        assert_eq!(
            effective_run_limit(-7),
            DEFAULT_RUN_LIMIT,
            "negative => default"
        );
        assert_eq!(effective_run_limit(1), 1, "positive => verbatim");
        assert_eq!(effective_run_limit(500), 500, "positive => verbatim");
    }

    // TRA-320 Defect 3, mirroring the observed incident: TRA-309 failed 90 times in a clone-retry
    // loop and reduced a 10-issue jobs list to 3 rows. Paging by ISSUE must return one row per
    // issue regardless of how many runs each produced.
    #[test]
    fn list_issue_runs_pages_by_issue_not_by_run() {
        let st = open_mem();
        let seed = |ident: &str, started: &str, outcome: &str| {
            let id = st
                .start_run(RunStart {
                    issue_identifier: ident.into(),
                    started_at: started.into(),
                    ..Default::default()
                })
                .expect("start");
            st.end_run(
                id,
                RunEnd {
                    outcome: outcome.into(),
                    ended_at: started.into(),
                    ..Default::default()
                },
            )
            .expect("end");
            id
        };
        // Nine quiet issues, one run each, earliest.
        for i in 0..9 {
            seed(
                &format!("TRA-4{i:02}"),
                &format!("2026-08-01T01:{i:02}:00Z"),
                OUTCOME_COMPLETED,
            );
        }
        // The noisy issue: 90 failures in a retry loop, all NEWER — exactly the observed incident.
        let mut newest_noisy = 0;
        for i in 0..90 {
            newest_noisy = seed(
                "TRA-309",
                &format!("2026-08-01T{:02}:{:02}:00Z", 2 + i / 60, i % 60),
                OUTCOME_FAILED,
            );
        }

        // A run-paged fetch is exactly the broken behavior: the first default page is all TRA-309,
        // and the nine other issues are unrendered.
        let by_run = st.list_runs(RunFilter::default()).expect("list_runs");
        assert_eq!(by_run.len(), DEFAULT_RUN_LIMIT as usize);
        let distinct_by_run: std::collections::HashSet<_> =
            by_run.iter().map(|r| r.issue_identifier.as_str()).collect();
        assert_eq!(
            distinct_by_run.len(),
            1,
            "precondition: the run-paged page is entirely the noisy issue"
        );

        // Issue-paged: 10 rows, one per issue, on the FIRST page.
        let by_issue = st
            .list_issue_runs(RunFilter::default())
            .expect("list_issue_runs");
        assert_eq!(by_issue.len(), 10, "one row per issue on the first page");
        let distinct: std::collections::HashSet<_> = by_issue
            .iter()
            .map(|r| r.issue_identifier.as_str())
            .collect();
        assert_eq!(distinct.len(), 10, "every row is a different issue");
        // The kept row is each issue's NEWEST run, and most-recent activity sorts first.
        assert_eq!(
            by_issue[0].issue_identifier, "TRA-309",
            "most recent activity first"
        );
        assert_eq!(
            by_issue[0].id, newest_noisy,
            "the newest run represents the issue"
        );
    }

    // TRA-320 — issue paging counts ISSUES, and unattributed runs (empty identifier) are never
    // collapsed into one synthetic row.
    #[test]
    fn list_issue_runs_paging_and_unattributed_rows() {
        let st = open_mem();
        let seed = |ident: &str, started: &str| {
            st.start_run(RunStart {
                issue_identifier: ident.into(),
                started_at: started.into(),
                ..Default::default()
            })
            .expect("start")
        };
        for i in 0..3 {
            seed("MT-1", &format!("2026-01-01T00:0{i}:00Z"));
        }
        seed("MT-2", "2026-01-01T01:00:00Z");
        // Two unattributed runs: distinct rows, not one merged "" group.
        seed("", "2026-01-01T02:00:00Z");
        seed("", "2026-01-01T03:00:00Z");

        let all = st.list_issue_runs(RunFilter::default()).expect("all");
        assert_eq!(all.len(), 4, "MT-1 + MT-2 + two unattributed rows");
        assert_eq!(
            all.iter().filter(|r| r.issue_identifier.is_empty()).count(),
            2
        );

        let page0 = st
            .list_issue_runs(RunFilter {
                limit: 2,
                offset: 0,
                ..Default::default()
            })
            .expect("page0");
        let page1 = st
            .list_issue_runs(RunFilter {
                limit: 2,
                offset: 2,
                ..Default::default()
            })
            .expect("page1");
        assert_eq!(page0.len(), 2);
        assert_eq!(page1.len(), 2);
        let ids: std::collections::HashSet<_> =
            page0.iter().chain(page1.iter()).map(|r| r.id).collect();
        assert_eq!(ids.len(), 4, "the two pages don't overlap");

        // Filters apply before grouping, exactly as they do for the run-paged listing.
        let scoped = st
            .list_issue_runs(RunFilter {
                issue: "MT-1".into(),
                ..Default::default()
            })
            .expect("scoped");
        assert_eq!(scoped.len(), 1, "one row for the one matching issue");
        assert_eq!(
            scoped[0].started_at, "2026-01-01T00:02:00Z",
            "its newest run"
        );
    }

    // TRA-320 Defect 2: the day totals are a whole-store SUM, not a fold over a page — with more
    // runs than one page they must still match a direct aggregate.
    #[test]
    fn day_totals_aggregate_the_whole_store_not_a_page() {
        let st = open_mem();
        // 120 finished runs today (> 2 default pages), each 60s and 1000 total tokens.
        for i in 0..120 {
            let id = st
                .start_run(RunStart {
                    issue_identifier: format!("MT-{i}"),
                    started_at: format!("2026-08-01T{:02}:{:02}:00Z", i / 60, i % 60),
                    ..Default::default()
                })
                .expect("start");
            st.end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: format!("2026-08-01T{:02}:{:02}:00Z", (i + 1) / 60, (i + 1) % 60),
                    input_tokens: 10,
                    output_tokens: 20,
                    total_tokens: 1000,
                    ..Default::default()
                },
            )
            .expect("end");
        }
        // One run BEFORE the window — must not be counted.
        let old = st
            .start_run(RunStart {
                issue_identifier: "MT-old".into(),
                started_at: "2026-07-31T23:00:00Z".into(),
                ..Default::default()
            })
            .expect("start old");
        st.end_run(
            old,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: "2026-07-31T23:30:00Z".into(),
                total_tokens: 999_999,
                ..Default::default()
            },
        )
        .expect("end old");

        let got = st
            .day_totals("2026-08-01T00:00:00Z", "2026-08-01T12:00:00Z")
            .expect("day_totals");
        assert_eq!(got.runs, 120, "every run in the window, not one page");
        assert_eq!(got.completed, 120);
        assert_eq!(got.input_tokens, 120 * 10);
        assert_eq!(got.output_tokens, 120 * 20);
        assert_eq!(got.total_tokens, 120 * 1000);
        assert_eq!(got.seconds, 120 * 60);

        // Cross-check against a direct SQL aggregate over the same window.
        let conn = st.lock();
        let (runs, tokens): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM runs \
                   WHERE started_at >= '2026-08-01T00:00:00Z'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("direct sum");
        assert_eq!(
            (got.runs, got.total_tokens),
            (runs, tokens),
            "matches a direct SUM"
        );
    }

    // TRA-320 — an in-flight run contributes elapsed-so-far (measured against `now`), counted once;
    // a row with an unparseable/absent end contributes 0 rather than poisoning the sum.
    #[test]
    fn day_totals_include_in_flight_elapsed_once() {
        let st = open_mem();
        let running = st
            .start_run(RunStart {
                issue_identifier: "MT-live".into(),
                started_at: "2026-08-01T10:00:00Z".into(),
                ..Default::default()
            })
            .expect("start running");
        st.update_run_progress(
            running,
            RunProgress {
                turns: 3,
                input_tokens: 5,
                output_tokens: 7,
                total_tokens: 100,
                ..Default::default()
            },
        )
        .expect("progress");
        let done = st
            .start_run(RunStart {
                issue_identifier: "MT-done".into(),
                started_at: "2026-08-01T09:00:00Z".into(),
                ..Default::default()
            })
            .expect("start done");
        st.end_run(
            done,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: "2026-08-01T09:00:30Z".into(),
                total_tokens: 50,
                ..Default::default()
            },
        )
        .expect("end done");

        let got = st
            .day_totals("2026-08-01T00:00:00Z", "2026-08-01T10:05:00Z")
            .expect("day_totals");
        assert_eq!(got.runs, 2, "the live row is counted exactly once");
        assert_eq!(got.completed, 1);
        assert_eq!(got.total_tokens, 150, "live progress + finished total");
        assert_eq!(
            got.seconds,
            5 * 60 + 30,
            "live elapsed (5m) + finished span (30s)"
        );

        // A `now` BEFORE the live run started clamps ITS elapsed to 0 (never negative), leaving
        // only the finished run's 30s span.
        let clamped = st
            .day_totals("2026-08-01T00:00:00Z", "2026-08-01T08:00:00Z")
            .expect("clamped");
        assert_eq!(
            clamped.seconds, 30,
            "live elapsed clamped at 0, finished span kept"
        );
    }

    // TRA-320 — an empty window matches nothing and yields all-zero totals (not an error), so a
    // fresh install renders "0" rather than failing the header.
    #[test]
    fn day_totals_empty_window_is_zero() {
        let st = open_mem();
        let got = st
            .day_totals("2999-01-01T00:00:00Z", "2999-01-01T01:00:00Z")
            .expect("day_totals");
        assert_eq!(got, DayTotals::default());
    }

    // Mirror TestIssueHistory: per-issue history with an optional project scope.
    #[test]
    fn issue_history() {
        let st = open_mem();
        let a = st
            .start_run(RunStart {
                issue_identifier: "MT-9".into(),
                project_slug: "alpha".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("start a");
        st.end_run(
            a,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ..Default::default()
            },
        )
        .expect("end a");
        let b = st
            .start_run(RunStart {
                issue_identifier: "MT-9".into(),
                project_slug: "beta".into(),
                started_at: "2026-01-02T00:00:00Z".into(),
                ..Default::default()
            })
            .expect("start b");
        st.end_run(
            b,
            RunEnd {
                outcome: OUTCOME_FAILED.into(),
                ..Default::default()
            },
        )
        .expect("end b");

        let all = st.issue_history("MT-9", "", 0).expect("history all");
        assert_eq!(all.len(), 2, "history (no project)");
        let alpha = st.issue_history("MT-9", "alpha", 0).expect("history alpha");
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].project_slug, "alpha");
    }

    // Mirror TestAppendEventsAndRunEvents: ordered append + read-back, and an empty batch no-op.
    #[test]
    fn append_events_and_run_events() {
        let st = open_mem();
        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-3".into(),
                ..Default::default()
            })
            .expect("start");
        let rows = [
            EventRow {
                seq: 1,
                at: "t1".into(),
                kind: "event".into(),
                text: "session started".into(),
                ..Default::default()
            },
            EventRow {
                seq: 2,
                at: "t2".into(),
                kind: "text".into(),
                text: "hello world".into(),
                ..Default::default()
            },
            EventRow {
                seq: 3,
                at: "t3".into(),
                kind: "event".into(),
                text: "turn completed".into(),
                ..Default::default()
            },
        ];
        st.append_events(id, &rows).expect("append");
        let got = st.run_events(id).expect("run events");
        assert_eq!(got.len(), 3);
        for (i, e) in got.iter().enumerate() {
            assert_eq!(e.seq, (i + 1) as i64, "seq order broken at {i}");
        }
        // Empty batch is a no-op.
        st.append_events(id, &[]).expect("empty append");
    }

    // Mirror TestSearchEvents: text LIKE (with literal-% escaping via escapeLike + ESCAPE), issue
    // filter, and kind filter.
    #[test]
    fn search_events() {
        let st = open_mem();
        let id1 = st
            .start_run(RunStart {
                issue_identifier: "MT-1".into(),
                ..Default::default()
            })
            .expect("start 1");
        let id2 = st
            .start_run(RunStart {
                issue_identifier: "MT-2".into(),
                ..Default::default()
            })
            .expect("start 2");
        st.append_events(
            id1,
            &[
                EventRow {
                    seq: 1,
                    kind: "text".into(),
                    text: "the cat sat".into(),
                    ..Default::default()
                },
                EventRow {
                    seq: 2,
                    kind: "event".into(),
                    text: "turn completed".into(),
                    ..Default::default()
                },
            ],
        )
        .expect("append 1");
        st.append_events(
            id2,
            &[EventRow {
                seq: 1,
                kind: "text".into(),
                text: "50% off literal".into(),
                ..Default::default()
            }],
        )
        .expect("append 2");

        let hits = st
            .search_events(EventQuery {
                text: "cat".into(),
                ..Default::default()
            })
            .expect("text search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].issue_identifier, "MT-1");
        // Literal % must be escaped so it does not act as a wildcard.
        let hits = st
            .search_events(EventQuery {
                text: "50% off".into(),
                ..Default::default()
            })
            .expect("escaped %");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].issue_identifier, "MT-2");
        // A bare % must NOT match arbitrary rows beyond those literally containing '%'.
        let hits = st
            .search_events(EventQuery {
                text: "%".into(),
                ..Default::default()
            })
            .expect("literal %");
        assert_eq!(hits.len(), 1, "literal % should match only the one row");
        // Issue filter.
        let hits = st
            .search_events(EventQuery {
                issue: "MT-1".into(),
                ..Default::default()
            })
            .expect("issue filter");
        assert_eq!(hits.len(), 2);
        // Kind filter.
        let hits = st
            .search_events(EventQuery {
                kind: "event".into(),
                ..Default::default()
            })
            .expect("kind filter");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "event");
    }

    // Mirror TestMetricsRollup: per-day UTC buckets with completed/failed counts, token sums, and
    // a project filter. (continued segments bucket as neither completed nor failed.)
    #[test]
    fn metrics_rollup() {
        let st = open_mem();
        let seed = |outcome: &str, proj: &str, started: &str, tokens: i64| {
            let id = st
                .start_run(RunStart {
                    issue_identifier: "MT-x".into(),
                    started_at: started.into(),
                    project_slug: proj.into(),
                    ..Default::default()
                })
                .expect("start");
            st.end_run(
                id,
                RunEnd {
                    outcome: outcome.into(),
                    total_tokens: tokens,
                    ended_at: started.into(),
                    ..Default::default()
                },
            )
            .expect("end");
        };
        // Day 1: 1 completed, 1 failed.
        seed(OUTCOME_COMPLETED, "alpha", "2026-01-01T01:00:00Z", 100);
        seed(OUTCOME_FAILED, "alpha", "2026-01-01T02:00:00Z", 50);
        // Day 2: 2 failed, 1 completed.
        seed(OUTCOME_FAILED, "alpha", "2026-01-02T01:00:00Z", 10);
        seed(OUTCOME_FAILED, "beta", "2026-01-02T02:00:00Z", 20);
        seed(OUTCOME_COMPLETED, "alpha", "2026-01-02T03:00:00Z", 5);

        let all = st.metrics(0, "").expect("metrics");
        assert_eq!(all.len(), 2, "days");
        let (d1, d2) = (&all[0], &all[1]);
        assert_eq!(
            (
                d1.date.as_str(),
                d1.runs,
                d1.completed,
                d1.failed,
                d1.total_tokens
            ),
            ("2026-01-01", 2, 1, 1, 150),
            "day1 rollup: {d1:?}"
        );
        assert_eq!(
            (
                d2.date.as_str(),
                d2.runs,
                d2.completed,
                d2.failed,
                d2.total_tokens
            ),
            ("2026-01-02", 3, 1, 2, 35),
            "day2 rollup: {d2:?}"
        );
        // Project filter.
        let beta = st.metrics(0, "beta").expect("beta metrics");
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].failed, 1);
    }

    // Mirror TestPrune: retentionDays 0 keeps everything; 30 removes only the OLD ended run and its
    // events + operator messages, leaving the recent and still-running rows intact.
    /// The evidence horizon (STUDIO-672): the oldest run this store still holds, `None` when it
    /// holds none, and moving forward as `prune` deletes the old ones. A caller that would act on
    /// an absence bounds itself by this, so "no rows" must never read as "the beginning of time".
    #[test]
    fn earliest_run_start_reports_the_stores_coverage() {
        let st = open_mem();
        assert_eq!(
            st.earliest_run_start().expect("empty store"),
            None,
            "a store with no runs vouches for no instant at all"
        );

        let old = days_ago_rfc3339(40);
        let recent = days_ago_rfc3339(1);
        // Inserted newest-first, so the answer cannot come from insertion order.
        for at in [&recent, &old] {
            st.start_run(RunStart {
                issue_identifier: "MT-1".into(),
                started_at: at.clone(),
                ..Default::default()
            })
            .expect("start");
        }
        // A run with the column default is not an instant and must not claim the horizon.
        st.start_run(RunStart {
            issue_identifier: "MT-2".into(),
            started_at: String::new(),
            ..Default::default()
        })
        .expect("start defaulted");
        assert_eq!(st.earliest_run_start().expect("populated"), Some(old));
    }

    #[test]
    fn prune() {
        let st = open_mem();
        let old = days_ago_rfc3339(40);
        let recent = days_ago_rfc3339(1);

        let old_id = st
            .start_run(RunStart {
                issue_identifier: "OLD".into(),
                started_at: old.clone(),
                ..Default::default()
            })
            .expect("start OLD");
        st.append_events(
            old_id,
            &[EventRow {
                seq: 1,
                text: "old event".into(),
                ..Default::default()
            }],
        )
        .expect("append OLD");
        st.insert_run_message(old_id, "old operator message", 1000)
            .expect("msg OLD");
        st.end_run(
            old_id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: old.clone(),
                ..Default::default()
            },
        )
        .expect("end OLD");

        let recent_id = st
            .start_run(RunStart {
                issue_identifier: "NEW".into(),
                started_at: recent.clone(),
                ..Default::default()
            })
            .expect("start NEW");
        st.insert_run_message(recent_id, "recent operator message", 2000)
            .expect("msg NEW");
        st.end_run(
            recent_id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: recent,
                ..Default::default()
            },
        )
        .expect("end NEW");

        st.start_run(RunStart {
            issue_identifier: "LIVE".into(),
            started_at: old,
            ..Default::default()
        })
        .expect("start LIVE"); // no EndRun -> ended_at ''

        // retentionDays 0 = keep forever.
        st.prune(0).expect("prune 0");
        assert_eq!(
            st.list_runs(RunFilter::default()).expect("list").len(),
            3,
            "prune(0) deleted rows"
        );

        // retentionDays 30: only the OLD ended run + its children are removed.
        st.prune(30).expect("prune 30");
        let runs = st.list_runs(RunFilter::default()).expect("list");
        assert_eq!(runs.len(), 2, "prune(30): recent + running kept");
        assert!(
            !runs.iter().any(|r| r.issue_identifier == "OLD"),
            "old ended run should have been pruned"
        );
        assert!(
            st.run_events(old_id).expect("events").is_empty(),
            "old events should be pruned"
        );
        // The old run's operator messages go with it; the recent run's survive.
        assert!(
            st.list_run_messages(old_id).expect("msgs").is_empty(),
            "old run_messages should be pruned"
        );
        assert_eq!(
            st.list_run_messages(recent_id).expect("msgs").len(),
            1,
            "recent run_messages should be kept"
        );
        // The still-running row (empty ended_at) is never pruned.
        assert_eq!(
            st.list_runs(RunFilter {
                issue: "LIVE".into(),
                ..Default::default()
            })
            .expect("list LIVE")
            .len(),
            1,
            "running row must never be pruned"
        );
    }

    // Mirror TestGetRun: a present row returns Some(row); a missing id returns None (no error).
    #[test]
    fn get_run() {
        let st = open_mem();
        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-1".into(),
                transcript_path: "/logs/MT-1/20260101-1.jsonl".into(),
                ..Default::default()
            })
            .expect("start");

        let r = st.get_run(id).expect("get run").expect("found");
        assert_eq!(r.id, id);
        assert_eq!(r.issue_identifier, "MT-1");
        assert_eq!(r.transcript_path, "/logs/MT-1/20260101-1.jsonl");

        assert!(st.get_run(99999).expect("get missing").is_none());
    }

    // Mirror TestProgressAndEndRunRecordConcreteTranscriptPath: a non-empty TranscriptPath on
    // progress/end overwrites the column; an empty one leaves it unchanged (COALESCE(NULLIF(...))).
    #[test]
    fn progress_and_end_run_record_concrete_transcript_path() {
        let st = open_mem();
        let id = st
            .start_run(RunStart {
                issue_identifier: "MT-1".into(),
                ..Default::default()
            })
            .expect("start"); // empty at dispatch

        // Progress with a concrete path stamps the column.
        st.update_run_progress(
            id,
            RunProgress {
                turns: 1,
                transcript_path: "/logs/MT-1/run-1.jsonl".into(),
                ..Default::default()
            },
        )
        .expect("progress");
        assert_eq!(
            st.get_run(id).expect("get").expect("found").transcript_path,
            "/logs/MT-1/run-1.jsonl",
            "after progress: want recorded"
        );
        // A later progress with an EMPTY path must NOT wipe the recorded value.
        st.update_run_progress(
            id,
            RunProgress {
                turns: 2,
                ..Default::default()
            },
        )
        .expect("progress 2");
        assert_eq!(
            st.get_run(id).expect("get").expect("found").transcript_path,
            "/logs/MT-1/run-1.jsonl",
            "after empty progress: want unchanged"
        );
        // EndRun with a path overwrites again.
        st.end_run(
            id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                transcript_path: "/logs/MT-1/run-final.jsonl".into(),
                ..Default::default()
            },
        )
        .expect("end");
        assert_eq!(
            st.get_run(id).expect("get").expect("found").transcript_path,
            "/logs/MT-1/run-final.jsonl",
            "after end: want overwritten"
        );
    }

    // Mirror TestPruneDeletesTranscriptFiles: pruning an old run deletes its on-disk transcript
    // too, while a recent run's file is kept and a missing file is tolerated (best-effort).
    #[test]
    fn prune_deletes_transcript_files() {
        let st = open_mem();
        let dir = scratch_dir();
        let old = days_ago_rfc3339(40);
        let recent = days_ago_rfc3339(1);

        let mkfile = |name: &str| -> String {
            let p = dir.join(name);
            write_file_for_test(&p);
            p.to_string_lossy().into_owned()
        };
        let old_path = mkfile("old.jsonl");
        let recent_path = mkfile("recent.jsonl");
        let gone_path = dir
            .join("already-gone.jsonl")
            .to_string_lossy()
            .into_owned(); // never created

        let old_id = st
            .start_run(RunStart {
                issue_identifier: "OLD".into(),
                started_at: old.clone(),
                transcript_path: old_path.clone(),
                ..Default::default()
            })
            .expect("start OLD");
        st.end_run(
            old_id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: old.clone(),
                ..Default::default()
            },
        )
        .expect("end OLD");
        let gone_id = st
            .start_run(RunStart {
                issue_identifier: "GONE".into(),
                started_at: old.clone(),
                transcript_path: gone_path,
                ..Default::default()
            })
            .expect("start GONE");
        st.end_run(
            gone_id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: old,
                ..Default::default()
            },
        )
        .expect("end GONE");
        let recent_id = st
            .start_run(RunStart {
                issue_identifier: "NEW".into(),
                started_at: recent.clone(),
                transcript_path: recent_path.clone(),
                ..Default::default()
            })
            .expect("start NEW");
        st.end_run(
            recent_id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ended_at: recent,
                ..Default::default()
            },
        )
        .expect("end NEW");

        st.prune(30).expect("prune"); // gone_path missing must not surface as an error
        assert!(
            !file_exists_for_test(Path::new(&old_path)),
            "old run's transcript file should have been deleted"
        );
        assert!(
            file_exists_for_test(Path::new(&recent_path)),
            "recent run's transcript file must be kept"
        );
    }

    // Mirror TestRecoveryPrimitives: SaveRetry/SaveClaim upserts, LoadRecovery ordering, and
    // DeleteRetry/DeleteClaim.
    #[test]
    fn recovery_primitives() {
        let st = open_mem();

        // SaveRetry / LoadRecovery ORDER BY due_at_ms.
        st.save_retry(RetryRow {
            issue_id: "MT-2".into(),
            identifier: "MT-2".into(),
            attempt: 1,
            due_at_ms: 2000,
            error: "later".into(),
            project_slug: "beta".into(),
        })
        .expect("save MT-2");
        st.save_retry(RetryRow {
            issue_id: "MT-1".into(),
            identifier: "MT-1".into(),
            attempt: 3,
            due_at_ms: 1000,
            error: "sooner".into(),
            project_slug: "alpha".into(),
        })
        .expect("save MT-1");
        // Upsert: re-save MT-1 with a new attempt.
        st.save_retry(RetryRow {
            issue_id: "MT-1".into(),
            identifier: "MT-1".into(),
            attempt: 4,
            due_at_ms: 1000,
            error: "sooner".into(),
            project_slug: "alpha".into(),
        })
        .expect("resave MT-1");

        st.save_claim("MT-1", CLAIM_RUNNING, "alpha")
            .expect("claim MT-1");
        st.save_claim("MT-2", CLAIM_RETRY_QUEUED, "beta")
            .expect("claim MT-2");
        // Upsert claim state transition.
        st.save_claim("MT-1", CLAIM_RETRY_QUEUED, "alpha")
            .expect("reclaim MT-1");

        let rec = st.load_recovery().expect("load recovery");
        assert_eq!(rec.retries.len(), 2);
        assert_eq!(rec.retries[0].identifier, "MT-1");
        assert_eq!(rec.retries[0].attempt, 4, "upsert applied");
        assert_eq!(rec.retries[0].due_at_ms, 1000);
        assert_eq!(rec.retries[1].due_at_ms, 2000, "ordered by due_at_ms");
        assert_eq!(rec.claims.len(), 2);

        // DeleteRetry / DeleteClaim.
        st.delete_retry("MT-1").expect("delete retry");
        st.delete_claim("MT-2").expect("delete claim");
        let rec = st.load_recovery().expect("load recovery");
        assert_eq!(rec.retries.len(), 1);
        assert_eq!(rec.retries[0].identifier, "MT-2");
        assert_eq!(rec.claims.len(), 1);
        assert_eq!(rec.claims[0].issue_id, "MT-1");
    }

    // Mirror TestMarkRunningInterrupted: only the still-"running" rows flip to interrupted with an
    // ended_at stamp.
    #[test]
    fn mark_running_interrupted() {
        let st = open_mem();
        st.start_run(RunStart {
            issue_identifier: "MT-1".into(),
            ..Default::default()
        })
        .expect("start r1"); // running
        let r2 = st
            .start_run(RunStart {
                issue_identifier: "MT-2".into(),
                ..Default::default()
            })
            .expect("start r2"); // running
        st.end_run(
            r2,
            RunEnd {
                outcome: OUTCOME_COMPLETED.into(),
                ..Default::default()
            },
        )
        .expect("end r2"); // r2 no longer running

        let n = st.mark_running_interrupted().expect("mark");
        assert_eq!(n, 1, "only r1 was running");
        let runs = st
            .list_runs(RunFilter {
                issue: "MT-1".into(),
                ..Default::default()
            })
            .expect("list");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, OUTCOME_INTERRUPTED);
        assert!(!runs[0].ended_at.is_empty(), "ended_at must be set");
    }

    // Mirror TestTotalsRoundTrip: empty DB -> zero Totals; SaveTotals upserts the single id=1 row.
    #[test]
    fn totals_round_trip() {
        let st = open_mem();
        // Empty DB -> zero Totals.
        assert_eq!(
            st.load_totals().expect("load empty"),
            Totals::default(),
            "empty totals must be zero"
        );
        st.save_totals(Totals {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            seconds_running: 42,
        })
        .expect("save 1");
        // Upsert (single id=1 row).
        let want = Totals {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            seconds_running: 99,
        };
        st.save_totals(want).expect("save 2");
        assert_eq!(st.load_totals().expect("load"), want);
    }

    // Mirror TestLoadTotalsNoRowsZero: LoadTotals on an empty store returns the zero value, not an
    // error (Go's sql.ErrNoRows branch -> QueryReturnedNoRows here).
    #[test]
    fn load_totals_no_rows_zero() {
        let st = open_mem();
        assert_eq!(st.load_totals().expect("load empty"), Totals::default());
    }

    // Mirror TestStartRunPersistsTeamID (INF-223): the team_id passed at dispatch round-trips
    // through StartRun -> GetRun so a finished run can be resumed.
    #[test]
    fn start_run_persists_team_id() {
        let st = open_mem();
        let id = st
            .start_run(RunStart {
                issue_id: "I1".into(),
                issue_identifier: "INF-9".into(),
                title: "t".into(),
                team_id: "TEAM-1".into(),
                ..Default::default()
            })
            .expect("start");
        let got = st.get_run(id).expect("get run").expect("found");
        assert_eq!(got.team_id, "TEAM-1");
    }

    // ---------------------------------------------------------------------------------------------
    // S3 round-trip golden — the P2 phase gate. A real database written by the Go daemon
    // (harness/fixtures/db/go-daemon.db) must read back through the Rust Store API matching the
    // committed row dump (go-daemon-rows.json) after normalization; then rows written via the Rust
    // API must read back with identical semantics.
    // ---------------------------------------------------------------------------------------------

    // The read-side API projects away storage-internal columns: EventRow carries no `id`/`run_id`
    // (run_id is implied by the run_events(run_id) query). `usage_estimated` is the raw INTEGER 0/1
    // column, so it is emitted as an integer to match the sqlite3 -json dump.
    fn run_summary_to_json(r: &RunSummary) -> Value {
        json!({
            "id": r.id,
            "issue_id": r.issue_id,
            "issue_identifier": r.issue_identifier,
            "title": r.title,
            "attempt": r.attempt,
            "session_uuid": r.session_uuid,
            "branch": r.branch,
            "started_at": r.started_at,
            "ended_at": r.ended_at,
            "outcome": r.outcome,
            "turns": r.turns,
            "input_tokens": r.input_tokens,
            "output_tokens": r.output_tokens,
            "total_tokens": r.total_tokens,
            "usage_estimated": i64::from(r.usage_estimated),
            "error": r.error,
            "transcript_path": r.transcript_path,
            "project_slug": r.project_slug,
            "repo": r.repo,
            "team_id": r.team_id,
        })
    }
    fn event_row_to_json(e: &EventRow) -> Value {
        json!({ "seq": e.seq, "at": e.at, "kind": e.kind, "tool": e.tool, "text": e.text })
    }
    fn retry_row_to_json(r: &RetryRow) -> Value {
        json!({
            "issue_id": r.issue_id,
            "identifier": r.identifier,
            "attempt": r.attempt,
            "due_at_ms": r.due_at_ms,
            "error": r.error,
            "project_slug": r.project_slug,
        })
    }
    fn claim_row_to_json(c: &ClaimRow) -> Value {
        json!({
            "issue_id": c.issue_id,
            "state": c.state,
            "claimed_at": c.claimed_at,
            "project_slug": c.project_slug,
        })
    }
    fn run_message_to_json(m: &RunMessage) -> Value {
        json!({
            "id": m.id,
            "run_id": m.run_id,
            "body": m.body,
            "created_at_ms": m.created_at_ms,
            "status": m.status,
            "delivered_turn": m.delivered_turn,
        })
    }
    fn totals_to_json(t: &Totals) -> Value {
        json!({
            "id": 1,
            "input_tokens": t.input_tokens,
            "output_tokens": t.output_tokens,
            "total_tokens": t.total_tokens,
            "seconds_running": t.seconds_running,
        })
    }

    /// Project the committed golden to the fields the read-side API surfaces: drop the
    /// storage-internal `events.id` / `events.run_id` that [`EventRow`] intentionally omits.
    fn project_golden_to_api_shape(mut want: Value) -> Value {
        if let Some(events) = want["events"].as_array_mut() {
            for e in events {
                if let Some(obj) = e.as_object_mut() {
                    obj.remove("id");
                    obj.remove("run_id");
                }
            }
        }
        want
    }

    #[test]
    fn round_trip_go_daemon_db() {
        // Part A — read the Go daemon's committed rows via the Rust Store API. Work on a COPY:
        // Sqlite::open applies journal_mode=WAL, which rewrites the file header and spawns
        // -wal/-shm sidecars, so opening the fixture in place would dirty the tree (S2 pattern).
        let src = harness_fixtures::fixtures_dir().join("db/go-daemon.db");
        let scratch = scratch_dir();
        let db = scratch.join("go-daemon.db");
        std::fs::copy(&src, &db).expect("copy fixture db");
        let store = Sqlite::open(StorePath::Disk(db)).expect("open go-written db");

        let runs = store.list_runs(RunFilter::default()).expect("list_runs");
        assert_eq!(runs.len(), 1, "expected the Go daemon's single run row");
        let events = store.run_events(runs[0].id).expect("run_events");
        assert_eq!(events.len(), 80, "expected the smoke run's 80 events");
        let rec = store.load_recovery().expect("load_recovery");
        let totals = store.load_totals().expect("load_totals");
        let msgs = store
            .list_run_messages(runs[0].id)
            .expect("list_run_messages");

        // The capture HOME that normalize.sh rewrote to <HOME> when it produced the committed rows
        // JSON is recoverable from the run's transcript_path and applied via the SAME substitution
        // so the golden compares byte-for-byte. The daemon writes transcripts to the fixed layout
        // `<HOME>/.symphony/logs/<identifier>/<ts>-<attempt>.jsonl`, so HOME is the prefix before
        // `/.symphony/logs/`. (Splitting on a bare `/.symphony/` would be wrong: the capture home
        // itself contains `.symphony`.)
        let home = runs[0]
            .transcript_path
            .split("/.symphony/logs/")
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            !home.is_empty() && home != runs[0].transcript_path,
            "transcript_path should carry the capture home before /.symphony/logs/"
        );

        let got = json!({
            "runs": runs.iter().map(run_summary_to_json).collect::<Vec<_>>(),
            "events": events.iter().map(event_row_to_json).collect::<Vec<_>>(),
            "retry_queue": rec.retries.iter().map(retry_row_to_json).collect::<Vec<_>>(),
            "claims": rec.claims.iter().map(claim_row_to_json).collect::<Vec<_>>(),
            "run_messages": msgs.iter().map(run_message_to_json).collect::<Vec<_>>(),
            "totals": [totals_to_json(&totals)],
        });
        let got_norm: Value = serde_json::from_str(&harness_fixtures::normalize_with_home(
            &got.to_string(),
            &home,
        ))
        .expect("normalized read-back is valid JSON");

        let want =
            project_golden_to_api_shape(harness_fixtures::load_json("db/go-daemon-rows.json"));
        assert_eq!(
            got_norm, want,
            "Go-written rows must read back through the Rust Store API matching the golden after normalize"
        );

        // Part B — write a run via the Rust API into the same store and read it back with identical
        // semantics (StartRun -> AppendEvents -> EndRun -> GetRun/RunEvents).
        let new_id = store
            .start_run(RunStart {
                issue_id: "iss_rt".into(),
                issue_identifier: "RT-1".into(),
                title: "round-trip".into(),
                attempt: 1,
                started_at: "2026-02-02T00:00:00Z".into(),
                transcript_path: "/logs/RT-1/run-1.jsonl".into(),
                project_slug: "rtproj".into(),
                repo: "git@x/rt.git".into(),
                team_id: "team_rt".into(),
                ..Default::default()
            })
            .expect("start_run write-back");
        store
            .append_events(
                new_id,
                &[
                    EventRow {
                        seq: 1,
                        at: "2026-02-02T00:00:01Z".into(),
                        kind: "event".into(),
                        text: "session started".into(),
                        ..Default::default()
                    },
                    EventRow {
                        seq: 2,
                        at: "2026-02-02T00:00:02Z".into(),
                        kind: "text".into(),
                        text: "hi".into(),
                        ..Default::default()
                    },
                ],
            )
            .expect("append_events write-back");
        store
            .end_run(
                new_id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ended_at: "2026-02-02T00:01:00Z".into(),
                    turns: 2,
                    input_tokens: 5,
                    output_tokens: 7,
                    total_tokens: 12,
                    ..Default::default()
                },
            )
            .expect("end_run write-back");

        let rr = store.get_run(new_id).expect("get_run").expect("found");
        assert_eq!(rr.issue_identifier, "RT-1");
        assert_eq!(rr.outcome, OUTCOME_COMPLETED);
        assert_eq!(
            (rr.turns, rr.input_tokens, rr.output_tokens, rr.total_tokens),
            (2, 5, 7, 12)
        );
        assert_eq!(rr.team_id, "team_rt");
        assert_eq!(rr.transcript_path, "/logs/RT-1/run-1.jsonl");
        let rev = store.run_events(new_id).expect("run_events write-back");
        assert_eq!(rev.len(), 2);
        assert_eq!(rev[0].text, "session started");
        assert_eq!(rev[1].kind, "text");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    // -------------------------------------------------------------------------------------------
    // STUDIO-711 — ticketless review watch set (design STUDIO-703 §14.4 slice 2).
    //
    // No Go counterpart: the frozen reference has no review feature, so these assert a designed
    // behavior rather than a captured one. The parity gate they must NOT weaken is
    // `schema_matches_committed_golden` above — see `divergent_objects_are_gated_by_name_only`.
    // -------------------------------------------------------------------------------------------

    /// A watch-set key for `owner/repo#number` reviewed by `reviewer`.
    fn wkey(owner: &str, repo: &str, number: i64, reviewer: &str) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: owner.into(),
            repo: repo.into(),
            number,
            reviewer: reviewer.into(),
        }
    }

    /// A freshly-introduced row: requested by a handoff, nothing dispatched or reviewed yet.
    fn introduced(key: ReviewWatchKey) -> ReviewWatchRow {
        ReviewWatchRow {
            key,
            author: "alice".into(),
            introduced_by: "handoff".into(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.into(),
            open: true,
        }
    }

    // The whole point of the slice: the watch set survives a daemon restart. Writing through a
    // store on disk, dropping it, and re-opening the same file must return the identical set.
    #[test]
    fn watch_set_round_trips_across_a_restart() {
        let scratch = scratch_dir();
        let db = scratch.join("watch.db");

        {
            let store = Sqlite::open(StorePath::Disk(db.clone())).expect("open");
            let alice = wkey("makewhat", "rhapsody", 84, "alice");
            store
                .save_review_watch(introduced(alice.clone()))
                .expect("introduce");
            store
                .mark_review_requested(&alice, "aaa111")
                .expect("dispatch");
            store
                .mark_review_completed(&alice, "aaa111", REVIEW_STATUS_REVIEWED)
                .expect("complete");
            // A second PR still only requested, to prove partial state survives too.
            store
                .save_review_watch(introduced(wkey("makewhat", "rhapsody", 85, "jimmy")))
                .expect("introduce 2");
        } // store dropped — the daemon "restarts" here

        let store = Sqlite::open(StorePath::Disk(db)).expect("reopen");
        let set = store.load_review_watch().expect("recover watch set");
        assert_eq!(set.len(), 2, "both rows must survive the restart");
        assert_eq!(
            set[0],
            ReviewWatchRow {
                key: wkey("makewhat", "rhapsody", 84, "alice"),
                author: "alice".into(),
                introduced_by: "handoff".into(),
                requested_sha: "aaa111".into(),
                last_reviewed_sha: "aaa111".into(),
                status: REVIEW_STATUS_REVIEWED.into(),
                open: true,
            }
        );
        assert_eq!(set[1].key.number, 85);
        assert_eq!(set[1].status, REVIEW_STATUS_REQUESTED);
        assert_eq!(set[1].requested_sha, "", "nothing was dispatched for #85");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    // STUDIO-721: a reviewer run that burned its whole turn budget without finishing is recorded
    // NON-terminally. `last_reviewed_sha` must NOT move — the head was read only partially, and a
    // watcher that saw it advance would consider that partial read sufficient and never look at
    // this head again (which is how an absent review ships as a completed one).
    #[test]
    fn a_truncated_round_parks_the_row_without_advancing_either_sha() {
        let store = Sqlite::open(StorePath::InMemory).expect("open");
        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");
        store
            .mark_review_requested(&key, "aaa111")
            .expect("dispatch");

        store.mark_review_truncated(&key).expect("truncate");

        let row = store.get_review_watch(&key).expect("get").expect("row");
        assert_eq!(row.status, REVIEW_STATUS_TRUNCATED);
        assert_eq!(
            row.requested_sha, "aaa111",
            "the dispatched head is still the head that needs reviewing"
        );
        assert_eq!(
            row.last_reviewed_sha, "",
            "a truncated round read the head only partially, so nothing was reviewed at it"
        );
    }

    // The author rides the row because nothing else in the daemon still knows it by the time a
    // capped reviewer needs replacing: `runs` has no identity column.
    #[test]
    fn the_author_round_trips_and_survives_re_introduction() {
        let store = Sqlite::open(StorePath::InMemory).expect("open");
        let key = wkey("makewhat", "rhapsody", 84, "jimmy");
        store
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "alice".into(),
                introduced_by: "handoff:STUDIO-721".into(),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("introduce");
        assert_eq!(
            store
                .get_review_watch(&key)
                .expect("get")
                .expect("row")
                .author,
            "alice"
        );

        // Re-introduction refreshes provenance (a different handoff may carry a different author),
        // exactly as it refreshes `introduced_by`.
        store
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "bob".into(),
                introduced_by: "console".into(),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("re-introduce");
        assert_eq!(
            store
                .get_review_watch(&key)
                .expect("get")
                .expect("row")
                .author,
            "bob"
        );
    }

    // A database already at the shipped step-7 schema migrates forward to step 8 rather than
    // re-running step 7 (which would not add the column) or failing: the ALTER backfills every
    // existing row with the empty author the selection path fails closed on.
    #[test]
    fn a_v7_database_gains_the_author_column_with_an_empty_backfill() {
        let scratch = scratch_dir();
        let db = scratch.join("v7.db");
        {
            let mut conn = Connection::open(&db).expect("open raw");
            let tx = conn.transaction().expect("tx");
            for m in &MIGRATIONS[0..7] {
                tx.execute_batch(m).expect("apply step");
            }
            tx.execute_batch("PRAGMA user_version = 7")
                .expect("stamp v7");
            tx.commit().expect("commit");
            conn.execute(
                "INSERT INTO rhapsody_review_watch (owner, repo, number, reviewer) \
                 VALUES ('makewhat', 'rhapsody', 84, 'alice')",
                [],
            )
            .expect("insert legacy row");
        }

        let store = Sqlite::open(StorePath::Disk(db)).expect("migrate forward");
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, SCHEMA_VERSION);
        let row = store
            .get_review_watch(&wkey("makewhat", "rhapsody", 84, "alice"))
            .expect("get")
            .expect("the pre-existing row survives the migration");
        assert_eq!(
            row.author, "",
            "a row written before the column existed has no author, and must read as unknown"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    // Per-(PR, reviewer) granularity: two reviewers on ONE pull request are two independent rows,
    // and one reviewer's progress can never stamp the other's (design §14.2, "N reviewers share
    // one per-PR SHA" — the bug that silently ships a PR with a crashed reviewer's review missing).
    #[test]
    fn two_reviewers_on_one_pr_are_independent_rows() {
        let store = open_mem();
        let alice = wkey("makewhat", "rhapsody", 84, "alice");
        let jimmy = wkey("makewhat", "rhapsody", 84, "jimmy");
        store
            .save_review_watch(introduced(alice.clone()))
            .expect("introduce alice");
        store
            .save_review_watch(introduced(jimmy.clone()))
            .expect("introduce jimmy");

        // alice reviews the head; jimmy's run crashed before it started.
        store
            .mark_review_requested(&alice, "head1")
            .expect("dispatch alice");
        store
            .mark_review_completed(&alice, "head1", REVIEW_STATUS_APPROVED)
            .expect("complete alice");

        let a = store
            .get_review_watch(&alice)
            .expect("get alice")
            .expect("alice row");
        let j = store
            .get_review_watch(&jimmy)
            .expect("get jimmy")
            .expect("jimmy row");
        assert_eq!(a.last_reviewed_sha, "head1");
        assert_eq!(a.status, REVIEW_STATUS_APPROVED);
        assert_eq!(
            j.last_reviewed_sha, "",
            "jimmy has reviewed nothing; alice's completion must not stamp his row"
        );
        assert_eq!(j.status, REVIEW_STATUS_REQUESTED);
        assert_eq!(store.load_review_watch().expect("load").len(), 2);
    }

    // F-DUP: `requested_sha` is written at DISPATCH, before any review completes, and moves the
    // row in-flight — that is what lets the watcher edge-trigger instead of re-dispatching onto a
    // live worktree every tick.
    #[test]
    fn dispatch_records_the_requested_sha_and_goes_in_flight() {
        let store = open_mem();
        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");

        store
            .mark_review_requested(&key, "dispatched1")
            .expect("dispatch");
        let row = store.get_review_watch(&key).expect("get").expect("row");
        assert_eq!(row.requested_sha, "dispatched1");
        assert_eq!(row.status, REVIEW_STATUS_IN_FLIGHT);
        assert_eq!(
            row.last_reviewed_sha, "",
            "dispatch must not touch the reviewed SHA — nothing has been read yet"
        );
    }

    // F-SHA: completion records the SHA the reviewer was PINNED to, which may already be behind
    // the live head. Recording it must not disturb `requested_sha`, so a head that advanced
    // mid-review is still visibly un-reviewed.
    #[test]
    fn completion_records_the_pinned_sha_and_leaves_requested_alone() {
        let store = open_mem();
        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");
        store
            .mark_review_requested(&key, "pinned1")
            .expect("dispatch");

        store
            .mark_review_completed(&key, "pinned1", REVIEW_STATUS_REVIEWED)
            .expect("complete");
        let row = store.get_review_watch(&key).expect("get").expect("row");
        assert_eq!(row.requested_sha, "pinned1");
        assert_eq!(row.last_reviewed_sha, "pinned1");
        assert_eq!(row.status, REVIEW_STATUS_REVIEWED);
    }

    // Re-introducing a PR that is already watched must not forget either SHA. A `save` that
    // clobbered them would re-open F-DUP (a forgotten requested SHA re-dispatches onto a live
    // worktree) and F-SHA (a forgotten reviewed SHA re-reviews what was already read).
    #[test]
    fn re_introduction_preserves_both_shas() {
        let store = open_mem();
        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");
        store
            .mark_review_requested(&key, "sha_req")
            .expect("dispatch");
        store
            .mark_review_completed(&key, "sha_req", REVIEW_STATUS_REVIEWED)
            .expect("complete");

        // The operator re-introduces the same PR with an all-empty row.
        store
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "alice".into(),
                introduced_by: "operator".into(),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("re-introduce");

        let row = store.get_review_watch(&key).expect("get").expect("row");
        assert_eq!(
            row.requested_sha, "sha_req",
            "requested SHA must survive re-introduction"
        );
        assert_eq!(
            row.last_reviewed_sha, "sha_req",
            "reviewed SHA must survive re-introduction"
        );
        assert_eq!(row.introduced_by, "operator", "origin is refreshed");
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED, "status is re-armed");
        assert_eq!(
            store.load_review_watch().expect("load").len(),
            1,
            "still one row"
        );
    }

    // Merge/close/gone (Slice 1's MERGED | CLOSED | Gone) clears `open` and parks the row at
    // `dropped`, keeping both SHAs as the record of what was reviewed. Idempotent.
    #[test]
    fn drop_clears_open_and_keeps_the_reviewed_record() {
        let store = open_mem();
        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");
        store.mark_review_requested(&key, "sha1").expect("dispatch");
        store
            .mark_review_completed(&key, "sha1", REVIEW_STATUS_REVIEWED)
            .expect("complete");

        store.drop_review_watch(&key).expect("drop");
        store
            .drop_review_watch(&key)
            .expect("drop again is idempotent");
        let row = store.get_review_watch(&key).expect("get").expect("row");
        assert!(!row.open);
        assert_eq!(row.status, REVIEW_STATUS_DROPPED);
        assert_eq!(
            row.last_reviewed_sha, "sha1",
            "the review record is kept, not erased"
        );
    }

    // An absent row is `Ok(None)`, not an error — the watcher asks "is this pair watched?" and a
    // "no" must be answerable without treating it as a failure. The mutating methods are likewise
    // no-ops on an absent row rather than errors.
    #[test]
    fn absent_row_reads_none_and_writes_are_no_ops() {
        let store = open_mem();
        let key = wkey("makewhat", "rhapsody", 999, "nobody");
        assert!(store.get_review_watch(&key).expect("get").is_none());
        store
            .mark_review_requested(&key, "x")
            .expect("no-op dispatch");
        store
            .mark_review_completed(&key, "x", REVIEW_STATUS_REVIEWED)
            .expect("no-op complete");
        store.drop_review_watch(&key).expect("no-op drop");
        assert!(store.get_review_watch(&key).expect("get").is_none());
        assert!(store.load_review_watch().expect("load").is_empty());
    }

    // Recovery order is deterministic (owner, repo, number, reviewer) so a rebuilt watch set does
    // not depend on insertion order.
    #[test]
    fn load_orders_by_pr_then_reviewer() {
        let store = open_mem();
        for k in [
            wkey("zeta", "repo", 1, "alice"),
            wkey("makewhat", "rhapsody", 84, "jimmy"),
            wkey("makewhat", "rhapsody", 9, "alice"),
            wkey("makewhat", "rhapsody", 84, "alice"),
            wkey("makewhat", "other", 84, "alice"),
        ] {
            store.save_review_watch(introduced(k)).expect("introduce");
        }
        let got: Vec<(String, String, i64, String)> = store
            .load_review_watch()
            .expect("load")
            .into_iter()
            .map(|r| (r.key.owner, r.key.repo, r.key.number, r.key.reviewer))
            .collect();
        assert_eq!(
            got,
            vec![
                ("makewhat".into(), "other".into(), 84, "alice".into()),
                ("makewhat".into(), "rhapsody".into(), 9, "alice".into()),
                ("makewhat".into(), "rhapsody".into(), 84, "alice".into()),
                ("makewhat".into(), "rhapsody".into(), 84, "jimmy".into()),
                ("zeta".into(), "repo".into(), 1, "alice".into()),
            ]
        );
    }

    // Teams-gating (design §16): the table and its access path exist harmlessly on EVERY daemon.
    // This slice ships no writer at all, so a Teams-off daemon — the shipped default — opens a
    // store whose watch set is empty and stays empty.
    #[test]
    fn a_fresh_store_has_an_empty_watch_set() {
        let store = open_mem();
        assert!(store.load_review_watch().expect("load").is_empty());
        // The table itself is present and queryable, not merely absent-and-forgiving.
        let n: i64 = store
            .lock()
            .query_row("SELECT count(*) FROM rhapsody_review_watch", [], |row| {
                row.get(0)
            })
            .expect("the table exists");
        assert_eq!(n, 0);
    }

    // The rejected alternative was overloading a `runs` column to carry a SHA, which would surface
    // a SHA everywhere the console renders a branch. Two halves: `runs` still has EXACTLY its
    // v6 column set (no smuggled column), and exercising the whole watch-set API writes no run row.
    #[test]
    fn review_state_never_touches_the_runs_table() {
        let store = open_mem();
        let cols: Vec<String> = {
            let conn = store.lock();
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info('runs')")
                .expect("prepare");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.map(|c| c.expect("col")).collect()
        };
        assert_eq!(
            cols,
            vec![
                "id",
                "issue_id",
                "issue_identifier",
                "title",
                "attempt",
                "session_uuid",
                "branch",
                "started_at",
                "ended_at",
                "outcome",
                "turns",
                "input_tokens",
                "output_tokens",
                "total_tokens",
                "error",
                "transcript_path",
                "project_slug",
                "repo",
                "usage_estimated",
                "team_id",
            ],
            "the review watch set must add no column to `runs` — a SHA in `branch` would render \
             as a branch in the console"
        );

        let key = wkey("makewhat", "rhapsody", 84, "alice");
        store
            .save_review_watch(introduced(key.clone()))
            .expect("introduce");
        store.mark_review_requested(&key, "sha1").expect("dispatch");
        store
            .mark_review_completed(&key, "sha1", REVIEW_STATUS_REVIEWED)
            .expect("complete");
        store.drop_review_watch(&key).expect("drop");

        let runs: i64 = store
            .lock()
            .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
            .expect("count runs");
        assert_eq!(runs, 0, "the whole watch-set API must write no run row");
    }

    // THE GATE, asserted rather than assumed. `schema_dump` hides Rhapsody-only objects from the
    // Go-recaptured golden by NAME PREFIX only (`rhapsody_`), because the golden is recapturable
    // only from a Go daemon that has no review feature. This proves the exclusion is that narrow:
    // every object in the live schema is either byte-present in the committed golden or carries
    // the prefix, so a future un-prefixed table cannot slip past the golden.
    #[test]
    fn divergent_objects_are_gated_by_name_only() {
        let store = Sqlite::open(StorePath::InMemory).expect("open in-memory");
        let golden = harness_fixtures::load("schema.sql");
        let conn = store.lock();
        // Only `sqlite_%` is dropped here — the SQLite-internal bookkeeping `schema_dump` already
        // documents as not-application-schema. Every OTHER live object must justify itself below.
        let mut stmt = conn
            .prepare(
                "SELECT name, sql FROM sqlite_master \
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid",
            )
            .expect("prepare");
        let objects: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        drop(stmt);
        drop(conn);

        let mut divergent = Vec::new();
        for (name, sql) in &objects {
            if golden.contains(&format!("{sql};\n")) {
                continue; // a Go-owned object, byte-present in the golden
            }
            assert!(
                name.starts_with(DIVERGENT_OBJECT_PREFIX),
                "`{name}` is neither in the Go golden nor named `{DIVERGENT_OBJECT_PREFIX}*`: \
                 either it is drift from the reference, or it is a Rhapsody-only object that must \
                 be renamed to carry the prefix so the gate can see it"
            );
            divergent.push(name.clone());
        }
        assert_eq!(
            divergent,
            vec!["rhapsody_review_watch".to_string()],
            "exactly one documented divergent object exists today (README `Divergences`)"
        );
    }

    // The gate excludes by the LITERAL prefix `rhapsody_`, not by a LIKE pattern in which `_` is a
    // single-character wildcard. An unescaped `'rhapsody_%'` would also hide, say, `rhapsodyXfoo`,
    // quietly widening the exclusion beyond the documented rule. A table that merely resembles the
    // prefix must still reach the golden — and therefore still turn it red.
    #[test]
    fn the_gate_prefix_is_literal_not_a_like_wildcard() {
        let store = Sqlite::open(StorePath::InMemory).expect("open in-memory");
        store
            .lock()
            .execute_batch("CREATE TABLE rhapsodyXfoo (a TEXT);")
            .expect("create look-alike table");
        assert!(
            schema_dump(&store).contains("CREATE TABLE rhapsodyXfoo"),
            "only the literal `rhapsody_` prefix is excluded; a look-alike must still be compared \
             against the golden"
        );
    }
}
