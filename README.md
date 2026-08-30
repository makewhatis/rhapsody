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
STUDIO-603 later ALIASED most of these (see below); none was removed.

**Fixture policy:** the config goldens (`harness/fixtures/config/*.json` + `api/config.json`) encode
the daemon's resolved DEFAULTS, which now diverge. `harness/capture/capture.sh` applies a documented,
idempotent `sed` (the two default strings above) to those files after capturing from the Go daemon,
so `make fixtures` re-derives the committed state deterministically. Every other golden — including
the Go-written transcript paths in `api/history.json` + `db/go-daemon-rows.json` — stays a byte-exact
record of Go's output, and the red-on-drift canary is unchanged.

### Both brand spellings accepted on every contract (STUDIO-603)

Every name that crosses a process boundary now accepts a `rhapsody` spelling **as well as** the
`symphony` one. This is strictly additive — nothing was removed, and no existing config, hook, or
prompt changes behavior. Deprecation and removal is a later ticket.

| Contract | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| Agent "me" identity env | `SYMPHONY_ISSUE` / `SYMPHONY_RUN_ID` | **both**, plus `RHAPSODY_ISSUE` / `RHAPSODY_RUN_ID` |
| Lifecycle-hook env | `SYMPHONY_REPO` / `_PROJECT` / `_ISSUE` | **both**, plus the `RHAPSODY_*` trio |
| Agent-facing MCP tools | 11 × `symphony_*` | **both**, plus 11 aliases of the same handlers: `rhapsody_*`, except `symphony_send_message`, whose alias is the semantic `agent_send_message` |
| Summon token matching | the one configured token | the configured token; either brand token accepts **both** |

The MCP aliases are derived from the router AFTER the `cfg.mcp` gating removals, so a disabled write
tool has no alias either — the opt-in gate cannot be walked around by spelling the tool the other
way. On the read side, `rhapsodyd mcp` resolves its "me" defaults from either prefix.

The summon pair is symmetric and narrow: configuring **either** `@symphony` or `@rhapsody` accepts
both, so the shipped default answers to the new name and no in-flight `@symphony` comment is missed.
A token that is neither brand (e.g. `@bot`) is matched VERBATIM and is never expanded — an operator
who narrowed the token did so precisely so the daemon would not answer to another bot's mentions.

**Deliberately unchanged (still `symphony`, by decision):**

- The merged MCP **server key** in `.symphony-mcp.json`, which determines the agent's tool namespace
  (`mcp__symphony__*`). A second server entry would duplicate every tool; renaming the key would
  break any prompt naming `mcp__symphony__symphony_handoff`, including `.rhapsody/PROMPT.md`. The
  approach is proposed in the STUDIO-603 PR body rather than picked silently.
- `summon_token` (`@symphony`) and `otel.service_name` (`symphony`) as **resolved `decode` defaults**
  — both appear in the `api/config.json` + `config/*.json` goldens captured from the frozen Go
  daemon, so they are frozen by PARITY, not merely by compatibility. What a NEW user receives is
  fixed at the seed instead: the desktop onboarding writes `summon_token: '@rhapsody'` and
  `service_name: rhapsody` explicitly into the initial WORKFLOW.md, and the summon matcher accepts
  both spellings regardless of which default resolved.

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

### Reportable build identity — `GET /api/v1/version` (STUDIO-380)

The daemon answers `/state` with `status: ok` regardless of how old the binary is, so "Rhapsody is
running" and "Rhapsody is current" were indistinguishable from the outside. A daemon ran for a month
on a build that predated eight merges — including the TRA-279 fix above, which it had built for
itself — and the drift surfaced only when someone hand-audited runs and found successful ones
recorded `stopped` / `"ticket moved externally"`.

| Question | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| "which build is this daemon?" | unanswerable — no endpoint, no version in any payload | `GET /api/v1/version` |

```json
{ "version": "v0.3.1-8-g581e281", "commit": "581e281…", "built_at": "2026-08-13T16:10:35Z" }
```

Baked in at compile time by `crates/httpapi/build.rs`. Every probe is best-effort and reports the
`"unknown"` sentinel rather than failing the build, so the crate still compiles outside a git
checkout; `RHAPSODY_BUILD_{COMMIT,VERSION,TIME}` and `SOURCE_DATE_EPOCH` override the probes for a
reproducible or source-tarball build.

This is an **additive endpoint, deliberately not a field on `/api/v1/state`**. `/state` is a byte-parity
port of Go `toStateJSON` pinned to the committed `api/state.json` golden, and that golden is
recaptured from the frozen Go daemon — which will never emit a build identity. A field there could be
made green only by hand-editing the fixture or loosening the assertion, both of which are drift
laundering. A separate route leaves every existing payload and golden untouched, following the
precedent TRA-320 set. No existing payload changes shape.

