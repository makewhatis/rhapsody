//! handlers_rundiff — the console's Diff tab: `GET /api/v1/runs/{id}/diff` (STUDIO-749; design
//! record `~/.rhapsody/docs/console-run-detail-design.md` §5, §9 slice 7).
//!
//! §5 named this "the one real new endpoint" of the whole Trace redesign and deferred it. Until it
//! existed the console's Diff tab named its dependency and deep-linked to the pull request rather
//! than showing a diff nobody served; this route is what turns it into a live surface.
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — the additive shape `/api/v1/reviews` and
//! `/api/v1/teams/*` established. It IS a new served route, so it carries a README Divergences
//! entry alongside `/api/v1/version`, `/api/v1/runs/{id}/merge` and the history-paging endpoints.
//!
//! # What this route is, and the two things it deliberately is not
//!
//! **GET-only, and it writes nothing.** The provider method it calls hands on
//! [`rhapsody_orchestrator::rundiff::DiffDeps`], which holds five READ seams and no
//! `MergeSource` — so there is no branch through this handler that merges, comments on or
//! otherwise touches a pull request. That is enforced by what the diff path is HANDED, the same
//! way [`crate::handlers_runmerge`]'s read half is, rather than by a flag.
//!
//! **It serves no mergeability VERDICT.** `GET /api/v1/runs/{id}/mergeability` already does, from
//! the daemon's one shared resolution, and a second implementation of that judgement is exactly
//! the disagreement that function exists to prevent. What rides on this response is GitHub's own
//! `merge_state` — a fact, not a verdict.
//!
//! # The coordinate, and why the body cannot name one
//!
//! The route takes no body at all: the only client-supplied value is the `{id}` path segment,
//! which [`parse_run_id`] rejects unless it is a positive run id. The repository comes from the
//! run row (written from the project's configured remote, never from an agent), the branch is
//! derived from the run's ticket, and the pull-request NUMBER is resolved from GitHub by head
//! branch — which rejects a fork's pull request. So the same guardrail-G1 shape the merge action
//! is built on holds here, for the same reason: no `gh` call this console can trigger should take
//! its coordinate from something a caller wrote.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::rundiff::{DiffOutcome, RunDiff};
use serde::Serialize;

use crate::handlers::require_get;
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// The "there is no diff, and here is why" body.
///
/// A **200**, not a 4xx, for [`crate::handlers_runmerge`]'s mergeability reason: the console asked
/// what this run changed and the daemon answered. An unpushed branch, a pull request that was
/// merged and closed, a remote that is not on GitHub — none of those is an error, and rendering
/// them as one would put a red state on the ordinary life of a ticket. The error envelope stays
/// reserved for the question that could not be ASKED, which is a different thing and must render
/// differently.
#[derive(Serialize)]
struct NoDiffJson<'a> {
    /// Always `false` here. Present so the console's narrowing is a discriminant check rather than
    /// a `"patch" in body` guess about a field that is legitimately empty on an empty diff.
    available: bool,
    /// The daemon's own sentence. The panel shows it verbatim.
    reason: &'a str,
}

/// The served diff: [`RunDiff`] plus the discriminant, so both arms of the union are read the same
/// way.
#[derive(Serialize)]
struct DiffJson<'a> {
    available: bool,
    #[serde(flatten)]
    diff: &'a RunDiff,
}

