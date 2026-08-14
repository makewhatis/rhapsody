//! Fetch-by-ids — parity port of `internal/tracker/linear/by_ids.go` (upstream §11.1, §11.2).
//!
//! [`fetch_issue_states_by_ids`] returns minimal normalized issues (id, identifier, title, state)
//! for the given tracker IDs — the reconciliation read, whose staleness contract other callers
//! depend on. Empty IDs short-circuits with no API call; the running set is bounded by concurrency
//! so no pagination is needed.

use super::decode::IssueNodes;
use super::{Client, query};
use crate::TrackerError;
use rhapsody_core::Issue;
use serde::Deserialize;

/// The `issues` connection shape for the by-ids / branch-by-id queries (no pagination).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct IdsPage {
    pub issues: IdsConnection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct IdsConnection {
    /// Lenient per-node decode (STUDIO-406) — one undecodable issue must not blank a whole
    /// reconciliation read.
    pub nodes: IssueNodes,
}

/// FetchIssueStatesByIDs returns minimal normalized issues for the given tracker IDs. Empty IDs
/// returns an empty result with no API call (by_ids.go's `FetchIssueStatesByIDs`).
pub(super) async fn fetch_issue_states_by_ids(
    c: &Client,
    ids: &[String],
) -> Result<Vec<Issue>, TrackerError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    super::client::traced(crate::tracker_span!("fetch_issue_states"), async move {
        let vars = serde_json::json!({ "ids": ids, "first": ids.len() });
        let page: IdsPage = c.do_graphql(query::QUERY_BY_IDS, Some(vars)).await?;
        page.issues.nodes.warn_dropped("fetch issue states by id");
        Ok(page
            .issues
            .nodes
            .kept
            .into_iter()
            .map(|n| c.normalize_issue(n))
            .collect())
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, new_test_client};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // Mirrors Go TestFetchByIDsEmptyMakesNoCall.
    #[tokio::test]
    async fn fetch_by_ids_empty_makes_no_call() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{"issues":{"nodes":[]}}}"#)
        })
        .await;
        let got = c.fetch_issue_states_by_ids(&[]).await.expect("empty ids");
        assert!(got.is_empty(), "empty ids should short-circuit");
        assert!(
            !called.load(Ordering::SeqCst),
            "empty ids should make no API call"
        );
    }

    // Mirrors Go TestFetchByIDsUsesIDListAndNormalizes.
    #[tokio::test]
    async fn fetch_by_ids_uses_id_list_and_normalizes() {
        let seen = Arc::new(Mutex::new(Option::<(bool, Vec<String>)>::None));
        let seen_h = Arc::clone(&seen);
        let (c, _server) = new_test_client(move |req| {
            let has_id_type = req.query.contains("[ID!]");
            let ids: Vec<String> = req
                .var("ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            *seen_h.lock().expect("seen") = Some((has_id_type, ids));
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"a","identifier":"MT-1","title":"t1","state":{"name":"Done"}},
                    {"id":"b","identifier":"MT-2","title":"t2","state":{"name":"In Progress"}}
                ]}}}"#,
            )
        })
        .await;
        let got = c
            .fetch_issue_states_by_ids(&["a".into(), "b".into()])
            .await
            .expect("by ids");
        let seen = seen.lock().expect("seen");
        let (has_id_type, ids) = seen.as_ref().expect("request seen");
        assert!(has_id_type, "query must declare ids as [ID!]");
        assert_eq!(ids, &["a".to_string(), "b".to_string()], "ids var");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[0].state, "Done");
        assert_eq!(got[1].state, "In Progress");
    }
}
