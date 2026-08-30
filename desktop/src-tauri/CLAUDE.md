# CLAUDE.md — desktop/src-tauri

This is the `rhapsody-desktop` crate itself (see `desktop/CLAUDE.md` for build/test commands, the
module-map pointer, the two-bin-targets pitfall, and the `fakedaemon` default-feature gotcha — not
repeated here). This file only adds what that doc comment and `desktop/CLAUDE.md` don't cover.

## Read `src/supervisor/` first if you're touching daemon lifecycle

`supervisor/` is a plain module directory (no manifest of its own), but it's where the real
complexity lives — `mod.rs` alone is 775 lines because it's a from-scratch async port of a Go
goroutine+channel state machine (`$REF/desktop/internal/supervisor/supervisor.go`) onto a spawned
tokio task per `Start`, with `oneshot`/`watch` primitives standing in for the channels. Read
`mod.rs`'s own doc comment before editing any of the three files:

- `mod.rs` — the `Supervisor` type: state machine (`Stopped`/`Starting`/`Running`), start/stop,
  health-poll loop, crash-restart with backoff.
- `resolve.rs` — locates the `rhapsodyd` binary (dev override vs. bundled `Resources/`).
- `env.rs` — builds the child process's PATH/env (the launchd/Finder minimal-PATH problem, plus
  `LINEAR_API_KEY` and `GIT_CONFIG_*` injection).

The module is deliberately Tauri-free (no `tauri` import anywhere in it) so it's unit-testable
against a fake daemon in-process. This Tauri-free-core / thin-Tauri-adapter split recurs elsewhere
in the crate — `app.rs` (lifecycle) and `apiproxy.rs` (proxy core) are also Tauri-free; `windowserver.rs`
and the tray/menu wiring in `main.rs` are the thin layers that plug the Tauri types in on top. If a bug
looks like it should be in the core logic, check the Tauri-free module first — that's usually where
the actual test coverage is.

## `src/bin/fakedaemon.rs`

If you change the supervisor's health-check or shutdown contract, check whether `fakedaemon` needs
a matching change or the lifecycle tests will pass against a fake that no longer represents real
`rhapsodyd` behavior.

## Four integration test files, each testing a different boundary

`tests/` is unusually heavy for a Tauri shell — all four are black-box (real subprocess / real
scripts, not mocked):

- `supervisor_lifecycle.rs` — runs by default; drives the real compiled `fakedaemon` bin through
  start/health/restart/drain. This is the primary supervisor test coverage; `mod.rs`'s unit tests
  are secondary to it.
- `packaging_gate.rs` — runs by default, unlike the two below; see `desktop/CLAUDE.md` for what it
  shells out to and why.
- `real_rhapsodyd_smoke.rs` / `parity_e2e.rs` — gated off by default (env vars documented in
  `desktop/CLAUDE.md`); the only tests in the crate that exercise the *real* `rhapsodyd` binary
  instead of `fakedaemon`. If a bug only reproduces against real `rhapsodyd` (not the stub), one of
  these two is where to add a regression test, not `supervisor_lifecycle.rs`.

## Tauri v2 capability-permission gotcha

`capabilities/default.json` is an allowlist, not a default-allow surface: an IPC command (e.g. the
dialog plugin's file/folder picker) with no matching `permissions` entry here is silently denied at
runtime — no compile error, no panic, just a picker that does nothing. If you add a new Tauri plugin
or a new frontend-invokable command, check this file, not just the plugin's own Cargo feature flags.
