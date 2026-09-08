//! handlers_runmerge — the console's merge action: `POST /api/v1/runs/{id}/merge` (STUDIO-767;
//! design record `~/.rhapsody/docs/STUDIO-767-console-merge-action.md`, §2, §3, §7 slice 3).
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — the additive shape `/api/v1/reviews` and
//! `/api/v1/teams/*` established. It IS a new served route, so it carries a README Divergences
//! entry alongside `/api/v1/version` and the history-paging endpoints.
//!
//! # The route's whole security argument, in one paragraph
//!
//! A merge moves `main`. §14.1's **F-SEC** finding — a `from: operator` room line is forgeable by
//! any local process — is why the trigger is this endpoint and not a room post, and why
//! [`rhapsody_orchestrator::teamsears`]'s closed `Intent` enum gains no `Merge` variant. What the
//! loopback listener buys is NOT authentication (see [`crate::handlers_reviews`]'s module doc,
//! which says so outright, and §G5, which says the honest version: a local process under
//! `bypassPermissions` already holds the operator's `gh` login). What it buys is that the
//! coordinate comes out of the daemon's own run row instead of out of text anybody can write.
//!
//! **The request body carries no pull-request number, no repository and no branch** — guardrail
//! G1, expressed as a type ([`RunMergeReq`]). The only client-supplied values are the `{id}` path
//! segment, which [`parse_run_id`] already rejects unless it is a positive run id, and a
//! confirmation token whose only power is to fail to match. Everything else is derived on the
//! control task from the run row, whose `repo` is written from the project's configured remote and
//! never from an agent.
//!
//! # The confirm handshake (G3)
//!
//! A merge is hard to reverse, so a console-side "are you sure" — which a forged POST simply skips
//! — is not the confirmation. The confirmation is server-enforced and takes two round trips:
//!
//! 1. `POST` with no `confirm` resolves the pull request, merges **nothing**, and answers **409
//!    `confirm_required`** carrying the receipt: the coordinate, the URL and the head SHA.
//! 2. The console renders that receipt and re-POSTs `{"confirm": "<head_sha>"}`. The daemon
//!    refuses unless it equals the head SHA it just re-resolved, so a push between the two legs
//!    invalidates the confirmation and the operator re-reads what they are about to merge.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::runmerge::{MergeControlOutcome, MergeReceipt};
use serde::{Deserialize, Serialize};

use crate::handlers::require_post;
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds the body at the door. The only field is a 40-character SHA; anything approaching this is
/// not one. Mirrors [`crate::handlers_reviews`]'s `MAX_CONTROL_BODY`.
const MAX_MERGE_BODY: usize = 8 << 10;

/// The merge request body — **one optional field, and it is not a coordinate**.
///
/// There is deliberately no `number`, no `owner`, no `repo` and no `branch` here, and adding one
/// would be the diff that breaks guardrail G1: with no such field there is no code path from a
/// client-supplied integer to `gh pr merge`, which is a stronger statement than "the handler
/// validates it". `merge_request_type_carries_no_pull_request_coordinate` pins that.
#[derive(Debug, Default, Deserialize)]
struct RunMergeReq {
    /// The head SHA the operator is confirming, echoed from a `confirm_required` receipt. Empty or
    /// absent ⇒ the handshake's first leg (§3/G3).
    #[serde(default)]
    confirm: String,
}

/// The 409 `confirm_required` body: the standard error envelope, plus the receipt the operator is
/// being asked to confirm.
///
/// The envelope is repeated here rather than reused because [`write_error`] has nowhere to put a
/// receipt, and the console's error handling reads `body.error.message` on every route — so the
/// one answer that carries a payload keeps that shape rather than becoming a special case the
/// client has to know about twice.
#[derive(Serialize)]
struct ConfirmRequiredJson<'a> {
    error: ConfirmRequiredError,
    receipt: &'a MergeReceipt,
}

#[derive(Serialize)]
struct ConfirmRequiredError {
    code: &'static str,
    message: &'static str,
}

/// `POST /api/v1/runs/{id}/merge` — merge the run's pull request (§7 slice 3).
///
/// | Outcome | Status | Code |
/// |---|---|---|
/// | merged, or GitHub's auto-merge armed | 200 | — (the receipt) |
/// | not confirmed | 409 | `confirm_required` (+ the receipt) |
/// | Teams off | 409 | `teams_disabled` |
/// | refused (no open PR, closed, already merged, a live review, in flight…) | 409 | `merge_refused` |
/// | no such run, or a bad `{id}` | 404 | `not_found` |
/// | the lookup or the merge itself failed | 500 | `merge_failed` |
///
/// Every refusal is one status for [`crate::handlers_reviews`]'s reason: the reasons differ in
/// kind, but they agree on what the caller is being told — the daemon's state and this request
/// cannot both stand — and splitting them across 400/403/409 would need this handler to re-derive
/// which kind each is from its text.
pub(crate) async fn handle_run_merge(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to merge a run's pull request") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    if body.len() > MAX_MERGE_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "request body is too large for a merge confirmation",
            None,
        );
    }
    // An EMPTY body is the handshake's first leg and not a malformed request: the console's first
    // POST carries nothing at all, and requiring `{}` of it would be a wire quirk for no gain.
    let req: RunMergeReq = if body.is_empty() {
        RunMergeReq::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(e) => {
                return write_error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("invalid JSON body: {e}"),
                    None,
                );
            }
        }
    };
    render(provider.merge_run(run_id, req.confirm.trim()).await)
}

