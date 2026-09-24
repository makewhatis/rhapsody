//! handlers_provider_config — authoring provider definitions from Settings (STUDIO-1048).
//! Rhapsody-only: the frozen Go daemon has no provider concept, so this route is an ADDITIVE
//! divergence (README "Divergences").
//!
//! ```text
//! POST /api/v1/providers/config → add | edit | remove a definition; echoes the config view
//! ```
//!
//! Definitions are NON-SECRET configuration, so this route is reachable from both the desktop app
//! and the browser dashboard — behind the shared operator-write guard (STUDIO-982), exactly like
//! `/api/v1/config`. Key actions stay desktop-only and are NOT here.
//!
//! The write is deliberately NOT the typed `/api/v1/config` POST: that path decodes to a typed
//! `Config` and re-encodes the whole file, which would re-serialize (and drop every comment in) the
//! operator's WORKFLOW.md. Instead the definition is spliced into the file by
//! [`rhapsody_config::apply_provider_edit`], preserving every byte outside the `providers:` block,
//! and the CANDIDATE file is then run through the daemon's own load pipeline
//! (`StateProvider::validate_config`) so the operator sees the daemon's own error text.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_config::provider_edit::{EditError, ProviderOp};
use rhapsody_config::providers::{
    BrokerLimits, CREDENTIAL_SOURCE_KEYCHAIN, CredentialSource, PROTOCOL_OPENAI_COMPATIBLE,
    ProviderDefinition,
};
use rhapsody_config::workflow::{load, parse, save_text};
use serde::Deserialize;

use crate::handlers::require_post;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Caps the request body. A provider definition is a few hundred bytes; 64 KiB is generous and
/// bounds abuse on the loopback socket.
const MAX_PROVIDER_BODY: usize = 64 * 1024;

/// The `POST /api/v1/providers/config` body.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ProviderMutationReq {
    /// `add` | `edit` | `remove`.
    op: String,
    /// The canonical provider id (the YAML map key).
    provider_id: String,
    /// The id to rename FROM on an edit; defaults to `provider_id`.
    previous_id: Option<String>,
    /// The definition to write on add/edit.
    definition: Option<ProviderDefinitionReq>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProviderDefinitionReq {
    protocol: String,
    display_name: String,
    base_url: String,
    allow_insecure_http: bool,
    limits: Option<LimitsReq>,
}

/// The optional broker-limits block. Every field is optional; an absent one keeps the V1 default.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LimitsReq {
    forwarded_requests_per_turn: Option<u32>,
    denied_requests_before_revocation: Option<u32>,
    concurrent_upstream_requests_per_turn: Option<u32>,
    json_request_bytes: Option<u64>,
    aggregate_request_bytes_per_turn: Option<u64>,
    response_bytes_per_request: Option<u64>,
    aggregate_response_bytes_per_turn: Option<u64>,
    requested_output_tokens_per_request: Option<u64>,
    reserved_token_units_per_turn: Option<u64>,
    reserved_token_units_per_session: Option<u64>,
    capability_lifetime_ms: Option<u64>,
    max_reserved_token_units_per_utc_day: Option<u64>,
}

