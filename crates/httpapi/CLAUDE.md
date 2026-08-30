# CLAUDE.md — crates/httpapi

Parity port of Go `internal/httpapi`. Read `src/lib.rs`'s top-of-file doc comment first — it names
the exact Go package and walks the H1–H3 ticket chain this crate is delivered as, one module group
per ticket. Each module's own doc comment says which Go file(s) it mirrors and which H-ticket added
it. When extending this crate, follow that same grain: add to the module that owns the matching Go
file rather than introducing a new split.

## The StateProvider trait is the whole crate's shape

`server.rs`'s `StateProvider` (mirrors Go's `StateProvider` interface) is the one surface every
handler reads/writes; it grew across H1→H3 exactly as Go's interface grows across `handlers*.go`.
Read its doc comments before adding an endpoint — they're the authoritative map of which Go file
backs which method. Two conventions repeat across every write method (stop/resume/handoff/message):

- **Business outcomes are not errors.** "Not running", "already superseded", "partial Backlog move
  failure" travel inside the `Ok(...)` result type (`StopResult`, `ResumeResult`, `HandoffResult`,
  ...) and render as 200/404/409. Only a failed *control round-trip* (the channel to the
  orchestrator itself broke) is `Err(RunActionError)`, rendered 500. Don't collapse these — a
  handler that turns a business 409 into an `Err` changes the wire contract.
- **Read handlers reuse the owning crate's renderer, never reimplement it.** `/state` calls
  `rhapsody_orchestrator::snapshot_json::render`; `/config` GET calls
  `rhapsody_config::effective_json::render`. This is the repo's byte-parity rule applied locally —
  if you're formatting a DTO by hand for one of these, check whether the source crate already owns
  that view.

The real implementor is the orchestrator, wired in by the final daemon assembly — this crate never
constructs one. Every test drives `testutil::FakeProvider` instead (the Rust analog of Go's
`fakeProvider`), built with a `with_*` builder per canned field.

## Routing: method-agnostic registration

`server::build_router` registers every route with `any(...)`, not `get(...)`/`post(...)`. Each
handler enforces its own method internally (via `require_get` or an explicit `match`) and returns a
405 envelope on mismatch. This is deliberate, not laziness: if routes were method-typed, a `GET` on
a POST-only path (e.g. `/api/v1/refresh`) would fall through to the SPA `fallback` and return a
misleading 200 HTML page instead of a 405. When adding a route, register it with `any` and add the
method guard yourself — don't reach for `axum::routing::get`.

Multi-segment patterns (`/runs/{id}/stop`, `/issues/{id}/history`, ...) don't need to be registered
before their catch-all cousin (`/runs/{id}`); axum's `matchit` resolves specificity regardless of
order, unlike Go's `ServeMux`. The ordering comments in `build_router` are informational (kept to
track parity against the Go registration list), not load-bearing.

## The embedded dashboard (`web.rs` + `web-dist/`)

`web-dist/` is **build output, not source** — only `.gitkeep` is committed (see the crate's
`.gitignore`); `web/`'s `npm run build` (root CLAUDE.md) writes the real vite bundle there via its
`outDir`. `rust-embed` embeds it at compile time, so:

- A clean checkout compiles fine (the derive only needs the folder to exist) but serves "dashboard
  not built" (500) for every path until you've run the web build once.
- CI's `web` job and `harness/e2e/boot.sh` both build the dashboard before building `rhapsodyd` —
  if you change `web/`'s output layout, this crate's embed is the thing that goes stale silently
  (it'll still compile against an old bundle).

`serve_web`/`serve_index` are generic over the embedded asset set specifically so the SPA-fallback
logic (deep-link fallback to `index.html`, `/api/*` never shadowed, static assets served with their
real MIME type) can be exercised in tests against a small committed fixture dist instead of a real
vite build — that's what `testdata/webdist/` and `testdata/emptydist/` are for; you won't need to
touch them unless you're changing the fallback contract itself.

## Golden tests (`goldens.rs`)

This crate's acceptance gate: every read handler's served JSON body, normalized, must byte-match a
committed fixture under `harness/fixtures/`. Two fixture sources feed it:

- The success-path history/run-detail/events/metrics goldens replay against the **committed**
  `harness/fixtures/db/go-daemon.db` — the real SQLite file the Go daemon wrote during capture — via
  a writable scratch copy (`Sqlite::open` rewrites the file for WAL mode, so never open the
  committed copy directly). This proves the port against real daemon output, not a hand-built stub.
- Error/stalled-run/projects/logs/transcript goldens reconstruct their scenario in-process (there's
  no capture DB for these) using `testutil`'s builders.

Running `cargo test -p rhapsody-httpapi` needs no `$REF` and touches no external daemon — it only
reads the fixtures already committed in this repo. `$REF`/`make fixtures` (root CLAUDE.md) is only
needed if you're recapturing a golden because the Go reference or a scenario changed, not for normal
iteration here. A mismatch means the port drifted — never loosen the assertion to make it pass.

## build.rs: build-identity, not codegen

`build.rs` doesn't generate code; it shells out to `git` to bake `RHAPSODY_BUILD_{COMMIT,VERSION,
TIME}` into the binary via `rustc-env`, consumed by `src/build_info.rs` for `GET /api/v1/version`
(a Rhapsody-only endpoint, no Go counterpart). Every probe is best-effort and falls back to the
literal string `"unknown"` — it must never fail the build outside a git checkout (a release tarball,
a vendored source drop). Each baked value can be overridden by an identically-named env var, which
is how a reproducible/tarball build supplies an identity `git` can't infer locally.
