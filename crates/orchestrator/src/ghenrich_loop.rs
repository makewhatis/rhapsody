//! ghenrich_loop — parity port of Go `internal/orchestrator/ghenrich_loop_test.go`, plus the
//! STUDIO-574 regression that drives the WHOLE GitHub-summons chain (fetch through apply through
//! suppression).
//!
//! Test-only: a child module of [`crate::control_loop`] so it can call the private
//! [`Orchestrator::poll_all_projects`](crate::orchestrator::Orchestrator) poll pass, exactly as Go's
//! same-package `_test.go` file calls `pollAllProjects`.
//!
//! The Go file's `TestBuildEffective_ParsesGitHubOwnerRepo` lives with the pure enrichment tests in
//! [`crate::ghenrich`]; the five `pollAllProjects` cases are ported here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use rhapsody_core::{Issue, LinkedPRRef};
use rhapsody_store::{Sqlite, Store, StorePath};
use rhapsody_tracker::fake::Fake;

use super::*;
use crate::effective::ResolvedProject;
use crate::ghsummons::{GH, RunFn, SummonHit, SummonResult, SummonSource};
use crate::testsupport::{orch_for_retry_multi, proj_with_tracker, seed_run, set_of};

/// A deterministic clock for asserting the github-summons `since` watermark. Mirrors Go `fixedNow`.
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 25, 12, 0, 0)
        .single()
        .expect("fixedNow")
}

/// The recording half of Go's `fakeSrc`: what the source was asked and how often. Shared with the
/// boxed source the orchestrator owns, so a test can read it back after the poll.
#[derive(Default)]
struct SrcLog {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, String, DateTime<Utc>)>>,
}

impl SrcLog {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    /// The `(owner, repo, since)` of the first query, or `None` when never queried.
    fn first(&self) -> Option<(String, String, DateTime<Utc>)> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first()
            .cloned()
    }
}

/// A programmable [`SummonSource`] returning a fixed `by_pr` map, recording every query. Mirrors Go
/// `fakeSrc` (`out` + the `seen`/`seenSince`/`calls` recording fields the loop tests read).
struct FakeSrc {
    out: HashMap<i64, SummonHit>,
    log: Arc<SrcLog>,
}

#[async_trait::async_trait]
impl SummonSource for FakeSrc {
    async fn summons_since(&self, owner: &str, repo: &str, since: DateTime<Utc>) -> SummonResult {
        self.log.calls.fetch_add(1, Ordering::SeqCst);
        self.log
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((owner.to_string(), repo.to_string(), since));
        Ok(self.out.clone())
    }
}

/// Builds a recording fake source over `hits` (PR number -> summon time, empty body). Mirrors Go
/// `&fakeSrc{out: hits(...)}`.
fn fake_src(hits: &[(i64, DateTime<Utc>)]) -> (Box<dyn SummonSource>, Arc<SrcLog>) {
    let log = Arc::new(SrcLog::default());
    let out = hits
        .iter()
        .map(|(n, at)| {
            (
                *n,
                SummonHit {
                    at: *at,
                    body: String::new(),
                },
            )
        })
        .collect();
    (
        Box::new(FakeSrc {
            out,
            log: Arc::clone(&log),
        }),
        log,
    )
}

/// An `In Progress` candidate carrying one unmerged linked PR in `owner/repo`.
fn issue_with_pr(id: &str, ident: &str, owner: &str, repo: &str, number: i64) -> Issue {
    Issue {
        id: id.to_string(),
        identifier: ident.to_string(),
        title: "t".to_string(),
        state: "Todo".to_string(),
        linked_pr: true,
        linked_prs: Some(vec![LinkedPRRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            merged: false,
        }]),
        ..Default::default()
    }
}

