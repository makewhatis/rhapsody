//! End-to-end TLS tests for the OTLP exporters: an `https://` endpoint must verify the collector
//! against the **platform trust store**, over both transports (STUDIO-1397).
//!
//! Why a real server rather than a unit assertion: the bug this pins is that the gRPC (tonic)
//! exporter built a `ClientTlsConfig::new()` with an *empty* trust store and the HTTP exporter's
//! reqwest had no TLS backend at all — so the failure only shows up when a TLS handshake is actually
//! attempted. Each test therefore stands a local TLS server up with a certificate minted by a
//! throwaway test CA, makes that CA trusted **the way the chosen mechanism reads roots**
//! (`SSL_CERT_FILE` — honored by `rustls-native-certs` on every platform and by OpenSSL, which backs
//! native-tls on Linux), and asserts the exporter connects and delivers a span.
//!
//! Platform note: the gRPC path loads roots via `rustls-native-certs`, which reads `SSL_CERT_FILE`
//! on macOS *and* Linux, so that test runs everywhere. The HTTP path loads roots via `native-tls`:
//! OpenSSL on Linux (honors `SSL_CERT_FILE`) but Security.framework on macOS (reads the keychain,
//! with no way to inject a test CA), so the HTTP test is gated to Linux — see the PR body.
//!
//! No external network: both servers bind `127.0.0.1:0` and the CA is generated in-process.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::net::TcpListener;

use opentelemetry::trace::{Tracer, TracerProvider};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
    trace_service_server::{TraceService, TraceServiceServer},
};
use opentelemetry_sdk::trace::SdkTracerProvider;
use rhapsody_telemetry::{Config, new_trace_exporter};

/// Serializes the tests that mutate the process-global `SSL_CERT_FILE` (and, transitively, the
/// `LazyLock`-cached OpenSSL probe native-tls reads once per process). Both tests take this for
/// their whole body; neither holds it across an `await` (both drive their own runtime).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // A panic in one test must not poison the other's lock — the guard only orders the two tests.
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// --- test CA + server certificate ------------------------------------------------------------------

struct TestCa {
    /// The CA certificate, PEM-encoded — what the exporter is told to trust.
    pem: String,
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

/// Mints a throwaway CA. `rcgen`'s default key type (ECDSA P-256) is what `ring`/OpenSSL both accept.
fn make_ca() -> TestCa {
    let mut params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rhapsody telemetry test CA");
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let key = rcgen::KeyPair::generate().expect("ca key");
    let cert = params.self_signed(&key).expect("self-sign ca");
    let pem = cert.pem();
    TestCa { pem, cert, key }
}

/// Mints a `127.0.0.1`/`localhost` server certificate signed by `ca`. Returns `(cert_pem, key_pem)`.
fn make_server_cert(ca: &TestCa) -> (String, String) {
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("server params");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().expect("server key");
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .expect("sign server cert");
    (cert.pem(), key.serialize_pem())
}

// --- a scratch dir that holds the CA file (no `tempfile` dev-dep; mirrors `lib.rs`'s `TempDir`) ---

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "rhapsody-telemetry-tls-{tag}-{}-{n}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Matches `lib.rs`'s test `TempDir`: an env escape hatch keeps the CA around for debugging.
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Points `SSL_CERT_FILE` at the test CA for as long as it is alive, restoring the previous value on
/// drop. This is exactly how an operator would make a private CA trusted on a Linux host, and how
/// `rustls-native-certs` (gRPC) and OpenSSL (native-tls HTTP, Linux) both discover it.
struct TrustedCaEnv {
    _dir: TempDir,
    previous: Option<std::ffi::OsString>,
}

impl TrustedCaEnv {
    fn install(ca_pem: &str) -> TrustedCaEnv {
        let dir = TempDir::new("ca");
        let path = dir.0.join("test-ca.pem");
        std::fs::write(&path, ca_pem).expect("write CA pem");
        let previous = std::env::var_os("SSL_CERT_FILE");
        // SAFETY: `set_var` is unsafe in edition 2024 because concurrent env reads/writes race. The
        // tests that call this hold `SERIAL` for their whole body, so no other test thread in this
        // binary reads the environment concurrently; `env!`-style reads in library code only happen
        // on the export path this test itself drives.
        unsafe { std::env::set_var("SSL_CERT_FILE", &path) };
        TrustedCaEnv {
            _dir: dir,
            previous,
        }
    }
}

impl Drop for TrustedCaEnv {
    fn drop(&mut self) {
        // SAFETY: see `install` — still under the `SERIAL` guard held by the calling test.
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var("SSL_CERT_FILE", v),
                None => std::env::remove_var("SSL_CERT_FILE"),
            }
        }
    }
}

