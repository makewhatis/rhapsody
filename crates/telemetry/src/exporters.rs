//! exporters — the OTLP span/metric/log exporter constructors. Parity port of Go
//! `telemetry.newTraceExporter` / `newMetricExporter` / `newLogExporter`.
//!
//! Transport security: TLS is the default. Insecure (plaintext) transport is selected by an
//! `http://` endpoint scheme (Go's `WithInsecure`), so `cfg.insecure` picks the scheme applied to a
//! bare `host:port`. A URL endpoint (carrying an explicit scheme) is honored as-is; only the HTTP
//! exporters fill the per-signal `/v1/*` path (gRPC has no URL path), because the Rust OTLP/HTTP
//! exporter POSTs a programmatic `with_endpoint` value verbatim — the same path-less-URL gap Go's
//! `endpointURLForSignal` closes (a POST to `/` is a 404). Mirrors `$REF/internal/telemetry/telemetry.go`.
//!
//! Trust store: a secure endpoint verifies the collector against the **platform** trust store —
//! Go's default, which uses `x509.SystemCertPool`. For gRPC that means an explicit
//! [`ClientTlsConfig::with_native_roots`] (tonic's own default is a `ClientTlsConfig::new()` with an
//! *empty* store, so an `https://` endpoint fails closed until it is supplied); for OTLP/HTTP it is
//! reqwest's `native-tls` backend, enabled in `Cargo.toml`. Both read the OS roots — Security.framework
//! on macOS, OpenSSL + system ca-certificates on Linux — so a distrust update reaches the daemon
//! without a release (the 2026-10-17 eMudhra intermediates, STUDIO-1395/1397).

use std::collections::HashMap;

use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, MetricExporter, SpanExporter, WithExportConfig,
    WithHttpConfig, WithTonicConfig,
};

use crate::config::Config;
use crate::endpoint::{
    DEFAULT_LOGS_PATH, DEFAULT_METRICS_PATH, DEFAULT_TRACES_PATH, endpoint_is_url,
    endpoint_url_for_signal,
};

/// The HTTP OTLP endpoint with the per-signal `/v1/*` path filled in: a URL endpoint is defaulted
/// via [`endpoint_url_for_signal`] (Go's `WithEndpointURL`); a bare `host:port` is first given the
/// scheme `cfg.insecure` selects (`http` plaintext / `https` TLS).
fn http_endpoint(cfg: &Config, default_path: &str) -> String {
    if endpoint_is_url(&cfg.endpoint) {
        endpoint_url_for_signal(&cfg.endpoint, default_path)
    } else {
        let scheme = if cfg.insecure { "http" } else { "https" };
        endpoint_url_for_signal(&format!("{scheme}://{}", cfg.endpoint), default_path)
    }
}

/// The gRPC OTLP endpoint (no URL path): a URL is honored as-is; a bare `host:port` is given the
/// scheme `cfg.insecure` selects.
fn grpc_endpoint(cfg: &Config) -> String {
    if endpoint_is_url(&cfg.endpoint) {
        cfg.endpoint.clone()
    } else {
        let scheme = if cfg.insecure { "http" } else { "https" };
        format!("{scheme}://{}", cfg.endpoint)
    }
}

/// Whether the configured endpoint speaks TLS: an explicit `https://` URL, or a bare `host:port`
/// with `insecure` off (the scheme [`grpc_endpoint`] then prepends). An **empty** endpoint is left
/// to the SDK (which reads `OTEL_*` env) and reported as not-secure: we cannot see that scheme, and
/// eagerly loading a trust store for a possibly-plaintext endpoint would change `http://` behavior.
/// Mirrors Go, where TLS is the default unless `WithInsecure` was applied.
fn endpoint_is_secure(cfg: &Config) -> bool {
    if endpoint_is_url(&cfg.endpoint) {
        cfg.endpoint.starts_with("https://")
    } else if cfg.endpoint.is_empty() {
        false
    } else {
        !cfg.insecure
    }
}

