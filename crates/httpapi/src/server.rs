//! server — the loopback server core: the [`StateProvider`] interface, the mux/route table, and the
//! [`Server`] listener wrapper. Parity port of `$REF/internal/httpapi/server.go`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::FromRef;
use axum::handler::Handler;
use axum::routing::any;
use rhapsody_config::ValidationError;
use rhapsody_config::workflow::Definition;
use rhapsody_orchestrator::teamsmemory::{
    InvalidateView, PostView, RecallView, RetainView, RoomView, RosterView, TeamsMemoryError,
};
use rhapsody_orchestrator::{
    HandoffResult, Identity, ReadsError, RefreshResult, ResumeResult, RunMessageResult, Snapshot,
    StopResult,
};

use crate::handlers::{handle_healthz, handle_refresh, handle_state, handle_version};
use crate::handlers_config::{handle_capabilities, handle_config};
use crate::handlers_history::{
    handle_event_search, handle_history, handle_history_summary, handle_issue_history,
    handle_issue_runs, handle_metrics, handle_run_detail, handle_run_events, handle_run_transcript,
};
use crate::handlers_linear::{handle_linear_identity, handle_linear_projects};
use crate::handlers_logs::{handle_log_stream, handle_logs};
use crate::handlers_message::{handle_run_message, handle_run_messages};
use crate::handlers_projects::handle_projects;
use crate::handlers_runaction::{handle_run_handoff, handle_run_resume, handle_run_stop};
use crate::handlers_teams::{
    handle_run_post, handle_run_retain, handle_teams_invalidate, handle_teams_recall,
    handle_teams_room, handle_teams_roster,
};
use crate::history::HistoryStore;
use crate::logs::LogSource;
use crate::web::{WebDist, serve_web};

/// The orchestrator surface the HTTP layer reads + writes. Mirrors Go's `StateProvider` interface
/// (`$REF/internal/httpapi/handlers.go`), grown across the H-lane exactly as Go grows its one
/// interface across `handlers*.go`: H1's [`snapshot`](StateProvider::snapshot) (`GET /api/v1/state`),
/// H2's read surfaces ([`history`](StateProvider::history)/[`run_transcript`](StateProvider::run_transcript)/
/// [`list_linear_projects`](StateProvider::list_linear_projects)/[`connected_viewer`](StateProvider::connected_viewer)),
/// and H3's writes ([`refresh`](StateProvider::refresh), [`stop_run`](StateProvider::stop_run)/
/// [`resume_run`](StateProvider::resume_run), [`send_run_message`](StateProvider::send_run_message),
/// and [`workflow_path`](StateProvider::workflow_path) + [`validate_config`](StateProvider::validate_config)
/// for the config endpoint).
///
/// The real implementor is the orchestrator, wired as the live provider by the final assembly (F1) —
/// the analog of Go's `var _ StateProvider = (*orchestrator.Orchestrator)(nil)` compile-time check.
/// Every handler tests against a fake, exactly as Go's `server_test.go` uses `fakeProvider`.
#[async_trait]
pub trait StateProvider: Send + Sync {
    /// The synchronous runtime view served at `/api/v1/state` (Go `Snapshot(ctx)`). An `Err` renders
    /// as a 503 `snapshot_unavailable` envelope; the HTTP layer bounds the wait (the state handler's
    /// `SNAPSHOT_TIMEOUT`, mirroring Go's request-scoped `snapshotTimeout`).
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError>;

    /// The read-only history store backing `/history`, `/issues/{id}/history`, `/runs/{id}` (+
    /// `/events`), `/events`, and `/metrics`. Never absent: a daemon with persistence disabled returns
    /// a [`rhapsody_store::Noop`] whose reads yield empty lists (so history degrades to `[]`, never a
    /// 500). Mirrors Go `StateProvider.Store()`, narrowed to the read subset via [`HistoryStore`]
    /// (Go's `api.History()`).
    fn history(&self) -> Arc<dyn HistoryStore>;

    /// The humanized per-run transcript for a run id (the concrete `*.jsonl` recorded on the run row),
    /// feeding `GET /api/v1/runs/{id}/transcript`. `None` ⇒ no such run row (→ 404); `Some(entries)`
    /// ⇒ a found run (its transcript file missing/pruned yields an empty `entries`, → 200 `entries:[]`).
    /// Mirrors Go `StateProvider.RunTranscript`, whose `([]agent.LogEntry, bool)` return collapses to
    /// this `Option`.
    fn run_transcript(&self, run_id: i64) -> Option<Vec<rhapsody_agent::LogEntry>>;