impl ProviderDefinitionReq {
    /// The typed definition to write. `id` is stamped from the request key. The credential source
    /// is DERIVED here as the config layer derives it (the one v1 storage kind) — the client never
    /// sends a credential, and WORKFLOW.md never stores a value.
    fn to_definition(&self, id: &str) -> ProviderDefinition {
        let mut limits = BrokerLimits::default();
        if let Some(l) = &self.limits {
            if let Some(v) = l.forwarded_requests_per_turn {
                limits.forwarded_requests_per_turn = v;
            }
            if let Some(v) = l.denied_requests_before_revocation {
                limits.denied_requests_before_revocation = v;
            }
            if let Some(v) = l.concurrent_upstream_requests_per_turn {
                limits.concurrent_upstream_requests_per_turn = v;
            }
            if let Some(v) = l.json_request_bytes {
                limits.json_request_bytes = v;
            }
            if let Some(v) = l.aggregate_request_bytes_per_turn {
                limits.aggregate_request_bytes_per_turn = v;
            }
            if let Some(v) = l.response_bytes_per_request {
                limits.response_bytes_per_request = v;
            }
            if let Some(v) = l.aggregate_response_bytes_per_turn {
                limits.aggregate_response_bytes_per_turn = v;
            }
            if let Some(v) = l.requested_output_tokens_per_request {
                limits.requested_output_tokens_per_request = v;
            }
            if let Some(v) = l.reserved_token_units_per_turn {
                limits.reserved_token_units_per_turn = v;
            }
            if let Some(v) = l.reserved_token_units_per_session {
                limits.reserved_token_units_per_session = v;
            }
            limits.capability_lifetime_ms = l.capability_lifetime_ms;
            limits.max_reserved_token_units_per_utc_day = l.max_reserved_token_units_per_utc_day;
        }
        ProviderDefinition {
            id: id.to_string(),
            protocol: if self.protocol.is_empty() {
                PROTOCOL_OPENAI_COMPATIBLE.to_string()
            } else {
                self.protocol.clone()
            },
            display_name: self.display_name.clone(),
            base_url: self.base_url.clone(),
            allow_insecure_http: self.allow_insecure_http,
            credential: CredentialSource {
                source: CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: limits,
        }
    }
}

/// `POST /api/v1/providers/config` — add, edit or remove a `providers.<id>` definition. Validates
/// the spliced candidate through the daemon's own pipeline; a removal that something still selects
/// is refused with every reference listed.
pub(crate) async fn handle_provider_config(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to add, edit or remove a provider") {
        return resp;
    }
    if body.len() > MAX_PROVIDER_BODY {
        return write_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("provider config body must be at most {MAX_PROVIDER_BODY} bytes"),
            None,
        );
    }
    let req: ProviderMutationReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                err.to_string(),
                None,
            );
        }
    };
    let op = match req.op.as_str() {
        "add" => ProviderOp::Add,
        "edit" => ProviderOp::Edit,
        "remove" => ProviderOp::Remove,
        other => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("unknown op {other:?}; want add, edit or remove"),
                None,
            );
        }
    };

    // A removal is refused (before any write) when the provider is still selected somewhere.
    if op == ProviderOp::Remove {
        let references = provider.provider_references(&req.provider_id);
        if !references.is_empty() {
            return write_json(
                StatusCode::CONFLICT,
                &serde_json::json!({
                    "error": {
                        "code": "provider_in_use",
                        "message": format!(
                            "provider {:?} is still selected and cannot be removed",
                            req.provider_id
                        ),
                        "references": references
                            .iter()
                            .map(|r| serde_json::json!({ "kind": r.kind, "label": r.label }))
                            .collect::<Vec<_>>(),
                    }
                }),
            );
        }
    }

    let path = provider.workflow_path();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_unavailable",
                err.to_string(),
                None,
            );
        }
    };

    let definition = if op == ProviderOp::Remove {
        None
    } else {
        let Some(def_req) = req.definition.as_ref() else {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "add and edit require a definition",
                None,
            );
        };
        Some(def_req.to_definition(&req.provider_id))
    };

    let candidate = match rhapsody_config::apply_provider_edit(
        &text,
        op,
        &req.provider_id,
        req.previous_id.as_deref(),
        definition.as_ref(),
    ) {
        Ok(candidate) => candidate,
        Err(err) => return edit_error_response(err, &req.provider_id),
    };

    // Validate the candidate through the daemon's own load pipeline so an operator sees exactly the
    // error text the daemon would emit on reload, and a rejected edit never touches the disk file.
    let candidate_def = match parse(&candidate) {
        Ok(def) => def,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_config",
                err.to_string(),
                None,
            );
        }
    };
    if let Err(err) = provider.validate_config(&candidate_def) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_config",
            err.to_string(),
            None,
        );
    }

    if let Err(err) = save_text(std::path::Path::new(path), &candidate) {
        return write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_write_failed",
            err.to_string(),
            None,
        );
    }
    match load(std::path::Path::new(path)) {
        Ok(saved) => write_json(
            StatusCode::OK,
            &rhapsody_config::effective_json::render(&saved),
        ),
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_unavailable",
            err.to_string(),
            None,
        ),
    }
}

