//! Linear GraphQL tracker adapter — port of Go `internal/tracker/linear`.
//!
//! P3 T3 ships the foundation ported from `{client,query,errors,tracing,normalize}.go`: the
//! GraphQL transport ([`client`]), the byte-identical query builder ([`query`]), the typed error
//! sentinels ([`errors`]), and issue normalization ([`normalize`]). T4 fills in the read path
//! (candidates, by-states, by-ids, blocked-backlog + branch, projects) and T5 the write path
//! (state moves in [`move_state`], the pool-mode claim assign/comment surface in [`claim`]) — every
//! [`Tracker`](crate::Tracker) method now has a real body.

mod attach;
mod backlog;
mod by_ids;
mod by_states;
mod candidates;
mod claim;
mod client;
mod create;
mod decode;
mod documents;
mod errors;
mod issue_id;
mod labels;
mod move_state;
mod normalize;
mod projects;
pub mod query;
#[cfg(test)]
mod testutil;

pub(crate) use client::traced;
pub use client::{Client, Config, new};
pub use errors::{LinearError, LinearErrorKind};
pub use normalize::RawIssue;

use crate::{NewIssue, TrackerError};
use async_trait::async_trait;
use rhapsody_core::{Comment, Issue, Project, Viewer};

/// Opens a `symphony.tracker.<op>` span for one Linear API operation — the Rust mirror of
/// tracing.go's `startTrackerSpan`, which resolves a span from the global tracer provider so the
/// external Linear dependency gets request latency + error visibility. `$op` is a string literal
/// (concatenated onto the `symphony.tracker.` prefix at compile time) because the `tracing` crate
/// requires span names to be constant. The read/write operation methods (P3 T4/T5) hold this span
/// for the duration of the call, recording the operation's error onto it before it ends.
#[macro_export]
macro_rules! tracker_span {
    ($op:literal) => {
        ::tracing::info_span!(
            concat!("symphony.tracker.", $op),
            error = ::tracing::field::Empty
        )
    };
}

/// Runs one tracker operation under its `symphony.tracker.<op>` span AND counts every Linear
/// request it sends under `<op>` in the key's request budget (STUDIO-1896) — one name, so the span
/// and the budget's attribution cannot drift apart. Expands to [`client::traced`].
#[macro_export]
macro_rules! traced_op {
    ($op:literal, $fut:expr $(,)?) => {
        $crate::linear::traced($op, $crate::tracker_span!($op), $fut)
    };
}

#[async_trait]
impl crate::Tracker for Client {
    async fn fetch_documents(
        &self,
        issue: Option<&str>,
        query: &str,
        excerpt: bool,
    ) -> Result<crate::Documents, TrackerError> {
        crate::traced_op!(
            "fetch_documents",
            documents::fetch_documents(self, issue, query, excerpt)
        )
        .await
    }
    async fn fetch_issue_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<Option<Issue>, TrackerError> {
        crate::traced_op!(
            "fetch_issue_by_identifier",
            by_ids::fetch_issue_by_identifier(self, identifier)
        )
        .await
    }
    async fn update_issue_description(
        &self,
        id: &str,
        description: &str,
    ) -> Result<(), TrackerError> {
        issue_id::require(id)?;
        crate::traced_op!(
            "update_issue_description",
            by_ids::update_issue_description(self, id, description)
        )
        .await
    }
    async fn fetch_parked_issue(&self, id: &str) -> Result<Option<Issue>, TrackerError> {
        crate::traced_op!(
            "fetch_parked_issue",
            candidates::fetch_parked_issue(self, id)
        )
        .await
    }
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        candidates::fetch_candidate_issues(self).await
    }
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        crate::traced_op!(
            "fetch_issues_by_states",
            by_states::fetch_issues_by_states(self, states)
        )
        .await
    }
    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        by_ids::fetch_issue_states_by_ids(self, ids).await
    }
    async fn fetch_issue_labels_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        by_ids::fetch_issue_labels_by_ids(self, ids).await
    }
    async fn fetch_issue_description_by_identifier(
        &self,
        identifier: &str,
    ) -> Result<Option<String>, TrackerError> {
        by_ids::fetch_issue_description_by_identifier(self, identifier).await
    }
    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        backlog::fetch_blocked_backlog_issues(self).await
    }
    async fn fetch_issue_branch_by_id(&self, id: &str) -> Result<(String, i64), TrackerError> {
        backlog::fetch_issue_branch_by_id(self, id).await
    }
    async fn move_issue_state(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        issue_id::require(issue_id)?;
        move_state::move_issue_state(self, issue_id, team_id, state_name).await
    }
    async fn move_issue_to_type(
        &self,
        issue_id: &str,
        team_id: &str,
        state_type: &str,
    ) -> Result<String, TrackerError> {
        issue_id::require(issue_id)?;
        crate::traced_op!(
            "move_issue_to_type",
            move_state::move_issue_to_type(self, issue_id, team_id, state_type)
        )
        .await
    }
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        crate::traced_op!("resolve_viewer", client::resolve_viewer(self)).await
    }
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        crate::traced_op!("list_projects", projects::list_projects(self)).await
    }
    async fn assign_issue(&self, issue_id: &str, assignee_id: &str) -> Result<(), TrackerError> {
        issue_id::require(issue_id)?;
        claim::assign_issue(self, issue_id, assignee_id).await
    }
    async fn fetch_issue_assignee(&self, issue_id: &str) -> Result<String, TrackerError> {
        claim::fetch_issue_assignee(self, issue_id).await
    }
    async fn create_comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
        issue_id::require(issue_id)?;
        claim::create_comment(self, issue_id, body).await
    }
    async fn list_comments(&self, issue_id: &str) -> Result<Vec<Comment>, TrackerError> {
        claim::list_comments(self, issue_id).await
    }
    async fn delete_comment(&self, comment_id: &str) -> Result<(), TrackerError> {
        claim::delete_comment(self, comment_id).await
    }
    async fn add_issue_label(
        &self,
        issue_id: &str,
        team_id: &str,
        label_name: &str,
    ) -> Result<(), TrackerError> {
        issue_id::require(issue_id)?;
        labels::add_issue_label(self, issue_id, team_id, label_name).await
    }
    async fn remove_issue_label(
        &self,
        issue_id: &str,
        team_id: &str,
        label_name: &str,
    ) -> Result<(), TrackerError> {
        issue_id::require(issue_id)?;
        labels::remove_issue_label(self, issue_id, team_id, label_name).await
    }
    async fn fetch_open_issues_by_labels(
        &self,
        label_names: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        labels::fetch_open_issues_by_labels(self, label_names).await
    }
    async fn create_issue(&self, spec: &NewIssue) -> Result<String, TrackerError> {
        create::create_issue(self, spec).await
    }
    async fn link_pull_request(&self, issue_id: &str, url: &str) -> Result<(), TrackerError> {
        issue_id::require(issue_id)?;
        attach::link_pull_request(self, issue_id, url).await
    }
    fn request_budget(&self) -> Option<crate::budget::BudgetStatus> {
        Some(self.request_ledger().status(std::time::Instant::now()))
    }
}

