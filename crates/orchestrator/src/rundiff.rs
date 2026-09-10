//! rundiff — the console's Diff tab: the unified diff a run produced on its branch, plus the pull
//! request it produced it on (STUDIO-749; design record
//! `~/.rhapsody/docs/console-run-detail-design.md` §5, §8, §9 slice 7).
//!
//! §5 called this "the one real new endpoint" of the whole Trace plan, and deferred it: until it
//! existed, the console's Diff tab named its dependency and deep-linked to the pull request rather
//! than showing a diff nobody served. This module is that endpoint's whole daemon half.
//!
//! **No Go v0.4.0 counterpart.** The frozen Symphony reference has no console diff; this is the
//! additive Rhapsody surface the design record specifies, and it takes a README Divergences entry
//! as a new served route alongside `/api/v1/version` and the history-paging endpoints (§10).
//!
//! # What this is NOT, and the two boundaries that keep it small
//!
//! **It merges nothing, and it decides nothing about merging.** The merge action is
//! STUDIO-767's, and that record says so outright: *"the Diff tab wants a unified diff; Merge
//! wants `gh pr merge`. Nothing in this design reads a diff."* The two were bundled into slice 7
//! by accident of adjacency and have since separated. So there is no [`crate::ghsummons::MergeSource`]
//! in [`DiffDeps`], down any branch, and this module never reaches one.
//!
//! **It does not re-derive mergeability either.** `GET /api/v1/runs/{id}/mergeability` already
//! serves the daemon's verdict, resolved by [`crate::runmerge::resolve_pull_request`] — the
//! function that exists precisely so that *"the reason the header shows and the reason the merge
//! would give"* are not two implementations that can disagree. A second one here would be that
//! defect, re-introduced on a route with no reason to hold an opinion. What [`RunDiff`] carries is
//! [`RunDiff::merge_state`]: GitHub's OWN `mergeStateStatus`, a raw fact rather than a judgement,
//! which cannot contradict a verdict because it is not one.
//!
//! # Why it refuses nothing — and what it nonetheless cannot show
//!
//! Every gate on the merge path exists because a merge is irreversible. Reading a diff is not, so
//! none of them applies here and none is copied. Take an OPEN pull request the merge path would
//! turn away — one a live Rhapsody review is watching, one whose reviewer asked for changes, one
//! whose ticket is not waiting in review: each has a diff, and this module reads it. The outcome
//! vocabulary says so, with
//! [`DiffOutcome::Unavailable`] where the merge path has `Refused`: this module never denies a
//! request it could have served, it only reports that there is nothing to serve.
//!
//! **But the diff is available only while the pull request is OPEN, and that is a real limit
//! rather than a gate.** The coordinate is resolved by
//! [`crate::ghsummons::OpenPrSource::open_pr_for_branch`], which filters `--state open`, so a
//! merged or closed pull request yields no number and the read stops at
//! [`DiffOutcome::Unavailable`] — even though `gh pr diff` would serve its diff perfectly well.
//! Nothing refuses it; there is simply never a coordinate. This bites hardest on exactly the runs
//! an operator browses in history, because STUDIO-712 moves a ticket to Done when its pull request
//! merges, so a finished run's pull request is usually closed. Lifting it means resolving the
//! number without that filter, which is a change to a seam three other callers share and is not
//! this ticket's.
//!
//! What it DOES keep from the merge path is guardrail G1's shape, because the same argument holds
//! for any `gh` call the console can trigger: the coordinate is derived from the run row, never
//! supplied by the caller. [`DiffPlan`] has no field a request body could fill, the branch comes
//! from the ticket the way the daemon derives it everywhere else, and the pull-request number is
//! resolved from GitHub by HEAD BRANCH through [`crate::ghsummons::OpenPrSource::open_pr_for_branch`],
//! which rejects a fork's pull request (STUDIO-674's fork hazard).
//!
//! # Why this file holds no `Orchestrator`
//!
//! [`crate::runmerge`]'s reason, and one better. Everything here shells out through `gh`, whose
//! future has no await point the control task could be rescued at, so the containment is
//! structural: this module takes no `Orchestrator`, sends no control event and holds no lock the
//! control task takes.
//!
//! Where the merge path still needed a control round trip — for the single-flight claim, the live
//! review snapshot and the audit record — a diff read needs NONE of those, because it claims
//! nothing, gates on nothing and records nothing. So [`crate::ControlHandle::run_diff`] reads the
//! run row straight off the handle's own store (as [`crate::ControlHandle::resume_run`] already
//! does) and the control task is never involved at all. That is the whole reason this is one file
//! where the merge action is two.

