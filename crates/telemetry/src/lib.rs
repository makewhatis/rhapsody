//! rhapsody-telemetry — parity port of Go `internal/telemetry`.
//!
//! Wires optional OpenTelemetry export (traces/metrics/logs) for Symphony. When disabled it is a
//! complete no-op that still logs to stderr and into the in-memory ring backing the desktop app's
//! Logs tab — it NEVER returns an error that should stop the daemon. Mirrors `$REF/internal/telemetry/`.

pub mod config;
pub mod endpoint;
pub mod exporters;
pub mod logbuffer;
pub mod metrics;
pub mod operator;
pub mod resource;

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use opentelemetry::global;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::{Dispatch, Level};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

pub use config::Config;
pub use endpoint::{endpoint_is_url, endpoint_url_for_signal};
pub use exporters::{new_log_exporter, new_metric_exporter, new_trace_exporter};
pub use logbuffer::{LogBuffer, LogEntry};
pub use metrics::Metrics;
pub use operator::{derive_operator, resolve_operator};
pub use resource::build_resource;

use logbuffer::LOG_BUFFER_CAP;

/// Bounds telemetry provider shutdown (the final export flush) so an unreachable collector can't
/// stall daemon exit (INF-473). Kept comfortably under the daemon's own shutdown budget. Mirrors Go
/// `shutdownTimeout`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The live telemetry handles Symphony records against. Mirrors Go `telemetry.Telemetry`: a metrics
/// recorder, the in-memory log ring for the Logs tab, and the composed `tracing` subscriber the
/// daemon installs (fmt→stderr + the ring + — when enabled — the OTLP span/log bridges). Spans come
/// from `tracing` macros against that subscriber, so there is no separate tracer handle.
pub struct Telemetry {
    /// The metrics recorder (a no-op when disabled/failed).
    pub metrics: Metrics,
    /// The in-memory process-log ring (always present, independent of OTLP export).
    pub logs: LogBuffer,
    dispatch: Dispatch,
    shutdown: Box<dyn Fn() + Send + Sync>,
    /// The non-blocking rolling-file writer's flush guard (TRA-267). Held for the whole process
    /// lifetime — the `tracing-appender` worker stops flushing to disk the moment this drops, so it
    /// lives as long as `Telemetry`. `None` when file logging is off (no `log_dir`) or its setup
    /// failed (dir uncreatable / appender unbuildable), matching the OTLP exporters' clean fallback.
    _log_guard: Option<WorkerGuard>,
}

impl Telemetry {
    /// The composed `tracing` subscriber. The daemon installs it
    /// (`tracing::subscriber::set_global_default`); tests drive it via `with_default`.
    pub fn subscriber(&self) -> Dispatch {
        self.dispatch.clone()
    }

    /// Flushes + stops exporters, bounded by [`SHUTDOWN_TIMEOUT`] so an unreachable collector can't
    /// stall exit (no-op when disabled). Mirrors Go `Telemetry.Shutdown`.
    pub fn shutdown(&self) {
        (self.shutdown)();
    }
}