    /// The workspace's Linear projects for the add-agent picker (`GET /api/v1/linear/projects`).
    /// [`ReadsError::ConfigNotLoaded`] (before the first config load) maps to 503 `config_not_loaded`;
    /// any other error to 502 `linear_unavailable`. Mirrors Go `StateProvider.ListLinearProjects`.
    /// (No request deadline is threaded in: the Rust orchestrator's reads deliberately dropped Go's
    /// `context.Context` — cancellation is task-abort — see `orchestrator::reads`.)
    async fn list_linear_projects(&self) -> Result<Vec<rhapsody_core::Project>, ReadsError>;

    /// The connected-as Linear identity (`GET /api/v1/linear/identity`). Best-effort: the [`Identity`]
    /// is ALWAYS meaningful (masked token even on failure) and the `Option<String>` is the resolution
    /// error surfaced only for logging — the endpoint always answers 200. Mirrors Go
    /// `StateProvider.ConnectedViewer`, whose `(Identity, error)` return becomes this `(Identity,
    /// Option<String>)` (the same best-effort split `orchestrator::connected_viewer` already uses).
    async fn connected_viewer(&self) -> (Identity, Option<String>);

    /// Kill the agent for `run_id` and move its ticket to Backlog (`POST /api/v1/runs/{id}/stop`,
    /// Go `StopRun`). A *business* outcome — the run isn't running (→ 409), or it was killed but the
    /// Backlog move failed (a partial success: 200 with `move_error`) — travels in the returned
    /// [`StopResult`]; only a failed control round-trip is an [`Err`] (→ 500 `stop_failed`). No
    /// request deadline is threaded in: like the read surfaces, the Rust port drops Go's
    /// `context.Context` (the real adapter supplies the control task's `CancelWait`).
    async fn stop_run(&self, run_id: i64) -> Result<StopResult, RunActionError>;

    /// Move a stopped run's ticket back to Todo so the daemon re-dispatches it
    /// (`POST /api/v1/runs/{id}/resume`, Go `ResumeRun`). Business outcomes (not found → 404;
    /// not-stopped / live-run / superseded / no-team → 409; partial move failure → 200 with
    /// `move_error`) travel in the [`ResumeResult`]; only a failed control round-trip is an [`Err`]
    /// (→ 500 `resume_failed`).
    async fn resume_run(&self, run_id: i64) -> Result<ResumeResult, RunActionError>;

    /// Move a live run's ticket to the configured review handoff state so it leaves the active set and
    /// the run cleanly ends (`POST /api/v1/runs/{id}/handoff`, TRA-242 — the daemon-mediated review
    /// handoff, NEW beyond Go v0.4.0). Business outcomes travel in the [`HandoffResult`]: no live run
    /// (`not_running` → 409), review handoff not configured (`not_configured` → 409), or the tracker
    /// rejected the move (`move_err` → 502 — a handoff FAILURE, not a partial success like stop/resume,
    /// so the agent falls back to the Linear-MCP path). A clean move is 200 `{identifier, moved_to}`.
    /// Only a failed control round-trip is an [`Err`] (→ 500 `handoff_failed`). Unlike stop it does NOT
    /// kill the agent (it is the agent's own terminal action) — the ticket move alone winds the run down.
    async fn handoff_run(&self, run_id: i64) -> Result<HandoffResult, RunActionError>;

    /// Queue an operator "btw" message for a live run's agent (`POST /api/v1/runs/{id}/message`,
    /// Go `SendRunMessage`, INF-250). The `text` is already trimmed + length-checked by the handler.
    /// Unlike stop/resume there is no error return: the Rust orchestrator collapses "the run's
    /// control loop is gone" into `not_running` (→ 409), and a full mailbox into `full` (→ 409
    /// `backlog_full`); a clean accept carries the inserted `id` + `identifier` (→ 202). Mirrors the
    /// O6 `ControlHandle::send_run_message` surface this forwards to.
    async fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult;

    /// Request a coalesced poll+reconcile tick (`POST /api/v1/refresh`, Go `Refresh`). Synchronous +
    /// infallible like Go's non-blocking channel send: the returned [`RefreshResult`] reports whether
    /// the tick was `queued` or `coalesced` into a pending one, so the handler always answers 202.
    fn refresh(&self) -> RefreshResult;

