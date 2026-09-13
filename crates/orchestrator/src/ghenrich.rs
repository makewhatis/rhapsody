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
//!   * STUDIO-882 gives [`apply_github_summons`] a SECOND source of the PR→ticket mapping Go only
//!     ever read off tracker attachments — [`DaemonPrLinks`], the daemon's own review watch set —
//!     because on a repository the tracker's GitHub integration is not connected to there is no
//!     attachment the daemon can write that `linked_prs` will accept. Additive: an empty index
//!     leaves the pass byte-identical to Go's.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_core::Issue;

use crate::ghsummons::{SummonHit, SummonSource};

/// Bounds the `gh` exec per enrichment call. 15s is well under the 30s poll interval; a
/// network-stalled `gh` subprocess cannot wedge the control loop longer than this. Mirrors Go
/// `ghSummonsTimeout`.
pub(crate) const GH_SUMMONS_TIMEOUT: Duration = Duration::from_secs(15);

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

// ─── the drop nobody was reading (STUDIO-875) ──────────────────────────────────────────────────

/// What one [`apply_github_summons`] pass produced: the enriched issues, and every ticket a hit
/// could not be attributed to for want of a linked pull request.
///
/// The two halves are separated because they have different owners. The enrichment is pure and
/// belongs to the poll path; the drop report is a MISCONFIGURATION finding, and whether it is worth
/// saying out loud depends on the ticket's state and on what has already been said — neither of
/// which this function has. See [`report_unlinked_summons`].
#[derive(Debug, Default)]
pub struct SummonApply {
    /// The issues, enriched exactly as before.
    pub issues: Vec<Issue>,
    /// Tickets the polled repo's summons hits could not reach. See [`UnlinkedSummons`].
    pub unlinked: Vec<UnlinkedSummons>,
}

/// A ticket that the polled repository's summons hits could not be attributed to, because NEITHER
/// source has an unmerged pull request of its there.
///
/// This is STUDIO-875's whole signature, and it is the one a reviewer's findings die in: the
/// comment is posted, the scanner finds it, and the walk over `linked_prs` has nothing to walk. It
/// fails IDENTICALLY to "the reviewer approved and there is nothing to do" — an idle board with an
/// open pull request — which is why it cost eleven hours with the information sitting in the log
/// the whole time.
///
/// Since STUDIO-882 the tracker's `linked_prs` is only one of the two sources, and the daemon's own
/// [`DaemonPrLinks`] is the other — so this report now means "no link ANYWHERE", which is both
/// rarer and more actionable than what it used to mean. A ticket the daemon parked for review is
/// reachable through its own watch row whatever the tracker says, so a warning here on such a
/// ticket points at a genuinely missing record rather than at an unconnected integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkedSummons {
    /// The ticket, as a human reads it.
    pub identifier: String,
    /// Its tracker state, VERBATIM — the caller normalizes before comparing, as every other state
    /// comparison in the crate does.
    pub state: String,
    /// The POLLED REPOSITORY's pull requests with a summons hit this tick, ascending — **not this
    /// ticket's**, which has none; that is the fault being reported. Named in the warning anyway,
    /// because on an unlinked ticket they are the only handle an operator has on the comment that
    /// was dropped, and the field name says whose they are so `STUDIO-900 repo_prs=[154]` cannot be
    /// read as "154 belongs to STUDIO-900".
    ///
    /// Deliberately NOT part of the report memo's key: see [`SummonDropLog`].
    pub repo_prs: Vec<i64>,
}

