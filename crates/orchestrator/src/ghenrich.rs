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

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
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
        Ok(Ok(by_pr)) => Some(by_pr),
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
/// `owner`/`repo`, considering only UNMERGED linked PRs in that repo. Pure. Mirrors Go
/// `applyGitHubSummons`. `pub` for O7's per-project apply (see [`fetch_github_summons`]).
pub fn apply_github_summons(
    mut issues: Vec<Issue>,
    by_pr: &HashMap<i64, SummonHit>,
    owner: &str,
    repo: &str,
) -> Vec<Issue> {
    if by_pr.is_empty() {
        return issues;
    }
    for iss in issues.iter_mut() {
        // Clone the PR refs out so the loop can mutate the issue's summon fields without aliasing the
        // `linked_prs` borrow (Go iterates a slice field while assigning sibling fields — legal in Go,
        // not in Rust). The list is small (an issue's linked PRs).
        let prs = iss.linked_prs.clone().unwrap_or_default();
        for pr in &prs {
            // Skip PRs from a different repo — `by_pr` only holds data for owner/repo, so a matching PR
            // number from another repo would falsely advance `latest_summon_at` and trigger a spurious
            // dispatch. GitHub owner/repo are case-insensitive, so compare case-folded (the configured
            // repo URL and the Linear attachment URL can legitimately differ in casing).
            if !pr.owner.eq_ignore_ascii_case(owner) || !pr.repo.eq_ignore_ascii_case(repo) {
                continue;
            }
            if pr.merged {
                continue;
            }
            let Some(hit) = by_pr.get(&pr.number) else {
                continue;
            };
            if iss.latest_summon_at.is_none_or(|current| hit.at > current) {
                iss.latest_summon_at = Some(hit.at);
                iss.latest_summon_body = hit.body.clone();
                tracing::info!(issue_identifier = %iss.identifier, pr = pr.number, at = %hit.at, "github-summons: advanced latest_summon_at from PR comment");
            }
        }
    }
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