use std::sync::Arc;

use serde::Serialize;

use crate::ghsummons::{
    CheckRun, HeadAllowlist, MergeStateSource, OpenPrSource, PrChecksSource, PrDiffSource,
    PrLookup, PrStateSource,
};
use crate::teamsknow::parse_pr_ref;

/// The `gh` seams the diff read drives, and the trust boundary it drives them under.
///
/// Five reads and **no writer**. Mirrors [`crate::runmerge::ResolveDeps`], which is the merge
/// path's read-only half for the same reason: a property enforced by what a task is handed beats
/// one enforced by a convention a reviewer has to keep.
pub struct DiffDeps {
    /// Resolves the run's head branch to its open pull request — the ONLY way a number enters.
    pub prs: Arc<dyn OpenPrSource>,
    /// Resolves that number's head SHA, so the diff can say which commit it is of.
    pub state: Arc<dyn PrStateSource>,
    /// GitHub's own `mergeStateStatus`, carried as a fact and never acted on here.
    pub mergestate: Arc<dyn MergeStateSource>,
    /// The status-check rollup — the "checks" of the ticket's "PR number / checks / mergeability".
    pub checks: Arc<dyn PrChecksSource>,
    /// The diff itself.
    pub diff: Arc<dyn PrDiffSource>,
    /// Head repositories trusted besides the base's own owner. [`HeadAllowlist::none`] on the
    /// daemon — the watcher's default trust boundary, and widening it is a code change.
    pub allow: HeadAllowlist,
}

/// The coordinate a diff read resolves against — **derived from the run row, and from nothing
/// else**.
///
/// [`crate::runmerge::MergePlan`] minus every field that exists to gate a merge. The absence of a
/// `number` here is the same statement it is there: there is no code path from a client-supplied
/// integer to `gh`, because there is nowhere for one to travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPlan {
    pub run_id: i64,
    /// The run's ticket, e.g. `STUDIO-749`.
    pub issue: String,
    pub owner: String,
    pub repo: String,
    /// The branch `issue` names, derived rather than read out of `runs.branch` — see
    /// [`plan_run_diff`].
    pub branch: String,
}

/// What a run's branch changed, as the console's Diff tab reads it.
///
/// The patch is the deliverable; everything above it is the answer to "of what?", which a diff
/// with no coordinate cannot give. `merge_state` and `checks` are the design record's "PR number /
/// checks / mergeability where resolvable", carried as GitHub's own values — see the module doc
/// for why mergeability is a raw `mergeStateStatus` here and a verdict only on its own route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunDiff {
    pub run_id: i64,
    pub issue: String,
    /// The branch the diff is of, e.g. `symphony/STUDIO-749`.
    pub branch: String,
    /// `owner/repo#number` — the coordinate, echoed so a log or a header can name it.
    pub pr: String,
    /// The pull request's browser URL, as GitHub gave it.
    pub url: String,
    pub number: i64,
    /// The head commit this diff is of. The one field that makes the patch citable: a diff with no
    /// SHA cannot be told apart from the same file read a push later.
    pub head_sha: String,
    /// GitHub's own `mergeStateStatus`, or empty when GitHub stated none — **and also empty when
    /// the read failed**, which is the same answer for the same reason: this is a garnish on a
    /// diff, the console renders empty as silence, and losing the whole diff over it would be the
    /// wrong trade. The failure is logged.
    pub merge_state: String,
    /// The pull request's status checks, or empty when it has none — and, as with `merge_state`,
    /// empty when the read failed rather than the whole answer being lost.
    pub checks: Vec<CheckRun>,
    /// The unified diff, `diff --git` headers and `@@` hunks exactly as `gh` printed them.
    pub patch: String,
    /// Whether [`crate::ghsummons::MAX_DIFF_BYTES`] cut `patch` short. The console says so; a diff
    /// that silently stops is one an operator reads as complete.
    pub truncated: bool,
}

