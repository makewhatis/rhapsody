//! handlers_teams — the Rhapsody Teams memory endpoints (STUDIO-645, slice T4;
//! design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5, §0.11.7).
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — this is the same
//! additive shape `/api/v1/capabilities` established. Every route here is NEW:
//! nothing is added to a parity-checked view, no golden moves, and a daemon
//! with Teams off answers each of them `teams_disabled` rather than growing a
//! key anywhere an existing fixture can see (§2.4 row 3).
//!
//! | Route | Backs |
//! |---|---|
//! | `GET /api/v1/teams/roster` | `teams_roster` |
//! | `GET /api/v1/teams/recall` | `teams_recall {identity, query}` |
//! | `POST /api/v1/teams/invalidate` | `teams_invalidate {identity, fact_id, reason}` |
//! | `POST /api/v1/runs/{id}/retain` | `teams_retain {content}` |
//! | `GET /api/v1/teams/room` | `teams_room_read {limit?}` (STUDIO-650, T5) |
//! | `POST /api/v1/teams/room` | the OPERATOR's own post — no MCP tool (STUDIO-661) |
//! | `POST /api/v1/runs/{id}/post` | `teams_post {body, to?, refs?}` (STUDIO-653, T6) |
//! | `GET /api/v1/teams` | the dashboard's one view (STUDIO-652) |
//! | `GET`/`POST /api/v1/teams/config` | the dashboard's enable flow (STUDIO-652) |
//!
//! `/api/v1/teams/config` is the ONE route here that is **not** gated on Teams being enabled, and
//! it has to be: it is how a disabled daemon gets enabled, and off is the only state from which
//! anyone would open it. It follows `POST /api/v1/config`'s discipline instead — validate with the
//! daemon's own `Teams::validate`, atomically rewrite only when valid, and leave the on-disk file
//! untouched on a rejection. §2.1's never-seed rule survives intact: nothing here writes unless a
//! human explicitly POSTs a config.
//!
//! Retain is deliberately **run-scoped in its path**, following
//! `/api/v1/runs/{id}/handoff`: the run id is what the host resolves the
//! identity, ticket and commit from, and the body carries `content` and nothing
//! else. There is no route by which an agent can supply its own provenance
//! (§5.1).
//!
//! `POST /api/v1/runs/{id}/post` is the same shape for the same reason (§0.11.4:
//! "`from` is stamped by the host … a run cannot supply it"). Its body carries
//! `body`, an optional `to` and optional `refs`; **there is no `from` field**,
//! and a body that invents one is ignored the way retain's is.
//!
//! `POST /api/v1/teams/room` (STUDIO-661) is the one write here that is NOT run-scoped, because a
//! human post has no run: the daemon stamps the reserved `operator` name on it. Same
//! no-`from`-field rule, one route rather than two because there is no run id to put in the path —
//! and deliberately no MCP tool, since agents already have `teams_post` and this door exists for
//! the dashboard and `curl`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_config::teams::{Teams, TeamsError};
use rhapsody_orchestrator::teamsmemory::TeamsMemoryError;
use serde::{Deserialize, Serialize};

use crate::handlers::{require_get, require_post};
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds a retained record — and, from T6, a room post — on the wire. Each has
/// its own tighter truncation further in (the bank's
/// `MAX_RETAIN_CONTENT_BYTES`, the room's `MAX_POST_BODY_BYTES`); this rejects
/// an obvious paste of a whole transcript at the door, with a message that says
/// what the surface is for (§5.1: a *constructed record, never a transcript*).
const MAX_RETAIN_BODY: usize = 1 << 16;

/// `GET /api/v1/teams/recall?identity=&query=`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RecallParams {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    query: String,
}

/// `GET /api/v1/teams/room?limit=` (STUDIO-650, T5).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RoomParams {
    /// How many of the newest posts to return. Absent, empty, `0` or unparseable ⇒ the room's own
    /// default window; any value is clamped to its ceiling, so this parameter can widen nothing
    /// (§0.5's "bounded read window, non-negotiable").
    ///
    /// Carried as a `String` and parsed here rather than typed `usize`, because a typed field would
    /// make `?limit=abc` fail axum's own extractor and answer with ITS error body instead of this
    /// crate's `{error, message}` envelope — a wire-shape inconsistency no other read route has. A
    /// garbage limit is a request for the default window, not a 400.
    #[serde(default)]
    limit: String,
}

impl RoomParams {
    fn limit(&self) -> usize {
        self.limit.trim().parse().unwrap_or(0)
    }
}

/// `POST /api/v1/teams/invalidate` body.
#[derive(Debug, Default, Deserialize)]
struct InvalidateReq {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    fact_id: String,
    #[serde(default)]
    reason: String,
}

/// `POST /api/v1/runs/{id}/post` body (STUDIO-653, T6). No `from` and no
/// `identity`: unknown keys are ignored by serde, so a body that invents one
/// changes nothing — the author is resolved from the run id in the PATH.
#[derive(Debug, Default, Deserialize)]
struct PostReq {
    #[serde(default)]
    body: String,
    /// The recipient's name, or `*`/absent for the whole room.
    #[serde(default)]
    to: String,
    /// Ticket ids, PR urls, commit SHAs — what proves it (§0.10).
    #[serde(default)]
    refs: Vec<String>,
}

/// `POST /api/v1/runs/{id}/retain` body — `content` and nothing else. Any other
/// key is ignored, which is the point: there is no field an agent could add to
/// influence the provenance the host stamps.
#[derive(Debug, Default, Deserialize)]
struct RetainReq {
    #[serde(default)]
    content: String,
}

/// Maps a [`TeamsMemoryError`] onto the response envelope. The split matters:
/// `teams_disabled` and `not_running` are *business* outcomes an agent should
/// read and stop on, while a backend failure is a 500 it may retry.
fn teams_error(err: &TeamsMemoryError) -> Response {
    match err {
        TeamsMemoryError::Disabled => write_error(
            StatusCode::CONFLICT,
            "teams_disabled",
            err.to_string(),
            None,
        ),
        TeamsMemoryError::NotRunning => {
            write_error(StatusCode::CONFLICT, "not_running", err.to_string(), None)
        }
        TeamsMemoryError::NotFound(_) => {
            write_error(StatusCode::NOT_FOUND, "not_found", err.to_string(), None)
        }
        TeamsMemoryError::Invalid(_) => write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            err.to_string(),
            None,
        ),
        TeamsMemoryError::Backend(_) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_backend_error",
            err.to_string(),
            None,
        ),
    }
}

