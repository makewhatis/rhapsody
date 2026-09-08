//! runmerge — the console merge action's OFF-LOOP half: resolve a run's pull request, refuse
//! everything that should be refused, and perform the one bounded merge (STUDIO-767, slices 1–3 of
//! the design record `~/.rhapsody/docs/STUDIO-767-console-merge-action.md`, §2/§3/§7).
//!
//! **No Go v0.4.0 counterpart.** The frozen Symphony reference has no console merge; this is the
//! additive Rhapsody surface the design record specifies, dormant unless Teams is on.
//!
//! # Why this file holds no `Orchestrator`
//!
//! Everything here shells out through [`crate::ghsummons::GH`]'s synchronous
//! `std::process::Command`. Its future has no await point, so a `tokio::time::timeout` around it
//! cannot cancel it and whatever task drives it is blocked for the whole round-trip —
//! [`crate::prstate`]'s finding, and the reason its sweep is a free function too. The containment
//! is therefore **structural**: this module takes no `Orchestrator`, sends no control event and
//! holds no lock the control task takes, so a stalled `gh` parks the task that called it and
//! nothing else. Its caller is [`crate::mergeconsole`]'s [`crate::ControlHandle`] method, which
//! runs on the HTTP request's own task — so a hung merge delays that one request. `prstate`'s
//! standing `pr_state_is_never_called_from_the_control_loop` check knows this file by name.
//!
//! The DECISIONS that need loop state — the run row, the single-flight claim, the live review
//! snapshot, the audit record — are made in [`crate::mergeconsole`] on the control task and
//! arrive here already made, as a [`MergePlan`]. That split is what lets this half be a pure
//! function of its plan and its `gh` answers.
//!
//! # The guardrails this half owns (§3)
//!
//! * **G1 — the operator never names the pull request.** [`MergePlan`] is built from the run row
//!   alone; nothing in it comes from a request body. There is no field on it, and no parameter
//!   here, that a client could put a pull-request number in. The number is resolved from GitHub by
//!   HEAD BRANCH ([`OpenPrSource::open_pr_for_branch`], which rejects a fork's pull request) and
//!   then cross-checked back against the plan's own account before anything acts on it.
//! * **G2 — the daemon never overrides a gate.** The merge is [`MERGE_METHOD`] + [`MERGE_AUTO`],
//!   i.e. `--squash --auto`: GitHub's own auto-merge, which lands the pull request only once the
//!   required contexts pass. `--admin` cannot be reached from here — it is not a parameter of
//!   [`MergeSource::merge_pr`] at all. On top of that, a pull request a live Rhapsody review round
//!   is still watching is refused ([`MergePlan::watched`]).
//! * **G3 — the confirm handshake is server-enforced.** An unconfirmed request is answered with
//!   the receipt and NOTHING is merged; confirming means echoing back the head SHA the daemon
//!   itself just resolved, so a stale confirmation after a push is refused.

use std::sync::Arc;

use serde::Serialize;

use crate::ghsummons::{
    BranchUpdateSource, HeadAllowlist, MERGE_STATE_BEHIND, MergeMethod, MergeSource,
    MergeStateSource, OpenPrSource, PrLookup, PrStateSource, PrStatus,
};
use crate::teamsknow::parse_pr_ref;

/// How the console's merge action merges. `--squash` is the repo's convention — the squash subject
/// is the pull-request title release-please parses — and it is what the sign-off pinned (§9.2).
pub const MERGE_METHOD: MergeMethod = MergeMethod::Squash;

/// Whether the console's merge action arms GitHub's own auto-merge rather than merging now.
///
/// `true`, and this constant is a guardrail rather than a preference (§3/G2). With `--auto` the
/// daemon holds no state, waits for nothing and **cannot merge a red pull request even by
/// mistake**: GitHub lands it when `lint`, `test`, `web` and `desktop` pass, and never before.
/// Flipping this to `false` would move the authorization boundary from GitHub into this daemon.
pub const MERGE_AUTO: bool = true;