/// Remembers which tickets have already been reported, so the warning fires ONCE per ticket rather
/// than on every poll.
///
/// That is not a nicety. The pre-existing `linked_prs_total=0` line already said this, every ~35
/// seconds, for eleven hours, and being repeated is exactly why nobody read it — a line that fires
/// on every tick reads as background, and a misconfiguration reported as background is a
/// misconfiguration nobody acts on.
///
/// # The key is `(repo, ticket)` and nothing else
///
/// It is tempting to put [`UnlinkedSummons::repo_prs`] in the key so that "a new dropped summons"
/// re-reports. Those numbers are not the ticket's, though — the ticket has none, which is the
/// whole fault — they are every pull request in the POLLED REPOSITORY with a hit this tick, over a
/// rolling `DEFAULT_GH_LOOKBACK` window (five minutes, `loop.rs`). That set changes
/// whenever any summons anywhere in the repository lands or ages out, so keying on it re-fired the
/// warning for EVERY unlinked in-review ticket on traffic that had nothing to do with any of them:
/// two such tickets and one new summons comment cost four more warnings. The repetition this type
/// exists to prevent, at WARN instead of DEBUG.
///
/// So a ticket is reported once per repository per daemon lifetime. There is deliberately no
/// re-arm: the only honest one would be time-based, and the operator already has the line. A
/// restart clears the memo, which is also when a re-report is WANTED, since a restart means
/// somebody may have changed the configuration.
///
/// Deliberately unbounded-in-principle and bounded-in-practice: it grows by one entry per ticket
/// whose summons is being dropped, which is a population an operator is actively being told to
/// shrink.
#[derive(Debug, Default)]
pub struct SummonDropLog {
    seen: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl SummonDropLog {
    /// Whether this ticket's drop has not been reported before for this repository, recording it
    /// either way.
    fn claim(&self, repo: &str, drop: &UnlinkedSummons) -> bool {
        let key = format!("{repo}@{}", drop.identifier);
        // A poisoned lock is recovered rather than propagated: the worst a lost set costs is a
        // repeated warning, and panicking the poll path over a log memo would be absurd.
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
    }
}

/// Warns, at most once per (repository, ticket), about every dropped summons a ticket AWAITING
/// REVIEW could not be given. Returns how many warnings it emitted (the test seam; production
/// ignores it). See [`SummonDropLog`] for why the key is that and not the hits.
///
/// # Why only a review-state ticket
///
/// A ticket with no pull request that nobody has worked yet is not a misconfiguration — it is a
/// ticket in `Todo`. Warning about every candidate would put the loud line back into the
/// background it is being rescued from, in a different coat. A ticket sitting in a configured
/// REVIEW state with no linked pull request, while the repository is producing summons hits, is
/// the actual fault: it is waiting for exactly the re-engagement that can never arrive.
///
/// `review_states` is the caller's already-normalized set, so the state comparison matches every
/// other one in the crate (`normalize_state` then `contains`).
pub fn report_unlinked_summons(
    log: &SummonDropLog,
    owner: &str,
    repo: &str,
    unlinked: &[UnlinkedSummons],
    review_states: &std::collections::HashSet<String>,
) -> usize {
    let slug = format!("{owner}/{repo}");
    let mut warned = 0usize;
    for drop in unlinked {
        if !review_states.contains(&rhapsody_core::normalize_state(&drop.state)) {
            continue;
        }
        if !log.claim(&slug, drop) {
            continue;
        }
        warned += 1;
        tracing::warn!(
            issue_identifier = %drop.identifier,
            repo = %slug,
            repo_prs = ?drop.repo_prs,
            state = %drop.state,
            "github-summons: a summons was found on this repo's pull requests but neither the \
             tracker nor this daemon links one to this ticket, so it can never be re-engaged; \
             link the pull request to the ticket, or connect the repository in the tracker's \
             GitHub integration"
        );
    }
    warned
}

// ─── the link the tracker will not classify (STUDIO-882) ───────────────────────────────────────

/// The pull requests THIS DAEMON recorded against each ticket, as a second source of the PR→ticket
/// mapping `apply_github_summons` needs — one that does not depend on the tracker classifying
/// anything.
///
/// # Why a second source exists at all
///
/// STUDIO-875 had the daemon write the missing link into the tracker with `attachmentLinkGitHubPR`,
/// on the stated ground that the mutation, not the caller, decides the attachment's `sourceType`
/// and that the GitHub-specific mutation therefore yields `sourceType: "github"`. Read back from
/// the live API, a daemon-written attachment on a repository whose GitHub integration is NOT
/// connected answers:
///
/// ```text
/// sourceType: "api"      metadata: {}
/// ```
///
/// against an integration-written one on a connected repository:
///
/// ```text
/// sourceType: "github"   metadata: { url, number, status, mergedAt, … }
/// ```
///
/// So the write lands, Linear shows it, and it fails
/// [`is_github_pr`](rhapsody_tracker) twice over: the `sourceType` gate rejects it, and even with
/// that gate widened there is no coordinate left to read, because `linked_prs` is built by matching
/// a PR url out of `metadata.url` and `metadata` is empty. There is no write the daemon can make on
/// an unconnected repository that `linked_prs` will accept, which is why the fix is here and not in
/// the write.
///
/// # Why the watch set is the right record, and not merely an available one
///
/// A row in `rhapsody_review_watch` exists because THIS DAEMON parked a ticket in review for a pull
/// request it resolved itself, on the run's own trusted repository binding — the same provenance
/// [`crate::reviewdone`] already moves tickets on, and a stricter one than an attachment, which any
/// account with tracker access can write. `introduced_by` names the ticket as `handoff:<id>` /
/// `adopt:<id>`, and [`crate::reviewdone::origin_ticket`] is the single reader of those spellings,
/// called here rather than re-implemented.
///
/// It also supplies, for free, the field the attachment could not keep honest: liveness. This index
/// is built from `load_live_review_watch`, whose rows are `open` and not `dropped`, maintained by
/// the daemon's own [`crate::prstate`] sweep. `LinkedPRRef::merged` on an unconnected repository is
/// written once and refreshed by nothing — the staleness [`crate::prlink`] documents at length —
/// whereas a merged pull request leaves this index on the sweep that observes the merge.
///
/// A `console:` origin names an operator rather than a ticket and contributes nothing, exactly as it
/// contributes no auto-done transition.
#[derive(Debug, Default, Clone)]
pub struct DaemonPrLinks {
    /// `(owner, repo, identifier)` — all upper/lower-cased for lookup — to that ticket's pull
    /// request numbers in that repository, ascending and deduplicated.
    ///
    /// Case-folded on BOTH the repository coordinate and the identifier. The repository half
    /// matches the case-insensitive guard the apply loop already applies; the identifier half is
    /// defensive — `introduced_by` is written from the issue's own identifier so the two agree
    /// byte-for-byte today, and a folded key costs nothing to make a future disagreement not be a
    /// silent drop, which is the failure mode this whole ticket is about.
    by_ticket: HashMap<(String, String, String), Vec<i64>>,
}

impl DaemonPrLinks {
    /// Builds the index from a watch-set snapshot — normally
    /// [`Store::load_live_review_watch`](rhapsody_store::Store::load_live_review_watch), so every
    /// row is already open and not dropped.
    ///
    /// Rows whose origin names no ticket are skipped. A pull request watched by several reviewers
    /// has one row each and contributes its number once.
    pub fn from_watch_rows(rows: &[rhapsody_store::ReviewWatchRow]) -> Self {
        let mut by_ticket: HashMap<(String, String, String), Vec<i64>> = HashMap::new();
        for row in rows {
            let Some(identifier) = crate::reviewdone::origin_ticket(&row.introduced_by) else {
                continue;
            };
            let key = (
                row.key.owner.to_ascii_lowercase(),
                row.key.repo.to_ascii_lowercase(),
                identifier.to_ascii_uppercase(),
            );
            by_ticket.entry(key).or_default().push(row.key.number);
        }
        for numbers in by_ticket.values_mut() {
            numbers.sort_unstable();
            numbers.dedup();
        }
        Self { by_ticket }
    }