/// Builds telemetry. When `cfg.enabled` is false — or exporter construction fails — it returns a
/// no-op `Telemetry` that still logs to stderr and into the in-memory ring; it NEVER returns an
/// error that should stop the daemon. Mirrors Go `telemetry.Init`. `stderr` is any `MakeWriter`
/// (the daemon passes `std::io::stderr`; tests pass a capture buffer).
///
/// `log_dir` is the resolved `logging.dir` (TRA-238's `~/.rhapsody/logs` default). When `Some`, the
/// daemon's process log is ALSO written as rotating files there (daily rotation, 7 files retained) —
/// present on both the enabled and disabled paths, exactly like the in-memory ring, since file
/// logging is independent of OTLP export. This is NEW behavior: Go v0.4.0 keeps `logging.dir`
/// config-only with no file writer (a documented divergence — see the README). File-log setup is
/// best-effort: if the dir can't be created or the appender can't be built, the file layer is skipped
/// with one stderr warning — it must NEVER block or crash startup (the OTLP exporters' contract).
///
/// When enabled with the gRPC transport (the default), this MUST be called from within a tokio
/// runtime — the tonic exporter's channel construction needs an ambient runtime handle (the daemon
/// boots telemetry from its async `main`, so this holds). The disabled and HTTP paths do not require
/// a runtime.
pub fn init<W>(cfg: &Config, log_dir: Option<&Path>, stderr: W) -> Telemetry
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    // The in-memory ring backs the desktop app's Logs tab; it always fans alongside stderr so the
    // daemon process log is viewable whether or not OTLP export is enabled.
    let logs = LogBuffer::new(LOG_BUFFER_CAP, Level::INFO);
    // The rolling-file layer (best-effort; `None` when off/failed). Built BEFORE `stderr` is consumed
    // by the fmt layer, so its setup warning can still fan to the same stream. The file layer is
    // boxed as `Layer<Registry>` and so must be added to the registry FIRST (below); the stderr fmt
    // layer stays generic over the subscriber type, so it composes after it.
    let (file_layer, log_guard) = build_file_layer(log_dir, &stderr);

    if !cfg.enabled {
        return noop(file_layer, stderr, logs, log_guard);
    }

    // Exporter construction is lazy (dialing happens later), but if ANY fails, disable export and
    // fall back to the stderr + ring no-op — telemetry must never block startup.
    let (trace_exp, metric_exp, log_exp) = match (
        new_trace_exporter(cfg),
        new_metric_exporter(cfg),
        new_log_exporter(cfg),
    ) {
        (Ok(t), Ok(m), Ok(l)) => (t, m, l),
        _ => return noop(file_layer, stderr, logs, log_guard),
    };

    let resource = build_resource(cfg);
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(trace_exp)
        .with_resource(resource.clone())
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metric_exp).build())
        .with_resource(resource.clone())
        .build();
    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exp)
        .with_resource(resource)
        .build();

    // Publish the global providers + a W3C(+baggage) propagator so packages that resolve telemetry
    // via `opentelemetry::global` (rather than by injection) see the SAME providers (Go's
    // `otel.SetTracerProvider` / `SetMeterProvider` / `SetTextMapPropagator`). Set ONLY on the
    // enabled path, preserving the clean-no-op contract when disabled.
    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());
    global::set_text_map_propagator(opentelemetry::propagation::TextMapCompositePropagator::new(
        vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ],
    ));

    let metrics = Metrics::new(&meter_provider.meter("symphony"));
    // The composed subscriber: the rolling-file layer (TRA-267), stderr (fmt), the in-memory ring,
    // the OTLP span bridge (`tracing-opentelemetry`), and the OTLP log bridge
    // (`opentelemetry-appender-tracing`). The file layer is added first so it composes as
    // `Layer<Registry>` regardless of the layers stacked after it.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(stderr);
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(file_layer)
            .with(stderr_layer)
            .with(logs.clone())
            .with(tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("symphony")))
            .with(OpenTelemetryTracingBridge::new(&logger_provider)),
    );

    let shutdown = make_shutdown(tracer_provider, meter_provider, logger_provider);
    Telemetry {
        metrics,
        logs,
        dispatch,
        shutdown,
        _log_guard: log_guard,
    }
}

