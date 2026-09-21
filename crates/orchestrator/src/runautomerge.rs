//! runautomerge — the auto-merge's OFF-LOOP half: ask GitHub everything the control task could
//! not, and perform the one bounded merge (STUDIO-874).
//!
//! **No Go v0.4.0 counterpart.** [`crate::runmerge`]'s sibling, and deliberately its shape: every
//! `gh` call is out here, the module holds no [`Orchestrator`](crate::orchestrator::Orchestrator),
//! sends no control event, and the only lock it shares with the control task is
//! [`AutoMergeLedger`]'s — taken read-only, through [`AutoMergeLedger::peek`] (see below) — so a
//! slow `gh` delays the watcher's next tick and nothing else. The DECISION that needs loop state —
//! the head-keyed reviewer verdicts — was made in [`crate::automerge`] and arrives here already
//! made, as an [`AutoMergePlan`].
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
//! # Only `CLEAN` proceeds — and `CLEAN` is not sufficient
//!
//! The `mergeStateStatus` gate is an ALLOWLIST of one. GitHub's vocabulary here is open and has
//! grown before, and every other value it currently spells is a reason not to merge — `DRAFT`,
//! `DIRTY` (a conflict), `BLOCKED`, `UNSTABLE`, `UNKNOWN` (GitHub is still computing). A blocklist
//! of the ones known today would merge on the one added tomorrow, which is this batch's signature
//! defect: a guard that does not guard. `BEHIND` is the single value with a branch of its own,
//! because it is the one that can be CLEARED — see below.
//!
//! ⚠️ It is nonetheless NECESSARY and not sufficient, and STUDIO-881 is what that cost: a DRAFT
//! pull request with approvals at the head and green checks reports `CLEAN`, not `DRAFT`. Both of
//! the live pull requests that motivated that ticket did. So `isDraft` is read separately, off the
//! snapshot step 1 already takes, and refused there — see [`DECLINE_DRAFT`]. Reading the state
//! GitHub volunteers was never going to be enough on its own; the field that answers the question
//! has to be asked for.
//!
//! The check rollup is then read anyway, at the same head, even though `CLEAN` already means
//! GitHub's required contexts passed. Two independent reads of "is it green" is the ticket's
//! explicit ask (the slow `desktop` job among them), and they fail in different ways: `CLEAN` is
//! GitHub's judgement about REQUIRED contexts under branch protection, while the rollup is every
//! check that ran. A required context nobody marked required yet is invisible to the first and
//! visible to the second.
//!
//! That rollup is resolved per check NAME, not per entry — one head carries several entries for a
//! name and the non-green ones are routinely superseded rather than owed. See [`blocking_check`].
//!
//! # A refusal is not an unreadable gate
//!
//! Every failed `gh pr merge` used to become [`AutoMergeOutcome::Failed`], logged as *"a gate could
//! not be read"*. That phrase is a claim — that nothing is KNOWN, and the next tick may learn more
//! — and for `GraphQL: Pull Request is still a draft` it is false: the gate was read perfectly and
//! the answer was no. The daemon re-asked it every minute for three hours, 182 times, and each
//! attempt cost the three reads that precede it as well.
//!
//! [`classify_merge_error`] draws the line at whether GitHub ANSWERED, and the default is to retry,
//! because the fail-open direction here is abandoning a mergeable pull request on a blip. Nothing
//! latches: a refusal is re-decided from a fresh reading of GitHub on the next tick, exactly like
//! the gates above it. What does not repeat is the REPORT — see [`AutoMergeLedger`], which is the
//! only state this half keeps and holds what has been SAID rather than what GitHub answered. The
//! detail lines the gates emit on the way to a refusal go through the same verdict
//! ([`refuse_loudly`]), because a line spoken before the ledger rules repeats however the ledger
//! rules.
//!
//! A refusal is deliberately NOT surfaced outside the log as well — not on the run, not on the pull
//! request, not the way [`CREDENTIAL_DEAD_WARNING`](crate::preflight) reaches `/api/v1/projects` per
//! project. That warning rides loop-owned state; this module holds no `Orchestrator`, sends no
//! control event, and shares no lock with the control task beyond the ledger's own — which is its
//! whole containment guarantee for the `gh` I/O this half performs (see the opening paragraph).
//! Surfacing from here beyond the one exception below needs a new control event, which is a design
//! change rather than a bug fix, and the volume problem that prompted the question is answered at
//! its source, on BOTH sides of the seam: the control task announces a plan once per head
//! ([`crate::reviewwatch`]) and this half announces its refusal once per head and reason, where the
//! two of them together logged 383 lines for two stuck pull requests in three hours.
//!
//! **One narrow, reviewed exception** (STUDIO-923): [`AutoMergeLedger::peek`] lets
//! [`crate::reviewreconcile`]'s sweep — itself a human-facing report of an approved pull request
//! stuck open — READ the reason already on record, so its own warning can name it instead of
//! claiming nobody has said anything. That is not the control event this doc argues against: the
//! sweep already runs on the control task and already reports this exact pull request on its own
//! terms; `peek` hands it a fact this process already has rather than teaching this module to push
//! anywhere. Nothing here writes the ledger from outside, arms a merge, or reaches the run, the
//! tracker, the pull request or the store — see `reviewreconcile::set_review_divergences`, `peek`'s
//! one caller.
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::automerge::AutoMergePlan;
use crate::ghsummons::{
    BranchUpdateSource, BranchUpdater, CheckRun, HeadAllowlist, MERGE_STATE_BEHIND, MergeMethod,
    MergeSource, MergeStateSource, PrChecksSource, PrLookup, PrStateSource, PrStatus,
};
use crate::prstate::PrCoord;

/// How an auto-merge merges. `--squash` is the repository's convention — the squash subject is the
/// pull-request title release-please parses — matching [`crate::runmerge::MERGE_METHOD`].
pub const AUTO_MERGE_METHOD: MergeMethod = MergeMethod::Squash;

/// The one `mergeStateStatus` an auto-merge proceeds on. See the module doc: an allowlist, because
/// GitHub's vocabulary is open.
pub const MERGE_STATE_CLEAN: &str = "CLEAN";

/// The refusal a DRAFT pull request gets (STUDIO-881). Named because two gates answer with it —
/// the snapshot read below, and the classifier that reads GitHub's own refusal if a draft somehow
/// reaches the merge anyway.
const DECLINE_DRAFT: &str = "the pull request is still a draft";

/// The one conclusion that says a check NAME judged this head and passed. Named because
/// [`blocking_check`] asks about it specifically, not merely as a member of
/// [`NON_BLOCKING_CHECKS`].
const CHECK_SUCCESS: &str = "SUCCESS";

