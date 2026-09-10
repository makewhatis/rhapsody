//! runmerge — the console merge action's OFF-LOOP half: resolve a run's pull request, refuse
//! everything that should be refused, and perform the one bounded merge (STUDIO-767, slices 1–3 of
//! the design record `~/.rhapsody/docs/STUDIO-767-console-merge-action.md`, §2/§3/§7).
//!
//! The refusals are also READ on their own, without merging, so the console can show one before
//! the click instead of only after it ([`mergeability`], STUDIO-790). Both sides share
//! [`resolve_pull_request`], which is what keeps the reason the header shows and the reason the
//! merge would give from being two implementations that can disagree.
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
//! nothing else. Its callers are [`crate::mergeconsole`]'s [`crate::ControlHandle`] methods, which
//! run on the HTTP request's own task — so a hung `gh` delays that one request. `prstate`'s
//! standing `pr_state_is_never_called_from_the_control_loop` check knows this file by name.
//!
//! Two things about the READ half ([`mergeability`], STUDIO-790) sharpen that, and neither is a
//! click: the console asks on every run-detail mount, and a question takes no single-flight claim
//! (a probe must leave no trace, see [`crate::mergeconsole::MergeIntent`]) — so nothing bounds how
//! many blocking `gh` resolutions can be in flight for one pull request at once. `rhapsodyd` is a
//! multi-thread `#[tokio::main]` with no `spawn_blocking` on this path, so "parks the task" is
//! really "parks a worker thread", from the pool the HTTP server and the control loop share.
//! What keeps that small is the ORDER: `mergeconsole::ticket_not_waiting_in_review` runs before
//! any `gh` and short-circuits every ticket not waiting in review, so an ordinary run detail costs
//! one tracker read and zero GitHub round trips, and only a review-state ticket pays for
//! `gh pr list` + `gh pr view`.
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
//!   is still watching is refused ([`MergePlan::watched`]), one whose newest completed round asked
//!   for changes is refused ([`MergePlan::changes_requested`]), and one GitHub reports as BEHIND on
//!   a repository that will not update the branch is refused rather than armed to never fire.
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