/// The tonic TLS config for a secure endpoint, or `None` for a plaintext one.
///
/// tonic's OTLP exporter defaults to a bare `ClientTlsConfig::new()`, whose trust store is **empty**
/// — an `https://` endpoint then fails the handshake with an unknown-CA error and the bundled roots
/// are never consulted. Supplying [`ClientTlsConfig::with_native_roots`] loads the platform store
/// instead (Go's `x509.SystemCertPool` default). Applying it is also what makes the roots load, so
/// removing this call is exactly the mutation that reds the gRPC TLS test.
///
/// It is applied only for a secure endpoint: `ClientTlsConfig` loads the roots eagerly when the
/// connector is built, so passing it for a plaintext endpoint would make an `http://` exporter fail
/// to build on a host with no cert store — and `http://`/`otel.insecure` must keep working unchanged.
fn tonic_tls_config(cfg: &Config) -> Option<ClientTlsConfig> {
    endpoint_is_secure(cfg).then(|| ClientTlsConfig::new().with_native_roots())
}

/// Best-effort gRPC metadata from the header map (invalid keys/values are skipped, never panic).
/// Mirrors Go's OTLP `WithHeaders` on the gRPC exporters.
fn to_metadata(headers: &HashMap<String, String>) -> MetadataMap {
    let mut hm = http::HeaderMap::with_capacity(headers.len());
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(v),
        ) {
            hm.insert(name, value);
        }
    }
    MetadataMap::from_headers(hm)
}

/// Builds the OTLP span exporter (gRPC default, HTTP when `protocol == "http"`). Mirrors Go
/// `newTraceExporter`.
pub fn new_trace_exporter(cfg: &Config) -> Result<SpanExporter, ExporterBuildError> {
    if cfg.protocol == "http" {
        let mut b = SpanExporter::builder().with_http();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(http_endpoint(cfg, DEFAULT_TRACES_PATH));
        }
        if !cfg.headers.is_empty() {
            b = b.with_headers(cfg.headers.clone());
        }
        b.build()
    } else {
        let mut b = SpanExporter::builder().with_tonic();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(grpc_endpoint(cfg));
        }
        if let Some(tls) = tonic_tls_config(cfg) {
            b = b.with_tls_config(tls);
        }
        if !cfg.headers.is_empty() {
            b = b.with_metadata(to_metadata(&cfg.headers));
        }
        b.build()
    }
}

/// Builds the OTLP metric exporter. Mirrors Go `newMetricExporter`.
pub fn new_metric_exporter(cfg: &Config) -> Result<MetricExporter, ExporterBuildError> {
    if cfg.protocol == "http" {
        let mut b = MetricExporter::builder().with_http();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(http_endpoint(cfg, DEFAULT_METRICS_PATH));
        }
        if !cfg.headers.is_empty() {
            b = b.with_headers(cfg.headers.clone());
        }
        b.build()
    } else {
        let mut b = MetricExporter::builder().with_tonic();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(grpc_endpoint(cfg));
        }
        if let Some(tls) = tonic_tls_config(cfg) {
            b = b.with_tls_config(tls);
        }
        if !cfg.headers.is_empty() {
            b = b.with_metadata(to_metadata(&cfg.headers));
        }
        b.build()
    }
}