/// The one conclusion an entry can carry and still be SUPERSEDED by a green sibling of its own
/// name. Named for the same reason as [`CHECK_SUCCESS`]: [`blocking_check`] asks about it
/// specifically, and the rescue is deliberately this narrow.
const CHECK_CANCELLED: &str = "CANCELLED";

/// Check conclusions that do not BLOCK a merge.
///
/// `SKIPPED` and `NEUTRAL` are non-failures — a path-filtered job reports one and would otherwise
/// block a pull request forever. They are safe to admit here only because
/// [`MERGE_STATE_CLEAN`] has already been required, which is GitHub's own verdict that every
/// REQUIRED context is satisfied; this list judges the rest. Everything else — `FAILURE`,
/// `CANCELLED`, `TIMED_OUT`, `ACTION_REQUIRED`, `IN_PROGRESS`, `QUEUED`, `PENDING`, and any state
/// GitHub adds later — blocks, unless a green sibling supersedes it: see [`blocking_check`].
const NON_BLOCKING_CHECKS: [&str; 3] = [CHECK_SUCCESS, "SKIPPED", "NEUTRAL"];

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
    /// What this half has already SAID, and the only state it keeps. See [`AutoMergeLedger`].
    ///
    /// `Arc`-held, not owned outright, so the SAME ledger can be shared with
    /// `Orchestrator::automerge_ledger` (STUDIO-923) — a read-only handle the control task's
    /// reconciliation sweep uses through [`AutoMergeLedger::peek`]. This half remains the only
    /// writer; the `Arc` exists to let a second reader in, not a second writer.
    pub ledger: Arc<AutoMergeLedger>,
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
    /// The same gate refused for the same reason at the same head as last time, so there is
    /// nothing new to say (STUDIO-881). The decision is identical to [`AutoMergeOutcome::Declined`]
    /// — nothing merged — and it is reported apart from it only so the caller can stay quiet: a
    /// refusal repeated once a minute for three hours is not a signal anyone reads, and it is
    /// indistinguishable from a daemon that is stuck.
    Held(&'static str),
    /// A lookup could not be MADE. Distinct from a refusal: nothing is known, so nothing is
    /// concluded and the next tick asks again.
    Failed(String),
}

/// Runs every gate that needs GitHub and, if they all clear, merges `plan`'s pull request.
///
/// The order is cheapest-refusal-first and, more importantly, safest-first: the pull request is
/// re-resolved before anything else, so a merged, closed, DRAFT or moved head costs one call and
/// stops.
pub async fn perform_auto_merge(plan: &AutoMergePlan, deps: &AutoMergeDeps) -> AutoMergeOutcome {
    let outcome = attempt_auto_merge(plan, deps).await;
    // A pull request that MOVED is a change of subject, so its next refusal is news again. Done in
    // one place rather than on each of the returns that qualify, because a path that forgot would
    // go silent instead of loud and nothing would notice.
    //
    // A `Failed` deliberately does not qualify, though it is not a refusal either: nothing was
    // learned, so the subject did not change. Forgetting on one would let a single flaky `gh pr
    // view` between two draft ticks re-announce the same refusal, and a source flapping every
    // other tick would restore half the noise this ledger removes. Nothing is lost by keeping it:
    // a refusal that has genuinely cleared reappears as a `Merged`, an `Updated` or a different
    // `why`, and all three are news on their own.
    if matches!(
        outcome,
        AutoMergeOutcome::Merged(_) | AutoMergeOutcome::Updated
    ) {
        deps.ledger.forget(&plan.pr);
    }
    outcome
}

async fn attempt_auto_merge(plan: &AutoMergePlan, deps: &AutoMergeDeps) -> AutoMergeOutcome {
    let (owner, repo, number) = (&plan.pr.owner, &plan.pr.repo, plan.pr.number);

    // 1. Is this still the pull request the verdicts were about? The control task decided from a
    //    watch-set snapshot and an observation from earlier in the tick; both can be stale by now.
    let snap = match deps.prs.pr_state(owner, repo, number, &deps.allow).await {
        Ok(PrLookup::Found(snap)) => snap,
        Ok(PrLookup::Gone) => return refuse(plan, deps, "the pull request is gone"),
        Ok(PrLookup::Untrusted) => {
            return refuse(plan, deps, "the head repository is not trusted");
        }
        Err(e) => return AutoMergeOutcome::Failed(e.to_string()),
    };
    if snap.status != PrStatus::Open {
        return refuse(plan, deps, "the pull request is no longer open");
    }
    // The head moved between the observation and now, so every verdict on record is about a commit
    // that is no longer what would land. `--match-head-commit` below would catch this too; catching
    // it here spends no merge attempt and says so precisely.
    //
    // Case-folded, unlike `automerge::auto_merge_verdict`'s deliberately exact comparison, and the
    // difference is principled rather than an oversight: that one compares against a STORED row
    // that two other predicates also compare exactly, so it must not be the loosest of the three.
    // This compares two answers from GitHub about the same field, where a case difference would
    // mean the same commit — so folding can only avoid a FALSE refusal, never admit a wrong head.
    if !snap.head_sha.eq_ignore_ascii_case(&plan.head) {
        return refuse(plan, deps, "the head moved after the verdicts were read");
    }
    // A DRAFT is a definite no, and it is invisible to every gate below: GitHub reports a draft
    // with approvals and green checks as `CLEAN` (STUDIO-881 measured exactly that on both of the
    // pull requests it was filed for), so only `isDraft` catches it. Refusing HERE, off the
    // snapshot step 1 has already paid for, is what stops the merge_state/pr_checks reads and the
    // `gh pr merge` write that used to follow — 182 of them in the three hours the ticket covers.
    //
    // Re-read from GitHub on every tick, and deliberately not remembered: marking a draft ready
    // for review does NOT move the head, so a gate that latched on this answer would strand a pull
    // request the author had already un-drafted. Only the REPORT is de-duplicated — see `refuse`.
    //
    // `draft_blocks_merge`, not a bare bool (STUDIO-962): the gate refuses unless GitHub POSITIVELY
    // said the pull request is not a draft, and the reader carries that default itself so the draft
    // poke can take the opposite one from the same unstated answer.
    if snap.draft_blocks_merge() {
        return refuse(plan, deps, DECLINE_DRAFT);
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
        return refuse_loudly(
            plan,
            deps,
            "GitHub does not report the pull request as mergeable",
            || {
                tracing::info!(
                    pr = %plan.pr, %state,
                    "auto-merge: declining a pull request GitHub does not report as CLEAN"
                )
            },
        );
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
        return refuse(plan, deps, "no checks have reported on this head");
    }
    if let Some(bad) = blocking_check(&checks) {
        return refuse_loudly(plan, deps, "a check is failing or has not finished", || {
            tracing::info!(
                pr = %plan.pr, check = %bad.name, state = %bad.state,
                "auto-merge: declining on a check that is not green"
            )
        });
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
            // The one audit line for a merge. The caller logs nothing further: two INFO lines
            // saying `auto-merge: merged` read as two merges.
            tracing::info!(
                pr = %plan.pr, head = %plan.head, approved_by = ?plan.approved_by, said = %said,
                "auto-merge: merged"
            );
            AutoMergeOutcome::Merged(said)
        }
        Err(e) => classify_merge_error(plan, deps, e.to_string()),
    }
}