#[cfg(test)]
mod tracing_tests {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    /// Records the name of every span created under it — the Rust analogue of Go's
    /// `tracetest.SpanRecorder` (tracing_test.go).
    #[derive(Clone, Default)]
    struct RecordingLayer {
        names: Arc<Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> Layer<S> for RecordingLayer {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.names
                .lock()
                .expect("recorder mutex")
                .push(attrs.metadata().name().to_string());
        }
    }

    // Mirrors Go TestTrackerOpSpans: a tracker operation runs under a `symphony.tracker.<op>` span.
    // Go drives this through FetchIssueStatesByIDs; that read method lands in P3 T4, so T3 exercises
    // the span primitive (`tracker_span!`) directly with the same op name Go's test asserts.
    #[test]
    fn tracker_span_names_the_operation() {
        let names = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(RecordingLayer {
            names: Arc::clone(&names),
        });
        tracing::subscriber::with_default(subscriber, || {
            let _span = crate::tracker_span!("fetch_issue_states");
        });
        let recorded = names.lock().expect("recorder mutex");
        assert!(
            recorded
                .iter()
                .any(|n| n == "symphony.tracker.fetch_issue_states"),
            "expected a symphony.tracker.fetch_issue_states span; got {recorded:?}"
        );
    }
}

/// STUDIO-1896: every Linear request is attributed to the tracker operation that sent it, so a
/// per-caller count adds up to the total the server saw, and a 429 blocks the whole API key until
/// Linear's reset rather than letting every caller hammer it again.
#[cfg(test)]
mod budget_tests {
    use super::testutil::{MockResp, MockServer, TEST_VIEWER_RESP};
    use super::{Config, LinearErrorKind, new};
    use crate::{NewIssue, Tracker, TrackerError};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    const ID: &str = "11111111-2222-3333-4444-555555555555";

    fn client(url: String, key: &str) -> super::Client {
        new(Config {
            endpoint: url,
            api_key: key.into(),
            project_slug: "proj".into(),
            ..Config::default()
        })
    }

