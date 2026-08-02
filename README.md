# Rhapsody

Rust parity port of Symphony — the daemon that reads work from Linear, creates isolated
per-issue workspaces, and runs Claude Code agents inside them. The daemon binary ships as
`rhapsodyd` — a standalone Rust daemon whose runtime behavior is a faithful clone of the Go
`symphony` daemon, with the deliberate exceptions listed under [Divergences](#divergences)
(the binary name and the runtime filesystem paths).

- Specs & plans: Linear project documents (Rhapsody project) — never committed to this repo.
- Parity reference (read-only, NOT in this repo): `$REF` (operator-provided path to the frozen
  Symphony v0.4.0 tree).
- Golden fixtures: `harness/fixtures/` — captured via `make fixtures`, asserted by every crate.

Build: `cargo build --workspace` · Test: `make test` · Lint: `make lint`

## Parity testing

Porting crates take `harness-fixtures` as a dev-dependency and assert their output equals the
committed goldens (after `normalize`). The crate exposes `load`/`load_json` (read a fixture by
path relative to `harness/fixtures/`) and `normalize`/`normalize_with_home` — a Rust mirror of
`harness/capture/normalize.sh`, kept in lockstep by a canary that runs the shell script and
requires byte-identical output. Editing, corrupting, or losing a committed golden turns
`cargo test -p harness-fixtures` red. Fixture provenance + recapture: `harness/capture/README.md`.

## Divergences

Rhapsody is a byte-for-byte parity port of Go Symphony v0.4.0 EXCEPT where this section says
otherwise. Each entry is a deliberate, reviewed decision; nothing else may drift from the frozen
reference (the parity goldens stay byte-strict).

### Runtime paths → `~/.rhapsody` + `rhapsody.db` (TRA-238)

Rhapsody gets its own runtime home. The daemon's filesystem paths and the history DB filename are
rebranded off Symphony's `~/.symphony`:

| Purpose | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| Workspace root default | `~/.symphony/symphony_workspaces` | `~/.rhapsody/workspaces` |
| Log/transcript dir default | `~/.symphony/logs` | `~/.rhapsody/logs` |
| History DB default | `~/.symphony/symphony.db` | `~/.rhapsody/rhapsody.db` |
| Runtime port file | `~/.symphony/runtime.json` | `~/.rhapsody/runtime.json` |
| Desktop supervised WORKFLOW.md | `~/.symphony/WORKFLOW.md` | `~/.rhapsody/WORKFLOW.md` |
| Repo-relative prompt defaults | `.symphony/PROMPT.md`, `.symphony/PROMPT.dep_mod.md` | `.rhapsody/PROMPT.md`, `.rhapsody/PROMPT.dep_mod.md` |

The repo-relative prompt defaults **fall back to the legacy `.symphony/` names** when the new
`.rhapsody/` path is absent from a checkout, so target repos that still ship `.symphony/PROMPT.md`
keep resolving their prompt untouched (the daemon's prompt resolver retries the `.symphony/`
counterpart before soft-falling-back to the inline prompt).

### Telemetry default → off, no bundled hub

| Default | Go v0.4.0 | Rhapsody |
|---|---|---|
| `otel.endpoint` when unset | a company-internal fleet collector | `""` (empty — no bundled hub) |
| Desktop onboarding seed | `otel.enabled: true`, export ON to that hub | `otel.enabled: false`, empty endpoint |

Rhapsody **never phones home**: the Go daemon defaulted telemetry export ON to a company-internal
collector, and the desktop onboarding seeded a fresh install to export there. Rhapsody defaults
export OFF with no endpoint; an operator opts in via the Observability toggle and supplies their own
OTLP collector. Affects the same config goldens as the path divergence above.

**Out of scope (unchanged live wire contracts):** the `SYMPHONY_RUN_ID` / `SYMPHONY_ISSUE` (and
sibling) agent env vars, the `symphony_*` MCP tool names, the `symphony/<key>` git branch prefix,
and the `@symphony` summon token — all cross-process contracts that a path rebrand must not break.

**Fixture policy:** the config goldens (`harness/fixtures/config/*.json` + `api/config.json`) encode
the daemon's resolved DEFAULTS, which now diverge. `harness/capture/capture.sh` applies a documented,
idempotent `sed` (the two default strings above) to those files after capturing from the Go daemon,
so `make fixtures` re-derives the committed state deterministically. Every other golden — including
the Go-written transcript paths in `api/history.json` + `db/go-daemon-rows.json` — stays a byte-exact
record of Go's output, and the red-on-drift canary is unchanged.

### Rotating daemon file logs in `logging.dir` (TRA-267)

The Rust daemon writes its process log as **rotating files** into the resolved `logging.dir`
(default `~/.rhapsody/logs`): daily rotation with the 7 most recent files retained (older ones
pruned), so the log is bounded and never grows without limit. This is a new file layer added
alongside — not replacing — the stderr fmt layer and the in-memory `LogBuffer` ring (the Logs tab);
it is independent of OTLP export and present whether or not telemetry is enabled. Setup is
best-effort: if the dir can't be created or the appender can't be built, the file layer is skipped
with one stderr warning and startup continues.

| Behavior | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| `logging.dir` | config-only field; no file writer (logs go to stderr / journald) | rotating file logs written here |

This makes the Settings › General "Logs path" setting real — in Go it was plumbed through config and
shown in the UI but nothing ever wrote files to it. The retention count (7) is hardcoded; no new
config field is added, keeping the config schema at parity with Go.

### `review_states` classifies a clean worker exit (TRA-279)

Go's `classifyCleanExit` never receives `review_states`. An agent that follows its prompt — open a
draft PR, move the issue to review — and then ends a turn without emitting a `HANDOFF:` marker leaves
the ticket in the configured review state, which falls through Go's branch chain to a catch-all that
records the run `stopped` / `"ticket moved externally"`. Nothing external happened, and the work
succeeded. Rhapsody threads the owning project's effective `review_states` into the classifier and
adds a branch for it.

| Clean exit, undeclared hand-off, ticket in a configured review state | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| stored outcome / error | `stopped` / `"ticket moved externally"` | `completed` / `""` |

The branch sits **after** the cancel/terminal/declared checks and **before** the catch-all, so
cancel-type and Done-type states keep their existing semantics and a move to any other non-active
state is still `"ticket moved externally"`. With `review_states` unset — the Go default — behavior is
byte-identical to the reference. No new `OUTCOME_*` constant is introduced; the missing hand-off
declaration is preserved as a `tracing::warn!` naming the run, issue and state.

### Honest history paging + store-computed dashboard aggregates (TRA-320)

Go's `handleHistory` derives `next_offset` from the limit the CALLER sent, while the store applies
`defaultRunLimit = 50` whenever the caller sends none. A request with no `limit` therefore returns a
silently truncated 50-row page **and** `next_offset: null` — the rest of the history is unreachable
without guessing a limit. Observed against a live daemon holding 192 runs: the dashboard read the
truncated page as the whole store and reported 3 jobs and 5.4M tokens today against a real 76 issues
and 53.9M.

| `GET /api/v1/history` with 192 rows stored | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| `?limit=50` | 50 rows, `next_offset: 50` | unchanged |
| *(no limit)* | 50 rows, `next_offset: null` | 50 rows, **`next_offset: 50`** |
| `?limit=500` | 192 rows, `next_offset: null` | unchanged |

`next_offset` is now computed from the page size the store ACTUALLY applied
(`rhapsody_store::effective_run_limit`, the single source of truth for the `<= 0 ⇒ default` rule).
The default limit itself is unchanged at 50 — raising it would move the truncation cliff without
removing it, and would leave `next_offset` still lying on the default path.

Two **additive** Rhapsody-only endpoints support the dashboard; no existing payload changes shape,
and the `api/history.json` golden is untouched:

| Endpoint | Serves |
| --- | --- |
| `GET /api/v1/history/issues` | one row per issue (its latest matching run), paged by **issue** |
| `GET /api/v1/history/summary?since=` | whole-store run/token/runtime totals for a window |

Both exist because the dashboard's two headline surfaces cannot be derived correctly from a
run-paged fetch at any page size. An issue-grouped Jobs list built by grouping runs lets one ticket
in a retry loop consume the entire page — 90 failures hid 73 other issues — and header totals folded
over a page report a sample as a total. Grouping and aggregation therefore happen in SQL.

The day boundary for `/history/summary` is **local, not UTC**: the caller sends its own local
midnight as `since` (the dashboard does), and omitting it falls back to the daemon host's local
midnight. This preserves the local-day semantics the client-side fold had; a UTC boundary would
silently shift every figure for anyone off UTC. `total_tokens` keeps its cache-inclusive billed
meaning, so the header's `cached = total − in − out` reconciliation still adds up.