/// The `gh pr merge` errors that are GitHub ANSWERING "no", mapped to the refusal each one is.
///
/// An ALLOWLIST, and small on purpose. Everything absent from it — a network error, an `HTTP 503`,
/// a message GitHub adds next year — is a FAILURE and is asked again next tick, because the
/// fail-open direction here is abandoning a mergeable pull request on a blip. The motivating log
/// carries both shapes against the same pull request within the same hour: `GraphQL: Pull Request
/// is still a draft (mergePullRequest)`, which will say the same thing forever, and `HTTP 503:
/// Service Unavailable`, which was gone on the next tick.
///
/// A phrase earns a place here by being observed LOOPING, not by seeming plausible.
const TERMINAL_MERGE_ERRORS: [(&str, &str); 2] = [
    // STUDIO-881's own error, measured 182 times in three hours. Reachable even with the `isDraft`
    // gate above, because that gate reads a snapshot taken one call earlier.
    ("pull request is still a draft", DECLINE_DRAFT),
    // A conflict with the base. `MERGE_STATE_CLEAN` normally catches this first; when it does not,
    // the merge cannot succeed until somebody pushes — and a push moves the head, which re-gates
    // the pull request from the top.
    (
        "pull request is not mergeable",
        "GitHub does not report the pull request as mergeable",
    ),
];

/// What a failed `gh pr merge` was: GitHub refusing, or GitHub not answering.
///
/// **This is the distinction the module was missing** (STUDIO-881). Every error from the merge used
/// to become [`AutoMergeOutcome::Failed`] — logged as *"a gate could not be read"* — which is a
/// claim that nothing is KNOWN and the next tick may learn more. For `Pull Request is still a
/// draft` that claim is simply false: the gate was read perfectly and the answer was no, and the
/// daemon re-asked it once a minute for three hours to be told the same thing.
///
/// So the rule is about whether an answer was GIVEN, not about how bad it was:
///
/// * GitHub refused, in words this daemon recognises ⇒ a [`AutoMergeOutcome::Declined`], reported
///   like any other gate's refusal and announced once (see [`AutoMergeLedger`]).
/// * Anything else ⇒ [`AutoMergeOutcome::Failed`], retried next tick.
///
/// Nothing here latches. A refusal is re-decided from a fresh reading of GitHub on the next tick,
/// exactly like the gates above it, so a draft marked ready or a conflict resolved clears on its
/// own without bookkeeping. What does not repeat is the REPORT.
fn classify_merge_error(
    plan: &AutoMergePlan,
    deps: &AutoMergeDeps,
    err: String,
) -> AutoMergeOutcome {
    let lower = err.to_ascii_lowercase();
    match TERMINAL_MERGE_ERRORS
        .iter()
        .find(|(marker, _)| lower.contains(marker))
    {
        Some((_, why)) => refuse_loudly(plan, deps, why, || {
            tracing::info!(
                pr = %plan.pr, head = %plan.head, %err,
                "auto-merge: GitHub refused the merge; this is a refusal, not an unreadable gate"
            )
        }),
        None => AutoMergeOutcome::Failed(err),
    }
}

/// Reports `why` as this pull request's refusal, quietly if it is the same refusal as last time.
///
/// See [`AutoMergeLedger`] for why the second and later reports are held rather than repeated.
fn refuse(plan: &AutoMergePlan, deps: &AutoMergeDeps, why: &'static str) -> AutoMergeOutcome {
    deps.ledger.refuse(&plan.pr, &plan.head, why)
}

/// [`refuse`], plus a `detail` line that only the ANNOUNCED refusal emits.
///
/// The ledger de-duplicates what the CALLER says about an outcome; it cannot reach a `tracing`
/// call this module makes on its own way to a refusal. Four gates want to name something their
/// fixed refusal phrase cannot carry — the mergeable state GitHub reported, the check that is red,
/// `gh`'s own words, the branch-update policy that would not read — and each of them holds for as
/// long as the condition does, so speaking BEFORE the ledger has ruled is one line a minute no
/// matter what the ledger decides. A red non-required check is the ordinary case: it leaves
/// `mergeStateStatus` at `CLEAN`, so it is refused here and nowhere else, forever (see
/// [`blocking_check`]).
///
/// `detail` therefore runs after the verdict and only on [`AutoMergeOutcome::Declined`]. It is a
/// closure rather than a pre-formatted string so that a held refusal does no formatting at all.
fn refuse_loudly(
    plan: &AutoMergePlan,
    deps: &AutoMergeDeps,
    why: &'static str,
    detail: impl FnOnce(),
) -> AutoMergeOutcome {
    let outcome = refuse(plan, deps, why);
    if matches!(outcome, AutoMergeOutcome::Declined(_)) {
        detail();
    }
    outcome
}