/// The `gh` seams the merge path drives, and the trust boundary it drives them under. Mirrors
/// [`crate::reviewwatch::ReviewWatchDeps`]: the daemon builds one [`crate::ghsummons::GH`] and
/// hands it in as all three, and a test hands in three fakes.
pub struct MergeDeps {
    /// Resolves the run's head branch to its open pull request — the ONLY way a number enters.
    pub prs: Arc<dyn OpenPrSource>,
    /// Resolves that number's head SHA and state, for the confirm handshake and the state gate.
    pub state: Arc<dyn PrStateSource>,
    /// Performs the merge itself.
    pub merger: Arc<dyn MergeSource>,
    /// Asks GitHub whether this pull request can be merged at all — its `mergeStateStatus`
    /// (STUDIO-784). Read on every attempt, and the answer rides on the receipt.
    pub mergestate: Arc<dyn MergeStateSource>,
    /// Asks whether the REPOSITORY updates a behind branch itself. Consulted only when the answer
    /// above was [`MERGE_STATE_BEHIND`], so an ordinary merge pays for no extra round trip.
    pub policy: Arc<dyn BranchUpdateSource>,
    /// Head repositories trusted besides the base's own owner. [`HeadAllowlist::none`] on the
    /// daemon — the watcher's default trust boundary, and widening it is a code change.
    pub allow: HeadAllowlist,
}

/// Everything the control task validated before the merge path was allowed to run, and everything
/// the off-loop half is permitted to know.
///
/// Every field is DAEMON-derived, never client-supplied: `repo` is written from the project's
/// configured remote and never from an agent, and `branch` is the name the daemon's own branch
/// naming determines for `issue` (see `mergeconsole::plan_run_merge` — `runs.branch` is unwritten,
/// so a stored branch is only ever cross-checked against that name, never trusted in place of it).
/// That is G1 expressed as a type: the absence of a `number` field is why no code path leads from
/// a request body to `gh pr merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub run_id: i64,
    /// The run's ticket, e.g. `STUDIO-767`. Named in the audit record and the room line.
    pub issue: String,
    pub owner: String,
    pub repo: String,
    /// The branch `issue` names, derived on the control task — and cross-checked there against a
    /// stored `runs.branch` on the rare row that carries one.
    pub branch: String,
    /// Pull-request numbers in `owner/repo` that a LIVE Rhapsody review round is watching,
    /// snapshotted on the control task at plan time (§3/G2's review gate).
    ///
    /// A snapshot and not a live read, because the live read needs the control task and this half
    /// does not run there. The window is the one round-trip between planning and resolving, so a
    /// review round that starts inside it is not seen — which is the right way round: the gate
    /// exists to stop a merge racing a review already in progress, and `--auto` means even a
    /// missed one cannot land red code.
    pub watched: Vec<i64>,
}

/// What the operator is about to act on, or just did — the body of both the 409 `confirm_required`
/// answer and the 200 that follows it.
///
/// It carries no pull-request TITLE and no checks rollup, which the design record's §3/G3 sketch
/// named. Both would need either a second bounded `gh` call or a widening of the shared
/// [`PrStateSource`] argv that the review watcher polls every two minutes, and neither is load
/// bearing: the console already holds the run's own title, and `--auto` makes GitHub — not this
/// receipt — the thing that decides whether a red pull request lands. Noted rather than quietly
/// dropped; a richer receipt is a clean follow-up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MergeReceipt {
    pub run_id: i64,
    pub issue: String,
    /// `owner/repo#number` — the coordinate, echoed so a log or a toast can name it.
    pub pr: String,
    /// The pull request's browser URL, as GitHub gave it.
    pub url: String,
    pub number: i64,
    /// The head commit the daemon resolved. **This is the confirmation token**: confirming a merge
    /// means echoing this value back, so a push between the two round trips invalidates it.
    pub head_sha: String,
    /// `squash` — [`MERGE_METHOD`], rendered for the operator.
    pub method: String,
    /// Whether GitHub's own auto-merge was armed rather than an immediate merge performed.
    pub auto: bool,
    /// GitHub's own `mergeStateStatus` when the daemon resolved this pull request — `CLEAN`,
    /// `BLOCKED`, `UNSTABLE`, `BEHIND` — or empty when GitHub stated none.
    ///
    /// It is on the receipt because an armed `--auto` merge is otherwise a silent promise: the
    /// room line says *"queued for merge"* and nothing afterwards ever says whether it landed. This
    /// is the one fact the console can show that separates a merge waiting on green checks from one
    /// waiting on a human (STUDIO-784).
    pub merge_state: String,
    /// `gh`'s own words about the merge. Empty on a `confirm_required` receipt — nothing has
    /// happened yet — and the audit record's evidence on an applied one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub said: String,
}

