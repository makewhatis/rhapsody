# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Rhapsody is a Rust parity port of **Symphony**, a Go daemon (`rhapsodyd`) that reads work from
Linear, creates isolated per-issue git workspaces, and runs Claude Code agents inside them. It is
byte-for-byte behavior-identical to the frozen Go reference **except where `README.md`'s
"Divergences" section documents a deliberate, reviewed exception** (e.g. `~/.rhapsody` runtime
paths, telemetry defaults). Treat any other apparent behavior mismatch as a porting bug, not an
invitation to "improve" it — and add a new Divergences entry if a change is intentional.

## Commands

- Build: `cargo build --workspace`
- Lint: `make lint` (`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings`)
- Test: `make test` (`cargo test --workspace`)
- Single crate: `cargo test -p <crate>` (crate names are `rhapsody-<dir>`, e.g. `rhapsody-config`)
- Single test: `cargo test -p <crate> <test_name_substring>`
- Recapture golden fixtures (operator machine only, needs `$REF`): `make fixtures`
- Web dashboard (`web/`): `npm run build` (`tsc -b && vite build`), `npm test` (`vitest run`)
- Desktop app (`desktop/`, its own cargo workspace — see below): `make app` builds the unsigned
  `.app` with an embedded `rhapsodyd` sidecar; signing/notarization details in `desktop/SIGNING.md`.

## Architecture

**Three build units, deliberately separate:**

- `crates/*` — the daemon workspace (root `Cargo.toml`). Builds `rhapsodyd`.
- `desktop/` — its own cargo workspace (Tauri v2), excluded from the root workspace so its heavy
  dependency tree stays out of the daemon build. Root `cargo`/`make lint` never touch it; CI builds
  it as a separate job. Supervises `rhapsodyd` as a bundled sidecar.
- `web/` — a single React (Vite) build serving **two** roles: the daemon's embedded dashboard
  (`crates/httpapi/web-dist`, via `rust-embed`) and the desktop app's own window content (Tauri's
  `frontendDist`). One build, two consumers — don't assume changes here only affect one of them.

**Parity-port testing model** (`harness-fixtures` crate + `harness/fixtures/`): porting crates
serialize their output, run it through `normalize` (a Rust mirror of `harness/capture/normalize.sh`,
kept byte-identical by a canary test), and assert equality against a committed golden. Editing or
losing a golden turns that crate's tests red — goldens are recaptured from the real Go daemon via
`make fixtures`, not hand-edited.

**Crate map** (each crate's own top-of-file doc comment names its exact Go source package —
read that first when working in a crate you don't know):

| Crate | Role |
|---|---|
| `core` | Shared domain types (issue/project/summon), ported field-for-field from Go |
| `config` | WORKFLOW.md loading, prompt rendering (Liquid), the capabilities registry |
| `workspace` | Git layer: per-repo bare-mirror cache, worktree creation, branch naming |
| `agent` | Backend-agnostic coding-agent abstraction (`Runner`/`Session` traits); Claude subprocess backend |
| `tracker` | The `Tracker` trait the orchestrator schedules against; Linear (GraphQL) + file adapters |
| `orchestrator` | The daemon's heart — dispatch, turn loop, retry, one `Orchestrator` struct grown across many files |
| `store` | Durable history/restart-recovery (SQLite via `rusqlite`, WAL) or `Noop` when storage is off |
| `httpapi` | Loopback JSON API + embedded React dashboard, read-only except `/refresh` |
| `mcp` | `rhapsodyd mcp` — thin read-mostly MCP facade over the daemon's own HTTP API |
| `telemetry` | Optional OpenTelemetry export; a no-op (never fails the daemon) when disabled |
| `rhapsodyd` | The daemon binary — signal handling, delegates to `run::run` |

## Divergences from the Go reference

Full list with rationale lives in `README.md`. The categories so far: runtime filesystem paths
(`~/.rhapsody` vs `~/.symphony`), telemetry defaults (off, no bundled collector), rotating file
logs, one classifier fix (`review_states` on a clean exit), an additive `/api/v1/version` endpoint,
and additive history-paging endpoints. Git branch prefix (`symphony/<key>`), MCP tool name prefixes,
and agent env var names (`SYMPHONY_*`) are explicitly **out of scope** — cross-process contracts
that must not change even though the project renamed.