/// The outcome of one console diff read.
///
/// Four variants and — deliberately — no `Refused`. See the module doc: reading a diff is
/// reversible, so this path denies nothing, and [`Unavailable`](Self::Unavailable) is the
/// different statement that there is no diff to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOutcome {
    /// No run has that id. Distinct from [`Unavailable`](Self::Unavailable) because the console
    /// renders it as a dead link rather than as a panel with nothing in it.
    NotFound,
    /// There is no diff to show, and this names why in words an operator reads. `&'static str` so
    /// it can never carry a value from GitHub or from a request body.
    Unavailable(&'static str),
    /// A `gh` seam could not answer, and this is its own complaint verbatim.
    Failed(String),
    /// The diff, and the pull request it is of. Boxed because it carries the patch, and the other
    /// three variants should not each be half a megabyte wide.
    Ready(Box<RunDiff>),
}

/// The run row's own coordinate, or the reason it has none (the loop-side half, such as it is).
///
/// Pure over the row, so what it asserts is testable without a daemon. Two things it does, both
/// borrowed from [`crate::mergeconsole::plan_run_merge`] because the argument for them is about
/// `gh` rather than about merging:
///
/// * `parse_repo` refuses a look-alike host (`evilgithub.com`, `…/github.com/…`; STUDIO-721), so
///   a remote this daemon will not vouch for never reaches `gh`.
/// * The branch is **derived** from the ticket rather than read out of `runs.branch`, which is
///   unwritten on every row this daemon has ever produced (`persist_start_run` leaves it empty and
///   nothing `UPDATE`s it). Reading the column instead would answer nothing in production while
///   passing every test that fabricates it. The cross-check survives as a DISAGREEMENT test: a row
///   that does carry a branch must agree with the one its ticket names.
pub fn plan_run_diff(run: &rhapsody_store::RunSummary) -> Result<DiffPlan, &'static str> {
    let Some((owner, repo)) = crate::ghsummons::parse_repo(&run.repo) else {
        return Err("this run has no GitHub repository, so there is no diff to read");
    };
    let want = format!(
        "symphony/{}",
        rhapsody_workspace::sanitize_key(&run.issue_identifier)
    );
    let branch = if run.branch.is_empty() {
        want.clone()
    } else {
        run.branch.clone()
    };
    if branch != want {
        tracing::warn!(
            run = run.id,
            issue = %run.issue_identifier,
            branch = %branch,
            "console diff: this run's branch does not belong to its ticket"
        );
        return Err("this run's branch does not belong to its ticket");
    }
    Ok(DiffPlan {
        run_id: run.id,
        issue: run.issue_identifier.clone(),
        owner,
        repo,
        branch,
    })
}