/// A resolved project with github-summons on and an owner/repo, backed by a fake tracker returning
/// `candidates`. Mirrors Go `summonTrackerProject`.
fn summon_project(slug: &str, owner: &str, repo: &str, candidates: Vec<Issue>) -> ResolvedProject {
    let mut tr = Fake::new();
    tr.candidates = candidates;
    let mut p = proj_with_tracker(slug, Arc::new(tr), "prompt");
    p.github_summons = true;
    p.gh_owner = owner.to_string();
    p.gh_repo = repo.to_string();
    p
}

// Mirrors Go `TestPollAllProjects_EnrichAdvancesSummonWhenSourceSet`: with a source set and the
// project flag on, the candidate's latest_summon_at advances, queried at `now - lookback`.
#[tokio::test]
async fn poll_all_projects_enrich_advances_summon_when_source_set() {
    let summon = fixed_now() - chrono::Duration::minutes(1);
    let issues = vec![issue_with_pr("x1", "X-1", "o", "r", 7)];
    let (mut o, _spawned) = orch_for_retry_multi(vec![summon_project("x", "o", "r", issues)], 10);
    o.now = Box::new(fixed_now);
    let (src, log) = fake_src(&[(7, summon)]);
    o.gh_source = Some(src);

    let tagged = o.poll_all_projects().await;

    let (owner, repo, since) = log.first().expect("gh source must be queried");
    assert_eq!((owner.as_str(), repo.as_str()), ("o", "r"));
    assert_eq!(
        since,
        fixed_now() - chrono::Duration::seconds(DEFAULT_GH_LOOKBACK.as_secs() as i64),
        "since must be now - lookback (deterministic clock)"
    );
    assert_eq!(tagged.len(), 1, "expected 1 tagged issue");
    assert_eq!(
        tagged[0].iss.latest_summon_at,
        Some(summon),
        "latest_summon_at must advance from the PR comment"
    );
}

// Mirrors Go `TestPollAllProjects_NoSourceLeavesIssuesUntouched`: source nil (feature off) ⇒ the
// enrich call site is skipped and candidates flow through unchanged.
#[tokio::test]
async fn poll_all_projects_no_source_leaves_issues_untouched() {
    let issues = vec![issue_with_pr("x1", "X-1", "o", "r", 7)];
    let (mut o, _spawned) = orch_for_retry_multi(vec![summon_project("x", "o", "r", issues)], 10);
    o.gh_source = None; // feature off

    let tagged = o.poll_all_projects().await;

    assert_eq!(tagged.len(), 1, "expected 1 tagged issue");
    assert!(
        tagged[0].iss.latest_summon_at.is_none(),
        "a nil source must leave latest_summon_at untouched"
    );
}

// Mirrors Go `TestPollAllProjects_FlagOffSkipsEnrich`: a project whose github_summons flag is off is
// never queried, even with a source set.
#[tokio::test]
async fn poll_all_projects_flag_off_skips_enrich() {
    let issues = vec![issue_with_pr("x1", "X-1", "o", "r", 7)];
    let mut p = summon_project("x", "o", "r", issues);
    p.github_summons = false; // feature off for this project
    let (mut o, _spawned) = orch_for_retry_multi(vec![p], 10);
    o.now = Box::new(fixed_now);
    let (src, log) = fake_src(&[(7, fixed_now())]);
    o.gh_source = Some(src);

    let _ = o.poll_all_projects().await;

    assert_eq!(
        log.calls(),
        0,
        "a project with github_summons off must NOT query the source"
    );
}

