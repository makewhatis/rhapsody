//! ghenrich — parity port of Go `internal/orchestrator/ghenrich.go`.
//!
//! GitHub-summons enrichment: advance a candidate's `latest_summon_at` (and, in the same update, its
//! `latest_summon_body` — so time and body always describe the SAME comment, INF-448) from the newest
//! summoning PR comment on an UNMERGED linked PR. Split into three functions mirroring the Go source:
//!
//!   * [`fetch_github_summons`] — the (bounded) source query for ONE repo, so a multi-project tick
//!     fetches each distinct repo only once. Best-effort: a nil source / empty owner|repo / a source
//!     error or timeout all yield `None` (the caller treats `None` as "nothing to apply").
//!   * [`apply_github_summons`] — the PURE apply step over a pre-fetched map (max-only, unmerged-only,
//!     repo-guarded).
//!   * [`enrich_with_github_summons`] — the single-repo convenience (fetch + apply) used by the legacy
//!     single-project poll path.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's `context.WithTimeout(ctx, ghSummonsTimeout)` bounding the `gh` exec becomes
//!     [`tokio::time::timeout`]; a timeout is folded into the same "skip this tick → `None`" path as a
//!     source error.
//!   * Go passes/returns `[]core.Issue` (the slice is mutated in place and returned); the Rust port
//!     takes `Vec<Issue>` by value and returns it, so `enrich(issues)` reads back the enriched issues.
//!   * `strings.EqualFold` (the case-insensitive owner/repo guard) becomes
//!     [`str::eq_ignore_ascii_case`]; GitHub owner/repo identifiers are ASCII, so the two agree on
//!     every real input.
//!   * The best-effort diagnostics log via `tracing` (as the sibling crates do) instead of a threaded
//!     `slog` logger.
//!   * STUDIO-574 adds success-path diagnostics Go does not emit: [`fetch_github_summons`] logs the
//!     repo / `since` watermark / PR numbers found, and [`apply_github_summons`] logs a per-reason
//!     drop tally. Both are additive `tracing` events — the enrichment's data flow is unchanged.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_core::Issue;

use crate::ghsummons::{SummonHit, SummonSource};

/// Bounds the `gh` exec per enrichment call. 15s is well under the 30s poll interval; a
/// network-stalled `gh` subprocess cannot wedge the control loop longer than this. Mirrors Go
/// `ghSummonsTimeout`.
const GH_SUMMONS_TIMEOUT: Duration = Duration::from_secs(15);

/// Makes the source call for one repo and returns the per-PR summon hits (newest comment time +
/// body). Best-effort: a `None` src / empty owner|repo / a source error or timeout all yield `None`
/// (the caller treats `None` as "nothing to apply"); a source error/timeout logs one info line. The
/// call is bounded by [`GH_SUMMONS_TIMEOUT`] so a stalled network call cannot wedge the control loop.
/// Split from the apply step so a multi-project tick fetches each distinct repo only ONCE. Mirrors Go
/// `fetchGitHubSummons`.
///
/// `pub` (Go's package-private `fetchGitHubSummons`): the three enrichment functions are the crate's
/// GitHub-summons enrichment API, consumed by O7's `poll_all_projects` (the per-repo fetch/apply
/// split) + the daemon wiring — exposed as public API rather than carrying a dead-code `#[allow]`
/// until that consumer lands.
pub async fn fetch_github_summons(
    src: Option<&dyn SummonSource>,
    owner: &str,
    repo: &str,
    since: DateTime<Utc>,
) -> Option<HashMap<i64, SummonHit>> {
    let src = src?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    match tokio::time::timeout(GH_SUMMONS_TIMEOUT, src.summons_since(owner, repo, since)).await {
        Ok(Ok(by_pr)) => {
            // STUDIO-574: the success path used to log NOTHING, so "the source was never queried",
            // "it ran and found nothing", and "it found hits that were then dropped" were all the
            // same zero lines. Naming the repo, the watermark, and the PR numbers found separates
            // the first two; `apply_github_summons`'s counters separate the third.
            let mut prs: Vec<i64> = by_pr.keys().copied().collect();
            prs.sort_unstable();
            tracing::debug!(
                repo = %format!("{owner}/{repo}"),
                since = %since.to_rfc3339_opts(SecondsFormat::Secs, true),
                hits = by_pr.len(),
                prs = ?prs,
                "github-summons: fetched PR summons"
            );
            Some(by_pr)
        }
        Ok(Err(e)) => {
            tracing::info!(repo = %format!("{owner}/{repo}"), err = %e, "github-summons: enrichment skipped this tick");
            None
        }
        Err(_elapsed) => {
            tracing::info!(repo = %format!("{owner}/{repo}"), "github-summons: enrichment skipped this tick (gh timed out)");
            None
        }
    }
}

