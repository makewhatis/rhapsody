//! `symphony_run_status` composition — parity port of `$REF/internal/mcpfacade/status.go`.
//!
//! [`Client::run_status`] composes the verdict for a run id and/or issue identifier from `/state`,
//! `/runs/{id}`, and `/issues/{id}/history`. It tolerates the daemon's two 404 "no detail row"
//! codes (falling back to the live running row or the history summary) but SURFACES any other
//! failure (daemon_unreachable / timeout / 5xx) — a finished run whose detail request errored must
//! never look alive.

use crate::client::{Client, FacadeError, RunDetail, StateResp};
use crate::verdict::{Status, VerdictInput, default_stall_threshold, verdict};
use chrono::{DateTime, Utc};

impl Client {
    /// Composes the `symphony_run_status` verdict for a run id and/or issue identifier (status.go's
    /// `runStatus`). Exactly one of `run_id` / `issue` is normally set (the tool defaults them from
    /// SYMPHONY_RUN_ID / SYMPHONY_ISSUE); both empty ⇒ a `bad_request` FacadeError.
    pub(crate) async fn run_status(
        &self,
        now: DateTime<Utc>,
        run_id: &str,
        issue: &str,
    ) -> Result<Status, FacadeError> {
        if run_id.is_empty() && issue.is_empty() {
            return Err(FacadeError::new(
                "bad_request",
                "no run_id or issue given (and neither SYMPHONY_RUN_ID nor SYMPHONY_ISSUE is set)",
            ));
        }

        let state = self.get_state().await?;
        let mut input = VerdictInput::new(now, default_stall_threshold());

        if !run_id.is_empty() {
            input.running = find_running_by_run_id(&state, run_id);
            match self.get_run(run_id).await {
                Ok(run) => input.run = Some(run),
                Err(e) => {
                    // Trust the /state running row ONLY when there is simply no persisted detail to
                    // read — the daemon's two 404 codes (not_found / run_not_found). Any OTHER
                    // failure is SURFACED, never masked as a possibly-stale "alive".
                    if !(is_not_found(&e) && input.running.is_some()) {
                        return Err(e);
                    }
                }
            }
            return Ok(verdict(&input));
        }

        // issue path
        input.running = find_running_by_issue(&state, issue);
        let hist = self.get_issue_history(issue).await?;
        if let Some(latest) = hist.runs.iter().max_by_key(|r| r.id) {
            // Pull full detail for the latest run so outcome/error/last_event are authoritative.
            match self.get_run(&latest.id.to_string()).await {
                Ok(run) => input.run = Some(run),
                Err(e) if is_not_found(&e) => {
                    // No persisted detail (persistence disabled / pruned): fall back to the history
                    // SUMMARY shape, which still carries outcome/error the verdict reads.
                    input.run = Some(RunDetail {
                        run_id: latest.id,
                        issue_identifier: latest.issue_identifier.clone(),
                        outcome: latest.outcome.clone(),
                        error: latest.error.clone(),
                        ..Default::default()
                    });
                }
                // A non-not-found failure must be SURFACED, not masked behind the summary.
                Err(e) => return Err(e),
            }
        }
        // No running row and no run history ⇒ verdict() reports not-dispatched(unknown).
        Ok(verdict(&input))
    }
}

/// Whether `err` carries one of the daemon's two 404 "no detail row" codes — the getRun failures
/// the status verdict tolerates, as opposed to a transport/5xx failure that must be surfaced
/// (status.go's `isNotFound`):
///
/// - `not_found`     — run id 0 / invalid id (persistence-disabled runs use id 0)
/// - `run_not_found` — a VALID id absent from BOTH the live snapshot and the history store
fn is_not_found(err: &FacadeError) -> bool {
    err.code == "not_found" || err.code == "run_not_found"
}

/// The `/state.running` row for a numeric run id (status.go's `findRunningByRunID`).
fn find_running_by_run_id(s: &StateResp, run_id: &str) -> Option<crate::client::RunningSession> {
    let id: i64 = run_id.parse().ok()?;
    s.running.iter().find(|r| r.run_id == id).cloned()
}

/// The `/state.running` row for an issue identifier (status.go's `findRunningByIssue`).
fn find_running_by_issue(s: &StateResp, issue: &str) -> Option<crate::client::RunningSession> {
    s.running
        .iter()
        .find(|r| r.issue_identifier == issue)
        .cloned()
}

#[cfg(test)]
mod tests {
    //! Mirror of the client-level `runStatus` tests in `$REF/internal/mcpfacade/status_test.go`
    //! (the server-level `TestRunStatusExplicitIssueBeatsEnvRunID` lives in `server` tests).
    use crate::testutil::{client_for_port, spawn_router};
    use crate::verdict::{KIND_ALIVE, KIND_COMPLETED};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::any;
    use chrono::Utc;

    /// A stub route returning `body` with `status` (cloned per request so the handler stays `Fn`).
    fn route(status: StatusCode, body: impl Into<String>) -> axum::routing::MethodRouter {
        let body = body.into();
        any(move || {
            let body = body.clone();
            async move { (status, [("Content-Type", "application/json")], body) }
        })
    }

