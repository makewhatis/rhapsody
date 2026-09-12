//! runautomerge — the auto-merge's OFF-LOOP half: ask GitHub everything the control task could
//! not, and perform the one bounded merge (STUDIO-874).
//!
//! **No Go v0.4.0 counterpart.** [`crate::runmerge`]'s sibling, and deliberately its shape: every
//! `gh` call is out here, the module holds no [`Orchestrator`](crate::orchestrator::Orchestrator),
//! sends no control event and takes no lock the control task takes, so a slow `gh` delays the
//! watcher's next tick and nothing else. The DECISION that needs loop state — the head-keyed
//! reviewer verdicts — was made in [`crate::automerge`] and arrives here already made, as an
//! [`AutoMergePlan`].
//!
//! # Why this does not arm GitHub's own auto-merge
//!
//! [`crate::runmerge`] merges with `--auto` and calls that a guardrail, which it is for a HUMAN
//! click: the operator has decided, and GitHub holds the merge until the checks pass. Here it
//! would be the opposite of a guardrail. Nobody is watching, so an armed auto-merge that GitHub
//! fires LATER fires at whatever head exists then — including one pushed after the arming, which
//! no reviewer approved. An armed auto-merge landing on somebody else's later push is a hazard
//! this installation has already seen.
//!
//! So this half verifies green ITSELF, at a named commit, and merges immediately or not at all.
//! The merge carries `--match-head-commit`, so if the author pushed between the observation and
//! the merge, GITHUB refuses it rather than this daemon noticing afterwards.
//!
//! # Only `CLEAN` proceeds
//!
//! The `mergeStateStatus` gate is an ALLOWLIST of one. GitHub's vocabulary here is open and has
//! grown before, and every other value it currently spells is a reason not to merge — `DRAFT`,
//! `DIRTY` (a conflict), `BLOCKED`, `UNSTABLE`, `UNKNOWN` (GitHub is still computing). A blocklist
//! of the ones known today would merge on the one added tomorrow, which is this batch's signature
//! defect: a guard that does not guard. `BEHIND` is the single value with a branch of its own,
//! because it is the one that can be CLEARED — see below.
//!
//! The check rollup is then read anyway, at the same head, even though `CLEAN` already means
//! GitHub's required contexts passed. Two independent reads of "is it green" is the ticket's
//! explicit ask (the slow `desktop` job among them), and they fail in different ways: `CLEAN` is
//! GitHub's judgement about REQUIRED contexts under branch protection, while the rollup is every
//! check that ran. A required context nobody marked required yet is invisible to the first and
//! visible to the second.
//!
//! # BEHIND updates and re-gates; it never merges on a stale approval
//!
//! STUDIO-784 is this bug already shipped once: the console armed an auto-merge on a BEHIND branch
//! that could never land. A BEHIND branch here is never merged — the approval on record is for a
//! commit that has not been merged with its base, so it is not an approval of what would land.
//! Instead the branch is UPDATED (when the repository permits it), which advances the head, which
//! re-arms the review rows through [`crate::reviewwatch`]'s existing head-advance signal. The next
//! round approves the new head or does not, and only then can this gate clear again.
//!
//! That loop is bounded by the machinery it rides rather than by a counter of its own: a re-review
//! costs a dispatch, and [`REVIEW_ROUNDS_PER_PR_CAP`](crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP)
//! caps the dispatches one pull request may draw. A repository that will not update its own
//! branches, or whose policy cannot be read, is DECLINED — never merged — which is the same
//! direction [`crate::runmerge`] fails in and for the same reason.

use std::sync::Arc;

use crate::automerge::AutoMergePlan;
use crate::ghsummons::{
    BranchUpdateSource, BranchUpdater, HeadAllowlist, MERGE_STATE_BEHIND, MergeMethod, MergeSource,
    MergeStateSource, PrChecksSource, PrLookup, PrStateSource, PrStatus,
};

/// How an auto-merge merges. `--squash` is the repository's convention — the squash subject is the
/// pull-request title release-please parses — matching [`crate::runmerge::MERGE_METHOD`].
pub const AUTO_MERGE_METHOD: MergeMethod = MergeMethod::Squash;

/// The one `mergeStateStatus` an auto-merge proceeds on. See the module doc: an allowlist, because
/// GitHub's vocabulary is open.
pub const MERGE_STATE_CLEAN: &str = "CLEAN";

