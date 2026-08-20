# CLAUDE.md — harness

The whole parity-port fixture-capture and testing rig. The root CLAUDE.md only mentions the
`harness-fixtures` crate and `harness/fixtures/` (the committed golden data); this file maps the
other five subdirectories that produce or consume those goldens.

| Dir | Role |
|---|---|
| `capture/` | `make fixtures`'s implementation: boots the reference Go daemon + the stubs below and records `harness/fixtures/` |
| `fixtures/` | committed goldens (see root CLAUDE.md) |
| `stubs/` | the fake agent (`fake-claude*`) and fake Linear (`linear-stub`, a real Rust crate) that every capture/e2e/test run drives against |
| `release/` | standalone bash validators for the release pipeline (PR title, `make print-version`) — **not** part of `make test` |
| `e2e/` | `boot.sh`, CI's boot gate — builds the *real* assembled `rhapsodyd` + web dashboard and drives it end-to-end |
| `workflows/` | `smoke.md` — a template WORKFLOW.md kept in sync by hand with `capture/workflows/minimal.md`; not read by any script directly (see harness/workflows/CLAUDE.md) |

## capture/ — how `make fixtures` works

`capture.sh` requires `REF` (path to the frozen, read-only Symphony v0.4.0 Go tree) — falls back
to `~/workspace/symphony-go-reference/golang/symphony` if `$REF` is unreadable (macOS TCC blocks
`~/Downloads` for daemon-spawned processes on some machines). It **never writes into `$REF`**; its
own build output and scratch dir live under `capture/{target,work}`.

Each scenario: boot `linear-stub` (from `stubs/`) + the real Go daemon against a private
`$CAPTURE_HOME`, drive one run with a `fake-claude*` stub, snapshot the API through `grab()`
(`jq -S .` then `normalize.sh`). `capture/scenarios/*.json` are the Linear-issue-state inputs;
`capture/workflows/*.md` are WORKFLOW.md templates with three sed placeholders
(`__STUB_PORT__`, `__CLAUDE_CMD__`, `__STORE_PATH__`) capture.sh fills in.

`normalize.sh` is the **single source of truth** for placeholder rewriting (`<TIMESTAMP>` `<UUID>`
`<HOME>` `<PORT>` `<NUM>`); root CLAUDE.md covers the Rust-mirror/canary-test relationship — the
actionable part here is that if you change one, you change the other in the same commit. Read the
comment block at the top of `normalize.sh` before touching a rule — it documents which fields are
deliberately *not* normalized (e.g. `_ms` config constants) so parity signal isn't erased.

Determinism contract: two `make fixtures` runs must `diff -r` empty, with one documented exception
— `fixtures/db/go-daemon.db`'s bytes vary (SQLite page layout/rowids), so determinism is instead
asserted on its `go-daemon-rows.json` normalized dump. See `capture/README.md` for the full
capture-fidelity rationale (why `VACUUM INTO` not `cp`, the single-run-snapshot race guard, the
post-flush wait before reading events).

`capture.sh` also carries the repo's first Divergence (TRA-238) as code, not just doc: after
capturing, it rewrites `~/.symphony/*` default paths to `~/.rhapsody/*` and blanks the Go
reference's otel endpoint, **only** in the `config/*.json` + `api/config.json` goldens. Every other
golden (transcript paths, `__STORE_PATH__`, `db/go-daemon-rows.json`) stays a byte-exact record of
Go's real output. If you add a new default-path divergence, it likely needs a matching `sed` here.

## stubs/ — the fake agent and fake Linear

- `fake-claude` speaks the real Claude Code stream-json JSONL protocol (see its own header comment
  for the exact contract: drains stdin continuously so the runner's held-open stdin never blocks,
  first line must carry `apiKeySource:"none"`). Controlled by env: `FAKE_CLAUDE_SLEEP_S`,
  `FAKE_CLAUDE_OUTCOME` (`success`|`error`), `FAKE_CLAUDE_HANG` (never emits a result — exercises
  the turn-timeout/kill path). `fake-claude-error` and `fake-claude-hang` are one-line wrappers
  that just set the env var and exec `fake-claude` — edit the real logic in `fake-claude` itself.
- `linear-stub` is a full Rust crate (its own `Cargo.toml`; listed directly in the root workspace
  `members`, not covered by the `crates/*` glob). It answers exactly the GraphQL operations
  enumerated from the Go reference's `query.go` (see `lib.rs`'s module doc for the full list) and
  mutates in-memory state so multi-step runs behave (issue state moves, comments, assignee). Drive
  it with `--scenario <path.json> --port N`; it prints `LISTENING <port>` once bound — every caller
  (`capture.sh`, `e2e/boot.sh`) greps stdout for that line rather than assuming a fixed port.
  Scenario JSON schema (v1) is documented in `linear-stub/src/scenario.rs`'s module doc.
  `cargo test -p linear-stub` covers both the stub's GraphQL routing and the `fake-claude*`
  protocol contract (it shells out to the sibling `stubs/fake-claude*` scripts directly).

## release/ — standalone, not under `make test`

`check-pr-title.sh` / `pr_title_test.sh` / `version_test.sh` are plain bash with no cargo
involvement; `make test` does not run them. They're invoked directly:
`.github/workflows/pr-title.yml` runs `pr_title_test.sh` (self-test) then
`check-pr-title.sh "$PR_TITLE"` (the actual gate) on every PR. Run them locally the same way,
e.g. `harness/release/pr_title_test.sh`. `version_test.sh` drives the *real* root `Makefile`'s
`print-version` target inside throwaway git repos it creates with `mktemp`/`git init` — it is not
a copy of the version-derivation logic, so it exercises the shipped `git describe` default exactly.

## e2e/ — the CI boot gate

`boot.sh` (run by `.github/workflows/ci.yml`) is the only place in this repo that builds and
drives the **actual assembled `rhapsodyd`** end-to-end (not a crate's unit tests): it builds the
web dashboard first (rust-embed is compile-time, so `crates/httpapi/web-dist` must exist before
`cargo build -p rhapsodyd`), then boots `linear-stub` + the real daemon under a private `$HOME`,
and asserts seven things in sequence — `/healthz` reachable, live `/api/v1/config` byte-matches
the committed golden, a scripted issue completes end-to-end, `/api/v1/state` is live, the embedded
dashboard serves non-empty HTML, `rhapsodyd mcp` discovers the daemon via `runtime.json` and gets a
live response, and the daemon wrote only under `~/.rhapsody` (never a legacy `~/.symphony`). If you
change a config default or the runtime-home path, this is the test that catches a live/golden
mismatch that unit tests can't see.

## workflows/smoke.md vs capture/workflows/*.md

`e2e/boot.sh` and `capture.sh` both read and sed-fill `capture/workflows/minimal.md` — not
`workflows/smoke.md`. `smoke.md` survives as the original R3 template that `minimal.md`'s own
header comment says it "mirrors," hand-kept in sync, not generated. It also names one of its three
placeholders differently: `__FAKE_CLAUDE__` where the four `capture/workflows/*.md` files use
`__CLAUDE_CMD__` (all four share `__STUB_PORT__`/`__STORE_PATH__` with it). See
`harness/workflows/CLAUDE.md` for the file itself — not repeated here.