/// Builds the rolling-file logging layer for `log_dir` (TRA-267). Best-effort, mirroring the OTLP
/// exporters' clean fallback: returns `(None, None)` — and writes ONE warning to `stderr` — when
/// there is no dir, the dir can't be created, or the appender can't be built, so file logging never
/// blocks or crashes startup. On success returns the boxed `fmt` layer plus the non-blocking writer's
/// [`WorkerGuard`], which the caller stores on [`Telemetry`] for the process lifetime (dropping it
/// silently stops disk flushing).
///
/// Daily rotation with `max_log_files(7)` gives rotation AND truncation (the oldest files are pruned)
/// with no unbounded growth and — deliberately — no new config field, keeping the schema at parity
/// with Go (the retention count is hardcoded). `with_ansi(false)` keeps the on-disk log free of the
/// terminal color escapes the stderr layer may emit.
fn build_file_layer<W>(
    log_dir: Option<&Path>,
    stderr: &W,
) -> (
    Option<Box<dyn Layer<Registry> + Send + Sync>>,
    Option<WorkerGuard>,
)
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let Some(dir) = log_dir else {
        return (None, None);
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        let _ = writeln!(
            stderr.make_writer(),
            "symphony: cannot create log dir {}: {e}; file logging disabled",
            dir.display()
        );
        return (None, None);
    }
    let appender = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("rhapsodyd")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
    {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(
                stderr.make_writer(),
                "symphony: cannot open rolling log appender in {}: {e}; file logging disabled",
                dir.display()
            );
            return (None, None);
        }
    };
    // Non-blocking so file IO never blocks the logging hot path; the returned guard MUST outlive the
    // process (held on `Telemetry`) or the writer silently stops flushing.
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .boxed();
    (Some(layer), Some(guard))
}

/// The disabled/failed no-op: a subscriber of the rolling-file layer (TRA-267) + stderr + the ring, a
/// no-op metrics recorder, and a no-op shutdown. File logging is independent of OTLP export, so the
/// file layer (and its guard) rides the no-op path exactly like the in-memory ring. Mirrors Go
/// `telemetry.noop`.
fn noop<W>(
    file_layer: Option<Box<dyn Layer<Registry> + Send + Sync>>,
    stderr: W,
    logs: LogBuffer,
    log_guard: Option<WorkerGuard>,
) -> Telemetry
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    // The file layer is boxed as `Layer<Registry>`, so it goes onto the bare registry FIRST; the
    // stderr fmt layer (generic over the subscriber type) composes after it.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(stderr);
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(file_layer)
            .with(stderr_layer)
            .with(logs.clone()),
    );
    Telemetry {
        metrics: Metrics::noop(),
        logs,
        dispatch,
        shutdown: Box::new(|| {}),
        _log_guard: log_guard,
    }
}