// Mirrors Go `TestPollAllProjects_SharedRepoFetchedOnce`: two projects on the SAME repo trigger
// exactly ONE fetch, and both projects' candidates are enriched from it.
#[tokio::test]
async fn poll_all_projects_shared_repo_fetched_once() {
    let summon = fixed_now() - chrono::Duration::minutes(1);
    let a = summon_project("a", "o", "r", vec![issue_with_pr("a1", "A-1", "o", "r", 7)]);
    let b = summon_project("b", "o", "r", vec![issue_with_pr("b1", "B-1", "o", "r", 7)]);
    let (mut o, _spawned) = orch_for_retry_multi(vec![a, b], 10);
    o.now = Box::new(fixed_now);
    let (src, log) = fake_src(&[(7, summon)]);
    o.gh_source = Some(src);

    let tagged = o.poll_all_projects().await;

    assert_eq!(log.calls(), 1, "two projects on o/r ⇒ one fetch");
    assert_eq!(tagged.len(), 2, "expected 2 tagged issues");
    for ti in &tagged {
        assert_eq!(
            ti.iss.latest_summon_at,
            Some(summon),
            "{} must be enriched from the single fetch",
            ti.iss.identifier
        );
    }
}

// Mirrors Go `TestPollAllProjects_SharedRepoCaseInsensitiveDedup`: the per-tick fetch-cache key is
// case-insensitive, matching the case-folded repo guard in `apply_github_summons`.
#[tokio::test]
async fn poll_all_projects_shared_repo_case_insensitive_dedup() {
    let a = summon_project("a", "o", "r", vec![issue_with_pr("a1", "A-1", "o", "r", 7)]);
    let b = summon_project("b", "O", "R", vec![issue_with_pr("b1", "B-1", "O", "R", 7)]);
    let (mut o, _spawned) = orch_for_retry_multi(vec![a, b], 10);
    o.now = Box::new(fixed_now);
    let (src, log) = fake_src(&[(7, fixed_now() - chrono::Duration::minutes(1))]);
    o.gh_source = Some(src);

    let _ = o.poll_all_projects().await;

    assert_eq!(log.calls(), 1, "o/r and O/R are the same repo ⇒ one fetch");
}

// Mirrors Go `TestPollAllProjects_DuplicateIssueTaggedOnceAndEnriched`: a duplicate issue ID is
// tagged once AND the kept copy is still enriched (enrichment runs after the dedup).
#[tokio::test]
async fn poll_all_projects_duplicate_issue_tagged_once_and_enriched() {
    let summon = fixed_now() - chrono::Duration::minutes(1);
    let a = summon_project(
        "a",
        "o",
        "r",
        vec![issue_with_pr("dup1", "DUP-1", "o", "r", 7)],
    );
    let b = summon_project(
        "b",
        "o",
        "r",
        vec![issue_with_pr("dup1", "DUP-1", "o", "r", 7)],
    );
    let (mut o, _spawned) = orch_for_retry_multi(vec![a, b], 10);
    o.now = Box::new(fixed_now);
    let (src, log) = fake_src(&[(7, summon)]);
    o.gh_source = Some(src);

    let tagged = o.poll_all_projects().await;

    assert_eq!(tagged.len(), 1, "expected 1 tagged issue (deduped)");
    assert_eq!(
        tagged[0].iss.latest_summon_at,
        Some(summon),
        "the kept copy must be enriched after the dedup"
    );
    assert_eq!(log.calls(), 1, "one repo ⇒ one fetch");
}

// --- STUDIO-574: fetch → apply → suppression, end to end ---------------------------------------

/// The Linear GraphQL shape of the reported ticket: an `In Review` issue whose only attachment is
/// the UNMERGED GitHub PR the summons was posted on. Normalizing this is what produces the
/// `linked_prs` entry the enrichment maps hits onto — the ticket→PR linkage STUDIO-574 suspected.
const RAW_IN_REVIEW_ISSUE: &str = r#"{
  "id": "iss-569",
  "identifier": "STUDIO-569",
  "title": "Discovery: persistent named agents",
  "state": { "name": "In Review" },
  "team": { "id": "team-1" },
  "attachments": { "nodes": [
    { "sourceType": "github", "metadata": {
        "url": "https://github.com/studio49dev/studio-infra/pull/71",
        "status": "open",
        "createdAt": "2026-08-24T15:10:00.000Z",
        "updatedAt": "2026-08-24T20:00:00.000Z"
    } }
  ] },
  "comments": { "nodes": [] }
}"#;