// --- gRPC ------------------------------------------------------------------------------------------

/// Records the `ExportTraceServiceRequest` the exporter sends, so the test can assert on the span.
struct MockTraceService {
    tx: Sender<ExportTraceServiceRequest>,
}

#[tonic::async_trait]
impl TraceService for MockTraceService {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        let _ = self.tx.send(request.into_inner());
        Ok(tonic::Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

/// An `https://` gRPC endpoint must verify the collector against the platform trust store. On main
/// this reds: the exporter passes a bare `ClientTlsConfig::new()`, whose trust store is empty, so
/// the handshake fails with an unknown-CA error and no span is delivered.
#[test]
fn grpc_exporter_verifies_against_native_roots() {
    let _serial = serial();
    let ca = make_ca();
    let (server_cert_pem, server_key_pem) = make_server_cert(&ca);
    let _env = TrustedCaEnv::install(&ca.pem);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (addr, request_rx) = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind grpc listener");
        let addr = listener.local_addr().expect("grpc listener addr");
        let (tx, rx) = std::sync::mpsc::channel();
        let tls = tonic::transport::ServerTlsConfig::new().identity(
            tonic::transport::Identity::from_pem(server_cert_pem, server_key_pem),
        );
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .tls_config(tls)
                .expect("grpc server tls")
                .add_service(TraceServiceServer::new(MockTraceService { tx }))
                .serve_with_incoming(incoming)
                .await;
        });
        (addr, rx)
    });

    let cfg = Config {
        enabled: true,
        endpoint: format!("https://127.0.0.1:{}", addr.port()),
        protocol: "grpc".to_string(),
        insecure: false,
        ..Config::default()
    };

    runtime.block_on(async {
        let exporter = new_trace_exporter(&cfg).expect("build grpc exporter");
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        provider.tracer("tls-test").in_span("tls-span", |_| {});
        let _ = provider.force_flush();
    });

    let request = request_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("gRPC exporter did not deliver a span over https");
    let span_name = request
        .resource_spans
        .first()
        .and_then(|rs| rs.scope_spans.first())
        .and_then(|ss| ss.spans.first())
        .map(|s| s.name.clone());
    assert_eq!(span_name.as_deref(), Some("tls-span"));
}

// --- HTTP ------------------------------------------------------------------------------------------

/// Reads one HTTP/1.1 request from `stream`, returning its request line and body length.
#[cfg(target_os = "linux")]
fn read_http_request(stream: &mut impl Read) -> (String, usize) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return (String::new(), 0),
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body_len = buf.len().saturating_sub(header_end);
    (
        head.lines().next().unwrap_or_default().to_string(),
        body_len,
    )
}

/// An `https://` OTLP/HTTP endpoint must verify the collector against the platform trust store.
/// Linux-only: the HTTP path verifies through `native-tls`, which reads `SSL_CERT_FILE` via OpenSSL
/// on Linux but uses Security.framework (keychain) on macOS, where a test CA cannot be injected.
#[cfg(target_os = "linux")]
#[test]
fn http_exporter_verifies_against_native_roots() {
    let _serial = serial();
    let ca = make_ca();
    let (server_cert_pem, server_key_pem) = make_server_cert(&ca);
    let _env = TrustedCaEnv::install(&ca.pem);

    let identity =
        native_tls::Identity::from_pkcs8(server_cert_pem.as_bytes(), server_key_pem.as_bytes())
            .expect("native-tls identity");
    let acceptor = native_tls::TlsAcceptor::new(identity).expect("native-tls acceptor");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind http listener");
    let addr = listener.local_addr().expect("http listener addr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept()
            && let Ok(mut tls) = acceptor.accept(stream)
        {
            let (request_line, body_len) = read_http_request(&mut tls);
            let _ = tx.send((request_line, body_len));
            let _ = tls.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
            );
            let _ = tls.flush();
        }
    });

    let cfg = Config {
        enabled: true,
        endpoint: format!("https://127.0.0.1:{}", addr.port()),
        protocol: "http".to_string(),
        insecure: false,
        ..Config::default()
    };
    let exporter = new_trace_exporter(&cfg).expect("build http exporter");
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();
    provider.tracer("tls-test").in_span("tls-span", |_| {});
    let _ = provider.force_flush();

    let (request_line, body_len) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("HTTP exporter did not deliver a span over https");
    assert!(
        request_line.starts_with("POST /v1/traces "),
        "unexpected request line: {request_line:?}"
    );
    assert!(body_len > 0, "exported request carried an empty body");
}