    /// The absolute path of the WORKFLOW.md this daemon loads + watches, so `/api/v1/config` can read
    /// it (GET) and atomically rewrite it (POST); the fsnotify watcher then hot-reloads the change.
    /// Mirrors Go `WorkflowPath`.
    fn workflow_path(&self) -> &str;

    /// Run the daemon's load-time validation on a candidate `def` WITHOUT applying it, so
    /// `POST /api/v1/config` rejects exactly what a hot-reload would reject. `Ok(())` ⇒ the config
    /// would load cleanly; an `Err` carries a classifiable [`ConfigValidateError`] (the typed path
    /// maps it to a field code, the legacy path surfaces it as `invalid_config`). Mirrors Go
    /// `ValidateConfig` (Decode → Resolve → ValidateDispatch → buildEffective).
    fn validate_config(&self, def: &Definition) -> Result<(), ConfigValidateError>;

    /// The agent-capabilities registry (`GET /api/v1/capabilities`), so a UI can render the opt-in
    /// checkbox list without hardcoding the options. `Some(registry)` ⇒ the daemon's loaded registry;
    /// `None` ⇒ no registry loaded yet, which the handler serves as an empty `[]`. Rhapsody-only (no
    /// Go v0.4.0 counterpart — this whole endpoint is a Rhapsody addition).
    fn capabilities_registry(&self) -> Option<Vec<rhapsody_config::capabilities::CapabilityDef>>;

    /// The Teams roster with each identity's derived status (`GET /api/v1/teams/roster`,
    /// STUDIO-645). Rhapsody-only, like [`capabilities_registry`](StateProvider::capabilities_registry)
    /// — no Go v0.4.0 counterpart.
    ///
    /// The four `teams_*` methods default to [`TeamsMemoryError::Disabled`] so a provider that
    /// predates Teams — the parity fake, and anything embedding this crate for the Go-shaped API —
    /// behaves EXACTLY as a Teams-off daemon: `teams_disabled` on every route, nothing created,
    /// nothing changed. Only a provider that actually has a Teams runtime overrides them.
    async fn teams_roster(&self) -> Result<RosterView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }

    /// One identity's recalled memory for a free-text query (`GET /api/v1/teams/recall`).
    async fn teams_recall(
        &self,
        _identity: &str,
        _query: &str,
    ) -> Result<RecallView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }

    /// The newest posts in the team room (`GET /api/v1/teams/room`, STUDIO-650 T5). Read-only:
    /// serving it advances no identity's cursor.
    async fn teams_room(&self, _limit: usize) -> Result<RoomView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }

    /// Mark one record non-valid, with its reason (`POST /api/v1/teams/invalidate`, §5.3).
    async fn teams_invalidate(
        &self,
        _identity: &str,
        _fact_id: &str,
        _reason: &str,
    ) -> Result<InvalidateView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }

    /// Record what a live run learned, with the provenance stamped by the HOST from the run id —
    /// never from the request body (`POST /api/v1/runs/{id}/retain`, §5.1).
    async fn teams_retain(
        &self,
        _run_id: i64,
        _content: &str,
    ) -> Result<RetainView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }

    /// Post a message to the team room as a live run (`POST /api/v1/runs/{id}/post`, STUDIO-653 T6;
    /// §0.5, §0.11.4). Like retain, run-scoped in its PATH: the host resolves the run to the
    /// identity it was dispatched as and stamps that as `from`, so there is no route by which an
    /// agent can name itself. `to` empty or `*` is the room; any other value must name a roster
    /// member. The room append is the post; the timeline row and any direct-to-live delivery are
    /// best-effort mirrors reported in the view.
    async fn teams_post(
        &self,
        _run_id: i64,
        _body: &str,
        _to: &str,
        _refs: &[String],
    ) -> Result<PostView, TeamsMemoryError> {
        Err(TeamsMemoryError::Disabled)
    }
}

/// Why a candidate config would not load (the `Err` of [`StateProvider::validate_config`]). The
/// [`ConfigValidateError::Validation`] variant carries the config crate's structured
/// [`ValidationError`] so the typed config POST can map it to a stable field code + path (Go
/// `classifyConfigError`); every other pipeline failure (decode / resolve / buildEffective) is an
/// opaque [`ConfigValidateError::Other`] surfaced as `invalid_config`. `Display` is the message the
/// handler echoes — byte-identical to the config crate's error string, which the P1 validate tests
/// already pin.
#[derive(Debug)]
pub enum ConfigValidateError {
    /// A `ValidateDispatch` rejection with a stable variant the config POST maps to a field code.
    Validation(ValidationError),
    /// Any other load-pipeline failure (decode / resolve / buildEffective) → `invalid_config`.
    Other(String),
}