/// The `gh` seams that RESOLVE a run's pull request and judge it — everything the answer needs,
/// and no way to act on it.
///
/// It is split out from [`MergeDeps`] so that the read half is unable to merge rather than merely
/// trusted not to (STUDIO-790). [`resolve_pull_request`] and [`mergeability`] take this and only
/// this, so no branch of the code a GET reaches has a [`MergeSource`] in scope to call — the
/// property is enforced by what those functions are handed, not by a convention a reviewer has to
/// keep.
pub struct ResolveDeps {
    /// Resolves the run's head branch to its open pull request — the ONLY way a number enters.
    pub prs: Arc<dyn OpenPrSource>,
    /// Resolves that number's head SHA and state, for the confirm handshake and the state gate.
    pub state: Arc<dyn PrStateSource>,
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

/// The `gh` seams the merge path drives, and the trust boundary it drives them under. Mirrors
/// [`crate::reviewwatch::ReviewWatchDeps`]: the daemon builds one [`crate::ghsummons::GH`] and
/// hands it in as every seam, and a test hands in fakes.
///
/// The resolution seams and the merge seam are deliberately separate members: the resolve half
/// takes [`ResolveDeps`] alone, so it cannot reach `merge_pr` even by mistake.
pub struct MergeDeps {
    /// Everything needed to resolve and judge the pull request, and nothing that can act on it.
    pub resolve: ResolveDeps,
    /// Performs the merge itself. Reachable only from [`resolve_and_merge`], past the confirm
    /// handshake.
    pub merger: Arc<dyn MergeSource>,
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
    /// Pull-request numbers in `owner/repo` whose newest COMPLETED Rhapsody review round posted
    /// findings — the reviewer asked for changes and no later round has approved it (STUDIO-784).
    ///
    /// Separate from [`watched`](Self::watched) because it is the more dangerous of the two and
    /// earns its own refusal. A round still in flight is a race; a round that FINISHED by
    /// requesting changes is a reviewer's explicit no, and `--auto` does not save us from it —
    /// GitHub enforces nothing here (teammates post verdicts as PR comments, and `main` has no
    /// `required_pull_request_reviews`), so an armed auto-merge lands the moment the author's next
    /// push turns the checks green.
    pub changes_requested: Vec<i64>,
    /// What the ticket-state gate needs in order to be asked, or `None` when this run's project
    /// configures no `review_states` and the question does not arise (STUDIO-784).
    ///
    /// Resolved on the control task because the project resolution lives there; ANSWERED off-loop,
    /// by [`crate::mergeconsole::ticket_not_waiting_in_review`], because answering it is a tracker
    /// read. See that function for why the answer comes from the tracker rather than from the
    /// poller's own snapshot.
    pub review_gate: Option<TicketReviewGate>,
}

/// The ticket-state gate's raw material: which ticket to ask about, and which states count as
/// waiting for review (STUDIO-784).
///
/// Both halves are daemon-derived — the id is `runs.issue_id`, written when the run started, and
/// the states are the run's own project's configured set, normalized. Neither can come from a
/// request body, so this carries no more authority than [`MergePlan`] itself does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketReviewGate {
    /// The ticket's TRACKER id (`runs.issue_id`) — an opaque Linear id, not the `STUDIO-784`
    /// identifier, because it is what the by-ids read filters on.
    pub issue_id: String,
    /// The states a ticket waits for review in, already normalized, so the answer is compared the
    /// way every other state comparison in the daemon is.
    pub states: std::collections::HashSet<String>,
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
///    (§9.3: "refuse while a live review round is watching the PR"), and so is one whose newest
///    completed round asked for changes (STUDIO-784 gap 2) — the more dangerous of the two, since
///    that is a reviewer's explicit no GitHub enforces nothing about. Then the MERGEABILITY gate:
///    a branch GitHub reports as BEHIND, on a repository that will not update it, is refused
///    rather than armed to never fire (STUDIO-784 gap 1).
/// 5. **The confirm handshake**, against the head SHA resolved in step 3 and never against one the
///    caller supplied for itself.
/// 6. **The merge**, at last, as `--squash --auto` and nothing else.
pub async fn resolve_and_merge(
    plan: &MergePlan,
    confirm: &str,
    deps: &MergeDeps,
) -> MergeControlOutcome {
    let receipt = match resolve_pull_request(plan, &deps.resolve).await {
        MergeResolution::Resolved(receipt) => receipt,
        MergeResolution::Refused(why) => return MergeControlOutcome::Refused(why),
        MergeResolution::Failed(err) => return MergeControlOutcome::Failed(err),
    };
    // Constant-time comparison would be theatre here: the value being compared is a public commit
    // SHA that `GET /api/v1/runs/{id}` neighbours already expose, and §3/G3 is explicit that the
    // handshake is a speed bump against mistake and drive-by forgery, not a secret.
    if confirm != receipt.head_sha {
        return MergeControlOutcome::ConfirmRequired(receipt);
    }
    match deps
        .merger
        .merge_pr(
            &plan.owner,
            &plan.repo,
            receipt.number,
            MERGE_METHOD,
            MERGE_AUTO,
        )
        .await
    {
        Ok(said) => {
            tracing::info!(run = plan.run_id, issue = %plan.issue, pr = %receipt.pr, "console merge: merged");
            MergeControlOutcome::Applied(MergeReceipt { said, ..receipt })
        }
        Err(e) => MergeControlOutcome::Failed(e.to_string()),
    }
}

/// What resolving a run's pull request produced — the receipt, or the reason there will not be one.
///
/// It exists so that steps 1-4 above are written ONCE and read the same on both sides of the
/// click. [`resolve_and_merge`] and [`mergeability`] share this function, so a refusal the console
/// shows before the click is not a re-derivation of the daemon's rule but literally the same
/// `&'static str` the merge would have refused with (STUDIO-790).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResolution {
    /// The pull request resolved and every refusal was passed. Nothing has been merged.
    Resolved(MergeReceipt),
    /// One of the refusals fired, and this is its sentence.
    Refused(&'static str),
    /// A `gh` seam could not answer, and this is its own complaint verbatim.
    Failed(String),
}

/// Steps 1-4 of [`resolve_and_merge`]: resolve `plan`'s pull request and apply every refusal, up to
/// but NOT including the confirm handshake and the merge.
///
/// **Nothing in this function can merge anything**, and that is the point rather than an accident:
/// it takes [`ResolveDeps`], which holds no [`MergeSource`], so the read path that calls it
/// ([`mergeability`], serving a GET) has nothing to reach `gh pr merge` WITH, down any branch.
/// That is the same discipline §3/G1 applies to the request type — a property of what the code is
/// handed rather than of a flag someone has to pass correctly.
pub async fn resolve_pull_request(plan: &MergePlan, deps: &ResolveDeps) -> MergeResolution {
    let url = match deps
        .prs
        .open_pr_for_branch(&plan.owner, &plan.repo, &plan.branch)
        .await
    {
        // The URL alone: the console merge resolves a NUMBER, and `OpenPr::head_sha` belongs to
        // the review quorum's repeat-guard rather than to anything here.
        Ok(Some(pr)) => pr.url,
        Ok(None) => {
            return MergeResolution::Refused("no open pull request on this run's branch");
        }
        Err(e) => return MergeResolution::Failed(e.to_string()),
    };
    // The number, and ONLY the number, is taken from GitHub's answer. `parse_pr_ref` fails closed
    // on anything that is not a positive number under a two-segment owner/repo path.
    let Some(found) = parse_pr_ref(&url) else {
        return MergeResolution::Refused("the pull request's URL could not be read");
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
        return MergeResolution::Refused("the resolved pull request belongs to another account");
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
            return MergeResolution::Refused("GitHub cannot resolve that pull request");
        }
        Ok(PrLookup::Untrusted) => {
            return MergeResolution::Refused("the pull request's head repository is not this one");
        }
        Err(e) => return MergeResolution::Failed(e.to_string()),
    };
    match snapshot.status {
        PrStatus::Merged => {
            return MergeResolution::Refused("that pull request is already merged");
        }
        PrStatus::Closed => return MergeResolution::Refused("that pull request is closed"),
        PrStatus::Open => {}
    }
    if plan.watched.contains(&number) {
        return MergeResolution::Refused("a Rhapsody review of that pull request is still live");
    }
    // The reviewer's explicit no, and the one the design's liveness-only gate let through
    // (STUDIO-784 gap 2). GitHub cannot hold this line for us — the verdict is a PR COMMENT, not a
    // formal review, and `main` requires no approving review — so the daemon's own ledger is the
    // only place the fact lives.
    if plan.changes_requested.contains(&number) {
        // "has not pushed since" and not "has not been re-reviewed": a head advance RE-ARMS a
        // `reviewed` row (`REVIEW_STATUS_REVIEWED`'s own doc), so the author's next push clears
        // this block whether or not anyone read the new head. The refusal says what the ledger
        // actually holds.
        return MergeResolution::Refused(
            "a Rhapsody review of that pull request asked for changes and the author has not pushed since",
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
        Err(e) => return MergeResolution::Failed(e.to_string()),
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
                return MergeResolution::Refused(
                    "this branch is behind its base and cannot update itself; push or update the \
                     branch, then merge",
                );
            }
            // BEHIND plus an UNKNOWN policy is the one case where refusing beats reporting the
            // fault. `allow_update_branch` is only in the repository payload for a token with
            // admin permission, so a push-only token yields `null` and this read fails on a
            // perfectly healthy repository — and the refusal below is true regardless of how the
            // read went: the branch IS behind, and the only thing in doubt is whether GitHub would
            // fix that itself. An operator can act on "push the branch"; they cannot act on a
            // `gh` error. The fault still reaches the log rather than vanishing.
            Err(e) => {
                tracing::warn!(
                    run = plan.run_id,
                    issue = %plan.issue,
                    pr = %pr,
                    err = %e,
                    "console merge: the branch is behind its base and the repository's \
                     branch-update policy could not be read; refusing"
                );
                return MergeResolution::Refused(
                    "this branch is behind its base and cannot update itself; push or update the \
                     branch, then merge",
                );
            }
        }
    }

    let receipt = MergeReceipt {
        run_id: plan.run_id,
        issue: plan.issue.clone(),
        pr,
        url,
        number,
        head_sha: snapshot.head_sha.clone(),
        method: MERGE_METHOD.name().to_string(),
        auto: MERGE_AUTO,
        merge_state,
        said: String::new(),
    };
    MergeResolution::Resolved(receipt)
}

