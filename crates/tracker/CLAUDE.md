# CLAUDE.md — crates/tracker

Parity port of Go `internal/tracker`. Three implementations: `fake` (in-memory test double),
`file` (JSON-backed, for Linear-free smoke tests), and `linear` (the real GraphQL adapter,
upstream §11.1-§11.4).

## Layout

- `lib.rs` — the `Tracker` trait (async, `Any + Send + Sync` so callers can downcast a
  `Box<dyn Tracker>`) and `TrackerError`.
- `factory.rs` — `Spec` (union of every call site's construction inputs) and `new()`, which
  switches on `spec.kind` ("file" vs everything else → linear, the historical default).
- `fake.rs` — programmable fields set directly by tests (`candidates`, `by_id`, `*_err` injectors);
  mutated call-count/recorded-call state lives behind one `Mutex` because trait methods take
  `&self`. Read back via accessors (`candidate_calls()`, `move_calls()`, …), not public fields.
- `file.rs` — schema types (`Doc` et al.) and the tracker share this one file (Go splits them into
  `schema.go`; not worth a second file here).
- `linear/` — the GraphQL adapter, one file per Go source file / operation group (see below). Not
  a separate crate — no manifest, just Rust module structure gated behind `mod` in `lib.rs`.
- `tests/stub_gate.rs` — the P3 phase-gate integration test; see "Stub gate" below.

## TrackerError / sentinel strings are a cross-process contract

`TrackerError::Other`/`StateNotFound` `Display` strings (`linear_state_not_found: …`) and every
`LinearErrorKind::as_str()` (`linear_api_request`, `linear_move_rejected`, …) must stay
byte-identical to the Go `errors.New(...)` messages in `errors.go` — callers (tests, the
cross-language stub, log-based debugging) match on the text. Don't reword these even when a
clearer phrasing seems obvious; adding a new sentinel is fine, changing an existing one isn't.

## `linear/` submodule map

Read the sibling file's top-of-file doc comment for its exact Go source (`client.go`,
`normalize.go`, `claim.go`, `move.go`, `errors.go`, `candidates.go`/`by_states.rs`/`by_ids.rs`/
`backlog.rs`/`projects.rs` per-operation files). Non-obvious cross-file structure:

- **`client.rs`** owns the `Client` struct, `do_graphql` transport, and three caches: resolved
  `viewer`, resolved `milestone_id`, and a `state_id_cache` (team+name → workflow-state UUID).
  The viewer/milestone caches use `tokio::sync::Mutex`, not `std::sync::Mutex` — the guard is held
  **across** the GraphQL `.await` (single-flight resolution mirroring Go's `viewerMu`/
  `milestoneMu`), which a `std` mutex guard can't do inside a `Send` async-trait future. The
  `state_id_cache` lock, by contrast, is released before the resolution query runs — don't
  "consolidate" these two patterns, they're deliberately different.
- **`normalize.rs`** converts `RawIssue` → `core::Issue`. Every plain-`String` field in every
  response struct across `linear/` must decode a JSON `null` via `decode::null_to_empty` — Linear's
  schema declares far more fields nullable than usual traffic exercises, and Go's `encoding/json`
  silently zero-values a null string where Rust's `String` rejects it outright (STUDIO-406: one
  bad field failed decoding of an entire page, silently disabling every project holding an
  in-review PR issue). When adding a new response struct field, default to
  `#[serde(deserialize_with = "super::decode::null_to_empty")]` on every `String`, not just the
  ones you've observed null.
- **`decode.rs`**'s `IssueNodes` decodes a page of issues **one node at a time**, not as
  `Vec<RawIssue>` — a deliberate, documented divergence from Go (which decodes the page as a unit).
  One malformed issue is dropped + logged (`warn_dropped`) rather than blanking every sibling on
  the same page. Identical output to Go on a well-formed payload; only kicks in where Go would also
  have failed.
- **`claim.rs`** is the INF-477 pool-mode claim protocol (`assign_issue` / `fetch_issue_assignee` /
  `create_comment` / `list_comments` / `delete_comment`). `list_comments` follows the comment
  connection's cursor to completion (bounded by `candidates::MAX_PAGES`) so a busy ticket's claim
  markers are never truncated into a wrong election winner; `hasNextPage` with an empty cursor is
  `MissingCursor`, never a silent stop.
- **`testutil.rs`** (test-only, `#[cfg(test)] mod testutil` in `mod.rs`) is a hand-rolled loopback
  GraphQL mock (`MockServer`) — the project has no `httptest`-equivalent dependency, so this reads
  raw HTTP/1.1 off a `TcpStream`. `start_with_viewer` auto-answers the `viewer {}` query so
  per-test handlers only need to cover the query under test.
- The `file` tracker satisfies the pool-mode claim methods as inert no-ops (`assign_issue` no-op,
  `create_comment` returns `file_tracker_claim_unsupported`) — pool claiming is Linear-only by
  design; don't "implement" it there.

## Testing conventions

- Every ported test has a `// Mirrors Go TestXxx` comment naming the exact Go test it reproduces.
  Keep this convention for new ported tests — it's how a reviewer confirms parity coverage without
  diffing test bodies against the Go reference.
- `#[tokio::test]` (current-thread runtime, from the `macros`+`rt` dev-features) drives every async
  `Tracker` method under test — no `#[tokio::main]`, no multi-thread runtime needed anywhere here.

## Stub gate (`tests/stub_gate.rs`)

Spawns the in-workspace `harness/stubs/linear-stub` binary against the committed
`harness/stubs/linear-stub/testdata/basic.json` scenario, points the real `linear::Client` at it,
and drives the full read surface plus a `move_issue_state` write, asserting against literal values
baked into the scenario file. It's the P3 cross-language integration gate — no `$REF`, no network,
runs in CI.

`cargo test --workspace` builds `linear-stub` as an ordinary workspace member before any test runs,
so the binary is just there. A crate-scoped `cargo test -p rhapsody-tracker` does **not** build it
(it's a `harness/` member) — `stub_binary()` detects the missing binary and shells out to
`cargo build -p linear-stub` itself as a fallback, so the crate-scoped run still works, just slower
on first invocation.