/// Advances each issue's `latest_summon_at` (max only) — and, in the same update, `latest_summon_body`
/// so time and body always describe the SAME comment (INF-448) — using a pre-fetched `by_pr` map for
/// `owner`/`repo`, considering only UNMERGED linked PRs in that repo. Pure (its only side effects are
/// `tracing` events). Mirrors Go `applyGitHubSummons`. `pub` for O7's per-project apply (see
/// [`fetch_github_summons`]).
///
/// STUDIO-574: every way a fetched hit fails to land is a bare `continue`, so a broken link was
/// indistinguishable from "nobody summoned". Each drop reason is now counted and reported on one
/// debug line, and an issue whose linked PRs ALL sit outside the polled repo — which no summons can
/// ever reach — is named at info.
pub fn apply_github_summons(
    mut issues: Vec<Issue>,
    by_pr: &HashMap<i64, SummonHit>,
    owner: &str,
    repo: &str,
) -> Vec<Issue> {
    if by_pr.is_empty() {
        return issues;
    }
    // STUDIO-574 observability: every drop below is a `continue` with no trace, so a hit that never
    // reaches an issue is invisible. Count each drop REASON and emit one line per call, so
    // "hits found, none applied" is distinguishable from "no hits" — and says WHY.
    let issue_count = issues.len();
    let (mut linked, mut other_repo, mut merged, mut no_hit, mut matched, mut advanced) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for iss in issues.iter_mut() {
        // Clone the PR refs out so the loop can mutate the issue's summon fields without aliasing the
        // `linked_prs` borrow (Go iterates a slice field while assigning sibling fields — legal in Go,
        // not in Rust). The list is small (an issue's linked PRs).
        let prs = iss.linked_prs.clone().unwrap_or_default();
        linked += prs.len();
        // Per-issue tally of the repo guard, so a ticket whose linked PRs ALL live outside the polled
        // repo can be named below — that ticket can never be re-engaged by a GitHub summons, and it
        // is otherwise completely silent.
        let mut iss_other_repo = 0usize;
        // The distinct repos those PRs DO live in — the operator needs the repo to point the project
        // at, not just the count. Only grows for a foreign PR, which is the rare case.
        let mut iss_pr_repos: Vec<String> = Vec::new();
        for pr in &prs {
            // Skip PRs from a different repo — `by_pr` only holds data for owner/repo, so a matching PR
            // number from another repo would falsely advance `latest_summon_at` and trigger a spurious
            // dispatch. GitHub owner/repo are case-insensitive, so compare case-folded (the configured
            // repo URL and the Linear attachment URL can legitimately differ in casing).
            if !pr.owner.eq_ignore_ascii_case(owner) || !pr.repo.eq_ignore_ascii_case(repo) {
                other_repo += 1;
                iss_other_repo += 1;
                let name = format!("{}/{}", pr.owner, pr.repo);
                if !iss_pr_repos.contains(&name) {
                    iss_pr_repos.push(name);
                }
                continue;
            }
            if pr.merged {
                merged += 1;
                continue;
            }
            let Some(hit) = by_pr.get(&pr.number) else {
                no_hit += 1;
                continue;
            };
            matched += 1;
            if iss.latest_summon_at.is_none_or(|current| hit.at > current) {
                advanced += 1;
                iss.latest_summon_at = Some(hit.at);
                iss.latest_summon_body = hit.body.clone();
                tracing::info!(issue_identifier = %iss.identifier, pr = pr.number, at = %hit.at, "github-summons: advanced latest_summon_at from PR comment");
            }
        }
        // Every linked PR on this issue lives outside the repo we polled, so no summons on any of
        // them can EVER reach this ticket — a routing fault (the ticket's project points at a
        // different repo than its PRs), not a quiet no-op. INFO because it is the one drop reason an
        // operator must act on, and it cannot fire for a correctly-routed ticket.
        if iss_other_repo > 0 && iss_other_repo == prs.len() {
            tracing::info!(
                issue_identifier = %iss.identifier,
                polled_repo = %format!("{owner}/{repo}"),
                pr_repos = %iss_pr_repos.join(", "),
                linked_prs = prs.len(),
                "github-summons: issue's linked PRs are all in another repo; summons on them can never re-engage it"
            );
        }
    }
    // One line per apply pass: `hits > 0` with `matched == 0` is exactly the STUDIO-574 signature,
    // and the drop-reason counters say which link broke (no linked PRs at all / wrong repo / already
    // merged / no summons on that PR number).
    tracing::debug!(
        repo = %format!("{owner}/{repo}"),
        hits = by_pr.len(),
        issues = issue_count,
        linked_prs_total = linked,
        skipped_other_repo = other_repo,
        skipped_merged = merged,
        skipped_no_hit = no_hit,
        matched,
        advanced,
        "github-summons: applied PR summons"
    );
    issues
}