/// What the daemon would answer if the operator clicked **Merge** right now (STUDIO-790).
///
/// A deliberately NARROWER type than [`MergeControlOutcome`]: it has no `Applied` arm and no
/// `ConfirmRequired` arm, because the read that produces it never merges and never asks for a
/// confirmation. The console renders a live Merge control from [`Mergeable`](Self::Mergeable) and
/// a reason-bearing disabled one from [`Refused`](Self::Refused), so an answer this type cannot
/// hold is an answer the header cannot claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeabilityOutcome {
    /// Teams is off, so there is no merge path at all — [`MergeControlOutcome::Dormant`]'s twin.
    Dormant,
    /// No run has that id.
    NotFound,
    /// The daemon resolved this run's pull request and would proceed to the confirm handshake.
    /// The receipt is the same one the handshake's first leg would carry.
    Mergeable(MergeReceipt),
    /// The daemon would refuse, and this is the sentence it would refuse with — the SAME
    /// `&'static str`, not a console-side paraphrase of it.
    Refused(&'static str),
    /// The question could not be answered; the payload is the failing seam's own complaint.
    Failed(String),
}

impl MergeabilityOutcome {
    /// Reads a plan-time denial as an answer to the question instead of as a refused attempt.
    ///
    /// [`crate::mergeconsole::MergePlanOutcome::Denied`] is documented to carry only `Dormant`,
    /// `NotFound`, `Refused` or `Failed` — nothing is resolved at plan time, so neither of the
    /// other two can be built there. They are still mapped rather than unwrapped: a panic is not
    /// how this daemon says "impossible" on a path an HTTP request drives.
    pub fn from_denial(denial: MergeControlOutcome) -> Self {
        match denial {
            MergeControlOutcome::Dormant => Self::Dormant,
            MergeControlOutcome::NotFound => Self::NotFound,
            MergeControlOutcome::Refused(why) => Self::Refused(why),
            MergeControlOutcome::Failed(err) => Self::Failed(err),
            MergeControlOutcome::ConfirmRequired(_) | MergeControlOutcome::Applied(_) => {
                Self::Failed(
                    "the merge planner answered with a resolved pull request, which it cannot"
                        .to_string(),
                )
            }
        }
    }
}

