# CLAUDE.md — crates/harness-fixtures

## What this crate is

Not a port of any Go package — it's the shared test harness every porting crate's golden tests
depend on. Root CLAUDE.md's crate-map table omits it (it's mentioned only in prose, under
"Parity-port testing model") because it isn't a runtime component of `rhapsodyd`: it ships as a
`[dev-dependencies]`-only crate, pulled in by `rhapsody-config`, `rhapsody-agent`, `rhapsody-mcp`,
`rhapsody-httpapi`, `rhapsody-orchestrator`, and `rhapsody-store`. It is never linked into the
`rhapsodyd` binary itself.

For what a specific consumer uses it for: `config`, `agent`, `mcp`, `httpapi`, and `orchestrator`
each carry a comment directly above their `harness-fixtures = { path = ... }` line explaining the
golden it checks — check that first. `store` is the exception: its `Cargo.toml` line is bare (the
comment immediately below it belongs to the adjacent `serde_json` dependency, not this crate); its
usage is documented inline at the call sites instead, e.g. the `harness_fixtures::load(...)` /
`fixtures_dir()` calls around
`crates/store/src/sqlite.rs:1009` and `:2587`.

**Naming deviation**: the package is named `harness-fixtures`, not `rhapsody-harness-fixtures` —
it doesn't follow the root CLAUDE.md's `rhapsody-<dir>` crate-naming convention. That's because
it isn't part of the daemon's own crate family; it's infrastructure shared across them.

## What it does

Two things, both in `src/lib.rs` (single file):

1. **Loads committed goldens** (`fixtures_dir()`, `load()`, `load_json()`) from `harness/fixtures/`
   (captured from the reference Go daemon — see `harness/capture/README.md`). This is the loading
   half of the serialize/normalize/diff pattern root CLAUDE.md's "Parity-port testing model"
   describes — see that section for the pattern itself.
2. **`normalize()` / `normalize_with_home()`** — a Rust line-for-line mirror of
   `harness/capture/normalize.sh`'s `sed` pipeline, rewriting capture-run-specific values
   (timestamps, UUIDs, the capture `$HOME`, loopback ports, wall-clock numerics) to fixed
   placeholders (`<TIMESTAMP>`, `<UUID>`, `<HOME>`, `<PORT>`, `<NUM>`) so goldens are stable
   across captures.

## The lockstep contract (the one thing you must not break silently)

`normalize()`'s seven rules are numbered comments matching `normalize.sh`'s `sed -e` steps
**in the same order**. The `normalize_matches_shell_rules` test is a canary: it shells out to the
real `normalize.sh` and asserts byte-identical output on a fixed sample. If you change a rule in
one place, change it in the other in the same commit — the canary is what catches drift, not
manual review. `interval_ms`-style plain config constants are deliberately NOT normalized (only
`*_at_ms`, `*duration`, `*_running` fields are); don't "fix" that asymmetry, it's load-bearing
(config fixtures need their literal `500` to stay `500`).

## `unwrap`/`expect`/`panic!` are intentional — do not add error handling here

This is a test-only dev-dependency. A missing or malformed fixture, or a failed shell-out to
`normalize.sh`, is meant to **panic loudly with an actionable message** (e.g. `load()`'s
`"missing fixture {path}: {e} — run \`make fixtures\`"`). A `Result`-returning API that let a
caller silently skip a golden comparison would defeat the entire parity gate this crate exists to
enforce. If you're tempted to make a function here more defensive, that's the wrong instinct for
this crate specifically — it cuts against the whole codebase's parity-testing model, not just a
local style preference.

## Other non-obvious things

- `fixtures_dir()` resolves `harness/fixtures/` via `CARGO_MANIFEST_DIR` + `../../harness/fixtures`
  — it assumes this crate stays at `crates/harness-fixtures/`; moving it breaks every consumer's
  fixture path silently until tests fail.
- `canary_fixtures_are_normalized` asserts every *text* fixture in the tree is already a no-op
  under `normalize()` — i.e. the committed goldens themselves must be pre-normalized. `db/*.db`
  binary fixtures are the one exempt case (non-deterministic SQLite bytes by design; see
  `harness/capture/README.md`, "Go-written database fixture" — the sibling `*-rows.json` dump
  carries the determinism signal instead).
- `canary_schema_has_all_tables` hardcodes the count `6` and the six table names. If
  `harness/fixtures/schema.sql` legitimately grows a table, update this test in the same change —
  it's meant to go red on any schema drift, intentional or not.
- These tests read real files from `harness/fixtures/` and shell out to `bash`
  (`harness/capture/normalize.sh`) — they are not hermetic unit tests. They do not, however,
  require Go or a running daemon (that's only `make fixtures`, run separately, operator-only).