/// Resolves `plan`'s pull request and reads its diff. **The whole off-loop half of the Diff tab.**
///
/// The order, and why each step is where it is:
///
/// 1. **Resolve by head branch.** [`OpenPrSource::open_pr_for_branch`] filters `--state open` and
///    rejects a fork's pull request, so a stranger cannot get a coordinate in here by opening a
///    pull request whose head branch is named like this run's.
/// 2. **Cross-check the URL back against the plan.** Only the NUMBER is taken from GitHub's
///    answer; owner and repo stay the run row's, which is config-derived. A URL in another
///    ACCOUNT means an assumption is wrong, and it is not followed.
/// 3. **Ask where the pull request stands**, for the head SHA. `Gone` and `Untrusted` end the read
///    — the first because there is nothing there, the second because it is the F-SEC boundary and
///    it fails closed even for a read.
/// 4. **The garnish**: `mergeStateStatus` and the checks rollup, neither of which can end the read
///    (see [`RunDiff::merge_state`]).
/// 5. **The diff**, which is the answer, and whose failure IS a failure.
pub async fn run_diff(plan: &DiffPlan, deps: &DiffDeps) -> DiffOutcome {
    let url = match deps
        .prs
        .open_pr_for_branch(&plan.owner, &plan.repo, &plan.branch)
        .await
    {
        // Only the URL. `OpenPr` also carries `head_sha` (STUDIO-822), but the head this
        // endpoint serves a diff at is the one `pr_state` returns below, because that is the
        // lookup carrying the `Gone`/`Untrusted` refusals — taking the sha from here instead
        // would skip the boundary that vets it.
        Ok(Some(open)) => open.url,
        // Not a refusal and not an error, and it covers TWO different situations: a branch never
        // pushed, and one whose pull request was merged or closed. `open_pr_for_branch` filters
        // `--state open`, so the second has no coordinate to read a diff at even though `gh pr
        // diff` would serve one — and since STUDIO-712 moves a ticket to Done when its pull
        // request merges, the finished runs an operator browses in history are mostly that
        // second case. So the reason names it, rather than leaving a merged pull request to read
        // as a branch nobody ever pushed. The console deep-links to the branch either way, which
        // is what it did before this endpoint existed.
        Ok(None) => {
            return DiffOutcome::Unavailable(
                "this run's branch has no open pull request — a merged or closed one is not read here",
            );
        }
        Err(e) => return DiffOutcome::Failed(e.to_string()),
    };
    let Some(found) = parse_pr_ref(&url) else {
        return DiffOutcome::Unavailable("the pull request's URL could not be read");
    };
    // On the OWNER and not the whole slug, for the reason `open_pr_for_branch` gives at length: a
    // same-account fork is inside the trust boundary, and a whole-slug match would break on a
    // repository rename.
    if !found.owner.eq_ignore_ascii_case(&plan.owner) {
        tracing::warn!(
            run = plan.run_id,
            url = %url,
            want = %plan.owner,
            "console diff: the resolved pull request belongs to another account"
        );
        return DiffOutcome::Unavailable("the resolved pull request belongs to another account");
    }
    let number = found.number;
    let pr = format!("{}/{}#{number}", plan.owner, plan.repo);

    let head_sha = match deps
        .state
        .pr_state(&plan.owner, &plan.repo, number, &deps.allow)
        .await
    {
        Ok(PrLookup::Found(snap)) => snap.head_sha,
        Ok(PrLookup::Gone) => {
            return DiffOutcome::Unavailable("GitHub cannot resolve that pull request");
        }
        // The same fail-closed boundary the watcher applies, and it applies to a READ too: the
        // next thing that happens to this patch is that a browser renders it, and rendering a
        // stranger's diff under this run's ticket is the console asserting something false about
        // whose work it is.
        Ok(PrLookup::Untrusted) => {
            return DiffOutcome::Unavailable("the pull request's head repository is not this one");
        }
        Err(e) => return DiffOutcome::Failed(e.to_string()),
    };

    // Steps 4a and 4b. Both degrade to their empty value on failure, because both are context on a
    // diff rather than the diff, and both already USE empty to mean "GitHub said nothing" — so the
    // degradation lands on an answer the console already renders as silence rather than inventing
    // a third state for it.
    let merge_state = match deps
        .mergestate
        .merge_state(&plan.owner, &plan.repo, number)
        .await
    {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!(run = plan.run_id, pr = %pr, error = %e, "console diff: could not read the merge state; showing the diff without it");
            String::new()
        }
    };
    let checks = match deps.checks.pr_checks(&plan.owner, &plan.repo, number).await {
        Ok(checks) => checks,
        Err(e) => {
            tracing::warn!(run = plan.run_id, pr = %pr, error = %e, "console diff: could not read the status checks; showing the diff without them");
            Vec::new()
        }
    };

    match deps.diff.pr_diff(&plan.owner, &plan.repo, number).await {
        Ok(diff) => DiffOutcome::Ready(Box::new(RunDiff {
            run_id: plan.run_id,
            issue: plan.issue.clone(),
            branch: plan.branch.clone(),
            pr,
            url,
            number,
            head_sha,
            merge_state,
            checks,
            patch: diff.patch,
            truncated: diff.truncated,
        })),
        Err(e) => DiffOutcome::Failed(e.to_string()),
    }
}

