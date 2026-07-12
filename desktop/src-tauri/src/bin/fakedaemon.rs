//! fakedaemon — a test stand-in for `rhapsodyd` used by the supervisor lifecycle tests. Parity port
//! of `$REF/desktop/internal/supervisor/testdata/fakedaemon/main.go`.
//!
//! It mimics the parts the supervisor cares about — a `--port` flag, a `/healthz` route, graceful
//! SIGTERM shutdown — and can be told (via env vars) to delay readiness, exit unexpectedly, or crash
//! on its first launch so the restart-on-crash path can be exercised. It has no tauri/lib dependency
//! and is located by the integration tests via `CARGO_BIN_EXE_fakedaemon`.

use std::convert::Infallible;
use std::path::Path;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Reads a millisecond duration from `key` (0 when unset/invalid). Mirrors Go `envDurationMS`.
fn env_duration_ms(key: &str) -> Duration {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<u64>()
            .map(Duration::from_millis)
            .unwrap_or(Duration::ZERO),
        Err(_) => Duration::ZERO,
    }
}

/// Parses the `--port N` / `--port=N` flag from argv (positional workflow args are ignored).
fn parse_port() -> u16 {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--port" {
            if let Some(v) = args.next() {
                return v.parse().unwrap_or(0);
            }
        } else if let Some(v) = a.strip_prefix("--port=") {
            return v.parse().unwrap_or(0);
        }
    }
    0
}

fn ok_json() -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from_static(b"{\"status\":\"ok\"}")));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = status;
    r
}

/// `/healthz` answers 200 once `ready_delay` has elapsed since start, 503 before then; every other
/// path is 404. Mirrors the fakedaemon's mux.
fn handle(
    req: &Request<Incoming>,
    elapsed: Duration,
    ready_delay: Duration,
) -> Response<Full<Bytes>> {
    if req.uri().path() == "/healthz" {
        if elapsed < ready_delay {
            return empty(StatusCode::SERVICE_UNAVAILABLE);
        }
        return ok_json();
    }
    empty(StatusCode::NOT_FOUND)
}

/// Completes on SIGTERM or SIGINT (the daemon's graceful-shutdown trigger).
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[tokio::main]
async fn main() {
    let port = parse_port();

    // crash-once: if the marker file is absent, create it and exit non-zero immediately. The
    // supervisor relaunches with the same argv, so the second run finds the marker and serves
    // normally — exercising restart-on-crash.
    if let Ok(marker) = std::env::var("FAKE_CRASH_MARKER")
        && !marker.is_empty()
        && !Path::new(&marker).exists()
    {
        let _ = std::fs::write(&marker, b"crashed");
        eprintln!("fakedaemon: simulated crash on first launch");
        std::process::exit(3);
    }

    let ready_delay = env_duration_ms("FAKE_READY_DELAY_MS");
    let started = Instant::now();

    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fakedaemon: bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    // Parity with the real daemon's readiness log line (handy when debugging tests).
    eprintln!("fakedaemon: observability server listening on {addr}");

    // Simulate an unexpected clean exit after the daemon has been up a while, when configured.
    let exit_after = env_duration_ms("FAKE_EXIT_AFTER_MS");

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let exit_timer = async {
        if exit_after > Duration::ZERO {
            tokio::time::sleep(exit_after).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(exit_timer);

    loop {
        tokio::select! {
            _ = &mut shutdown => break, // SIGTERM/SIGINT -> graceful exit (drops in-flight conns)
            _ = &mut exit_timer => std::process::exit(0),
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((s, _)) => s,
                    Err(_) => continue,
                };
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| async move {
                        Ok::<_, Infallible>(handle(&req, started.elapsed(), ready_delay))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        }
    }
}