/// Check conclusions that do not BLOCK a merge.
///
/// `SKIPPED` and `NEUTRAL` are non-failures — a path-filtered job reports one and would otherwise
/// block a pull request forever. They are safe to admit here only because
/// [`MERGE_STATE_CLEAN`] has already been required, which is GitHub's own verdict that every
/// REQUIRED context is satisfied; this list judges the rest. Everything else — `FAILURE`,
/// `CANCELLED`, `TIMED_OUT`, `ACTION_REQUIRED`, `IN_PROGRESS`, `QUEUED`, `PENDING`, and any state
/// GitHub adds later — blocks.
const NON_BLOCKING_CHECKS: [&str; 3] = ["SUCCESS", "SKIPPED", "NEUTRAL"];

/// Everything the off-loop half runs against. No `Orchestrator`, no store, no control channel —
/// the off-loop guarantee, in the type.
pub struct AutoMergeDeps {
    /// Re-resolves the pull request: is it still open, and is it still at the head the verdicts
    /// were recorded against?
    pub prs: Arc<dyn PrStateSource>,
    pub mergestate: Arc<dyn MergeStateSource>,
    /// Whether the REPOSITORY permits a branch update — read only when the state is `BEHIND`, so
    /// an ordinary merge pays for no extra round trip.
    pub policy: Arc<dyn BranchUpdateSource>,
    /// Performs that update. A different seam from [`AutoMergeDeps::policy`] so that reading the
    /// policy cannot perform the write.
    pub updater: Arc<dyn BranchUpdater>,
    pub checks: Arc<dyn PrChecksSource>,
    pub merger: Arc<dyn MergeSource>,
    /// The head repositories a watched pull request may come from besides the base's own owner.
    pub allow: HeadAllowlist,
}

/// What one auto-merge attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeOutcome {
    /// It merged. Carries `gh`'s own words, for the audit record.
    Merged(String),
    /// The branch was BEHIND and has been brought up to date. Nothing merged: the head has moved,
    /// so the review rows re-arm and a later tick re-gates at the new head.
    Updated,
    /// A gate refused. The reason is a fixed phrase, so the same refusal reads identically in the
    /// log every time it happens.
    Declined(&'static str),
    /// A lookup could not be MADE. Distinct from a refusal: nothing is known, so nothing is
    /// concluded and the next tick asks again.
    Failed(String),
}

/// Runs every gate that needs GitHub and, if they all clear, merges `plan`'s pull request.
///
/// The order is cheapest-refusal-first and, more importantly, safest-first: the pull request is
/// re-resolved before anything else, so a merged, closed or MOVED head costs one call and stops.
pub async fn perform_auto_merge(plan: &AutoMergePlan, deps: &AutoMergeDeps) -> AutoMergeOutcome {
    let (owner, repo, number) = (&plan.pr.owner, &plan.pr.repo, plan.pr.number);

    // 1. Is this still the pull request the verdicts were about? The control task decided from a
    //    watch-set snapshot and an observation from earlier in the tick; both can be stale by now.
    let snap = match deps.prs.pr_state(owner, repo, number, &deps.allow).await {
        Ok(PrLookup::Found(snap)) => snap,
        Ok(PrLookup::Gone) => return AutoMergeOutcome::Declined("the pull request is gone"),
        Ok(PrLookup::Untrusted) => {
            return AutoMergeOutcome::Declined("the head repository is not trusted");
        }
        Err(e) => return AutoMergeOutcome::Failed(e.to_string()),
    };
    if snap.status != PrStatus::Open {
        return AutoMergeOutcome::Declined("the pull request is no longer open");
    }
    // The head moved between the observation and now, so every verdict on record is about a commit
    // that is no longer what would land. `--match-head-commit` below would catch this too; catching
    // it here spends no merge attempt and says so precisely.
    if !snap.head_sha.eq_ignore_ascii_case(&plan.head) {
        return AutoMergeOutcome::Declined("the head moved after the verdicts were read");
    }

    // 2. GitHub's own verdict on whether this can merge at all.
    let state = match deps.mergestate.merge_state(owner, repo, number).await {
        Ok(state) => state,
        Err(e) => return AutoMergeOutcome::Failed(e.to_string()),
    };
    if state == MERGE_STATE_BEHIND {
        return update_behind_branch(plan, deps).await;
    }
    if state != MERGE_STATE_CLEAN {
        // Every other value, including one this daemon has never seen. Named in the log rather
        // than in the refusal, which is a fixed phrase.
        tracing::info!(
            pr = %plan.pr, %state,
            "auto-merge: declining a pull request GitHub does not report as CLEAN"
        );
        return AutoMergeOutcome::Declined("GitHub does not report the pull request as mergeable");
    }

    // 3. The checks, read at the same head. `CLEAN` covers the REQUIRED contexts; this covers the
    //    rest, and is the ticket's explicit second opinion on "green".
    let checks = match deps.checks.pr_checks(owner, repo, number).await {
        Ok(checks) => checks,
        Err(e) => return AutoMergeOutcome::Failed(e.to_string()),
    };
    if checks.is_empty() {
        // Not "no checks, so nothing failed": an empty rollup is no EVIDENCE of green, and this
        // repository always runs several. Fails closed.
        return AutoMergeOutcome::Declined("no checks have reported on this head");
    }
    if let Some(bad) = checks
        .iter()
        .find(|c| !NON_BLOCKING_CHECKS.contains(&c.state.as_str()))
    {
        tracing::info!(
            pr = %plan.pr, check = %bad.name, state = %bad.state,
            "auto-merge: declining on a check that is not green"
        );
        return AutoMergeOutcome::Declined("a check is failing or has not finished");
    }

    // 4. Merge, pinned to the head every verdict was recorded against.
    match deps
        .merger
        .merge_pr(
            owner,
            repo,
            number,
            AUTO_MERGE_METHOD,
            // Never GitHub's own auto-merge: see the module doc. Green is verified here, at a
            // named commit, and the merge happens now or not at all.
            false,
            Some(&plan.head),
        )
        .await
    {
        Ok(said) => {
            tracing::info!(
                pr = %plan.pr, head = %plan.head, approved_by = ?plan.approved_by,
                "auto-merge: merged"
            );
            AutoMergeOutcome::Merged(said)
        }
        Err(e) => AutoMergeOutcome::Failed(e.to_string()),
    }
}

