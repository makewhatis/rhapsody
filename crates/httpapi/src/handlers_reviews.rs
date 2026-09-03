//! handlers_reviews — the ticketless review console's three routes (STUDIO-722, slice 8; design
//! record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §7, §15-e, §16).
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — the same additive shape
//! `/api/v1/capabilities` and `/api/v1/teams/*` established. Nothing is added to a parity-checked
//! view and no golden moves.
//!
//! | Route | Backs |
//! |---|---|
//! | `GET /api/v1/reviews` | the watch set: per (PR, reviewer) — status, both SHAs, the open flag |
//! | `POST /api/v1/reviews/rerun` | the operator asking for one more review round |
//! | `POST /api/v1/reviews/dismiss` | the operator taking a pull request out of the watch set |
//!
//! # These two writes are the security fix, not a convenience
//!
//! §14.1's fatal **F-SEC** finding is that a review checks a pull request's head out and reads its
//! diff under `bypassPermissions`, so whoever decides WHICH pull request that is decides what code
//! the daemon runs — and a room post's `from: operator` is forgeable by any local process. §15-e
//! moves operator control of reviews here, to **the authenticated console**, and this module is
//! that surface. The room reader gained no `pr:` Intent and must not gain one.
//!
//! "Authenticated" is the loopback listener itself, which is how every other write on this API is
//! authenticated ([`crate::handlers_runaction`]'s stop/resume, `POST /api/v1/config`): the server
//! binds loopback only, so reaching these routes already means being on the operator's machine.
//! What matters for F-SEC is what that replaces — a coordinate lifted out of room text, which any
//! agent can write, by one an operator typed into a console.
//!
//! # Gating (§16)
//!
//! `GET` answers `{enabled: false, reviews: []}` when Teams is off or the mode is not `ticketless`
//! — the "surface absent/empty" the acceptance asks for, rather than a 409. It is the console's only
//! way to learn the review MODE (`/api/v1/version`'s `teams_enabled` carries the Teams half and
//! nothing more), so this read is the surface's own capability probe and has to answer to be one.
//!
//! The two writes are ACTIONS, not probes, so they follow the `teams_*` discipline instead and
//! refuse with 409 `review_disabled`. A console that read the GET first never sends them.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::prstate::PrCoord;
use rhapsody_orchestrator::reviewconsole::ReviewControlOutcome;
use serde::{Deserialize, Serialize};

use crate::handlers::{require_get, require_post};
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds a control body at the door. A coordinate is three short fields; anything approaching this
/// is not one.
const MAX_CONTROL_BODY: usize = 8 << 10;

/// The body both control routes take: the pull request, and nothing else.
///
/// There is no `reviewer` field, deliberately. A review is a property of the pull request — a
/// two-reviewer round is one round — so both controls act on every row of the coordinate rather
/// than letting a caller steer one reviewer's half of it.
#[derive(Debug, Default, Deserialize)]
struct ReviewControlReq {
    #[serde(default)]
    owner: String,
    #[serde(default)]
    repo: String,
    /// The pull-request NUMBER — the coordinate `gh` and the watch set both key on. Signed because
    /// the store column is, so a negative reaches the daemon's own validation rather than being
    /// rejected by serde with a different error envelope.
    #[serde(default)]
    number: i64,
}

/// The 200 body for both control routes: how many watch-set rows the action changed.
#[derive(Serialize)]
struct ReviewControlJson {
    /// `owner/repo#number`, echoed so a log or a toast can name what was acted on.
    pr: String,
    /// Rows re-armed (rerun) or dropped (dismiss). `0` is possible and is not an error: every row
    /// of the pull request already had a review in flight.
    rows: usize,
}

/// `GET /api/v1/reviews` — the watch set as the Reviews surface renders it.
pub(crate) async fn handle_reviews(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.reviews().await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "review_backend_error",
            e.to_string(),
            None,
        ),
    }
}

/// `POST /api/v1/reviews/rerun` — one more review round of a watched pull request (§15-e).
pub(crate) async fn handle_review_rerun(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    let pr = match parse_control(&method, &body, "use POST to re-run a review") {
        Ok(pr) => pr,
        Err(resp) => return *resp,
    };
    let label = pr.to_string();
    render(provider.review_rerun(pr).await, label)
}