/// A `gh api --paginate --slurp repos/.../issues/comments?...` body carrying the reported summons.
const GH_ISSUE_COMMENTS: &str = r#"[[
  {"body":"nice work","created_at":"2026-08-24T16:00:00Z","updated_at":"2026-08-24T16:00:00Z","issue_url":"https://api.github.com/repos/studio49dev/studio-infra/issues/71"},
  {"body":"@rhapsody Reposting with the correct summon token, please address the review findings.","created_at":"2026-08-24T21:48:32Z","updated_at":"2026-08-24T21:48:32Z","issue_url":"https://api.github.com/repos/studio49dev/studio-infra/issues/71"}
]]"#;

/// Normalizes [`RAW_IN_REVIEW_ISSUE`] through the REAL Linear normalizer, so the test consumes the
/// same `linked_prs` the daemon derives from a live candidate page (rather than a hand-built ref).
fn normalized_in_review_issue() -> Issue {
    let raw: rhapsody_tracker::linear::RawIssue =
        serde_json::from_str(RAW_IN_REVIEW_ISSUE).expect("raw issue decodes");
    let client = rhapsody_tracker::linear::new(rhapsody_tracker::linear::Config {
        endpoint: String::new(),
        api_key: "k".to_string(),
        project_slug: "studio-infra".to_string(),
        active_states: vec!["Todo".to_string(), "In Progress".to_string()],
        review_states: vec!["In Review".to_string()],
        summon_token: "@rhapsody".to_string(),
        milestone: String::new(),
        claim_mode: String::new(),
    });
    client.normalize_issue(raw)
}

/// The REAL `gh`-exec summon source with an injected runner that answers the two `gh api` endpoints
/// from canned bodies, recording the endpoint arguments it was asked for.
fn gh_source_for(token: &str, endpoints: Arc<Mutex<Vec<String>>>) -> Box<dyn SummonSource> {
    let run: RunFn = Box::new(move |args| {
        let ep = args.last().copied().unwrap_or_default().to_string();
        endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ep.clone());
        if ep.contains("issues/comments") {
            return Ok(GH_ISSUE_COMMENTS.as_bytes().to_vec());
        }
        Ok(b"[[]]".to_vec())
    });
    Box::new(GH::new(token, Some(run)))
}

/// The tick clock for the STUDIO-574 scenario: 21 seconds after the summons was posted, mirroring
/// the reported `21:48:53` poll cycle.
fn studio574_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 24, 21, 48, 53)
        .single()
        .expect("studio574 now")
}

/// The reported run start — SEVEN HOURS before the summons, so a correctly-applied summons is
/// strictly newer than the last run's start and must lift the suppression.
fn studio574_run_start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 24, 14, 43, 3)
        .single()
        .expect("studio574 run start")
}

/// Wires the STUDIO-574 orchestrator: one project on `studio49dev/studio-infra` with
/// github-summons on, the real `GH` source over canned `gh api` bodies, a store seeded with the
/// 14:43:03Z run, and the fixed 21:48:53Z tick clock.
fn studio574_orch(
    active_states: &[&str],
) -> (
    Orchestrator,
    crate::testsupport::DispatchedEntries,
    Arc<Mutex<Vec<String>>>,
) {
    let iss = normalized_in_review_issue();
    assert!(iss.linked_pr, "the attachment must register as a linked PR");
    let mut p = summon_project("studio-infra", "studio49dev", "studio-infra", vec![iss]);
    p.active_states = set_of(active_states);
    p.review_states = set_of(&["in review"]);
    let (mut o, spawned) = orch_for_retry_multi(vec![p], 10);
    if let Some(eff) = o.eff.as_mut() {
        eff.review_promote_state = "In Progress".to_string();
        eff.review_states = set_of(&["in review"]);
        eff.active_states = set_of(active_states);
    }
    o.now = Box::new(studio574_now);
    let endpoints = Arc::new(Mutex::new(Vec::new()));
    o.gh_source = Some(gh_source_for("@rhapsody", Arc::clone(&endpoints)));
    let store: Arc<dyn Store + Send + Sync> =
        Arc::new(Sqlite::open(StorePath::InMemory).expect("in-memory store"));
    seed_run(
        store.as_ref(),
        "iss-569",
        "STUDIO-569",
        studio574_run_start() + chrono::Duration::minutes(1),
    );
    o.set_store(store);
    (o, spawned, endpoints)
}