/// `GET /api/v1/runs/{id}/diff` — the unified diff this run produced on its branch.
///
/// | Outcome | Status | Body |
/// |---|---|---|
/// | resolved | 200 | `{"available": true, "patch": "…", "pr": "…", "checks": […], …}` |
/// | nothing to show | 200 | `{"available": false, "reason": "<the daemon's own sentence>"}` |
/// | no such run, or a bad `{id}` | 404 | `not_found` |
/// | the question could not be answered | 500 | `diff_unavailable` |
///
/// The 500's code is deliberately not `diff_refused` and not `diff_failed`: nothing was refused,
/// because this route refuses nothing, and nothing failed to happen because nothing was being
/// attempted. A `gh` that would not answer is a question nobody could ask, and the console renders
/// that as a retryable state rather than as the daemon reporting on the run.
pub(crate) async fn handle_run_diff(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    match provider.run_diff(run_id).await {
        DiffOutcome::Ready(diff) => write_json(
            StatusCode::OK,
            &DiffJson {
                available: true,
                diff: &diff,
            },
        ),
        DiffOutcome::Unavailable(reason) => write_json(
            StatusCode::OK,
            &NoDiffJson {
                available: false,
                reason,
            },
        ),
        DiffOutcome::NotFound => {
            write_error(StatusCode::NOT_FOUND, "not_found", "no such run", None)
        }
        DiffOutcome::Failed(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "diff_unavailable",
            err,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    //! The route end to end over a real loopback listener, against a canned provider. What is
    //! asserted here is the WIRE contract — the statuses, the union's discriminant, and that a
    //! reason is an ANSWER rather than an error. Every DECISION (which coordinate is derived, what
    //! ends the read, what merely thins it) belongs to `rhapsody_orchestrator`'s `rundiff` and is
    //! tested there.

    use std::sync::Arc;

    use rhapsody_orchestrator::ghsummons::CheckRun;
    use serde_json::Value;

    use super::*;
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PATCH: &str = "diff --git a/x.rs b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";

    fn diff() -> RunDiff {
        RunDiff {
            run_id: 7,
            issue: "STUDIO-749".to_string(),
            branch: "symphony/STUDIO-749".to_string(),
            pr: "makewhatis/rhapsody#64".to_string(),
            url: "https://github.com/makewhatis/rhapsody/pull/64".to_string(),
            number: 64,
            head_sha: HEAD.to_string(),
            merge_state: "CLEAN".to_string(),
            checks: vec![CheckRun {
                name: "test".to_string(),
                state: "SUCCESS".to_string(),
            }],
            patch: PATCH.to_string(),
            truncated: false,
        }
    }

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    /// The whole contract: a resolved diff crosses the wire with its patch, the coordinate the
    /// DAEMON derived, the head commit it is of, and the checks — flattened beside the
    /// discriminant so the console reads one shape.
    #[tokio::test]
    async fn a_resolved_diff_crosses_the_wire_whole() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_diff(DiffOutcome::Ready(Box::new(diff()))),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = reqwest::get(format!("{url}/api/v1/runs/7/diff"))
            .await
            .expect("GET");

        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["available"], true);
        assert_eq!(body["patch"], PATCH);
        assert_eq!(body["pr"], "makewhatis/rhapsody#64");
        assert_eq!(body["number"], 64);
        assert_eq!(body["head_sha"], HEAD);
        assert_eq!(body["branch"], "symphony/STUDIO-749");
        assert_eq!(body["merge_state"], "CLEAN");
        assert_eq!(body["checks"][0]["name"], "test");
        assert_eq!(body["checks"][0]["state"], "SUCCESS");
        assert_eq!(body["truncated"], false);
        assert_eq!(provider.diff_asked(), Some(7));
    }

    /// A truncated patch says so on the wire. The console must be able to tell a diff that ENDED
    /// from one that was cut, because they look identical otherwise.
    #[tokio::test]
    async fn a_truncated_patch_is_flagged() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_diff(DiffOutcome::Ready(Box::new(RunDiff {
                truncated: true,
                ..diff()
            }))),
        );
        let url = spawn(provider).await;
        let body = body_json(
            reqwest::get(format!("{url}/api/v1/runs/7/diff"))
                .await
                .expect("GET"),
        )
        .await;
        assert_eq!(body["truncated"], true);
    }

    /// "There is nothing to show" is a **200** carrying the daemon's own sentence, not an error:
    /// an unpushed branch is the ordinary life of a ticket, and a 4xx would paint it red.
    #[tokio::test]
    async fn nothing_to_show_is_the_answer_and_carries_the_daemons_own_words() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_diff(
            DiffOutcome::Unavailable("no open pull request on this run's branch"),
        ));
        let url = spawn(provider).await;

        let resp = reqwest::get(format!("{url}/api/v1/runs/7/diff"))
            .await
            .expect("GET");

        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["available"], false);
        assert_eq!(body["reason"], "no open pull request on this run's branch");
        assert!(body.get("patch").is_none(), "{body}");
    }

    /// A question nobody could answer is NOT "nothing to show", and gets its own status and code
    /// so the console can tell them apart — only the first is a statement about the run.
    #[tokio::test]
    async fn an_unanswerable_question_is_a_500_carrying_githubs_complaint() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_diff(DiffOutcome::Failed("gh pr diff: HTTP 502".to_string())),
        );
        let url = spawn(provider).await;

        let resp = reqwest::get(format!("{url}/api/v1/runs/7/diff"))
            .await
            .expect("GET");

        assert_eq!(resp.status(), 500);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "diff_unavailable");
        assert_eq!(body["error"]["message"], "gh pr diff: HTTP 502");
    }

    /// A run that is not there is a 404, and so is a `{id}` that was never a run id — the same
    /// vocabulary the merge routes use, so the console needs no second one.
    #[tokio::test]
    async fn a_missing_run_is_a_404() {
        let provider =
            Arc::new(FakeProvider::ok(empty_snapshot()).with_diff(DiffOutcome::NotFound));
        let url = spawn(Arc::clone(&provider)).await;
        assert_eq!(
            reqwest::get(format!("{url}/api/v1/runs/7/diff"))
                .await
                .expect("GET")
                .status(),
            404
        );
        for id in ["0", "-3", "abc"] {
            assert_eq!(
                reqwest::get(format!("{url}/api/v1/runs/{id}/diff"))
                    .await
                    .expect("GET")
                    .status(),
                404,
                "run id {id:?}"
            );
        }
    }

    /// The route is GET-only. A read surface that answered a POST would be one more door onto the
    /// same `gh` seams for no reason — and the console has exactly one write route per action.
    #[tokio::test]
    async fn the_route_is_get_only() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_diff(DiffOutcome::Ready(Box::new(diff()))),
        );
        let url = spawn(Arc::clone(&provider)).await;

        let resp = reqwest::Client::new()
            .post(format!("{url}/api/v1/runs/7/diff"))
            .send()
            .await
            .expect("POST");

        assert_eq!(resp.status(), 405);
        assert_eq!(provider.diff_asked(), None, "the daemon was never asked");
    }

    /// **Reading a diff must never merge one.** The whole handler is scanned for the merge
    /// provider methods, so wiring one in here is a failing test rather than a review someone has
    /// to catch (STUDIO-767 §5 keeps the two apart).
    #[test]
    fn the_diff_route_reaches_no_merge_path() {
        let whole = include_str!("handlers_rundiff.rs");
        let src = &whole[..whole.find("#[cfg(test)]").expect("a test module")];
        // CODE only. The module doc above names the merge route at length — saying why this one is
        // not it is the point of that prose — so a scan that read comments would match itself and
        // could never fail for the right reason.
        let code: String = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["merge_run", "run_mergeability", "MergeSource"] {
            assert!(
                !code.contains(forbidden),
                "handlers_rundiff reached `{forbidden}`: the Diff tab reads, it does not act"
            );
        }
    }
}
