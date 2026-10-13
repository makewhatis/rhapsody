//! Landing operator surface (STUDIO-1252); no Go counterpart.

use crate::handlers::{require_get, require_post};
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::landing::{LandingAction, LandingActionOutcome};
use std::sync::Arc;

pub(crate) async fn handle_landings(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(response) = require_get(&method) {
        return response;
    }
    match provider.landings().await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(error) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "landing_backend_error",
            error,
            None,
        ),
    }
}

pub(crate) async fn handle_landing_action(
    method: Method,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(response) = require_post(&method, "use POST for a landing action") {
        return response;
    }
    let Ok(id) = id.parse::<i64>() else {
        return render(LandingActionOutcome::NotFound, 0);
    };
    let action = match uri.path().rsplit('/').next().unwrap_or("") {
        "retry" => LandingAction::Retry,
        "skip" => LandingAction::Skip,
        "stop" => LandingAction::Stop,
        _ => return render(LandingActionOutcome::NotFound, id),
    };
    render(provider.landing_action(id, action).await, id)
}

fn render(outcome: LandingActionOutcome, id: i64) -> Response {
    match outcome {
        LandingActionOutcome::Applied => write_json(
            StatusCode::OK,
            &serde_json::json!({"id": id, "applied": true}),
        ),
        LandingActionOutcome::NotFound => write_error(
            StatusCode::NOT_FOUND,
            "landing_not_found",
            "unknown landing id",
            None,
        ),
        LandingActionOutcome::Refused(reason) => {
            write_error(StatusCode::CONFLICT, "landing_refused", reason, None)
        }
        LandingActionOutcome::Failed(error) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "landing_backend_error",
            error,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, operator_client, spawn_router};

    #[tokio::test]
    async fn landings_list_and_each_action_method_unknown_id_and_operator_gate() {
        let provider = std::sync::Arc::new(FakeProvider::ok(empty_snapshot()));
        let base = spawn_router(new_handler(provider, None)).await;
        let response = reqwest::get(format!("{base}/api/v1/landings"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["landings"],
            serde_json::json!([])
        );
        for action in ["retry", "skip", "stop"] {
            let url = format!("{base}/api/v1/landings/999/{action}");
            assert_eq!(
                operator_client().post(&url).send().await.unwrap().status(),
                404
            );
            assert_eq!(
                operator_client().get(&url).send().await.unwrap().status(),
                405
            );
            assert_eq!(
                reqwest::Client::new()
                    .post(&url)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                403
            );
        }
    }

    #[tokio::test]
    async fn each_action_forwards_its_id_and_returns_200_404_409_or_500() {
        use super::{LandingAction, LandingActionOutcome};
        for (outcome, status) in [
            (LandingActionOutcome::Applied, 200),
            (LandingActionOutcome::NotFound, 404),
            (LandingActionOutcome::Refused("wrong phase".into()), 409),
            (
                LandingActionOutcome::Failed("store unavailable".into()),
                500,
            ),
        ] {
            let provider = std::sync::Arc::new(
                FakeProvider::ok(empty_snapshot()).with_landing_outcome(outcome),
            );
            let base = spawn_router(new_handler(provider.clone(), None)).await;
            for (action, expected) in [
                ("retry", LandingAction::Retry),
                ("skip", LandingAction::Skip),
                ("stop", LandingAction::Stop),
            ] {
                let response = operator_client()
                    .post(format!("{base}/api/v1/landings/42/{action}"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
                assert_eq!(provider.landing_asked(), Some((42, expected)));
                let body = response.json::<serde_json::Value>().await.unwrap();
                if status == 409 {
                    assert_eq!(body["error"]["message"], "wrong phase");
                }
            }
        }
    }

    #[tokio::test]
    async fn list_reports_phase_age_and_paused_pr_coordinates() {
        use rhapsody_orchestrator::landing::{LandingView, LandingsView};
        let provider = std::sync::Arc::new(FakeProvider::ok(empty_snapshot()).with_landings(
            LandingsView {
                landings: vec![LandingView {
                    id: 42,
                    project_slug: "app".into(),
                    ticket: "APP-1".into(),
                    pr: "o/r#1".into(),
                    phase: "AwaitingAcceptance".into(),
                    phase_since_s: Some(5),
                    phase_age_s: Some(60),
                    first_failing_check: None,
                    hold_reason: None,
                    revert_of: None,
                    pausing_auto_merge: vec!["o/r#7".into()],
                }],
            },
        ));
        let base = spawn_router(new_handler(provider, None)).await;
        let body = reqwest::get(format!("{base}/api/v1/landings"))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            body["landings"][0]["pausing_auto_merge"],
            serde_json::json!(["o/r#7"])
        );
        assert_eq!(body["landings"][0]["phase_age_s"], 60);
    }
}
