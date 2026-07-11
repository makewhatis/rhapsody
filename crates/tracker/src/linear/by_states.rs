//! Fetch-by-states — parity port of `internal/tracker/linear/by_states.go` (upstream §11.1).
//!
//! [`fetch_issues_by_states`] returns the configured project's issues in the given states, reusing
//! the shared [`paginate`](super::candidates::paginate) loop. Empty states short-circuits with no
//! API call.

use super::{Client, query};
use crate::TrackerError;
use rhapsody_core::Issue;

/// FetchIssuesByStates returns issues for the configured project in the given states. Empty states
/// returns an empty result with no API call (by_states.go's `FetchIssuesByStates`).
pub(super) async fn fetch_issues_by_states(
    c: &Client,
    states: &[String],
) -> Result<Vec<Issue>, TrackerError> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    super::candidates::paginate(c, query::QUERY_BY_STATES, states, &[]).await
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, new_test_client};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Mirrors Go TestFetchByStatesEmptyMakesNoCall.
    #[tokio::test]
    async fn fetch_by_states_empty_makes_no_call() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let got = c.fetch_issues_by_states(&[]).await.expect("empty states");
        assert!(got.is_empty(), "expected empty result");
        assert!(
            !called.load(Ordering::SeqCst),
            "no API call should be made for empty states"
        );
    }

    // Mirrors Go TestFetchByStatesReturnsIssues.
    #[tokio::test]
    async fn fetch_by_states_returns_issues() {
        let saw_states = Arc::new(AtomicBool::new(false));
        let saw_states_h = Arc::clone(&saw_states);
        let (c, _server) = new_test_client(move |req| {
            if req.var("states").is_some() {
                saw_states_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[{"id":"9","identifier":"MT-9","title":"t","state":{"name":"Done"}}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
            )
        })
        .await;
        let got = c
            .fetch_issues_by_states(&["Done".into(), "Cancelled".into()])
            .await
            .expect("by states");
        assert!(saw_states.load(Ordering::SeqCst), "states var must be sent");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].identifier, "MT-9");
        assert_eq!(got[0].state, "Done");
    }
}