/// Maps the daemon's six-way outcome onto the response envelope.
fn render(outcome: MergeControlOutcome) -> Response {
    match outcome {
        MergeControlOutcome::Applied(receipt) => write_json(StatusCode::OK, &receipt),
        MergeControlOutcome::ConfirmRequired(receipt) => write_json(
            StatusCode::CONFLICT,
            &ConfirmRequiredJson {
                error: ConfirmRequiredError {
                    code: "confirm_required",
                    message: "confirm the merge by echoing the pull request's head commit",
                },
                receipt: &receipt,
            },
        ),
        MergeControlOutcome::Dormant => write_error(
            StatusCode::CONFLICT,
            "teams_disabled",
            "Rhapsody Teams is not enabled on this daemon",
            None,
        ),
        MergeControlOutcome::NotFound => {
            write_error(StatusCode::NOT_FOUND, "not_found", "no such run", None)
        }
        MergeControlOutcome::Refused(why) => {
            write_error(StatusCode::CONFLICT, "merge_refused", why, None)
        }
        MergeControlOutcome::Failed(err) => {
            write_error(StatusCode::INTERNAL_SERVER_ERROR, "merge_failed", err, None)
        }
    }
}

#[cfg(test)]
mod tests {
    //! The route end to end over a real loopback listener, against a canned provider. What is
    //! asserted here is the WIRE contract — the statuses, the receipt shape, the handshake, and
    //! that nothing but the run id and the confirmation reaches the daemon. Every DECISION
    //! (coordinate resolution, the refusals, single-flight, the audit record) belongs to
    //! `rhapsody_orchestrator`'s `runmerge`/`mergeconsole` and is tested there.

    use std::sync::Arc;

    use serde_json::Value;