/// Answers "could this run's pull request be merged?" without merging it (STUDIO-790).
///
/// The console needs the refusal BEFORE the click — a control that only learns it was impossible
/// afterwards is the bug this exists to fix — and the daemon already knew: this is
/// [`resolve_pull_request`], the very ladder the merge walks, read for its answer instead of acted
/// on. So the console's disabled tooltip carries the daemon's own words rather than a second
/// implementation of the same rules that could drift from them.
///
/// It cannot merge anything, structurally: it is handed [`ResolveDeps`], which carries no
/// [`MergeSource`] to call, and there is no `confirm` parameter here for a caller to supply one
/// through. Its caller takes no single-flight claim and writes no audit record either — see
/// [`crate::mergeconsole::MergeIntent`] for why a question must leave neither trace.
pub async fn mergeability(plan: &MergePlan, deps: &ResolveDeps) -> MergeabilityOutcome {
    match resolve_pull_request(plan, deps).await {
        MergeResolution::Resolved(receipt) => MergeabilityOutcome::Mergeable(receipt),
        MergeResolution::Refused(why) => MergeabilityOutcome::Refused(why),
        MergeResolution::Failed(err) => MergeabilityOutcome::Failed(err),
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
            Ok(self.url.map(|u| crate::ghsummons::OpenPr {
                url: u.to_string(),
                head_sha: String::new(),
            }))
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
            changes_requested: Vec::new(),
            // The `gh` half never reads it: the ticket-state gate is answered before this half
            // runs at all (`mergeconsole::ticket_not_waiting_in_review`).
            review_gate: None,
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
            resolve: ResolveDeps {
                prs,
                state,
                mergestate,
                policy,
                allow: HeadAllowlist::none(),
            },
            merger,
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

    /// **STUDIO-784, gap 2 — the refusal the design's liveness-only gate let through.** A round
    /// that FINISHED by requesting changes is a reviewer's explicit no, and it is the most
    /// dangerous state of all: GitHub enforces nothing here (a teammate's verdict is a PR comment,
    /// not a formal review, and `main` requires no approving review), so an armed auto-merge lands
    /// the moment the author's next push turns the checks green.
    #[tokio::test]
    async fn a_finished_review_round_that_asked_for_changes_refuses_the_merge() {
        let merger = FakeMerger::ok();
        let blocked = MergePlan {
            changes_requested: vec![64],
            ..plan()
        };
        let got = resolve_and_merge(
            &blocked,
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
            MergeControlOutcome::Refused(
                "a Rhapsody review of that pull request asked for changes and the author has not pushed since"
            )
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");

        // A requested change on some OTHER pull request in the same repository is not this one's.
        let elsewhere = MergePlan {
            changes_requested: vec![63, 65],
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

    /// **STUDIO-790, the whole point.** The read answers with the receipt the click's first leg
    /// would have asked the operator to confirm — and merges nothing on the way to saying so.
    #[tokio::test]
    async fn mergeability_answers_the_receipt_the_click_would_ask_to_confirm() {
        let merger = FakeMerger::ok();
        let deps = deps(
            FakePrs::at("https://github.com/o/r/pull/64"),
            FakeState::open(),
            Arc::clone(&merger),
        );

        let got = mergeability(&plan(), &deps.resolve).await;

        let MergeabilityOutcome::Mergeable(receipt) = got else {
            panic!("want Mergeable, got {got:?}");
        };
        // The same receipt, field for field, that the handshake's first leg carries — so the
        // console renders one thing before the click and confirms the same thing at it.
        let MergeControlOutcome::ConfirmRequired(asked) =
            resolve_and_merge(&plan(), "", &deps).await
        else {
            panic!("the unconfirmed click must still be a confirm_required");
        };
        assert_eq!(receipt, asked);
        assert_eq!(receipt.head_sha, HEAD);
        assert!(
            merger.calls().is_empty(),
            "asking whether a merge is possible must never perform one"
        );
    }

    /// A refusal reaches the console VERBATIM, as the same `&'static str` the click would have been
    /// refused with. If these two ever diverge the header starts telling its own story.
    #[tokio::test]
    async fn mergeability_gives_the_same_refusal_the_merge_would() {
        let merger = FakeMerger::ok();
        let deps = deps(
            FakePrs::at("https://github.com/o/r/pull/64"),
            FakeState::found(PrStatus::Merged, HEAD),
            Arc::clone(&merger),
        );

        let read = mergeability(&plan(), &deps.resolve).await;
        let clicked = resolve_and_merge(&plan(), HEAD, &deps).await;

        assert_eq!(
            read,
            MergeabilityOutcome::Refused("that pull request is already merged")
        );
        assert_eq!(
            clicked,
            MergeControlOutcome::Refused("that pull request is already merged"),
            "the reason shown before the click is the reason the click gives"
        );
        assert!(merger.calls().is_empty());
    }

    /// The refusal a REAL merged pull request produces, and the one STUDIO-790's own motivating
    /// ticket will show. `open_pr_for_branch` filters `--state open`, so a merged pull request
    /// drops out at resolution rather than reaching the `PrStatus::Merged` arm above — which is
    /// why that arm's parity test is not the whole story and this one exists beside it.
    #[tokio::test]
    async fn a_branch_whose_pull_request_has_merged_is_refused_at_resolution() {
        let merger = FakeMerger::ok();
        let deps = deps(FakePrs::none(), FakeState::open(), Arc::clone(&merger));

        assert_eq!(
            mergeability(&plan(), &deps.resolve).await,
            MergeabilityOutcome::Refused("no open pull request on this run's branch")
        );
        assert_eq!(
            resolve_and_merge(&plan(), HEAD, &deps).await,
            MergeControlOutcome::Refused("no open pull request on this run's branch"),
            "and the click says exactly the same thing"
        );
        assert!(merger.calls().is_empty());
    }

    /// A `gh` seam that cannot answer is reported as a failure to ANSWER, never as a refusal — the
    /// console must not print "the daemon says no" when what happened is that nobody could ask.
    #[tokio::test]
    async fn an_unanswerable_question_is_a_failure_and_not_a_refusal() {
        let merger = FakeMerger::ok();
        let got = mergeability(
            &plan(),
            &deps(FakePrs::failing(), FakeState::open(), Arc::clone(&merger)).resolve,
        )
        .await;

        let MergeabilityOutcome::Failed(err) = got else {
            panic!("want Failed, got {got:?}");
        };
        assert!(err.contains("gh pr list"), "{err}");
        assert!(merger.calls().is_empty());
    }

    /// The read path cannot merge, and the reason is the TYPE rather than a flag or a convention:
    /// [`resolve_pull_request`] and [`mergeability`] are handed [`ResolveDeps`], which holds no
    /// [`MergeSource`], so neither they nor anything they call has one in scope. The compiler is
    /// the real check — a `deps.merger` in there does not build — and these two assertions guard
    /// the boundary that makes it work: that the read half still takes `ResolveDeps`, and that
    /// `ResolveDeps` has not quietly grown a merge seam of its own.
    ///
    /// Both are read off this module's own source because neither can be asserted at run time: a
    /// widening is a compiling change, and it should be a failing test rather than a review
    /// someone has to catch.
    #[test]
    fn the_resolve_half_is_never_handed_the_merge_seam() {
        let src = include_str!("runmerge.rs");
        for signature in [
            "pub async fn resolve_pull_request(plan: &MergePlan, deps: &ResolveDeps)",
            "pub async fn mergeability(plan: &MergePlan, deps: &ResolveDeps)",
        ] {
            assert!(
                src.contains(signature),
                "`{signature}` is gone: the read a GET serves must be handed ResolveDeps and \
                 never the whole MergeDeps, or it can reach `gh pr merge` (STUDIO-790)"
            );
        }

        let start = src
            .find("pub struct ResolveDeps {")
            .expect("the resolve half's dependency set is still called ResolveDeps");
        let end = src[start..]
            .find("\n}")
            .expect("ResolveDeps is still a braced struct");
        let body = &src[start..start + end];
        for forbidden in ["MergeSource", "merger"] {
            assert!(
                !body.contains(forbidden),
                "ResolveDeps gained `{forbidden}`: the read that a GET serves must not be able to \
                 merge anything (STUDIO-790)"
            );
        }
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

    /// Neither lookup may fail QUIETLY into a merge — but the two failures answer differently,
    /// and deliberately.
    ///
    /// A `mergeStateStatus` this daemon could not read says nothing about the pull request at all,
    /// so it is `Failed` carrying `gh`'s own words. An unreadable BRANCH POLICY is the one case
    /// where a refusal is both safer and more useful: `allow_update_branch` is absent from the
    /// repository payload for any token without admin permission, so this read fails on a healthy
    /// repository — and the refusal is true either way, because the branch is behind and only the
    /// question of who fixes that is unknown. Neither may merge anything.
    #[tokio::test]
    async fn an_unreadable_merge_state_or_branch_policy_merges_nothing() {
        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps_with(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
                FakeMergeState::failing(),
                FakePolicy::answering(false),
            ),
        )
        .await;
        assert!(
            matches!(got, MergeControlOutcome::Failed(ref e) if e.contains("502")),
            "an unreadable merge state is a fault, not a refusal: {got:?}"
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");

        let merger = FakeMerger::ok();
        let got = resolve_and_merge(
            &plan(),
            HEAD,
            &deps_with(
                FakePrs::at("https://github.com/o/r/pull/64"),
                FakeState::open(),
                Arc::clone(&merger),
                FakeMergeState::at(MERGE_STATE_BEHIND),
                FakePolicy::failing(),
            ),
        )
        .await;
        assert_eq!(
            got,
            MergeControlOutcome::Refused(
                "this branch is behind its base and cannot update itself; push or update the \
                 branch, then merge"
            ),
            "a behind branch with an unreadable policy gets the actionable refusal"
        );
        assert!(merger.calls().is_empty(), "nothing may be merged");
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