/// `GET /api/v1/teams/roster` — who is on the roster, the profile each wears,
/// and the runs live as each right now (§6.7's "derived status").
pub(crate) async fn handle_teams_roster(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.teams_roster().await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `GET /api/v1/teams/recall` — an identity's memory for a free-text query, the
/// memory-first path that costs no model turn (§6.1, §6.7).
pub(crate) async fn handle_teams_recall(
    method: Method,
    Query(params): Query<RecallParams>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.teams_recall(&params.identity, &params.query).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `GET`/`POST /api/v1/teams/room`, dispatched by method (the shape [`handle_teams_config`] uses).
///
/// * **GET** — the newest posts in the team room (§0.5, §0.11.4). **Read-only, and it advances no
///   cursor.** Cursors belong to hydration; a mid-run peek that ate a catch-up would silently hide
///   a hand-off from the teammate it was addressed to.
/// * **POST** — the room's HUMAN door (STUDIO-661): the operator's own post, `from` stamped by the
///   daemon. See [`operator_room_post`].
pub(crate) async fn handle_teams_room(
    method: Method,
    Query(params): Query<RoomParams>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    match method {
        Method::GET | Method::HEAD => match provider.teams_room(params.limit()).await {
            Ok(view) => write_json(StatusCode::OK, &view),
            Err(e) => teams_error(&e),
        },
        Method::POST => operator_room_post(provider.as_ref(), &body).await,
        _ => write_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use GET to read the team room or POST to post to it",
            Some("GET, HEAD, POST"),
        ),
    }
}

/// `POST /api/v1/teams/room` body (STUDIO-661) — `body` and optional `refs`, and **nothing else**.
///
/// There is deliberately no `from`: the operator has no run to resolve an identity through, so the
/// daemon stamps the reserved name itself, and a body that invents a `from` (or an `identity`) is
/// ignored by serde exactly as `teams_retain`'s and `teams_post`'s are.
///
/// There is deliberately no `to` either — v1 is **room-wide only**. Direct-to-a-live-run from the
/// operator already exists as the operator-message mailbox (`POST /api/v1/runs/{id}/message`),
/// which is the authoritative live channel; an async direct note to a *sleeping* teammate is an
/// unproven need and a follow-up if it is ever wanted. The log already carries the `to` field, so
/// that day costs one additive key here.
#[derive(Debug, Default, Deserialize)]
struct OperatorPostReq {
    #[serde(default)]
    body: String,
    /// Ticket ids, PR urls, commit SHAs — what proves it (§0.10).
    #[serde(default)]
    refs: Vec<String>,
}

/// The operator's own room post. Shares [`MAX_RETAIN_BODY`] as its wire cap for the same reason
/// [`handle_run_post`] does — the room applies its own tighter truncation on the way to disk, and
/// rejecting the obvious paste at the door tells the caller rather than silently losing the tail.
///
/// An empty body is `bad_request` (the daemon's own message, from the same check a teammate's post
/// runs through), and a Teams-off daemon answers `teams_disabled` like every other Teams route.
/// Posting starts no run, writes no label and touches no tracker (§0.2).
async fn operator_room_post(provider: &dyn StateProvider, body: &Bytes) -> Response {
    if body.len() > MAX_RETAIN_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "content_too_long",
            "a room post is a short message to the team, not a transcript",
            None,
        );
    }
    let req: OperatorPostReq = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider.teams_room_post(&req.body, &req.refs).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `POST /api/v1/teams/invalidate` — §5.3's per-record correction, with the
/// reason stored and nothing deleted.
pub(crate) async fn handle_teams_invalidate(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to invalidate a memory") {
        return resp;
    }
    let req: InvalidateReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider
        .teams_invalidate(&req.identity, &req.fact_id, &req.reason)
        .await
    {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `POST /api/v1/runs/{id}/retain` — record what THIS run learned, with every
/// provenance field stamped by the host (§5.1).
pub(crate) async fn handle_run_retain(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to retain a memory") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    if body.len() > MAX_RETAIN_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "content_too_long",
            "a retained record is a short constructed observation, not a transcript",
            None,
        );
    }
    let req: RetainReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider.teams_retain(run_id, &req.content).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `POST /api/v1/runs/{id}/post` — post to the team room AS the run named in the