/// Builds the OTLP log exporter. Mirrors Go `newLogExporter`.
pub fn new_log_exporter(cfg: &Config) -> Result<LogExporter, ExporterBuildError> {
    if cfg.protocol == "http" {
        let mut b = LogExporter::builder().with_http();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(http_endpoint(cfg, DEFAULT_LOGS_PATH));
        }
        if !cfg.headers.is_empty() {
            b = b.with_headers(cfg.headers.clone());
        }
        b.build()
    } else {
        let mut b = LogExporter::builder().with_tonic();
        if !cfg.endpoint.is_empty() {
            b = b.with_endpoint(grpc_endpoint(cfg));
        }
        if let Some(tls) = tonic_tls_config(cfg) {
            b = b.with_tls_config(tls);
        }
        if !cfg.headers.is_empty() {
            b = b.with_metadata(to_metadata(&cfg.headers));
        }
        b.build()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use opentelemetry::logs::{Logger, LoggerProvider};
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry::trace::{Tracer, TracerProvider};
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::SdkTracerProvider;

    use super::*;

    fn http_cfg(endpoint: &str) -> Config {
        Config {
            enabled: true,
            endpoint: endpoint.to_string(),
            protocol: "http".to_string(),
            insecure: true,
            ..Config::default()
        }
    }

    /// A local HTTP server that records the URL path of the first request and answers `200` with an
    /// empty (valid) protobuf body — the Rust analogue of the Go test's `newPathCaptureServer`.
    fn path_capture_server() -> (String, impl Fn() -> Option<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let path: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&path);
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                if let Some(line) = req.lines().next()
                    && let Some(p) = line.split_whitespace().nth(1)
                {
                    *sink.lock().expect("lock") = Some(p.to_string());
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });
        (format!("http://{addr}"), move || {
            path.lock().expect("lock").clone()
        })
    }

    // Mirrors Go `TestExportersAcceptURLAndSecureEndpoints`: an insecure URL endpoint (loopback) and
    // a secure (TLS-default) non-loopback endpoint both construct (dialing is lazy). Runs inside a
    // tokio runtime because the gRPC (tonic) channel construction needs an ambient runtime handle —
    // production `init` is likewise called from the daemon's runtime.
    #[tokio::test]
    async fn exporters_accept_url_and_secure_endpoints() {
        let url_cfg = Config {
            enabled: true,
            endpoint: "http://localhost:4318".to_string(),
            protocol: "http".to_string(),
            insecure: true,
            ..Config::default()
        };
        new_trace_exporter(&url_cfg).expect("trace http/url");
        new_metric_exporter(&url_cfg).expect("metric http/url");
        new_log_exporter(&url_cfg).expect("log http/url");

        let secure_cfg = Config {
            enabled: true,
            endpoint: "https://otel.example.com".to_string(),
            protocol: "grpc".to_string(),
            insecure: false,
            ..Config::default()
        };
        new_trace_exporter(&secure_cfg).expect("trace grpc/secure");
        new_metric_exporter(&secure_cfg).expect("metric grpc/secure");
        new_log_exporter(&secure_cfg).expect("log grpc/secure");
    }

    // Mirrors Go `TestTraceExporterPostsToV1Traces`.
    #[test]
    fn trace_exporter_posts_to_v1_traces() {
        let (endpoint, captured) = path_capture_server();
        let exp = new_trace_exporter(&http_cfg(&endpoint)).expect("exporter");
        let tp = SdkTracerProvider::builder()
            .with_simple_exporter(exp)
            .build();
        tp.tracer("test").in_span("test-span", |_| {});
        let _ = tp.force_flush();
        assert_eq!(captured().as_deref(), Some("/v1/traces"));
    }

    // Mirrors Go `TestMetricExporterPostsToV1Metrics`.
    #[test]
    fn metric_exporter_posts_to_v1_metrics() {
        let (endpoint, captured) = path_capture_server();
        let exp = new_metric_exporter(&http_cfg(&endpoint)).expect("exporter");
        let mp = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exp).build())
            .build();
        mp.meter("test")
            .u64_counter("test_counter")
            .build()
            .add(1, &[]);
        let _ = mp.force_flush();
        assert_eq!(captured().as_deref(), Some("/v1/metrics"));
    }

    // Mirrors Go `TestLogExporterPostsToV1Logs`.
    #[test]
    fn log_exporter_posts_to_v1_logs() {
        let (endpoint, captured) = path_capture_server();
        let exp = new_log_exporter(&http_cfg(&endpoint)).expect("exporter");
        let lp = SdkLoggerProvider::builder()
            .with_simple_exporter(exp)
            .build();
        let logger = lp.logger("test");
        logger.emit(logger.create_log_record());
        let _ = lp.force_flush();
        assert_eq!(captured().as_deref(), Some("/v1/logs"));
    }
}
