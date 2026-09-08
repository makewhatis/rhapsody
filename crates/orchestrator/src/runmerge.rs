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
//!   then cross-checked back against the plan's own owner/repo before anything acts on it.
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
    HeadAllowlist, MergeMethod, MergeSource, OpenPrSource, PrLookup, PrStateSource, PrStatus,
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
    /// Head repositories trusted besides the base's own owner. [`HeadAllowlist::none`] on the
    /// daemon — the watcher's default trust boundary, and widening it is a code change.
    pub allow: HeadAllowlist,
}

/// Everything the control task validated before the merge path was allowed to run, and everything
/// the off-loop half is permitted to know.
///
/// Every field is derived from the RUN ROW ([`rhapsody_store::RunSummary`]) — `repo` is written
/// from the project's configured remote and never from an agent — so there is no client-supplied
/// value anywhere in it. That is G1 expressed as a type: the absence of a `number` field is why
/// no code path leads from a request body to `gh pr merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub run_id: i64,
    /// The run's ticket, e.g. `STUDIO-767`. Named in the audit record and the room line.
    pub issue: String,
    pub owner: String,
    pub repo: String,
    /// The run's own branch, already cross-checked against `issue` on the control task.
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
///    `--repo owner/repo`, so a URL naming another repository means something is wrong with an
///    assumption rather than with the operator; it is refused rather than followed. Only the
///    NUMBER is taken from the URL — owner and repo stay the run row's, which is config-derived.
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
    if !found.owner.eq_ignore_ascii_case(&plan.owner)
        || !found.repo.eq_ignore_ascii_case(&plan.repo)
    {
        tracing::warn!(
            run = plan.run_id,
            url = %url,
            want = %format!("{}/{}", plan.owner, plan.repo),
            "console merge: the resolved pull request is in another repository; refusing"
        );
        return MergeControlOutcome::Refused("the resolved pull request is in another repository");
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

    let receipt = MergeReceipt {
        run_id: plan.run_id,
        issue: plan.issue.clone(),
        pr: pr.clone(),
        url,
        number,
        head_sha: snapshot.head_sha.clone(),
        method: MERGE_METHOD.name().to_string(),
        auto: MERGE_AUTO,
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
    use crate::ghsummons::{MergeResult, OpenPrResult, PrSnapshot, PrStateResult, PrStatus};

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

    fn deps(prs: Arc<FakePrs>, state: Arc<FakeState>, merger: Arc<FakeMerger>) -> MergeDeps {
        MergeDeps {
            prs,
            state,
            merger,
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
    /// already scoped to the run's own repository; a URL naming a DIFFERENT repository means an
    /// assumption has broken, and following it would be exactly the "a coordinate is trusted
    /// because of what it says about itself" mistake the review subsystem refuses to make.
    #[tokio::test]
    async fn a_pull_request_resolved_in_another_repository_is_refused() {
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
            MergeControlOutcome::Refused("the resolved pull request is in another repository")
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
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