    use super::*;
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn receipt() -> MergeReceipt {
        MergeReceipt {
            run_id: 7,
            issue: "STUDIO-767".to_string(),
            pr: "makewhatis/rhapsody#64".to_string(),
            url: "https://github.com/makewhatis/rhapsody/pull/64".to_string(),
            number: 64,
            head_sha: HEAD.to_string(),
            method: "squash".to_string(),
            auto: true,
            merge_state: "CLEAN".to_string(),
            said: String::new(),
        }
    }

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn post(url: &str, body: &str) -> reqwest::Response {
        let mut req = reqwest::Client::new().post(url);
        if !body.is_empty() {
            req = req
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        req.send().await.expect("POST")
    }

    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    /// **G3's first leg, on the wire.** An unconfirmed POST is a 409 `confirm_required` carrying
    /// the receipt — the coordinate, the URL and the head SHA the operator must echo back.
    #[tokio::test]
    async fn an_unconfirmed_merge_is_a_409_carrying_the_receipt() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_merge_outcome(MergeControlOutcome::ConfirmRequired(receipt())),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = post(&format!("{url}/api/v1/runs/7/merge"), "").await;

        assert_eq!(resp.status(), 409);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "confirm_required");
        assert_eq!(body["receipt"]["pr"], "makewhatis/rhapsody#64");
        assert_eq!(body["receipt"]["head_sha"], HEAD);
        assert_eq!(body["receipt"]["number"], 64);
        assert_eq!(body["receipt"]["method"], "squash");
        assert_eq!(body["receipt"]["auto"], true);
        assert_eq!(
            provider.merge_asked(),
            Some((7, String::new())),
            "an empty body is the first leg, not a malformed request"
        );
    }

    /// The second leg: the confirmation reaches the daemon verbatim (trimmed), and a merge answers
    /// 200 with the receipt including `gh`'s own words.
    #[tokio::test]
    async fn a_confirmed_merge_answers_200_with_the_receipt() {
        let applied = MergeReceipt {
            said: "✓ Pull request #64 will be automatically merged".to_string(),
            ..receipt()
        };
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_merge_outcome(MergeControlOutcome::Applied(applied)),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = post(
            &format!("{url}/api/v1/runs/7/merge"),
            &format!("{{\"confirm\":\"  {HEAD}  \"}}"),
        )
        .await;

        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(
            body["url"],
            "https://github.com/makewhatis/rhapsody/pull/64"
        );
        assert_eq!(
            body["said"],
            "✓ Pull request #64 will be automatically merged"
        );
        assert_eq!(provider.merge_asked(), Some((7, HEAD.to_string())));
    }

    /// **G1, on the wire.** A body naming a pull request gets that number IGNORED: the handler
    /// forwards the run id and the confirmation and nothing else, because there is no field on the
    /// request type for a coordinate to travel in.
    #[tokio::test]
    async fn a_client_supplied_pull_request_number_reaches_nothing() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_merge_outcome(MergeControlOutcome::Applied(receipt())),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = post(
            &format!("{url}/api/v1/runs/7/merge"),
            &format!(
                "{{\"confirm\":\"{HEAD}\",\"number\":999,\"owner\":\"attacker\",\
                 \"repo\":\"evil\",\"branch\":\"main\"}}"
            ),
        )
        .await;

        assert_eq!(resp.status(), 200);
        assert_eq!(
            provider.merge_asked(),
            Some((7, HEAD.to_string())),
            "only the run id and the confirmation cross the door"
        );
    }

    /// **G1, structurally.** The request type has no coordinate field at all, which is a stronger
    /// statement than "the handler validates one". Asserted on this file's own source, so adding
    /// one is a failing test rather than a review someone has to catch.
    #[test]
    fn the_merge_request_type_carries_no_pull_request_coordinate() {
        let src = include_str!("handlers_runmerge.rs");
        let start = src
            .find("struct RunMergeReq {")
            .expect("the request type is still called RunMergeReq");
        let body = &src[start..start + src[start..].find('}').expect("a closing brace")];
        // FIELDS only: a doc comment is free to talk about pull requests, and this check is about
        // what the type can HOLD.
        let fields: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("#["))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["number", "owner", "repo", "branch", "pr", "url"] {
            assert!(
                !fields.contains(forbidden),
                "RunMergeReq gained a `{forbidden}` field: the merge coordinate must come from \
                 the run row and never from the request body (design §3/G1)"
            );
        }
    }

    /// Teams off is `teams_disabled`, not a 404 and not a silent success: the console reads it and
    /// keeps Merge dependency-named.
    #[tokio::test]
    async fn a_dormant_daemon_answers_teams_disabled() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = post(&format!("{url}/api/v1/runs/7/merge"), "").await;
        assert_eq!(resp.status(), 409);
        assert_eq!(body_json(resp).await["error"]["code"], "teams_disabled");
    }

    /// Every daemon refusal is one status and one code, with the daemon's own reason as the
    /// message — `handlers_reviews`'s discipline, for its reason.
    #[tokio::test]
    async fn a_refusal_is_a_409_carrying_the_daemons_own_reason() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_merge_outcome(
            MergeControlOutcome::Refused("a Rhapsody review of that pull request is still live"),
        ));
        let url = spawn(provider).await;
        let resp = post(&format!("{url}/api/v1/runs/7/merge"), "").await;
        assert_eq!(resp.status(), 409);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "merge_refused");
        assert_eq!(
            body["error"]["message"],
            "a Rhapsody review of that pull request is still live"
        );
    }

    /// A run that is not there is a 404, and so is a `{id}` that was never a run id — `parse_run_id`
    /// rejects a zero, a negative and a non-number alike.
    #[tokio::test]
    async fn a_missing_run_is_a_404() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_merge_outcome(MergeControlOutcome::NotFound),
        );
        let url = spawn(provider).await;
        assert_eq!(
            post(&format!("{url}/api/v1/runs/7/merge"), "")
                .await
                .status(),
            404
        );
        for id in ["0", "-3", "abc"] {
            assert_eq!(
                post(&format!("{url}/api/v1/runs/{id}/merge"), "")
                    .await
                    .status(),
                404,
                "run id {id:?}"
            );
        }
    }

    /// A failed lookup or a merge GitHub refused is a 500 carrying `gh`'s own complaint — the
    /// operator reads what GitHub said, not a paraphrase.
    #[tokio::test]
    async fn a_failure_is_a_500_carrying_githubs_complaint() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_merge_outcome(
            MergeControlOutcome::Failed(
                "Pull request is not mergeable: merge conflicts".to_string(),
            ),
        ));
        let url = spawn(provider).await;
        let resp = post(&format!("{url}/api/v1/runs/7/merge"), "").await;
        assert_eq!(resp.status(), 500);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "merge_failed");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("merge conflicts"),
            "{body}"
        );
    }

    /// The route is POST-only, and says so in `Allow` — a GET must never be able to merge
    /// anything, least of all from a link somebody clicks.
    #[tokio::test]
    async fn the_route_is_post_only() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_merge_outcome(MergeControlOutcome::NotFound),
        );
        let url = spawn(Arc::clone(&provider)).await;
        let resp = reqwest::get(format!("{url}/api/v1/runs/7/merge"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers().get("allow").and_then(|v| v.to_str().ok()),
            Some("POST")
        );
        assert_eq!(provider.merge_asked(), None, "the daemon was never asked");
    }

    /// A malformed or oversized body is rejected at the door, before the daemon is asked anything.
    #[tokio::test]
    async fn a_bad_body_never_reaches_the_daemon() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_merge_outcome(MergeControlOutcome::NotFound),
        );
        let url = spawn(Arc::clone(&provider)).await;
        for body in ["{".to_string(), "x".repeat(MAX_MERGE_BODY + 1)] {
            let resp = post(&format!("{url}/api/v1/runs/7/merge"), &body).await;
            assert_eq!(resp.status(), 400);
            assert_eq!(body_json(resp).await["error"]["code"], "bad_request");
        }
        assert_eq!(provider.merge_asked(), None, "the daemon was never asked");
    }
}
