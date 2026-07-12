//! handlers_config — the read-write config endpoint `GET`/`POST /api/v1/config`. Parity port of
//! `$REF/internal/httpapi/handlers_config.go` (`handleConfig`/`handleConfigGet`/`handleConfigPost`).
//!
//! * `GET`/`HEAD` → the current on-disk WORKFLOW.md as the `{config, prompt_body, global, projects}`
//!   view (front matter + prompt body VERBATIM, pre-`$VAR` resolution, so the `api_key` indirection
//!   is preserved and no secret leaks). The response is `effective_json::render`, REUSED (the plan's
//!   byte-parity rule) — not reimplemented here.
//! * `POST` → validate the submitted config exactly as the daemon does at load time (Decode → Resolve
//!   → ValidateDispatch → buildEffective) and, only if valid, atomically rewrite WORKFLOW.md; the
//!   watcher then hot-reloads it. Invalid configs are rejected with 400 and the on-disk file is left
//!   untouched, so a bad edit can never corrupt a working config.
//! * Any other method → 405. Loopback-only binding keeps the write path safe (the server binds
//!   127.0.0.1).

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_config::effective_json;
use rhapsody_config::workflow::{load, save};

use crate::config_view::{ConfigPostReq, build_typed_definition, classify_config_error};
use crate::responses::{write_error, write_error_fields, write_json};
use crate::server::StateProvider;

/// Caps a POST body. WORKFLOW.md is tens of KB in practice; 1 MiB bounds abuse on the loopback socket
/// while sitting comfortably above any real config (Go `maxConfigBody`).
const MAX_CONFIG_BODY: usize = 1 << 20;

/// `GET`/`HEAD`/`POST /api/v1/config`, dispatched by method (Go `handleConfig`). Registered
/// method-agnostically so a mismatch yields an explicit 405 envelope rather than the SPA fallback.
pub(crate) async fn handle_config(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    match method {
        Method::GET | Method::HEAD => handle_config_get(provider.as_ref()),
        Method::POST => handle_config_post(provider.as_ref(), &body),
        _ => write_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use GET to read or POST to update config",
            Some("GET, HEAD, POST"),
        ),
    }
}

/// `GET /api/v1/config` — the current on-disk WORKFLOW.md rendered via the REUSED
/// `effective_json::render`. A load failure is a 500 `config_unavailable`. Mirrors Go
/// `handleConfigGet`.
fn handle_config_get(provider: &dyn StateProvider) -> Response {
    match load(Path::new(provider.workflow_path())) {
        Ok(def) => write_json(StatusCode::OK, &effective_json::render(&def)),
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_unavailable",
            err.to_string(),
            None,
        ),
    }
}