/// Map an [`EditError`] to the wire envelope: a missing provider is 404, a duplicate 409, an
/// invalid id / unparseable block 400, a serialization failure 500.
fn edit_error_response(err: EditError, id: &str) -> Response {
    match err {
        EditError::NotFound(_) => write_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            format!("no configured provider with id {id:?}"),
            None,
        ),
        EditError::AlreadyExists(_) => write_error(
            StatusCode::CONFLICT,
            "provider_exists",
            format!("a provider with id {id:?} already exists"),
            None,
        ),
        EditError::InvalidId(reason) => {
            write_error(StatusCode::BAD_REQUEST, "invalid_provider_id", reason, None)
        }
        EditError::NotAMap => write_error(
            StatusCode::BAD_REQUEST,
            "invalid_config",
            err.to_string(),
            None,
        ),
        EditError::Serialize(reason) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_write_failed",
            reason,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::{Value, json};

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, operator_client, spawn_router};

    /// A comment-rich, VALID workflow the edits are spliced into. `$HOME` is a reliably-set env var
    /// the config resolver expands (the same idiom the config POST tests use), and opencode is the
    /// one backend v1 materializes providers for.
    const BASE: &str = r"---
# the operator's own notes live in this file
tracker:
  kind: linear
  api_key: $HOME
  project_slug: symphony
agent:
  backend: opencode
# a dated note next to a tunable
opencode:
  turn_timeout_ms: 1800000
