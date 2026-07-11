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
pub fn init<W>(cfg: &Config, stderr: W) -> Telemetry
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    // The in-memory ring backs the desktop app's Logs tab; it always fans alongside stderr so the
    // daemon process log is viewable whether or not OTLP export is enabled.
    let logs = LogBuffer::new(LOG_BUFFER_CAP, Level::INFO);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(stderr);

    if !cfg.enabled {
        return noop(stderr_layer, logs);
    }

    // Exporter construction is lazy (dialing happens later), but if ANY fails, disable export and
    // fall back to the stderr + ring no-op — telemetry must never block startup.
    let (trace_exp, metric_exp, log_exp) = match (
        new_trace_exporter(cfg),
        new_metric_exporter(cfg),
        new_log_exporter(cfg),
    ) {
        (Ok(t), Ok(m), Ok(l)) => (t, m, l),
        _ => return noop(stderr_layer, logs),
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
    // The composed subscriber: stderr (fmt), the in-memory ring, the OTLP span bridge
    // (`tracing-opentelemetry`), and the OTLP log bridge (`opentelemetry-appender-tracing`).
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
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
    }
}

/// The disabled/failed no-op: a subscriber of just stderr + the ring, a no-op metrics recorder, and
/// a no-op shutdown. Mirrors Go `telemetry.noop`.
fn noop<L>(stderr_layer: L, logs: LogBuffer) -> Telemetry
where
    L: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
{
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(logs.clone()),
    );
    Telemetry {
        metrics: Metrics::noop(),
        logs,
        dispatch,
        shutdown: Box::new(|| {}),
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
        let tel = init(&Config::default(), buf.clone()); // enabled = false
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
        let tel = init(&cfg, buf.clone());
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
        let tel = init(&cfg, buf.clone());
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