    // On the run-id path, a NON-not-found /runs/{id} failure (500) must be SURFACED even when
    // /state still lists the run running — never masked as a stale "alive".
    #[tokio::test]
    async fn run_status_get_run_transport_error_surfaces() {
        let now = Utc::now();
        let fresh = now.to_rfc3339();
        let state = format!(
            r#"{{"status":"running","running":[{{"issue_identifier":"INF-1","run_id":7,"last_event_at":"{fresh}"}}]}}"#
        );
        let router = Router::new()
            .route("/api/v1/state", route(StatusCode::OK, state))
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"error":{"code":"store_error","message":"boom"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let err = client
            .run_status(now, "7", "")
            .await
            .expect_err("500 must surface, not mask as alive");
        assert_eq!(err.code, "store_error");
    }

    // A NOT-FOUND run detail with a live /state row is the persistence-disabled case: trust the
    // running row and report alive.
    #[tokio::test]
    async fn run_status_get_run_not_found_falls_back_to_running_row() {
        let now = Utc::now();
        let fresh = now.to_rfc3339();
        let state = format!(
            r#"{{"status":"running","running":[{{"issue_identifier":"INF-1","run_id":7,"last_event_at":"{fresh}"}}]}}"#
        );
        let router = Router::new()
            .route("/api/v1/state", route(StatusCode::OK, state))
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::NOT_FOUND,
                    r#"{"error":{"code":"not_found","message":"no such run"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let st = client
            .run_status(now, "7", "")
            .await
            .expect("fallback alive");
        assert_eq!(st.kind, KIND_ALIVE);
    }

    // The daemon returns run_not_found (NOT not_found) for a valid-but-unknown id; the fallback
    // must recognize both codes.
    #[tokio::test]
    async fn run_status_get_run_run_not_found_falls_back_to_running_row() {
        let now = Utc::now();
        let fresh = now.to_rfc3339();
        let state = format!(
            r#"{{"status":"running","running":[{{"issue_identifier":"INF-1","run_id":7,"last_event_at":"{fresh}"}}]}}"#
        );
        let router = Router::new()
            .route("/api/v1/state", route(StatusCode::OK, state))
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::NOT_FOUND,
                    r#"{"error":{"code":"run_not_found","message":"no run with id: 7"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let st = client
            .run_status(now, "7", "")
            .await
            .expect("fallback alive");
        assert_eq!(st.kind, KIND_ALIVE);
    }

    // On the ISSUE path, a NON-not-found /runs/{id} failure while fetching the latest history run's
    // detail must be SURFACED, not silently folded into the history-summary fallback.
    #[tokio::test]
    async fn run_status_issue_path_get_run_transport_error_surfaces() {
        let now = Utc::now();
        let router = Router::new()
            .route(
                "/api/v1/state",
                route(StatusCode::OK, r#"{"status":"idle","running":[]}"#),
            )
            .route(
                "/api/v1/issues/INF-1/history",
                route(
                    StatusCode::OK,
                    r#"{"issue_identifier":"INF-1","runs":[{"id":7,"issue_identifier":"INF-1","outcome":"completed"}]}"#,
                ),
            )
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"error":{"code":"store_error","message":"boom"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let err = client
            .run_status(now, "", "INF-1")
            .await
            .expect_err("issue-path 500 must surface");
        assert_eq!(err.code, "store_error");
    }

    // A NOT-FOUND latest-run detail on the issue path falls back to the history SUMMARY.
    #[tokio::test]
    async fn run_status_issue_path_get_run_not_found_falls_back_to_summary() {
        let now = Utc::now();
        let ended = now.to_rfc3339();
        let hist = format!(
            r#"{{"issue_identifier":"INF-1","runs":[{{"id":7,"issue_identifier":"INF-1","outcome":"completed","ended_at":"{ended}"}}]}}"#
        );
        let router = Router::new()
            .route(
                "/api/v1/state",
                route(StatusCode::OK, r#"{"status":"idle","running":[]}"#),
            )
            .route("/api/v1/issues/INF-1/history", route(StatusCode::OK, hist))
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::NOT_FOUND,
                    r#"{"error":{"code":"not_found","message":"no such run"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let st = client
            .run_status(now, "", "INF-1")
            .await
            .expect("summary fallback");
        assert_eq!(st.kind, KIND_COMPLETED);
    }

    // The issue-path mirror of the run_not_found fallback.
    #[tokio::test]
    async fn run_status_issue_path_get_run_run_not_found_falls_back_to_summary() {
        let now = Utc::now();
        let ended = now.to_rfc3339();
        let hist = format!(
            r#"{{"issue_identifier":"INF-1","runs":[{{"id":7,"issue_identifier":"INF-1","outcome":"completed","ended_at":"{ended}"}}]}}"#
        );
        let router = Router::new()
            .route(
                "/api/v1/state",
                route(StatusCode::OK, r#"{"status":"idle","running":[]}"#),
            )
            .route("/api/v1/issues/INF-1/history", route(StatusCode::OK, hist))
            .route(
                "/api/v1/runs/7",
                route(
                    StatusCode::NOT_FOUND,
                    r#"{"error":{"code":"run_not_found","message":"no run with id: 7"}}"#,
                ),
            );
        let client = client_for_port(spawn_router(router).await);
        let st = client
            .run_status(now, "", "INF-1")
            .await
            .expect("summary fallback");
        assert_eq!(st.kind, KIND_COMPLETED);
    }
}