/// The refusal this half has already ANNOUNCED for each pull request, and the only state it keeps.
///
/// It remembers what was SAID, never what GitHub answered: every gate above is re-read from GitHub
/// on every tick and re-decided from scratch, so nothing here can strand a pull request whose
/// refusal has since cleared. Marking a draft ready for review, resolving a conflict and a slow
/// check turning green all clear on the very next tick, and the ledger has no say in it.
///
/// What it stops is the noise. STUDIO-881's daemon logged the same refusal every minute for three
/// hours; as the ticket puts it, a refusal that repeats forever is indistinguishable from a daemon
/// that is stuck. So a refusal is announced when it is NEWS — the first time it is decided, and
/// again whenever the reason or the head changes — and [`AutoMergeOutcome::Held`] otherwise.
///
/// A pull request that MOVED — merged, or updated onto a new head — is forgotten, so its next
/// refusal is news again. A [`AutoMergeOutcome::Failed`] deliberately is not, even though it is no
/// refusal: nothing was learned, so the subject did not change, and forgetting on one would let a
/// flaky source re-announce the same refusal every other tick.
///
/// The map holds one entry per pull request this daemon has refused, which the open watch set
/// bounds in practice; [`LEDGER_CAPACITY`] bounds it in a daemon that runs for months anyway, at a
/// cost of one repeated log line per pull request on the tick it is emptied.
#[derive(Default)]
pub struct AutoMergeLedger(Mutex<HashMap<PrCoord, (String, &'static str)>>);

/// See [`AutoMergeLedger`]. Comfortably above any plausible open watch set.
const LEDGER_CAPACITY: usize = 256;

impl AutoMergeLedger {
    fn refuse(&self, pr: &PrCoord, head: &str, why: &'static str) -> AutoMergeOutcome {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= LEDGER_CAPACITY && !seen.contains_key(pr) {
            seen.clear();
        }
        match seen.insert(pr.clone(), (head.to_string(), why)) {
            Some((was_head, was_why)) if was_head == head && was_why == why => {
                AutoMergeOutcome::Held(why)
            }
            _ => AutoMergeOutcome::Declined(why),
        }
    }

    fn forget(&self, pr: &PrCoord) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(pr);
    }

    /// The reason this half most recently declined `pr`, or `None` when it holds nothing for that
    /// coordinate — auto-merge off, this pull request never reached a gate, or its last outcome was
    /// [`AutoMergeOutcome::Merged`]/[`AutoMergeOutcome::Updated`] and [`forget`](Self::forget)
    /// cleared it.
    ///
    /// The one reviewed exception to "not surfaced outside the log" (module doc, STUDIO-923): a
    /// READ, never a write, and it answers with what this half has already SAID rather than
    /// deciding anything fresh — the caller gets no more freshness guarantee than the log line
    /// itself had.
    pub fn peek(&self, pr: &PrCoord) -> Option<&'static str> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(pr)
            .map(|(_, why)| *why)
    }

    /// Test-only seam for [`crate::reviewreconcile`]'s STUDIO-923 tests: writes an entry directly,
    /// bypassing [`refuse`](Self::refuse)'s Declined/Held bookkeeping, which this half's own tests
    /// already cover above. `#[cfg(test)]`, so a non-test build has exactly one writer, unqualified.
    #[cfg(test)]
    pub(crate) fn test_seed(&self, pr: &PrCoord, head: &str, why: &'static str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pr.clone(), (head.to_string(), why));
    }
}

/// The check that blocks this merge, or `None` when every check NAME resolved green at this head.
///
/// Judged per NAME rather than per ENTRY, because one head carries SEVERAL entries for the same
/// name and the non-green ones among them are routinely superseded rather than owed. This
/// repository produces exactly that: `.github/workflows/pr-title.yml` runs on `pull_request`
/// **edited** under `concurrency: cancel-in-progress`, so editing a pull request's title or body
/// while its first run is in flight cancels that run — and the CANCELLED run stays attached to the
/// head forever, beside the SUCCESS that replaced it. Per-entry judgement reads that head as "a
/// check was cancelled" and refuses a pull request GitHub reports `CLEAN`, forever, on every tick.
/// Two of this repository's four open pull requests carried such an entry when this was written.
///
/// The rule is narrow in BOTH of its halves, and each half is a separate fail-open door held shut.
///
/// The rescuing entry must be a [`CHECK_SUCCESS`] of the SAME name at the same head. So a check
/// that was cancelled and never re-ran — no green sibling — still blocks, which is why `CANCELLED`
/// is not simply added to [`NON_BLOCKING_CHECKS`].
///
/// The rescued entry must be [`CHECK_CANCELLED`], the one conclusion that means this run produced
/// NO verdict on this head. That restriction is the whole safety argument, because the rollup is
/// keyed to ONE commit: every entry in it ran against the head about to be merged, so a SUCCESS
/// sibling is evidence that the name passed on exactly this code — but a `FAILURE` sibling is
/// exactly as much evidence that the same name FAILED on exactly this code, and [`CheckRun`]
/// carries no timestamp with which to tell which of the two ran later. Preferring the green one on
/// that tie is a choice, and it is the fail-open choice; everywhere else this module meets the same
/// uncertainty — an empty rollup, an unreadable lookup, a `mergeStateStatus` it does not know — it
/// refuses. So `FAILURE`, `TIMED_OUT` and `ACTION_REQUIRED` are never superseded, and neither is a
/// still-running `IN_PROGRESS`/`QUEUED`/`PENDING` entry, whose verdict simply has not arrived yet.
///
/// Narrowing to `CANCELLED` costs nothing real, because a re-run REPLACES a check run rather than
/// appending one: a job re-run to green leaves one entry for its name, not a stale red beside it.
/// `cancel-in-progress` is the only thing in this repository that strands an entry at all.
///
/// This matters most for the checks branch protection does NOT require — `pr-title` and `boot-e2e`
/// are not required contexts here, so `mergeStateStatus` stays CLEAN while either is red and this
/// read is the only gate on them. The head itself is pinned twice over, by the equality above and
/// by `--match-head-commit` on the merge.
///
/// Quadratic in the rollup's length, which is a handful of entries per head: a map keyed by name
/// would cost an allocation to save nothing measurable, and this way the entry REPORTED is the
/// offending one rather than a name reconstructed from a key.
fn blocking_check(checks: &[CheckRun]) -> Option<&CheckRun> {
    checks.iter().find(|c| {
        !NON_BLOCKING_CHECKS.contains(&c.state.as_str())
            && !(c.state == CHECK_CANCELLED
                && checks
                    .iter()
                    .any(|sibling| sibling.name == c.name && sibling.state == CHECK_SUCCESS))
    })
}