impl std::fmt::Display for ConfigValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigValidateError::Validation(err) => err.fmt(f),
            ConfigValidateError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigValidateError {}

/// Why a run action (stop/resume) could not be attempted at all — the control round-trip itself
/// failed (the second return of Go's `StopRun`/`ResumeRun`). The handler renders it as a 500
/// (`stop_failed` / `resume_failed`). A *business* outcome (not running, not found, a partial
/// Backlog/Todo move failure) is NOT an error — it is carried in the `StopResult`/`ResumeResult`
/// value and can still yield a 200/404/409. This split mirrors the Go handlers exactly.
#[derive(Debug, Clone)]
pub struct RunActionError(String);

impl RunActionError {
    /// Construct a run-action error with a human `message` (rendered as the 500 body's `message`).
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for RunActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RunActionError {}

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
/// The full Go mux registers, in order (more-specific multi-segment patterns first); ✓ = landed
/// (H1's two + H2's read handlers), and the H3 write lane slots the rest in here:
/// ```text
///   /healthz                       (H1) ✓     /api/v1/runs/{id}/events      (H2) ✓
///   /api/v1/state                  (H1) ✓     /api/v1/runs/{id}/transcript  (H2) ✓
///   /api/v1/refresh                (H3)        /api/v1/runs/{id}/stop        (H3)
///   /api/v1/config                 (H3)        /api/v1/runs/{id}/resume      (H3)
///   /api/v1/linear/projects        (H2) ✓     /api/v1/runs/{id}/message     (H3)
///   /api/v1/linear/identity        (H2) ✓     /api/v1/runs/{id}/messages    (H3)
///   /api/v1/projects               (H2) ✓     /api/v1/issues/{id}/history   (H2) ✓
///   /api/v1/history                (H2) ✓     /api/v1/logs/stream           (H2) ✓
///   /api/v1/events                 (H2) ✓     /api/v1/logs                  (H2) ✓
///   /api/v1/metrics                (H2) ✓     /api/v1/runs/{id}             (H2) ✓
/// ```
/// The `/runs/{id}/messages` GET lives with the run-message write surface in the H3 lane (Go's
/// `handlers_message.go`, whose tests H3 mirrors), so it slots in there — not H2. Unlike Go's
/// `ServeMux`, axum's matchit resolves pattern specificity itself, so the Go registration ORDER is
/// informational (it does not affect matching) — the paths + method semantics are the contract.
pub(crate) fn build_router<H, T>(
    provider: Arc<dyn StateProvider>,
    logs: Option<Arc<dyn LogSource>>,
    web_fallback: H,
) -> Router
where
    H: Handler<T, ApiState>,
    T: 'static,
{
    Router::new()
        .route("/healthz", any(handle_healthz))
        .route("/api/v1/state", any(handle_state))
        // Build identity (STUDIO-380): which commit this daemon was built from. Additive and
        // state-free — `/state` is golden-pinned to the Go daemon's payload and cannot carry it.
        .route("/api/v1/version", any(handle_version))
        // Coalesced poll+reconcile trigger (H3): POST-only, 202. Registered method-agnostically so a
        // GET yields a 405 envelope rather than the SPA fallback.
        .route("/api/v1/refresh", any(handle_refresh))
        // Read-write config (H3): GET returns the on-disk WORKFLOW.md view; POST validates + atomically
        // rewrites it (the watcher then hot-reloads). Loopback-only by server construction.
        .route("/api/v1/config", any(handle_config))
        // Agent-capabilities registry (Rhapsody-only, no Go v0.4.0 counterpart): GET returns the
        // registry so the Settings UI can render the opt-in checkbox list. Method-agnostic like the
        // other read routes; the handler guards GET/HEAD.
        .route("/api/v1/capabilities", any(handle_capabilities))
        // Rhapsody Teams memory (STUDIO-645, Rhapsody-only — no Go v0.4.0 counterpart): the roster
        // with derived status, an identity's recalled memory, and the per-record invalidate that
        // §5.2.3 wants "reachable at the moment someone notices". All static paths, so they never
        // contend with anything already registered; a Teams-off daemon answers `teams_disabled`.
        .route("/api/v1/teams/roster", any(handle_teams_roster))
        .route("/api/v1/teams/recall", any(handle_teams_recall))
        .route("/api/v1/teams/invalidate", any(handle_teams_invalidate))
        // The team room's read side (STUDIO-650, T5): a bounded, read-only peek that advances no
        // identity's cursor.
        .route("/api/v1/teams/room", any(handle_teams_room))
        // History + run-detail read API (H2). The multi-segment patterns (runs/{id}/events,
        // runs/{id}/transcript, issues/{id}/history) are more specific than runs/{id}; axum's matchit
        // dispatches them first regardless of registration order.
        .route("/api/v1/history", any(handle_history))
        // Rhapsody-only history surfaces (TRA-320): an ISSUE-paged listing (one row per issue) and
        // whole-store day totals, so the dashboard's Jobs list and header cells stop being derived
        // from whatever run page the client happened to fetch. Both are static paths, so they never
        // contend with `/api/v1/history` itself.
        .route("/api/v1/history/issues", any(handle_issue_runs))
        .route("/api/v1/history/summary", any(handle_history_summary))
        .route("/api/v1/events", any(handle_event_search))
        .route("/api/v1/metrics", any(handle_metrics))
        .route("/api/v1/runs/{id}/events", any(handle_run_events))
        .route("/api/v1/runs/{id}/transcript", any(handle_run_transcript))
        .route("/api/v1/issues/{id}/history", any(handle_issue_history))
        // Run actions (H3): kill a running agent (+ move its ticket to Backlog) and resume a stopped
        // run (+ move it back to Todo). More-specific multi-segment POST patterns; axum's matchit
        // dispatches them ahead of the catch-all runs/{id} detail route regardless of order.
        .route("/api/v1/runs/{id}/stop", any(handle_run_stop))
        .route("/api/v1/runs/{id}/resume", any(handle_run_resume))
        // Daemon-mediated review handoff (TRA-242): move a live run's ticket to the review state so it
        // leaves the active set and the run cleanly ends. POST-only; more-specific than runs/{id}.
        .route("/api/v1/runs/{id}/handoff", any(handle_run_handoff))
        // Host-stamped memory retain for a live run (STUDIO-645): the body carries `content` and
        // nothing else — the identity, ticket and commit come from the run this path names, which
        // is what makes provenance unforgeable (§5.1). More-specific than runs/{id}.
        .route("/api/v1/runs/{id}/retain", any(handle_run_retain))
        // The room's write side (STUDIO-653, T6): run-scoped in its path for the same reason
        // retain is — the run id in the PATH is what `from` is stamped from, and the body carries
        // no provenance key at all.
        .route("/api/v1/runs/{id}/post", any(handle_run_post))
        // Operator messages (H3): POST queues a "btw" for a live run's agent; GET lists the run's
        // messages with their delivery status. More-specific than runs/{id}, so they win the match.
        .route("/api/v1/runs/{id}/message", any(handle_run_message))
        .route("/api/v1/runs/{id}/messages", any(handle_run_messages))
        .route("/api/v1/runs/{id}", any(handle_run_detail))
        // Per-project live status + the read-only Linear surfaces for the Settings page (H2).
        .route("/api/v1/projects", any(handle_projects))
        .route("/api/v1/linear/projects", any(handle_linear_projects))
        .route("/api/v1/linear/identity", any(handle_linear_identity))
        // Daemon process-log surface for the Logs settings tab (H2): a one-shot ring snapshot + an SSE
        // stream that replays the backlog then tails live. Distinct exact paths (no overlap with
        // runs/{id}); read the optional `LogSource` from the router state.
        .route("/api/v1/logs/stream", any(handle_log_stream))
        .route("/api/v1/logs", any(handle_logs))
        // SPA catch-all — the embedded React dashboard. As the router fallback it is consulted LAST
        // (never shadows an /api route) and 404s any /api path defensively (see `web::serve_web`). Go
        // mounts this on "/"; axum models the same "everything else" match as the fallback.
        .fallback(web_fallback)
        .with_state(ApiState { provider, logs })
}

/// The router state: the read [`StateProvider`] + the optional process-log [`LogSource`]. Mirrors Go's
/// `api` struct (`provider` + `logs` + `logger`); the Rust port uses `tracing`'s global subscriber in
/// place of Go's per-`api` `logger`, so only the two data sources are carried. The [`FromRef`] impls let
/// every provider-only handler keep extracting `State<Arc<dyn StateProvider>>` while the log handlers
/// extract `State<Option<Arc<dyn LogSource>>>` — one router state, two sub-state views.
#[derive(Clone)]
pub(crate) struct ApiState {
    provider: Arc<dyn StateProvider>,
    logs: Option<Arc<dyn LogSource>>,
}

impl FromRef<ApiState> for Arc<dyn StateProvider> {
    fn from_ref(state: &ApiState) -> Self {
        state.provider.clone()
    }
}

impl FromRef<ApiState> for Option<Arc<dyn LogSource>> {
    fn from_ref(state: &ApiState) -> Self {
        state.logs.clone()
    }
}

/// Build the API + embedded-dashboard handler over the production dashboard embed (`web-dist/`).
/// Mirrors Go `NewHandler(p, logs, logger)`: `provider` is the read surface, `logs` is the optional
/// process-log ring the `/api/v1/logs*` endpoints serve (`None` ⇒ empty snapshot + heartbeat-only
/// stream). Go's `logger` has no analog — the Rust port logs through `tracing`'s global subscriber.
///
/// Deferred (serial-chain): Go wraps the mux in `otelhttp.NewHandler(mux, "symphony.http")` for
/// per-request server spans/metrics. That wrap needs the telemetry providers, which land with the
/// telemetry lane (T1) and are initialized by the final assembly (F1); it is transparent to routing,
/// so this returns the bare router and F1 layers telemetry on.
pub fn new_handler(provider: Arc<dyn StateProvider>, logs: Option<Arc<dyn LogSource>>) -> Router {
    build_router(provider, logs, serve_web::<WebDist>)
}

/// The loopback HTTP server wrapping [`new_handler`]. Mirrors Go `httpapi.Server` (`server.go`): bind
/// a loopback listener (port 0 for an ephemeral port), then [`Server::serve`] until shutdown.
pub struct Server {
    listener: tokio::net::TcpListener,
    router: Router,
}

impl Server {
    /// Bind a loopback listener on `addr` (use `127.0.0.1:0` for an ephemeral port) and build the
    /// handler for `provider` + the optional log `logs` source. Mirrors Go `New` (`net.Listen("tcp",
    /// addr)` + `NewHandler`).
    ///
    /// The `WriteTimeout`/`IdleTimeout` Go intentionally omits (loopback, small responses, plus the
    /// SSE `/logs/stream` endpoint that must not be write-deadline-cut) have no axum knob to set;
    /// hyper's built-in header-read guard covers the slowloris case Go handles via `ReadHeaderTimeout`.
    pub async fn bind(
        provider: Arc<dyn StateProvider>,
        logs: Option<Arc<dyn LogSource>>,
        addr: &str,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            router: new_handler(provider, logs),
        })
    }

    /// The actual bound address (useful when the port was 0). Mirrors Go `Addr`.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Publish this server's ACTUAL bound loopback port to `~/.rhapsody/runtime.json` via T1's
    /// [`rhapsody_core::runtimeport`] (REUSED, not reimplemented), so `symphony mcp` — an operator's
    /// CLI and the workers the daemon injects — can reach a daemon launched on a dynamic/ephemeral
    /// `--port` instead of the stale `server.port` in WORKFLOW.md. `local_addr` resolves the real port
    /// even when the bind port was 0. Best-effort: the caller treats a write failure as non-fatal
    /// (`symphony mcp` then falls back to the config port), exactly as Go does.
    ///
    /// This is the server-side capability the H3 lane owns; the *invocation* (call after bind, and
    /// `runtimeport::remove()` on clean shutdown) lands with the final assembly's `run.rs`, mirroring
    /// Go's `cmd/symphony/run.go` — which is also where Go places and tests `runtimeport.Write`, so
    /// like Go there is no httpapi-level test here (T1's `runtimeport` unit tests cover the atomic
    /// write; F1's boot e2e covers the daemon-to-`symphony mcp` round-trip). No httpapi test drives it
    /// because `runtimeport::write` targets the single shared `~/.rhapsody/runtime.json`, which a
    /// test must not clobber on the self-hosted CI runner (a live daemon may own it).
    pub fn publish_runtime_port(&self) -> std::io::Result<()> {
        let port = self.local_addr()?.port();
        rhapsody_core::runtimeport::write(i32::from(port))
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
        let server = Server::bind(
            Arc::new(FakeProvider::ok(empty_snapshot())),
            None,
            "127.0.0.1:0",
        )
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
        let server = Server::bind(
            Arc::new(FakeProvider::ok(empty_snapshot())),
            None,
            "127.0.0.1:0",
        )
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
