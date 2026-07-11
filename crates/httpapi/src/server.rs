//! server — the loopback server core: the [`StateProvider`] interface, the mux/route table, and the
//! [`Server`] listener wrapper. Parity port of `$REF/internal/httpapi/server.go`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::handler::Handler;
use axum::routing::any;
use rhapsody_orchestrator::Snapshot;

use crate::handlers::{handle_healthz, handle_state};
use crate::web::{WebDist, serve_web};

/// The orchestrator surface the HTTP layer reads. Mirrors Go's `StateProvider` interface
/// (`$REF/internal/httpapi/handlers.go`), narrowed to the H1 slice: only [`StateProvider::snapshot`],
/// backing `GET /api/v1/state`. Later H-lane tickets extend this trait as their handlers land (Go
/// grows the one interface across `handlers*.go`): H2 adds the read surfaces
/// (`Store`/history/projects/linear), H3 the writes (`Refresh`/`StopRun`/`ResumeRun`/
/// `SendRunMessage`/`ValidateConfig`).
///
/// The real implementor is the orchestrator, whose async `snapshot` (the control-task channel
/// round-trip) lands with the control loop (O7) and is wired as the live provider by the final
/// assembly (F1) — the analog of Go's `var _ StateProvider = (*orchestrator.Orchestrator)(nil)`
/// compile-time check. H1 tests against a fake, exactly as Go's `server_test.go` uses `fakeProvider`.
#[async_trait]
pub trait StateProvider: Send + Sync {
    /// The synchronous runtime view served at `/api/v1/state` (Go `Snapshot(ctx)`). An `Err` renders
    /// as a 503 `snapshot_unavailable` envelope; the HTTP layer bounds the wait (the state handler's
    /// `SNAPSHOT_TIMEOUT`, mirroring Go's request-scoped `snapshotTimeout`).
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError>;
}

/// Why a snapshot could not be produced. The HTTP layer renders ANY snapshot failure as a 503
/// `snapshot_unavailable` envelope — mirroring Go, whose handler maps both `ErrSnapshotUnavailable`
/// and `ErrSnapshotTimeout` to the same body (code `snapshot_unavailable`, message = the error
/// string). The concrete round-trip that distinguishes those (the control-task channel + the
/// `ErrSnapshotTimeout` deadline) lands with the orchestrator control loop (O7); this newtype carries
/// the observable message contract now.
#[derive(Debug, Clone)]
pub struct SnapshotError(String);