/// Bounds provider shutdown at [`SHUTDOWN_TIMEOUT`]: the three provider shutdowns run on a worker
/// thread (entering the daemon's tokio runtime so the gRPC exporter's flush has a reactor), and the
/// caller waits at most `SHUTDOWN_TIMEOUT` for them — so an unreachable, tailnet-only collector
/// (off-tailnet / CI) can't hang exit on the OTLP client's much longer retry budget (INF-473).
fn make_shutdown(
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
) -> Box<dyn Fn() + Send + Sync> {
    let handle = tokio::runtime::Handle::try_current().ok();
    Box::new(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let (tp, mp, lp) = (
            tracer_provider.clone(),
            meter_provider.clone(),
            logger_provider.clone(),
        );
        let handle = handle.clone();
        std::thread::spawn(move || {
            let _guard = handle.as_ref().map(|h| h.enter());
            let _ = tp.shutdown();
            let _ = mp.shutdown();
            let _ = lp.shutdown();
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(SHUTDOWN_TIMEOUT);
    })
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use tracing::dispatcher::with_default;

    use super::*;

    /// A shared in-memory `MakeWriter` capturing the stderr fan (Go passes a `bytes.Buffer`).
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> SharedBuf {
            SharedBuf(Arc::new(Mutex::new(Vec::new())))
        }
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'w> MakeWriter<'w> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'w self) -> Self::Writer {
            self.clone()
        }
    }

    // Mirrors Go `TestInitDisabledIsNoop`: disabled telemetry still provides a working metrics
    // recorder + log ring, recording is safe, and the logger still writes to stderr; shutdown is a
    // no-op.
    #[test]
    fn init_disabled_is_noop() {
        let buf = SharedBuf::new();
        let tel = init(&Config::default(), None, buf.clone()); // enabled = false
        with_default(&tel.subscriber(), || {
            let span = tracing::info_span!("x");
            let _e = span.enter();
            tel.metrics.dispatched(&[]); // safe on the no-op recorder
            tracing::info!("hello");
        });
        assert!(
            buf.contents().contains("hello"),
            "disabled logger must still write to stderr: {:?}",
            buf.contents()
        );
        tel.shutdown();
    }

    /// A unique temp directory, removed on drop — the workspace idiom (hand-rolled, no `tempfile`
    /// dev-dep; mirrors `rhapsodyd`'s `testutil::TempDir`).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rhapsody-telemetry-{tag}-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // TRA-267: `init` with a `log_dir` writes the daemon process log as a rotating file there — the
    // new behavior making the "Logs path" setting real (Go v0.4.0 leaves `logging.dir` config-only).
    // Present on the disabled path too, since file logging is independent of OTLP export.
    #[test]
    fn writes_rotating_file_log_to_log_dir() {
        let dir = TempDir::new("filelog");
        let buf = SharedBuf::new();
        let tel = init(&Config::default(), Some(&dir.0), buf.clone()); // enabled = false
        with_default(&tel.subscriber(), || {
            tracing::info!("file-sink-line");
        });
        // Non-blocking writer: dropping `tel` drops the WorkerGuard, which flushes the queued event
        // and joins the writer thread — so the file is complete before we read (no fixed sleep).
        drop(tel);

        let contents = read_log_file(&dir.0).expect("a rhapsodyd*.log file must exist in log_dir");
        assert!(
            contents.contains("file-sink-line"),
            "rolling file log must contain the emitted line, got: {contents:?}"
        );
    }

    /// Reads the single `rhapsodyd*.log` rolling file in `dir` (the daily appender names it
    /// `rhapsodyd.<date>.log`). Returns `None` when no such file exists yet.
    fn read_log_file(dir: &Path) -> Option<String> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rhapsodyd") && name.ends_with("log") {
                return std::fs::read_to_string(entry.path()).ok();
            }
        }
        None
    }

    // Mirrors Go `TestInitEnabledBuildsProviders`: a dead local gRPC endpoint — dialing is lazy, so
    // init must succeed and the logger still fans to stderr; shutdown is bounded.
    #[tokio::test]
    async fn init_enabled_builds_providers() {
        let buf = SharedBuf::new();
        let cfg = Config {
            enabled: true,
            endpoint: "127.0.0.1:4317".to_string(),
            protocol: "grpc".to_string(),
            service_name: "symphony-test".to_string(),
            ..Config::default()
        };
        let tel = init(&cfg, None, buf.clone());
        with_default(&tel.subscriber(), || tracing::info!("enabled-log-line"));
        assert!(
            buf.contents().contains("enabled-log-line"),
            "enabled logger must still fan out to stderr"
        );
        tel.shutdown();
    }

    // Mirrors Go `TestShutdownBoundedWhenCollectorUnreachable` (INF-473): a black-hole collector
    // (accepts TCP, never responds) must not stall daemon exit — the internal SHUTDOWN_TIMEOUT cap,
    // not the caller, bounds the flush.
    #[tokio::test]
    async fn shutdown_bounded_when_collector_unreachable() {
        // A black-hole collector: accept connections but never respond, so each export blocks
        // reading the reply (deterministic, no real network).
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept() {
                held.push(conn); // hold open, never respond
            }
        });

        let buf = SharedBuf::new();
        let cfg = Config {
            enabled: true,
            endpoint: format!("http://{addr}"),
            protocol: "http".to_string(),
            insecure: true,
            service_name: "symphony-test".to_string(),
            ..Config::default()
        };
        let tel = init(&cfg, None, buf.clone());
        // Give each provider something to flush at shutdown.
        with_default(&tel.subscriber(), || {
            let span = tracing::info_span!("x");
            let _e = span.enter();
            tel.metrics.dispatched(&[]);
            tracing::info!("flush-me");
        });

        let start = Instant::now();
        tel.shutdown();
        let elapsed = start.elapsed();
        assert!(
            elapsed <= SHUTDOWN_TIMEOUT + Duration::from_secs(3),
            "shutdown took {elapsed:?}, want ≲ {SHUTDOWN_TIMEOUT:?} — cap not applied"
        );
    }
}