/// `POST /api/v1/reviews/dismiss` — take a pull request out of the watch set (§15-e).
pub(crate) async fn handle_review_dismiss(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    let pr = match parse_control(&method, &body, "use POST to dismiss a review") {
        Ok(pr) => pr,
        Err(resp) => return *resp,
    };
    let label = pr.to_string();
    render(provider.review_dismiss(pr).await, label)
}

/// POST-only, one bounded JSON coordinate. The error is `Box`ed so the common `Ok` path stays small
/// (clippy `result_large_err`: a [`Response`] is large), following [`crate::handlers_runaction`].
///
/// The coordinate is NOT validated here beyond being parseable. The daemon re-checks every field on
/// the control task — including the watched-repo allowlist — because that is the check that has to
/// hold whoever the caller is, and a copy of it at the door would be free to drift from it.
fn parse_control(
    method: &Method,
    body: &Bytes,
    verb: &'static str,
) -> Result<PrCoord, Box<Response>> {
    if let Some(resp) = require_post(method, verb) {
        return Err(Box::new(resp));
    }
    if body.len() > MAX_CONTROL_BODY {
        return Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "request body is too large for a pull-request coordinate",
            None,
        )));
    }
    let req: ReviewControlReq = serde_json::from_slice(body).map_err(|e| {
        Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("invalid JSON body: {e}"),
            None,
        ))
    })?;
    Ok(PrCoord::new(req.owner.trim(), req.repo.trim(), req.number))
}

