//! Candidate fetch + the shared pagination/milestone helpers — parity port of
//! `internal/tracker/linear/candidates.go` (upstream §11.1, §11.2).
//!
//! [`fetch_candidate_issues`] returns active ∪ review-state issues for the configured project,
//! narrowed by the claim-mode assignee switch (assignee == viewer, or UNASSIGNED in pool mode) and
//! an optional milestone. [`paginate`] is the shared project+states pagination loop (also driven by
//! by-states); [`resolve_milestone_id`] resolves + caches a configured milestone name/UUID to its
//! Linear id.

use super::{Client, LinearError, LinearErrorKind, RawIssue, query};
use crate::TrackerError;
use regex::Regex;
use rhapsody_core::Issue;
use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;

/// Caps pagination so a server returning `hasNextPage: true` with a repeating cursor cannot loop
/// forever (the context/timeout is the only other backstop). Mirrors candidates.go's `maxPages`.
pub(super) const MAX_PAGES: u32 = 2000;

/// Matches a canonical UUID (8-4-4-4-12 hex). A configured milestone value that matches is treated
/// as a milestone ID and used verbatim; anything else is a milestone NAME resolved via the API.
/// `None` only if the constant pattern failed to compile (impossible) — then no value is a UUID, so
/// every milestone is treated as a name (graceful, never a panic — the `core::compile_summon_re`
/// decision).
static UUID_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").ok()
});

/// The `issues` connection shape returned by the candidate/by-states/backlog queries.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct IssuesPage {
    pub issues: IssuesConnection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct IssuesConnection {
    pub nodes: Vec<RawIssue>,
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
}

/// A GraphQL `pageInfo` (shared by every paginated connection in this adapter).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: String,
}

/// The `projectMilestones` connection shape returned by [`query::QUERY_PROJECT_MILESTONES`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MilestonesPage {
    #[serde(rename = "projectMilestones")]
    project_milestones: MilestonesConnection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MilestonesConnection {
    nodes: Vec<MilestoneNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MilestoneNode {
    id: String,
    name: String,
}

/// FetchCandidateIssues returns candidate issues for the configured project, following pagination
/// (candidates.go's `FetchCandidateIssues`). The state filter is active ∪ review states. The
/// assignee clause depends on claim mode: assignee mode narrows to the API key owner's assigned
/// issues (viewer resolved + cached first; a resolution failure fails the whole fetch), pool mode
/// (INF-477) narrows to UNASSIGNED issues and needs no viewer here.
pub(super) async fn fetch_candidate_issues(c: &Client) -> Result<Vec<Issue>, TrackerError> {
    super::client::traced(crate::tracker_span!("fetch_candidates"), async move {
        let pool = c.pool_mode();
        let mut extra: Vec<(&str, Value)> = Vec::new();
        if !pool {
            let v = super::client::resolve_viewer(c).await?;
            extra.push(("assigneeID", Value::String(v.id)));
        }
        let states = candidate_states(c);
        if c.config.milestone.is_empty() {
            return paginate(c, &query::query_candidates(false, pool), &states, &extra).await;
        }
        let id = resolve_milestone_id(c).await?;
        extra.push(("milestoneID", Value::String(id)));
        paginate(c, &query::query_candidates(true, pool), &states, &extra).await
    })
    .await
}

/// candidateStates returns active ∪ review states — review states appended after the active ones,
/// dedup'd by exact name. With no review states it returns the active slice unchanged, preserving
/// the ordering the live API was verified against (candidates.go's `candidateStates`).
fn candidate_states(c: &Client) -> Vec<String> {
    if c.config.review_states.is_empty() {
        return c.config.active_states.clone();
    }
    let mut seen = std::collections::HashSet::with_capacity(
        c.config.active_states.len() + c.config.review_states.len(),
    );
    let mut out = Vec::with_capacity(c.config.active_states.len() + c.config.review_states.len());
    for s in c
        .config
        .active_states
        .iter()
        .chain(c.config.review_states.iter())
    {
        if seen.insert(s.clone()) {
            out.push(s.clone());
        }
    }
    out
}

/// paginate runs a project+states query across all pages, preserving order. `extra` variables (the
/// resolved assignee/milestone for the candidate query) are merged into each page's variables
/// (candidates.go's `paginate`).
pub(super) async fn paginate(
    c: &Client,
    query: &str,
    states: &[String],
    extra: &[(&str, Value)],
) -> Result<Vec<Issue>, TrackerError> {
    let mut out: Vec<Issue> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        // Defensive guard against an endless pagination loop.
        pages += 1;
        if pages > MAX_PAGES {
            return Err(LinearError::new(
                LinearErrorKind::MissingCursor,
                format!("exceeded {MAX_PAGES} pages without completing pagination"),
            )
            .into());
        }
        let mut vars = serde_json::Map::new();
        vars.insert(
            "projectSlug".into(),
            Value::from(c.config.project_slug.clone()),
        );
        vars.insert("states".into(), Value::from(states.to_vec()));
        vars.insert("first".into(), Value::from(c.page_size));
        vars.insert("after".into(), Value::from(after.clone()));
        for (k, v) in extra {
            vars.insert((*k).to_string(), v.clone());
        }
        let page: IssuesPage = c.do_graphql(query, Some(Value::Object(vars))).await?;
        for n in page.issues.nodes {
            out.push(c.normalize_issue(n));
        }
        if !page.issues.page_info.has_next_page {
            return Ok(out);
        }
        if page.issues.page_info.end_cursor.is_empty() {
            return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
        }
        after = Some(page.issues.page_info.end_cursor);
    }
}