    /// This ticket's daemon-recorded pull requests in `owner`/`repo`, ascending; empty when there
    /// are none.
    fn numbers_for(&self, owner: &str, repo: &str, identifier: &str) -> &[i64] {
        self.by_ticket
            .get(&(
                owner.to_ascii_lowercase(),
                repo.to_ascii_lowercase(),
                identifier.to_ascii_uppercase(),
            ))
            .map_or(&[], Vec::as_slice)
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
///
/// STUDIO-882: `links` is the daemon's OWN record of which pull request belongs to which ticket,
/// consulted in addition to the tracker's `linked_prs` because on a repository the tracker's GitHub
/// integration is not connected to, `linked_prs` is empty and no write can fill it. Pass
/// `&DaemonPrLinks::default()` for the tracker-only behaviour. See [`DaemonPrLinks`].
pub fn apply_github_summons(
    mut issues: Vec<Issue>,
    by_pr: &HashMap<i64, SummonHit>,
    owner: &str,
    repo: &str,
    links: &DaemonPrLinks,
) -> SummonApply {
    if by_pr.is_empty() {
        return SummonApply {
            issues,
            unlinked: Vec::new(),
        };
    }
    // STUDIO-574 observability: every drop below is a `continue` with no trace, so a hit that never
    // reaches an issue is invisible. Count each drop REASON and emit one line per call, so
    // "hits found, none applied" is distinguishable from "no hits" — and says WHY.
    let issue_count = issues.len();
    let (mut linked, mut other_repo, mut merged, mut no_hit, mut matched, mut advanced) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    // STUDIO-882: how many pull requests the DAEMON's own record contributed that the tracker did
    // not. On an unconnected repository this is the whole of `linked_prs_total`'s missing count, and
    // reading `linked_prs_total=0 daemon_links=1 matched=1` is how an operator tells "the tracker
    // still classifies nothing, and it no longer matters" from "the fix is not running".
    let mut daemon_linked = 0usize;
    // The pull requests the hits are ON, named on every drop report so the warning can point at
    // the comment somebody actually wrote. Sorted for a stable line across polls.
    let mut hit_prs: Vec<i64> = by_pr.keys().copied().collect();
    hit_prs.sort_unstable();
    let mut unlinked: Vec<UnlinkedSummons> = Vec::new();
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
        // Whether this issue has ANY pull request a summons on this repo could have reached — an
        // unmerged one, in the polled repo. STUDIO-875: when it stays false the hits had nowhere to
        // land on this ticket, and that is a misconfiguration rather than a quiet no-op.
        let mut iss_reachable = false;
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
            iss_reachable = true;
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
        // STUDIO-882: the same walk over the pull requests the DAEMON recorded for this ticket in
        // this repository, for the ones the tracker did not already supply. Separate from the loop
        // above rather than folded into it so the ported walk stays byte-identical to Go's, and
        // because the two sources answer different questions: that one asks what the tracker
        // believes, this one asks what this daemon did.
        //
        // No repo guard and no merged check are needed here and their absence is not an oversight:
        // the index is keyed BY repository, and it is built from live watch rows, so a foreign or a
        // merged pull request is not in it to begin with. See [`DaemonPrLinks`].
        for number in links.numbers_for(owner, repo, &iss.identifier) {
            if prs.iter().any(|pr| {
                pr.number == *number
                    && pr.owner.eq_ignore_ascii_case(owner)
                    && pr.repo.eq_ignore_ascii_case(repo)
            }) {
                continue; // the tracker already offered this one; the loop above handled it
            }
            daemon_linked += 1;
            iss_reachable = true;
            let Some(hit) = by_pr.get(number) else {
                no_hit += 1;
                continue;
            };
            matched += 1;
            if iss.latest_summon_at.is_none_or(|current| hit.at > current) {
                advanced += 1;
                iss.latest_summon_at = Some(hit.at);
                iss.latest_summon_body = hit.body.clone();
                tracing::info!(issue_identifier = %iss.identifier, pr = number, at = %hit.at, "github-summons: advanced latest_summon_at from PR comment (daemon-recorded link)");
            }
        }
        // Every linked PR on this issue lives outside the repo we polled, so no summons on any of
        // them can EVER reach this ticket — a routing fault (the ticket's project points at a
        // different repo than its PRs), not a quiet no-op. INFO because it is the one drop reason an
        // operator must act on, and it cannot fire for a correctly-routed ticket.
        //
        // `!iss_reachable` (STUDIO-882) so a ticket the daemon's own links DID reach is not also
        // accused of pointing at the wrong repository. It changes nothing for a tracker-only
        // ticket: if every tracker link is foreign then nothing set `iss_reachable` in the loop
        // above, and only a daemon link in the POLLED repo can have set it since.
        let all_in_another_repo =
            iss_other_repo > 0 && iss_other_repo == prs.len() && !iss_reachable;
        if all_in_another_repo {
            tracing::info!(
                issue_identifier = %iss.identifier,
                polled_repo = %format!("{owner}/{repo}"),
                pr_repos = %iss_pr_repos.join(", "),
                linked_prs = prs.len(),
                "github-summons: issue's linked PRs are all in another repo; summons on them can never re-engage it"
            );
        }
        // STUDIO-875: this repo produced summons hits and this ticket has no pull request in it
        // that one could have reached. Reported rather than logged here, because whether it is
        // WORTH saying depends on state this pure function does not have — see
        // [`report_unlinked_summons`].
        //
        // NOT reported when STUDIO-574's line above already named this ticket: that one says the
        // ticket's links are in another repository and names which, which is both truer and more
        // actionable than "it has no linked pull request". Two lines about one ticket, one of them
        // wrong, is worse than the single accurate line.
        if !iss_reachable && !all_in_another_repo {
            unlinked.push(UnlinkedSummons {
                identifier: iss.identifier.clone(),
                state: iss.state.clone(),
                repo_prs: hit_prs.clone(),
            });
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
        daemon_links = daemon_linked,
        skipped_other_repo = other_repo,
        skipped_merged = merged,
        skipped_no_hit = no_hit,
        matched,
        advanced,
        "github-summons: applied PR summons"
    );
    SummonApply { issues, unlinked }
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
    links: &DaemonPrLinks,
) -> SummonApply {
    match fetch_github_summons(src, owner, repo, since).await {
        Some(by_pr) => apply_github_summons(issues, &by_pr, owner, repo, links),
        None => SummonApply {
            issues,
            unlinked: Vec::new(),
        },
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
            &DaemonPrLinks::default(),
        )
        .await;
        assert_eq!(
            got.issues[0].latest_summon_at,
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
        let got = enrich_with_github_summons(
            issues,
            Some(&src),
            "o",
            "r",
            older,
            &DaemonPrLinks::default(),
        )
        .await;
        assert_eq!(
            got.issues[0].latest_summon_at,
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
        let got = enrich_with_github_summons(
            issues,
            Some(&src),
            "o",
            "r",
            utc(2026, 6, 25, 12, 0, 0),
            &DaemonPrLinks::default(),
        )
        .await;
        assert!(
            got.issues[0].latest_summon_at.is_none(),
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
            &DaemonPrLinks::default(),
        )
        .await;
        assert!(
            got.issues[0].latest_summon_at.is_none(),
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
            &DaemonPrLinks::default(),
        )
        .await;
        assert_eq!(
            got.issues[0].latest_summon_at,
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
                let _ = apply_github_summons(issues, &by_pr, "o", "r", &DaemonPrLinks::default());
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
                let _ = apply_github_summons(
                    issues,
                    &by_pr,
                    "studio49dev",
                    "other-repo",
                    &DaemonPrLinks::default(),
                );
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
                let got = apply_github_summons(
                    issues,
                    &by_pr,
                    "studio49dev",
                    "studio-infra",
                    &DaemonPrLinks::default(),
                );
                assert_eq!(got.issues[0].latest_summon_at, Some(summon));
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

    // ── the drop nobody was reading (STUDIO-875) ────────────────────────────────────────────────
    //
    // The acceptance criterion these pin is the FAILURE branch, not the happy path: a summon hit
    // that reaches an issue with empty `linked_prs` must be reported, once, naming the ticket and
    // the pull request. A fix whose failure branch is untested is the same defect again.

    const WARNING: &str = "github-summons: a summons was found on this repo's pull requests but \
                           neither the tracker nor this daemon links one to this ticket, so it can \
                           never be re-engaged; link the pull request to the ticket, or connect \
                           the repository in the tracker's GitHub integration";

    fn in_review(identifier: &str, prs: Vec<LinkedPRRef>) -> Issue {
        Issue {
            identifier: identifier.to_string(),
            state: "In Review".to_string(),
            linked_prs: (!prs.is_empty()).then_some(prs),
            ..Default::default()
        }
    }

    fn review_states() -> std::collections::HashSet<String> {
        crate::testsupport::set_of(&["in review"])
    }

    fn drops() -> Vec<UnlinkedSummons> {
        vec![UnlinkedSummons {
            identifier: "STUDIO-872".to_string(),
            state: "In Review".to_string(),
            repo_prs: vec![154],
        }]
    }

    /// STUDIO-875's exact shape, from the daemon's own log: `hits=1 … linked_prs_total=0
    /// matched=0 advanced=0`. The hit is real, the ticket is in the set, and there is nothing to
    /// match it against.
    #[test]
    fn a_hit_against_an_issue_with_no_linked_prs_is_reported() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review("STUDIO-872", vec![])],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert_eq!(
            got.unlinked,
            vec![UnlinkedSummons {
                identifier: "STUDIO-872".to_string(),
                state: "In Review".to_string(),
                repo_prs: vec![154],
            }],
            "the drop names the ticket AND the pull request the comment is on"
        );
        assert_eq!(got.issues[0].latest_summon_at, None, "and nothing advanced");
    }

    /// The link working is the whole point; a ticket whose pull request IS attached is not a fault.
    #[test]
    fn a_linked_issue_is_not_reported() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review(
                "STUDIO-872",
                vec![linked("makewhatis", "rhapsody", 154, false)],
            )],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert!(got.unlinked.is_empty());
        assert!(got.issues[0].latest_summon_at.is_some(), "it matched");
    }

    /// A linked pull request with NO summons on it is not a broken link — the ticket is reachable,
    /// nobody has summoned. Reporting it would be the every-poll noise in a different coat.
    #[test]
    fn a_linked_issue_with_no_hit_of_its_own_is_not_reported() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review(
                "STUDIO-871",
                vec![linked("makewhatis", "rhapsody", 155, false)],
            )],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert!(got.unlinked.is_empty());
    }

