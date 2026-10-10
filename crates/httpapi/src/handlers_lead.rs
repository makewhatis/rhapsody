//! Tech-lead decision history and operator overrules (STUDIO-1138). No Go counterpart.

use crate::{
    handlers::{require_get, require_post},
    responses::{write_error, write_json},
    server::StateProvider,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{Method, StatusCode},
    response::Response,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(crate) struct Since {
    #[serde(default)]
    since: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Overrule {
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessesRequest {
    harnesses: Vec<HarnessInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessInput {
    harness: String,
    model: String,
    #[serde(default)]
    effort: String,
}

pub(crate) async fn handle_harnesses(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    body: Bytes,
) -> Response {
    if method != Method::GET && method != Method::HEAD && method != Method::PUT {
        return write_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "use GET or PUT for lead harnesses",
            Some("GET, HEAD, PUT"),
        );
    }
    let Some(service) = provider.lead_harnesses() else {
        return write_error(
            StatusCode::CONFLICT,
            "lead_harnesses_unavailable",
            "the manager harness runtime is unavailable",
            None,
        );
    };
    if method != Method::PUT {
        return write_json(StatusCode::OK, &service.snapshot());
    }
    if body.len() > 4096 {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "harnesses body exceeds 4096 bytes",
            None,
        );
    }
    let request: HarnessesRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("harnesses: {error}"),
                None,
            );
        }
    };
    use rhapsody_orchestrator::managerselftest::runtime::UpdateError;
    let entries = request
        .harnesses
        .into_iter()
        .map(|entry| rhapsody_config::teams::ManagerHarnessEntry {
            harness: entry.harness,
            model: entry.model,
            effort: entry.effort,
        })
        .collect();
    match service.update(entries).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(UpdateError::Invalid(reason)) => {
            write_error(StatusCode::BAD_REQUEST, "invalid_request", reason, None)
        }
        Err(UpdateError::AllFailed(tested)) => write_json(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "error": {"code": "all_harnesses_failed", "message": "No entry passed; the old list is still active"},
                "tested": tested.harnesses, "active": service.snapshot(),
            }),
        ),
        Err(UpdateError::Persist(error)) => {
            tracing::warn!(%error, "lead harness persistence failed; old list retained");
            write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "harnesses_write_failed",
                "teams.yaml could not be saved; the old list is still active",
                None,
            )
        }
    }
}

pub(crate) async fn handle_decisions(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<Since>,
) -> Response {
    if let Some(response) = require_get(&method) {
        return response;
    }
    let Some(reports) = provider.lead_reports() else {
        return write_error(
            StatusCode::CONFLICT,
            "lead_disabled",
            "the lead is disabled or durable storage is unavailable",
            None,
        );
    };
    match reports.decisions(&q.since) {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(error) if error == "since must be RFC3339" => {
            write_error(StatusCode::BAD_REQUEST, "bad_request", &error, None)
        }
        Err(error) => {
            tracing::warn!(%error, "lead decisions unavailable");
            write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "lead_unavailable",
                "lead decisions could not be read",
                None,
            )
        }
    }
}