/// Maps the daemon's four-way outcome onto the response envelope.
///
/// Every refusal is one status, `409`, with the daemon's own reason as the message. The reasons
/// differ in kind — an unwatched pull request, a repository off the allowlist, a coordinate with no
/// owner — but they agree on what the caller is being told: the daemon's state and this request
/// cannot both stand. Splitting them across 400/403/404 would need this handler to re-derive which
/// kind each reason is from its text, which is exactly the drift the door deliberately avoids.
fn render(outcome: ReviewControlOutcome, pr: String) -> Response {
    match outcome {
        ReviewControlOutcome::Applied(rows) => {
            write_json(StatusCode::OK, &ReviewControlJson { pr, rows })
        }
        // Business outcomes an operator reads and stops on, split from a backend failure they may
        // retry — the split [`crate::handlers_teams`]'s `teams_error` makes, for its reason.
        ReviewControlOutcome::Dormant => write_error(
            StatusCode::CONFLICT,
            "review_disabled",
            "ticketless review is not enabled on this daemon",
            None,
        ),
        ReviewControlOutcome::Refused(why) => {
            write_error(StatusCode::CONFLICT, "review_refused", why, None)
        }
        ReviewControlOutcome::Failed(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "review_backend_error",
            err,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    //! The three routes end to end over a real loopback listener, against a canned provider.
    //! What is asserted here is the WIRE contract — the shapes the console reads, the gating, and
    //! that the coordinates a control forwards are the ones the body carried. The decisions
    //! themselves (allowlist, in-flight guard, churn budget) belong to
    //! `rhapsody_orchestrator::reviewconsole` and are tested there against a real watch set.

    use std::sync::Arc;

    use rhapsody_orchestrator::reviewconsole::{ReviewJobRow, ReviewsView};
    use serde_json::Value;

    use super::*;
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
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

    fn row(reviewer: &str, status: &str, reviewed: &str, open: bool) -> ReviewJobRow {
        ReviewJobRow {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: reviewer.to_string(),
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-720".to_string(),
            requested_sha: HEAD_A.to_string(),
            last_reviewed_sha: reviewed.to_string(),
            status: status.to_string(),
            open,
        }
    }

    /// **Acceptance 1, on the wire.** Every watched pull request is served with its reviewer,
    /// status and `last_reviewed_sha` — the three columns the surface renders — plus the open flag.
    #[tokio::test]
    async fn the_reviews_route_serves_a_row_per_pr_and_reviewer() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()).with_reviews(
            ReviewsView {
                enabled: true,
                reviews: vec![
                    row("bob", "reviewed", HEAD_A, true),
                    row("carol", "in_flight", "", true),
                ],
            },
        )))
        .await;

        let view = body_json(
            reqwest::get(&format!("{url}/api/v1/reviews"))
                .await
                .expect("GET reviews"),
        )
        .await;

        assert_eq!(view["enabled"], true);
        assert_eq!(view["reviews"][0]["owner"], "makewhatis");
        assert_eq!(view["reviews"][0]["repo"], "rhapsody");
        assert_eq!(view["reviews"][0]["number"], 12);
        assert_eq!(view["reviews"][0]["reviewer"], "bob");
        assert_eq!(view["reviews"][0]["status"], "reviewed");
        assert_eq!(view["reviews"][0]["last_reviewed_sha"], HEAD_A);
        assert_eq!(view["reviews"][0]["author"], "alice");
        assert_eq!(view["reviews"][0]["introduced_by"], "handoff:STUDIO-720");
        assert_eq!(view["reviews"][0]["open"], true);
        assert_eq!(view["reviews"][1]["reviewer"], "carol");
        assert_eq!(view["reviews"][1]["last_reviewed_sha"], "");
    }

    /// **Acceptance 4, the read half (§16).** A daemon with Teams off — or with the mode not
    /// `ticketless` — serves an EMPTY, disabled surface rather than an error, so the console's own
    /// capability probe answers and the section simply does not render.
    #[tokio::test]
    async fn a_dormant_daemon_serves_an_empty_reviews_surface() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = reqwest::get(&format!("{url}/api/v1/reviews"))
            .await
            .expect("GET reviews");
        assert_eq!(resp.status(), 200);
        let view = body_json(resp).await;
        assert_eq!(view["enabled"], false);
        assert_eq!(view["reviews"].as_array().map(Vec::len), Some(0));
    }

    /// **Acceptances 2 and 3, on the wire.** Each control forwards exactly the coordinate its body
    /// carried and reports how many watch-set rows changed — and they are two distinct routes, not
    /// one registered twice.
    #[tokio::test]
    async fn each_control_forwards_the_bodys_coordinate_and_reports_the_rows_it_changed() {
        type Seen = fn(&FakeProvider) -> Option<PrCoord>;
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_review_outcome(ReviewControlOutcome::Applied(2)),
        );
        let url = spawn(Arc::clone(&provider)).await;

        for (path, seen) in [
            ("rerun", FakeProvider::review_rerun_pr as Seen),
            ("dismiss", FakeProvider::review_dismiss_pr as Seen),
        ] {
            assert_eq!(seen(&provider), None, "{path} has not been called yet");
            let resp = post(
                &format!("{url}/api/v1/reviews/{path}"),
                r#"{"owner":"makewhatis","repo":"rhapsody","number":12}"#,
            )
            .await;
            assert_eq!(resp.status(), 200, "{path}");
            let body = body_json(resp).await;
            assert_eq!(body["pr"], "makewhatis/rhapsody#12", "{path}");
            assert_eq!(body["rows"], 2, "{path}");
            assert_eq!(
                seen(&provider),
                Some(PrCoord::new("makewhatis", "rhapsody", 12)),
                "{path} must forward the body's own coordinate"
            );
        }
    }

    /// Surrounding whitespace is trimmed off the coordinate before it leaves the door — an operator
    /// pasting `makewhatis / rhapsody` from a URL bar should steer the same rows the watcher polls,
    /// not be refused for a space.
    #[tokio::test]
    async fn a_control_trims_the_coordinate_it_forwards() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_review_outcome(ReviewControlOutcome::Applied(1)),
        );
        let url = spawn(Arc::clone(&provider)).await;
        post(
            &format!("{url}/api/v1/reviews/rerun"),
            r#"{"owner":" makewhatis ","repo":" rhapsody\n","number":12}"#,
        )
        .await;
        assert_eq!(
            provider.review_rerun_pr(),
            Some(PrCoord::new("makewhatis", "rhapsody", 12))
        );
    }

    /// **Acceptance 4, the control half (§16).** A dormant daemon refuses both controls with
    /// `review_disabled` — the answer that says "configured off", not "something went wrong".
    #[tokio::test]
    async fn a_dormant_daemon_refuses_both_controls() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        for path in ["rerun", "dismiss"] {
            let resp = post(
                &format!("{url}/api/v1/reviews/{path}"),
                r#"{"owner":"makewhatis","repo":"rhapsody","number":12}"#,
            )
            .await;
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(err_code(resp).await, "review_disabled", "{path}");
        }
    }

    /// The daemon's refusal reaches the operator verbatim: 409 `review_refused` carrying the reason
    /// the control task gave. A paraphrase here would hide which of the four refusals happened, and
    /// "no configured project owns the PR's repo" is the one an operator must be able to act on.
    #[tokio::test]
    async fn a_refusal_carries_the_daemons_own_reason() {
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_review_outcome(ReviewControlOutcome::Refused(
                "no configured project owns the PR's repo",
            )),
        ))
        .await;
        let resp = post(
            &format!("{url}/api/v1/reviews/rerun"),
            r#"{"owner":"attacker","repo":"evil","number":1}"#,
        )
        .await;
        assert_eq!(resp.status(), 409);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "review_refused");
        assert_eq!(
            body["error"]["message"],
            "no configured project owns the PR's repo"
        );
    }

    /// A store failure is a 500 the operator may retry, split from the business outcomes above —
    /// the same split the `teams_*` routes make. Both the read and the controls owe it: a watch set
    /// that cannot be read must not render as "no reviews".
    #[tokio::test]
    async fn a_backend_failure_is_a_500_on_both_the_read_and_the_controls() {
        let url = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_reviews_error("database is locked")
                .with_review_outcome(ReviewControlOutcome::Failed(
                    "database is locked".to_string(),
                )),
        ))
        .await;

        let resp = reqwest::get(&format!("{url}/api/v1/reviews"))
            .await
            .expect("GET reviews");
        assert_eq!(resp.status(), 500);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "review_backend_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("database is locked"),
            "the store's own complaint reaches the operator: {}",
            body["error"]["message"]
        );

        let resp = post(
            &format!("{url}/api/v1/reviews/dismiss"),
            r#"{"owner":"makewhatis","repo":"rhapsody","number":12}"#,
        )
        .await;
        assert_eq!(resp.status(), 500);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "review_backend_error");
        assert_eq!(body["error"]["message"], "database is locked");
    }

    /// Method discipline, the crate's `any(...)` registration contract: a wrong method is a 405
    /// envelope, never the SPA fallback's 200 HTML.
    #[tokio::test]
    async fn each_route_enforces_its_own_method() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;

        let resp = post(&format!("{url}/api/v1/reviews"), "{}").await;
        assert_eq!(resp.status(), 405);
        assert_eq!(err_code(resp).await, "method_not_allowed");

        for path in ["rerun", "dismiss"] {
            let resp = reqwest::get(&format!("{url}/api/v1/reviews/{path}"))
                .await
                .expect("GET a POST-only route");
            assert_eq!(resp.status(), 405, "{path}");
            assert_eq!(err_code(resp).await, "method_not_allowed", "{path}");
        }
    }

    /// A malformed or oversized body is rejected at the door, in THIS crate's `{error, message}`
    /// envelope, and never reaches the control task.
    #[tokio::test]
    async fn a_malformed_or_oversized_body_is_rejected_at_the_door() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_review_outcome(ReviewControlOutcome::Applied(1)),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = post(&format!("{url}/api/v1/reviews/rerun"), "not json at all").await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "bad_request");

        let huge = format!(
            r#"{{"owner":"makewhatis","repo":"{}","number":12}}"#,
            "r".repeat(MAX_CONTROL_BODY)
        );
        let resp = post(&format!("{url}/api/v1/reviews/rerun"), &huge).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "bad_request");

        assert_eq!(
            provider.review_rerun_pr(),
            None,
            "neither body reached the control task"
        );
    }

    /// An absent field is empty rather than a serde rejection, so the daemon's own validation is
    /// what refuses it — one place decides what a usable coordinate is.
    #[tokio::test]
    async fn a_body_missing_fields_reaches_the_daemons_own_validation() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_review_outcome(
            ReviewControlOutcome::Refused("pull request has no owner/repo"),
        ));
        let url = spawn(Arc::clone(&provider)).await;
        let resp = post(&format!("{url}/api/v1/reviews/rerun"), r#"{"number":12}"#).await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "review_refused");
        assert_eq!(
            provider.review_rerun_pr(),
            Some(PrCoord::new("", "", 12)),
            "the empty coordinate is forwarded, not rejected here"
        );
    }
}
