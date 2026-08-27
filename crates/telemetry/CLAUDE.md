# CLAUDE.md — crates/telemetry

Parity port of Go `internal/telemetry`. Mirrors `$REF/internal/telemetry/` — check each file's
top comment for the exact Go source file it ports before changing behavior.

Unlike most porting crates, this one does **not** go through the `harness-fixtures` golden-file
model (there's no wire format to diff — telemetry is fire-and-forget OTLP export). Parity here is
enforced by hand-written tests mirroring the named Go `Test*` functions (see each file's test
module); porting a Go test means adding/updating the matching Rust `#[test]`, not touching a
golden.

## The never-fail contract

Everything in this crate answers to one invariant, stated in `lib.rs`'s module doc: `init` **must
never** return an error or block startup, regardless of `cfg.enabled` or whether the collector is
reachable. Concretely:

- Exporter construction failure (any of trace/metric/log) silently falls back to `noop()` — it
  does not partially wire providers.
- File-log setup failure (dir uncreatable, appender unbuildable) logs one stderr warning and
  proceeds without a file layer — never panics, never blocks.
- `Telemetry::shutdown()` is bounded by `SHUTDOWN_TIMEOUT` (2s) via a worker thread + a
  `recv_timeout` on a channel — an unreachable collector cannot hang daemon exit (INF-473). If you
  add a new provider to `init`, add its shutdown to `make_shutdown` and make sure it's covered by
  the same bound, not called synchronously on the caller's thread.

When changing `init`, preserve this shape: try the risky thing, and on any failure fall through to
the same `noop()` path the `enabled: false` case uses — don't special-case new failure modes.

## Layer composition order matters

`init`'s `tracing_subscriber::registry()` stack (and `noop`'s smaller one) adds `file_layer`
**first**, then the stderr fmt layer, then the ring, then (enabled path only) the OTLP span/log
bridges. This order is load-bearing, not stylistic: `file_layer` is boxed as
`Box<dyn Layer<Registry> + Send + Sync>` (built once in `build_file_layer`, shared by both the
enabled and noop paths), so it must land on the bare `Registry` before any generic-over-subscriber
layer stacks on top. Reordering breaks the type composition, not just log ordering.

The `WorkerGuard` returned alongside `file_layer` (`_log_guard` on `Telemetry`) must be held for
the process lifetime — dropping it silently stops the non-blocking file writer from flushing, with
no error anywhere. `writes_rotating_file_log_to_log_dir` in `lib.rs` catches regressions here by
asserting the file is non-empty only *after* dropping `Telemetry` (relying on the guard's drop to
flush), not on a sleep.

## Module map (read together, not in isolation)

- `config.rs` — `Config`, the T1→F1 contract built from the `config` crate's `otel:` YAML block.
  Pure data; no logic to port-check against Go beyond field names.
- `endpoint.rs` + `exporters.rs` — endpoint scheme/path resolution is split from exporter
  construction because gRPC and HTTP disagree on what "endpoint" means: gRPC never touches a URL
  path (`grpc_endpoint`), HTTP always fills the per-signal default (`/v1/traces` etc.) via
  `endpoint_url_for_signal` when the configured endpoint carries none — this is the fix for a real
  404 bug in the upstream SDK's path-less-URL handling, so don't "simplify" it back to a bare
  `with_endpoint`.
- `resource.rs` + `operator.rs` — the shared OTLP `Resource` (service.name/host.name/operator).
  `operator.rs::hostname()` is `pub(crate)` specifically so `resource.rs` can reuse it for
  `host.name` — the same syscall backs two different resource attributes; don't duplicate it.
- `metrics.rs` — **bounded cardinality is a hard contract**, stated in the file's module doc:
  metric attributes are restricted to `ATTR_PROJECT`/`ATTR_MODEL`/`ATTR_OUTCOME`/`ATTR_REASON`
  only. Never add an issue/run/session identifier as a metric attribute (unbounded cardinality
  blows up the collector) — identifiers belong on spans/logs, not metrics. This is enforced by
  convention/review, not the type system, so watch for it in review.
- `logbuffer.rs` — the in-memory ring backing the desktop Logs tab, implemented as a
  `tracing_subscriber::Layer`. Spans double as slog's `WithGroup`: a span's fields are captured in
  `on_new_span` and folded into every event within it, dotted-path-prefixed
  (`poll.component`, not `component`). If you add a span around new code that shouldn't leak into
  logged attrs, know this layer will still capture and prefix it.
- `lib.rs` — wires all of the above into `Telemetry`; see sections above.

## Test idioms specific to this crate

- No `tempfile` dev-dependency — `lib.rs`'s test module hand-rolls a `TempDir` (PID + atomic
  counter suffix, `remove_dir_all` on drop), matching the `rhapsodyd` binary's own
  `testutil::TempDir`. Reuse that pattern rather than adding the crate.
- `exporters.rs` tests spin a raw `TcpListener` to capture the request path a real OTLP/HTTP POST
  hits (`path_capture_server`), and `lib.rs`'s shutdown test uses a listener that accepts but never
  responds (a "black-hole collector") to exercise the shutdown bound deterministically — no real
  network, no fixed sleeps.
- `metrics.rs` tests collect through `InMemoryMetricExporter` + `force_flush`, the Rust analogue of
  Go's `ManualReader.Collect`.
- Tests that build a gRPC (tonic) exporter must run inside a tokio runtime (`#[tokio::test]`) —
  channel construction needs an ambient runtime handle, same requirement `init` documents for
  production callers.