    /// A merged attachment is invisible to the walk above it, so it must be invisible here: the
    /// ticket is, for summons purposes, unlinked.
    #[test]
    fn an_issue_whose_only_link_is_merged_is_reported() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review(
                "STUDIO-872",
                vec![linked("makewhatis", "rhapsody", 154, true)],
            )],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert_eq!(got.unlinked.len(), 1);
    }

    /// STUDIO-574 already names this ticket, and names it better — "its links are all in
    /// another repo, here is which". Reporting it AGAIN as "it has no linked pull request" would
    /// put a second, untrue line beside the accurate one.
    #[test]
    fn an_issue_whose_links_are_all_in_another_repo_is_left_to_the_line_that_names_it() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review(
                "STUDIO-872",
                vec![linked("makewhatis", "tally", 246, false)],
            )],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert!(got.unlinked.is_empty());
    }

    /// …but a ticket with SOME link in another repo and nothing reachable here is not covered by
    /// that line (it only fires when EVERY link is foreign), so it is still reported.
    #[test]
    fn an_issue_with_a_foreign_link_and_a_merged_local_one_is_reported() {
        let by_pr = hits(&[(154, utc(2026, 9, 12, 5, 10, 29))]);
        let got = apply_github_summons(
            vec![in_review(
                "STUDIO-872",
                vec![
                    linked("makewhatis", "tally", 246, false),
                    linked("makewhatis", "rhapsody", 154, true),
                ],
            )],
            &by_pr,
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert_eq!(got.unlinked.len(), 1);
    }

    /// No hits, no finding: a repo nobody summoned on says nothing about anybody's links.
    #[test]
    fn an_empty_hit_map_reports_nothing() {
        let got = apply_github_summons(
            vec![in_review("STUDIO-872", vec![])],
            &HashMap::new(),
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert!(got.unlinked.is_empty());
    }

    /// The acceptance criterion in full: the drop produces a WARNING naming the ticket and the
    /// pull request — and does it exactly once, however many times the poll comes round. The old
    /// line said this every ~35 seconds for eleven hours, which is why nobody read it.
    #[tokio::test]
    async fn the_warning_names_the_ticket_and_the_pr() {
        // A fresh memo per pass, because `captured` deliberately runs its closure twice (callsite
        // warm-up) and the whole point of the memo is that the second pass is silent.
        let events = captured(|| async {
            let log = SummonDropLog::default();
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &drops(), &review_states());
        })
        .await;

        let f = only(&events, WARNING);
        assert_eq!(
            f.get("issue_identifier").map(String::as_str),
            Some("STUDIO-872")
        );
        assert_eq!(
            f.get("repo").map(String::as_str),
            Some("makewhatis/rhapsody")
        );
        assert_eq!(
            f.get("repo_prs").map(String::as_str),
            Some("[154]"),
            "the repo's hits, named so the dropped comment can be opened"
        );
        assert_eq!(
            events
                .iter()
                .find(|e| e.message == WARNING)
                .map(|e| e.level.as_str()),
            Some("WARN"),
            "a misconfiguration is a warning, not a debug line"
        );
    }

    /// Making it loud is only half the fix: the pre-existing line ALREADY said this, every ~35
    /// seconds for eleven hours, and being repeated is why nobody read it.
    #[test]
    fn the_warning_fires_once_per_ticket_not_every_poll() {
        let log = SummonDropLog::default();
        let states = review_states();
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &drops(), &states),
            1
        );
        for _ in 0..5 {
            assert_eq!(
                report_unlinked_summons(&log, "makewhatis", "rhapsody", &drops(), &states),
                0,
                "the same drop must not be reported on every poll"
            );
        }
    }

    /// A ticket nobody has started has no pull request and no fault; warning about it would put
    /// the loud line straight back into the background.
    #[test]
    fn a_ticket_that_is_not_awaiting_review_is_not_warned_about() {
        let log = SummonDropLog::default();
        let drops = vec![UnlinkedSummons {
            identifier: "STUDIO-900".to_string(),
            state: "Todo".to_string(),
            repo_prs: vec![154],
        }];
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &drops, &review_states()),
            0
        );
    }

    /// The churn the hit-set key could not survive. `UnlinkedSummons::repo_prs` is every pull
    /// request in the POLLED REPO with a summons hit this tick — not the ticket's, which has none;
    /// that is the defect — and that set is a rolling `DEFAULT_GH_LOOKBACK` window, so it changes
    /// on its own as comments age out and as summons land on unrelated pull requests. A key
    /// carrying those numbers therefore re-fired for EVERY unlinked in-review ticket on every
    /// change to it, which is the "one line every ~35 seconds" this warning exists to replace,
    /// wearing a WARN coat.
    #[test]
    fn a_changed_repo_hit_set_does_not_re_warn_the_same_ticket() {
        let log = SummonDropLog::default();
        let states = review_states();
        let with = |prs: Vec<i64>| {
            vec![UnlinkedSummons {
                repo_prs: prs,
                ..drops()[0].clone()
            }]
        };
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &with(vec![154]), &states),
            1,
            "the first drop is said out loud"
        );
        for prs in [vec![154, 160], vec![160], vec![160, 161], vec![154]] {
            assert_eq!(
                report_unlinked_summons(&log, "makewhatis", "rhapsody", &with(prs), &states),
                0,
                "a summons on an unrelated pull request is not news about THIS ticket"
            );
        }
    }

    /// Once per TICKET, and per ticket: the memo must not silence a second unlinked ticket just
    /// because the first one was reported from the same hit set.
    #[test]
    fn every_unlinked_ticket_is_warned_about_once() {
        let log = SummonDropLog::default();
        let states = review_states();
        let two = vec![
            drops()[0].clone(),
            UnlinkedSummons {
                identifier: "STUDIO-900".to_string(),
                ..drops()[0].clone()
            },
        ];
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &two, &states),
            2
        );
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &two, &states),
            0
        );
    }

    /// The same ticket in a different repository is a different fault with a different fix, so the
    /// memo is keyed on both.
    #[test]
    fn the_same_ticket_in_another_repository_is_warned_about_in_its_own_right() {
        let log = SummonDropLog::default();
        let states = review_states();
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "rhapsody", &drops(), &states),
            1
        );
        assert_eq!(
            report_unlinked_summons(&log, "makewhatis", "tally", &drops(), &states),
            1
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

    // ─── the unconnected repository (STUDIO-882) ────────────────────────────────────────────────

    /// A watch row for `owner/repo#number`, introduced by a handoff of `identifier`.
    fn watch_row(
        owner: &str,
        repo: &str,
        number: i64,
        introduced_by: &str,
    ) -> rhapsody_store::ReviewWatchRow {
        rhapsody_store::ReviewWatchRow {
            key: rhapsody_store::ReviewWatchKey {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                reviewer: "jimmy".to_string(),
            },
            introduced_by: introduced_by.to_string(),
            ..Default::default()
        }
    }

    /// A ticket exactly as an UNCONNECTED repository's tracker answers it: no attachments at all,
    /// so no `linked_prs`. This is the population STUDIO-875 targeted and could not reach — its
    /// `attachmentLinkGitHubPR` write lands with `sourceType: "api"` and `metadata: {}`, so
    /// `linked_prs` stays empty however many times the daemon writes it.
    fn unlinked_issue(identifier: &str) -> Issue {
        Issue {
            id: "i1".into(),
            identifier: identifier.into(),
            state: "In Review".into(),
            ..Default::default()
        }
    }

    /// THE regression: a summons on a pull request the tracker does not link, on a ticket the
    /// DAEMON linked, advances `latest_summon_at` — and reports no drop.
    #[test]
    fn daemon_link_attributes_a_summons_the_tracker_cannot() {
        let summon = utc(2026, 9, 13, 1, 46, 59);
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "handoff:STUDIO-880",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        assert_eq!(
            got.issues[0].latest_summon_at,
            Some(summon),
            "a daemon-recorded link must attribute the summons"
        );
        assert!(
            got.unlinked.is_empty(),
            "the ticket IS reachable now, so it must not be reported as unlinked: {:?}",
            got.unlinked
        );
    }

    /// The same ticket WITHOUT the daemon's link is still dropped and still reported — the
    /// behaviour STUDIO-875 shipped is unchanged where nothing recorded a link, so this test is
    /// what tells "the new source applied" from "the old path happened to work".
    #[test]
    fn no_daemon_link_still_drops_and_reports() {
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, utc(2026, 9, 13, 1, 46, 59))]),
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert!(got.issues[0].latest_summon_at.is_none());
        assert_eq!(got.unlinked.len(), 1, "the drop must still be reported");
        assert_eq!(got.unlinked[0].identifier, "STUDIO-880");
    }

    /// The index is per-repository: a daemon link in ANOTHER repo must not attribute a summons
    /// whose number collides, exactly as a foreign `linked_prs` entry does not.
    #[test]
    fn daemon_link_in_another_repo_is_not_applied() {
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "tally",
            159,
            "handoff:STUDIO-880",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, utc(2026, 9, 13, 1, 46, 59))]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        assert!(
            got.issues[0].latest_summon_at.is_none(),
            "a link in makewhatis/tally must not attribute a makewhatis/rhapsody summons"
        );
    }

    /// A daemon link belonging to a DIFFERENT ticket must not attribute this ticket's summons —
    /// the index is keyed by ticket, and getting that wrong would re-engage the wrong author.
    #[test]
    fn daemon_link_of_another_ticket_is_not_applied() {
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "handoff:STUDIO-881",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, utc(2026, 9, 13, 1, 46, 59))]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        assert!(got.issues[0].latest_summon_at.is_none());
    }

    /// An `adopt:` origin names a ticket and counts; a `console:` origin names an OPERATOR and
    /// contributes nothing, exactly as it moves no ticket in `reviewdone`.
    #[test]
    fn adopt_origin_counts_and_console_origin_does_not() {
        let summon = utc(2026, 9, 13, 1, 46, 59);
        let adopted = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "adopt:STUDIO-880",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &adopted,
        );
        assert_eq!(
            got.issues[0].latest_summon_at,
            Some(summon),
            "adopt: counts"
        );

        let console = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "console:david",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &console,
        );
        assert!(
            got.issues[0].latest_summon_at.is_none(),
            "console: names no ticket and must contribute no link"
        );
    }

    /// A pull request BOTH sources offer is walked once, not twice — a connected repository must
    /// behave exactly as it did before this ticket.
    #[test]
    fn a_pr_both_sources_offer_is_not_double_counted() {
        let summon = utc(2026, 9, 13, 1, 46, 59);
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "handoff:STUDIO-880",
        )]);
        let mut iss = unlinked_issue("STUDIO-880");
        iss.linked_prs = Some(vec![linked("makewhatis", "rhapsody", 159, false)]);
        let got = apply_github_summons(
            vec![iss.clone()],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        let tracker_only = apply_github_summons(
            vec![iss],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &DaemonPrLinks::default(),
        );
        assert_eq!(got.issues[0].latest_summon_at, Some(summon));
        assert_eq!(
            got.issues[0].latest_summon_at, tracker_only.issues[0].latest_summon_at,
            "a connected repository must be unaffected by the daemon's index"
        );
        assert!(got.unlinked.is_empty());
    }

    /// A ticket whose ONLY tracker link is foreign, but which the daemon linked in the polled
    /// repository, is reachable — and must not be accused of pointing at the wrong repository.
    #[test]
    fn a_daemon_link_rescues_a_ticket_whose_tracker_links_are_all_foreign() {
        let summon = utc(2026, 9, 13, 1, 46, 59);
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "makewhatis",
            "rhapsody",
            159,
            "handoff:STUDIO-880",
        )]);
        let mut iss = unlinked_issue("STUDIO-880");
        iss.linked_prs = Some(vec![linked("makewhatis", "tally", 246, false)]);
        let got = apply_github_summons(
            vec![iss],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        assert_eq!(got.issues[0].latest_summon_at, Some(summon));
        assert!(got.unlinked.is_empty());
    }

    /// Repository coordinate and ticket identifier are both matched case-insensitively, the same
    /// rule the apply loop's own repo guard applies.
    #[test]
    fn the_index_folds_case_on_both_coordinates() {
        let summon = utc(2026, 9, 13, 1, 46, 59);
        let links = DaemonPrLinks::from_watch_rows(&[watch_row(
            "MakeWhatIs",
            "Rhapsody",
            159,
            "handoff:studio-880",
        )]);
        let got = apply_github_summons(
            vec![unlinked_issue("STUDIO-880")],
            &hits(&[(159, summon)]),
            "makewhatis",
            "rhapsody",
            &links,
        );
        assert_eq!(got.issues[0].latest_summon_at, Some(summon));
    }

    /// Two reviewers watching one pull request are two rows and one link.
    #[test]
    fn two_reviewer_rows_of_one_pr_contribute_one_number() {
        let mut second = watch_row("makewhatis", "rhapsody", 159, "handoff:STUDIO-880");
        second.key.reviewer = "alice".into();
        let links = DaemonPrLinks::from_watch_rows(&[
            watch_row("makewhatis", "rhapsody", 159, "handoff:STUDIO-880"),
            second,
        ]);
        assert_eq!(
            links.numbers_for("makewhatis", "rhapsody", "STUDIO-880"),
            &[159]
        );
    }
}
