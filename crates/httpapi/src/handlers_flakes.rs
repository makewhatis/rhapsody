//! Durable CI rerun / flake log (STUDIO-2136); no Go counterpart.

use crate::handlers::require_get;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;
use axum::{
    extract::State,
    http::{Method, StatusCode},
    response::Response,
};
use std::sync::Arc;

pub(crate) async fn handle_flakes(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(response) = require_get(&method) {
        return response;
    }
    let Some(store) = provider.notification_store() else {
        return write_json(StatusCode::OK, &serde_json::json!({"flakes": []}));
    };
    match rhapsody_orchestrator::reviewci::flakes(store.as_ref()) {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(error) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "flake_backend_error",
            error,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        server::new_handler,
        testutil::{FakeProvider, empty_snapshot, spawn_router},
    };

    #[tokio::test]
    async fn flakes_is_a_json_get_surface_with_a_method_guard() {
        let base = spawn_router(new_handler(
            std::sync::Arc::new(FakeProvider::ok(empty_snapshot())),
            None,
        ))
        .await;
        let body: serde_json::Value = reqwest::get(format!("{base}/api/v1/flakes"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body, serde_json::json!({"flakes": []}));
        let response = reqwest::Client::new()
            .post(format!("{base}/api/v1/flakes"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn flakes_reads_the_durable_operator_store() {
        use rhapsody_orchestrator::reviewci::{CiFailure, CiSnapshot};
        use rhapsody_store::{CiRerun, Sqlite, Store, StorePath};
        let store = std::sync::Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let original = CiSnapshot {
            head: "sha".into(),
            failed: vec![CiFailure {
                name: "test".into(),
                url: "https://github.com/o/r/actions/runs/1/job/2".into(),
                tests: vec!["--- FAIL: TestUsage (0.01s)".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            store
                .reserve_ci_rerun(&CiRerun {
                    owner: "o".into(),
                    repo: "r".into(),
                    number: 1,
                    head: "sha".into(),
                    original: serde_json::to_string(&original).unwrap(),
                    ..Default::default()
                })
                .unwrap()
        );
        let provider = FakeProvider::ok(empty_snapshot()).with_notification_store(store);
        let base = spawn_router(new_handler(std::sync::Arc::new(provider), None)).await;
        let body: serde_json::Value = reqwest::get(format!("{base}/api/v1/flakes"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["flakes"].as_array().unwrap().len(), 1);
        assert_eq!(body["flakes"][0]["repo"], "o/r");
        assert_eq!(body["flakes"][0]["pr"], 1);
        assert_eq!(body["flakes"][0]["head"], "sha");
        assert_eq!(body["flakes"][0]["check"], "test");
        assert_eq!(body["flakes"][0]["tests"][0], "TestUsage");
        assert_eq!(body["flakes"][0]["outcome"], "requested");
        assert!(body["flakes"][0]["rerun_job_url"].is_null());
    }
}
