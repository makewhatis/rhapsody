//! Linear GraphQL tracker adapter — port of Go `internal/tracker/linear`.
//!
//! P3 T3 ships the foundation ported from `{client,query,errors,tracing,normalize}.go`: the
//! GraphQL transport ([`client`]), the byte-identical query builder ([`query`]), the typed error
//! sentinels ([`errors`]), and issue normalization ([`normalize`]). T4 fills in the read path
//! (candidates, by-states, by-ids, blocked-backlog + branch, projects) and T5 the write path
//! (state moves in [`move_state`], the pool-mode claim assign/comment surface in [`claim`]) — every
//! [`Tracker`](crate::Tracker) method now has a real body.

mod backlog;
mod by_ids;
mod by_states;
mod candidates;
mod claim;
mod client;
mod create;
mod decode;
mod errors;
mod labels;
mod move_state;
mod normalize;
mod projects;
pub mod query;
#[cfg(test)]
mod testutil;

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

#[async_trait]
impl crate::Tracker for Client {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        candidates::fetch_candidate_issues(self).await
    }
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        by_states::fetch_issues_by_states(self, states).await
    }
    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        by_ids::fetch_issue_states_by_ids(self, ids).await
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
        move_state::move_issue_state(self, issue_id, team_id, state_name).await
    }
    async fn move_issue_to_type(
        &self,
        issue_id: &str,
        team_id: &str,
        state_type: &str,
    ) -> Result<String, TrackerError> {
        move_state::move_issue_to_type(self, issue_id, team_id, state_type).await
    }
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        client::resolve_viewer(self).await
    }
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        projects::list_projects(self).await
    }
    async fn assign_issue(&self, issue_id: &str, assignee_id: &str) -> Result<(), TrackerError> {
        claim::assign_issue(self, issue_id, assignee_id).await
    }
    async fn fetch_issue_assignee(&self, issue_id: &str) -> Result<String, TrackerError> {
        claim::fetch_issue_assignee(self, issue_id).await
    }
    async fn create_comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
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
        labels::add_issue_label(self, issue_id, team_id, label_name).await
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
