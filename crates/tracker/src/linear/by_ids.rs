//! Fetch-by-ids — parity port of `internal/tracker/linear/by_ids.go` (upstream §11.1, §11.2).
//!
//! [`fetch_issue_states_by_ids`] returns minimal normalized issues (id, identifier, title, state)
//! for the given tracker IDs — the reconciliation read, whose staleness contract other callers
//! depend on. Empty IDs short-circuits with no API call; the running set is bounded by concurrency
//! so no pagination is needed.
//!
//! [`fetch_issue_labels_by_ids`] is its Rhapsody-only sibling (STUDIO-735): the same id filter, but
//! asking for LABELS instead of the state — a separate round trip precisely so the every-tick
//! reconciliation read keeps costing exactly what it costs today.

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

/// The LABELS of `ids`, whatever state those issues are in — the read behind the console's durable
/// per-ticket assignee (STUDIO-735). Only `id`, `identifier` and `labels` are populated; every
/// other field of the returned [`Issue`] is its default.
///
/// Empty IDs short-circuits with no API call, exactly like [`fetch_issue_states_by_ids`], and the
/// same unpaginated `first: len(ids)` bound applies — the caller batches.
pub(super) async fn fetch_issue_labels_by_ids(
    c: &Client,
    ids: &[String],
) -> Result<Vec<Issue>, TrackerError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    super::client::traced(crate::tracker_span!("fetch_issue_labels"), async move {
        let vars = serde_json::json!({ "ids": ids, "first": ids.len() });
        let page: IdsPage = c
            .do_graphql(query::QUERY_ISSUE_LABELS_BY_IDS, Some(vars))
            .await?;
        page.issues.nodes.warn_dropped("fetch issue labels by id");
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

/// The `issue(id:)` envelope for the by-identifier description read (STUDIO-1034). `issue` is
/// `Option` because Linear answers a single-object query with `null` for an identifier that
/// matches nothing — the same "there is no ticket" the absent-node case meant before.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueByIdentifierPage {
    issue: Option<IssueByIdentifierNode>,
}

/// One `issue(id:)` node. `identifier` tolerates a JSON `null` like every other string in this
/// adapter ([`super::decode::null_to_empty`]); `description` is `Option` because Linear declares it
/// nullable.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueByIdentifierNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    identifier: String,
    description: Option<String>,
}

