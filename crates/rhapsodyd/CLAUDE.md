# CLAUDE.md — crates/rhapsodyd

Parity port of `$REF/cmd/symphony/{main,run,mcp}.go`. This crate carries both the `rhapsodyd`
binary (`src/main.rs`) and a library (`src/lib.rs`) of the pieces it assembles from every other
porting crate — it is the daemon's composition root, not a place to add new domain logic. Only the
binary's name diverges from Go; every operator-facing string (stderr diagnostics, the `symphony
mcp:` error marker, the ASCII banner, `SYMPHONY_*` env var names) stays byte-identical to Go on
purpose — see root CLAUDE.md's "out of scope" cross-process contracts.

## Module map (read in this order to understand boot)

1. **`main.rs`** — installs a SIGINT/SIGTERM → `CancelSignal` bridge, then calls `run::run` and
   `process::exit`s its return code.
2. **`run.rs`** — the actual boot sequence, in order: `mcp` subcommand dispatch → flag parse →
   single-instance flock → telemetry init (+ install as global `tracing` subscriber) → build
   `Orchestrator`, open the durable store, load the capabilities registry, inject both *before*
   `o.run()` → snapshot the off-loop `ControlHandle` → optionally bind the observability HTTP
   server → render the startup banner → start the prune scheduler → await the control loop →
   drain everything in reverse order.
3. **`bootcfg.rs`** — pure helper functions `run.rs` calls for each boot decision (server port,
   store-open, capabilities-registry path, banner data). Kept pure and separate specifically so
   tests can drive each decision without a full daemon boot.
4. **`otel.rs`** — maps `otel:` config + `OTEL_*` env into a `telemetry::Config` (enablement,
   protocol normalization, loopback-insecure heuristic).
5. **`mcp.rs`** — the `rhapsodyd mcp` subcommand: a thin stdio MCP facade that talks to the
   *running* daemon over loopback HTTP; never touches `~/.rhapsody` or the DB directly.
6. **`runlock.rs`** — the single-instance advisory flock.
7. **`prune.rs`** — the daily history + stale-worktree GC scheduler.
8. **`state.rs`** / **`logsource.rs`** — adapters wiring `httpapi`'s `StateProvider`/`LogSource`
   traits onto the orchestrator/telemetry types (see "Why two adapter modules" below).
9. **`banner.rs`** — the pure, TTY-agnostic startup-banner renderer.

## Why `state.rs` and `logsource.rs` exist at all

Go hands `*Orchestrator` and `*telemetry.LogBuffer` straight to `httpapi.New` because both satisfy
Go's interfaces directly. Rust can't do that: the control loop owns `&mut self` on the
orchestrator, so `Orchestrator` moves into its own task and the daemon can only reach it through
the cloneable `ControlHandle` snapshotted *before* the move (`o.control()`, taken in `run.rs`
before `o.run()`). `state.rs`'s `DaemonState` implements `httpapi::StateProvider` by delegating to
that handle; `logsource.rs`'s `LogBufferSource` similarly bridges telemetry's `LogEntry` (with a
`DateTime<Utc>`) to httpapi's wire `LogEntry` (pre-formatted RFC3339 `String`) via a background
forwarder thread, because the telemetry ring's `subscribe()` returns a blocking `std::mpsc`
receiver, not a tokio channel. Don't try to remove these adapters by making the crates depend on
each other — the split is required by the trait-ownership constraint, not accidental.

## Pitfalls specific to this crate

- **Bool flag parsing (`run.rs::parse_bool_flag`)**: mirrors Go's `strconv.ParseBool`, so
  `--no-store=0`/`=false`/`=f`/`=FALSE` mean the store stays **ON** — a naive `!= "false"` check
  silently inverts this. `--no-store` with no inline value is `true`. Covered by
  `parse_flags_semantics`; if you touch flag parsing, extend that test, not just the happy path.
- **Store path is shared, not duplicated**: `bootcfg::resolve_capabilities_path` deliberately
  reuses the exact same `--db`/`storage.path` resolution as `open_store` (down to `off`/
  `:memory:` handling) so `capabilities.yaml` always colocates with `rhapsody.db`. If you change
  one, change the other or you'll split their homes silently.
- **Lock file naming (`runlock::with_lock_suffix`)**: appends the literal `.lock` suffix via
  `OsString::push`, not `set_extension` — `set_extension` would replace `.md` and collide two
  different workflow files' locks. `canonical_lock_path` also resolves through symlinks and the
  macOS `/var`→`/private/var` indirection so two spellings of one file take one lock.
- **`banner.rs`'s `SYMPHONY_ART` lines carry deliberate trailing spaces** to keep the wordmark's
  slanted right edge aligned. An editor with "trim trailing whitespace on save" will silently
  break this; `cargo fmt` does not touch string literals so it won't catch it either.
- **Shutdown ordering in `run.rs`** is load-bearing, not incidental: the prune task is stopped and
  joined *before* the observability server drains, and both finish *before* the final stderr
  write, so log lines from shutdown never race the process's final output. Preserve this order in
  any edit that touches the tail of `run`.
- **`testutil.rs`'s `TempDir` is hand-rolled** (no `tempfile` dev-dependency) — consistent with
  `rhapsody-core`'s runtimeport tests and the orchestrator's `testsupport`. Don't add `tempfile`
  here; follow the existing pattern.
- **Daemon tests must stay hermetic**: every test workflow points `workspace.root` / `logging.dir`
  / `storage.path` inside a `TempDir`, and the tracker endpoint at a dead loopback address
  (`http://127.0.0.1:9`) so network calls fail fast without touching the real `~/.rhapsody`. A CI
  runner may have a live production daemon; a test that defaults these paths can corrupt its state.
- **The Rust orchestrator defers disk store-open to the daemon** (unlike Go, where `Orchestrator`
  opens its own store) — `run.rs` opens the store and capabilities registry and injects both via
  `set_store`/`capabilities_registry` *before* calling `o.run()`. If you add a new piece of state
  the orchestrator needs at boot, it likely belongs in this same inject-before-`run()` block, not
  inside the orchestrator itself.