/// Fetches summons for one repo and applies them — the single-repo convenience used by the legacy
/// single-project poll path. Multi-project callers should fetch once per distinct repo
/// ([`fetch_github_summons`]) and apply per project ([`apply_github_summons`]) to keep GitHub usage
/// flat at two `gh` calls per repo per tick. Pure given `src`. Mirrors Go `enrichWithGitHubSummons`.
/// `pub` for the legacy single-project poll path (see [`fetch_github_summons`]).
pub async fn enrich_with_github_summons(
    issues: Vec<Issue>,
    src: Option<&dyn SummonSource>,
    owner: &str,
    repo: &str,
    since: DateTime<Utc>,
) -> Vec<Issue> {
    match fetch_github_summons(src, owner, repo, since).await {
        Some(by_pr) => apply_github_summons(issues, &by_pr, owner, repo),
        None => issues,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use rhapsody_config::workflow::{Definition, YamlMap};
    use rhapsody_config::{Config, decode, resolve};
    use rhapsody_core::LinkedPRRef;

    use super::*;
    use crate::effective::build_effective;
    use crate::ghsummons::SummonResult;
    use crate::testsupport::utc;

    /// A programmable [`SummonSource`] returning a fixed `by_pr` map (or an error). Mirrors the
    /// `out`/`err` half of Go `fakeSrc` — the `seen`/`seenSince`/`calls` recording fields Go's
    /// `fakeSrc` also carries are read only by the `pollAllProjects` loop tests (O7), so they are not
    /// modelled here.
    struct FakeSrc {
        out: HashMap<i64, SummonHit>,
        err: bool,
    }

    impl FakeSrc {
        fn ok(out: HashMap<i64, SummonHit>) -> FakeSrc {
            FakeSrc { out, err: false }
        }
        fn failing() -> FakeSrc {
            FakeSrc {
                out: HashMap::new(),
                err: true,
            }
        }
    }

    #[async_trait]
    impl SummonSource for FakeSrc {
        async fn summons_since(
            &self,
            _owner: &str,
            _repo: &str,
            _since: DateTime<Utc>,
        ) -> SummonResult {
            if self.err {
                return Err("gh down".into());
            }
            Ok(self.out.clone())
        }
    }

    /// Builds a `by_pr` map with empty bodies, for tests that only assert times. Mirrors Go `hits`.
    fn hits(m: &[(i64, DateTime<Utc>)]) -> HashMap<i64, SummonHit> {
        m.iter()
            .map(|(n, at)| {
                (
                    *n,
                    SummonHit {
                        at: *at,
                        body: String::new(),
                    },
                )
            })
            .collect()
    }

    fn linked(owner: &str, repo: &str, number: i64, merged: bool) -> LinkedPRRef {
        LinkedPRRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            merged,
        }
    }

    // Mirrors Go `TestEnrich_AdvancesUnmergedOnly`.
    #[tokio::test]
    async fn enrich_advances_unmerged_only() {
        let summon = utc(2026, 6, 25, 16, 55, 0);
        let issues = vec![Issue {
            identifier: "AIE-1".into(),
            linked_prs: Some(vec![
                linked("o", "r", 100, true),  // merged → ignored
                linked("o", "r", 101, false), // unmerged → applies
            ]),
            ..Default::default()
        }];
        let src = FakeSrc::ok(hits(&[
            (100, summon + chrono::Duration::hours(1)),
            (101, summon),
        ]));
        let got = enrich_with_github_summons(
            issues,
            Some(&src),
            "o",
            "r",
            summon - chrono::Duration::hours(1),
        )
        .await;
        assert_eq!(
            got[0].latest_summon_at,
            Some(summon),
            "want PR101; PR100 merged must be ignored"
        );
    }

    // Mirrors Go `TestEnrich_MaxOnly`.
    #[tokio::test]
    async fn enrich_max_only() {
        let existing = utc(2026, 6, 25, 18, 0, 0); // newer Linear summon already present
        let older = utc(2026, 6, 25, 16, 0, 0);
        let issues = vec![Issue {
            identifier: "AIE-1".into(),
            latest_summon_at: Some(existing),
            linked_prs: Some(vec![linked("o", "r", 101, false)]),
            ..Default::default()
        }];
        let src = FakeSrc::ok(hits(&[(101, older)]));
        let got = enrich_with_github_summons(issues, Some(&src), "o", "r", older).await;
        assert_eq!(
            got[0].latest_summon_at,
            Some(existing),
            "want unchanged (max only)"
        );
    }

    // Mirrors Go `TestEnrich_ErrorLeavesIssuesUntouched`.
    #[tokio::test]
    async fn enrich_error_leaves_issues_untouched() {
        let issues = vec![Issue {
            identifier: "AIE-1".into(),
            linked_prs: Some(vec![linked("o", "r", 101, false)]),
            ..Default::default()
        }];
        let src = FakeSrc::failing();
        let got =
            enrich_with_github_summons(issues, Some(&src), "o", "r", utc(2026, 6, 25, 12, 0, 0))
                .await;
        assert!(
            got[0].latest_summon_at.is_none(),
            "error must leave latest_summon_at nil"
        );
    }

    // Mirrors Go `TestEnrich_CrossRepoPRNotAdvanced`: a linked PR whose owner/repo differs from the
    // polled repo must NOT advance latest_summon_at even if its number collides with a summoned PR.
    #[tokio::test]
    async fn enrich_cross_repo_pr_not_advanced() {
        let summon = utc(2026, 6, 25, 16, 55, 0);
        let issues = vec![Issue {
            identifier: "AIE-1".into(),
            // PR #42 but in a different repo — must be skipped.
            linked_prs: Some(vec![linked("o2", "r2", 42, false)]),
            ..Default::default()
        }];
        // by_pr has #42 for the polled repo "o"/"r" — a collision by number only.
        let src = FakeSrc::ok(hits(&[(42, summon)]));
        let got = enrich_with_github_summons(
            issues,
            Some(&src),
            "o",
            "r",
            summon - chrono::Duration::hours(1),
        )
        .await;
        assert!(
            got[0].latest_summon_at.is_none(),
            "cross-repo PR must not advance latest_summon_at"
        );
    }

    // Mirrors Go `TestEnrich_RepoGuardCaseInsensitive`: the repo guard compares owner/repo
    // case-insensitively, so a casing-only mismatch between the configured repo URL and the Linear
    // attachment URL must still match.
    #[tokio::test]
    async fn enrich_repo_guard_case_insensitive() {
        let summon = utc(2026, 6, 25, 16, 55, 0);
        let issues = vec![Issue {
            identifier: "AIE-1".into(),
            // Same repo as polled, but different casing — must still match. (The Go case uses
            // pre-purge legacy vendor-prefixed names; Rhapsody's brand guard forbids those, so
            // brand-neutral mixed-case names stand in — the assertion is purely about case-insensitive matching.)
            linked_prs: Some(vec![linked("Acme-Corp", "Neat-Widget", 42, false)]),
            ..Default::default()
        }];
        let src = FakeSrc::ok(hits(&[(42, summon)]));
        let got = enrich_with_github_summons(
            issues,
            Some(&src),
            "acme-corp",
            "neat-widget",
            summon - chrono::Duration::hours(1),
        )
        .await;
        assert_eq!(
            got[0].latest_summon_at,
            Some(summon),
            "casing-only mismatch must still advance"
        );
    }

    // --- STUDIO-574: the enrichment must not fail silently ------------------------------------
    //
    // Before this, every stage logged only on error: zero lines meant "never ran", "ran and found
    // nothing", and "found hits that were then dropped" alike. These pin the success-path
    // diagnostics that tell those apart. They follow the TRA-243 recording-subscriber protocol
    // (warm the callsite, rebuild the interest cache, then capture) because these callsites are
    // shared with the tests above, which run without a subscriber.

    /// Runs `f` under a recording subscriber and returns the captured events, warming the callsites
    /// with a throwaway pass first so a sibling test cannot pin them `Interest::never` (TRA-243).
    async fn captured<F, Fut>(f: F) -> Vec<crate::testsupport::CapturedEvent>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        f().await; // warm-up: force every callsite to register
        tracing::callsite::rebuild_interest_cache();
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        f().await;
        drop(guard);
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The one captured event with `message`, or a panic naming what WAS captured.
    fn only(
        events: &[crate::testsupport::CapturedEvent],
        message: &str,
    ) -> HashMap<String, String> {
        let hits: Vec<&crate::testsupport::CapturedEvent> =
            events.iter().filter(|e| e.message == message).collect();
        assert_eq!(
            hits.len(),
            1,
            "want exactly one {message:?} event, got {:?}",
            events.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
        hits[0].fields.clone()
    }

    // STUDIO-574: the fetch success path must say WHICH repo, from WHICH watermark, and WHAT it
    // found — so "no hits" is distinguishable from "never ran".
    #[tokio::test]
    async fn fetch_logs_repo_since_and_hits_on_success() {
        let since = utc(2026, 8, 24, 21, 43, 53);
        let src = FakeSrc::ok(hits(&[(71, utc(2026, 8, 24, 21, 48, 32))]));
        let events = captured(|| async {
            let _ = fetch_github_summons(Some(&src), "studio49dev", "studio-infra", since).await;
        })
        .await;

        let f = only(&events, "github-summons: fetched PR summons");
        assert_eq!(
            f.get("repo").map(String::as_str),
            Some("studio49dev/studio-infra")
        );
        assert_eq!(
            f.get("since").map(String::as_str),
            Some("2026-08-24T21:43:53Z")
        );
        assert_eq!(f.get("hits").map(String::as_str), Some("1"));
        assert_eq!(
            f.get("prs").map(String::as_str),
            Some("[71]"),
            "the PR numbers found"
        );
    }

    // STUDIO-574: a fetch that finds nothing still logs, with `hits = 0` — the line that separates
    // "ran and found nothing" from "never ran at all".
    #[tokio::test]
    async fn fetch_logs_zero_hits_distinctly() {
        let src = FakeSrc::ok(HashMap::new());
        let events = captured(|| async {
            let _ = fetch_github_summons(Some(&src), "o", "r", utc(2026, 8, 24, 21, 43, 53)).await;
        })
        .await;

        let f = only(&events, "github-summons: fetched PR summons");
        assert_eq!(f.get("hits").map(String::as_str), Some("0"));
    }

    // STUDIO-574: hits found but applied to nothing is the reported bug's exact signature. The apply
    // line must carry the drop-reason breakdown so the broken link is readable at a glance.
    #[tokio::test]
    async fn apply_logs_hits_found_but_none_applied_with_reasons() {
        let by_pr = hits(&[(71, utc(2026, 8, 24, 21, 48, 32))]);
        let issues = vec![
            // PR in another repo — the routing fault.
            Issue {
                identifier: "AIE-1".into(),
                linked_prs: Some(vec![linked("other", "repo", 71, false)]),
                ..Default::default()
            },
            // Right repo, but already merged.
            Issue {
                identifier: "AIE-2".into(),
                linked_prs: Some(vec![linked("o", "r", 71, true)]),
                ..Default::default()
            },
            // Right repo, unmerged, but no summons on THAT PR number.
            Issue {
                identifier: "AIE-3".into(),
                linked_prs: Some(vec![linked("o", "r", 99, false)]),
                ..Default::default()
            },
        ];
        let events = captured(|| {
            let by_pr = by_pr.clone();
            let issues = issues.clone();
            async move {
                let _ = apply_github_summons(issues, &by_pr, "o", "r");
            }
        })
        .await;

        let f = only(&events, "github-summons: applied PR summons");
        assert_eq!(f.get("repo").map(String::as_str), Some("o/r"));
        assert_eq!(f.get("hits").map(String::as_str), Some("1"));
        assert_eq!(f.get("issues").map(String::as_str), Some("3"));
        assert_eq!(f.get("linked_prs_total").map(String::as_str), Some("3"));
        assert_eq!(f.get("skipped_other_repo").map(String::as_str), Some("1"));
        assert_eq!(f.get("skipped_merged").map(String::as_str), Some("1"));
        assert_eq!(f.get("skipped_no_hit").map(String::as_str), Some("1"));
        assert_eq!(
            f.get("matched").map(String::as_str),
            Some("0"),
            "hits found, none applied — the STUDIO-574 signature"
        );
        assert_eq!(f.get("advanced").map(String::as_str), Some("0"));
    }

    // STUDIO-574: an issue whose linked PRs ALL sit outside the polled repo can never be re-engaged
    // by a GitHub summons. That routing fault is named at info, once per issue, not swallowed.
    #[tokio::test]
    async fn apply_names_issue_whose_prs_are_all_in_another_repo() {
        let by_pr = hits(&[(71, utc(2026, 8, 24, 21, 48, 32))]);
        let issues = vec![Issue {
            identifier: "STUDIO-569".into(),
            linked_prs: Some(vec![linked("studio49dev", "studio-infra", 71, false)]),
            ..Default::default()
        }];
        let events = captured(|| {
            let by_pr = by_pr.clone();
            let issues = issues.clone();
            async move {
                let _ = apply_github_summons(issues, &by_pr, "studio49dev", "other-repo");
            }
        })
        .await;

        let f = only(
            &events,
            "github-summons: issue's linked PRs are all in another repo; summons on them can never re-engage it",
        );
        assert_eq!(
            f.get("issue_identifier").map(String::as_str),
            Some("STUDIO-569")
        );
        assert_eq!(
            f.get("polled_repo").map(String::as_str),
            Some("studio49dev/other-repo")
        );
        assert_eq!(
            f.get("pr_repos").map(String::as_str),
            Some("studio49dev/studio-infra"),
            "the line must name the repo the PRs actually live in, not just the count"
        );
        assert_eq!(f.get("linked_prs").map(String::as_str), Some("1"));
    }

    // …and a correctly-routed issue must NOT trip that line (it is an operator-actionable fault, so
    // a false positive would be worse than silence).
    #[tokio::test]
    async fn apply_does_not_name_a_correctly_routed_issue() {
        let summon = utc(2026, 8, 24, 21, 48, 32);
        let by_pr = hits(&[(71, summon)]);
        let issues = vec![Issue {
            identifier: "STUDIO-569".into(),
            linked_prs: Some(vec![linked("studio49dev", "studio-infra", 71, false)]),
            ..Default::default()
        }];
        let events = captured(|| {
            let by_pr = by_pr.clone();
            let issues = issues.clone();
            async move {
                let got = apply_github_summons(issues, &by_pr, "studio49dev", "studio-infra");
                assert_eq!(got[0].latest_summon_at, Some(summon));
            }
        })
        .await;

        assert_eq!(
            crate::testsupport::count_messages(
                &events,
                "github-summons: issue's linked PRs are all in another repo; summons on them can never re-engage it"
            ),
            0,
            "a correctly-routed issue must not be flagged"
        );
        let f = only(&events, "github-summons: applied PR summons");
        assert_eq!(f.get("matched").map(String::as_str), Some("1"));
        assert_eq!(f.get("advanced").map(String::as_str), Some("1"));
    }

    /// A minimal claude WORKFLOW with a GitHub repo and github_summons on. Mirrors Go `summonsWF`
    /// (with `api_key: tok` in place of Go's `$ORCH_TEST_KEY`, per the effective tests' env-free
    /// convention — the `$VAR` indirection is covered by the config crate's resolve tests).
    const SUMMONS_WF: &str = "\
tracker:
  kind: linear
  api_key: tok
  project_slug: proj
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  github_summons: true
repo: git@github.com:acme/widget.git
agent:
  backend: claude
claude:
  command: claude
";

    fn decode_cfg(front: &str, body: &str) -> Config {
        let config: YamlMap = serde_yaml_ng::from_str(front).expect("front matter parses");
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        let decoded = decode(&def).expect("decode");
        resolve(decoded, "/tmp/wf").expect("resolve")
    }

    // Mirrors Go `TestBuildEffective_ParsesGitHubOwnerRepo`: build_effective populates gh_owner/gh_repo
    // from a GitHub remote URL and mirrors the github_summons flag onto the resolved project.
    #[test]
    fn build_effective_parses_github_owner_repo() {
        let cfg = decode_cfg(SUMMONS_WF, "Do {{ issue.identifier }}.");
        let eff = build_effective(&cfg).expect("build_effective");
        assert!(
            !eff.projects.is_empty(),
            "expected at least one resolved project"
        );
        let p = &eff.projects[0];
        assert_eq!(p.gh_owner, "acme");
        assert_eq!(p.gh_repo, "widget");
        assert!(
            p.github_summons,
            "github_summons should mirror cfg.tracker.github_summons=true"
        );
    }
}
