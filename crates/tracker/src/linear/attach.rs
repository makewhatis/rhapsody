//! GitHub pull-request attachments — putting a clickable link to the pull request on the ticket
//! (STUDIO-875, corrected by STUDIO-882). **No Go v0.4.0 counterpart**: Symphony only ever READ
//! attachments, on the assumption that Linear's own GitHub integration had written them.
//!
//! # What this write is for, and what it was believed to be for
//!
//! It was shipped to repair summons routing. `apply_github_summons` attributes a summoning PR
//! comment to an issue by walking that issue's `linked_prs`, which `normalize` builds from the
//! issue's GitHub attachments; on a workspace whose repository is not connected in Linear's GitHub
//! integration every issue comes back with `attachments: []`, so a review that files findings posts
//! a perfectly good `@rhapsody` comment and the daemon drops it on every poll, forever.
//!
//! **That is not what this write achieves, and STUDIO-882 measured it rather than reasoning about
//! it.** Read back off the live API, an attachment this function wrote for a repository with no
//! GitHub integration answers:
//!
//! ```text
//! sourceType: "api"      metadata: {}
//! ```
//!
//! where an integration-written attachment on a CONNECTED repository answers:
//!
//! ```text
//! sourceType: "github"   metadata: { url, number, status, mergedAt, … }
//! ```
//!
//! So [`normalize::is_github_pr`](super::normalize) rejects it on the `sourceType` gate, and even
//! with that gate widened `linked_prs` would still get nothing, because it is built by matching a
//! pull-request url out of `metadata.url` and `metadata` is empty. There is no attachment this
//! daemon can write on an unconnected repository that `linked_prs` will accept. Summons routing
//! moved to the daemon's own record of the link instead (`ghenrich::DaemonPrLinks`).
//!
//! What the write still does, and the only reason it survives, is put `makewhatis/rhapsody#159`
//! on the Linear issue as a link a PERSON can click. On an unconnected repository nothing else
//! does.
//!
//! # `attachmentLinkGitHubPR`, not `attachmentLinkURL`
//!
//! Kept, but no longer for the stated reason. The claim was that the mutation, not the caller,
//! decides `sourceType`, so the GitHub-specific one yields `"github"` and the generic one would not
//! — load-bearing, not cosmetic. The first half is true and the conclusion is false: what the
//! mutation yields on an UNCONNECTED repository is `"api"`, the same as the generic one would, so
//! neither reaches `linked_prs`. It is kept because on a repository that IS connected it is the
//! honest mutation for what is being linked, and switching it now would be an unmeasured change to
//! the case that works, for no gain in the case that does not.
//!
//! # Repeats
//!
//! Linear keys a link attachment on (issue, url), so re-linking a pull request already attached to
//! the same issue is expected to be a no-op. Nothing here DEPENDS on that: the caller's own gate
//! (`prlink::link_pr_best_effort` — "this ticket already links the pull request that was just
//! resolved") is what keeps a working installation from writing at all, and a duplicate that got
//! through would be two identical links in Linear's UI. Untidy, and nothing reads them.

use super::client::traced;
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use serde::Deserialize;
use serde_json::json;

