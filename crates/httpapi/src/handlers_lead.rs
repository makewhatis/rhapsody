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