/// The `BEHIND` branch: update it if the repository permits, and never merge it.
async fn update_behind_branch(plan: &AutoMergePlan, deps: &AutoMergeDeps) -> AutoMergeOutcome {
    let (owner, repo, number) = (&plan.pr.owner, &plan.pr.repo, plan.pr.number);
    match deps.policy.allows_branch_update(owner, repo).await {
        Ok(true) => {}
        Ok(false) => {
            return AutoMergeOutcome::Declined(
                "the branch is behind its base and the repository will not update it",
            );
        }
        // An unreadable policy is not permission. `allow_update_branch` is only in the repository
        // payload for a token with admin permission, so this read fails on perfectly healthy
        // repositories — and the refusal is true regardless of how it went: the branch IS behind.
        Err(e) => {
            tracing::warn!(
                pr = %plan.pr, err = %e,
                "auto-merge: the branch is behind its base and the repository's branch-update \
                 policy could not be read; declining"
            );
            return AutoMergeOutcome::Declined(
                "the branch is behind its base and the repository will not update it",
            );
        }
    }
    match deps.updater.update_branch(owner, repo, number).await {
        Ok(said) => {
            tracing::info!(
                pr = %plan.pr, said = %said,
                "auto-merge: the branch was behind its base and has been updated; the new head is \
                 re-reviewed before this pull request can merge"
            );
            AutoMergeOutcome::Updated
        }
        Err(e) => AutoMergeOutcome::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghsummons::{
        BranchUpdateResult, CheckRun, MergeResult, MergeStateResult, PrChecksResult, PrSnapshot,
        PrStateResult,
    };
    use crate::prstate::PrCoord;
    use async_trait::async_trait;
    use std::sync::Mutex;

    const HEAD: &str = "c366a61c366a61c366a61c366a61c366a61c366a";
    const OTHER: &str = "a324d2da324d2da324d2da324d2da324d2da324d";

    fn plan() -> AutoMergePlan {
        AutoMergePlan {
            pr: PrCoord::new("makewhatis", "tally", 151),
            head: HEAD.to_string(),
            approved_by: vec!["alice".to_string()],
        }
    }

    struct FakePrs(Option<PrLookup>);
    #[async_trait]
    impl PrStateSource for FakePrs {
        async fn pr_state(&self, _: &str, _: &str, _: i64, _: &HeadAllowlist) -> PrStateResult {
            match &self.0 {
                Some(l) => Ok(l.clone()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    fn found(head: &str, status: PrStatus) -> Arc<FakePrs> {
        Arc::new(FakePrs(Some(PrLookup::Found(PrSnapshot {
            head_sha: head.to_string(),
            status,
            merged_at: None,
            head_repo: "makewhatis/tally".to_string(),
        }))))
    }

    struct FakeState(Option<&'static str>);
    #[async_trait]
    impl MergeStateSource for FakeState {
        async fn merge_state(&self, _: &str, _: &str, _: i64) -> MergeStateResult {
            match self.0 {
                Some(s) => Ok(s.to_string()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    struct FakeChecks(Option<Vec<CheckRun>>);
    #[async_trait]
    impl PrChecksSource for FakeChecks {
        async fn pr_checks(&self, _: &str, _: &str, _: i64) -> PrChecksResult {
            match &self.0 {
                Some(c) => Ok(c.clone()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    fn checks(states: &[(&str, &str)]) -> Arc<FakeChecks> {
        Arc::new(FakeChecks(Some(
            states
                .iter()
                .map(|(n, s)| CheckRun {
                    name: (*n).to_string(),
                    state: (*s).to_string(),
                })
                .collect(),
        )))
    }

    /// The repository's six checks, all green — what a mergeable pull request looks like here.
    fn all_green() -> Arc<FakeChecks> {
        checks(&[
            ("lint", "SUCCESS"),
            ("test", "SUCCESS"),
            ("web", "SUCCESS"),
            ("desktop", "SUCCESS"),
            ("pr-title", "SUCCESS"),
            ("claude-review", "SUCCESS"),
        ])
    }

    #[derive(Default)]
    struct FakeMerger {
        calls: Mutex<Vec<(i64, MergeMethod, bool, Option<String>)>>,
        fail: Option<&'static str>,
    }
    #[async_trait]
    impl MergeSource for FakeMerger {
        async fn merge_pr(
            &self,
            _: &str,
            _: &str,
            number: i64,
            method: MergeMethod,
            auto: bool,
            match_head: Option<&str>,
        ) -> MergeResult {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                number,
                method,
                auto,
                match_head.map(str::to_string),
            ));
            match self.fail {
                Some(e) => Err(e.into()),
                None => Ok("✓ Squashed and merged pull request #151".to_string()),
            }
        }
    }

    struct FakePolicy(Option<bool>);
    #[async_trait]
    impl BranchUpdateSource for FakePolicy {
        async fn allows_branch_update(&self, _: &str, _: &str) -> BranchUpdateResult {
            match self.0 {
                Some(a) => Ok(a),
                None => Err("gh api repos/o/r: HTTP 403".into()),
            }
        }
    }

    #[derive(Default)]
    struct FakeUpdater {
        calls: Mutex<Vec<i64>>,
        fail: Option<&'static str>,
    }
    #[async_trait]
    impl BranchUpdater for FakeUpdater {
        async fn update_branch(&self, _: &str, _: &str, number: i64) -> MergeResult {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(number);
            match self.fail {
                Some(e) => Err(e.into()),
                None => Ok("✓ Updated branch".to_string()),
            }
        }
    }

    /// The deps a clean, green, up-to-date pull request sees. Tests override one member each, so
    /// what a test is ABOUT is the line it changes.
    fn deps(
        prs: Arc<FakePrs>,
        state: &'static str,
        checks: Arc<FakeChecks>,
        merger: Arc<FakeMerger>,
    ) -> AutoMergeDeps {
        AutoMergeDeps {
            prs,
            mergestate: Arc::new(FakeState(Some(state))),
            policy: Arc::new(FakePolicy(Some(true))),
            updater: Arc::new(FakeUpdater::default()),
            checks,
            merger,
            allow: HeadAllowlist::none(),
        }
    }

    /// The acceptance criterion: verdicts at head H, green CI at H, so it merges — with no human
    /// action, `--squash`, NOT GitHub's `--auto`, and pinned to H.
    #[tokio::test]
    async fn a_clean_green_approved_pull_request_merges_pinned_to_its_head() {
        let merger = Arc::new(FakeMerger::default());
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::clone(&merger),
        );

        let got = perform_auto_merge(&plan(), &d).await;

        assert_eq!(
            got,
            AutoMergeOutcome::Merged("✓ Squashed and merged pull request #151".to_string())
        );
        assert_eq!(
            merger
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![(151, MergeMethod::Squash, false, Some(HEAD.to_string()))],
            "one squash merge, GitHub's auto-merge NOT armed, pinned to the reviewed head"
        );
    }

    /// ⚠️ STUDIO-784's exact regression: a BEHIND branch must never merge on the approval it
    /// already has. It is updated instead, and the new head is re-gated by a later tick.
    #[tokio::test]
    async fn a_behind_branch_updates_and_never_merges() {
        let merger = Arc::new(FakeMerger::default());
        let updater = Arc::new(FakeUpdater::default());
        let d = AutoMergeDeps {
            updater: Arc::clone(&updater) as Arc<dyn BranchUpdater>,
            ..deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_BEHIND,
                all_green(),
                Arc::clone(&merger),
            )
        };

        let got = perform_auto_merge(&plan(), &d).await;

        assert_eq!(got, AutoMergeOutcome::Updated);
        assert_eq!(
            updater
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![151],
            "the branch was brought up to date"
        );
        assert!(
            merger
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "a BEHIND pull request is never merged, however green it looks"
        );
    }

    /// A repository that will not update its own branches cannot clear BEHIND, so the pull request
    /// is declined — never merged, and never updated either.
    #[tokio::test]
    async fn a_behind_branch_a_repository_will_not_update_is_declined() {
        for policy in [Some(false), None] {
            let merger = Arc::new(FakeMerger::default());
            let updater = Arc::new(FakeUpdater::default());
            let d = AutoMergeDeps {
                policy: Arc::new(FakePolicy(policy)),
                updater: Arc::clone(&updater) as Arc<dyn BranchUpdater>,
                ..deps(
                    found(HEAD, PrStatus::Open),
                    MERGE_STATE_BEHIND,
                    all_green(),
                    Arc::clone(&merger),
                )
            };

            let got = perform_auto_merge(&plan(), &d).await;

            assert_eq!(
                got,
                AutoMergeOutcome::Declined(
                    "the branch is behind its base and the repository will not update it"
                ),
                "({policy:?})"
            );
            assert!(
                updater
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty()
                    && merger
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty(),
                "({policy:?})"
            );
        }
    }

    /// ⚠️ The race the pin exists for, caught one call earlier: the author pushed between the
    /// observation and now, so every verdict on record is about a commit that would not land.
    #[tokio::test]
    async fn a_head_that_moved_after_the_verdicts_is_declined() {
        let merger = Arc::new(FakeMerger::default());
        let d = deps(
            found(OTHER, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::clone(&merger),
        );

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined("the head moved after the verdicts were read")
        );
        assert!(
            merger
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    /// Only `CLEAN` proceeds. A draft, a conflict, a blocked pull request and a state this daemon
    /// has never heard of are all refused by the same allowlist — which is the point: the value
    /// GitHub adds next year is refused too.
    #[tokio::test]
    async fn only_a_clean_merge_state_proceeds() {
        for state in [
            "DRAFT",
            "DIRTY",
            "BLOCKED",
            "UNSTABLE",
            "UNKNOWN",
            "HAS_HOOKS",
            "",
            "SPARKLY",
        ] {
            let merger = Arc::new(FakeMerger::default());
            let d = AutoMergeDeps {
                mergestate: Arc::new(FakeState(Some(state))),
                ..deps(
                    found(HEAD, PrStatus::Open),
                    MERGE_STATE_CLEAN,
                    all_green(),
                    Arc::clone(&merger),
                )
            };

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined("GitHub does not report the pull request as mergeable"),
                "({state})"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "({state})"
            );
        }
    }

    /// A failing or still-running check blocks, whatever GitHub says about mergeability — the slow
    /// `desktop` job included, which is exactly the one a merge is tempted to race.
    #[tokio::test]
    async fn a_check_that_is_not_green_declines() {
        for state in [
            "FAILURE",
            "IN_PROGRESS",
            "QUEUED",
            "PENDING",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "",
        ] {
            let merger = Arc::new(FakeMerger::default());
            let d = deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                checks(&[
                    ("lint", "SUCCESS"),
                    ("test", "SUCCESS"),
                    ("web", "SUCCESS"),
                    ("desktop", state),
                ]),
                Arc::clone(&merger),
            );

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined("a check is failing or has not finished"),
                "({state})"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "({state})"
            );
        }
    }

    /// An empty rollup is no evidence of green, so it is refused rather than read as "nothing
    /// failed".
    #[tokio::test]
    async fn a_pull_request_with_no_checks_is_declined() {
        let merger = Arc::new(FakeMerger::default());
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            checks(&[]),
            Arc::clone(&merger),
        );

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined("no checks have reported on this head")
        );
        assert!(
            merger
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    /// A skipped or neutral job does not block. Admissible only because `CLEAN` above is GitHub's
    /// own verdict that every REQUIRED context is satisfied.
    #[tokio::test]
    async fn a_skipped_or_neutral_check_does_not_block() {
        let merger = Arc::new(FakeMerger::default());
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            checks(&[
                ("lint", "SUCCESS"),
                ("desktop", "SKIPPED"),
                ("codeql", "NEUTRAL"),
            ]),
            Arc::clone(&merger),
        );

        assert!(matches!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Merged(_)
        ));
        assert_eq!(
            merger.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );
    }

    /// A pull request that closed or merged under the daemon's feet is not merged again
    /// (STUDIO-790's shape: an action offered on work that is already finished).
    #[tokio::test]
    async fn a_pull_request_that_is_no_longer_open_is_declined() {
        for status in [PrStatus::Merged, PrStatus::Closed] {
            let merger = Arc::new(FakeMerger::default());
            let d = deps(
                found(HEAD, status),
                MERGE_STATE_CLEAN,
                all_green(),
                Arc::clone(&merger),
            );

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined("the pull request is no longer open"),
                "({status:?})"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "({status:?})"
            );
        }
    }

    /// A pull request that has vanished, and one whose head repository is not trusted, are both
    /// refused before any merge is attempted.
    #[tokio::test]
    async fn a_gone_or_untrusted_pull_request_is_declined() {
        for (lookup, want) in [
            (PrLookup::Gone, "the pull request is gone"),
            (PrLookup::Untrusted, "the head repository is not trusted"),
        ] {
            let merger = Arc::new(FakeMerger::default());
            let d = AutoMergeDeps {
                prs: Arc::new(FakePrs(Some(lookup.clone()))),
                ..deps(
                    found(HEAD, PrStatus::Open),
                    MERGE_STATE_CLEAN,
                    all_green(),
                    Arc::clone(&merger),
                )
            };

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined(want)
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty()
            );
        }
    }

    /// A lookup that could not be MADE is a failure and never a refusal: nothing is known, so
    /// nothing is concluded and nothing is merged. Each of the three reads fails this way.
    #[tokio::test]
    async fn an_unreadable_lookup_fails_rather_than_merging() {
        let cases: Vec<(&str, Box<dyn Fn(Arc<FakeMerger>) -> AutoMergeDeps>)> = vec![
            (
                "pr state",
                Box::new(|m| AutoMergeDeps {
                    prs: Arc::new(FakePrs(None)),
                    ..deps(
                        found(HEAD, PrStatus::Open),
                        MERGE_STATE_CLEAN,
                        all_green(),
                        m,
                    )
                }),
            ),
            (
                "merge state",
                Box::new(|m| AutoMergeDeps {
                    mergestate: Arc::new(FakeState(None)),
                    ..deps(
                        found(HEAD, PrStatus::Open),
                        MERGE_STATE_CLEAN,
                        all_green(),
                        m,
                    )
                }),
            ),
            (
                "checks",
                Box::new(|m| {
                    deps(
                        found(HEAD, PrStatus::Open),
                        MERGE_STATE_CLEAN,
                        Arc::new(FakeChecks(None)),
                        m,
                    )
                }),
            ),
        ];
        for (what, build) in cases {
            let merger = Arc::new(FakeMerger::default());
            let got = perform_auto_merge(&plan(), &build(Arc::clone(&merger))).await;
            assert!(
                matches!(got, AutoMergeOutcome::Failed(_)),
                "{what}: {got:?}"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "{what}"
            );
        }
    }

    /// GitHub refusing the merge is reported, not swallowed — including the refusal the pin
    /// itself produces when the head moved inside the last window.
    #[tokio::test]
    async fn a_refused_merge_is_reported() {
        let merger = Arc::new(FakeMerger {
            calls: Mutex::new(Vec::new()),
            fail: Some("gh pr merge: Head branch was modified. Review and try the merge again."),
        });
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::clone(&merger),
        );

        assert!(matches!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Failed(e) if e.contains("Head branch was modified")
        ));
    }
}