// STUDIO-574 (fetch + apply): a summons comment on an UNMERGED PR linked to an In-Review ticket
// must advance `latest_summon_at` past the last run's start, so `pr_suppressed` stops suppressing
// and the poller re-engages the agent. Drives the REAL `GH` source (two `gh api` endpoints, slurped
// pages, token regex) and the REAL Linear normalizer (attachment → `linked_prs`) — the fetch half is
// what `summons_since_matches_and_picks_max` covers; the apply + suppression halves are new.
#[tokio::test]
async fn github_summons_on_unmerged_pr_lifts_pr_suppression() {
    // "In Review" is BOTH active and review here — the reported daemon's config, which routes the
    // ticket down the active branch and into `pr_suppressed` (the branch that logged the failure).
    let (o, _spawned, endpoints) = studio574_orch(&["todo", "in progress", "in review"]);

    let tagged = o.poll_all_projects().await;

    assert_eq!(tagged.len(), 1, "the candidate must survive the poll");
    let iss = &tagged[0].iss;
    assert_eq!(
        iss.linked_prs.as_deref(),
        Some(
            [LinkedPRRef {
                owner: "studio49dev".to_string(),
                repo: "studio-infra".to_string(),
                number: 71,
                merged: false,
            }]
            .as_slice()
        ),
        "the Linear attachment must resolve to the PR number the summons map is keyed by"
    );
    assert_eq!(
        iss.latest_summon_at,
        Utc.with_ymd_and_hms(2026, 8, 24, 21, 48, 32).single(),
        "the newest tokened PR comment must land on the issue"
    );
    assert!(
        iss.latest_summon_body
            .contains("Reposting with the correct"),
        "the summons body must ride along with its time, got {:?}",
        iss.latest_summon_body
    );
    assert!(
        !o.pr_suppressed(iss),
        "a summons 7h after the last run start must lift the linked-PR suppression"
    );
    // Scoped so the guard is released before the `on_tick().await` below.
    {
        let eps = endpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(eps.len(), 2, "exactly two gh api calls per repo per tick");
        assert!(
            eps[0].contains(
                "repos/studio49dev/studio-infra/issues/comments?since=2026-08-24T21:43:53Z"
            ),
            "the issue-comments endpoint must carry the repo and the now-lookback watermark: {:?}",
            eps[0]
        );
    }

    // …and the whole tick dispatches it.
    let (mut o, spawned, _eps) = studio574_orch(&["todo", "in progress", "in review"]);
    o.on_tick().await;
    if let Some(t) = o.tick_timer.take() {
        t.abort();
    }
    let entries = spawned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        entries.len(),
        1,
        "the summoned ticket must be re-dispatched, not suppressed"
    );
    assert_eq!(entries[0].issue.identifier, "STUDIO-569");
}

// STUDIO-574 (review-reopen branch): with "In Review" a review-ONLY state, the same GitHub summons
// must make `review_reopen_eligible` true so the ticket is promoted and re-dispatched.
#[tokio::test]
async fn github_summons_reopens_review_only_ticket() {
    let (mut o, spawned, _eps) = studio574_orch(&["todo", "in progress"]);

    o.on_tick().await;
    if let Some(t) = o.tick_timer.take() {
        t.abort();
    }

    let entries = spawned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        entries.len(),
        1,
        "the summoned review ticket must be promoted and dispatched"
    );
    assert_eq!(entries[0].issue.identifier, "STUDIO-569");
}