/// The outcome of one console merge request.
///
/// Modelled on [`crate::reviewconsole::ReviewControlOutcome`], with the two variants a merge earns:
/// a merge is irreversible, so "you have not confirmed yet" is a first-class answer rather than an
/// error, and "there is no such run" is distinguishable from "there is, and no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeControlOutcome {
    /// Teams is off, so this Rhapsody-additive write surface is dormant. Nothing was read and
    /// nothing was written.
    Dormant,
    /// No run has that id. Distinct from a refusal because the console renders it as a dead link
    /// rather than as the daemon saying no.
    NotFound,
    /// The request was refused; the payload names why, in words an operator reads. `&'static str`
    /// so a refusal can never carry a value from GitHub or from a request body.
    Refused(&'static str),
    /// The merge was resolved but not performed, because the request carried no matching
    /// confirmation. The receipt is what the operator is being asked to confirm (§3/G3).
    ConfirmRequired(MergeReceipt),
    /// A lookup or the merge itself failed; the payload is `gh`'s own complaint verbatim.
    Failed(String),
    /// The merge was performed — or, with [`MERGE_AUTO`], armed to perform when the required
    /// contexts pass.
    Applied(MergeReceipt),
}

/// Resolves `plan`'s pull request and, when everything holds and `confirm` matches its head SHA,
/// merges it. **The whole off-loop half of the merge action** (§7 slices 1–3).
///
/// The order is the order the refusals must happen in, and each step earns its place:
///
/// 1. **Resolve by head branch.** [`OpenPrSource::open_pr_for_branch`] filters `--state open` and
///    rejects a fork's pull request, so a stranger cannot get a coordinate in here by opening a
///    pull request whose head branch is named like this run's (STUDIO-674's fork hazard).
/// 2. **Cross-check the URL back against the plan.** GitHub answered a query already scoped to
///    `--repo owner/repo`, so a URL in another ACCOUNT means something is wrong with an assumption
///    rather than with the operator; it is refused rather than followed. Only the NUMBER is taken
///    from the URL — owner and repo stay the run row's, which is config-derived.
/// 3. **Ask where the pull request stands.** `Gone` and `Untrusted` are refusals; `Merged` is a
///    refusal too, and deliberately an idempotent-feeling one rather than an error — clicking
///    Merge twice is a thing operators do.
/// 4. **The review gate.** A pull request a live Rhapsody review round is watching is refused
///    (§9.3: "refuse while a live review round is watching the PR").
/// 5. **The confirm handshake**, against the head SHA resolved in step 3 and never against one the
///    caller supplied for itself.
/// 6. **The merge**, at last, as `--squash --auto` and nothing else.
pub async fn resolve_and_merge(
    plan: &MergePlan,
    confirm: &str,
    deps: &MergeDeps,
) -> MergeControlOutcome {
    let url = match deps
        .prs
        .open_pr_for_branch(&plan.owner, &plan.repo, &plan.branch)
        .await
    {
        Ok(Some(url)) => url,
        Ok(None) => {
            return MergeControlOutcome::Refused("no open pull request on this run's branch");
        }
        Err(e) => return MergeControlOutcome::Failed(e.to_string()),
    };
    // The number, and ONLY the number, is taken from GitHub's answer. `parse_pr_ref` fails closed
    // on anything that is not a positive number under a two-segment owner/repo path.
    let Some(found) = parse_pr_ref(&url) else {
        return MergeControlOutcome::Refused("the pull request's URL could not be read");
    };
    // On the OWNER and not on the whole slug, for the reason `open_pr_for_branch` gives at length:
    // a same-account fork is inside the trust boundary, and a whole-slug match would break on a
    // repository RENAME — `gh` follows GitHub's redirect and answers under the canonical name while
    // this daemon still asks under the stale one from config. Matching the slug here would
    // re-introduce, one call later, exactly the fragility that helper avoids. What this check is
    // for is a URL in someone ELSE's account, which no `--repo`-scoped query should ever produce.
    if !found.owner.eq_ignore_ascii_case(&plan.owner) {
        tracing::warn!(
            run = plan.run_id,
            url = %url,
            want = %plan.owner,
            "console merge: the resolved pull request belongs to another account; refusing"
        );
        return MergeControlOutcome::Refused(
            "the resolved pull request belongs to another account",
        );
    }
    let number = found.number;
    let pr = format!("{}/{}#{number}", plan.owner, plan.repo);

    let snapshot = match deps
        .state
        .pr_state(&plan.owner, &plan.repo, number, &deps.allow)
        .await
    {
        Ok(PrLookup::Found(snap)) => snap,
        Ok(PrLookup::Gone) => {
            return MergeControlOutcome::Refused("GitHub cannot resolve that pull request");
        }
        Ok(PrLookup::Untrusted) => {
            return MergeControlOutcome::Refused(
                "the pull request's head repository is not this one",
            );
        }
        Err(e) => return MergeControlOutcome::Failed(e.to_string()),
    };
    match snapshot.status {
        PrStatus::Merged => {
            return MergeControlOutcome::Refused("that pull request is already merged");
        }
        PrStatus::Closed => return MergeControlOutcome::Refused("that pull request is closed"),
        PrStatus::Open => {}
    }
    if plan.watched.contains(&number) {
        return MergeControlOutcome::Refused(
            "a Rhapsody review of that pull request is still live",
        );
    }

    // Can this merge ever land? `--auto` arms GitHub's own auto-merge, which is what keeps a red
    // pull request from merging — but an armed auto-merge that GitHub will never fire is parked
    // silently and forever, and the operator is told "queued for merge" for something that will
    // not happen (STUDIO-784, gap 1). `main` requires branches to be up to date and does not
    // update them itself, so BEHIND is exactly that state.
    let merge_state = match deps
        .mergestate
        .merge_state(&plan.owner, &plan.repo, number)
        .await
    {
        Ok(state) => state,
        Err(e) => return MergeControlOutcome::Failed(e.to_string()),
    };
    if merge_state == MERGE_STATE_BEHIND {
        // Only here, and only for this reason: with `allow_update_branch` on, GitHub's auto-merge
        // brings the branch up to date itself and the merge lands, so refusing would be a false
        // refusal. The repository setting is READ rather than assumed, because assuming it is how
        // a gate goes wrong the day someone changes it.
        match deps
            .policy
            .allows_branch_update(&plan.owner, &plan.repo)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    run = plan.run_id,
                    issue = %plan.issue,
                    pr = %pr,
                    "console merge: the branch is behind its base and the repository will not \
                     update it; refusing rather than arming an auto-merge that cannot fire"
                );
                return MergeControlOutcome::Refused(
                    "this branch is behind its base and cannot update itself; push or update the \
                     branch, then merge",
                );
            }
            Err(e) => return MergeControlOutcome::Failed(e.to_string()),
        }
    }

    let receipt = MergeReceipt {
        run_id: plan.run_id,
        issue: plan.issue.clone(),
        pr: pr.clone(),
        url,
        number,
        head_sha: snapshot.head_sha.clone(),
        method: MERGE_METHOD.name().to_string(),
        auto: MERGE_AUTO,
        merge_state,
        said: String::new(),
    };
    // Constant-time comparison would be theatre here: the value being compared is a public commit
    // SHA that `GET /api/v1/runs/{id}` neighbours already expose, and §3/G3 is explicit that the
    // handshake is a speed bump against mistake and drive-by forgery, not a secret.
    if confirm != snapshot.head_sha {
        return MergeControlOutcome::ConfirmRequired(receipt);
    }

    match deps
        .merger
        .merge_pr(&plan.owner, &plan.repo, number, MERGE_METHOD, MERGE_AUTO)
        .await
    {
        Ok(said) => {
            tracing::info!(run = plan.run_id, issue = %plan.issue, pr = %pr, "console merge: merged");
            MergeControlOutcome::Applied(MergeReceipt { said, ..receipt })
        }
        Err(e) => MergeControlOutcome::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! Every `Refused` arm the design record names (§3, §7 slice 2), plus the confirm handshake and
    //! the one thing the merge call itself must always be: `--squash --auto`, on a coordinate this
    //! module resolved rather than one it was handed.

    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::ghsummons::{
        BranchUpdateResult, MergeResult, MergeStateResult, OpenPrResult, PrSnapshot, PrStateResult,
        PrStatus,
    };

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// An [`OpenPrSource`] answering one canned result, recording what it was asked.
    struct FakePrs {
        url: Option<&'static str>,
        fail: bool,
        asked: Mutex<Vec<String>>,
    }

    impl FakePrs {
        fn at(url: &'static str) -> Arc<FakePrs> {
            Arc::new(FakePrs {
                url: Some(url),
                fail: false,
                asked: Mutex::new(Vec::new()),
            })
        }
        fn none() -> Arc<FakePrs> {
            Arc::new(FakePrs {
                url: None,
                fail: false,
                asked: Mutex::new(Vec::new()),
            })
        }
        fn failing() -> Arc<FakePrs> {
            Arc::new(FakePrs {
                url: None,
                fail: true,
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl OpenPrSource for FakePrs {
        async fn open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> OpenPrResult {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{owner}/{repo}:{branch}"));
            if self.fail {
                return Err("gh pr list: HTTP 502".into());
            }
            Ok(self.url.map(str::to_string))
        }
    }

    /// A [`PrStateSource`] answering one canned lookup.
    struct FakeState {
        lookup: Option<PrLookup>,
    }

    impl FakeState {
        fn found(status: PrStatus, head: &str) -> Arc<FakeState> {
            Arc::new(FakeState {
                lookup: Some(PrLookup::Found(PrSnapshot {
                    head_sha: head.to_string(),
                    status,
                    merged_at: None,
                    head_repo: "o/r".to_string(),
                })),
            })
        }
        fn open() -> Arc<FakeState> {
            FakeState::found(PrStatus::Open, HEAD)
        }
        fn answering(lookup: PrLookup) -> Arc<FakeState> {
            Arc::new(FakeState {
                lookup: Some(lookup),
            })
        }
        fn failing() -> Arc<FakeState> {
            Arc::new(FakeState { lookup: None })
        }
    }

    #[async_trait]
    impl PrStateSource for FakeState {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            match &self.lookup {
                Some(l) => Ok(l.clone()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    /// A [`MergeSource`] recording every merge it was asked to perform.
    struct FakeMerger {
        fail: Option<&'static str>,
        calls: Mutex<Vec<(String, i64, MergeMethod, bool)>>,
    }

    impl FakeMerger {
        fn ok() -> Arc<FakeMerger> {
            Arc::new(FakeMerger {
                fail: None,
                calls: Mutex::new(Vec::new()),
            })
        }
        fn failing(err: &'static str) -> Arc<FakeMerger> {
            Arc::new(FakeMerger {
                fail: Some(err),
                calls: Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> Vec<(String, i64, MergeMethod, bool)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl MergeSource for FakeMerger {
        async fn merge_pr(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            method: MergeMethod,
            auto: bool,
        ) -> MergeResult {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                format!("{owner}/{repo}"),
                number,
                method,
                auto,
            ));
            match self.fail {
                Some(e) => Err(e.into()),
                None => Ok("✓ Pull request #64 will be automatically merged".to_string()),
            }
        }
    }

    /// A [`MergeStateSource`] answering one canned `mergeStateStatus`, or failing.
    struct FakeMergeState {
        state: Option<&'static str>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeMergeState {
        fn at(state: &'static str) -> Arc<FakeMergeState> {
            Arc::new(FakeMergeState {
                state: Some(state),
                asked: Mutex::new(Vec::new()),
            })
        }
        fn clean() -> Arc<FakeMergeState> {
            FakeMergeState::at("CLEAN")
        }
        fn failing() -> Arc<FakeMergeState> {
            Arc::new(FakeMergeState {
                state: None,
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl MergeStateSource for FakeMergeState {
        async fn merge_state(&self, owner: &str, repo: &str, number: i64) -> MergeStateResult {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{owner}/{repo}#{number}"));
            match self.state {
                Some(s) => Ok(s.to_string()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    /// A [`BranchUpdateSource`] answering one canned repository policy, recording whether it was
    /// asked at all — an ordinary merge must never pay for this call.
    struct FakePolicy {
        allows: Option<bool>,
        asked: Mutex<Vec<String>>,
    }

    impl FakePolicy {
        fn answering(allows: bool) -> Arc<FakePolicy> {
            Arc::new(FakePolicy {
                allows: Some(allows),
                asked: Mutex::new(Vec::new()),
            })
        }
        fn failing() -> Arc<FakePolicy> {
            Arc::new(FakePolicy {
                allows: None,
                asked: Mutex::new(Vec::new()),
            })
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl BranchUpdateSource for FakePolicy {
        async fn allows_branch_update(&self, owner: &str, repo: &str) -> BranchUpdateResult {
            self.asked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{owner}/{repo}"));
            match self.allows {
                Some(a) => Ok(a),
                None => Err("gh api repos/o/r: HTTP 403".into()),
            }
        }
    }

    fn plan() -> MergePlan {
        MergePlan {
            run_id: 42,
            issue: "STUDIO-767".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            branch: "symphony/STUDIO-767".to_string(),
            watched: Vec::new(),
        }
    }

    /// The ordinary dependency set: a pull request GitHub says is `CLEAN`, on a repository that
    /// would not update a behind branch anyway. Every pre-existing test wants exactly this.
    fn deps(prs: Arc<FakePrs>, state: Arc<FakeState>, merger: Arc<FakeMerger>) -> MergeDeps {
        deps_with(
            prs,
            state,
            merger,
            FakeMergeState::clean(),
            FakePolicy::answering(false),
        )
    }

    fn deps_with(
        prs: Arc<FakePrs>,
        state: Arc<FakeState>,
        merger: Arc<FakeMerger>,
        mergestate: Arc<FakeMergeState>,
        policy: Arc<FakePolicy>,
    ) -> MergeDeps {
        MergeDeps {
            prs,
            state,
            merger,
            mergestate,
            policy,
            allow: HeadAllowlist::none(),
        }
    }

    /// The plain path: no open pull request on the branch is a refusal an operator reads, not an
    /// error they retry — and nothing is merged.
    #[tokio::test]
    async fn a_branch_with_no_open_pull_request_is_refused() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(FakePrs::none(), FakeState::open(), Arc::clone(&merger)),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused("no open pull request on this run's branch")
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
    }

    /// **G1, on the resolution path.** The number comes from a URL GitHub answered for a query
    /// already scoped to the run's own repository; a URL naming another ACCOUNT means an
    /// assumption has broken, and following it would be exactly the "a coordinate is trusted
    /// because of what it says about itself" mistake the review subsystem refuses to make.
    #[tokio::test]
    async fn a_pull_request_resolved_in_another_account_is_refused() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(
                FakePrs::at("https://github.com/attacker/evil/pull/1"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused("the resolved pull request belongs to another account")
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
    }

    /// A repository RENAMED since the daemon's config was written still merges. `gh` follows
    /// GitHub's redirect and answers under the canonical name, so a whole-slug cross-check here
    /// would refuse every merge in a renamed repository — re-introducing one call later the exact
    /// fragility `open_pr_for_branch` avoids by matching on the owner.
    #[tokio::test]
    async fn a_renamed_repository_still_merges() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(
                FakePrs::at("https://github.com/o/renamed/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        assert!(matches!(got, MergeControlOutcome::Applied(_)), "{got:?}");
        assert_eq!(
            merger.calls(),
            vec![("o/r".to_string(), 64, MergeMethod::Squash, true)],
            "and it merges at the coordinate CONFIG names, which gh redirects for us"
        );
    }

    /// A URL with no readable pull-request number fails closed rather than guessing one.
    #[tokio::test]
    async fn an_unreadable_pull_request_url_is_refused() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(
                FakePrs::at("https://github.com/o/r/pulls"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused("the pull request's URL could not be read")
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
    }

    /// `Gone` and `Untrusted` are the two non-answers [`PrStateSource`] gives, and both refuse. The
    /// second is the fork guard: a head repository that cannot be SHOWN to be ours is not merged.
    #[tokio::test]
    async fn a_gone_or_untrusted_pull_request_is_refused() {
        for (lookup, want) in [
            (PrLookup::Gone, "GitHub cannot resolve that pull request"),
            (
                PrLookup::Untrusted,
                "the pull request's head repository is not this one",
            ),
        ] {
            let merger = FakeMerger::ok();
            let got = resolve_and_merge(
                &plan(),
                HEAD,
                &deps(
                    FakePrs::at("https://github.com/o/r/pull/64"),
                    FakeState::answering(lookup),
                    Arc::clone(&merger),
                ),
            )
            .await;
            assert_eq!(got, MergeControlOutcome::Refused(want));
            assert!(merger.calls().is_empty(), "nothing may be merged");
        }
    }

    /// Clicking Merge on a pull request that already landed is a thing operators do. It is a plain
    /// refusal that says so — not an error, and certainly not a second merge.
    #[tokio::test]
    async fn an_already_merged_or_closed_pull_request_is_refused() {
        for (status, want) in [
            (PrStatus::Merged, "that pull request is already merged"),
            (PrStatus::Closed, "that pull request is closed"),
        ] {
            let merger = FakeMerger::ok();
            let got = resolve_and_merge(
                &plan(),
                HEAD,
                &deps(
                    FakePrs::at("https://github.com/o/r/pull/64"),
                    FakeState::found(status, HEAD),
                    Arc::clone(&merger),
                ),
            )
            .await;
            assert_eq!(got, MergeControlOutcome::Refused(want));
            assert!(merger.calls().is_empty(), "nothing may be merged");
        }
    }

    /// **The review gate (§9.3).** A pull request a live Rhapsody review round is still watching is
    /// refused, so the operator's click cannot land code out from under a review in progress.
    #[tokio::test]
    async fn a_live_review_round_refuses_the_merge() {
        let merger = FakeMerger::ok();
        let watched = MergePlan {
            watched: vec![64],
            ..plan()
        };
        let got = resolve_and_merge(
            &watched,
            HEAD,
            &deps(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused("a Rhapsody review of that pull request is still live")
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
        // A review of some OTHER pull request in the same repository is not this one's business.
        let elsewhere = MergePlan {
            watched: vec![63, 65],
            ..plan()
        };
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &elsewhere,
            HEAD,
            &deps(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        assert!(matches!(got, MergeControlOutcome::Applied(_)), "{got:?}");
    }

    /// **G3.** An unconfirmed request performs NO merge and answers with the receipt the operator
    /// is being asked to confirm — including the head SHA that IS the confirmation token.
    #[tokio::test]
    async fn an_unconfirmed_request_merges_nothing_and_returns_the_receipt() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            "",
            &deps(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        let MergeControlOutcome::ConfirmRequired(receipt) = got else {
            panic!("want ConfirmRequired, got {got:?}");
        };
        assert_eq!(receipt.pr, "o/r#64");
        assert_eq!(receipt.url, "https://github.com/o/r/pull/64");
        assert_eq!(receipt.number, 64);
        assert_eq!(receipt.head_sha, HEAD);
        assert_eq!(receipt.issue, "STUDIO-767");
        assert_eq!(receipt.run_id, 42);
        assert_eq!(receipt.method, "squash");
        assert!(
            receipt.auto,
            "the receipt says GitHub's auto-merge is armed"
        );
        assert_eq!(receipt.said, "", "nothing has happened yet");
        assert!(merger.calls().is_empty(), "nothing may be merged");
    }

    /// **G3's whole point.** A confirmation that names a head the pull request has moved past — the
    /// author pushed between the two round trips — is refused as unconfirmed, so the operator
    /// re-reads what they are about to merge instead of merging code they never saw.
    #[tokio::test]
    async fn a_stale_confirmation_merges_nothing() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            OTHER_HEAD,
            &deps(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
            ),
        )
        .await;
        let MergeControlOutcome::ConfirmRequired(receipt) = got else {
            panic!("want ConfirmRequired, got {got:?}");
        };
        assert_eq!(receipt.head_sha, HEAD, "the receipt carries the LIVE head");
        assert!(merger.calls().is_empty(), "nothing may be merged");
    }

    /// The happy path, end to end: one merge, at the coordinate this module resolved, as
    /// `--squash --auto` — and `gh`'s own words come back on the receipt as the audit evidence.
    #[tokio::test]
    async fn a_confirmed_merge_is_one_squash_auto_call_on_the_resolved_coordinate() {
        let prs = FakePrs::at("https://github.com/o/r/pull/64");
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(Arc::clone(&prs), FakeState::open(), Arc::clone(&merger)),
        )
        .await;

        let MergeControlOutcome::Applied(receipt) = got else {
            panic!("want Applied, got {got:?}");
        };
        assert_eq!(receipt.pr, "o/r#64");
        assert_eq!(
            receipt.said,
            "✓ Pull request #64 will be automatically merged"
        );
        assert_eq!(
            merger.calls(),
            vec![("o/r".to_string(), 64, MergeMethod::Squash, true)],
            "exactly one merge, squash, with GitHub's own auto-merge armed"
        );
        assert_eq!(
            prs.asked.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec!["o/r:symphony/STUDIO-767".to_string()],
            "the pull request is resolved from the run's own branch and nothing else"
        );
    }

    /// A lookup that could not be MADE is `Failed`, never `Refused`: "we do not know" and "no" are
    /// different answers, and only the first is worth retrying.
    #[tokio::test]
    async fn a_failed_lookup_is_failed_rather_than_refused() {
        for (prs, state) in [
            (FakePrs::failing(), FakeState::open()),
            (
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::failing(),
            ),
        ] {
            let merger = FakeMerger::ok();
            let got =
                resolve_and_merge(&plan(), HEAD, &deps(prs, state, Arc::clone(&merger))).await;
            assert!(
                matches!(got, MergeControlOutcome::Failed(ref e) if e.contains("502")),
                "want a Failed carrying gh's complaint, got {got:?}"
            );
            assert!(merger.calls().is_empty(), "nothing may be merged");
        }
    }

    /// A merge GitHub refuses — a conflict, a branch behind `main` — comes back as `Failed` with
    /// `gh`'s own words. Nothing here rebases, force-pushes or retries.
    #[tokio::test]
    async fn a_refused_merge_carries_githubs_complaint() {
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                FakeMerger::failing("X Pull request #64 is not mergeable: merge conflicts"),
            ),
        )
        .await;
        assert!(
            matches!(got, MergeControlOutcome::Failed(ref e) if e.contains("merge conflicts")),
            "{got:?}"
        );
    }

    /// **STUDIO-784, gap 1.** `main` requires a branch to be up to date (`strict: true`) and will
    /// not update one itself (`allow_update_branch: false`), so arming GitHub's auto-merge on a
    /// BEHIND branch parks it forever: green, armed, and unlandable until a human pushes. Refusing
    /// with a reason the operator can act on is the honest answer; reporting *"queued for merge"*
    /// for something that will never happen is not.
    #[tokio::test]
    async fn a_behind_branch_that_cannot_update_itself_is_refused() {
        let merger = FakeMerger::ok();
        let policy = FakePolicy::answering(false);
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps_with(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
                FakeMergeState::at(MERGE_STATE_BEHIND),
                Arc::clone(&policy),
            ),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused(
                "this branch is behind its base and cannot update itself; push or update the \
                 branch, then merge"
            )
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
        assert_eq!(
            policy.asked(),
            vec!["o/r".to_string()],
            "the repository policy is what decides a BEHIND branch, so it must be asked"
        );
    }

    /// The same BEHIND branch on a repository that DOES update pull-request branches merges
    /// normally: GitHub's auto-merge brings it up to date and lands it, so refusing would be a
    /// false refusal. The repository setting is read rather than assumed for exactly this reason.
    #[tokio::test]
    async fn a_behind_branch_the_repository_will_update_still_merges() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps_with(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
                FakeMergeState::at(MERGE_STATE_BEHIND),
                FakePolicy::answering(true),
            ),
        )
        .await;
        assert!(matches!(got, MergeControlOutcome::Applied(_)), "{got:?}");
        assert_eq!(
            merger.calls(),
            vec![("o/r".to_string(), 64, MergeMethod::Squash, true)]
        );
    }

    /// An ordinary merge never asks about the repository's branch policy at all — the question
    /// only arises for a BEHIND branch, so the common path pays for one extra `gh` call and not
    /// two.
    #[tokio::test]
    async fn a_clean_pull_request_never_asks_about_the_branch_policy() {
        let policy = FakePolicy::answering(false);
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps_with(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                FakeMerger::ok(),
                FakeMergeState::clean(),
                Arc::clone(&policy),
            ),
        )
        .await;
        assert!(matches!(got, MergeControlOutcome::Applied(_)), "{got:?}");
        assert!(
            policy.asked().is_empty(),
            "a CLEAN pull request raises no branch-policy question"
        );
    }

    /// Neither lookup may fail QUIETLY into a merge. A `mergeStateStatus` this daemon could not
    /// read, or a repository policy it could not read, is `Failed` carrying `gh`'s own words —
    /// because treating either as "not behind" arms the auto-merge this gate exists to prevent.
    #[tokio::test]
    async fn an_unreadable_merge_state_or_branch_policy_merges_nothing() {
        for (mergestate, policy, want) in [
            (
                FakeMergeState::failing(),
                FakePolicy::answering(false),
                "502",
            ),
            (
                FakeMergeState::at(MERGE_STATE_BEHIND),
                FakePolicy::failing(),
                "403",
            ),
        ] {
            let merger = FakeMerger::ok();
            let got = resolve_and_merge(
                &plan(),
                HEAD,
                &deps_with(
                    FakePrs::at("https://github.com/o/r/pull/64"),
                    FakeState::open(),
                    Arc::clone(&merger),
                    mergestate,
                    policy,
                ),
            )
            .await;
            assert!(
                matches!(got, MergeControlOutcome::Failed(ref e) if e.contains(want)),
                "want a Failed carrying gh's complaint, got {got:?}"
            );
            assert!(merger.calls().is_empty(), "nothing may be merged");
        }
    }

    /// The receipt carries GitHub's mergeability on BOTH legs of the handshake, so the console's
    /// confirm modal and its post-confirm line can say what the pull request is waiting on instead
    /// of implying the merge is done (STUDIO-784).
    #[tokio::test]
    async fn the_receipt_carries_githubs_merge_state() {
        for confirm in ["", HEAD] {
            let got = resolve_and_merge(
                &plan(),
                confirm,
                &deps_with(
                    FakePrs::at("https://github.com/o/r/pull/64"),
                    FakeState::open(),
                    FakeMerger::ok(),
                    FakeMergeState::at("BLOCKED"),
                    FakePolicy::answering(false),
                ),
            )
            .await;
            let receipt = match got {
                MergeControlOutcome::ConfirmRequired(r) | MergeControlOutcome::Applied(r) => r,
                other => panic!("want a receipt, got {other:?}"),
            };
            assert_eq!(receipt.merge_state, "BLOCKED", "confirm={confirm:?}");
        }
    }

    /// **G2, pinned at the call site.** The two constants this module merges with are the sign-off's
    /// (§9.2), and a change to either is a change to what an operator's click can do.
    #[test]
    fn the_merge_is_always_squash_with_githubs_own_auto_merge() {
        assert_eq!(
            (MERGE_METHOD, MERGE_AUTO),
            (MergeMethod::Squash, true),
            "--auto is what makes GitHub, and not this daemon, the thing that decides a pull \
             request is green enough to land"
        );
    }
}