---
Do the work for {{ issue.identifier }}.
";

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempWorkflow {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempWorkflow {
        fn new(body: &str) -> TempWorkflow {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rhapsody-httpapi-provider-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            let path = dir.join("WORKFLOW.md");
            fs::write(&path, body).expect("write WORKFLOW.md");
            TempWorkflow { dir, path }
        }

        fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.path).expect("read WORKFLOW.md")
        }
    }

    impl Drop for TempWorkflow {
        fn drop(&mut self) {
            if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
    }

    async fn spawn(path: &str, fake: FakeProvider) -> String {
        let fake = fake.with_workflow_path(path.to_string());
        spawn_router(new_handler(Arc::new(fake), None)).await
    }

    async fn post(base: &str, payload: &Value) -> (u16, Value) {
        let resp = operator_client()
            .post(format!("{base}/api/v1/providers/config"))
            .json(payload)
            .send()
            .await
            .expect("POST /providers/config");
        let status = resp.status().as_u16();
        let text = resp.text().await.expect("body");
        (status, serde_json::from_str(&text).expect("json"))
    }

    fn add_body(id: &str, base_url: &str) -> Value {
        json!({
            "op": "add",
            "provider_id": id,
            "definition": {
                "protocol": "openai-compatible",
                "display_name": "Fireworks",
                "base_url": base_url,
            }
        })
    }

    #[tokio::test]
    async fn add_splices_a_definition_and_preserves_comments() {
        let wf = TempWorkflow::new(BASE);
        let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = post(
            &base,
            &add_body("fireworks", "https://api.fireworks.ai/inference/v1"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let text = wf.read();
        assert!(text.contains("fireworks:"), "{text}");
        assert!(
            text.contains("base_url: https://api.fireworks.ai/inference/v1"),
            "{text}"
        );
        // Both comments survive — the proof the write did not re-serialize the file.
        assert!(
            text.contains("# the operator's own notes live in this file"),
            "{text}"
        );
        assert!(text.contains("# a dated note next to a tunable"), "{text}");
        // The echoed view carries the new registry.
        let providers = body["global"]["providers"].as_object().expect("providers");
        assert!(providers.contains_key("fireworks"), "{body}");
    }

    #[tokio::test]
    async fn plain_http_without_the_opt_in_is_refused_with_the_daemon_text() {
        let wf = TempWorkflow::new(BASE);
        let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = post(&base, &add_body("plain", "http://plain.example/v1")).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["code"], "invalid_config");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("allow_insecure_http"), "{body}");
        // The on-disk file is untouched by a refused edit.
        assert!(!wf.read().contains("plain"), "{}", wf.read());
    }

    #[tokio::test]
    async fn insecure_http_is_accepted_with_the_opt_in() {
        let wf = TempWorkflow::new(BASE);
        let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;
        let mut payload = add_body("plain", "http://plain.example/v1");
        payload["definition"]["allow_insecure_http"] = json!(true);
        let (status, body) = post(&base, &payload).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            wf.read().contains("allow_insecure_http: true"),
            "{}",
            wf.read()
        );
    }

    #[tokio::test]
    async fn remove_refuses_a_referenced_provider_listing_every_reference() {
        let wf = TempWorkflow::new(BASE);
        let fake = FakeProvider::ok(empty_snapshot()).with_provider_references(
            "fireworks",
            vec![
                rhapsody_config::ProviderReference {
                    kind: "global".to_string(),
                    label: "the global default (agent.provider)".to_string(),
                },
                rhapsody_config::ProviderReference {
                    kind: "roster".to_string(),
                    label: "roster entry \"jerry\"".to_string(),
                },
            ],
        );
        let base = spawn(&wf.path(), fake).await;
        let (status, body) = post(
            &base,
            &json!({ "op": "remove", "provider_id": "fireworks" }),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "provider_in_use");
        let refs = body["error"]["references"].as_array().expect("references");
        assert_eq!(refs.len(), 2, "{body}");
        assert!(
            refs.iter().any(|r| r["label"] == "roster entry \"jerry\""),
            "{body}"
        );
    }

    #[tokio::test]
    async fn edit_then_remove_round_trips_through_the_file() {
        let wf = TempWorkflow::new(BASE);
        let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = post(
            &base,
            &add_body("fireworks", "https://api.fireworks.ai/inference/v1"),
        )
        .await;
        assert_eq!(status, 200, "{body}");

        let (status, body) = post(
            &base,
            &json!({
                "op": "edit",
                "provider_id": "fireworks",
                "definition": {
                    "protocol": "openai-compatible",
                    "display_name": "Fireworks",
                    "base_url": "https://api.fireworks.ai/inference/v2",
                }
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(wf.read().contains("inference/v2"), "{}", wf.read());
        assert!(!wf.read().contains("inference/v1"), "{}", wf.read());

        let (status, body) = post(
            &base,
            &json!({ "op": "remove", "provider_id": "fireworks" }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(!wf.read().contains("providers:"), "{}", wf.read());
        // Every comment still present after add → edit → remove.
        assert!(
            wf.read()
                .contains("# the operator's own notes live in this file")
        );
        assert!(wf.read().contains("# a dated note next to a tunable"));
    }

    #[tokio::test]
    async fn unknown_op_and_unknown_remove_are_rejected() {
        let wf = TempWorkflow::new(BASE);
        let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;
        let (status, body) = post(&base, &json!({ "op": "frobnicate", "provider_id": "x" })).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["code"], "invalid_request");

        let (status, body) =
            post(&base, &json!({ "op": "remove", "provider_id": "missing" })).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "provider_not_found");
    }

    // ---------------------------------------------------------------------------------------------
    // End-to-end: Settings add → reload → Not connected → (fake desktop connect) → Test connection
    // → endpoint edit → binding_mismatch.
    //
    // The desktop half is represented at its daemon boundary: the desktop command stores the key and
    // the daemon observes a new bound owner revision (the coordinator's `begin_mutation_refresh` +
    // a `Present` read). The credential owner here is an in-memory fake (no Keychain, no persistent
    // test credential), and the "provider" is a loopback HTTP server the catalog refresh really
    // contacts — so the credentialed leg is exercised over a socket, no paid provider involved.
    // ---------------------------------------------------------------------------------------------
    mod e2e {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};

        use async_trait::async_trait;
        use rhapsody_credential_ipc::domain::{Binding, BoundCredentialLease, Revision};
        use rhapsody_provider_status::ModelEntry;
        use rhapsody_provider_status::coordinator::{
            CredentialReadSource, ObservedRead, ObservedState, ProviderConfig, RefreshCoordinator,
        };
        use rhapsody_provider_status::discovery::{
            DiscoveredCatalog, DiscoveryRequest, ModelDiscovery,
        };
        use rhapsody_provider_status::error::CatalogError;

        use super::*;

        /// An in-memory credential owner: stores one (binding, value) pair and answers `Present`
        /// only for an exact binding match, else `BindingMismatch` — exactly the owner behaviour the
        /// daemon relies on to move status to `binding_mismatch` after an endpoint edit.
        struct FakeOwner {
            stored: Mutex<Option<(Binding, String)>>,
            revision: Mutex<Revision>,
        }

        impl FakeOwner {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    stored: Mutex::new(None),
                    revision: Mutex::new(Revision::INITIAL),
                })
            }

            fn store(&self, binding: Binding) {
                *self.stored.lock().unwrap() = Some((binding, "sk-fake-loopback".to_string()));
                let mut rev = self.revision.lock().unwrap();
                *rev = rev.next();
            }
        }

        #[async_trait]
        impl CredentialReadSource for FakeOwner {
            async fn read_bound(&self, _account: String, binding: Binding) -> ObservedRead {
                let revision = *self.revision.lock().unwrap();
                let state = match self.stored.lock().unwrap().as_ref() {
                    None => ObservedState::Absent,
                    Some((stored, value)) if *stored == binding => {
                        ObservedState::Present(BoundCredentialLease::new(binding, value.clone()))
                    }
                    Some(_) => ObservedState::BindingMismatch,
                };
                ObservedRead {
                    state,
                    owner_revision: revision,
                    availability_generation: Revision::INITIAL,
                }
            }
        }

        /// A loopback fake OpenAI-compatible provider: a real TCP server answering every request
        /// with one model, so a catalog refresh genuinely reaches a socket.
        struct FakeProviderServer {
            port: u16,
            hits: Arc<AtomicU64>,
        }

        impl FakeProviderServer {
            fn start() -> FakeProviderServer {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
                let port = listener.local_addr().expect("addr").port();
                let hits = Arc::new(AtomicU64::new(0));
                let hits_thread = Arc::clone(&hits);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else { break };
                        let _ = hits_thread.fetch_add(1, Ordering::SeqCst);
                        // Drain the request line + headers, then answer.
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        let body = r#"{"data":[{"id":"loopback-model"}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                });
                FakeProviderServer { port, hits }
            }

            fn base_url(&self) -> String {
                format!("http://127.0.0.1:{}/v1", self.port)
            }
        }

        struct LoopbackDiscovery;

        #[async_trait]
        impl ModelDiscovery for LoopbackDiscovery {
            async fn list_models(
                &self,
                request: DiscoveryRequest,
            ) -> Result<DiscoveredCatalog, CatalogError> {
                let url = format!("{}/models", request.endpoint);
                let resp = reqwest::get(&url)
                    .await
                    .map_err(|_| CatalogError::Transport)?;
                let text = resp.text().await.map_err(|_| CatalogError::Malformed)?;
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).map_err(|_| CatalogError::Malformed)?;
                let entries = parsed["data"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r["id"].as_str())
                            .map(|id| ModelEntry {
                                id: id.to_string(),
                                display_name: None,
                                capabilities: Vec::new(),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(DiscoveredCatalog {
                    entries,
                    truncated: false,
                })
            }
        }

        /// The provider's binding as the composition root derives it (`ProviderDefinition::
        /// credential_binding`).
        fn binding_of(path: &str, id: &str) -> Binding {
            let def = rhapsody_config::workflow::load(std::path::Path::new(path)).expect("load");
            let config = rhapsody_config::decode::decode(&def).expect("decode");
            let pdef = config.providers.get(id).expect("provider defined").clone();
            let b = pdef.credential_binding().expect("binding");
            Binding {
                provider_id: b.provider_id,
                adapter: b.adapter,
                base_url: b.base_url,
            }
        }

        fn provider_config(path: &str, id: &str, allow_insecure_http: bool) -> ProviderConfig {
            ProviderConfig {
                provider_id: id.to_string(),
                binding: binding_of(path, id),
                allow_insecure_http,
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn settings_add_connect_test_and_edit_reaches_binding_mismatch() {
            let wf = TempWorkflow::new(BASE);
            let server = FakeProviderServer::start();
            let base = spawn(&wf.path(), FakeProvider::ok(empty_snapshot())).await;

            // 1. Add a provider through the Settings path, pointed at the loopback fake. The fake
            //    is plaintext http, so the operator opt-in accompanies it.
            let mut add = add_body("loop", &server.base_url());
            add["definition"]["allow_insecure_http"] = json!(true);
            let (status, body) = post(&base, &add).await;
            assert_eq!(status, 200, "{body}");

            // 2. The daemon reloads; the definition is now on disk and decodes.
            assert!(wf.read().contains("loop:"), "{}", wf.read());

            // 3. Status reads Not connected (absent) before any key is stored.
            let owner = FakeOwner::new();
            let coordinator = RefreshCoordinator::new(owner.clone(), Arc::new(LoopbackDiscovery));
            let intents = coordinator.apply_reload(1, &[provider_config(&wf.path(), "loop", true)]);
            assert_eq!(intents.len(), 1);
            coordinator.refresh_status(&intents[0]).await;
            let view = coordinator.status_view("loop", true).expect("tracked");
            assert_eq!(view.status, "absent", "Not connected");

            // 4. Store a key through the desktop command (its daemon-visible effect: a bound owner).
            owner.store(binding_of(&wf.path(), "loop"));
            let intent = coordinator.begin_mutation_refresh("loop").expect("known");
            coordinator.refresh_status(&intent).await;
            assert_eq!(
                coordinator
                    .status_view("loop", true)
                    .expect("tracked")
                    .status,
                "configured"
            );

            // 5. Test connection succeeds: a real credentialed catalog refresh over the loopback.
            let catalog = coordinator.refresh_catalog("loop").await.expect("catalog");
            assert!(catalog.error.is_none(), "{catalog:?}");
            assert_eq!(catalog.models.len(), 1);
            assert!(
                server.hits.load(Ordering::SeqCst) >= 1,
                "the fake provider was contacted"
            );

            // 6. Change the URL through the Settings path; status reaches binding_mismatch.
            let (status, body) = post(
                &base,
                &json!({
                    "op": "edit",
                    "provider_id": "loop",
                    "definition": {
                        "protocol": "openai-compatible",
                        "display_name": "Loopback",
                        "base_url": "http://127.0.0.1:1/v1",
                        "allow_insecure_http": true,
                    }
                }),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let intents = coordinator.apply_reload(2, &[provider_config(&wf.path(), "loop", true)]);
            assert_eq!(intents.len(), 1);
            coordinator.refresh_status(&intents[0]).await;
            let view = coordinator.status_view("loop", true).expect("tracked");
            assert_eq!(
                view.status, "binding_mismatch",
                "the old key must not follow the new URL"
            );
        }
    }
}