The dashboard footer reports the daemon stamp alongside the desktop shell's own (`appVersion()`),
collapsing to one line when they match and showing both when they diverge — they are separate
binaries and the sidecar can drift from the shell. Because it is served over the loopback API rather
than the Tauri bridge, the stamp now also renders in a plain browser, where the footer previously
showed nothing.

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

### Rhapsody Teams — an optional feature with no Go counterpart (STUDIO-639 … STUDIO-645)

Teams gives a daemon named identities with shared profiles and per-identity memory. The frozen Go
reference has none of it, so nothing here is a *difference* in ported behaviour — it is new surface,
and it is listed for the same reason `GET /api/v1/version` is: it adds `/api/v1` routes and MCP
tools that a reader comparing the two daemons will not find upstream.

**The whole feature is off by default and off is the shipped state.** `~/.rhapsody/teams.yaml` is
absent on a fresh install, absence means `enabled: false`, and nothing ever creates it — unlike
`capabilities.yaml`, which is seeded on first read. With Teams off:

| Surface | Off behaviour |
| --- | --- |
| `WORKFLOW.md` front matter | no new field — Teams is not a `WORKFLOW.md` key at all |
| `GET /api/v1/config`, `/projects`, `/state` | no new key; every committed golden untouched |
| `rhapsody.db` | no column, no table, no new row *kind* |
| Turn-1 prompt | byte-identical (the empty-guard BO-12 proved for `capabilities_section`) |
| Dispatch | `route()` is not called; the same issues dispatch in the same order |
| MCP `list_tools` | byte-identical — the `teams_*` routes are **removed**, not disabled |
| Filesystem | nothing created: no `teams.yaml`, no `teams/profiles/`, no `teams/banks/` |

Four **additive** Rhapsody-only endpoints back the memory tools; no existing payload changes shape
and no golden moves. Each answers `409 teams_disabled` when Teams is off:

| Endpoint | Serves |
| --- | --- |
| `GET /api/v1/teams/roster` | the roster, each identity's profile, and its live runs |
| `GET /api/v1/teams/recall?identity=&query=` | one identity's retained memory, bounded |
| `POST /api/v1/teams/invalidate` | mark one record non-valid, with the reason; reversible |
| `POST /api/v1/runs/{id}/retain` | record what a live run learned, provenance stamped by the host |

The matching MCP tools are `teams_roster`, `teams_recall`, `teams_invalidate` and `teams_retain`.
`teams_retain` takes `content` and nothing else on purpose: the identity, ticket, run and commit are
resolved by the daemon from the run id it injected into that worker, so a run dispatched as one
identity cannot write into another's memory bank.

Memory is a pluggable backend (`none` / `local`, with `hindsight` reserved). `local` is the default
because it works on a laptop with no cloud: append-only markdown records, one file per record, under
`~/.rhapsody/teams/banks/<name>/`, in files a human can read and correct. The bank directory appears
on the first retain and at no other time.

**Two cross-process contracts stay on the Go spelling and are not divergences:** the git branch
prefix is `symphony/<key>` and the agent env vars are `SYMPHONY_*`. Both are read by things outside
this repo.

### A reopening summons reaches the run it triggers (STUDIO-649)

Go delivers a summon comment's TEXT to exactly one place: a run that is already alive when the
comment lands (`deliverMidRunSummons`, INF-448). That router requires the summons to be strictly
newer than the run's start — but a summons that *reopens* a review-state ticket is, by construction,
older than the run it starts, so it is skipped forever. `promoteAndDispatch` then dispatched a fresh
run carrying the prompt and the ticket description and nothing else, and the reviewer's instructions
were dropped precisely when they mattered most.

| Summons on a ticket with no live run | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| reopen fires (ticket promoted, fresh run dispatched) | yes | yes (unchanged) |
| the summon comment's body reaches that run | **no — discarded** | seeded into the run's operator mailbox |

Rhapsody's reopen dispatch path seeds the new run's mailbox with `Issue.latest_summon_body` through
the *same* INF-250 admission path the mid-run route uses (`deliverToMailbox`): the wrapped body on
the bounded mailbox, the reviewer's original words persisted as a `run_messages` row. A body-less
summons seeds the same generic fallback nudge the mid-run route uses. The per-run
`last_delivered_summon_at` watermark is advanced on success, so the two routes agree the summons is
spent and neither can deliver it twice.

Nothing else moves: the reopen *gate* (INF-448) is untouched, the turn-1 prompt is byte-identical
(no template change), only the newest summons is delivered (the same contract as mid-run), and no
config field, endpoint or golden is added or changed.