/// resolveMilestoneID resolves the configured milestone (name or UUID) to its Linear ID, caching
/// the result for the client's lifetime (candidates.go's `resolveMilestoneID`). A UUID is returned
/// directly (no API call); a name is matched case-insensitively against the project's milestones;
/// a value that matches none returns [`MilestoneNotFound`](LinearErrorKind::MilestoneNotFound).
pub(super) async fn resolve_milestone_id(c: &Client) -> Result<String, TrackerError> {
    // Hold the milestone-cache lock across the resolution (single-flight), the async mirror of Go
    // holding `milestoneMu` across the network call.
    let mut cached = c.milestone_id.lock().await;
    if !cached.is_empty() {
        return Ok(cached.clone());
    }
    if UUID_RE
        .as_ref()
        .is_some_and(|re| re.is_match(&c.config.milestone))
    {
        *cached = c.config.milestone.clone();
        return Ok(cached.clone());
    }
    let want = rhapsody_core::normalize_state(&c.config.milestone);
    let mut after: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        pages += 1;
        if pages > MAX_PAGES {
            return Err(LinearError::new(
                LinearErrorKind::MissingCursor,
                format!("exceeded {MAX_PAGES} milestone pages"),
            )
            .into());
        }
        let vars = serde_json::json!({
            "projectSlug": c.config.project_slug,
            "first": c.page_size,
            "after": after,
        });
        let page: MilestonesPage = c
            .do_graphql(query::QUERY_PROJECT_MILESTONES, Some(vars))
            .await?;
        for m in &page.project_milestones.nodes {
            if rhapsody_core::normalize_state(&m.name) == want {
                *cached = m.id.clone();
                return Ok(cached.clone());
            }
        }
        if !page.project_milestones.page_info.has_next_page {
            return Err(LinearError::new(
                LinearErrorKind::MilestoneNotFound,
                format!(
                    "{:?} in project {:?}",
                    c.config.milestone, c.config.project_slug
                ),
            )
            .into());
        }
        if page.project_milestones.page_info.end_cursor.is_empty() {
            return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
        }
        after = Some(page.project_milestones.page_info.end_cursor.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{candidate_states, resolve_milestone_id};
    use crate::Tracker;
    use crate::TrackerError;
    use crate::linear::testutil::{MockResp, MockServer, TEST_VIEWER_RESP, new_test_client};
    use crate::linear::{Config, LinearErrorKind, new, query};
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn is_kind(err: &TrackerError, kind: LinearErrorKind) -> bool {
        matches!(err, TrackerError::Linear(e) if e.kind == kind)
    }

    // ─── candidateStates: active ∪ review dedup contract ─────────────────────────────────────────

    #[test]
    fn candidate_states_active_only_when_no_review() {
        let c = new(Config {
            active_states: vec!["Todo".into(), "In Progress".into()],
            ..Config::default()
        });
        assert_eq!(candidate_states(&c), vec!["Todo", "In Progress"]);
    }

    #[test]
    fn candidate_states_unions_and_dedups_review() {
        // review states appended after active, dedup'd by exact name ("Todo" already present).
        let c = new(Config {
            active_states: vec!["Todo".into(), "In Progress".into()],
            review_states: vec!["In Review".into(), "Todo".into()],
            ..Config::default()
        });
        assert_eq!(
            candidate_states(&c),
            vec!["Todo", "In Progress", "In Review"]
        );
    }

    // ─── resolveMilestoneID (candidates_test.go) ─────────────────────────────────────────────────

    // Mirrors Go TestResolveMilestoneByName.
    #[tokio::test]
    async fn resolve_milestone_by_name() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = Arc::clone(&calls);
        let server = MockServer::start(move |_req| {
            calls_h.fetch_add(1, Ordering::SeqCst);
            MockResp::ok(
                r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-1","name":"V1.0"},{"id":"ms-2","name":"v2.0"}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "V2.0".into(),
            ..Config::default()
        });
        let id = resolve_milestone_id(&c).await.expect("resolve");
        assert_eq!(id, "ms-2", "case-insensitive match");
        // Second call is cached: no extra HTTP request.
        resolve_milestone_id(&c).await.expect("resolve cached");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected 1 milestones query (cached)"
        );
    }

    // Mirrors Go TestResolveMilestoneByUUIDSkipsLookup.
    #[tokio::test]
    async fn resolve_milestone_by_uuid_skips_lookup() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let server = MockServer::start(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "123e4567-e89b-12d3-a456-426614174000".into(),
            ..Config::default()
        });
        let id = resolve_milestone_id(&c).await.expect("uuid passthrough");
        assert_eq!(id, "123e4567-e89b-12d3-a456-426614174000");
        assert!(
            !called.load(Ordering::SeqCst),
            "UUID milestone must not trigger a lookup request"
        );
    }

    // Mirrors Go TestResolveMilestoneNotFound.
    #[tokio::test]
    async fn resolve_milestone_not_found() {
        let server = MockServer::start(|_req| {
            MockResp::ok(
                r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-1","name":"other"}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "nope".into(),
            ..Config::default()
        });
        let err = resolve_milestone_id(&c).await.expect_err("not found");
        assert!(
            is_kind(&err, LinearErrorKind::MilestoneNotFound),
            "got {err:?}, want MilestoneNotFound"
        );
    }

    // Mirrors Go TestResolveMilestoneByNamePaginates.
    #[tokio::test]
    async fn resolve_milestone_by_name_paginates() {
        let afters = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let afters_h = Arc::clone(&afters);
        let page = Arc::new(AtomicUsize::new(0));
        let page_h = Arc::clone(&page);
        let server = MockServer::start(move |req| {
            afters_h
                .lock()
                .expect("afters")
                .push(req.var_str("after").map(String::from));
            // Target milestone is NOT on page 1 -> resolver must follow the cursor.
            if page_h.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResp::ok(
                    r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-1","name":"v1.0"}],"pageInfo":{"hasNextPage":true,"endCursor":"M1"}}}}"#,
                )
            } else {
                MockResp::ok(
                    r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-2","name":"v2.0"}],"pageInfo":{"hasNextPage":false,"endCursor":"M2"}}}}"#,
                )
            }
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "v2.0".into(),
            ..Config::default()
        });
        let id = resolve_milestone_id(&c).await.expect("resolve");
        assert_eq!(id, "ms-2", "found on page 2");
        let afters = afters.lock().expect("afters");
        assert_eq!(afters.len(), 2, "expected 2 milestone pages");
        assert_eq!(afters[0], None, "first page after should be nil");
        assert_eq!(
            afters[1].as_deref(),
            Some("M1"),
            "second page after should advance to M1"
        );
    }

    // Mirrors Go TestResolveMilestoneMissingEndCursor.
    #[tokio::test]
    async fn resolve_milestone_missing_end_cursor() {
        let server = MockServer::start(|_req| {
            MockResp::ok(
                r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-1","name":"other"}],"pageInfo":{"hasNextPage":true,"endCursor":""}}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "v2.0".into(),
            ..Config::default()
        });
        let err = resolve_milestone_id(&c)
            .await
            .expect_err("missing end cursor");
        assert!(
            is_kind(&err, LinearErrorKind::MissingCursor),
            "got {err:?}, want MissingCursor"
        );
    }

    // ─── FetchCandidateIssues (candidates_test.go) ───────────────────────────────────────────────

    // Mirrors Go TestFetchCandidatesAppliesMilestoneFilter.
    #[tokio::test]
    async fn fetch_candidates_applies_milestone_filter() {
        let saw_ms = Arc::new(AtomicBool::new(false));
        let saw_ms_h = Arc::clone(&saw_ms);
        let saw_var = Arc::new(AtomicBool::new(false));
        let saw_var_h = Arc::clone(&saw_var);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                return MockResp::ok(TEST_VIEWER_RESP);
            }
            if req.query.contains("projectMilestones(") {
                saw_ms_h.store(true, Ordering::SeqCst);
                return MockResp::ok(
                    r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-2","name":"v2.0"}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
                );
            }
            if req.var_str("milestoneID") == Some("ms-2")
                && req
                    .query
                    .contains("projectMilestone: { id: { eq: $milestoneID } }")
            {
                saw_var_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            milestone: "v2.0".into(),
            ..Config::default()
        });
        c.fetch_candidate_issues().await.expect("fetch");
        assert!(
            saw_ms.load(Ordering::SeqCst),
            "expected a projectMilestones resolution query"
        );
        assert!(
            saw_var.load(Ordering::SeqCst),
            "expected the issues query to carry milestoneID=ms-2 and the filter clause"
        );
    }

    // Mirrors Go TestFetchCandidatesNoMilestoneOmitsFilter.
    #[tokio::test]
    async fn fetch_candidates_no_milestone_omits_filter() {
        let saw_ms = Arc::new(AtomicBool::new(false));
        let saw_ms_h = Arc::clone(&saw_ms);
        let issues = Arc::new(Mutex::new(Option::<(String, bool)>::None));
        let issues_h = Arc::clone(&issues);
        let server = MockServer::start_with_viewer(move |req| {
            if req.query.contains("projectMilestones(") {
                saw_ms_h.store(true, Ordering::SeqCst);
                return MockResp::ok(r#"{"data":{"projectMilestones":{"nodes":[]}}}"#);
            }
            *issues_h.lock().expect("issues") =
                Some((req.query.clone(), req.var("milestoneID").is_some()));
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            ..Config::default()
        });
        c.fetch_candidate_issues().await.expect("fetch");
        assert!(
            !saw_ms.load(Ordering::SeqCst),
            "no milestone configured must NOT trigger a projectMilestones query"
        );
        let issues = issues.lock().expect("issues");
        let (q, saw_milestone_var) = issues.as_ref().expect("issues query ran");
        assert!(!saw_milestone_var, "must NOT send a milestoneID variable");
        assert!(!q.contains("$milestoneID"), "must NOT declare $milestoneID");
        assert!(
            !q.contains("projectMilestone: { id:"),
            "must NOT add the projectMilestone filter clause"
        );
        assert!(
            q.contains("projectMilestone { id name }"),
            "expected the additive projectMilestone {{ id name }} node selection"
        );
    }

    // Mirrors Go TestFetchCandidatesPaginatesAndPreservesOrder.
    #[tokio::test]
    async fn fetch_candidates_paginates_and_preserves_order() {
        let afters = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let afters_h = Arc::clone(&afters);
        let slug = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let slug_h = Arc::clone(&slug);
        let page = Arc::new(AtomicUsize::new(0));
        let page_h = Arc::clone(&page);
        let (c, _server) = new_test_client(move |req| {
            afters_h
                .lock()
                .expect("afters")
                .push(req.var_str("after").map(String::from));
            slug_h
                .lock()
                .expect("slug")
                .push(req.var_str("projectSlug").map(String::from));
            if page_h.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResp::ok(
                    r#"{"data":{"issues":{"nodes":[
                        {"id":"1","identifier":"MT-1","title":"a","state":{"name":"Todo"}},
                        {"id":"2","identifier":"MT-2","title":"b","state":{"name":"Todo"}}
                    ],"pageInfo":{"hasNextPage":true,"endCursor":"CUR1"}}}}"#,
                )
            } else {
                MockResp::ok(
                    r#"{"data":{"issues":{"nodes":[
                        {"id":"3","identifier":"MT-3","title":"c","state":{"name":"In Progress"}}
                    ],"pageInfo":{"hasNextPage":false,"endCursor":"CUR2"}}}}"#,
                )
            }
        })
        .await;
        let got = c.fetch_candidate_issues().await.expect("fetch");
        let ids: Vec<&str> = got.iter().map(|i| i.identifier.as_str()).collect();
        assert_eq!(
            ids,
            ["MT-1", "MT-2", "MT-3"],
            "order preserved across pages"
        );
        let afters = afters.lock().expect("afters");
        assert_eq!(afters[0], None, "first page after should be nil");
        assert_eq!(afters[1].as_deref(), Some("CUR1"), "second page after");
        assert!(
            slug.lock()
                .expect("slug")
                .iter()
                .all(|s| s.as_deref() == Some("proj")),
            "projectSlug var must be sent on every page"
        );
    }

    // Mirrors Go TestFetchCandidatesMissingEndCursor.
    #[tokio::test]
    async fn fetch_candidates_missing_end_cursor() {
        let (c, _server) = new_test_client(|_req| {
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[{"id":"1","identifier":"MT-1","title":"a","state":{"name":"Todo"}}],"pageInfo":{"hasNextPage":true,"endCursor":""}}}}"#,
            )
        })
        .await;
        let err = c
            .fetch_candidate_issues()
            .await
            .expect_err("missing cursor");
        assert!(
            is_kind(&err, LinearErrorKind::MissingCursor),
            "got {err:?}, want MissingCursor"
        );
    }

    // Mirrors Go TestFetchCandidatesSendsActiveStates.
    #[tokio::test]
    async fn fetch_candidates_sends_active_states() {
        let states = Arc::new(Mutex::new(Option::<Value>::None));
        let states_h = Arc::clone(&states);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                return MockResp::ok(TEST_VIEWER_RESP);
            }
            *states_h.lock().expect("states") = req.var("states").cloned();
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into(), "In Progress".into()],
            ..Config::default()
        });
        c.fetch_candidate_issues().await.expect("fetch");
        let states = states.lock().expect("states");
        let arr = states
            .as_ref()
            .and_then(|v| v.as_array())
            .expect("states array");
        let got: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(got, ["Todo", "In Progress"], "states var");
    }

    // Mirrors Go TestFetchCandidatesPage2FailureReturnsError.
    #[tokio::test]
    async fn fetch_candidates_page2_failure_returns_error() {
        let page = Arc::new(AtomicUsize::new(0));
        let page_h = Arc::clone(&page);
        let (c, _server) = new_test_client(move |_req| {
            if page_h.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResp::ok(
                    r#"{"data":{"issues":{"nodes":[{"id":"1","identifier":"MT-1","title":"a","state":{"name":"Todo"}}],"pageInfo":{"hasNextPage":true,"endCursor":"CUR1"}}}}"#,
                )
            } else {
                MockResp::status(500, "boom")
            }
        })
        .await;
        let err = c.fetch_candidate_issues().await.expect_err("page 2 fails");
        assert!(
            is_kind(&err, LinearErrorKind::ApiStatus),
            "got {err:?}, want ApiStatus"
        );
    }

    // ─── FetchCandidateIssues + viewer (viewer_test.go) ──────────────────────────────────────────

    // Mirrors Go TestFetchCandidatesFiltersByViewerAssignee.
    #[tokio::test]
    async fn fetch_candidates_filters_by_viewer_assignee() {
        let saw = Arc::new(AtomicBool::new(false));
        let saw_h = Arc::clone(&saw);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                return MockResp::ok(TEST_VIEWER_RESP);
            }
            if req.var_str("assigneeID") == Some("viewer-1")
                && req.query.contains("assignee: { id: { eq: $assigneeID } }")
                && req.query.contains("$assigneeID: ID!")
            {
                saw_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"1","identifier":"MT-1","title":"t","state":{"name":"Todo"},"assignee":{"id":"viewer-1","displayName":"Test Owner"}}
                ],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            ..Config::default()
        });
        let got = c.fetch_candidate_issues().await.expect("fetch");
        assert!(
            saw.load(Ordering::SeqCst),
            "expected assigneeID=viewer-1, the assignee.id filter clause, and $assigneeID: ID!"
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].assignee_id, "viewer-1");
        assert_eq!(got[0].assignee_name, "Test Owner");
    }

    // Mirrors Go TestFetchCandidatesViewerFailurePropagates.
    #[tokio::test]
    async fn fetch_candidates_viewer_failure_propagates() {
        let server = MockServer::start(|req| {
            if req.query.contains("viewer {") {
                MockResp::status(500, "boom")
            } else {
                // issues query must never run when viewer resolution fails.
                MockResp::status(599, "issues query must not run")
            }
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            ..Config::default()
        });
        let err = c.fetch_candidate_issues().await.expect_err("viewer failed");
        assert!(
            is_kind(&err, LinearErrorKind::ApiStatus),
            "got {err:?}, want ApiStatus"
        );
    }

    // Mirrors Go TestFetchCandidatesViewerFailureShortCircuitsMilestonePath.
    #[tokio::test]
    async fn fetch_candidates_viewer_failure_short_circuits_milestone_path() {
        let non_viewer = Arc::new(AtomicBool::new(false));
        let non_viewer_h = Arc::clone(&non_viewer);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                return MockResp::status(500, "boom");
            }
            // Neither projectMilestones nor issues may run once viewer resolution fails.
            non_viewer_h.store(true, Ordering::SeqCst);
            MockResp::status(599, "must not run")
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            milestone: "v2.0".into(),
            ..Config::default()
        });
        let err = c.fetch_candidate_issues().await.expect_err("viewer failed");
        assert!(
            is_kind(&err, LinearErrorKind::ApiStatus),
            "got {err:?}, want ApiStatus"
        );
        assert!(
            !non_viewer.load(Ordering::SeqCst),
            "viewer failure must short-circuit before projectMilestones/issues"
        );
    }

    // Mirrors Go TestFetchCandidatesAssigneeAndMilestone.
    #[tokio::test]
    async fn fetch_candidates_assignee_and_milestone() {
        let saw = Arc::new(AtomicBool::new(false));
        let saw_h = Arc::clone(&saw);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                return MockResp::ok(TEST_VIEWER_RESP);
            }
            if req.query.contains("projectMilestones(") {
                return MockResp::ok(
                    r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-2","name":"v2.0"}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
                );
            }
            if req.var_str("assigneeID") == Some("viewer-1")
                && req.var_str("milestoneID") == Some("ms-2")
                && req.query.contains("assignee: { id: { eq: $assigneeID } }")
                && req
                    .query
                    .contains("projectMilestone: { id: { eq: $milestoneID } }")
            {
                saw_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            milestone: "v2.0".into(),
            ..Config::default()
        });
        c.fetch_candidate_issues().await.expect("fetch");
        assert!(
            saw.load(Ordering::SeqCst),
            "expected BOTH assigneeID and milestoneID with both filter clauses"
        );
    }

    // Mirrors Go TestCandidateQueryAssigneeVarIsID.
    #[test]
    fn candidate_query_assignee_var_is_id() {
        for with_ms in [false, true] {
            let q = query::query_candidates(with_ms, false);
            assert!(
                q.contains("$assigneeID: ID!"),
                "withMilestone={with_ms}: must declare $assigneeID as ID!"
            );
            assert!(
                !q.contains("$assigneeID: String!"),
                "withMilestone={with_ms}: $assigneeID must be ID!, never String!"
            );
            assert!(
                q.contains("assignee: { id: { eq: $assigneeID } }"),
                "withMilestone={with_ms}: must include the assignee filter clause"
            );
        }
    }

    // Pool mode (INF-477): the `source` switch fetches UNASSIGNED issues — no viewer query, the
    // `assignee: { null: true }` clause, and no assigneeID variable. No Go test drives the pool
    // FETCH path (only the query TEXT is locked, by query.rs's GOLDEN_CANDIDATES_POOL); this guards
    // the Rust claim-mode branch (`if !pool`) against regression.
    #[tokio::test]
    async fn fetch_candidates_pool_mode_fetches_unassigned() {
        let saw_viewer = Arc::new(AtomicBool::new(false));
        let saw_viewer_h = Arc::clone(&saw_viewer);
        let saw_unassigned = Arc::new(AtomicBool::new(false));
        let saw_unassigned_h = Arc::clone(&saw_unassigned);
        let server = MockServer::start(move |req| {
            if req.query.contains("viewer {") {
                saw_viewer_h.store(true, Ordering::SeqCst);
                return MockResp::ok(TEST_VIEWER_RESP);
            }
            if req.query.contains("assignee: { null: true }")
                && !req.query.contains("$assigneeID")
                && req.var("assigneeID").is_none()
            {
                saw_unassigned_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            active_states: vec!["Todo".into()],
            claim_mode: "pool".into(),
            ..Config::default()
        });
        c.fetch_candidate_issues().await.expect("fetch");
        assert!(
            !saw_viewer.load(Ordering::SeqCst),
            "pool mode must NOT resolve the viewer"
        );
        assert!(
            saw_unassigned.load(Ordering::SeqCst),
            "pool mode must filter unassigned and omit the assigneeID variable"
        );
    }
}
