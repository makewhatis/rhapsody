# CLAUDE.md — harness/stubs/linear-stub

`harness/CLAUDE.md`'s "stubs/" section and `harness/stubs/CLAUDE.md` already cover this crate's
purpose, its workspace-membership asymmetry (own `Cargo.toml`, listed directly in root `members`,
not swept by the `crates/*` glob), the `--scenario`/`--port` CLI and `LISTENING <port>` handshake,
and the enumerated GraphQL operation set (also documented in `lib.rs`'s own module doc — read that
for the per-operation field/variable contract, not restated here). This file covers what only shows
up by reading the three source files together.

## Crate name breaks the `rhapsody-<dir>` convention

Root CLAUDE.md's crate-map table implies every crate follows `rhapsody-<dir>` naming. This one
doesn't: `Cargo.toml`'s `name` is the bare `linear-stub`, not `rhapsody-linear-stub` — because it
isn't a port of a Go package at all, it's a test double. `cargo test -p linear-stub` (as
`harness/CLAUDE.md` already says) — the `-p` argument is the literal crate name, no `rhapsody-`
prefix.

## Three files, one flow

- `scenario.rs` — the v1 scenario JSON schema (`Scenario`/`Viewer`/`Project`/`Issue`) and
  `Scenario::from_path`. `description`, `labels`, `blockedBy` are optional; everything else in an
  `Issue` is required. `Issue.state` is a **display name** (`"Todo"`, `"In Progress"`, ...) that
  must match a `name` in `lib.rs`'s fixed `WORKFLOW_STATES` table — an unrecognized state string
  isn't rejected at load, it just never matches any `Candidates`/`BacklogCandidates` filter.
- `lib.rs` — `router(scenario)` builds the axum app; `graphql` extracts the operation name with a
  **naive whitespace-token scan** (`operation_name`), not a real GraphQL parser: it looks for the
  literal keyword `query`/`mutation` then takes the next token up to `(` or `{`. This works only
  because every operation in the enumerated set is named and well-formed — an anonymous or
  oddly-formatted query document silently falls through to the "unknown operation" GraphQL-error
  branch rather than erroring at parse time.
- `main.rs` — arg parsing and the bind/announce/serve sequence. `parse_args` only accepts
  `--scenario` and `--port`; any other flag (including a bare positional arg) hits `bail!`
  immediately — there's no arg it silently ignores.

## `StubState` mutation model

Issue-state changes (`MoveIssueState`) mutate `scenario.issues[i].state` in place, but comments and
assignees are **not** stored on the `Issue` struct — they live in separate `HashMap<issue_id, _>`
fields (`comments`, `assignees`) on `StubState` and are joined back onto issue nodes at read time
(`assignee_node`, `comment_nodes`). If you add a new mutation, decide up front whether it belongs on
`scenario.issues` directly (visible to every node builder for free) or as a side-table joined in
each relevant builder — the two existing mutations chose differently for a reason (state is a plain
field every shape already threads through; comments/assignee have shape-dependent rendering, see
`comment_nodes(..., with_id: bool)` — `IssueComments` gets `id body createdAt`, the candidate-node
embed gets `createdAt body` with no `id`, matching each real query's field selection).

The lock is `Arc<Mutex<StubState>>` with `unwrap_or_else(PoisonError::into_inner)` — a panic inside
one request handler poisons the mutex but does **not** fail subsequent requests; the next handler
just recovers the (possibly mid-mutation) state and carries on. Don't rely on a panicking handler
to fail the run — it degrades silently instead.

## Fixed, single-team world

`TEAM_ID` (`"team_stub"`) is a constant every issue node reports regardless of scenario content —
there's no way to script a multi-team scenario; `TeamWorkflowStates` always returns the same
`WORKFLOW_STATES` table no matter what `teamID` is requested. `WORKFLOW_STATES` ids
(`state_backlog`, `state_todo`, ...) are stable literals that `MoveIssueState`'s `stateId` round-trips
through to resolve a `name` — if you add a workflow state, its `id` needs to be a new stable literal
too, not a synthesized one, since nothing else in the stub reads Linear's real state ids.

`blocked_by_nodes` resolves each `blockedBy` entry against `scenario.issues` by **either** `id` or
`identifier`; an entry matching neither is echoed back as a `blocks` edge with that literal string
as both id/identifier and an empty state name, rather than erroring — a typo'd `blockedBy` reference
in a scenario file fails silently as an unresolvable-looking edge, not a load error.

`synth_timestamp` produces deterministic, strictly-increasing RFC3339 stamps from a per-process
comment counter (`2020-01-01T00:00:0N` and up) — not wall-clock time — so fixture captures involving
`CreateComment` are reproducible across runs.

## Tests live in two `#[cfg(test)]` modules with different concerns

- `fake_claude_tests` — invokes the **sibling** scripts directly via
  `concat!(env!("CARGO_MANIFEST_DIR"), "/../fake-claude")` (and `-error`). This means
  `cargo test -p linear-stub` depends on `harness/stubs/`'s layout staying intact (see
  `harness/stubs/CLAUDE.md`'s deployment-unit note) — moving `linear-stub/` without its siblings, or
  renaming `fake-claude`, breaks these tests at run time with a "no such file" `Command` failure, not
  a compile error.
- `stub_tests` — hardcodes the **real** GraphQL operation documents as `const` strings, lifted
  verbatim from the Go reference's `query.go` (per the file comment). These are hand-copied, not
  generated or shared with any other source of truth — if the Go reference's query shape changes,
  these consts (and the crate's response builders) need a manual, matching update, and nothing will
  flag the drift except a fixture-capture mismatch downstream. Requests are posted with a spawned
  `curl` subprocess (`gql()`), a deliberate zero-extra-dev-dependency choice — there's no
  `reqwest`/http-client dev-dependency to reach for here even though the crate itself depends on
  `axum`.

`testdata/basic.json` is this crate's **own** minimal scenario for its unit tests (single issue,
single project) — distinct from `harness/capture/scenarios/*.json`, which are the larger scenarios
`make fixtures` drives against the real Go daemon. Don't confuse the two when adding a test case;
extending `testdata/basic.json` only affects `cargo test -p linear-stub`.