/// path, with `from` stamped by the host (STUDIO-653, T6; §0.5, §0.11.4).
///
/// Shares [`MAX_RETAIN_BODY`] as its wire cap: both are "a short constructed
/// message, not a transcript", and the room applies its own, tighter
/// `MAX_POST_BODY_BYTES` truncation on the way to disk. This one rejects the
/// obvious paste at the door so the caller learns rather than silently losing
/// the tail.
pub(crate) async fn handle_run_post(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to post to the team room") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    if body.len() > MAX_RETAIN_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "content_too_long",
            "a room post is a short message to the team, not a transcript",
            None,
        );
    }
    let req: PostReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider
        .teams_post(run_id, &req.body, &req.to, &req.refs)
        .await
    {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `GET /api/v1/teams` — the ONE view the dashboard renders (STUDIO-652): the roster with each
/// identity's derived status, plus the manager mode and memory backend that make it legible.
pub(crate) async fn handle_teams(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.teams_overview().await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// Caps a `teams.yaml` POST. A roster is a handful of short records; 256 KiB bounds abuse on the
/// loopback socket while sitting far above any real file. (`/api/v1/config` allows 1 MiB because a
/// WORKFLOW.md carries a whole prompt body; this file carries none.)
const MAX_TEAMS_CONFIG_BODY: usize = 1 << 18;

/// `GET`/`POST /api/v1/teams/config` — the enable flow's read and write of `teams.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct TeamsConfigView {
    /// Where this daemon reads `teams.yaml`.
    pub path: String,
    /// Whether the file exists at all. **`false` is the shipped state** (§2.1): absent ≡ disabled,
    /// and reading this endpoint does not change that — nothing here seeds.
    pub present: bool,
    /// Why a PRESENT file did not load, verbatim from the daemon's own loader; empty when it did
    /// (or when there is none). Surfaced rather than swallowed, because a `teams.yaml` the daemon
    /// rejected reads as "Teams is off" everywhere else in the app, which is indistinguishable
    /// from never having written one.
    pub error: String,
    /// The loaded config, or the schema defaults (i.e. disabled) when there is no file. Always
    /// present, so an editor has something to render either way.
    pub config: Teams,
    /// Teams config is **boot-loaded**: `run::run` reads `teams.yaml` once before `Run` and there
    /// is no watcher on it, unlike `WORKFLOW.md`. A save therefore takes effect on the next daemon
    /// start, and the UI has to say so. Carried as a field rather than assumed by the client so the
    /// day the daemon does hot-reload it, the note disappears by itself.
    pub restart_required: bool,
}

/// `POST /api/v1/teams/config` body — `{"config": {...}}`, mirroring `/api/v1/config`'s envelope
/// so the two config writes read the same on the wire.
#[derive(Debug, Default, Deserialize)]
struct TeamsConfigReq {
    #[serde(default)]
    config: Teams,
}

/// `GET`/`POST /api/v1/teams/config`, dispatched by method (the shape `handle_config` uses).
pub(crate) async fn handle_teams_config(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    match method {
        Method::GET | Method::HEAD => teams_config_get(provider.as_ref()),
        Method::POST => teams_config_post(provider.as_ref(), &body),
        _ => write_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use GET to read or POST to write teams.yaml",
            Some("GET, HEAD, POST"),
        ),
    }
}

/// The current on-disk `teams.yaml`, or the disabled default plus `present: false`.
fn teams_config_get(provider: &dyn StateProvider) -> Response {
    let path = provider.teams_config_path();
    if path.is_empty() {
        return no_teams_config_home();
    }
    let p = std::path::Path::new(path);
    let present = p.exists();
    let (config, error) = match Teams::try_load(p) {
        Ok(teams) => (teams, String::new()),
        // A present-but-rejected file is reported WITH its reason and WITH the off state, not as a
        // 500: the daemon booted through exactly this and is running disabled, so the endpoint
        // reports what the daemon did rather than a failure the daemon did not have.
        Err(err) => (Teams::disabled(), err.to_string()),
    };
    write_json(
        StatusCode::OK,
        &TeamsConfigView {
            path: path.to_string(),
            present,
            error,
            config,
            restart_required: true,
        },
    )
}

/// Validate → (only if valid) atomically write → echo what is now on disk.
fn teams_config_post(provider: &dyn StateProvider, body: &Bytes) -> Response {
    let path = provider.teams_config_path();
    if path.is_empty() {
        return no_teams_config_home();
    }
    if body.len() > MAX_TEAMS_CONFIG_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body exceeds 256 KiB",
            None,
        );
    }
    let req: TeamsConfigReq = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    // `Teams::save` validates first and writes nothing on a rejection, so a bad edit can never
    // replace a working file — and the complaint is the daemon's own, verbatim, rather than a
    // second implementation of the same rules that could disagree with the one that decides
    // whether the file loads at boot.
    match Teams::save(std::path::Path::new(path), &req.config) {
        Ok(()) => teams_config_get(provider),
        Err(err @ (TeamsError::Invalid(_) | TeamsError::Parse(_))) => write_error(
            StatusCode::BAD_REQUEST,
            "invalid_teams_config",
            err.to_string(),
            None,
        ),
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "teams_config_write_error",
            err.to_string(),
            None,
        ),
    }
}

/// A daemon with no on-disk runtime home (`--no-store`, `storage.path: off` / `:memory:`) has
/// nowhere to keep a `teams.yaml`, so the enable flow says that rather than guessing a path and
/// writing somewhere the daemon will never read.
fn no_teams_config_home() -> Response {
    write_error(
        StatusCode::CONFLICT,
        "teams_config_unavailable",
        "this daemon has no on-disk runtime home, so it has no teams.yaml to read or write",
        None,
    )
}

#[cfg(test)]
mod tests {
    //! The Teams memory endpoints, driven end to end against a REAL
    //! `TeamsMemory` over a temp bank — a canned provider result would prove
    //! only that the handler forwards, and the properties worth pinning here
    //! (host-stamped provenance, a Teams-off daemon answering `teams_disabled`)
    //! live in the interaction between the two.

    use std::sync::Arc;