/// The `attachmentLinkGitHubPR { success attachment { id } }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AttachmentLinkResp {
    #[serde(rename = "attachmentLinkGitHubPR")]
    attachment_link: AttachmentLinkNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AttachmentLinkNode {
    success: bool,
    /// `Option` because Linear answers a refused link with `attachment: null`, and this adapter's
    /// standing rule is that every nullable field decodes rather than failing the whole response —
    /// a decode error here would report `UnknownPayload` for what is plainly a rejection.
    attachment: Option<AttachmentIdNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AttachmentIdNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

/// LinkPullRequest attaches `url` to `issue_id` as a GitHub pull-request attachment.
///
/// A `success: false` response, or a success carrying no attachment id, is
/// [`MoveRejected`](LinearErrorKind::MoveRejected) — the same treatment `create_comment` gives a
/// write that reports success without producing anything, and for the same reason: a caller must be
/// able to tell that the link did not land, because the whole point of the call is that something
/// LATER reads the attachment back.
pub(super) async fn link_pull_request(
    c: &Client,
    issue_id: &str,
    url: &str,
) -> Result<(), TrackerError> {
    traced(crate::tracker_span!("link_pull_request"), async move {
        if issue_id.is_empty() || url.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!("link pull request requires issueID and url (got {issue_id:?},{url:?})"),
            )
            .into());
        }
        let vars = json!({ "issueId": issue_id, "url": url });
        let resp: AttachmentLinkResp = c
            .do_graphql(query::MUTATION_ATTACHMENT_LINK_GITHUB_PR, Some(vars))
            .await?;
        let id = resp
            .attachment_link
            .attachment
            .map(|a| a.id)
            .unwrap_or_default();
        if !resp.attachment_link.success || id.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!(
                    "link pull request {url} to issue {issue_id} (success={}, id={id:?})",
                    resp.attachment_link.success
                ),
            )
            .into());
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tracker as _;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, new};
    use std::sync::{Arc, Mutex};

    fn client_at(url: String) -> Client {
        new(Config {
            endpoint: url,
            api_key: "k".into(),
            project_slug: "proj".into(),
            ..Config::default()
        })
    }

    fn is_kind(err: &TrackerError, kind: LinearErrorKind) -> bool {
        matches!(err, TrackerError::Linear(e) if e.kind == kind)
    }

    const PR_URL: &str = "https://github.com/makewhatis/rhapsody/pull/154";

    /// The mutation carries exactly the two coordinates the link needs, and it is the GitHub-PR
    /// mutation rather than the generic URL one.
    ///
    /// STUDIO-882 note: this pins WHICH mutation is sent, and nothing more. It does NOT show that
    /// the resulting attachment reaches `Issue::linked_prs` — measured against the live API, on an
    /// unconnected repository it does not (`sourceType: "api"`, `metadata: {}`; see the module
    /// doc). That gap between "the write is well-formed" and "the write counts" is what let 875
    /// ship green and not work.
    #[tokio::test]
    async fn link_pull_request_sends_the_github_pr_mutation_with_the_issue_and_url() {
        let seen: Arc<Mutex<(String, serde_json::Value)>> =
            Arc::new(Mutex::new((String::new(), serde_json::Value::Null)));
        let rec = Arc::clone(&seen);
        let server = MockServer::start(move |req| {
            *rec.lock().expect("seen") = (req.query.to_string(), req.variables.clone());
            MockResp::ok(
                r#"{"data":{"attachmentLinkGitHubPR":{"success":true,"attachment":{"id":"att-1"}}}}"#,
            )
        })
        .await;

        client_at(server.url())
            .link_pull_request("iss-uuid", PR_URL)
            .await
            .expect("link_pull_request");

        let (doc, vars) = seen.lock().expect("seen").clone();
        assert!(
            doc.contains("attachmentLinkGitHubPR("),
            "the GitHub-PR mutation is what stamps sourceType github, got {doc}"
        );
        assert_eq!(vars["issueId"], "iss-uuid");
        assert_eq!(vars["url"], PR_URL);
    }

    /// A refusal must be a refusal. The caller treats the link as best-effort, but "best-effort"
    /// means it may LOG the failure — it must never be told a link landed that did not.
    #[tokio::test]
    async fn a_rejected_link_is_an_error() {
        let server = MockServer::start(|_| {
            MockResp::ok(
                r#"{"data":{"attachmentLinkGitHubPR":{"success":false,"attachment":null}}}"#,
            )
        })
        .await;
        let err = client_at(server.url())
            .link_pull_request("iss-uuid", PR_URL)
            .await
            .expect_err("a refused link must error");
        assert!(is_kind(&err, LinearErrorKind::MoveRejected), "got {err:?}");
    }

    /// Linear reporting success while producing no attachment is the same defect wearing a
    /// success code: nothing was linked, so nothing will read back.
    #[tokio::test]
    async fn success_with_no_attachment_is_an_error() {
        let server = MockServer::start(|_| {
            MockResp::ok(
                r#"{"data":{"attachmentLinkGitHubPR":{"success":true,"attachment":{"id":""}}}}"#,
            )
        })
        .await;
        let err = client_at(server.url())
            .link_pull_request("iss-uuid", PR_URL)
            .await
            .expect_err("an empty attachment id must error");
        assert!(is_kind(&err, LinearErrorKind::MoveRejected), "got {err:?}");
    }

    /// Argument validation is local and refuses before any request is made, matching every other
    /// write in this adapter.
    #[tokio::test]
    async fn an_incomplete_coordinate_never_reaches_the_network() {
        let calls = Arc::new(Mutex::new(0usize));
        let rec = Arc::clone(&calls);
        let server = MockServer::start(move |_| {
            *rec.lock().expect("calls") += 1;
            MockResp::ok(
                r#"{"data":{"attachmentLinkGitHubPR":{"success":true,"attachment":{"id":"att-1"}}}}"#,
            )
        })
        .await;
        let c = client_at(server.url());
        for (issue, url) in [("", PR_URL), ("iss-uuid", "")] {
            let err = c
                .link_pull_request(issue, url)
                .await
                .expect_err("an incomplete coordinate must error");
            assert!(is_kind(&err, LinearErrorKind::ApiRequest), "got {err:?}");
        }
        assert_eq!(*calls.lock().expect("calls"), 0, "no mutation was issued");
    }
}