/// `POST /api/v1/config` — decode → validate → (only if valid) atomically rewrite → echo the
/// persisted round-trip. A decode failure is 400 `invalid_request`; a validation failure is 400
/// (`invalid_config`, or a structured field error on the typed path) and the on-disk file is left
/// UNTOUCHED; a write/reload failure is 500. Mirrors Go `handleConfigPost`.
fn handle_config_post(provider: &dyn StateProvider, body: &Bytes) -> Response {
    if body.len() > MAX_CONFIG_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body exceeds 1 MiB",
            None,
        );
    }
    let req: ConfigPostReq = match serde_json::from_slice(body) {
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
    let path = provider.workflow_path();

    // Build the definition to validate + persist. The typed path patches the edit onto the on-disk
    // config and re-encodes; the legacy path persists the submitted front-matter verbatim.
    let def = if req.is_typed() {
        match build_typed_definition(path, &req) {
            Ok(def) => def,
            Err(msg) => {
                return write_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "config_unavailable",
                    msg,
                    None,
                );
            }
        }
    } else {
        req.legacy_definition()
    };

    // Validate through the daemon's FULL load pipeline so a config the API accepts is one the daemon
    // will accept on the subsequent hot-reload. On the TYPED path, surface structured field errors the
    // UI can attach to inputs; the legacy verbatim-map path keeps the opaque `invalid_config` code.
    if let Err(err) = provider.validate_config(&def) {
        return if req.is_typed() {
            let (code, fields) = classify_config_error(&err);
            write_error_fields(StatusCode::BAD_REQUEST, code, err.to_string(), fields)
        } else {
            write_error(
                StatusCode::BAD_REQUEST,
                "invalid_config",
                err.to_string(),
                None,
            )
        };
    }

    // Persist atomically (the watcher never sees a half-written file), then re-read + echo it back,
    // confirming the persisted round-trip to the caller.
    if let Err(err) = save(Path::new(path), &def) {
        return write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_write_failed",
            err.to_string(),
            None,
        );
    }
    match load(Path::new(path)) {
        Ok(saved) => write_json(StatusCode::OK, &effective_json::render(&saved)),
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_unavailable",
            err.to_string(),
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
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    // The mirrored Go fixtures use `api_key: $LINEAR_API_KEY` + `t.Setenv`. Under Rust 2024's
    // parallel-test env model the config crate's own tests instead use `$HOME` (a reliably-set var
    // resolveVar expands) to exercise the identical resolve→validate path with no `unsafe { set_var }`
    // / data race; these tests follow that pattern. The `$VAR` indirection is still preserved verbatim
    // on disk (the "no secret leak" property), and it resolves to a non-empty value so ValidateDispatch
    // accepts it.
    const SAMPLE_WORKFLOW_MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  project_slug: symphony\n\
agent:\n  backend: claude\n\
---\n\
Do the work for {{ issue.identifier }}.\n";

    // NB: keep each indented YAML block embedded within a single source line (via `\n  `); a `\`
    // line-continuation strips the NEXT source line's leading whitespace, which would corrupt the
    // indentation. So the whole `projects:` list lives on one (long) source line.
    const MULTI_PROJECT_WORKFLOW_MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  active_states:\n    - Todo\n    - In Progress\n  terminal_states:\n    - Done\n    - Cancelled\n  review_promote_state: In Progress\n\
repo: git@github.com:o/infra.git\n\
agent:\n  backend: claude\n  max_concurrent_agents: 8\n\
claude:\n  model: claude-sonnet-4-6\n  permission_mode: bypassPermissions\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n    milestone: David's Tasks\n    active_states:\n      - Todo\n      - In Progress\n      - In Review\n    claude:\n      model: claude-opus-4-8\n  - name: Core Bot\n    slugs:\n      - core-proj\n    repo: git@github.com:o/core.git\n  - name: Paused Bot\n    slugs:\n      - paused-proj\n    enabled: false\n\
---\n\
Default prompt for {{ issue.identifier }}.\n";

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII temp dir holding a WORKFLOW.md (mirrors Go `t.TempDir()` + `writeTempWorkflow`); the config
    /// endpoints read/write the returned path. Same idiom as the config/store crates' test temp dirs.
    struct TempWorkflow {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempWorkflow {
        fn new(body: &str) -> TempWorkflow {
            let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rhapsody-httpapi-config-{}-{n}",
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
    }

    impl Drop for TempWorkflow {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    async fn spawn(workflow_path: &str) -> String {
        let provider = FakeProvider::ok(empty_snapshot()).with_workflow_path(workflow_path);
        spawn_router(new_handler(Arc::new(provider), None)).await
    }

    async fn get_config(base: &str) -> (u16, Value) {
        let resp = reqwest::get(format!("{base}/api/v1/config"))
            .await
            .expect("GET /config");
        let status = resp.status().as_u16();
        let text = resp.text().await.expect("body");
        (status, serde_json::from_str(&text).expect("json"))
    }

    async fn get_config_ok(base: &str) -> Value {
        let (status, body) = get_config(base).await;
        assert_eq!(status, 200, "GET /config: {body}");
        body
    }

    async fn post_config(base: &str, payload: &Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}/api/v1/config"))
            .header("content-type", "application/json")
            .body(payload.to_string())
            .send()
            .await
            .expect("POST /config")
    }

    async fn err_code(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body");
        serde_json::from_str::<Value>(&text).expect("json")["error"].clone()
    }

    // ------- config_test.go mirrors -------

    // Mirrors Go `TestConfigGet`: the api_key indirection is returned VERBATIM (no secret leak).
    #[tokio::test]
    async fn config_get() {
        let wf = TempWorkflow::new(SAMPLE_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let body = get_config_ok(&base).await;
        assert_eq!(body["config"]["tracker"]["project_slug"], "symphony");
        assert_eq!(
            body["config"]["tracker"]["api_key"], "$HOME",
            "the $VAR indirection is returned unresolved (no secret leak)"
        );
        assert_eq!(
            body["prompt_body"],
            "Do the work for {{ issue.identifier }}."
        );
    }

    // Mirrors Go `TestConfigPostRoundTrip`: a POST persists the edit (on-disk rewritten) and a
    // subsequent GET reflects it; the persisted api_key stays the $VAR indirection.
    #[tokio::test]
    async fn config_post_round_trip() {
        let wf = TempWorkflow::new(SAMPLE_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let payload = json!({
            "config": {
                "tracker": { "kind": "linear", "api_key": "$HOME", "project_slug": "changed-slug" },
                "agent": { "backend": "claude" }
            },
            "prompt_body": "new prompt body"
        });
        let resp = post_config(&base, &payload).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);

        // A subsequent GET reflects the persisted edit (the config block IS the on-disk front matter).
        let body = get_config_ok(&base).await;
        assert_eq!(body["config"]["tracker"]["project_slug"], "changed-slug");
        assert_eq!(
            body["config"]["tracker"]["api_key"], "$HOME",
            "no resolved secret written to disk"
        );
        assert_eq!(body["prompt_body"], "new prompt body");
    }

    // Mirrors Go `TestConfigPostInvalidRejected`: an invalid config is 400'd and the on-disk file is
    // left UNTOUCHED (never corrupt a working config with a bad write).
    #[tokio::test]
    async fn config_post_invalid_rejected() {
        let wf = TempWorkflow::new(SAMPLE_WORKFLOW_MD);
        let before = fs::read(&wf.path).expect("read before");
        let base = spawn(&wf.path()).await;
        // kind != "linear" fails ValidateDispatch. Legacy path (no global) => opaque invalid_config.
        let payload = json!({
            "config": { "tracker": { "kind": "jira", "project_slug": "x" }, "agent": { "backend": "claude" } },
            "prompt_body": "x"
        });
        let resp = post_config(&base, &payload).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await["code"], "invalid_config");
        let after = fs::read(&wf.path).expect("read after");
        assert_eq!(
            before, after,
            "on-disk WORKFLOW.md must be untouched on a rejected POST"
        );
    }

    // Mirrors Go `TestConfigMethodNotAllowed`: DELETE is 405.
    #[tokio::test]
    async fn config_method_not_allowed() {
        let wf = TempWorkflow::new(SAMPLE_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let resp = reqwest::Client::new()
            .delete(format!("{base}/api/v1/config"))
            .send()
            .await
            .expect("DELETE /config");
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers().get("allow").and_then(|v| v.to_str().ok()),
            Some("GET, HEAD, POST")
        );
    }

    // ------- config_typed_test.go mirrors -------

    // Mirrors Go `TestConfigGetTypedMultiProject`: the typed view — global defaults, a projects[]
    // array with a sparse per-project overrides map + computed effective block, api_key exposed ONLY
    // as api_key_set.
    #[tokio::test]
    async fn config_get_typed_multi_project() {
        let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let body = get_config_ok(&base).await;

        assert_eq!(body["global"]["tracker"]["api_key_set"], true);
        assert!(
            body["global"]["tracker"].get("api_key").is_none(),
            "the typed view must not expose api_key"
        );
        assert_eq!(body["global"]["claude"]["model"], "claude-sonnet-4-6");
        assert_eq!(
            body["global"]["claude"]["permission_mode"],
            "bypassPermissions"
        );
        assert_eq!(
            body["global"]["active_states"],
            json!(["Todo", "In Progress"])
        );

        let projects = body["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 3);
        assert_eq!(projects[0]["name"], "Infra Bot");
        assert_eq!(projects[0]["enabled"], true);
        assert_eq!(projects[0]["overrides"]["model"], "claude-opus-4-8");
        assert!(
            projects[0]["overrides"].get("effort").is_none(),
            "an inherited knob is ABSENT from overrides"
        );
        assert_eq!(projects[0]["effective"]["model"], "claude-opus-4-8");
        assert_eq!(
            projects[0]["effective"]["permission"], "bypassPermissions",
            "inherited from the global default"
        );
        assert_eq!(projects[2]["name"], "Paused Bot");
        assert_eq!(projects[2]["enabled"], false);
        assert_eq!(projects[2]["effective"]["model"], "claude-sonnet-4-6");
    }

    // Mirrors Go `TestConfigPostTypedRoundTripStable`: GET → POST(verbatim) → GET leaves the typed
    // view (global + projects) byte-stable, and the persisted api_key stays the $VAR indirection.
    #[tokio::test]
    async fn config_post_typed_round_trip_stable() {
        let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let got1 = get_config_ok(&base).await;
        let resp = post_config(&base, &got1).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let got2 = get_config_ok(&base).await;
        assert_eq!(
            got1["global"], got2["global"],
            "global not stable across round-trip"
        );
        assert_eq!(
            got1["projects"], got2["projects"],
            "projects not stable across round-trip"
        );
        assert_eq!(
            got2["config"]["tracker"]["api_key"], "$HOME",
            "persisted api_key must stay the unresolved $VAR"
        );
    }

    // Mirrors Go `TestConfigPostTypedRemovesProject`: remove-agent = a POST whose payload omits the
    // entry (no separate delete endpoint).
    #[tokio::test]
    async fn config_post_typed_removes_project() {
        let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let mut got = get_config_ok(&base).await;
        let mut projects = got["projects"].as_array().expect("projects").clone();
        assert_eq!(projects.len(), 3, "precondition");
        projects.truncate(2); // drop Paused Bot
        got["projects"] = Value::Array(projects);
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);

        let after = get_config_ok(&base).await;
        let ap = after["projects"].as_array().expect("projects");
        assert_eq!(ap.len(), 2, "removed project must be gone");
        assert!(
            ap.iter().all(|p| p["name"] != "Paused Bot"),
            "Paused Bot should have been removed"
        );
    }

    // Mirrors Go `TestConfigPostTypedOmittedEnabledNotPaused`: a typed POST whose project omits
    // `enabled` does NOT pause the agent (enabled is a presence-pointer).
    #[tokio::test]
    async fn config_post_typed_omitted_enabled_not_paused() {
        let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let mut got = get_config_ok(&base).await;
        got["projects"][0]
            .as_object_mut()
            .expect("project object")
            .remove("enabled");
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let after = get_config_ok(&base).await;
        assert_eq!(
            after["projects"][0]["enabled"], true,
            "a project that omitted enabled must stay enabled"
        );
    }

    // Mirrors Go `TestConfigPostTypedInvalidReviewPromote`: an invalid typed config is 400'd with a
    // structured field error, and the on-disk file is UNTOUCHED.
    #[tokio::test]
    async fn config_post_typed_invalid_review_promote() {
        let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
        let before = fs::read(&wf.path).expect("read before");
        let base = spawn(&wf.path()).await;
        let mut got = get_config_ok(&base).await;
        got["global"]["review_states"] = json!(["In Review"]);
        got["global"]["review_promote_state"] = json!("Frozen"); // not in active_states
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 400, "POST body={:?}", resp.text().await);
        let error = err_code(resp).await;
        assert_eq!(error["code"], "invalid_review_promote_state");
        assert_eq!(
            error["fields"][0]["path"], "review_promote_state",
            "the structured field error must point at the offending input"
        );
        assert_eq!(
            fs::read(&wf.path).expect("read after"),
            before,
            "on-disk WORKFLOW.md must be untouched on a rejected typed POST"
        );
    }

    // Mirrors Go `TestConfigPostTypedClaimModeInvalidRejected` / `..DependencyModeInvalidRejected`:
    // an invalid typed enum knob is 400'd with the matching code + field path.
    #[tokio::test]
    async fn config_post_typed_invalid_enum_knobs() {
        for (knob, value, want_code, want_field) in [
            (
                "claim_mode",
                "sideways",
                "unsupported_claim_mode",
                "claim_mode",
            ),
            (
                "dependency_mode",
                "sideways",
                "unsupported_dependency_mode",
                "dependency_mode",
            ),
        ] {
            let wf = TempWorkflow::new(MULTI_PROJECT_WORKFLOW_MD);
            let base = spawn(&wf.path()).await;
            let mut got = get_config_ok(&base).await;
            got["global"][knob] = json!(value);
            let resp = post_config(&base, &got).await;
            assert_eq!(
                resp.status(),
                400,
                "{knob}: POST body={:?}",
                resp.text().await
            );
            let error = err_code(resp).await;
            assert_eq!(error["code"], want_code, "{knob}: code");
            assert_eq!(error["fields"][0]["path"], want_field, "{knob}: field path");
        }
    }

    // Mirrors Go `TestConfigPostTypedPreservesCanceledStates`: a global + per-project canceled_states
    // override survives GET → POST(verbatim) → GET (projectFromJSON rebuilds projects wholesale).
    #[tokio::test]
    async fn config_post_typed_preserves_canceled_states() {
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  active_states:\n    - Todo\n    - In Progress\n  terminal_states:\n    - Done\n    - Cancelled\n    - Abandoned\n  canceled_states:\n    - Cancelled\n\
repo: git@github.com:o/infra.git\n\
agent:\n  backend: claude\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n    terminal_states:\n      - Done\n      - Abandoned\n    canceled_states:\n      - Abandoned\n\
---\nDefault prompt.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got1 = get_config_ok(&base).await;
        assert_eq!(got1["global"]["canceled_states"], json!(["Cancelled"]));
        assert_eq!(got1["projects"][0]["canceled_states"], json!(["Abandoned"]));
        let resp = post_config(&base, &got1).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let got2 = get_config_ok(&base).await;
        assert_eq!(
            got1["global"]["canceled_states"],
            got2["global"]["canceled_states"]
        );
        assert_eq!(
            got2["projects"][0]["canceled_states"],
            json!(["Abandoned"]),
            "per-project canceled_states dropped on save"
        );
    }

    // Mirrors Go `TestConfigPostTypedPreservesUnexposedGlobalKnobs`: global knobs the typed view does
    // NOT surface (claude.allowed_tools/setting_sources/add_dirs + the whole codex block) survive a
    // typed POST via the patch-from-base path.
    #[tokio::test]
    async fn config_post_typed_preserves_unexposed_global_knobs() {
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  project_slug: solo\n\
agent:\n  backend: claude\n\
claude:\n  model: claude-sonnet-4-6\n  allowed_tools: Read,Edit,Bash\n  setting_sources: project\n  add_dirs:\n    - ../shared\n\
codex:\n  approval_policy: on-request\n  thread_sandbox: workspace-write\n\
---\nBody.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got = get_config_ok(&base).await;
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let after = get_config_ok(&base).await;
        let claude = &after["config"]["claude"];
        assert_eq!(
            claude["allowed_tools"], "Read,Edit,Bash",
            "unexposed claude knob dropped"
        );
        assert_eq!(claude["setting_sources"], "project");
        assert_eq!(claude["add_dirs"], json!(["../shared"]));
        let codex = &after["config"]["codex"];
        assert_eq!(
            codex["approval_policy"], "on-request",
            "codex block dropped on typed POST"
        );
        assert_eq!(codex["thread_sandbox"], "workspace-write");
    }

    // Mirrors Go `TestConfigPostTypedPreservesUnexposedProjectKnobs`: a per-project hooks block + a
    // claude extra_args override (both unexposed) survive a GET→POST→GET.
    #[tokio::test]
    async fn config_post_typed_preserves_unexposed_project_knobs() {
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n\
repo: git@github.com:o/infra.git\n\
agent:\n  backend: claude\n\
claude:\n  model: claude-sonnet-4-6\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n    claude:\n      model: claude-opus-4-8\n      extra_args:\n        - --verbose\n    hooks:\n      after_create: echo created\n\
---\nBody.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got = get_config_ok(&base).await;
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let after = get_config_ok(&base).await;
        let p0 = &after["config"]["projects"][0];
        assert_eq!(
            p0["claude"]["extra_args"],
            json!(["--verbose"]),
            "per-project claude.extra_args lost on round-trip"
        );
        assert_eq!(
            p0["hooks"]["after_create"], "echo created",
            "per-project hooks lost"
        );
    }

    // Mirrors Go `TestConfigTypedClaudeOverridesRoundTrip` (the INF-239 acceptance): the four
    // newly-surfaced per-project claude knobs (turn/stall timeouts, billing_guard, command) round-trip
    // as overrides on the overriding project and stay ABSENT (inherited) on the inheriting one.
    #[tokio::test]
    async fn config_typed_claude_overrides_round_trip() {
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n\
repo: git@github.com:o/infra.git\n\
agent:\n  backend: claude\n\
claude:\n  model: claude-sonnet-4-6\n  command: claude\n  billing_guard: true\n  turn_timeout_ms: 120000\n  stall_timeout_ms: 30000\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n    claude:\n      turn_timeout_ms: 300000\n      stall_timeout_ms: 60000\n      billing_guard: false\n      command: claude-custom\n  - name: Core Bot\n    slugs:\n      - core-proj\n\
---\nBody.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got = get_config_ok(&base).await;
        let ov0 = &got["projects"][0]["overrides"];
        assert_eq!(ov0["turn_timeout_ms"], 300000);
        assert_eq!(ov0["stall_timeout_ms"], 60000);
        assert_eq!(ov0["billing_guard"], false);
        assert_eq!(ov0["command"], "claude-custom");
        assert_eq!(got["projects"][0]["effective"]["turn_timeout_ms"], 300000);
        let ov1 = &got["projects"][1]["overrides"];
        for k in [
            "turn_timeout_ms",
            "stall_timeout_ms",
            "billing_guard",
            "command",
        ] {
            assert!(
                ov1.get(k).is_none(),
                "projects[1].overrides.{k} must be ABSENT (inherited)"
            );
        }
        assert_eq!(got["projects"][1]["effective"]["turn_timeout_ms"], 120000);

        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let got2 = get_config_ok(&base).await;
        assert_eq!(got["global"], got2["global"], "global not stable");
        assert_eq!(got["projects"], got2["projects"], "projects not stable");
    }

    // Mirrors the overrides-block round-trips of config_{gitflow,workspacemode,claimmode}_test.go: the
    // per-project knobs surfaced in `overrides` but stored on the top-level Project (git_flow /
    // workspace_mode / claim_mode) round-trip as overrides on the overriding project and stay ABSENT
    // (inherited) on the inheriting one. This exercises project_from_json's overrides-block handling,
    // which the global-invalid tests above do not reach at the per-project level.
    #[tokio::test]
    async fn config_typed_overrides_block_knobs_round_trip() {
        // claim_mode is a `tracker:` knob (like Go's claimModeWorkflowMD); git_flow / workspace_mode
        // are top-level. The per-project overrides are direct project keys.
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n  claim_mode: pool\n\
repo: git@github.com:o/top.git\n\
agent:\n  backend: claude\n\
git_flow: graphite\n\
workspace_mode: clone\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n    git_flow: any\n    workspace_mode: worktree\n    claim_mode: assignee\n  - name: Core Bot\n    slugs:\n      - core\n\
---\nBody.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got = get_config_ok(&base).await;

        // Global carries the three knobs; project 0 overrides each; project 1 inherits (ABSENT).
        assert_eq!(got["global"]["git_flow"], "graphite");
        assert_eq!(got["global"]["workspace_mode"], "clone");
        assert_eq!(got["global"]["claim_mode"], "pool");
        let ov0 = &got["projects"][0]["overrides"];
        assert_eq!(ov0["git_flow"], "any");
        assert_eq!(ov0["workspace_mode"], "worktree");
        assert_eq!(ov0["claim_mode"], "assignee");
        assert_eq!(
            got["projects"][0]["effective"]["git_flow"], "any",
            "override wins"
        );
        let ov1 = &got["projects"][1]["overrides"];
        for k in ["git_flow", "workspace_mode", "claim_mode"] {
            assert!(
                ov1.get(k).is_none(),
                "projects[1].overrides.{k} must be ABSENT (inherited)"
            );
        }
        assert_eq!(
            got["projects"][1]["effective"]["git_flow"], "graphite",
            "inherits the global git_flow"
        );

        // Verbatim POST → GET is byte-stable (the overrides-block knobs survive project_from_json).
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let got2 = get_config_ok(&base).await;
        assert_eq!(got["global"], got2["global"], "global not stable");
        assert_eq!(got["projects"], got2["projects"], "projects not stable");
        // The per-project override persists on disk; the inheriting project keeps none.
        assert_eq!(got2["config"]["projects"][0]["git_flow"], "any");
        assert!(
            got2["config"]["projects"][1].get("git_flow").is_none(),
            "inheriting project must persist no git_flow override"
        );
    }

    // Mirrors Go `TestConfigPostPreservesPRLabel`: a top-level knob the typed view never surfaces
    // (pr_label) survives a Save.
    #[tokio::test]
    async fn config_post_preserves_pr_label() {
        const MD: &str = "---\n\
tracker:\n  kind: linear\n  api_key: $HOME\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n\
repo: git@github.com:o/infra.git\n\
agent:\n  backend: claude\n\
pr_label: agent-authored\n\
projects:\n  - name: Infra Bot\n    slugs:\n      - infra\n\
---\nBody.\n";
        let wf = TempWorkflow::new(MD);
        let base = spawn(&wf.path()).await;
        let got = get_config_ok(&base).await;
        let resp = post_config(&base, &got).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let after = get_config_ok(&base).await;
        assert_eq!(
            after["config"]["pr_label"], "agent-authored",
            "pr_label must survive a Save"
        );
    }

    // Mirrors Go `TestConfigPostTypedLegacySingleCollapses`: a legacy single-project config
    // (tracker.project_slug, no projects:) GET→POST(verbatim) stays single-form on disk + typed-stable.
    #[tokio::test]
    async fn config_post_typed_legacy_single_collapses() {
        let wf = TempWorkflow::new(SAMPLE_WORKFLOW_MD);
        let base = spawn(&wf.path()).await;
        let got1 = get_config_ok(&base).await;
        assert_eq!(
            got1["projects"].as_array().map(|a| a.len()),
            Some(1),
            "legacy single-project GET synthesizes exactly one agent"
        );
        let resp = post_config(&base, &got1).await;
        assert_eq!(resp.status(), 200, "POST body={:?}", resp.text().await);
        let after = get_config_ok(&base).await;
        assert_eq!(
            after["config"]["tracker"]["project_slug"], "symphony",
            "collapsed single form on disk"
        );
        assert!(
            after["config"].get("projects").is_none(),
            "a trivial single agent must stay in the single-project form (no projects: list)"
        );
        assert_eq!(got1["global"], after["global"], "typed view not stable");
        assert_eq!(got1["projects"], after["projects"], "typed view not stable");
    }
}
