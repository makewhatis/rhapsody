# CLAUDE.md — crates/mcp

Ports Go's `internal/mcpfacade` (the `symphony mcp` subcommand, INF-473) over the daemon's loopback
`/api/v1`. It reads nothing from `~/.rhapsody` or the DB except `runtime.json`, and only for port
discovery — the daemon stays the single source of truth for everything else.

## File map (read in this order for a new tool or a bug)

- `client.rs` — `Client`: the loopback HTTP client (base URL + 15s-timeout `reqwest::Client`) and
  the minimal wire structs (`StateResp`, `RunDetail`, `IssueHistoryResp`, …). Deliberately
  **rmcp-free** — no `rmcp`/`tool` imports here. `FacadeError { code, message, status }` is the one
  error type everything below converges on.
- `discovery.rs` — `resolve_daemon_port`: reads `~/.rhapsody/runtime.json` and prefers its
  published port only when the writing PID is still alive (`kill(pid, 0)`); otherwise falls back to
  `Config.server.port`. The *only* filesystem access in the crate.
- `verdict.rs` — `verdict()`: a pure function (no I/O, no async) that reduces `{running-row,
  run-detail}` into one `Status` (`alive|stalled|completed|failed|interrupted|not-dispatched`).
  Read this file's taxonomy comments before touching `status.rs` — the precedence rules (terminal
  vs. running, same-run-id gating, `interrupted` exemption) are non-obvious and are the load-bearing
  logic of the whole crate.
- `status.rs` — `Client::run_status`: composes `verdict.rs`'s pure function from live HTTP calls
  (`/state`, `/runs/{id}`, `/issues/{id}/history`).
- `server.rs` — `Facade`: the seven always-on read tools (`symphony_state/_runs/_run/_ticket/
  _logs/_events/_run_status`) plus the shared helpers (`text_result`, `err_result`, `or_default`,
  `path_escape`, `encode_query`) that `writes.rs` reuses.
- `writes.rs` — the config-gated write tools (`symphony_send_message`, `_stop`, `_resume`,
  `_handoff`), registered via a second `#[tool_router(router = write_router)]` on the same `Facade`
  impl, then merged and pruned in `Facade::new`.
- `testutil.rs` (`cfg(test)` only) — `spawn_router` (axum stub on an ephemeral port),
  `client_for_port`, `test_config`. Shared by every module's unit tests.
- `tests/fixtures_stub.rs` — the crate's one integration test: drives the tools through an
  in-memory `rmcp` client against a stub serving the committed `harness/fixtures/api/*.json`
  goldens, asserting each read tool proxies its fixture **verbatim**.

## Conventions specific to this crate

- **Errors never become protocol errors.** Every tool handler catches its `FacadeError` and returns
  it as an `IsError` `CallToolResult` (`err_result`), never a raw `Err` bubbled through `rmcp` — the
  model has to be able to see and self-correct on a `daemon_unreachable`/`not_running`/etc code, not
  get a JSON-RPC fault. Preserve this pattern in any new tool.
- **Write-tool gating removes the route, it doesn't guard the handler.** `Facade::new` builds the
  full merged router then calls `tool_router.remove_route(name)` for each disabled `cfg.mcp.allow_*`
  toggle. A disabled tool is absent from `list_tools` and rejected on call — there is no
  runtime "permission denied" branch inside a write handler to imitate when adding a new gated tool.
- **`client.rs` must stay `rmcp`-free.** The HTTP/JSON layer and the tool-registration layer are
  intentionally split (called out in writes.rs as "M1's layering") so the client is testable and
  reusable without pulling in the MCP SDK. Don't add `rmcp` types to `client.rs` or `status.rs`.
- **Path segments use Go's `url.PathEscape` byte-for-byte, not Rust's usual percent-encoding.**
  `server::path_escape` keeps `$&+:=@` unescaped (Go's segment-safe sub-delims) — a naive
  `percent_encode` crate default would escape those and diverge from the Go reference's request
  paths. Likewise `encode_query` mirrors `url.Values.Encode` (sorted keys, space → `+`), not a
  generic query-builder.
- **`verdict.rs`'s not-dispatched reason is never fabricated.** An unresolvable case must produce
  the literal string `"unknown — check daemon logs"`, not a best-guess. This is a deliberate
  parity+honesty boundary (see the module doc comment), not an oversight to "improve."
- **Two 404 codes both mean "no persisted detail row," not "surface an error":** `not_found` (id 0 /
  persistence disabled) and `run_not_found` (a valid id absent from both the live snapshot and the
  store). `status.rs`'s `is_not_found` must keep recognizing both — any *other* error code (5xx,
  `daemon_unreachable`, a third code) must propagate, never get masked into a stale "alive".
- **`symphony_handoff` (TRA-242) has no Go reference analog.** Unlike every other tool here, it is
  genuinely new functionality (gated `cfg.mcp.allow_handoff`, default **on**), not a port. Don't go
  looking for `handoff.go` in `$REF` — there isn't one; it proxies the daemon's own
  `POST /api/v1/runs/{id}/handoff` via the same `run_action` helper `symphony_stop`/`_resume` use.
- **"Me" defaulting has an explicit-beats-env precedence rule that's easy to get backwards.**
  `symphony_run_status` only falls back to `Options.default_run_id` / `default_issue`
  (SYMPHONY_RUN_ID / SYMPHONY_ISSUE) when the caller passes **neither** `run_id` nor `issue`. An
  explicitly-passed `issue` must win even when a `default_run_id` is also set — see the
  `run_status_explicit_issue_beats_env_run_id` test in `server.rs` before changing this branch.