/// The `BEHIND` branch: update it if the repository permits, and never merge it.
async fn update_behind_branch(plan: &AutoMergePlan, deps: &AutoMergeDeps) -> AutoMergeOutcome {
    let (owner, repo, number) = (&plan.pr.owner, &plan.pr.repo, plan.pr.number);
    match deps.policy.allows_branch_update(owner, repo).await {
        Ok(true) => {}
        Ok(false) => {
            return refuse(
                plan,
                deps,
                "the branch is behind its base and the repository will not update it",
            );
        }
        // An unreadable policy is not permission. `allow_update_branch` is only in the repository
        // payload for a token with admin permission, so this read fails on perfectly healthy
        // repositories — and the refusal is true regardless of how it went: the branch IS behind.
        Err(e) => {
            return refuse_loudly(
                plan,
                deps,
                "the branch is behind its base and the repository will not update it",
                || {
                    tracing::warn!(
                        pr = %plan.pr, err = %e,
                        "auto-merge: the branch is behind its base and the repository's \
                         branch-update policy could not be read; declining"
                    )
                },
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
        BranchUpdateResult, MergeResult, MergeStateResult, PrChecksResult, PrSnapshot,
        PrStateResult,
    };
    use crate::prstate::PrCoord;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const HEAD: &str = "c366a61c366a61c366a61c366a61c366a61c366a";
    const OTHER: &str = "a324d2da324d2da324d2da324d2da324d2da324d";

    fn plan() -> AutoMergePlan {
        AutoMergePlan {
            pr: PrCoord::new("makewhatis", "tally", 151),
            head: HEAD.to_string(),
            approved_by: vec!["alice".to_string()],
        }
    }

    /// The answer is behind a `Mutex` so a test can run several TICKS against one set of deps and
    /// change what GitHub says between them — which is the only way to assert that un-drafting a
    /// pull request is noticed, and that a refusal is announced once rather than once a tick.
    struct FakePrs(Mutex<Option<PrLookup>>);
    #[async_trait]
    impl PrStateSource for FakePrs {
        async fn pr_state(&self, _: &str, _: &str, _: i64, _: &HeadAllowlist) -> PrStateResult {
            match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(l) => Ok(l.clone()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    impl FakePrs {
        fn set(&self, lookup: PrLookup) {
            *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(lookup);
        }
        /// The source stops answering — a blip, not a verdict.
        fn unset(&self) {
            *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    fn snapshot(head: &str, status: PrStatus, is_draft: bool) -> PrLookup {
        PrLookup::Found(PrSnapshot {
            head_sha: head.to_string(),
            status,
            is_draft: Some(is_draft),
            merged_at: None,
            head_repo: "makewhatis/tally".to_string(),
            merge_state: String::new(),
        })
    }

    fn found(head: &str, status: PrStatus) -> Arc<FakePrs> {
        Arc::new(FakePrs(Mutex::new(Some(snapshot(head, status, false)))))
    }

    /// The shape STUDIO-881 was filed for: approved, green — and still a draft.
    fn found_draft(head: &str) -> Arc<FakePrs> {
        Arc::new(FakePrs(Mutex::new(Some(snapshot(
            head,
            PrStatus::Open,
            true,
        )))))
    }

    /// Each GitHub seam counts its calls as well as answering, because the STUDIO-881 gates are
    /// about what is NOT asked: a refusal that still spends four `gh` calls a tick is the bug.
    struct FakeState {
        answer: Option<&'static str>,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl MergeStateSource for FakeState {
        async fn merge_state(&self, _: &str, _: &str, _: i64) -> MergeStateResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Some(s) => Ok(s.to_string()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    fn merge_state(answer: Option<&'static str>) -> Arc<FakeState> {
        Arc::new(FakeState {
            answer,
            calls: AtomicUsize::new(0),
        })
    }

    struct FakeChecks {
        answer: Option<Vec<CheckRun>>,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl PrChecksSource for FakeChecks {
        async fn pr_checks(&self, _: &str, _: &str, _: i64) -> PrChecksResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.answer {
                Some(c) => Ok(c.clone()),
                None => Err("gh pr view: HTTP 502".into()),
            }
        }
    }

    fn checks(states: &[(&str, &str)]) -> Arc<FakeChecks> {
        Arc::new(FakeChecks {
            answer: Some(
                states
                    .iter()
                    .map(|(n, s)| CheckRun {
                        name: (*n).to_string(),
                        state: (*s).to_string(),
                    })
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
        })
    }

    fn no_checks() -> Arc<FakeChecks> {
        Arc::new(FakeChecks {
            answer: None,
            calls: AtomicUsize::new(0),
        })
    }

    /// The repository's six checks, all green — what a mergeable pull request looks like here:
    /// the five `ci.yml` jobs plus `pr-title`. A real head carries more ENTRIES than names when a
    /// run was superseded; `a_superseded_check_run_does_not_block_its_green_sibling` has that shape.
    fn all_green() -> Arc<FakeChecks> {
        checks(&[
            ("lint", "SUCCESS"),
            ("test", "SUCCESS"),
            ("web", "SUCCESS"),
            ("boot-e2e", "SUCCESS"),
            ("desktop", "SUCCESS"),
            ("pr-title", "SUCCESS"),
        ])
    }

    /// One recorded `merge_pr`: number, method, whether GitHub's auto-merge was armed, and the
    /// head the merge was pinned to.
    type MergeCall = (i64, MergeMethod, bool, Option<String>);

    #[derive(Default)]
    struct FakeMerger {
        calls: Mutex<Vec<MergeCall>>,
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
            mergestate: merge_state(Some(state)),
            policy: Arc::new(FakePolicy(Some(true))),
            updater: Arc::new(FakeUpdater::default()),
            checks,
            merger,
            allow: HeadAllowlist::none(),
            ledger: Arc::new(AutoMergeLedger::default()),
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

    /// Only `CLEAN` proceeds. A conflict, a blocked pull request and a state this daemon has never
    /// heard of are all refused by the same allowlist — which is the point: the value GitHub adds
    /// next year is refused too.
    ///
    /// `DRAFT` is in this set because GitHub spells it, NOT because it is how a draft is caught:
    /// STUDIO-881 measured `CLEAN` on two live drafts, which is why `isDraft` has a gate of its own
    /// (`a_draft_pull_request_is_never_attempted_on_any_tick`).
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
                mergestate: merge_state(Some(state)),
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

    /// The rollup this repository actually produces. `pr-title` runs on `pull_request` **edited**
    /// under `concurrency: cancel-in-progress`, so editing a title or body while the first run is
    /// in flight cancels it — and the cancelled run stays attached to the head forever, beside the
    /// SUCCESS that superseded it. Judged per ENTRY that head can never merge; judged per NAME it
    /// merges, which is the same answer GitHub gives by reporting it `CLEAN`.
    ///
    /// The fixture is a real head: pull request #156 at `e72a3db`, read from
    /// `repos/makewhatis/rhapsody/commits/<head>/check-runs`. Both orders, because the rollup's
    /// order is GitHub's and a rule that depended on it would pass here and fail in production.
    #[tokio::test]
    async fn a_superseded_check_run_does_not_block_its_green_sibling() {
        let rollups: [&[(&str, &str)]; 2] = [
            &[
                ("pr-title", "SUCCESS"),
                ("boot-e2e", "SUCCESS"),
                ("web", "SUCCESS"),
                ("lint", "SUCCESS"),
                ("desktop", "SUCCESS"),
                ("test", "SUCCESS"),
                ("pr-title", "CANCELLED"),
            ],
            &[
                ("pr-title", "CANCELLED"),
                ("pr-title", "CANCELLED"),
                ("pr-title", "SUCCESS"),
                ("lint", "SUCCESS"),
                ("test", "SUCCESS"),
                ("web", "SUCCESS"),
                ("boot-e2e", "SUCCESS"),
                ("desktop", "SUCCESS"),
            ],
        ];
        for rollup in rollups {
            let merger = Arc::new(FakeMerger::default());
            let d = deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                checks(rollup),
                Arc::clone(&merger),
            );

            assert!(
                matches!(
                    perform_auto_merge(&plan(), &d).await,
                    AutoMergeOutcome::Merged(_)
                ),
                "{rollup:?}"
            );
            assert_eq!(
                merger.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
                1,
                "{rollup:?}"
            );
        }
    }

    /// The other half of the per-name rule, and the reason it is not just `CANCELLED` added to
    /// [`NON_BLOCKING_CHECKS`]: a name whose entries never reached SUCCESS was cancelled and never
    /// re-ran, so it still blocks. Only a SUCCESS sibling clears one — a SKIPPED one does not,
    /// because a check that was skipped never judged this head either.
    #[tokio::test]
    async fn a_cancelled_check_with_no_green_sibling_still_declines() {
        let rollups: [&[(&str, &str)]; 3] = [
            // Two cancelled runs of the same name and no green one: the whole name is cancelled.
            &[
                ("lint", "SUCCESS"),
                ("test", "SUCCESS"),
                ("desktop", "CANCELLED"),
                ("desktop", "CANCELLED"),
            ],
            // A SKIPPED sibling is not a green one.
            &[
                ("lint", "SUCCESS"),
                ("desktop", "SKIPPED"),
                ("desktop", "CANCELLED"),
            ],
            // A SUCCESS under a DIFFERENT name clears nothing.
            &[
                ("lint", "SUCCESS"),
                ("test", "SUCCESS"),
                ("desktop", "CANCELLED"),
            ],
        ];
        for rollup in rollups {
            let merger = Arc::new(FakeMerger::default());
            let d = deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                checks(rollup),
                Arc::clone(&merger),
            );

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined("a check is failing or has not finished"),
                "{rollup:?}"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "{rollup:?}"
            );
        }
    }

    /// The limit of the rescue, and the direction that is fail-OPEN if it is drawn too wide: a
    /// green sibling clears a CANCELLED entry only. An entry that reached a VERDICT of its own —
    /// `FAILURE`, `TIMED_OUT`, `ACTION_REQUIRED` — or that is still deciding — `IN_PROGRESS`,
    /// `QUEUED` — is not superseded by anything, because the rollup carries no timestamp and a
    /// green sibling is exactly as much evidence that the name passed as the failing entry is that
    /// it failed. Reachable here rather than theoretical: `pr-title` runs on `edited` on purpose,
    /// so retitling an at-head pull request to something release-please cannot parse leaves
    /// `pr-title=SUCCESS` (the old title) beside `pr-title=FAILURE` (the new one) — and `pr-title`
    /// is not a required context, so `mergeStateStatus` stays CLEAN and this read is the only gate
    /// on it. Merging there writes an unparseable subject onto `main`, which is STUDIO-408.
    #[tokio::test]
    async fn a_green_sibling_does_not_clear_an_entry_that_judged_this_head() {
        let states = ["FAILURE", "IN_PROGRESS", "TIMED_OUT", "ACTION_REQUIRED"];
        for state in states {
            let rollup: &[(&str, &str)] = &[
                ("lint", "SUCCESS"),
                ("test", "SUCCESS"),
                ("web", "SUCCESS"),
                ("boot-e2e", "SUCCESS"),
                ("desktop", "SUCCESS"),
                ("pr-title", "SUCCESS"),
                ("pr-title", state),
            ];
            let merger = Arc::new(FakeMerger::default());
            let d = deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                checks(rollup),
                Arc::clone(&merger),
            );

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined("a check is failing or has not finished"),
                "{state}"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "{state}"
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

    /// A pull request that has vanished, one whose head repository is not trusted, and one whose
    /// `isDraft` GitHub never stated are all refused before any merge is attempted.
    ///
    /// ⚠️ The third case is the STUDIO-881 direction pinned at the CALL SITE (STUDIO-962). Every
    /// other draft test here builds `is_draft: Some(..)` through `snapshot`, so the gate's choice of
    /// reader was invisible: swapping `snap.draft_blocks_merge()` for `snap.draft_observed()` —
    /// which answers `false` on an unstated `isDraft` — let auto-merge attempt a pull request GitHub
    /// never said was ready, and left the whole suite green. Before `is_draft` became an
    /// `Option<bool>` the composition was one bool with one default and the gate could not pick the
    /// wrong direction; now it can, so this case says which direction it must pick.
    #[tokio::test]
    async fn a_gone_untrusted_or_unstated_draft_pull_request_is_declined() {
        for (lookup, want) in [
            (PrLookup::Gone, "the pull request is gone"),
            (PrLookup::Untrusted, "the head repository is not trusted"),
            (
                // Open, at the planned head, trusted, `CLEAN` and every check green — so the draft
                // gate is the only thing between this and an irreversible `gh pr merge`.
                PrLookup::Found(PrSnapshot {
                    head_sha: HEAD.to_string(),
                    status: PrStatus::Open,
                    is_draft: None,
                    merged_at: None,
                    head_repo: "makewhatis/tally".to_string(),
                    // Not read on this path: `perform_auto_merge` asks its own
                    // `MergeStateSource` for mergeability (STUDIO-961).
                    merge_state: String::new(),
                }),
                DECLINE_DRAFT,
            ),
        ] {
            let merger = Arc::new(FakeMerger::default());
            let d = AutoMergeDeps {
                prs: Arc::new(FakePrs(Mutex::new(Some(lookup.clone())))),
                ..deps(
                    found(HEAD, PrStatus::Open),
                    MERGE_STATE_CLEAN,
                    all_green(),
                    Arc::clone(&merger),
                )
            };

            assert_eq!(
                perform_auto_merge(&plan(), &d).await,
                AutoMergeOutcome::Declined(want),
                "({lookup:?})"
            );
            assert!(
                merger
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "({lookup:?}) nothing may be merged on an answer GitHub did not give"
            );
        }
    }

    /// Builds the deps for one case of the test below, given the merger it must record into.
    type DepsBuilder = Box<dyn Fn(Arc<FakeMerger>) -> AutoMergeDeps>;

    /// A lookup that could not be MADE is a failure and never a refusal: nothing is known, so
    /// nothing is concluded and nothing is merged. Each of the three reads fails this way.
    #[tokio::test]
    async fn an_unreadable_lookup_fails_rather_than_merging() {
        let cases: Vec<(&str, DepsBuilder)> = vec![
            (
                "pr state",
                Box::new(|m| AutoMergeDeps {
                    prs: Arc::new(FakePrs(Mutex::new(None))),
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
                    mergestate: merge_state(None),
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
                        no_checks(),
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

    // ── STUDIO-881: a draft is a definite no, and a definite no is not an unreadable gate ───────

    /// ⚠️ The ticket's headline acceptance criterion, asserted across FIVE ticks rather than one,
    /// because the defect was never about a single wrong outcome: the daemon reached the same
    /// refusal 182 times in three hours, one `gh pr merge` plus three reads each time.
    ///
    /// A draft with approvals at the head and every check green — and `mergeStateStatus` reporting
    /// `CLEAN`, which is what both live pull requests reported and why the allowlist above did not
    /// catch them. Nothing is asked of GitHub past the snapshot, nothing is attempted, and the
    /// refusal names `draft`.
    #[tokio::test]
    async fn a_draft_pull_request_is_never_attempted_on_any_tick() {
        let merger = Arc::new(FakeMerger::default());
        let state = merge_state(Some(MERGE_STATE_CLEAN));
        let checks = all_green();
        let d = AutoMergeDeps {
            prs: found_draft(HEAD),
            mergestate: Arc::clone(&state) as Arc<dyn MergeStateSource>,
            checks: Arc::clone(&checks) as Arc<dyn PrChecksSource>,
            ..deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                all_green(),
                Arc::clone(&merger),
            )
        };

        let got: Vec<AutoMergeOutcome> = {
            let mut out = Vec::new();
            for _ in 0..5 {
                out.push(perform_auto_merge(&plan(), &d).await);
            }
            out
        };

        assert_eq!(
            got,
            vec![
                AutoMergeOutcome::Declined(DECLINE_DRAFT),
                AutoMergeOutcome::Held(DECLINE_DRAFT),
                AutoMergeOutcome::Held(DECLINE_DRAFT),
                AutoMergeOutcome::Held(DECLINE_DRAFT),
                AutoMergeOutcome::Held(DECLINE_DRAFT),
            ],
            "the refusal names draft, and is announced once rather than once a tick"
        );
        assert!(
            merger
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "no merge may be attempted at all"
        );
        assert_eq!(
            (
                state.calls.load(Ordering::SeqCst),
                checks.calls.load(Ordering::SeqCst)
            ),
            (0, 0),
            "and the reads that used to precede each attempt are not made either"
        );
    }

    /// The other half of the same gate, and the reason it is re-read every tick instead of
    /// remembered: marking a draft ready for review does NOT move the head, so a gate that latched
    /// would strand the pull request forever. The very next tick after the author un-drafts it
    /// merges, with no push and nothing else changed.
    #[tokio::test]
    async fn un_drafting_a_pull_request_merges_it_on_the_next_tick() {
        let merger = Arc::new(FakeMerger::default());
        let prs = found_draft(HEAD);
        let d = AutoMergeDeps {
            prs: Arc::clone(&prs) as Arc<dyn PrStateSource>,
            ..deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                all_green(),
                Arc::clone(&merger),
            )
        };

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        prs.set(snapshot(HEAD, PrStatus::Open, false));

        assert!(matches!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Merged(_)
        ));
        assert_eq!(
            merger.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );
    }

    /// ⚠️ The classifier, in the direction that was the bug: GitHub ANSWERED, and the answer was
    /// no. The error text is the one the motivating daemon log carries verbatim.
    ///
    /// Reachable even with the snapshot gate above, because that gate reads an answer from one
    /// call earlier — so this is the second, independent place `draft` is refused rather than
    /// retried. A `Failed` here would be a claim that nothing is known and the next tick may learn
    /// more, which is exactly the claim that repeated 182 times.
    ///
    /// Mutation check: drop the draft entry from `TERMINAL_MERGE_ERRORS` and this test reds.
    #[tokio::test]
    async fn a_merge_github_refuses_for_good_is_a_refusal_not_an_unreadable_gate() {
        let merger = Arc::new(FakeMerger {
            calls: Mutex::new(Vec::new()),
            fail: Some(
                "gh pr merge 247 --repo makewhatis/tally --squash --match-head-commit da6cfbfa \
                 exited with exit status: 1: GraphQL: Pull Request is still a draft \
                 (mergePullRequest)",
            ),
        });
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::clone(&merger),
        );

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Held(DECLINE_DRAFT),
            "and the second identical refusal says nothing new"
        );
    }

    /// ⚠️ The classifier in the OTHER direction, and the fail-open the ticket warns about: a
    /// transient error must still be retried, every tick, or one network blip abandons a mergeable
    /// pull request permanently. This text is also verbatim from the motivating log — the same
    /// pull request, the same hour, as the draft refusal above.
    ///
    /// Mutation check: add `503` to `TERMINAL_MERGE_ERRORS` and this test reds.
    #[tokio::test]
    async fn a_transient_merge_failure_is_retried_on_every_tick() {
        let merger = Arc::new(FakeMerger {
            calls: Mutex::new(Vec::new()),
            fail: Some(
                "gh pr merge 238 --repo makewhatis/tally --squash --match-head-commit f30a7498 \
                 exited with exit status: 1: HTTP 503: 503 Service Unavailable \
                 (https://api.github.com/graphql)",
            ),
        });
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::clone(&merger),
        );

        for tick in 0..5 {
            assert!(
                matches!(
                    perform_auto_merge(&plan(), &d).await,
                    AutoMergeOutcome::Failed(e) if e.contains("503")
                ),
                "(tick {tick})"
            );
        }
        assert_eq!(
            merger.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            5,
            "a gate that could not be READ is asked again, every tick"
        );
    }

    /// The ledger's bound, watched rather than merely written down: a daemon that runs for months
    /// must not accumulate an entry per pull request it has ever refused. Emptying costs one
    /// repeated log line per pull request on the tick it happens, which is the whole price.
    #[test]
    fn the_ledger_is_bounded_and_an_emptied_entry_is_merely_re_announced() {
        let ledger = AutoMergeLedger::default();
        let pr = |n: i64| PrCoord::new("makewhatis", "tally", n);

        // The first pull request is refused, and stays held while the map has room.
        assert_eq!(
            ledger.refuse(&pr(1), HEAD, DECLINE_DRAFT),
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        assert_eq!(
            ledger.refuse(&pr(1), HEAD, DECLINE_DRAFT),
            AutoMergeOutcome::Held(DECLINE_DRAFT)
        );

        // Fill it to the bound with distinct pull requests. The last one to arrive empties it.
        for n in 2..=(LEDGER_CAPACITY as i64 + 1) {
            ledger.refuse(&pr(n), HEAD, DECLINE_DRAFT);
        }
        assert!(
            ledger.0.lock().unwrap_or_else(|e| e.into_inner()).len() <= LEDGER_CAPACITY,
            "the ledger must not grow without bound"
        );

        // Whose only consequence is that an emptied pull request's refusal is news once more.
        assert_eq!(
            ledger.refuse(&pr(1), HEAD, DECLINE_DRAFT),
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        assert_eq!(
            ledger.refuse(&pr(1), HEAD, DECLINE_DRAFT),
            AutoMergeOutcome::Held(DECLINE_DRAFT)
        );
    }

    /// `peek` is the STUDIO-923 read seam: it answers with what the ledger holds, does not decide
    /// anything, and reflects `forget` immediately — the same three properties the reconciliation
    /// sweep's enrichment depends on.
    #[test]
    fn peek_reads_the_ledger_without_deciding_anything() {
        let ledger = AutoMergeLedger::default();
        let pr = PrCoord::new("makewhatis", "tally", 1);

        assert_eq!(ledger.peek(&pr), None, "nothing refused yet");

        ledger.refuse(&pr, HEAD, DECLINE_DRAFT);
        assert_eq!(ledger.peek(&pr), Some(DECLINE_DRAFT));
        // A second peek changes nothing: `refuse`'s own Declined→Held transition is untouched.
        assert_eq!(ledger.peek(&pr), Some(DECLINE_DRAFT));
        assert_eq!(
            ledger.refuse(&pr, HEAD, DECLINE_DRAFT),
            AutoMergeOutcome::Held(DECLINE_DRAFT),
            "peek must not itself count as the caller having reported this refusal before"
        );

        ledger.forget(&pr);
        assert_eq!(
            ledger.peek(&pr),
            None,
            "a forgotten pull request is news again"
        );
    }

    /// `classify_merge_error` lower-cases the error before looking, so a marker that is not itself
    /// lower-case can never match — it would be a terminal error silently classified as transient,
    /// which is the loop this ticket is about, reintroduced by a capital letter. Nothing about the
    /// array's type says so, so this does.
    #[test]
    fn every_terminal_merge_marker_is_lower_case() {
        for (marker, why) in TERMINAL_MERGE_ERRORS {
            assert_eq!(
                marker,
                marker.to_ascii_lowercase(),
                "the marker is compared against a lower-cased error ({why})"
            );
        }
    }

    /// ⚠️ The ledger de-duplicates the caller's line; it does not reach the `tracing` calls this
    /// module makes on the way to [`refuse`]. Four gates name the detail their fixed refusal
    /// phrase cannot carry — the mergeable state, the red check, `gh`'s own words, the unreadable
    /// branch-update policy — and all four used to speak before the ledger had ruled, which is one
    /// INFO line a minute for as long as the gate holds. The most reachable of them needs nothing
    /// exotic: a red NON-REQUIRED check leaves `mergeStateStatus` at `CLEAN`, so `pr-title` going
    /// red on an approved pull request is exactly this shape.
    ///
    /// So the detail is emitted THROUGH the ledger's verdict, and a held refusal says nothing at
    /// all.
    #[test]
    fn a_refusals_detail_line_is_spoken_only_when_the_refusal_is() {
        let d = deps(
            found(HEAD, PrStatus::Open),
            MERGE_STATE_CLEAN,
            all_green(),
            Arc::new(FakeMerger::default()),
        );
        let said = std::cell::Cell::new(0usize);
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(refuse_loudly(&plan(), &d, "a check is failing", || {
                said.set(said.get() + 1)
            }));
        }

        assert_eq!(
            got,
            vec![
                AutoMergeOutcome::Declined("a check is failing"),
                AutoMergeOutcome::Held("a check is failing"),
                AutoMergeOutcome::Held("a check is failing"),
                AutoMergeOutcome::Held("a check is failing"),
            ]
        );
        assert_eq!(said.get(), 1, "four ticks, one line");
    }

    /// A transient failure learned NOTHING, so it is not a change of subject: the ledger keeps
    /// what it has said across one. Otherwise a single flaky `gh pr view` between two draft ticks
    /// re-announces the same refusal, and a source flapping every other tick restores half the
    /// noise this ticket removes.
    #[tokio::test]
    async fn a_transient_failure_between_two_refusals_does_not_re_announce_it() {
        let prs = found_draft(HEAD);
        let d = AutoMergeDeps {
            prs: Arc::clone(&prs) as Arc<dyn PrStateSource>,
            ..deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                all_green(),
                Arc::new(FakeMerger::default()),
            )
        };

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        prs.unset(); // the source blips
        assert!(matches!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Failed(_)
        ));
        prs.set(snapshot(HEAD, PrStatus::Open, true));

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Held(DECLINE_DRAFT),
            "the same refusal, already announced"
        );
    }

    /// The ledger's whole contract, which is about what is SAID and never about what is decided:
    /// the same refusal at the same head is announced once, a DIFFERENT refusal is news, a
    /// different HEAD is news, and anything that is not a refusal forgets the pull request so its
    /// next refusal is news again.
    #[tokio::test]
    async fn a_refusal_is_announced_once_per_head_and_reason() {
        let merger = Arc::new(FakeMerger::default());
        let prs = found_draft(HEAD);
        let d = AutoMergeDeps {
            prs: Arc::clone(&prs) as Arc<dyn PrStateSource>,
            ..deps(
                found(HEAD, PrStatus::Open),
                MERGE_STATE_CLEAN,
                all_green(),
                Arc::clone(&merger),
            )
        };

        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Held(DECLINE_DRAFT)
        );

        // A different reason at the same head is news.
        prs.set(snapshot(HEAD, PrStatus::Closed, true));
        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined("the pull request is no longer open")
        );

        // The same reason at a DIFFERENT head is news too — a re-drafted pull request that has
        // since been pushed to is a fresh situation, not the one already reported.
        prs.set(snapshot(HEAD, PrStatus::Open, true));
        assert_eq!(
            perform_auto_merge(&plan(), &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
        let moved = AutoMergePlan {
            head: OTHER.to_string(),
            ..plan()
        };
        prs.set(snapshot(OTHER, PrStatus::Open, true));
        assert_eq!(
            perform_auto_merge(&moved, &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );

        // A merge is not a refusal, so the pull request is forgotten and the next one is news.
        prs.set(snapshot(OTHER, PrStatus::Open, false));
        assert!(matches!(
            perform_auto_merge(&moved, &d).await,
            AutoMergeOutcome::Merged(_)
        ));
        prs.set(snapshot(OTHER, PrStatus::Open, true));
        assert_eq!(
            perform_auto_merge(&moved, &d).await,
            AutoMergeOutcome::Declined(DECLINE_DRAFT)
        );
    }
}
