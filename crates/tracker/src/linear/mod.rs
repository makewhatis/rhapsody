//! Linear GraphQL tracker adapter — port of Go `internal/tracker/linear`.
//!
//! P3 T3 ships the foundation ported from `{client,query,errors,tracing,normalize}.go`: the
//! GraphQL transport ([`client`]), the byte-identical query builder ([`query`]), the typed error
//! sentinels ([`errors`]), and issue normalization ([`normalize`]). The read (T4) and write (T5)
//! paths fill in the per-operation [`Tracker`](crate::Tracker) methods, which until then report a
//! not-yet-implemented [`TrackerError`].

mod client;
mod errors;
mod normalize;
pub mod query;

pub use client::{Client, Config, new};
pub use errors::{LinearError, LinearErrorKind};
pub use normalize::RawIssue;

use crate::TrackerError;
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
        ::tracing::info_span!(concat!("symphony.tracker.", $op))
    };
}

#[async_trait]
impl crate::Tracker for Client {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issues_by_states(&self, _states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_states_by_ids(&self, _ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_branch_by_id(&self, _id: &str) -> Result<(String, i64), TrackerError> {
        Err(self.not_implemented())
    }
    async fn move_issue_state(
        &self,
        _issue_id: &str,
        _team_id: &str,
        _state_name: &str,
    ) -> Result<(), TrackerError> {
        Err(self.not_implemented())
    }
    async fn move_issue_to_type(
        &self,
        _issue_id: &str,
        _team_id: &str,
        _state_type: &str,
    ) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        Err(self.not_implemented())
    }
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn assign_issue(&self, _issue_id: &str, _assignee_id: &str) -> Result<(), TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_assignee(&self, _issue_id: &str) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn create_comment(&self, _issue_id: &str, _body: &str) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn list_comments(&self, _issue_id: &str) -> Result<Vec<Comment>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn delete_comment(&self, _comment_id: &str) -> Result<(), TrackerError> {
        Err(self.not_implemented())
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