    #[tokio::test]
    async fn every_tracker_method_is_attributed_and_the_counts_add_up() {
        let served = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&served);
        let server = MockServer::start(move |req| {
            seen.fetch_add(1, Ordering::SeqCst);
            if req.query.contains("viewer {") {
                MockResp::ok(TEST_VIEWER_RESP)
            } else {
                // Most decoders reject an empty payload; the request still went out, which is
                // all this test counts.
                MockResp::ok(r#"{"data":{}}"#)
            }
        })
        .await;
        let c = client(server.url(), "budget-attribution-key");
        let ids = vec![ID.to_string()];
        let names = vec!["Todo".to_string()];
        let spec = NewIssue {
            team_id: "team".into(),
            title: "t".into(),
            state_name: "Todo".into(),
            labels: vec!["l".into()],
            ..NewIssue::default()
        };
        let _ = c.fetch_documents(Some(ID), "q", false).await;
        let _ = c.fetch_issue_by_identifier("ABC-1").await;
        let _ = c.update_issue_description(ID, "d").await;
        let _ = c.fetch_candidate_issues().await;
        let _ = c.fetch_parked_issue(ID).await;
        let _ = c.fetch_issues_by_states(&names).await;
        let _ = c.fetch_issue_states_by_ids(&ids).await;
        let _ = c.fetch_issue_labels_by_ids(&ids).await;
        let _ = c.fetch_issue_description_by_identifier("ABC-1").await;
        let _ = c.fetch_blocked_backlog_issues().await;
        let _ = c.fetch_issue_branch_by_id(ID).await;
        let _ = c.move_issue_state(ID, "team", "Todo").await;
        let _ = c.move_issue_to_type(ID, "team", "backlog").await;
        let _ = c.resolve_viewer().await;
        let _ = c.list_projects().await;
        let _ = c.assign_issue(ID, "user").await;
        let _ = c.fetch_issue_assignee(ID).await;
        let _ = c.create_comment(ID, "b").await;
        let _ = c.list_comments(ID).await;
        let _ = c.delete_comment(ID).await;
        let _ = c.add_issue_label(ID, "team", "l").await;
        let _ = c.remove_issue_label(ID, "team", "l").await;
        let _ = c.fetch_open_issues_by_labels(&names).await;
        let _ = c.create_issue(&spec).await;
        let _ = c
            .link_pull_request(ID, "https://github.com/o/r/pull/1")
            .await;

        let per_op = c.request_ledger().per_op(Instant::now());
        let total: u32 = per_op.values().sum();
        // The same breakdown the tight warning logs, so `--nocapture` shows the attribution.
        eprintln!(
            "per-caller: total={total} {}",
            crate::budget::format_breakdown(&per_op)
        );
        assert!(
            !per_op.contains_key(crate::budget::UNTAGGED),
            "a request went out with no caller: {per_op:?}"
        );
        assert_eq!(
            total,
            served.load(Ordering::SeqCst),
            "per-caller counts must add up to what the server saw: {per_op:?}"
        );
        assert!(
            total >= 20,
            "most methods should have reached the server: {per_op:?}"
        );
    }

    #[tokio::test]
    async fn a_429_blocks_the_key_until_linears_reset_and_sends_nothing_meanwhile() {
        let served = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&served);
        let server = MockServer::start(move |_req| {
            seen.fetch_add(1, Ordering::SeqCst);
            MockResp::status(429, r#"{"errors":[{"message":"rate limited"}]}"#)
                .header("Retry-After", "120")
                .header("X-RateLimit-Requests-Limit", "2500")
                .header("X-RateLimit-Requests-Remaining", "0")
        })
        .await;
        let key = "budget-429-key";
        let c = client(server.url(), key);
        let ids = vec![ID.to_string()];

        let err = c
            .fetch_issue_labels_by_ids(&ids)
            .await
            .expect_err("a 429 is an error");
        assert!(err.is_rate_limited(), "got {err:?}");
        assert!(
            matches!(&err, TrackerError::Linear(e) if e.kind == LinearErrorKind::RateLimited),
            "got {err:?}"
        );
        assert_eq!(served.load(Ordering::SeqCst), 1);

        // A second client on the SAME key (another project) is blocked too, without a request.
        let sibling = client(server.url(), key);
        let err = sibling
            .fetch_issue_states_by_ids(&ids)
            .await
            .expect_err("the key is blocked");
        assert!(err.is_rate_limited(), "got {err:?}");
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "nothing sent while blocked"
        );

        let status = sibling.request_budget().expect("linear reports a budget");
        assert!(status.tight, "a blocked key is a tight budget: {status:?}");
        let wait = status.blocked_for.expect("blocked");
        assert!(
            wait.as_secs() > 100 && wait.as_secs() <= 120,
            "Retry-After is honored: {wait:?}"
        );
    }

    #[tokio::test]
    async fn a_ratelimited_graphql_error_on_a_200_blocks_the_key_too() {
        let server = MockServer::start(|_req| {
            MockResp::ok(
                r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#,
            )
        })
        .await;
        let c = client(server.url(), "budget-ratelimited-200-key");
        let err = c
            .fetch_issue_states_by_ids(&[ID.to_string()])
            .await
            .expect_err("rate limited");
        assert!(err.is_rate_limited(), "got {err:?}");
        assert!(c.request_ledger().blocked_for(Instant::now()).is_some());
    }
}