/// The DESCRIPTION of the issue with the given human identifier (e.g. `STUDIO-1034`), or `None`
/// when no such issue is readable (STUDIO-1034).
///
/// A single-purpose read, Rhapsody-only: a ticketless review has no Linear access, so the daemon
/// reads the origin ticket's acceptance criteria here, off the control task, and quotes them into
/// the review prompt. It resolves through `issue(id:)` — Linear's `IssueFilter` has no `identifier`
/// field, so the filter form is rejected at validation (see
/// [`query::QUERY_ISSUE_DESCRIPTION_BY_IDENTIFIER`]) — and still compares the returned
/// `identifier` case-insensitively, since Linear stores it uppercase while a caller naming a ticket
/// from prose should not have to match its casing. An empty or whitespace identifier returns `None`
/// with no API call, mirroring the other by-id reads' empty shortcut. An empty or whitespace
/// DESCRIPTION is `None` too: "the ticket says nothing" and "there is no ticket" reach the prompt as
/// the same honest statement.
pub(super) async fn fetch_issue_description_by_identifier(
    c: &Client,
    identifier: &str,
) -> Result<Option<String>, TrackerError> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return Ok(None);
    }
    super::client::traced(
        crate::tracker_span!("fetch_issue_description"),
        async move {
            let vars = serde_json::json!({ "id": identifier });
            let page: IssueByIdentifierPage = c
                .do_graphql(query::QUERY_ISSUE_DESCRIPTION_BY_IDENTIFIER, Some(vars))
                .await?;
            Ok(page
                .issue
                .filter(|n| n.identifier.eq_ignore_ascii_case(identifier))
                .and_then(|n| n.description)
                .filter(|d| !d.trim().is_empty()))
        },
    )
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

    // STUDIO-735: the labels read is the SAME id filter with a different selection set, and the
    // labels it returns are normalized (lowercased) exactly as every other issue read normalizes
    // them — the console's assignee is matched against a lowercase `rhapsody:@` prefix.
    #[tokio::test]
    async fn fetch_labels_by_ids_returns_normalized_labels() {
        let seen = Arc::new(Mutex::new(Option::<String>::None));
        let seen_h = Arc::clone(&seen);
        let (c, _server) = new_test_client(move |req| {
            *seen_h.lock().expect("seen") = Some(req.query.clone());
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"a","identifier":"MT-1","labels":{"nodes":[{"name":"Rhapsody:@Alice"}]}},
                    {"id":"b","identifier":"MT-2","labels":{"nodes":[]}}
                ]}}}"#,
            )
        })
        .await;

        let got = c
            .fetch_issue_labels_by_ids(&["a".into(), "b".into()])
            .await
            .expect("labels by ids");

        let query = seen.lock().expect("seen").clone().expect("request seen");
        assert!(query.contains("[ID!]"), "query must declare ids as [ID!]");
        assert!(
            query.contains("labels { nodes { name } }"),
            "the labels selection is the whole point of this read: {query}"
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].labels.as_deref(),
            Some(&["rhapsody:@alice".to_string()][..])
        );
        assert_eq!(got[1].labels, None, "an unlabelled issue carries no labels");
    }

    #[tokio::test]
    async fn fetch_labels_by_ids_empty_makes_no_call() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{"issues":{"nodes":[]}}}"#)
        })
        .await;

        let got = c.fetch_issue_labels_by_ids(&[]).await.expect("empty ids");

        assert!(got.is_empty(), "empty ids should short-circuit");
        assert!(
            !called.load(Ordering::SeqCst),
            "empty ids should make no API call"
        );
    }

    // ── STUDIO-1034: description by identifier ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_description_by_identifier_returns_the_body() {
        let seen = Arc::new(Mutex::new(Option::<(String, String)>::None));
        let seen_h = Arc::clone(&seen);
        let (c, _server) = new_test_client(move |req| {
            let id = req.var_str("id").unwrap_or_default().to_string();
            *seen_h.lock().expect("seen") = Some((req.query.clone(), id));
            MockResp::ok(
                r#"{"data":{"issue":{"identifier":"STUDIO-1034","description":"the acceptance text"}}}"#,
            )
        })
        .await;

        // Lower-case input must still match: the identifier is compared case-insensitively.
        let got = c
            .fetch_issue_description_by_identifier("studio-1034")
            .await
            .expect("description");

        let (query, id) = seen.lock().expect("seen").clone().expect("request seen");
        // The query resolves by `issue(id:)`. Linear's `IssueFilter` has NO `identifier` field, so
        // the old `issues(filter: { identifier: { eq: … } })` form was rejected at validation and
        // every live read degraded to "no ticket available" (the whole feature never lit up).
        assert!(
            query.contains("issue(id: $id)"),
            "the query must resolve by issue(id:), not an identifier filter: {query}"
        );
        assert!(
            query.contains("identifier description"),
            "the query must ask for the two fields the read uses: {query}"
        );
        assert!(
            query.contains("$id: String!"),
            "the query must type its argument as String!: {query}"
        );
        assert!(
            !query.contains("filter:"),
            "no filter is used — IssueFilter has no identifier field: {query}"
        );
        assert_eq!(id, "studio-1034", "the id variable is the raw identifier");
        assert_eq!(got.as_deref(), Some("the acceptance text"));
    }

    #[tokio::test]
    async fn fetch_description_by_identifier_empty_or_missing_is_none() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |req| {
            called_h.store(true, Ordering::SeqCst);
            // A null description, an empty one, a null issue (nothing matched), and a node whose
            // identifier is NOT what was asked for are all "no ticket".
            match req.var_str("id").unwrap_or_default() {
                "STUDIO-1" => MockResp::ok(
                    r#"{"data":{"issue":{"identifier":"STUDIO-1","description":null}}}"#,
                ),
                "STUDIO-2" => MockResp::ok(
                    r#"{"data":{"issue":{"identifier":"STUDIO-2","description":"   "}}}"#,
                ),
                "STUDIO-3" => {
                    MockResp::ok(r#"{"data":{"issue":{"identifier":"OTHER-3","description":"x"}}}"#)
                }
                _ => MockResp::ok(r#"{"data":{"issue":null}}"#),
            }
        })
        .await;

        assert_eq!(
            c.fetch_issue_description_by_identifier("STUDIO-1")
                .await
                .expect("null"),
            None
        );
        assert_eq!(
            c.fetch_issue_description_by_identifier("STUDIO-2")
                .await
                .expect("blank"),
            None
        );
        assert_eq!(
            c.fetch_issue_description_by_identifier("STUDIO-3")
                .await
                .expect("wrong identifier"),
            None,
            "a resolved issue that is not the one asked for is not the ticket"
        );
        assert_eq!(
            c.fetch_issue_description_by_identifier("STUDIO-404")
                .await
                .expect("absent"),
            None
        );
        assert!(
            called.load(Ordering::SeqCst),
            "a non-empty identifier must reach the API"
        );
    }

    #[tokio::test]
    async fn fetch_description_by_identifier_empty_identifier_makes_no_call() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{"issues":{"nodes":[]}}}"#)
        })
        .await;

        let got = c
            .fetch_issue_description_by_identifier("  ")
            .await
            .expect("blank identifier");

        assert_eq!(got, None);
        assert!(
            !called.load(Ordering::SeqCst),
            "a blank identifier should make no API call"
        );
    }
}