impl SnapshotError {
    /// Construct a snapshot error with a human `message` (rendered as the 503 body's `message`).
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SnapshotError {}

/// Build the API + embedded-dashboard router with `web_fallback` mounted as the SPA catch-all
/// (consulted LAST), so it never shadows an `/api` route. Generic over the fallback so production
/// (`WebDist`) and the tests' committed dist share one code path. Mirrors Go `NewHandler`'s mux
/// assembly.
///
/// Routes are registered method-AGNOSTICALLY (`any`) and each handler enforces its own method (a 405
/// envelope on mismatch), so the SPA fallback never turns a method mismatch into a 404 — the reason
/// Go registers its `/api` routes without a `"GET "` method prefix (`server.go` `NewHandler` note).
///
/// The full Go mux registers, in order (more-specific multi-segment patterns first); H1 owns the two
/// marked ✓ and later H-lane tickets slot the rest in here:
/// ```text
///   /healthz                       (H1) ✓     /api/v1/runs/{id}/events      (H2)
///   /api/v1/state                  (H1) ✓     /api/v1/runs/{id}/transcript  (H2)
///   /api/v1/refresh                (H3)        /api/v1/runs/{id}/stop        (H3)
///   /api/v1/config                 (H3)        /api/v1/runs/{id}/resume      (H3)
///   /api/v1/linear/projects        (H2)        /api/v1/runs/{id}/message     (H3)
///   /api/v1/linear/identity        (H2)        /api/v1/runs/{id}/messages    (H2)
///   /api/v1/projects               (H2)        /api/v1/issues/{id}/history   (H2)
///   /api/v1/history                (H2)        /api/v1/logs/stream           (H2)
///   /api/v1/events                 (H2)        /api/v1/logs                  (H2)
///   /api/v1/metrics                (H2)        /api/v1/runs/{id}             (H2)
/// ```
/// Unlike Go's `ServeMux`, axum's matchit resolves pattern specificity itself, so the Go registration
/// ORDER is informational (it does not affect matching) — the paths + method semantics are the contract.
pub(crate) fn build_router<H, T>(provider: Arc<dyn StateProvider>, web_fallback: H) -> Router
where
    H: Handler<T, Arc<dyn StateProvider>>,
    T: 'static,
{
    Router::new()
        .route("/healthz", any(handle_healthz))
        .route("/api/v1/state", any(handle_state))
        // SPA catch-all — the embedded React dashboard. As the router fallback it is consulted LAST
        // (never shadows an /api route) and 404s any /api path defensively (see `web::serve_web`). Go
        // mounts this on "/"; axum models the same "everything else" match as the fallback.
        .fallback(web_fallback)
        .with_state(provider)
}

/// Build the API + embedded-dashboard handler over the production dashboard embed (`web-dist/`).
/// Mirrors Go `NewHandler(p, logs, logger)` — the `logs` process-log ring + `logger` arrive with the
/// log endpoints (H2/H3); H1's handlers need only the provider.
///
/// Deferred (serial-chain): Go wraps the mux in `otelhttp.NewHandler(mux, "symphony.http")` for
/// per-request server spans/metrics. That wrap needs the telemetry providers, which land with the
/// telemetry lane (T1) and are initialized by the final assembly (F1); it is transparent to routing,
/// so H1 returns the bare router and F1 layers telemetry on.
pub fn new_handler(provider: Arc<dyn StateProvider>) -> Router {
    build_router(provider, serve_web::<WebDist>)
}

/// The loopback HTTP server wrapping [`new_handler`]. Mirrors Go `httpapi.Server` (`server.go`): bind
/// a loopback listener (port 0 for an ephemeral port), then [`Server::serve`] until shutdown.
pub struct Server {
    listener: tokio::net::TcpListener,
    router: Router,
}

impl Server {
    /// Bind a loopback listener on `addr` (use `127.0.0.1:0` for an ephemeral port) and build the
    /// handler for `provider`. Mirrors Go `New` (`net.Listen("tcp", addr)` + `NewHandler`).
    ///
    /// The `WriteTimeout`/`IdleTimeout` Go intentionally omits (loopback, small responses, plus the
    /// SSE `/logs/stream` endpoint that must not be write-deadline-cut) have no axum knob to set;
    /// hyper's built-in header-read guard covers the slowloris case Go handles via `ReadHeaderTimeout`.
    pub async fn bind(provider: Arc<dyn StateProvider>, addr: &str) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            router: new_handler(provider),
        })
    }

    /// The actual bound address (useful when the port was 0). Mirrors Go `Addr`.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve requests until the serving task is dropped. Mirrors Go `Serve`.
    pub async fn serve(self) -> std::io::Result<()> {
        axum::serve(self.listener, self.router).await
    }

    /// Serve requests until `shutdown` resolves, then drain in-flight requests (graceful). The Rust
    /// idiom for Go's `Server.Shutdown(ctx)`: hand the stop signal to `axum::serve`.
    pub async fn serve_with_shutdown(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::*;
    use crate::testutil::{FakeProvider, empty_snapshot};

    // The loopback Server binds an ephemeral port, serves /healthz, and shuts down gracefully when
    // signaled (exercises bind / local_addr / serve_with_shutdown together).
    #[tokio::test]
    async fn server_binds_serves_and_shuts_down() {
        let server = Server::bind(Arc::new(FakeProvider::ok(empty_snapshot())), "127.0.0.1:0")
            .await
            .expect("bind");
        let addr = server.local_addr().expect("local addr");
        assert!(addr.ip().is_loopback(), "must bind loopback, got {addr}");

        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            server
                .serve_with_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .expect("serve");
        });

        let resp = reqwest::get(format!("http://{addr}/healthz"))
            .await
            .expect("GET /healthz");
        assert_eq!(resp.status(), 200);

        // Signal shutdown and confirm the server task drains and returns.
        let _ = tx.send(());
        handle.await.expect("server task join");
    }

    // The plain `serve` (no shutdown signal, mirroring Go `Serve`) binds + routes correctly: a
    // spawned server answers /healthz. It runs until dropped, so the test aborts the task to stop it.
    #[tokio::test]
    async fn server_serve_answers_requests() {
        let server = Server::bind(Arc::new(FakeProvider::ok(empty_snapshot())), "127.0.0.1:0")
            .await
            .expect("bind");
        let addr = server.local_addr().expect("local addr");
        let handle = tokio::spawn(async move { server.serve().await });

        let resp = reqwest::get(format!("http://{addr}/healthz"))
            .await
            .expect("GET /healthz");
        assert_eq!(resp.status(), 200);

        handle.abort();
    }
}
