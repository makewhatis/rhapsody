# CLAUDE.md — crates/store

Parity port of Go `internal/store` (Symphony v0.4.0). Read `src/lib.rs`'s top-of-file doc comment
first — it names the exact Go package and lists the six tables this crate owns.

## Layout

- `lib.rs` — the `Store` trait (port of Go's `Store` interface), `StoreError`, and `StorePath` /
  `parse_store_path` (classifies a raw `storage.path` config string into `Off` / `InMemory` /
  `Disk`).
- `sqlite.rs` — the real backend: DDL migrations, pragmas, open path, and every `Store` method.
- `noop.rs` — the disabled backend used for `storage.path: off`.
- `types.rs` — every domain struct/constant, field-for-field from Go's `store.go`.

## The two `Store` impls must stay in lock-step

`Sqlite` and `Noop` both implement the full `Store` trait. Adding, renaming, or changing the
signature of a trait method means touching **three** files: the trait in `lib.rs`, the real
implementation in `sqlite.rs`, and the no-op stub in `noop.rs` (always `Ok(<empty/zero/None>)`,
never an error — that guard-free contract is what lets every non-store call site skip an `if
storage enabled` check). Forgetting `noop.rs` still compiles as long as the trait is unimplemented
nowhere else, so this is a review-time check, not a compiler-enforced one until you actually build.

## Migrations are the parity contract — treat them as append-only

`sqlite.rs`'s `MIGRATIONS` array is copied **verbatim** from Go's `migrations` slice. A dedicated
golden test (`schema_matches_committed_golden`) reassembles the live `sqlite_master` DDL after
opening a fresh `:memory:` store and asserts it is byte-identical to the committed
`harness/fixtures/schema.sql`.

- Never edit an existing migration string in place (even to "clean it up") — SQLite canonicalizes
  stored DDL text, so any wording change ripples into the golden and looks like drift from the Go
  reference even when the resulting schema is equivalent.
- A real schema change is a **new** array element plus `SCHEMA_VERSION += 1`. `migrate()` applies
  every step whose index is `>= current user_version`, so old on-disk databases (including ones the
  Go daemon wrote) upgrade forward through the exact same step sequence Go used.
- Recapturing `harness/fixtures/schema.sql` and `db/go-daemon.db` after a schema change requires
  the operator-only `make fixtures` (root CLAUDE.md) — you cannot regenerate them from this crate
  alone.

## Concurrency model

`Sqlite` holds one `Mutex<Connection>` and serializes **all** access through it — this
intentionally mirrors Go's `db.SetMaxOpenConns(1)` plus its write mutex, not a Rust-native
"let SQLite handle concurrency" design. WAL mode is enabled for crash-safety and committed-read
visibility, not for read/write parallelism; don't "fix" the single connection into a pool assuming
WAL makes that safe — it would diverge from the Go store's observable behavior (e.g. lock
ordering under `concurrent_read_during_write`-style tests). A poisoned mutex is recovered
(`poison.into_inner()`) rather than propagated, since no `Store` method panics while holding the
lock.

## Query-building conventions worth knowing before touching `sqlite.rs`

- `RUN_COLS` is the single shared column list for every run-projection query; `map_run_summary`
  reads it **positionally**. Adding a `runs` column means updating `RUN_COLS`, the migration that
  adds it, and the positional index in `map_run_summary` together, or the scan silently
  misassigns fields.
- `run_filter_where` is shared by `list_runs` and `list_issue_runs` so the two can never select a
  different row set — only their paging differs. Add new `RunFilter` predicates there, not
  independently in each query builder.
- `list_issue_runs` pages by **issue**, not by run, via a `ROW_NUMBER() OVER (PARTITION BY …)`
  window query that keeps only each issue's newest matching row (TRA-320: keeps one retry-looping
  issue from crowding others off a page). Unattributed runs (empty `issue_identifier`) partition
  by `run:<id>` so they never collapse into each other.
- `escape_like` must escape `\` before `%`/`_`, in that order, or the wildcard-escaping
  double-escapes itself.

## Test patterns specific to this crate

- Tests that open a copy of `harness/fixtures/db/go-daemon.db` (`opens_go_written_database...`,
  `round_trip_go_daemon_db`, the idempotent-migration test) copy it into a scratch dir first and
  open the **copy**, never the fixture in place: `Sqlite::open` sets `journal_mode=WAL`, which
  rewrites the file header and spawns `-wal`/`-shm` sidecars — opening the committed fixture
  directly would dirty the tree on every test run. Follow this pattern for any new test that reads
  a committed `.db` fixture.
- `round_trip_go_daemon_db` is the P2 phase gate: it reads the Go-written fixture through the Rust
  `Store` API, normalizes the result the same way `harness/capture/normalize.sh` does, and diffs it
  against the committed `db/go-daemon-rows.json`, then does the same round trip through rows
  written by the Rust API itself. `project_golden_to_api_shape` documents the one deliberate shape
  difference: `EventRow` omits the storage-internal `id`/`run_id` columns Go's raw dump includes.
- `scratch_dir()` (pid + atomic counter under the system temp dir) is this crate's own throwaway-db
  helper — reuse it rather than adding a `tempfile` dependency.