pub(crate) async fn handle_overrule(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if let Some(response) = require_post(&method, "use POST to overrule a lead decision") {
        return response;
    }
    if body.len() > 4096 {
        return write_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "overrule body exceeds 4096 bytes",
            None,
        );
    }
    let (Ok(id), Ok(request)) = (id.parse::<i64>(), serde_json::from_slice::<Overrule>(&body))
    else {
        return write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "provide a positive decision id and a JSON note",
            None,
        );
    };
    let Some(reports) = provider.lead_reports() else {
        return write_error(
            StatusCode::CONFLICT,
            "lead_disabled",
            "the lead is disabled or durable storage is unavailable",
            None,
        );
    };
    match reports.overrule(id, &request.note).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(error) if error.starts_with("overrule requires") => {
            write_error(StatusCode::BAD_REQUEST, "bad_request", &error, None)
        }
        Err(error) if error == "decision not found or still applying" => {
            write_error(StatusCode::NOT_FOUND, "not_found", &error, None)
        }
        Err(error) if error == "decision was already overruled with a different note" => {
            write_error(StatusCode::CONFLICT, "already_overruled", &error, None)
        }
        Err(error) => {
            tracing::warn!(%error, "lead overrule unavailable");
            write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "overrule_failed",
                "overrule could not be recorded",
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};
    use rhapsody_store::{LeadDecisionRow, LeadTrigger, Sqlite, Store, StorePath};

    struct HarnessTemp(std::path::PathBuf);
    impl Drop for HarnessTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct CanaryFactory(std::sync::atomic::AtomicUsize);
    impl rhapsody_orchestrator::managerselftest::CanaryRunnerFactory for CanaryFactory {
        fn probe_version(
            &self,
            _: &rhapsody_config::teams::ManagerHarnessEntry,
        ) -> Result<String, String> {
            Ok("fake".into())
        }
        fn runner(
            &self,
            _: &rhapsody_config::teams::ManagerHarnessEntry,
        ) -> Option<Box<dyn rhapsody_orchestrator::managerselftest::CanaryRunner>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // No runner is a genuine fail-closed boot-path failure, not a canned HTTP answer.
            None
        }
    }
    async fn harness_api() -> (String, Arc<CanaryFactory>, HarnessTemp) {
        use rhapsody_orchestrator::managerselftest::*;
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = HarnessTemp(std::env::temp_dir().join(format!(
            "rhapsody-lead-api-{}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join("teams.yaml");
        let state = Arc::new(ManagerSelfTestState::new(vec![
            rhapsody_config::teams::ManagerHarnessEntry {
                harness: "claude".into(),
                model: "old".into(),
                effort: "high".into(),
            },
        ]));
        state.record_entry(
            0,
            SelfTestRecord {
                cli_version: "fake".into(),
                verdict: SelfTestVerdict::Passed,
            },
        );
        rhapsody_config::teams::Teams::save(&path, &rhapsody_config::teams::Teams::disabled())
            .unwrap();
        let factory = Arc::new(CanaryFactory(std::sync::atomic::AtomicUsize::new(0)));
        let service = Arc::new(runtime::LeadHarnesses {
            state,
            factory: factory.clone(),
            path,
            room: None,
        });
        let url = spawn_router(crate::new_handler(
            Arc::new(FakeProvider::ok(empty_snapshot()).with_lead_harnesses(service)),
            None,
        ))
        .await;
        (url, factory, dir)
    }
    #[tokio::test]
    async fn put_requires_operator_guard() {
        let (url, factory, _dir) = harness_api().await;
        let client = reqwest::Client::new();
        let path = format!("{url}/api/v1/lead/harnesses");
        let request =
            serde_json::json!({"harnesses":[{"harness":"claude","model":"new","effort":"high"}]});
        assert_eq!(
            client
                .put(&path)
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(factory.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            client
                .put(&path)
                .header("X-Rhapsody-Operator", "1")
                .header("Origin", "https://foreign.invalid")
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .request(Method::OPTIONS, &path)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(&path)
                .header("X-Rhapsody-Operator", "1")
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(reqwest::get(&path).await.unwrap().status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn invalid_body_400_names_field() {
        let (url, factory, _dir) = harness_api().await;
        for (body, field) in [
            (
                serde_json::json!({"harnesses": (0..5).map(|n| serde_json::json!({"harness":"claude","model":format!("model-{n}")})).collect::<Vec<_>>() }),
                "harnesses",
            ),
            (
                serde_json::json!({"harnesses":[{"harness":"claude","model":"new","provider":"secret"}]}),
                "provider",
            ),
            (serde_json::json!({}), "harnesses"),
            (serde_json::json!({"harnesses":[]}), "harnesses"),
            (
                serde_json::json!({"harnesses":[{"harness":"codex","model":"new","effort":"high"}]}),
                "harnesses[0].harness",
            ),
            (
                serde_json::json!({"harnesses":[{"harness":"claude","model":" ","effort":"high"}]}),
                "harnesses[0].model",
            ),
            (
                serde_json::json!({"harnesses":[{"harness":"claude","model":"new","effort":"xhigh"}]}),
                "harnesses[0].effort",
            ),
            (
                serde_json::json!({"harnesses":[{"harness":"claude","model":"new"},{"harness":"claude","model":"new"}]}),
                "harnesses[1].model",
            ),
        ] {
            let response = reqwest::Client::new()
                .put(format!("{url}/api/v1/lead/harnesses"))
                .header("X-Rhapsody-Operator", "1")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(
                response.json::<serde_json::Value>().await.unwrap()["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(field)
            );
        }
        assert_eq!(factory.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn put_self_test_failure_returns_409_with_old_list_active() {
        let (url, factory, _dir) = harness_api().await;
        let path = format!("{url}/api/v1/lead/harnesses");
        let response = reqwest::Client::new().put(&path).header("X-Rhapsody-Operator", "1").json(&serde_json::json!({"harnesses":[{"harness":"claude","model":"new","effort":"high"}]})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            factory.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "PUT must call the real per-entry self-test path"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(
            body["tested"][0]["state"]
                .as_str()
                .unwrap()
                .contains("no self-test for harness claude")
        );
        assert_eq!(body["active"]["harnesses"][0]["model"], "old");
        let body: serde_json::Value = reqwest::get(&path).await.unwrap().json().await.unwrap();
        assert_eq!(body["harnesses"][0]["model"], "old");
    }
    #[tokio::test]
    async fn notification_api_exists_independently_of_lead_capability() {
        let url = spawn_router(crate::new_handler(
            Arc::new(FakeProvider::ok(empty_snapshot())),
            None,
        ))
        .await;
        let response = reqwest::get(format!("{url}/api/v1/notifications"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["notifications"],
            serde_json::json!([])
        );
    }
    #[tokio::test]
    async fn version_exposes_effective_lead_capability() {
        for enabled in [false, true] {
            let provider = FakeProvider::ok(empty_snapshot());
            let provider = if enabled {
                provider.with_lead_reports(Arc::new(
                    rhapsody_orchestrator::leadreport::LeadReports {
                        store: Arc::new(Sqlite::open(StorePath::InMemory).unwrap()),
                        memory: None,
                    },
                ))
            } else {
                provider
            };
            let url = spawn_router(crate::new_handler(Arc::new(provider), None)).await;
            let version: serde_json::Value = reqwest::get(format!("{url}/api/v1/version"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(version["lead_enabled"].as_bool().unwrap_or(false), enabled);
            if !enabled {
                assert!(version.get("lead_enabled").is_none());
            }
            assert_eq!(
                reqwest::get(format!("{url}/api/v1/lead/decisions"))
                    .await
                    .unwrap()
                    .status(),
                if enabled {
                    StatusCode::OK
                } else {
                    StatusCode::CONFLICT
                }
            );
        }
    }
    #[tokio::test]
    async fn decisions_api_since() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let item = store
            .enqueue_lead_item(
                &LeadTrigger::BlockedHandoff {
                    ticket: "TEST-1".into(),
                    question: "retry?".into(),
                },
                "2026-10-07T10:00:00Z",
            )
            .unwrap();
        for at in ["2026-10-07T10:00:00Z", "2026-10-08T10:00:00Z"] {
            store
                .save_lead_decision(&LeadDecisionRow {
                    item,
                    at: at.into(),
                    decision: "done: requeue".into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let reports = Arc::new(rhapsody_orchestrator::leadreport::LeadReports {
            store: store.clone(),
            memory: None,
        });
        let url = spawn_router(crate::new_handler(
            Arc::new(FakeProvider::ok(empty_snapshot()).with_lead_reports(reports)),
            None,
        ))
        .await;
        let response = reqwest::get(format!(
            "{url}/api/v1/lead/decisions?since=2026-10-08T10:00:00Z"
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["decisions"].as_array().unwrap().len(), 1);
        assert_eq!(body["decisions"][0]["subject"], "TEST-1");
        let client = reqwest::Client::new();
        let path = format!("{url}/api/v1/lead/decisions/2/overrule");
        assert_eq!(
            client
                .post(&path)
                .json(&serde_json::json!({"note":"diagnose"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = client
            .post(&path)
            .header("X-Rhapsody-Operator", "1")
            .json(&serde_json::json!({"note":"diagnose"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(store.load_lead_items().unwrap().len(), 2);
        assert_eq!(
            reqwest::get(&path).await.unwrap().status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            reqwest::get(format!("{url}/api/v1/lead/decisions?since=bad"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