impl crate::ControlHandle {
    /// The console asking what a run changed (`GET /api/v1/runs/{id}/diff`).
    ///
    /// **No control round trip at all.** The run row is read straight off the handle's own store,
    /// as [`crate::ControlHandle::resume_run`] already does, because a diff read takes no claim,
    /// consults no live loop state and records nothing — so there is nothing for the control task
    /// to own. THAT is the containment: nothing this route does can stall dispatch.
    ///
    /// It is not a claim that a `gh` call costs only the calling task. Every one of these blocks
    /// ([`crate::runmerge`]'s module doc), and a future with no await point holds the tokio WORKER
    /// THREAD it is polled on, not just its own task — a thread from the pool the control loop and
    /// the HTTP server share. That hazard is cheaper to reach on this route than on the merge one,
    /// because opening a tab triggers up to five `gh` reads where merging takes a click and a
    /// confirmation. So all five go through [`crate::ghsummons::GH::run_off_task`], capped at
    /// `GH_EXEC_TIMEOUT`: the two this route added
    /// ([`crate::ghsummons::PrDiffSource::pr_diff`],
    /// [`crate::ghsummons::PrChecksSource::pr_checks`]) from the start, and the three it shares
    /// with the merge path ([`crate::ghsummons::OpenPrSource::open_pr_for_branch`],
    /// [`crate::ghsummons::PrStateSource::pr_state`],
    /// [`crate::ghsummons::MergeStateSource::merge_state`]) since STUDIO-829, which moved every
    /// remaining inline exec in `ghsummons.rs` off the runtime thread. This route recorded that as
    /// a follow-up rather than doing it here, because those three are seams other callers share;
    /// it is done, not open, and a source pin in `ghsummons.rs` keeps it that way.
    ///
    /// A daemon built with no diff seams answers [`DiffOutcome::Unavailable`] rather than an
    /// error: the console asked a question and there is a true answer to it.
    pub async fn run_diff(&self, run_id: i64) -> DiffOutcome {
        let Some(deps) = self.diff.as_ref() else {
            return DiffOutcome::Unavailable(
                "this daemon has no GitHub access to read a diff with",
            );
        };
        let run = match self.store().get_run(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return DiffOutcome::NotFound,
            Err(e) => return DiffOutcome::Failed(e.to_string()),
        };
        match plan_run_diff(&run) {
            Ok(plan) => run_diff(&plan, deps).await,
            Err(why) => DiffOutcome::Unavailable(why),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The DECISIONS this module owns, against injected `gh` seams: which coordinate is derived
    //! from a run row, which answers end the read and which merely thin it, and — the load-bearing
    //! negative — that nothing here can merge. The WIRE contract belongs to
    //! `rhapsody_httpapi::handlers_rundiff` and is tested there.

    use std::sync::Mutex;

    use super::*;
    use async_trait::async_trait;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const URL: &str = "https://github.com/makewhatis/rhapsody/pull/64";
    const PATCH: &str = "diff --git a/x.rs b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";

    fn plan() -> DiffPlan {
        DiffPlan {
            run_id: 7,
            issue: "STUDIO-749".to_string(),
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            branch: "symphony/STUDIO-749".to_string(),
        }
    }

    fn run_row(branch: &str, repo: &str) -> rhapsody_store::RunSummary {
        rhapsody_store::RunSummary {
            id: 7,
            issue_identifier: "STUDIO-749".to_string(),
            branch: branch.to_string(),
            repo: repo.to_string(),
            ..rhapsody_store::RunSummary::default()
        }
    }

    // --- the seams, faked -------------------------------------------------------------------

    struct FakePrs {
        url: Option<&'static str>,
        fail: bool,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl OpenPrSource for FakePrs {
        async fn open_pr_for_branch(
            &self,
            owner: &str,
            repo: &str,
            branch: &str,
        ) -> crate::ghsummons::OpenPrResult {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{owner}/{repo}:{branch}"));
            if self.fail {
                return Err("gh pr list: HTTP 502".into());
            }
            // `head_sha` stays empty: `run_diff` takes only the URL from this seam, and a
            // value here would imply it reads one.
            Ok(self.url.map(|url| crate::ghsummons::OpenPr {
                url: url.to_string(),
                head_sha: String::new(),
            }))
        }
    }

    struct FakeState(PrLookup, bool);

    #[async_trait]
    impl PrStateSource for FakeState {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> crate::ghsummons::PrStateResult {
            if self.1 {
                return Err("gh pr view: HTTP 502".into());
            }
            Ok(self.0.clone())
        }
    }

    struct FakeMergeState(&'static str, bool);

    #[async_trait]
    impl MergeStateSource for FakeMergeState {
        async fn merge_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
        ) -> crate::ghsummons::MergeStateResult {
            if self.1 {
                return Err("gh pr view: HTTP 502".into());
            }
            Ok(self.0.to_string())
        }
    }

    struct FakeChecks(Vec<CheckRun>, bool);

    #[async_trait]
    impl PrChecksSource for FakeChecks {
        async fn pr_checks(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
        ) -> crate::ghsummons::PrChecksResult {
            if self.1 {
                return Err("gh pr view: HTTP 502".into());
            }
            Ok(self.0.clone())
        }
    }

    struct FakeDiff {
        patch: &'static str,
        truncated: bool,
        fail: bool,
        asked: Mutex<Vec<i64>>,
    }

    impl FakeDiff {
        fn at(patch: &'static str) -> Arc<FakeDiff> {
            Arc::new(FakeDiff {
                patch,
                truncated: false,
                fail: false,
                asked: Mutex::new(Vec::new()),
            })
        }
        fn failing() -> Arc<FakeDiff> {
            Arc::new(FakeDiff {
                patch: "",
                truncated: false,
                fail: true,
                asked: Mutex::new(Vec::new()),
            })
        }
        fn asked(&self) -> Vec<i64> {
            self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl PrDiffSource for FakeDiff {
        async fn pr_diff(
            &self,
            _owner: &str,
            _repo: &str,
            number: i64,
        ) -> crate::ghsummons::PrDiffResult {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(number);
            if self.fail {
                return Err("gh pr diff: HTTP 502".into());
            }
            Ok(crate::ghsummons::PrDiff {
                patch: self.patch.to_string(),
                truncated: self.truncated,
            })
        }
    }

    fn deps(prs: Arc<FakePrs>, diff: Arc<FakeDiff>) -> DiffDeps {
        deps_with(
            prs,
            Arc::new(FakeState(
                PrLookup::Found(crate::ghsummons::PrSnapshot {
                    head_sha: HEAD.to_string(),
                    status: crate::ghsummons::PrStatus::Open,
                    merged_at: None,
                    head_repo: "makewhatis/rhapsody".to_string(),
                }),
                false,
            )),
            Arc::new(FakeMergeState("CLEAN", false)),
            Arc::new(FakeChecks(
                vec![CheckRun {
                    name: "test".to_string(),
                    state: "SUCCESS".to_string(),
                }],
                false,
            )),
            diff,
        )
    }

    fn deps_with(
        prs: Arc<FakePrs>,
        state: Arc<FakeState>,
        mergestate: Arc<FakeMergeState>,
        checks: Arc<FakeChecks>,
        diff: Arc<FakeDiff>,
    ) -> DiffDeps {
        DiffDeps {
            prs,
            state,
            mergestate,
            checks,
            diff,
            allow: HeadAllowlist::none(),
        }
    }

    fn open_prs(url: Option<&'static str>) -> Arc<FakePrs> {
        Arc::new(FakePrs {
            url,
            fail: false,
            asked: Mutex::new(Vec::new()),
        })
    }

    // --- what the run row derives (plan_run_diff) --------------------------------------------

    /// The branch is DERIVED from the ticket, not read out of `runs.branch` — which is unwritten
    /// on every row this daemon has ever produced. Reading the column would answer nothing in
    /// production while passing a test that fabricates it, which is the defect STUDIO-779 caught
    /// on the merge path.
    #[test]
    fn the_branch_comes_from_the_ticket_when_the_row_carries_none() {
        let plan =
            plan_run_diff(&run_row("", "git@github.com:makewhatis/rhapsody.git")).expect("a plan");
        assert_eq!(plan.branch, "symphony/STUDIO-749");
        assert_eq!(plan.owner, "makewhatis");
        assert_eq!(plan.repo, "rhapsody");
        assert_eq!(plan.run_id, 7);
    }

    /// A row that DOES carry a branch must agree with the one its ticket names. The cross-check
    /// survives as a disagreement test rather than an equality test, so it holds for any future
    /// row that populates the column without refusing every row that does not.
    #[test]
    fn a_stored_branch_that_disagrees_with_its_ticket_has_no_diff() {
        let row = run_row("symphony/OTHER-1", "git@github.com:makewhatis/rhapsody.git");
        assert_eq!(
            plan_run_diff(&row),
            Err("this run's branch does not belong to its ticket")
        );

        // …and one that agrees is used as-is.
        let row = run_row(
            "symphony/STUDIO-749",
            "git@github.com:makewhatis/rhapsody.git",
        );
        assert_eq!(
            plan_run_diff(&row).expect("a plan").branch,
            "symphony/STUDIO-749"
        );
    }

    /// A remote this daemon will not vouch for never reaches `gh`. `parse_repo` refuses a
    /// look-alike host (STUDIO-721), and the diff path inherits that for free by using it.
    #[test]
    fn a_look_alike_or_absent_remote_has_no_diff() {
        for repo in [
            "",
            "git@evilgithub.com:makewhatis/rhapsody.git",
            "https://example.com/github.com/makewhatis/rhapsody",
        ] {
            assert_eq!(
                plan_run_diff(&run_row("", repo)),
                Err("this run has no GitHub repository, so there is no diff to read"),
                "repo {repo:?}"
            );
        }
    }

    // --- the read itself (run_diff) ----------------------------------------------------------

    /// The happy path, and the whole answer: the patch, the coordinate the daemon derived, the
    /// head commit it is of, and the design record's "PR number / checks / mergeability".
    #[tokio::test]
    async fn a_resolved_pull_request_answers_its_diff_and_its_coordinate() {
        let prs = open_prs(Some(URL));
        let diff = FakeDiff::at(PATCH);
        let got = run_diff(&plan(), &deps(Arc::clone(&prs), Arc::clone(&diff))).await;

        let DiffOutcome::Ready(d) = got else {
            panic!("expected a diff, got {got:?}");
        };
        assert_eq!(d.patch, PATCH);
        assert!(!d.truncated);
        assert_eq!(d.pr, "makewhatis/rhapsody#64");
        assert_eq!(d.number, 64);
        assert_eq!(d.url, URL);
        assert_eq!(d.head_sha, HEAD);
        assert_eq!(d.branch, "symphony/STUDIO-749");
        assert_eq!(d.issue, "STUDIO-749");
        assert_eq!(d.merge_state, "CLEAN");
        assert_eq!(
            d.checks,
            vec![CheckRun {
                name: "test".to_string(),
                state: "SUCCESS".to_string()
            }]
        );
        // The coordinate reaching `gh` is the plan's, and the number is GitHub's own answer —
        // never one a caller could have named, because there is nowhere for one to travel.
        assert_eq!(
            prs.asked.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec!["makewhatis/rhapsody:symphony/STUDIO-749".to_string()]
        );
        assert_eq!(diff.asked(), vec![64]);
    }

    /// A branch with no open pull request is not an error and not a refusal: there is genuinely
    /// nothing to show, and the console falls back to the deep link it used before this endpoint.
    /// **No diff is read**, so an unpushed branch costs one `gh` call rather than four.
    ///
    /// The reason must say OPEN. `open_pr_for_branch` filters `--state open`, so this same answer
    /// covers a merged-and-closed pull request — and since STUDIO-712 those are most of the
    /// finished runs in history. "No pull request on this branch" would tell their operator the
    /// branch was never pushed, which is the opposite of what happened to it.
    #[tokio::test]
    async fn a_branch_with_no_open_pull_request_has_nothing_to_show() {
        let diff = FakeDiff::at(PATCH);
        let got = run_diff(&plan(), &deps(open_prs(None), Arc::clone(&diff))).await;
        let DiffOutcome::Unavailable(reason) = got else {
            panic!("expected Unavailable, got {got:?}");
        };
        assert!(
            reason.contains("no open pull request"),
            "reason should say the pull request must be OPEN: {reason}"
        );
        assert!(
            reason.contains("merged or closed"),
            "a merged pull request must not read as an unpushed branch: {reason}"
        );
        assert!(diff.asked().is_empty(), "no diff should have been read");
    }

    /// A pull request in someone ELSE's account is not followed — the resolution is already scoped
    /// to `--repo owner/repo`, so a URL outside it means an assumption is wrong.
    #[tokio::test]
    async fn a_pull_request_in_another_account_is_not_followed() {
        let diff = FakeDiff::at(PATCH);
        let got = run_diff(
            &plan(),
            &deps(
                open_prs(Some("https://github.com/stranger/rhapsody/pull/64")),
                Arc::clone(&diff),
            ),
        )
        .await;
        assert_eq!(
            got,
            DiffOutcome::Unavailable("the resolved pull request belongs to another account")
        );
        assert!(diff.asked().is_empty());
    }

    /// The F-SEC trust boundary fails closed for a READ too. The next thing that happens to this
    /// patch is that a browser renders it under this run's ticket, so rendering a stranger's fork
    /// would be the console asserting something false about whose work it is.
    #[tokio::test]
    async fn an_untrusted_or_missing_head_ends_the_read() {
        for (lookup, want) in [
            (
                PrLookup::Untrusted,
                "the pull request's head repository is not this one",
            ),
            (PrLookup::Gone, "GitHub cannot resolve that pull request"),
        ] {
            let diff = FakeDiff::at(PATCH);
            let got = run_diff(
                &plan(),
                &deps_with(
                    open_prs(Some(URL)),
                    Arc::new(FakeState(lookup.clone(), false)),
                    Arc::new(FakeMergeState("CLEAN", false)),
                    Arc::new(FakeChecks(Vec::new(), false)),
                    Arc::clone(&diff),
                ),
            )
            .await;
            assert_eq!(got, DiffOutcome::Unavailable(want));
            assert!(
                diff.asked().is_empty(),
                "{lookup:?}: no diff should have been read"
            );
        }
    }

    /// **The garnish must not cost the diff.** A `gh` that would not answer the merge state or the
    /// checks leaves both at their empty value — which the console already renders as silence,
    /// because it is what GitHub itself answers for a pull request with neither — and the patch,
    /// which is the deliverable, still arrives.
    #[tokio::test]
    async fn a_failed_merge_state_or_checks_read_still_answers_the_diff() {
        let got = run_diff(
            &plan(),
            &deps_with(
                open_prs(Some(URL)),
                Arc::new(FakeState(
                    PrLookup::Found(crate::ghsummons::PrSnapshot {
                        head_sha: HEAD.to_string(),
                        status: crate::ghsummons::PrStatus::Open,
                        merged_at: None,
                        head_repo: "makewhatis/rhapsody".to_string(),
                    }),
                    false,
                )),
                Arc::new(FakeMergeState("", true)),
                Arc::new(FakeChecks(Vec::new(), true)),
                FakeDiff::at(PATCH),
            ),
        )
        .await;

        let DiffOutcome::Ready(d) = got else {
            panic!("the diff must survive a failed garnish read, got {got:?}");
        };
        assert_eq!(d.patch, PATCH);
        assert_eq!(d.merge_state, "");
        assert!(d.checks.is_empty());
    }

    /// The diff itself failing IS a failure, and it carries `gh`'s own complaint verbatim — an
    /// empty patch would read as "this run changed nothing".
    #[tokio::test]
    async fn a_failed_diff_read_is_a_failure_carrying_githubs_complaint() {
        let got = run_diff(&plan(), &deps(open_prs(Some(URL)), FakeDiff::failing())).await;
        let DiffOutcome::Failed(err) = got else {
            panic!("expected a failure, got {got:?}");
        };
        assert!(err.contains("HTTP 502"), "{err}");
    }

    /// A lookup that could not be MADE is a failure, not "no pull request" — the console must not
    /// tell an operator their branch has no pull request because GitHub was unreachable.
    #[tokio::test]
    async fn an_unreachable_github_is_a_failure_and_not_an_empty_answer() {
        let prs = Arc::new(FakePrs {
            url: None,
            fail: true,
            asked: Mutex::new(Vec::new()),
        });
        let got = run_diff(&plan(), &deps(prs, FakeDiff::at(PATCH))).await;
        let DiffOutcome::Failed(err) = got else {
            panic!("expected a failure, got {got:?}");
        };
        assert!(err.contains("HTTP 502"), "{err}");
    }

    /// **This module cannot merge**, and it is asserted on its own source rather than left to a
    /// reviewer: `DiffDeps` holds five READ seams, so nothing in `run_diff`'s call graph has a
    /// `MergeSource` in scope to call. Adding one is a failing test.
    #[test]
    fn the_diff_path_has_no_merge_seam_in_scope() {
        // The module's own source, MINUS this test module — otherwise the assertion messages
        // below match themselves and the check can never fail for the right reason.
        let whole = include_str!("rundiff.rs");
        let src = &whole[..whole.find("#[cfg(test)]").expect("a test module")];
        let start = src
            .find("pub struct DiffDeps {")
            .expect("DiffDeps is still called that");
        let body = &src[start..start + src[start..].find("\n}").expect("a closing brace")];
        let fields: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !fields.contains("MergeSource"),
            "DiffDeps gained a merge seam: reading a diff must never be able to merge one \
             (STUDIO-767 §5 keeps the two apart)"
        );
        // And the whole module, not just the type: no call to the one method that moves `main`.
        assert!(
            !src.contains("merge_pr("),
            "rundiff called merge_pr: the Diff tab reads, it does not act"
        );
    }
}