    use rhapsody_config::memory::{DEFAULT_BANKS_SUBDIR, LocalBank};
    use rhapsody_config::room::{
        Cursor, DEFAULT_ROOM_SUBDIR, LocalRoom, MAX_ROOM_WINDOW, Message as RoomMessage, RoomLog,
    };
    use rhapsody_config::teams::{Identity, Teams};
    use rhapsody_orchestrator::teamsmemory::{RunProvenance, TeamsMemory};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    /// A temp directory that cleans itself up; the crate takes no `tempfile`
    /// dependency, matching the sibling crates' hand-rolled helper.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("rhapsody-httpapi-teams-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn teams_memory(dir: &TempDir) -> Arc<TeamsMemory> {
        let teams = Teams {
            enabled: true,
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string()],
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        let bank = LocalBank::new(dir.0.join(DEFAULT_BANKS_SUBDIR), "agent-");
        Arc::new(TeamsMemory::new(Arc::new(teams), Arc::new(bank)))
    }

    /// The same runtime with a room attached (STUDIO-650, T5), returned alongside so a test can
    /// post into it.
    fn teams_memory_with_room(dir: &TempDir) -> (Arc<TeamsMemory>, Arc<LocalRoom>) {
        let room = Arc::new(LocalRoom::new(dir.0.join(DEFAULT_ROOM_SUBDIR)));
        let mem = Arc::new(
            Arc::try_unwrap(teams_memory(dir))
                .map(|m| m.with_room(Arc::clone(&room) as Arc<dyn RoomLog>))
                .unwrap_or_else(|_| unreachable!("sole owner")),
        );
        (mem, room)
    }

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn spawn_with(mem: Arc<TeamsMemory>) -> String {
        spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await
    }

    async fn post(url: &str, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("POST")
    }

    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    async fn err_code(resp: reqwest::Response) -> String {
        body_json(resp).await["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// **Retain → recall → invalidate, end to end through HTTP.** The retain
    /// body carries `content` and nothing else; every provenance field on the
    /// response was stamped by the host from the run id in the PATH (§5.1).
    #[tokio::test]
    async fn retain_recall_invalidate_round_trip() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(Arc::clone(&mem)).await;

        let retained = body_json(
            post(
                &format!("{url}/api/v1/runs/7/retain"),
                r#"{"content":"the mirror lock is per-repo","identity":"bob","ticket":"XX-1"}"#,
            )
            .await,
        )
        .await;
        assert_eq!(
            retained["identity"], "alice",
            "the identity is the RUN's, not the body's: {retained}"
        );
        assert_eq!(
            retained["ticket"], "MT-9",
            "the ticket is the RUN's, not the body's: {retained}"
        );
        assert_eq!(retained["document_id"], "run-7");
        let fact_id = retained["id"].as_str().expect("a record id").to_string();

        let recalled = body_json(
            reqwest::get(&format!(
                "{url}/api/v1/teams/recall?identity=alice&query=mirror%20lock"
            ))
            .await
            .expect("GET recall"),
        )
        .await;
        assert_eq!(recalled["facts"].as_array().expect("facts").len(), 1);
        assert_eq!(
            recalled["facts"][0]["content"],
            "the mirror lock is per-repo"
        );

        let invalidated = body_json(
            post(
                &format!("{url}/api/v1/teams/invalidate"),
                &format!(
                    r#"{{"identity":"alice","fact_id":"{fact_id}","reason":"the lock moved in MT-10"}}"#
                ),
            )
            .await,
        )
        .await;
        assert_eq!(invalidated["invalidated"], true);
        assert_eq!(invalidated["reason"], "the lock moved in MT-10");

        let after = body_json(
            reqwest::get(&format!(
                "{url}/api/v1/teams/recall?identity=alice&query=mirror%20lock"
            ))
            .await
            .expect("GET recall"),
        )
        .await;
        assert!(
            after["facts"].as_array().expect("facts").is_empty(),
            "an invalidated fact leaves recall: {after}"
        );
    }

    /// A retain naming a run the host has no binding for is `not_running`, not
    /// a record attributed to a guess.
    #[tokio::test]
    async fn retain_from_an_unbound_run_is_not_running() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/runs/99/retain"),
            r#"{"content":"anything"}"#,
        )
        .await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "not_running");
    }

    /// The roster reports who exists and what each is doing (§6.7).
    #[tokio::test]
    async fn the_roster_endpoint_reports_derived_status() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(mem).await;
        let view = body_json(
            reqwest::get(&format!("{url}/api/v1/teams/roster"))
                .await
                .expect("GET roster"),
        )
        .await;
        assert_eq!(view["backend"], "local");
        assert_eq!(view["roster"][0]["name"], "alice");
        assert_eq!(view["roster"][0]["profile"], "swe");
        assert_eq!(view["roster"][0]["live_runs"], 1);
        assert_eq!(view["roster"][0]["tickets"][0], "MT-9");
    }

    /// **A daemon with no Teams runtime answers `teams_disabled` on every
    /// route** — the same answer `enabled: false` gives. The routes exist (they
    /// are static paths on the router), but they contribute nothing, and the MCP
    /// facade removes the tools entirely so an agent never reaches them.
    #[tokio::test]
    async fn every_teams_route_is_disabled_without_a_teams_runtime() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;

        for path in ["/api/v1/teams/roster", "/api/v1/teams/recall?identity=a"] {
            let resp = reqwest::get(&format!("{url}{path}")).await.expect("GET");
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(err_code(resp).await, "teams_disabled", "{path}");
        }
        for (path, body) in [
            (
                "/api/v1/teams/invalidate",
                r#"{"identity":"a","fact_id":"b","reason":"c"}"#,
            ),
            ("/api/v1/runs/7/retain", r#"{"content":"x"}"#),
            ("/api/v1/runs/7/post", r#"{"body":"x"}"#),
            // The human door is gated exactly like the agent-facing ones (STUDIO-661).
            ("/api/v1/teams/room", r#"{"body":"x"}"#),
        ] {
            let resp = post(&format!("{url}{path}"), body).await;
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(err_code(resp).await, "teams_disabled", "{path}");
        }
    }

    /// An invalidation with no reason is refused: §5.3 stores the reason, and a
    /// reasonless correction is unreadable to whoever finds it later.
    #[tokio::test]
    async fn invalidate_without_a_reason_is_rejected() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/teams/invalidate"),
            r#"{"identity":"alice","fact_id":"20260101T000000Z-run-1"}"#,
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "bad_request");
    }

    /// A record that does not exist is a 404, distinguishable from a backend
    /// failure so a caller knows retrying will not help.
    #[tokio::test]
    async fn invalidating_an_unknown_record_is_404() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/teams/invalidate"),
            r#"{"identity":"alice","fact_id":"20260101T000000Z-run-1","reason":"why"}"#,
        )
        .await;
        assert_eq!(resp.status(), 404);
        assert_eq!(err_code(resp).await, "not_found");
    }

    /// Every route rejects the wrong method with a 405 envelope rather than
    /// falling through to the SPA fallback — the convention every other route
    /// in this crate follows.
    #[tokio::test]
    async fn wrong_methods_are_405_not_the_spa_fallback() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        for path in ["/api/v1/teams/roster", "/api/v1/teams/recall"] {
            let resp = post(&format!("{url}{path}"), "{}").await;
            assert_eq!(resp.status(), 405, "{path}");
        }
        for path in [
            "/api/v1/teams/invalidate",
            "/api/v1/runs/7/retain",
            "/api/v1/runs/7/post",
        ] {
            let resp = reqwest::get(&format!("{url}{path}")).await.expect("GET");
            assert_eq!(resp.status(), 405, "{path}");
        }
        // `/api/v1/teams/room` answers BOTH verbs since STUDIO-661, so its 405 is anything else.
        let resp = reqwest::Client::new()
            .delete(format!("{url}/api/v1/teams/room"))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers()
                .get("allow")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "GET, HEAD, POST"
        );
    }

    // ── the dashboard surface (STUDIO-652) ─────────────────────────────────────────────────────

    /// `GET /api/v1/teams` — the ONE view the dashboard renders: the roster with derived status,
    /// plus the manager mode and memory backend that make it legible.
    #[tokio::test]
    async fn the_overview_serves_the_roster_manager_mode_and_backend() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(mem).await;
        let view = body_json(
            reqwest::get(&format!("{url}/api/v1/teams"))
                .await
                .expect("GET teams"),
        )
        .await;
        assert_eq!(view["enabled"], true);
        assert_eq!(view["manager_mode"], "labels");
        assert_eq!(view["backend"], "local");
        assert_eq!(view["roster"][0]["name"], "alice");
        assert_eq!(view["roster"][0]["live_runs"], 1);
        assert_eq!(view["roster"][0]["tickets"][0], "MT-9");
    }

    /// Teams off ⇒ `teams_disabled` and a GET-only route, exactly like every other Teams surface.
    #[tokio::test]
    async fn the_overview_is_disabled_without_a_teams_runtime() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = reqwest::get(&format!("{url}/api/v1/teams"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "teams_disabled");

        let dir = TempDir::new();
        let on = spawn_with(teams_memory(&dir)).await;
        assert_eq!(
            post(&format!("{on}/api/v1/teams"), "{}").await.status(),
            405
        );
    }

    /// **`GET /api/v1/version` carries the gate** (STUDIO-652): the dashboard learns Teams is off
    /// from the request it already makes at mount, so a Teams-off app never touches
    /// `/api/v1/teams*` at all. The build fields are untouched beside it.
    #[tokio::test]
    async fn the_version_endpoint_carries_the_teams_gate() {
        let off = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let body = body_json(
            reqwest::get(&format!("{off}/api/v1/version"))
                .await
                .expect("GET version"),
        )
        .await;
        assert_eq!(body["teams_enabled"], false, "{body}");
        for field in ["version", "commit", "built_at"] {
            assert!(
                body[field].is_string(),
                "the build identity must keep its top-level fields: {body}"
            );
        }

        let dir = TempDir::new();
        let on = spawn_with(teams_memory(&dir)).await;
        let body = body_json(
            reqwest::get(&format!("{on}/api/v1/version"))
                .await
                .expect("GET version"),
        )
        .await;
        assert_eq!(body["teams_enabled"], true, "{body}");
    }

    // ── the enable flow (STUDIO-652) ───────────────────────────────────────────────────────────

    /// A daemon with no `teams.yaml` reports the off state **and creates nothing** — §2.1's
    /// never-seed rule, now with a read endpoint pointed straight at it.
    #[tokio::test]
    async fn reading_an_absent_teams_config_reports_off_and_seeds_nothing() {
        let dir = TempDir::new();
        let path = dir.0.join("teams.yaml");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_config_path(path.to_string_lossy()),
        ))
        .await;

        let body = body_json(
            reqwest::get(&format!("{url}/api/v1/teams/config"))
                .await
                .expect("GET"),
        )
        .await;
        assert_eq!(body["present"], false, "{body}");
        assert_eq!(body["config"]["enabled"], false, "{body}");
        assert_eq!(body["error"], "");
        assert_eq!(
            body["restart_required"], true,
            "teams.yaml is boot-loaded, and the UI has to say so: {body}"
        );
        assert!(
            !path.exists(),
            "reading the enable flow must never create teams.yaml"
        );
    }

    /// **The explicit enable, round-tripped.** A POST writes the file and the echoed view is read
    /// back off disk — so what the editor shows next is what the daemon will boot.
    #[tokio::test]
    async fn posting_a_teams_config_writes_it_and_echoes_what_is_on_disk() {
        let dir = TempDir::new();
        let path = dir.0.join("teams.yaml");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_config_path(path.to_string_lossy()),
        ))
        .await;

        let body = body_json(
            post(
                &format!("{url}/api/v1/teams/config"),
                r#"{"config":{"enabled":true,"manager":{"mode":"labels+model"},"memory":{"backend":"none"},"roster":[{"name":"alice","profile":"swe","labels":["rust"]}]}}"#,
            )
            .await,
        )
        .await;
        assert_eq!(body["present"], true, "{body}");
        assert_eq!(body["config"]["enabled"], true, "{body}");
        assert_eq!(body["config"]["manager"]["mode"], "labels+model", "{body}");
        assert_eq!(body["config"]["memory"]["backend"], "none", "{body}");
        assert_eq!(body["config"]["roster"][0]["name"], "alice", "{body}");
        assert!(path.exists(), "an explicit save creates teams.yaml");

        // The daemon's own loader agrees with what the endpoint echoed.
        let loaded = Teams::load(&path);
        assert!(loaded.enabled);
        assert_eq!(loaded.roster[0].name, "alice");
    }

    /// A rejected config is refused with the **daemon's own complaint, verbatim**, and leaves the
    /// working file exactly as it was — `POST /api/v1/config`'s discipline, applied here.
    #[tokio::test]
    async fn an_invalid_teams_config_is_refused_and_never_overwrites() {
        let dir = TempDir::new();
        let path = dir.0.join("teams.yaml");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_config_path(path.to_string_lossy()),
        ))
        .await;
        post(
            &format!("{url}/api/v1/teams/config"),
            r#"{"config":{"enabled":true,"roster":[{"name":"alice"}]}}"#,
        )
        .await;

        let resp = post(
            &format!("{url}/api/v1/teams/config"),
            r#"{"config":{"enabled":true,"roster":[{"name":"Alice"}]}}"#,
        )
        .await;
        assert_eq!(resp.status(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "invalid_teams_config", "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("label-safe"),
            "the daemon's own complaint must reach the operator verbatim: {body}"
        );
        assert_eq!(
            Teams::load(&path).roster[0].name,
            "alice",
            "a rejected save must leave the working file untouched"
        );
    }

    /// A present-but-broken `teams.yaml` reports the reason WITH the off state. Everywhere else in
    /// the app that file reads as "Teams is off", which is indistinguishable from never having
    /// written one — this is the one place that difference is visible.
    #[tokio::test]
    async fn a_broken_teams_config_reports_its_reason_with_the_off_state() {
        let dir = TempDir::new();
        let path = dir.0.join("teams.yaml");
        std::fs::write(&path, "enabled: true\nroster: [\n").expect("write a broken file");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_config_path(path.to_string_lossy()),
        ))
        .await;

        let body = body_json(
            reqwest::get(&format!("{url}/api/v1/teams/config"))
                .await
                .expect("GET"),
        )
        .await;
        assert_eq!(body["present"], true, "{body}");
        assert_eq!(body["config"]["enabled"], false, "the daemon booted off");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("teams_"),
            "the loader's reason must be reported: {body}"
        );
    }

    /// A daemon with no on-disk runtime home has nowhere to keep a `teams.yaml`, and says so
    /// rather than guessing a path and writing where the daemon will never read.
    #[tokio::test]
    async fn a_daemon_with_no_runtime_home_has_no_teams_config() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = reqwest::get(&format!("{url}/api/v1/teams/config"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "teams_config_unavailable");

        let resp = post(&format!("{url}/api/v1/teams/config"), r#"{"config":{}}"#).await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "teams_config_unavailable");
    }

    /// The config route answers a 405 ENVELOPE on anything but GET/HEAD/POST, rather than falling
    /// through to the SPA fallback — this crate's method-agnostic registration rule.
    #[tokio::test]
    async fn the_teams_config_route_refuses_other_methods() {
        let dir = TempDir::new();
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_teams_config_path(dir.0.join("teams.yaml").to_string_lossy()),
        ))
        .await;
        let resp = reqwest::Client::new()
            .delete(format!("{url}/api/v1/teams/config"))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(resp.status(), 405);
    }

    /// **The browse the memory panel needs** (§5.2.3): an empty `query` lists what the identity
    /// remembers, so a wrong fact can be seen before it can be invalidated.
    #[tokio::test]
    async fn recall_with_an_empty_query_lists_the_bank() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(Arc::clone(&mem)).await;
        for content in ["the mirror lock is per-repo", "goldens are recaptured only"] {
            post(
                &format!("{url}/api/v1/runs/7/retain"),
                &format!(r#"{{"content":"{content}"}}"#),
            )
            .await;
        }

        let body = body_json(
            reqwest::get(&format!("{url}/api/v1/teams/recall?identity=alice&query="))
                .await
                .expect("GET recall"),
        )
        .await;
        assert_eq!(
            body["facts"].as_array().map(Vec::len),
            Some(2),
            "an empty query lists the bank: {body}"
        );
    }

    // ── the room's read side (STUDIO-650, T5) ──────────────────────────────────────────────────

    /// The endpoint serves the room's newest posts, oldest first, with the host-stamped `from` and
    /// the stable `file:seq` id intact.
    #[tokio::test]
    async fn room_serves_the_newest_posts() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        room.append(
            &RoomMessage::room("@manager", chrono::Utc::now(), "assigned MT-1 to alice")
                .with_refs(["MT-1"]),
        )
        .expect("append");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        let body = body_json(
            reqwest::get(format!("{url}/api/v1/teams/room"))
                .await
                .expect("GET"),
        )
        .await;
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1, "{body}");
        assert_eq!(messages[0]["from"], "@manager");
        assert_eq!(messages[0]["to"], "*");
        assert_eq!(messages[0]["body"], "assigned MT-1 to alice");
        assert!(
            messages[0]["id"].as_str().unwrap_or_default().contains(':'),
            "the id is file:seq: {body}"
        );
    }

    /// **The tool must not eat a run's catch-up.** Serving this endpoint advances NO cursor, so a
    /// mid-run peek cannot hide a hand-off from the teammate it was addressed to. Checked by
    /// reading twice and getting the same answer, and by there being no cursor file at all.
    #[tokio::test]
    async fn room_reads_advance_no_cursor() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        room.append(&RoomMessage::room("@manager", chrono::Utc::now(), "news"))
            .expect("append");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        for _ in 0..2 {
            let body = body_json(
                reqwest::get(format!("{url}/api/v1/teams/room"))
                    .await
                    .expect("GET"),
            )
            .await;
            assert_eq!(
                body["messages"].as_array().map(Vec::len),
                Some(1),
                "a repeated read must return the same messages: {body}"
            );
        }
        assert!(
            !dir.0.join(DEFAULT_BANKS_SUBDIR).exists(),
            "a tool read must write no cursor anywhere"
        );
    }

    /// `limit` can only NARROW: it is clamped to the room's ceiling, so no caller can widen the
    /// window §0.5 calls non-negotiable, and `0` means the default rather than "nothing".
    #[tokio::test]
    async fn room_limit_narrows_and_never_widens() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        for n in 0..(MAX_ROOM_WINDOW + 10) {
            room.append(&RoomMessage::room(
                "@manager",
                chrono::Utc::now(),
                format!("m{n}"),
            ))
            .expect("append");
        }
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        let count = |q: &str| {
            let url = format!("{url}/api/v1/teams/room{q}");
            async move {
                body_json(reqwest::get(url).await.expect("GET")).await["messages"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or_default()
            }
        };
        assert_eq!(count("?limit=3").await, 3);
        assert!(count("?limit=0").await > 0, "0 is the default, not nothing");
        assert_eq!(
            count("?limit=100000").await,
            MAX_ROOM_WINDOW,
            "the ceiling cannot be widened from the wire"
        );
    }

    /// An unparseable `limit` is a request for the DEFAULT window, not a 400 — so the route keeps
    /// answering in this crate's envelope rather than falling through to axum's own extractor
    /// error body, which no other read route here does.
    #[tokio::test]
    async fn room_tolerates_a_garbage_limit() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        room.append(&RoomMessage::room("@manager", chrono::Utc::now(), "news"))
            .expect("append");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        for q in ["?limit=abc", "?limit=", "?limit=-3"] {
            let resp = reqwest::get(format!("{url}/api/v1/teams/room{q}"))
                .await
                .expect("GET");
            assert_eq!(resp.status(), 200, "{q} should serve the default window");
            let body = body_json(resp).await;
            assert_eq!(
                body["messages"].as_array().map(Vec::len),
                Some(1),
                "{q}: {body}"
            );
        }
    }

    /// A daemon with Teams enabled but no room configured answers as an EMPTY room, not an error:
    /// a room nobody has posted to and a room that cannot exist read the same.
    #[tokio::test]
    async fn no_room_configured_reads_as_an_empty_room() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = reqwest::get(format!("{url}/api/v1/teams/room"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(0), "{body}");
    }

    /// Teams off ⇒ `teams_disabled`, exactly like every other `teams_*` route.
    #[tokio::test]
    async fn room_is_teams_disabled_without_a_runtime() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = reqwest::get(format!("{url}/api/v1/teams/room"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "teams_disabled");
    }

    /// The route was GET-only until STUDIO-661 opened the human door on it, so a POST is no longer
    /// a 405 — it is the operator's own post, and the pinned behaviour here is that it reaches the
    /// handler rather than the method guard. Everything ELSE is still a 405 ENVELOPE rather than a
    /// fall-through to the SPA fallback (this crate's method-agnostic registration rule), which
    /// `wrong_methods_are_405_not_the_spa_fallback` pins with its `Allow` header.
    #[tokio::test]
    async fn room_accepts_a_post_and_still_refuses_other_methods() {
        let dir = TempDir::new();
        let (mem, _room) = teams_memory_with_room(&dir);
        let url = spawn_with(mem).await;

        let resp = post(&format!("{url}/api/v1/teams/room"), r#"{"body":"hi"}"#).await;
        assert_eq!(resp.status(), 200, "POST is the human door now");

        for method in [reqwest::Method::PUT, reqwest::Method::PATCH] {
            let resp = reqwest::Client::new()
                .request(method.clone(), format!("{url}/api/v1/teams/room"))
                .send()
                .await
                .expect("request");
            assert_eq!(resp.status(), 405, "{method}");
            assert_eq!(err_code(resp).await, "method_not_allowed", "{method}");
        }
    }

    // ── the room's write side (STUDIO-653, T6) ──────────────────────────────────────────────────

    /// A Teams runtime with a MULTI-name roster and a room, so a direct post has a real recipient
    /// to be validated against. (`teams_memory` above is deliberately a one-identity roster; these
    /// tests need more than one name and build their own rather than widening it.)
    fn teams_memory_with_roster(
        dir: &TempDir,
        names: &[&str],
    ) -> (Arc<TeamsMemory>, Arc<LocalRoom>) {
        let teams = Teams {
            enabled: true,
            roster: names
                .iter()
                .map(|n| Identity {
                    name: (*n).to_string(),
                    profile: "swe".to_string(),
                    ..Identity::default()
                })
                .collect(),
            ..Teams::disabled()
        };
        let bank = LocalBank::new(dir.0.join(DEFAULT_BANKS_SUBDIR), "agent-");
        let room = Arc::new(LocalRoom::new(dir.0.join(DEFAULT_ROOM_SUBDIR)));
        let mem = Arc::new(
            TeamsMemory::new(Arc::new(teams), Arc::new(bank))
                .with_room(Arc::clone(&room) as Arc<dyn RoomLog>),
        );
        (mem, room)
    }

    /// Binds `run_id` to `identity`, the way the control task does at dispatch.
    fn bind(mem: &TeamsMemory, run_id: i64, identity: &str) {
        mem.bind_run(
            run_id,
            RunProvenance {
                identity: identity.to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
    }

    /// **The post is host-stamped, end to end through HTTP.** The body invents a `from` and an
    /// `identity`; both are ignored, exactly the way `retain_recall_invalidate_round_trip` proves it
    /// for a retained record's provenance (§0.11.4 — "a run cannot supply it"). The message lands in
    /// the room log with the RUN's identity as its author.
    #[tokio::test]
    async fn a_post_is_stamped_from_the_run_never_from_the_body() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(Arc::clone(&mem)),
        ))
        .await;

        let view = body_json(
            post(
                &format!("{url}/api/v1/runs/7/post"),
                r#"{"body":"the mirror lock is per-repo","from":"bob","identity":"@manager","refs":["MT-9"]}"#,
            )
            .await,
        )
        .await;
        assert_eq!(
            view["from"], "alice",
            "`from` is the RUN's identity, not the body's: {view}"
        );
        assert_eq!(view["to"], "*", "no `to` ⇒ the room: {view}");
        assert_eq!(view["refs"][0], "MT-9");
        assert!(
            view["id"].as_str().unwrap_or_default().contains(':'),
            "the response carries the log's file:seq id: {view}"
        );

        let caught = room
            .read_since("bob", &Cursor::default(), 10)
            .expect("read");
        assert_eq!(caught.messages.len(), 1);
        assert_eq!(
            caught.messages[0].from, "alice",
            "the LOG's author is the run's identity"
        );
    }

    /// A direct post names its recipient, and only that recipient catches it up (§0.5's one log,
    /// two audiences).
    #[tokio::test]
    async fn a_direct_post_is_addressed_and_only_its_recipient_reads_it() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_roster(&dir, &["alice", "bob", "carol"]);
        bind(&mem, 7, "alice");
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        let view = body_json(
            post(
                &format!("{url}/api/v1/runs/7/post"),
                r#"{"body":"bob, the lock moved","to":"bob"}"#,
            )
            .await,
        )
        .await;
        assert_eq!(view["to"], "bob");
        // No control loop behind this fake provider, so nothing is delivered live; the post is in
        // the log, which IS the fallback (§0.5).
        assert_eq!(view["delivered"], 0);

        let seen = |reader: &str| {
            room.read_since(reader, &Cursor::default(), 10)
                .expect("read")
                .messages
                .len()
        };
        assert_eq!(seen("bob"), 1);
        assert_eq!(seen("carol"), 0, "`to: bob` must never render in carol's");
    }

    /// An unknown `to` is refused LOUDLY, naming the roster tool — never a silent room post. A
    /// message its author believed was private must not quietly become public.
    #[tokio::test]
    async fn an_unknown_recipient_is_a_loud_bad_request() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;

        let resp = post(
            &format!("{url}/api/v1/runs/7/post"),
            r#"{"body":"psst","to":"dave"}"#,
        )
        .await;
        assert_eq!(resp.status(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "bad_request");
        let msg = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("dave") && msg.contains("teams_roster"),
            "{msg}"
        );
        assert_eq!(
            room.read_since("", &Cursor::default(), 10)
                .expect("read")
                .messages
                .len(),
            0,
            "a refused post must not land in the room"
        );
    }

    /// A post from a run the host has no binding for is `not_running`, not a message attributed to
    /// a guess — the same answer retain gives, and the reason a run wearing no identity cannot post.
    #[tokio::test]
    async fn a_post_from_an_unbound_run_is_not_running() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;
        let resp = post(&format!("{url}/api/v1/runs/99/post"), r#"{"body":"hi"}"#).await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "not_running");
        assert_eq!(
            room.read_since("", &Cursor::default(), 10)
                .expect("read")
                .messages
                .len(),
            0
        );
    }

    /// An empty body is refused: a blank line in the room is noise in every teammate's turn-1
    /// prompt, forever.
    #[tokio::test]
    async fn an_empty_post_is_rejected() {
        let dir = TempDir::new();
        let (mem, _room) = teams_memory_with_room(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await;
        let resp = post(&format!("{url}/api/v1/runs/7/post"), r#"{"body":"   "}"#).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "bad_request");
    }

    // ── the room's human door (STUDIO-661) ─────────────────────────────────────────────────────

    /// **The operator posts, and the daemon stamps who.** The body carries `body` and `refs` and
    /// nothing else; the post lands in the log as `from: "operator"` and reads straight back
    /// through the room's own GET — the same log, the same window, the same rendering.
    #[tokio::test]
    async fn an_operator_post_lands_in_the_room_stamped_operator() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        let url = spawn_with(mem).await;

        let resp = post(
            &format!("{url}/api/v1/teams/room"),
            r#"{"body":"prefer the retry queue for STUDIO-6xx, see the design doc","refs":["STUDIO-661"]}"#,
        )
        .await;
        assert_eq!(resp.status(), 200);
        let view = body_json(resp).await;
        assert_eq!(view["from"], "operator");
        assert_eq!(view["to"], "*", "v1 is room-wide only");
        assert_eq!(view["refs"][0], "STUDIO-661");
        assert_eq!(view["delivered"], 0, "the room is a log, not a bus");

        // In the log, once, and readable by anyone catching up.
        let caught = room
            .read_since("alice", &Cursor::default(), 10)
            .expect("read");
        assert_eq!(caught.messages.len(), 1);
        assert_eq!(caught.messages[0].from, "operator");
        assert_eq!(
            caught.messages[0].body,
            "prefer the retry queue for STUDIO-6xx, see the design doc"
        );

        // And through the endpoint's own read side, agreeing byte for byte with what was echoed.
        let served = body_json(
            reqwest::get(format!("{url}/api/v1/teams/room"))
                .await
                .expect("GET"),
        )
        .await;
        assert_eq!(served["messages"][0]["from"], "operator");
        assert_eq!(served["messages"][0]["id"], view["id"]);
        assert_eq!(served["messages"][0]["at"], view["at"]);
    }

    /// **A body-supplied `from` is ignored** — the T4/T6 forgery-test pattern applied to the human
    /// door. There is no field an operator (or anything else reaching loopback) can add to make a
    /// post look like a teammate's: the daemon stamps the name, full stop.
    #[tokio::test]
    async fn a_body_supplied_from_is_ignored() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        let url = spawn_with(mem).await;

        let resp = post(
            &format!("{url}/api/v1/teams/room"),
            r#"{"from":"alice","identity":"alice","to":"bob","body":"not from alice"}"#,
        )
        .await;
        assert_eq!(resp.status(), 200);
        let view = body_json(resp).await;
        assert_eq!(view["from"], "operator", "`from` is host-stamped");
        assert_eq!(
            view["to"], "*",
            "and `to` is not a field on this door at all"
        );

        let caught = room
            .read_since("bob", &Cursor::default(), 10)
            .expect("read");
        assert_eq!(
            caught.messages.len(),
            1,
            "it is a ROOM post, so bob sees it"
        );
        assert_eq!(caught.messages[0].from, "operator");
    }

    /// An empty body is refused: a blank line in the room is noise in every teammate's turn-1
    /// prompt, forever — the same rule a teammate's post runs through.
    #[tokio::test]
    async fn an_empty_operator_post_is_rejected() {
        let dir = TempDir::new();
        let (mem, room) = teams_memory_with_room(&dir);
        let url = spawn_with(mem).await;
        for body in [r#"{"body":"   "}"#, "{}"] {
            let resp = post(&format!("{url}/api/v1/teams/room"), body).await;
            assert_eq!(resp.status(), 400, "{body}");
            assert_eq!(err_code(resp).await, "bad_request", "{body}");
        }
        assert_eq!(
            room.read_since("", &Cursor::default(), 10)
                .expect("read")
                .messages
                .len(),
            0,
            "a refused post writes nothing"
        );
    }

    /// A transcript pasted into the compose box is refused at the door rather than silently
    /// truncated on the way to disk, exactly like a teammate's oversized post.
    #[tokio::test]
    async fn an_oversized_operator_post_is_rejected() {
        let dir = TempDir::new();
        let (mem, _room) = teams_memory_with_room(&dir);
        let url = spawn_with(mem).await;
        let huge = "x".repeat(super::MAX_RETAIN_BODY + 1);
        let resp = post(
            &format!("{url}/api/v1/teams/room"),
            &format!(r#"{{"body":"{huge}"}}"#),
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "content_too_long");
    }
}
