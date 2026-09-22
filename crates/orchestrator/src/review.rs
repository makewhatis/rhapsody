//! Review-mode dispatch: the synthetic `Issue`, the overwrite guard, and the worktree teardown for
//! a ticketless PR-review run (STUDIO-715, slice 3 of the design record
//! `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §14.4).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature at all, so nothing in
//! this module is a port; it is the additive Rhapsody surface the design record specifies, and it is
//! gated on `teams.enabled` end to end (§16) — with Teams off, [`Orchestrator::dispatch_review`]
//! refuses before it touches the store, the running set, or a worktree.
//!
//! Nothing calls [`Orchestrator::dispatch_review`] in production yet: the trigger that introduces a
//! pull request and picks its reviewer is slice 5, and the review agent's own wind-down (ending the
//! turn loop without a Linear state) is slice 4. This slice builds and tests the MECHANICS a live
//! review will ride on, which is why every acceptance test here drives the dispatch path directly.
//!
//! The one substitution the whole subsystem rests on is the KEY. A dispatched run is identified by
//! its issue id everywhere — the running map, the claim set, the workspace directory name — so a
//! review borrows that identity space with a coordinate no tracker issue can collide with:
//!
//! ```text
//! pr:owner/repo#12@alice
//! ```
//!
//! The `@reviewer` suffix is not decoration. It is the only thing that keeps two reviewers of ONE
//! pull request in two worktrees rather than one (`sanitize_key` maps them to `pr_owner_repo_12_alice`
//! and `pr_owner_repo_12_bob`), and it is what makes the watch set's per-(PR, reviewer) rows line up
//! one-to-one with dispatched runs.

use std::collections::HashMap;

use rhapsody_core::Issue;
use rhapsody_store::{
    self as store, REVIEW_STATUS_APPROVED, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_REVIEWED,
    ReviewWatchKey, ReviewWatchRow,
};

use crate::orchestrator::{Orchestrator, RunningEntry};
use crate::retry::{DispatchRoute, EvWorkerExit};

/// The prefix every review run's issue id/identifier carries. A tracker identifier is
/// `TEAM-123`-shaped and can never begin with `pr:`, so the prefix alone distinguishes a review run
/// from a ticket run anywhere one is held by id.
pub const REVIEW_KEY_PREFIX: &str = "pr:";

/// The dispatch-time coordinates of one ticketless review run: WHICH pull request, at WHICH head,
/// for WHICH reviewer. Stamped onto the run's [`RunningEntry`](crate::orchestrator::RunningEntry)
/// and threaded to the worker, which provisions the detached worktree from it.
///
/// `head_sha` is pinned ONCE here and never re-resolved (design §14.1 F-SHA). Everything downstream
/// — the checkout, the `SYMPHONY_REVIEW_HEAD` the agent reads, the `requested_sha` in the watch set,
/// and (in slice 4) the SHA recorded as reviewed — is this same value, so a head that advances
/// mid-review cannot be recorded as having been read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewRun {
    /// GitHub repository owner.
    pub owner: String,
    /// GitHub repository name.
    pub repo: String,
    /// Pull-request number.
    pub number: i64,
    /// The reviewing teammate's Teams identity (the `rhapsody:@<name>` label's name).
    pub reviewer: String,
    /// The teammate who AUTHORED the pull request — carried onto the watch row so a later
    /// substitution can refuse to hand them their own pull request to review (STUDIO-721). Empty
    /// means unknown, which the selection path fails closed on.
    pub author: String,
    /// The reviewer's tracker team id, carried onto the synthetic issue.
    pub team_id: String,
    /// The clone URL of the pull request's repository. It comes from a TRUSTED origin — a handoff's
    /// own resolved project binding or the authenticated console (design §14.1 F-SEC, §15-a) — never
    /// from room text, and [`Orchestrator::dispatch_review`] additionally refuses any URL no
    /// configured project owns.
    pub repo_url: String,
    /// The head SHA this review is pinned to.
    pub head_sha: String,
    /// How this pull request entered the watch set, recorded rather than inferred (design §14.1
    /// F-SEC).
    pub introduced_by: String,
    /// The commit THIS reviewer last read on this pull request, as the watch row recorded it at
    /// completion (STUDIO-959). Empty for a reviewer's first round — no row, or no completion — and
    /// the flag that makes that first round full. Read from the store BEFORE this dispatch
    /// overwrites the row, so it is the prior round's, never this one's.
    pub prior_sha: String,
}

/// The two coordinates the WORKER needs to provision a review checkout: which pull request's head
/// ref to fetch, and the exact SHA to detach at. Deliberately narrower than [`ReviewRun`] — the
/// worker has no business with the reviewer's identity or where the PR came from, and `Option<_>`
/// being `None` is what makes every non-review run take the existing provisioning paths unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewCheckout {
    /// Pull-request number, used to fetch `refs/pull/<n>/head`.
    pub pr_number: i64,
    /// The head SHA pinned at dispatch — what the worktree is detached at, and what the agent reads
    /// as `SYMPHONY_REVIEW_HEAD`.
    pub head_sha: String,
    /// What the round needs to decide between a full and a delta read (STUDIO-959): the
    /// coordinates, the commit the reviewer last read, and the head. `None` whenever there is no
    /// prior commit to diff from — a first round — which is also the whole of the full-review
    /// decision the worker can make without asking GitHub anything.
    pub delta: Option<ReviewDeltaRequest>,
}

/// One delta-round's inputs: the pull request, the commit the reviewer last read, and the head
/// (STUDIO-959). Plain owned data — the worker is what turns it into `gh` reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewDeltaRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub prior_sha: String,
    pub head_sha: String,
}

impl ReviewRun {
    /// The checkout coordinates handed to the worker.
    pub(crate) fn checkout(&self) -> ReviewCheckout {
        ReviewCheckout {
            pr_number: self.number,
            head_sha: self.head_sha.clone(),
            // Only a reviewer with a recorded prior commit can be a delta round (STUDIO-959); an
            // empty `prior_sha` is a first round, and `None` is what carries that to the worker.
            delta: (!self.prior_sha.is_empty()).then(|| ReviewDeltaRequest {
                owner: self.owner.clone(),
                repo: self.repo.clone(),
                number: self.number,
                prior_sha: self.prior_sha.clone(),
                head_sha: self.head_sha.clone(),
            }),
        }
    }

    /// The run's issue id and identifier: `pr:owner/repo#number@reviewer`.
    pub(crate) fn key(&self) -> String {
        review_key(&self.owner, &self.repo, self.number, &self.reviewer)
    }

    /// The watch-set row this run is the dispatch of.
    pub(crate) fn watch_key(&self) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            number: self.number,
            reviewer: self.reviewer.clone(),
        }
    }

    /// Builds the synthetic [`Issue`] the dispatch path is typed against (design §13.3 F5).
    ///
    /// `dispatch_issue` takes an `Issue` and nothing else, so a review has to be one. Three fields
    /// are load-bearing rather than cosmetic:
    ///
    /// * `id` == `identifier` == [`Self::key`] — the id keys the running/claimed sets (so the
    ///   overwrite guard works) and the identifier names the worktree directory (so two reviewers
    ///   get two trees).
    /// * `labels` carries exactly `rhapsody:@<reviewer>`, which is what routing reads to attach the
    ///   reviewer's identity, profile and memory to the run (`teams::route`'s tier 0).
    /// * `team_id` is the reviewer's team, so the run is a first-class teammate run.
    ///
    /// `state` is deliberately left EMPTY. A `pr:` key resolves to no tracker issue, so any state
    /// here would be a fiction that the eligibility gate and the exit classifier would then read as
    /// fact; the review path routes around both instead ([`Orchestrator::dispatch_review`]'s own
    /// guard, and slice 4's wind-down).
    pub(crate) fn synthetic_issue(&self) -> Issue {
        let key = self.key();
        Issue {
            id: key.clone(),
            title: format!(
                "Review {}/{}#{} at {}",
                self.owner,
                self.repo,
                self.number,
                short_sha(&self.head_sha)
            ),
            identifier: key,
            team_id: self.team_id.clone(),
            labels: Some(vec![format!("rhapsody:@{}", self.reviewer)]),
            ..Issue::default()
        }
    }
}

/// Formats a review run's issue key: `pr:owner/repo#number@reviewer`.
pub fn review_key(owner: &str, repo: &str, number: i64, reviewer: &str) -> String {
    format!("{REVIEW_KEY_PREFIX}{owner}/{repo}#{number}@{reviewer}")
}

/// Reports whether an issue id/identifier belongs to a review run rather than a tracker ticket.
pub fn is_review_key(id: &str) -> bool {
    id.starts_with(REVIEW_KEY_PREFIX)
}

/// The first 7 characters of a SHA, for the synthetic issue's human-readable title only.
fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The synthetic issue's EXIT "state" when the reviewer found nothing worth posting — the agent
/// declared `HANDOFF: approved` (STUDIO-721; design §15-c, "approved-pauses, push-re-arms").
pub(crate) const REVIEW_STATE_APPROVED: &str = "review:approved";

/// The synthetic issue's EXIT "state" for a DECLARED rejection: the agent's `HANDOFF:` payload is
/// `findings` (the wording `reviewprompt/review-base.md` instructs) or `not approved` (an older
/// spelling this daemon still honours). Either way the reviewer said no, explicitly.
pub(crate) const REVIEW_STATE_FINDINGS: &str = "review:findings";

/// The synthetic issue's EXIT "state" when the agent emitted a `HANDOFF:` line — so
/// [`EvWorkerExit::declared_handoff`](crate::retry::EvWorkerExit) is `true` and the max_turns
/// backstop never fired — but its payload is neither `approved` nor a recognised rejection
/// (STUDIO-894). A reviewer who approves in prose without the exact line the prompt asks for lands
/// here, and so does one whose payload merely drifted from the instructed wording. Neither is
/// `findings`: recording either one `reviewed` would silently block a pull request nobody actually
/// asked for changes on, which is the defect this state exists to stop reproducing. See
/// [`Orchestrator::on_review_exit`](crate::orchestrator::Orchestrator::on_review_exit) for how it is
/// handled — the same non-terminal parking the max_turns backstop uses, logged loudly and distinctly
/// from it.
pub(crate) const REVIEW_STATE_UNDECLARED: &str = "review:undeclared";

/// Reads a review agent's VERDICT off its final result text, as the state a review run exits in.
///
/// A `pr:` key resolves to no tracker issue, so a review run has no tracker state to report and
/// [`EvWorkerExit::last_state`](crate::retry::EvWorkerExit) would otherwise carry the empty string
/// the synthetic issue was dispatched with. The review path gives that slot the one terminal fact a
/// review DOES have. It is safe to do so precisely because the slot is dead on this path otherwise:
/// its only reader is `classify_clean_exit`, which [`Orchestrator::on_review_exit`] returns before
/// ever reaching (STUDIO-716), and the review branch below is unreachable for a run with no
/// [`ReviewRun`].
///
/// Every payload compared here is an EXACT match (case- and whitespace-insensitive), never a
/// substring — widening this to `contains` is the fail-open direction the whole function exists to
/// refuse: it would read `HANDOFF: not approved` as an approval (STUDIO-874 leans on the strictness
/// to avoid prose-sniffing). A payload this function does not recognise is not guessed at either
/// way; it becomes [`REVIEW_STATE_UNDECLARED`] rather than defaulting to a rejection, which is the
/// STUDIO-894 fix — the old default silently recorded every unrecognised payload, including a
/// reviewer's own approval spelled slightly differently, as changes requested forever.
pub(crate) fn review_exit_state(result_text: &str) -> &'static str {
    let payloads: Vec<String> = result_text
        .lines()
        .filter_map(|ln| ln.trim().strip_prefix("HANDOFF:"))
        .map(|payload| payload.trim().to_ascii_lowercase())
        .collect();
    if payloads.iter().any(|p| p == "approved") {
        return REVIEW_STATE_APPROVED;
    }
    if payloads
        .iter()
        .any(|p| p == "findings" || p == "not approved")
    {
        return REVIEW_STATE_FINDINGS;
    }
    REVIEW_STATE_UNDECLARED
}

/// The watch-set status a DECLARED review completion is recorded with, from the verdict
/// [`review_exit_state`] put on the run's exit state.
///
/// Both are terminal at the reviewed head, so they pause re-review identically; they differ in what
/// the console and the room will say about the round. An unrecognised value is `reviewed`, the
/// conservative reading — "somebody looked and may have found something".
fn declared_review_status(exit_state: &str) -> &'static str {
    if exit_state == REVIEW_STATE_APPROVED {
        REVIEW_STATUS_APPROVED
    } else {
        REVIEW_STATUS_REVIEWED
    }
}

/// Canonicalizes a `rhapsody_review_watch.status` that a COMPLETED review round may be recorded
/// with, or `None` for anything outside that closed domain (STUDIO-716).
///
/// [`Store::mark_review_completed`](rhapsody_store::Store::mark_review_completed) takes a plain
/// string and cannot enforce the domain itself, so the WRITER does. Only the two CLOSED values
/// qualify: [`REVIEW_STATUS_REVIEWED`] (findings posted) and [`REVIEW_STATUS_APPROVED`] (nothing
/// found; re-review pauses at this head — design §15-c). [`REVIEW_STATUS_REQUESTED`] and
/// `in_flight` describe a round that has NOT finished and `dropped` is the watcher's own terminal,
/// so none of them is a completion — and a status the watcher cannot recognise is worse than no
/// write at all, because its edge-trigger would then either re-review forever or never again.
fn closed_review_status(status: &str) -> Option<&'static str> {
    match status {
        REVIEW_STATUS_REVIEWED => Some(REVIEW_STATUS_REVIEWED),
        REVIEW_STATUS_APPROVED => Some(REVIEW_STATUS_APPROVED),
        _ => None,
    }
}

/// Why a review dispatch did or did not happen. Returned rather than logged-and-swallowed because
/// slice 5's watcher has to distinguish "already in flight, come back next tick" from "this will
/// never work", and because the F-DUP refusal is the property this slice's acceptance test asserts.
// Not `Copy` since STUDIO-908: `Refused` carries a formatted message (String).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDispatchOutcome {
    /// A worker was dispatched for this (PR, reviewer).
    Dispatched,
    /// Teams is off, so the whole subsystem is dormant (design §16).
    TeamsOff,
    /// A run for this exact (PR, reviewer) is already running or claimed — THE overwrite guard
    /// (design §14.1 F-DUP). Nothing was touched.
    AlreadyInFlight,
    /// A drain is armed, so the daemon is taking no new work of any kind (STUDIO-880). Nothing was
    /// touched; the row stays where it is and the sweep re-offers it once the drain is cancelled.
    Draining,
    /// The coordinates cannot produce a review run; the payload names which. A `String` rather
    /// than `&'static str` because the `review.model` refusal (STUDIO-908) names the reviewer's
    /// harness, the configured model and the `review.model` origin — all data, not literals.
    Refused(String),
    /// The reviewer's provider is out of daily budget (STUDIO-957). Nothing was touched: the watch
    /// row stays exactly where it was and the sweep re-offers this head once the budget resets,
    /// exactly as [`ReviewDispatchOutcome::Draining`] defers. Distinct from `Refused` because it is
    /// a deliberate, temporary hold an operator can act on, not a coordinate that can never work.
    BudgetHeld,
}

impl Orchestrator {
    /// Dispatches one ticketless review run, or refuses and explains why. The single entry point
    /// into review mode; slice 5's watcher drives it.
    ///
    /// Refusal is ordered so that nothing observable happens before every check has passed: the
    /// Teams gate first (§16 — a Teams-off daemon must be side-effect-free), then the coordinates,
    /// then the routing, and only then the overwrite guard, the watch-set write and the dispatch.
    ///
    /// The overwrite guard is the reason this is not simply a `dispatch_issue` call. Every ticket
    /// dispatch reaches `dispatch_issue` through `eligibility`, which refuses an issue that is
    /// already running or claimed; a synthetic issue has no tracker state and cannot pass that gate,
    /// so it would have to bypass it — and `dispatch_issue` OVERWRITES `running[id]`. For a review
    /// key that means dropping the live entry's cancel handle (the run can then never be stopped)
    /// and pointing a second agent at the first one's detached worktree (design §14.1 F-DUP). The
    /// running/claimed half of the eligibility check is therefore reproduced here, where the review
    /// path cannot forget it.
    pub fn dispatch_review(&mut self, mut run: ReviewRun) -> ReviewDispatchOutcome {
        // §16: gated on teams.enabled, structurally, before anything is observed or written.
        if !self.teams.as_ref().is_some_and(|t| t.enabled) {
            return ReviewDispatchOutcome::TeamsOff;
        }
        // STUDIO-880: the drain gate's THIRD entry point. `on_tick` gates and `on_retry` parks, but
        // `Event::ReviewSweep` reaches dispatch through here, past both. It has no production caller
        // today (`ControlHandle::review_sweep` is unwired), so this gate is a backstop rather than a
        // live fix — but the path exists in-tree, and forgetting the second entry point is the bug
        // this feature already shipped once.
        //
        // It must refuse HERE rather than inside `dispatch_issue`: the writes below record the head
        // as requested and mark the row in-flight, so a refusal further down would leave the watcher
        // believing a review was dispatched and never re-offering this head. Refusing before them
        // leaves the row exactly where it was.
        if self.drain.is_draining() {
            return ReviewDispatchOutcome::Draining;
        }
        if run.owner.is_empty() || run.repo.is_empty() {
            return ReviewDispatchOutcome::Refused("pull request has no owner/repo".to_string());
        }
        if run.number <= 0 {
            return ReviewDispatchOutcome::Refused(
                "pull-request number is not positive".to_string(),
            );
        }
        if run.reviewer.is_empty() {
            return ReviewDispatchOutcome::Refused("no reviewer".to_string());
        }
        if run.head_sha.is_empty() {
            return ReviewDispatchOutcome::Refused("no pinned head SHA".to_string());
        }
        // A repo no configured project owns has no workspace, no agent and no prompt to run with —
        // and refusing it also keeps a review confined to repositories this daemon is configured
        // for, which is the trusted-origin property (design §14.1 F-SEC) restated at the dispatch.
        let Some(route) = self.review_route(&run.repo_url) else {
            return ReviewDispatchOutcome::Refused(
                "no configured project owns the PR's repo".to_string(),
            );
        };
        let id = run.key();
        // THE overwrite guard (F-DUP).
        if self.running.contains_key(&id) || self.claimed.contains(&id) {
            return ReviewDispatchOutcome::AlreadyInFlight;
        }

        // STUDIO-908: the operator's `review.model` is scoped by harness, so a review routed to a
        // reviewer whose harness has no entry is refused HERE — before the watch-set writes below,
        // which would otherwise record this head as requested and mark the row in-flight, leaving
        // the watcher believing a review ran when none did. The message names the reviewer's
        // harness, the configured model and the `review.model` origin; that is the whole point,
        // because the failure this replaces was a provider's generic `UnknownError` naming none of
        // them. Placed after the overwrite guard so a duplicate of an already-live review still
        // answers `AlreadyInFlight` rather than blaming a model.
        let iss = run.synthetic_issue();
        // `fallback` is the configured `agent.backend` — the harness the legacy bare-scalar
        // `review.model` spelling belongs to, so an all-opencode installation that wrote a bare
        // scalar is not refused on its own harness (alice's blocking finding on PR #172).
        let fallback = self.configured_backend();
        let refused = self
            .teams
            .as_ref()
            .filter(|t| t.review_ticketless())
            .and_then(|teams| {
                match teams.review_model_for(&self.review_harness_for(&iss), &fallback) {
                    rhapsody_config::teams::ReviewModelChoice::Refuse(why) => Some(why),
                    _ => None,
                }
            });
        if let Some(why) = refused {
            tracing::warn!(review = %id, reason = %why, "ticketless review: refused");
            return ReviewDispatchOutcome::Refused(why);
        }

        // STUDIO-957: the per-provider daily budget, the drain gate's sibling. It must refuse HERE
        // rather than inside `dispatch_issue` for the SAME reason the drain gate does: the writes
        // below record this head as requested and mark the row in-flight, so a refusal further down
        // would leave the watcher believing a review dispatched and never re-offer this head. The
        // incident's whole Claude bill was REVIEWS, so a budget that could not see this path would
        // have refused nothing.
        //
        // Keyed by the review IDENTITY (`id`, `pr:owner/repo#n@reviewer`), not by the pull request
        // coordinate (sol round 1 on PR #199). Dispatch is per `(PR, reviewer)`: in a mixed
        // roster one reviewer can be out of budget while another is not, and a coordinate key let
        // the second reviewer's successful dispatch release the first reviewer's still-active hold
        // (and let two held reviewers overwrite each other's provider/figures). The coordinate
        // rides on the hold as `pr` so the reconciliation sweep still finds every hold for a
        // divergence it reports.
        if self.budgets_configured() {
            let pr = format!("{}/{}#{}", run.owner, run.repo, run.number);
            let provider = self.review_projected_provider(&iss, &route.slug);
            if let Some((limit, spent)) = self.provider_budget_spent(&provider) {
                self.note_review_budget_hold(&id, &pr, &route.slug, &provider, limit, spent);
                return ReviewDispatchOutcome::BudgetHeld;
            }
            // A dispatched review clears this reviewer's own stale hold — and only this reviewer's.
            self.release_budget_hold(&id);
        }

        // Record the head this run was dispatched against BEFORE the dispatch. Without it the
        // watcher's re-review condition is level-triggered and stays true on every tick between
        // introduction and first completion, which is what produced the duplicate dispatch the guard
        // above refuses. The row is upserted first because `mark_review_requested` is an UPDATE:
        // dispatching a (PR, reviewer) the watch set has never seen would otherwise silently record
        // no `requested_sha` at all. `save_review_watch` preserves both SHAs on a row that already
        // exists, so re-arming an existing row cannot forget what was dispatched or reviewed.
        let watch_key = run.watch_key();
        // The reviewer's PRIOR round (STUDIO-959), read BEFORE the writes below touch the row: the
        // commit this reviewer last read is exactly what a delta round diffs from. Today neither
        // write can move `last_reviewed_sha` (`mark_review_completed` alone owns it), so this read
        // would see the same value after them; the placement is defensive, not a guard the suite
        // pins. A missing row (a first round) or a failed read both leave `prior_sha` empty, which
        // is a full review — the safe direction, because a delta from an unknown commit is no delta
        // at all.
        run.prior_sha = self
            .store()
            .get_review_watch(&watch_key)
            .ok()
            .flatten()
            .map(|row| row.last_reviewed_sha)
            .unwrap_or_default();
        if let Err(e) = self.store().save_review_watch(ReviewWatchRow {
            key: watch_key.clone(),
            author: run.author.clone(),
            introduced_by: run.introduced_by.clone(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.to_string(),
            open: true,
        }) {
            tracing::warn!(review = %id, err = %e, "review watch upsert failed; dispatching anyway");
        }
        if let Err(e) = self
            .store()
            .mark_review_requested(&watch_key, &run.head_sha)
        {
            tracing::warn!(review = %id, err = %e, "recording the requested head failed; dispatching anyway");
        }

        // Carried to the dispatch the way a graphite stacking hint is (`pending_stack`): the worker
        // spawn happens INSIDE `dispatch_issue`, so the pinned head has to be in place before the
        // call rather than stamped onto the running entry after it.
        self.pending_review.insert(id, run);
        self.dispatch_issue(iss, None, Some(route), String::new());
        ReviewDispatchOutcome::Dispatched
    }

    /// Resolves the dispatch routing for a pull request's repository: the enabled project whose
    /// `repo` IS that URL. `None` when no project owns it — which refuses the review rather than
    /// falling back to the top-level binding, whose repo would be some OTHER repository's.
    fn review_route(&self, repo_url: &str) -> Option<DispatchRoute> {
        if repo_url.is_empty() {
            return None;
        }
        let p = self
            .eff
            .as_ref()?
            .projects
            .iter()
            // Spelling-insensitive, so this router and the introduction-side allowlist agree on
            // what "the same repository" means (STUDIO-725). Today `review_repo_url` hands back the
            // project's own `repo` string verbatim, so a raw comparison would also match — this
            // keeps the pair correct if that binding source ever changes.
            .find(|p| !p.disabled && crate::reviewintro::same_repository(&p.repo, repo_url))?;
        Some(DispatchRoute {
            slug: p.slug.clone(),
            group: p.group.clone(),
            repo: p.repo.clone(),
            model: p.model.clone(),
            workspace_mode: p.workspace_mode.clone(),
        })
    }

    /// The exit path of a ticketless review run — what `classify_clean_exit` cannot be
    /// (STUDIO-716, design §14.2 F4).
    ///
    /// A synthetic `pr:` issue carries no state, so both of the classifier's samples are empty,
    /// `worker_left` and `snap_left` are both false, and EVERY clean review exit falls into its
    /// first branch: `OUTCOME_CONTINUED`, keep the claim, `schedule_retry_for`. That re-dispatches
    /// the same review a second later, and again, and again — permanently holding the reviewer's
    /// slot. So the review path does its own bookkeeping and schedules no retry at all.
    ///
    /// That holds for a FAILED exit too. A review round is one-shot: re-arming one at a new head is
    /// the watcher's edge-triggered decision (slice 5), and the retry queue could not re-dispatch a
    /// `pr:` key regardless, since [`Orchestrator::dispatch_issue`] refuses a review key that
    /// arrives without its coordinates — a backoff timer would only hold the claim until it fired.
    pub(crate) fn on_review_exit(&mut self, re: &RunningEntry, run: &ReviewRun, e: &EvWorkerExit) {
        self.completed.remove(&re.issue.id);
        self.claimed.remove(&re.issue.id);
        let (outcome, reason) = if e.failed {
            // The watch row is deliberately left exactly where the dispatch put it (`in_flight` at
            // its `requested_sha`): nobody read this head, so recording it as reviewed would be the
            // F-SHA lost update by another route, and clearing the in-flight marker of a crashed
            // review is the watcher's own recovery (design §14.1 F-DUP, "clear on crash").
            let reason = if e.err_msg.is_empty() {
                "worker failed"
            } else {
                e.err_msg.as_str()
            };
            (store::OUTCOME_FAILED, reason)
        } else if !e.declared_handoff {
            // The `max_turns` backstop fired: the agent burned its whole turn budget without ever
            // declaring it had finished, so this head was read PARTIALLY at best (STUDIO-721 — the
            // nit carried from slice 4). Recording it `reviewed` at `head_sha` is what ships a
            // partial — or entirely absent — review as a complete one, because the watcher's
            // edge-trigger would then see `last_reviewed_sha == head` and never look again. The row
            // is parked NON-terminally instead, which re-arms this same head for another round.
            // The CLASSIFICATION is the same either way — a partial read is parked non-terminally
            // and the head is re-reviewed — but the attributed CAUSE is not. A drained review wound
            // down at a turn boundary on purpose and burned no budget doing it; calling that "the
            // max_turns backstop" sends whoever reads this line looking for a review that ran away.
            let cause = if self.drain.is_draining() {
                "wound down at a turn boundary for an armed drain"
            } else {
                "ended on the max_turns backstop"
            };
            tracing::warn!(
                review = %run.key(),
                head = %run.head_sha,
                "review run {cause} without declaring it had finished; recording the round as \
                 truncated so the head is reviewed again"
            );
            self.record_review_truncated(run);
            (store::OUTCOME_COMPLETED, "")
        } else if e.last_state == REVIEW_STATE_UNDECLARED {
            // STUDIO-894: the agent DID emit a `HANDOFF:` line — the branch above did not fire —
            // but `review_exit_state` could not read its payload as `approved` or as a declared
            // rejection. Guessing either way is the defect this branch exists to avoid (a reviewer
            // who approved in prose without the exact line must not be recorded `reviewed`, which
            // would block the pull request forever since nothing then advances the head), so the
            // round is parked exactly where the max_turns backstop above parks one: non-terminally,
            // which re-offers this same head for another round. Logged at `error` rather than
            // `warn` — unlike the backstop, this is not an expected shape of a clean exit, and an
            // operator reading the log needs it to stand out from routine truncation.
            tracing::error!(
                review = %run.key(),
                head = %run.head_sha,
                "review run declared a hand-off but its payload is neither `approved` nor a \
                 recognised rejection; recording the round as truncated rather than guessing a \
                 verdict"
            );
            self.record_review_truncated(run);
            (store::OUTCOME_COMPLETED, "")
        } else {
            let status = declared_review_status(&e.last_state);
            self.record_review_completed(run, status);
            // STUDIO-1004: this verdict may be the ANSWER to an author round that has been waiting
            // for one. An author dispatch charges nothing until a reviewer's `last_reviewed_sha`
            // reaches the head that dispatch produced, and `record_review_completed` is the only
            // moment it moves — so the settle sits beside it and never on the truncated/crashed
            // branches above, which advance nothing. `run.head_sha` is the head this round READ,
            // pinned at dispatch; a verdict at a head other than the one the pending author round
            // was recorded against is exactly that answer.
            self.settle_author_round(
                &crate::prstate::PrCoord::new(&run.owner, &run.repo, run.number),
                &run.head_sha,
            );
            // The round is over and its verdict is known, which is the only moment the daemon can
            // tell the author findings are waiting (STUDIO-723). Handed to the off-loop task and
            // never waited on; the two branches above deliberately notify NOBODY — a crashed round
            // read nothing, and a truncated one is re-armed at the same head rather than reported
            // as a review the author should act on.
            self.request_review_notify(
                self.plan_review_notify(run, status == REVIEW_STATUS_APPROVED),
            );
            (store::OUTCOME_COMPLETED, "")
        };
        self.persist_end_run(re, outcome, reason);
        // Drop the persisted `running` claim row `persist_start_run` wrote — on BOTH outcomes.
        // A ticket run's failure path can leave its claim behind because the backoff retry it
        // schedules immediately rewrites the row as `retry_queued`, which is what re-arms the timer
        // across a restart; a review schedules nothing, so the row would simply outlive the daemon
        // and greet boot recovery as a live claim on a key no tracker can resolve.
        self.persist_complete(&re.issue.identifier);
        self.persist_totals();
    }

    /// Records the head a finished review round ACTUALLY read into its watch-set row, with a
    /// validated terminal `status` (STUDIO-716).
    ///
    /// The SHA is `run.head_sha` — the one pinned at DISPATCH and carried on the running entry ever
    /// since. It is deliberately not a completion-time reading of where the pull request's head is
    /// now: an author who pushes a fix mid-review would otherwise have that new head recorded as
    /// reviewed, and those commits would then never be read by anyone (design §14.1 F-SHA).
    ///
    /// An out-of-domain `status` is refused rather than written; see [`closed_review_status`].
    /// Best-effort like every other store write on this path — a failure is logged, never fatal.
    pub(crate) fn record_review_completed(&self, run: &ReviewRun, status: &str) {
        let Some(status) = closed_review_status(status) else {
            tracing::error!(
                review = %run.key(),
                status = %status,
                "refusing to record a review completion with an out-of-domain status"
            );
            return;
        };
        if let Err(e) = self
            .store()
            .mark_review_completed(&run.watch_key(), &run.head_sha, status)
        {
            tracing::warn!(review = %run.key(), err = %e, "recording the reviewed head failed");
        }
    }

    /// Parks a review round that ended WITHOUT the agent declaring it finished at a non-terminal
    /// status, leaving both SHAs alone (STUDIO-721).
    ///
    /// Separate from [`record_review_completed`](Orchestrator::record_review_completed) because it
    /// is the opposite bookkeeping: that one advances `last_reviewed_sha` to the head that WAS
    /// read, and the whole point here is that nothing can be said to have been read. Best-effort
    /// like every other store write on this path.
    pub(crate) fn record_review_truncated(&self, run: &ReviewRun) {
        if let Err(e) = self.store().mark_review_truncated(&run.watch_key()) {
            tracing::warn!(review = %run.key(), err = %e, "recording the truncated review round failed");
        }
    }

    /// Removes a finished review run's worktree. Called from `on_worker_exit` for review runs only.
    ///
    /// A ticket's worktree is reclaimed by `reconcile`'s `TerminateCleanup` when the ticket reaches a
    /// terminal tracker state. A `pr:` id resolves to no ticket and therefore reaches no state, so
    /// that path never fires for a review and the detached worktree would simply accumulate, one per
    /// (PR, reviewer, ever) — hence an explicit teardown at exit (design §14.2).
    ///
    /// Off-loop and best-effort, like every other post-exit cleanup: a removal failure is logged, and
    /// the workspace GC still sees the directory. A no-op when the daemon is not live (`ctx` unset —
    /// the direct-handler unit tests), which is also when nothing was ever spawned to clean up.
    pub(crate) fn teardown_review_worktree(&self, run: &ReviewRun, project_slug: &str) {
        let Some(eff) = self.eff.as_ref() else {
            return;
        };
        let Some(mut ctx) = self.ctx.clone() else {
            return;
        };
        let ws = match eff.project_by_slug(project_slug) {
            Some(p) => std::sync::Arc::clone(&p.workspace),
            None => std::sync::Arc::clone(&eff.workspace),
        };
        let (repo_url, slug, identifier) =
            (run.repo_url.clone(), project_slug.to_string(), run.key());
        let guard = self.wg.add();
        tokio::spawn(async move {
            let _guard = guard;
            let work = async {
                if let Err(e) = ws.remove_worktree(&repo_url, &slug, &identifier).await {
                    tracing::warn!(review = %identifier, err = %e, "review worktree teardown failed");
                }
            };
            tokio::select! {
                () = work => {}
                _ = ctx.cancelled() => {}
            }
        });
    }
}

/// The dispatch-time review coordinates awaiting their `dispatch_issue` call, keyed by issue id
/// (mirrors `pending_stack`'s hand-off from one control-task step to the next).
pub type PendingReviews = HashMap<String, ReviewRun>;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rhapsody_config::teams::{HarnessScoped, Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_APPROVED, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REVIEWED,
        REVIEW_STATUS_TRUNCATED, Sqlite, Store, StorePath,
    };
    use rhapsody_tracker::fake::Fake;
    use rhapsody_workspace::sanitize_key;

    use super::*;
    use crate::orchestrator::RunningEntry;
    use crate::testsupport::{
        DispatchedEntries, TempDir, empty_effective, empty_resolved_project, set_of,
    };

    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";

    /// A spawn seam recording each dispatched entry, so a test can see how many agents were spawned
    /// and with which review coordinates.
    fn record_entries(sink: &DispatchedEntries) -> crate::orchestrator::SpawnFn {
        let sink = Arc::clone(sink);
        Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        })
    }

    /// An orchestrator with Teams ON, one project owning [`REPO_URL`], an in-memory store, and a
    /// recording spawn seam.
    fn orch_with_review(teams_enabled: bool) -> (Orchestrator, DispatchedEntries) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: teams_enabled,
            // These are ticketless-review tests, so the harness is on the ticketless path — which
            // is also what arms the completion comment's own gate (STUDIO-723).
            review: Review {
                mode: ReviewMode::Ticketless,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: Vec::new(),
                bank: String::new(),
                max_concurrent: 0,
            }],
            ..Teams::disabled()
        });
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&dispatched));
        (o, dispatched)
    }

    fn review_run(reviewer: &str, head: &str) -> ReviewRun {
        ReviewRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: reviewer.to_string(),
            author: "alice".to_string(),
            team_id: "team-1".to_string(),
            repo_url: REPO_URL.to_string(),
            head_sha: head.to_string(),
            introduced_by: "handoff".to_string(),
            prior_sha: String::new(),
        }
    }

    /// The key shape the whole subsystem addresses a review by, and the property that makes it
    /// usable as a worktree directory name: two reviewers of ONE pull request sanitize to two
    /// DIFFERENT keys, so they cannot land in one worktree.
    #[test]
    fn two_reviewers_of_one_pr_get_distinct_collision_free_keys() {
        let alice = review_key("makewhatis", "rhapsody", 12, "alice");
        let bob = review_key("makewhatis", "rhapsody", 12, "bob");
        assert_eq!(alice, "pr:makewhatis/rhapsody#12@alice");
        assert_ne!(alice, bob);
        assert_ne!(
            sanitize_key(&alice),
            sanitize_key(&bob),
            "the @reviewer suffix must survive sanitization — it is the only thing keying them apart"
        );
        assert_eq!(sanitize_key(&alice), "pr_makewhatis_rhapsody_12_alice");
        // The sanitized key is a single safe path component, so it stays inside the workspace root.
        assert!(!sanitize_key(&alice).contains('/'));
        // A review key is distinguishable from a tracker identifier anywhere one is held by id.
        assert!(is_review_key(&alice) && !is_review_key("STUDIO-715"));
    }

    /// The synthetic issue (design §13.3 F5): the routing label that attaches the reviewer's
    /// identity, the reviewer's team, and the id/identifier that ARE the review key.
    #[test]
    fn synthetic_issue_carries_the_reviewer_identity_and_no_state() {
        let iss = review_run("alice", HEAD_A).synthetic_issue();
        assert_eq!(iss.id, "pr:makewhatis/rhapsody#12@alice");
        assert_eq!(iss.identifier, iss.id);
        assert_eq!(iss.team_id, "team-1");
        assert_eq!(
            iss.labels.as_deref(),
            Some(["rhapsody:@alice".to_string()].as_slice()),
            "routing reads this label to attach the reviewer's identity and memory"
        );
        assert!(
            iss.state.is_empty(),
            "a pr: key resolves to no ticket, so it must claim no tracker state"
        );
    }

    /// STUDIO-957: the review path is the one that mattered — the incident's whole Claude bill was
    /// REVIEWS — so a reviewer whose provider is out of daily budget must not dispatch, and the
    /// refusal must be recorded rather than silently dropped. It must also refuse BEFORE the
    /// watch-set writes, or the watcher would believe a review ran and never re-offer this head.
    ///
    /// Mutation check: drop the budget gate in [`Orchestrator::dispatch_review`] and this reds on
    /// `Dispatched` (and on the untouched-watch-row assertion).
    #[test]
    fn a_review_is_refused_when_the_reviewers_provider_is_out_of_budget() {
        use chrono::{SecondsFormat, Utc};
        use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunProvenance, RunStart};

        let (mut o, dispatched) = orch_with_review(true);
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.cfg.claude.model = "claude-opus-4-8".to_string();
            // The review runs under the owning PROJECT, whose model is what
            // `configured_model_for` reads first.
            eff.projects[0].mcfg.claude.model = "claude-opus-4-8".to_string();
            eff.cfg.budgets.insert(
                "anthropic".to_string(),
                rhapsody_config::ProviderBudget { daily_tokens: 200 },
            );
        }
        // Today's spend is already over the ceiling.
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let id = o
            .store()
            .start_run(RunStart {
                issue_identifier: "MT-seed".to_string(),
                started_at: started.clone(),
                ..Default::default()
            })
            .expect("start");
        o.store()
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.to_string(),
                    total_tokens: 300,
                    ended_at: started,
                    ..Default::default()
                },
            )
            .expect("end");
        o.store()
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: "anthropic".to_string(),
                    harness: "claude".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    ..Default::default()
                },
            )
            .expect("provenance");

        let outcome = o.dispatch_review(review_run("alice", HEAD_A));

        assert_eq!(
            outcome,
            ReviewDispatchOutcome::BudgetHeld,
            "a spent anthropic budget must refuse the review"
        );
        assert!(
            dispatched.lock().expect("dispatched lock").is_empty(),
            "no reviewer agent may be spawned"
        );
        // The hold is keyed by the review IDENTITY, and carries the pull request coordinate so the
        // reconciliation sweep can still find it.
        let ttl = o.budget_hold_ttl();
        let held = o
            .budget_ledger
            .get(&review_run("alice", HEAD_A).key(), ttl)
            .expect("the refusal is recorded under the review identity");
        assert_eq!(held.provider, "anthropic");
        assert_eq!(held.spent_tokens, 300);
        assert_eq!(held.pr, "makewhatis/rhapsody#12");
        assert_eq!(
            o.budget_ledger
                .get_for_pr("makewhatis/rhapsody#12", ttl)
                .as_ref(),
            Some(&held),
            "the sweep finds the hold by pull request coordinate"
        );
        // The watch row was NOT marked in-flight: the watcher must re-offer this head, not believe
        // a review ran.
        let watch = o
            .store()
            .get_review_watch(&rhapsody_store::ReviewWatchKey {
                owner: "makewhatis".to_string(),
                repo: "rhapsody".to_string(),
                number: 12,
                reviewer: "alice".to_string(),
            })
            .expect("read watch");
        assert!(
            watch.is_none(),
            "a budget refusal must leave the watch row exactly where it was, got: {watch:?}"
        );
    }

    /// **alice round 2, non-blocking N1.** A review is gated ONCE, at its own door — `dispatch_review`
    /// refuses before its watch-set writes and stages the review in `pending_review`. `dispatch_issue`
    /// must therefore not re-gate it with a second provider derivation: the spend map may have been
    /// re-fetched, and a refusal at that point would strand the already-consumed pending review and a
    /// watch row recorded `requested` while the caller still answered `Dispatched`. This pins the
    /// `review.is_none()` guard by staging a review and dispatching it directly with the ticket
    /// gate's own budget spent: the review still spawns, and no TICKET hold is recorded for it.
    ///
    /// Mutation: drop `review.is_none() &&` in `Orchestrator::dispatch_issue` and the spawned count
    /// reds to zero (a `BudgetHeld` return instead of a dispatch).
    #[test]
    fn a_staged_review_is_not_re_gated_by_dispatch_issue() {
        use chrono::{SecondsFormat, Utc};
        use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunProvenance, RunStart};

        let (mut o, dispatched) = orch_with_review(true);
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.cfg.claude.model = "claude-opus-4-8".to_string();
            eff.projects[0].mcfg.claude.model = "claude-opus-4-8".to_string();
            eff.cfg.budgets.insert(
                "anthropic".to_string(),
                rhapsody_config::ProviderBudget { daily_tokens: 200 },
            );
        }
        // Today's anthropic spend is already over the ceiling, so the TICKET gate would refuse.
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let id = o
            .store()
            .start_run(RunStart {
                issue_identifier: "MT-seed".to_string(),
                started_at: started.clone(),
                ..Default::default()
            })
            .expect("start");
        o.store()
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.to_string(),
                    total_tokens: 300,
                    ended_at: started,
                    ..Default::default()
                },
            )
            .expect("end");
        o.store()
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: "anthropic".to_string(),
                    harness: "claude".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    ..Default::default()
                },
            )
            .expect("provenance");

        // Stage the review exactly as `dispatch_review` does at its tail — after its own gate has
        // already passed — then dispatch it. The ticket gate must not run a second time.
        let run = review_run("alice", HEAD_A);
        let iss = run.synthetic_issue();
        let route = o
            .review_route(REPO_URL)
            .expect("the project owns the review repo");
        o.pending_review.insert(iss.id.clone(), run);
        o.dispatch_issue(iss, None, Some(route), String::new());

        assert_eq!(
            dispatched.lock().expect("dispatched lock").len(),
            1,
            "a review already gated at its own door must still spawn"
        );
        assert!(
            o.budget_ledger.held(o.budget_hold_ttl()).is_empty(),
            "the ticket gate must record no hold for a review it does not gate"
        );
    }

    /// **sol round 1 on PR #199, finding 1: the mixed-roster regression.** Dispatch is per
    /// `(PR, reviewer)`, so budget holds must be too. Alice reviews on Claude/Anthropic (out of
    /// budget) while Jerry reviews the SAME pull request on opencode/Fireworks (unspent). Alice is
    /// held; Jerry dispatches; Alice's hold must survive Jerry's success and still reach the
    /// reconciliation sweep by coordinate.
    ///
    /// Mutation: key the review hold (and its release) by the pull request coordinate and this reds
    /// — Jerry's dispatch erases Alice's hold.
    #[test]
    fn a_held_reviewers_budget_hold_survives_a_sibling_reviewers_dispatch() {
        use chrono::{SecondsFormat, Utc};
        use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunProvenance, RunStart};

        let dir = TempDir::new();
        write_profile(
            &dir,
            "claude-reviewer",
            "---\nextends: swe\nharness: claude\nmodel: claude-opus-4-8\n---\nClaude reviewer.\n",
        );
        write_profile(
            &dir,
            "opencode-reviewer",
            "---\nextends: swe\nharness: opencode\nmodel: fireworks-ai/accounts/fireworks/models/x\n---\nOpencode reviewer.\n",
        );
        let (mut o, dispatched) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "claude-reviewer".to_string();
            teams.roster.push(Identity {
                name: "jerry".to_string(),
                profile: "opencode-reviewer".to_string(),
                labels: Vec::new(),
                bank: String::new(),
                max_concurrent: 0,
            });
        }
        o.eff.as_mut().expect("eff").cfg.budgets.insert(
            "anthropic".to_string(),
            rhapsody_config::ProviderBudget { daily_tokens: 200 },
        );

        // Today's anthropic spend is already over the ceiling; Fireworks has no budget at all.
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let id = o
            .store()
            .start_run(RunStart {
                issue_identifier: "MT-seed".to_string(),
                started_at: started.clone(),
                ..Default::default()
            })
            .expect("start");
        o.store()
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.to_string(),
                    total_tokens: 300,
                    ended_at: started,
                    ..Default::default()
                },
            )
            .expect("end");
        o.store()
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: "anthropic".to_string(),
                    harness: "claude".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    ..Default::default()
                },
            )
            .expect("provenance");

        assert_eq!(
            o.review_projected_provider(&review_run("alice", HEAD_A).synthetic_issue(), "rhapsody"),
            "anthropic",
            "sanity: alice reviews on anthropic"
        );
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::BudgetHeld,
            "alice's anthropic budget is spent"
        );
        assert_eq!(
            o.dispatch_review(review_run("jerry", HEAD_B)),
            ReviewDispatchOutcome::Dispatched,
            "jerry is on fireworks, which has no budget"
        );

        let held = o
            .budget_ledger
            .get_for_pr("makewhatis/rhapsody#12", o.budget_hold_ttl())
            .expect("alice's hold must survive jerry's dispatch");
        assert_eq!(held.provider, "anthropic");
        assert_eq!(
            held.subject,
            review_key("makewhatis", "rhapsody", 12, "alice"),
            "the hold belongs to alice's review identity, not the shared coordinate"
        );
        assert_eq!(
            dispatched.lock().expect("dispatched lock").len(),
            1,
            "only jerry's agent may be spawned"
        );
    }

    /// F-DUP, the acceptance criterion: dispatching the SAME (PR, reviewer) twice must refuse the
    /// second. `dispatch_issue` overwrites `running[id]`, which would drop the live entry's cancel
    /// handle and point a second agent at the first one's detached worktree.
    #[test]
    fn a_duplicate_review_dispatch_is_refused_and_the_live_run_survives() {
        let (mut o, dispatched) = orch_with_review(true);
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_key("makewhatis", "rhapsody", 12, "alice");
        let first_started = o.running[&id].started_at;
        // A handle on the LIVE run's cancellation, taken before the duplicate arrives.
        let live_cancel = o.running[&id].cancel.wait();

        // The watcher fires again before the first review finished — with a NEWER head, which is the
        // case that would look most like legitimate new work.
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_B)),
            ReviewDispatchOutcome::AlreadyInFlight
        );

        assert_eq!(
            dispatched.lock().expect("dispatched lock").len(),
            1,
            "a second agent was spawned onto the first one's worktree"
        );
        let live = &o.running[&id];
        assert_eq!(
            live.started_at, first_started,
            "the live entry was replaced"
        );
        assert_eq!(
            live.review.as_ref().map(|r| r.head_sha.as_str()),
            Some(HEAD_A),
            "the live run's pinned head was overwritten"
        );
        // The live entry's cancel signal still drives the handle the worker is waiting on — a
        // replaced RunningEntry would have dropped the sender, leaving the first worker unstoppable.
        live.cancel.cancel();
        assert!(
            live_cancel.is_cancelled(),
            "the live run's cancel handle was lost"
        );
        // The refusal wrote nothing: the watch set still names the head actually dispatched.
        let row = o
            .store()
            .get_review_watch(&review_run("alice", HEAD_A).watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(row.requested_sha, HEAD_A);
    }

    /// A run already CLAIMED but not yet in `running` is refused for the same reason.
    #[test]
    fn a_claimed_review_key_is_refused() {
        let (mut o, dispatched) = orch_with_review(true);
        o.claimed
            .insert(review_key("makewhatis", "rhapsody", 12, "alice"));
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::AlreadyInFlight
        );
        assert!(dispatched.lock().expect("dispatched lock").is_empty());
    }

    /// Two reviewers of one PR are two independent runs — the guard keys on (PR, reviewer), not PR.
    #[test]
    fn two_reviewers_of_one_pr_both_dispatch() {
        let (mut o, dispatched) = orch_with_review(true);
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        assert_eq!(
            o.dispatch_review(review_run("bob", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        assert_eq!(dispatched.lock().expect("dispatched lock").len(), 2);
        assert_eq!(o.running.len(), 2);
    }

    /// F-DUP's other half: `requested_sha` is recorded AT DISPATCH, not at completion. Without it the
    /// watcher's re-review condition stays true on every tick from introduction until the first
    /// completion, which is what produces the duplicate the guard above has to refuse.
    #[test]
    fn dispatch_records_the_requested_head_in_the_watch_set() {
        let (mut o, _dispatched) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        assert_eq!(
            o.dispatch_review(run.clone()),
            ReviewDispatchOutcome::Dispatched
        );

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("dispatch must introduce the row it writes to");
        assert_eq!(row.requested_sha, HEAD_A);
        assert_eq!(row.status, REVIEW_STATUS_IN_FLIGHT);
        assert_eq!(row.introduced_by, "handoff");
        assert!(row.open);
        assert!(
            row.last_reviewed_sha.is_empty(),
            "dispatch must not touch the reviewed SHA — that is the completion's to write"
        );
    }

    /// STUDIO-959: dispatch reads the reviewer's PRIOR commit off the existing row BEFORE it writes
    /// this head as requested.
    ///
    /// Mutation: reading `requested_sha` AFTER the writes would name this head as its own prior
    /// commit and red the delta assertions here; the placement is otherwise unobservable, because
    /// neither write moves `last_reviewed_sha` (see the production read above). Reading
    /// `requested_sha` BEFORE the writes is invisible to this test — its two seeded SHA columns are
    /// equal, so either read returns the same answer — which is why
    /// [`a_truncated_round_carries_no_prior_commit_so_the_next_head_is_full`] pins the column. And a
    /// reviewer with no row at all must carry nothing — a first round is full, so the worker must
    /// not be handed a delta request it cannot honour.
    #[test]
    fn dispatch_carries_the_reviewers_prior_commit_into_the_run() {
        let (mut o, dispatched) = orch_with_review(true);
        let prior = review_run("alice", HEAD_A);
        // A COMPLETED first round at HEAD_A: the row records what was READ.
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: prior.watch_key(),
                author: prior.author.clone(),
                introduced_by: prior.introduced_by.clone(),
                requested_sha: HEAD_A.to_string(),
                last_reviewed_sha: HEAD_A.to_string(),
                status: REVIEW_STATUS_REVIEWED.to_string(),
                open: true,
            })
            .expect("seed the prior round's row");

        let next = review_run("alice", HEAD_B);
        assert_eq!(
            o.dispatch_review(next.clone()),
            ReviewDispatchOutcome::Dispatched
        );

        {
            let entries = dispatched.lock().expect("dispatched lock");
            let re = entries.first().expect("one dispatch");
            let review = re.review.as_ref().expect("the entry carries its review");
            assert_eq!(
                review.prior_sha, HEAD_A,
                "the prior round's commit must be carried into the run"
            );
            let delta = review
                .checkout()
                .delta
                .expect("a prior commit makes a delta request for the worker");
            assert_eq!(delta.prior_sha, HEAD_A);
            assert_eq!(
                delta.head_sha, HEAD_B,
                "the delta runs to THIS round's head"
            );
        }
        let row = o
            .store()
            .get_review_watch(&next.watch_key())
            .expect("read")
            .expect("row");
        assert_eq!(row.requested_sha, HEAD_B);
        assert_eq!(
            row.last_reviewed_sha, HEAD_A,
            "dispatch must not advance the reviewed SHA"
        );

        // A reviewer with NO row is a first round: no prior commit, hence no delta request.
        let first = review_run("bob", HEAD_A);
        assert_eq!(o.dispatch_review(first), ReviewDispatchOutcome::Dispatched);
        let entries = dispatched.lock().expect("dispatched lock");
        let re = entries.last().expect("the second dispatch");
        let review = re.review.as_ref().expect("the entry carries its review");
        assert!(
            review.prior_sha.is_empty(),
            "a first round must carry no prior commit"
        );
        assert!(
            review.checkout().delta.is_none(),
            "a first round must not present a delta request"
        );
    }

    /// A truncated round — one ASKED to review [`HEAD_A`] but which read NOTHING — must leave the
    /// reviewer with no prior commit, so its next round at [`HEAD_B`] is FULL, not a delta
    /// (STUDIO-963).
    ///
    /// This is the state `mark_review_truncated` leaves behind when a round dies on `max_turns`, a
    /// drain or a worker failure: `requested_sha` stays at the head it was dispatched against while
    /// `last_reviewed_sha` stays empty. `dispatch` must read the LATTER. Reading `requested_sha`
    /// would tell this reviewer "you last read A — confirm those findings are addressed" when it
    /// read A not at all and filed no findings: trap 2 of STUDIO-959 reintroduced, for the same
    /// reviewer rather than a different one. The sibling
    /// [`dispatch_carries_the_reviewers_prior_commit_into_the_run`] cannot see that defect because
    /// its two SHA columns are equal.
    #[test]
    fn a_truncated_round_carries_no_prior_commit_so_the_next_head_is_full() {
        let (mut o, dispatched) = orch_with_review(true);
        let asked = review_run("alice", HEAD_A);
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: asked.watch_key(),
                author: asked.author.clone(),
                introduced_by: asked.introduced_by.clone(),
                requested_sha: HEAD_A.to_string(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_IN_FLIGHT.to_string(),
                open: true,
            })
            .expect("seed the round dispatched at HEAD_A");
        o.store()
            .mark_review_truncated(&asked.watch_key())
            .expect("record that the round read nothing");

        // The row really is the divergent shape the defect needs: asked at A, reviewed nothing.
        let seeded = o
            .store()
            .get_review_watch(&asked.watch_key())
            .expect("read")
            .expect("row");
        assert_eq!(seeded.requested_sha, HEAD_A);
        assert!(seeded.last_reviewed_sha.is_empty());
        assert_eq!(seeded.status, REVIEW_STATUS_TRUNCATED);

        let next = review_run("alice", HEAD_B);
        assert_eq!(
            o.dispatch_review(next.clone()),
            ReviewDispatchOutcome::Dispatched
        );

        let entries = dispatched.lock().expect("dispatched lock");
        let re = entries.first().expect("one dispatch");
        let review = re.review.as_ref().expect("the entry carries its review");
        assert!(
            review.prior_sha.is_empty(),
            "a truncated round read nothing, so the next round has no prior commit to diff from"
        );
        assert!(
            review.checkout().delta.is_none(),
            "a truncated round must leave the next round FULL, not a delta"
        );
    }

    /// The SAME head is pinned in all three places it is read from: the running entry, the worker's
    /// checkout coordinates, and the watch set. A re-query anywhere else is the F-SHA lost update.
    #[test]
    fn the_pinned_head_reaches_the_worker_and_the_watch_set_unchanged() {
        let (mut o, dispatched) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        let entries = dispatched.lock().expect("dispatched lock");
        let re = entries.first().expect("one dispatch");
        let review = re.review.as_ref().expect("the entry carries its review");
        assert_eq!(review.head_sha, HEAD_A);
        assert_eq!(review.checkout().pr_number, 12);
        assert_eq!(review.checkout().head_sha, HEAD_A);
        assert_eq!(
            re.identity, "alice",
            "the reviewer's identity was routed on"
        );
        assert_eq!(re.project_repo, REPO_URL, "routed to the PR's own project");
        assert_eq!(
            o.store()
                .get_review_watch(&run.watch_key())
                .expect("read")
                .expect("row")
                .requested_sha,
            HEAD_A
        );
    }

    /// §16: with Teams off the whole subsystem is dormant. Nothing is dispatched, nothing is
    /// claimed, and — the part that is easy to get wrong — nothing is WRITTEN either.
    #[test]
    fn teams_off_dispatches_nothing_and_writes_nothing() {
        let (mut o, dispatched) = orch_with_review(false);
        let run = review_run("alice", HEAD_A);

        assert_eq!(
            o.dispatch_review(run.clone()),
            ReviewDispatchOutcome::TeamsOff
        );

        assert!(dispatched.lock().expect("dispatched lock").is_empty());
        assert!(o.running.is_empty() && o.claimed.is_empty());
        assert!(
            o.store()
                .get_review_watch(&run.watch_key())
                .expect("read watch row")
                .is_none(),
            "a Teams-off daemon must leave the watch set untouched"
        );
        assert!(o.pending_review.is_empty());
    }

    /// Coordinates that cannot produce a review run are refused before anything is written — and a
    /// repo no configured project owns is one of them, which keeps a review confined to the
    /// repositories this daemon is bound to (design §14.1 F-SEC).
    #[test]
    fn malformed_or_unowned_coordinates_are_refused() {
        type Break = Box<dyn Fn(&mut ReviewRun)>;
        let cases: Vec<(&str, Break)> = vec![
            ("owner", Box::new(|r: &mut ReviewRun| r.owner.clear())),
            ("repo", Box::new(|r: &mut ReviewRun| r.repo.clear())),
            ("number", Box::new(|r: &mut ReviewRun| r.number = 0)),
            ("reviewer", Box::new(|r: &mut ReviewRun| r.reviewer.clear())),
            ("head", Box::new(|r: &mut ReviewRun| r.head_sha.clear())),
            (
                "unowned repo",
                Box::new(|r: &mut ReviewRun| {
                    r.repo_url = "git@github.com:evil/evil.git".to_string()
                }),
            ),
            (
                "no repo url",
                Box::new(|r: &mut ReviewRun| r.repo_url.clear()),
            ),
        ];
        for (what, break_it) in cases {
            let (mut o, dispatched) = orch_with_review(true);
            let mut run = review_run("alice", HEAD_A);
            break_it(&mut run);
            assert!(
                matches!(
                    o.dispatch_review(run.clone()),
                    ReviewDispatchOutcome::Refused(_)
                ),
                "{what} should be refused"
            );
            assert!(
                dispatched.lock().expect("dispatched lock").is_empty()
                    && o.running.is_empty()
                    && o.pending_review.is_empty(),
                "{what}: a refused dispatch left state behind"
            );
        }
    }

    /// A disabled project does not own its repo for dispatch purposes — a paused project must not
    /// have reviews run against it.
    #[test]
    fn a_disabled_project_does_not_own_its_repo() {
        let (mut o, _d) = orch_with_review(true);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].disabled = true;
        }
        assert!(matches!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Refused(_)
        ));
    }

    /// The staged coordinates are consumed by the dispatch they were staged for, exactly as a
    /// graphite stacking hint is — a leftover entry would attach a stale head to an unrelated run.
    #[test]
    fn dispatch_consumes_the_pending_review() {
        let (mut o, _d) = orch_with_review(true);
        o.dispatch_review(review_run("alice", HEAD_A));
        assert!(
            o.pending_review.is_empty(),
            "the staged review must be cleared by the dispatch that consumed it"
        );
    }

    /// Writes a profile file under a temp dir's `profiles/` subdirectory, creating it — the same
    /// helper `teams.rs`'s STUDIO-868 tests use, duplicated here rather than shared because the two
    /// modules' test scaffolding (`orch_with_review` vs `orch_with_teams`) does not otherwise touch.
    fn write_profile(dir: &TempDir, name: &str, text: &str) {
        let p = std::path::PathBuf::from(dir.child("profiles"));
        std::fs::create_dir_all(&p).expect("create profiles dir");
        std::fs::write(p.join(format!("{name}.md")), text).expect("write profile");
    }

    /// Drains the batched event writer so the rows it queued are readable — the same helper
    /// `teams.rs`'s STUDIO-868 tests use, duplicated for [`write_profile`]'s reason.
    fn flush_events(o: &mut Orchestrator) {
        o.stop_event_writer();
    }

    fn events_of(store: &dyn Store, run_id: i64) -> Vec<(String, String)> {
        store
            .run_events(run_id)
            .expect("run events")
            .into_iter()
            .map(|e| (e.kind, e.text))
            .collect()
    }

    // ── review.model / review.effort (STUDIO-901) ───────────────────────────

    /// **STUDIO-901's acceptance criterion, still green after STUDIO-908 scoped the key by
    /// harness.** A review run whose reviewer's OWN profile names a model/effort still uses
    /// `review.model.claude`/`review.effort.claude` when the operator set them for the Claude
    /// harness the reviewer runs — the review is what is being priced, not that teammate's own
    /// work (decision 1: review.model wins for a review run).
    ///
    /// Mutation check (acceptance #4, "never apply"): reverting the `review.model` block to a
    /// no-op leaves `cheap-model`/`low` from the reviewer's profile here and turns this red.
    #[test]
    fn review_model_wins_over_the_reviewers_own_profile_for_a_review_run() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
            teams.review.model = HarnessScoped::bare("premium-model");
            teams.review.effort = HarnessScoped::bare("xhigh");
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override,
            rhapsody_agent::ModelOverride {
                // Cleared, not "alice" (alice round 1 finding 2 on PR #168): the model/effort
                // came from `review.model`/`review.effort`, not alice's own profile, so a CLI
                // rejection must not attribute it to her.
                identity: String::new(),
                model: "premium-model".to_string(),
                effort: "xhigh".to_string(),
            },
            "review.model/effort must win over the reviewer's own profile for a review run"
        );
    }

    /// **jimmy round-1 finding 1 on PR #168, mutation-checked.** The `teams.route` events row is
    /// the durable per-run record of the model an operator reads back to see what a run cost
    /// (`RunningEntry::model_override`'s own doc, `route_teams`'s own doc). It must name what
    /// `review.model`/`review.effort` overrode to, not the reviewer's own profile — the exact
    /// mismatch a cost-allocation feature cannot afford. Reverting `record_route_event` to compose
    /// the suffix from `td.model_override` (the pre-override profile value) instead of
    /// `re.model_override` (the post-override final value) turns this red.
    #[test]
    fn the_route_event_names_the_review_override_not_the_reviewers_profile() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
            teams.review.model = HarnessScoped::bare("premium-model");
            teams.review.effort = HarnessScoped::bare("xhigh");
        }
        o.start_event_writer();

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );

        let id = review_run("alice", HEAD_A).key();
        let run_id = o.running[&id].run_id;
        flush_events(&mut o);
        assert_eq!(
            events_of(o.store.as_ref(), run_id),
            vec![(
                "teams.route".to_string(),
                "identity=alice reason=label model=premium-model effort=xhigh".to_string()
            )],
            "the durable row must name review.model/effort, not the reviewer's cheap-model profile"
        );
    }

    /// **Precedence in the other direction (acceptance: "tested in both directions").** The SAME
    /// teammate, dispatched as an ordinary ticket (an implementation run, not a review), keeps
    /// their own profile's model/effort untouched — `review.model` must never leak onto a run it
    /// was not written for.
    #[test]
    fn review_model_does_not_apply_to_an_implementation_run() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, dispatched) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
            teams.review.model = HarnessScoped::bare("premium-model");
            teams.review.effort = HarnessScoped::bare("xhigh");
        }

        o.dispatch_issue(
            rhapsody_core::Issue {
                id: "1".into(),
                identifier: "STUDIO-1".into(),
                title: "work".into(),
                state: "Todo".into(),
                // Routes directly to alice (tier 0), the same mechanism the review path's
                // synthetic issue uses — so this run is routed to the SAME identity the review
                // above was, and only the run KIND differs.
                labels: Some(vec!["rhapsody:@alice".to_string()]),
                ..Default::default()
            },
            None,
            None,
            String::new(),
        );

        let entries = dispatched.lock().expect("dispatched lock");
        assert_eq!(entries[0].identity, "alice", "sanity: routed to alice");
        assert_eq!(
            entries[0].model_override,
            rhapsody_agent::ModelOverride {
                identity: "alice".to_string(),
                model: "cheap-model".to_string(),
                effort: "low".to_string(),
            },
            "an implementation run must keep the routed teammate's own profile model/effort"
        );
    }

    /// **Absent means inherit (acceptance: byte-identical without `review.model`).** With
    /// `review.model`/`review.effort` unset — the default — a review run resolves exactly the
    /// model/effort its reviewer's profile would have given an ordinary dispatch: nothing new to
    /// observe on an installation that never wrote the key.
    #[test]
    fn absent_review_model_leaves_a_review_run_on_the_reviewers_own_profile() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
        }
        assert!(o.teams.as_ref().is_some_and(|t| t.review.model.is_empty()));

        o.dispatch_review(review_run("alice", HEAD_A));

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override,
            rhapsody_agent::ModelOverride {
                identity: "alice".to_string(),
                model: "cheap-model".to_string(),
                effort: "low".to_string(),
            },
            "an unset review.model/effort must not change the reviewer's own profile override"
        );
    }

    /// A partial override — `review.model` alone — leaves `effort` at whatever the profile (or the
    /// installation) already had, the same per-field non-empty-wins shape `turn_cfg` already
    /// applies to a profile override.
    #[test]
    fn review_model_alone_leaves_effort_at_the_profiles_own_value() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
            teams.review.model = HarnessScoped::bare("premium-model");
            // review.effort left unset.
        }

        o.dispatch_review(review_run("alice", HEAD_A));

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override,
            rhapsody_agent::ModelOverride {
                // Cleared for the same reason as the full-override case: `model` came from
                // `review.model`, not alice's profile, even though `effort` still did.
                identity: String::new(),
                model: "premium-model".to_string(),
                effort: "low".to_string(),
            }
        );
    }

    /// **alice round-1 finding 2 on PR #168, mutation-checked.** `ModelOverride.identity` exists
    /// so a CLI-rejected model fails as "teammate `<name>`'s profile asked for `<model>`" rather
    /// than nobody (`crates/agent/src/lib.rs`). A `review.model` that overrides the reviewer's
    /// profile must not leave that identity in place — the value came from the operator's
    /// `review:` block, not from the routed reviewer's profile, and blaming her for the operator's
    /// typo names the wrong file in the diagnostic. Reverting the `identity = String::new()` line
    /// in `retry.rs`'s review-override block turns this red.
    #[test]
    fn review_model_override_clears_the_reviewers_identity_from_the_diagnostic() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
            teams.review.model = HarnessScoped::bare("premium-model");
            teams.review.effort = HarnessScoped::bare("xhigh");
        }

        o.dispatch_review(review_run("alice", HEAD_A));

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override.identity, "",
            "a rejected review.model must not be blamed on the routed reviewer's own profile"
        );
    }

    /// The sibling of the above: with `review.model`/`review.effort` both unset, the override is
    /// entirely the reviewer's own profile, so the diagnostic naming her is exactly right and
    /// must NOT be cleared.
    #[test]
    fn absent_review_model_leaves_the_reviewers_identity_on_the_diagnostic() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "staff",
            "---\nextends: swe\nmodel: cheap-model\neffort: low\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "staff".to_string();
        }

        o.dispatch_review(review_run("alice", HEAD_A));

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override.identity, "alice",
            "with no review override at all, a rejected model IS the reviewer's own profile"
        );
    }

    // ── review.model scoped by harness (STUDIO-908) ─────────────────────────

    /// **The seam this ticket closes, acceptance #1.** An opencode reviewer's profile names
    /// `harness: opencode`; `review.model` names a model for opencode; the run's model override is
    /// that model — a model its provider serves — not the Claude one. Before this ticket the
    /// Claude `review.model` was applied unconditionally and the opencode CLI rejected it one
    /// second in with a provider-generic error.
    ///
    /// Mutation check (acceptance #4, "apply regardless of harness"): making the lookup ignore the
    /// harness — returning any entry, or the bare scalar — puts `claude-opus-5` on this opencode
    /// run and turns the assertion red.
    #[test]
    fn an_opencode_reviewer_runs_on_the_model_scoped_to_its_own_harness() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
            teams.review.model.insert("claude", "claude-opus-5").insert(
                "opencode",
                "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash",
            );
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );

        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override.model,
            "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash",
            "an opencode reviewer must run on the model scoped to its own harness"
        );
        assert_eq!(
            o.running[&id].model_override.identity, "",
            "the value came from review.model, not the reviewer's own profile"
        );
    }

    /// **STUDIO-909 acceptance: the review override's ORIGIN is recorded, not just its value.** This
    /// is the exact shape that was undiagnosable tonight — a reviewer whose PROFILE names one model
    /// while `review.model.opencode` substitutes another, so the run failed on a model the operator
    /// could not see in the job. The durable provenance must name `review.model.opencode` as the
    /// model's origin (and the opencode harness as `[profile]`), so the console renders the override
    /// rather than blaming the teammate's own profile value.
    ///
    /// Mutation check (acceptance: "make a review run report its profile's model instead of the
    /// override"): recording the provenance from `td.model_override`/the profile instead of the
    /// FINAL `re.model_override` reports `profile` and `cheap-model` here, turning this red — and the
    /// derived provider would read as unknown instead of `fireworks-ai`.
    #[test]
    fn a_review_runs_provenance_names_the_review_model_origin_not_the_profile() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nmodel: cheap-model\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
            teams.review.model.insert(
                "opencode",
                "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash",
            );
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_origin, "review.model.opencode",
            "the model came from the operator's review block, not alice's profile"
        );
        assert_eq!(
            o.running[&id].harness_origin, "profile",
            "the opencode harness came from alice's profile"
        );

        let run_id = o.running[&id].run_id;
        let p = o
            .store()
            .run_provenance(run_id)
            .expect("read provenance")
            .expect("a provenance row");
        assert_eq!(
            p.model,
            "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash"
        );
        assert_eq!(p.model_origin, "review.model.opencode");
        assert_eq!(p.harness, "opencode");
        assert_eq!(p.harness_origin, "profile");
        assert_eq!(p.provider, "fireworks-ai");
    }

    /// **Acceptance #5: the mixed roster — today's live configuration and the shape that broke.**
    /// One Claude reviewer and one opencode reviewer of the SAME pull request, each configured a
    /// model for their own harness, both dispatch and each run on its own. Mutation check
    /// ("apply regardless of harness"): both would receive `claude-opus-5`, reddening the opencode
    /// assertion.
    #[test]
    fn a_mixed_roster_runs_each_reviewer_on_the_model_scoped_to_their_own_harness() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, dispatched) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            // alice keeps the built-in `swe`, whose empty harness inherits `agent.backend` (claude
            // in this test); jerry overlays it with `harness: opencode`.
            teams.roster[0].profile = "swe".to_string();
            teams.roster.push(Identity {
                name: "jerry".to_string(),
                profile: "oc".to_string(),
                ..Identity::default()
            });
            teams
                .review
                .model
                .insert("claude", "claude-opus-5")
                .insert("opencode", "fireworks-ai/x");
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        assert_eq!(
            o.dispatch_review(review_run("jerry", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        assert_eq!(dispatched.lock().expect("dispatched lock").len(), 2);
        assert_eq!(
            o.running[&review_run("alice", HEAD_A).key()]
                .model_override
                .model,
            "claude-opus-5"
        );
        assert_eq!(
            o.running[&review_run("jerry", HEAD_A).key()]
                .model_override
                .model,
            "fireworks-ai/x"
        );
    }

    /// **Acceptance #3.** A `review.model` the reviewer's harness cannot serve is refused at
    /// dispatch, with a message naming the harness, the model and the `review.model` origin — not
    /// the provider's generic `UnknownError`. Nothing is written, so the watcher cannot mistake a
    /// refused review for one in flight.
    ///
    /// Mutation check: removing the `dispatch_review` refusal (letting `dispatch_issue` apply the
    /// Claude model to the opencode run) turns this red.
    #[test]
    fn a_review_model_scoped_to_another_harness_is_refused_naming_harness_model_and_origin() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, dispatched) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
            teams.review.model.insert("claude", "claude-opus-5");
        }

        let outcome = o.dispatch_review(review_run("alice", HEAD_A));
        let ReviewDispatchOutcome::Refused(why) = outcome else {
            panic!("an opencode reviewer must be refused, not run on a claude model: {outcome:?}");
        };
        assert!(why.contains("opencode"), "must name the harness: {why}");
        assert!(why.contains("claude-opus-5"), "must name the model: {why}");
        assert!(why.contains("review.model"), "must name the origin: {why}");

        assert!(
            dispatched.lock().expect("dispatched lock").is_empty(),
            "a refused review must not spawn an agent"
        );
        assert!(
            o.running.is_empty(),
            "a refused review must leave no run behind"
        );
        assert!(o.pending_review.is_empty());
        assert!(
            o.store()
                .get_review_watch(&review_run("alice", HEAD_A).watch_key())
                .expect("read watch row")
                .is_none(),
            "a refused review must not record its head as requested"
        );
    }

    /// The other half of the decision (acceptance: the fail-CLOSED direction): an opencode reviewer
    /// whose harness has no `review.model` entry is REFUSED, never silently downgraded to its own
    /// (cheap) profile model — that silent downgrade is exactly what `review.model` exists to
    /// prevent.
    #[test]
    fn an_opencode_reviewer_with_no_scoped_review_model_is_refused_not_silently_cheap() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nmodel: cheap-model\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
            // Only a Claude entry: the opencode reviewer has nothing scoped to it.
            teams.review.model.insert("claude", "claude-opus-5");
        }

        assert!(
            matches!(
                o.dispatch_review(review_run("alice", HEAD_A)),
                ReviewDispatchOutcome::Refused(_)
            ),
            "an unconfigured harness must not quietly review on the cheap profile model"
        );
    }

    /// With `review.model` unset entirely, an opencode reviewer inherits its own profile model —
    /// the byte-identical-without-the-key property STUDIO-901 promised, preserved per harness.
    #[test]
    fn an_opencode_reviewer_with_no_review_model_at_all_inherits_its_profile() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "oc",
            "---\nextends: swe\nmodel: cheap-model\nharness: opencode\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
        }
        assert!(o.teams.as_ref().is_some_and(|t| t.review.model.is_empty()));

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_run("alice", HEAD_A).key();
        assert_eq!(o.running[&id].model_override.model, "cheap-model");
        assert_eq!(o.running[&id].model_override.identity, "alice");
    }

    /// **alice's blocking finding on PR #172.** The legacy bare `review.model` spelling belongs to
    /// the installation's configured `agent.backend`, not a hardcoded `claude`. On an all-opencode
    /// installation — `agent.backend: opencode`, every profile naming no harness, exactly alice's
    /// probe — a bare `review.model` works today and must keep working; scoping it to `claude`
    /// refused every ticketless review on that install. Mutation check: resolving `bare()` under a
    /// hardcoded `claude` turns this refusal red.
    #[test]
    fn a_legacy_bare_review_model_applies_on_an_all_opencode_installation() {
        let dir = TempDir::new();
        // No `harness:` line: this profile inherits the installation's backend, as an all-opencode
        // install's profiles do.
        write_profile(&dir, "oc", "---\nextends: swe\n---\nStaff.\n");
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        o.eff.as_mut().expect("eff").cfg.agent.backend = "opencode".to_string();
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "oc".to_string();
            teams.review.model = HarnessScoped::bare("some-opencode-model");
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_run("alice", HEAD_A).key();
        assert_eq!(
            o.running[&id].model_override.model, "some-opencode-model",
            "the bare scalar belongs to the configured backend, which is opencode here"
        );
    }

    /// The other direction of the same finding: the bare scalar is still refused for a reviewer on
    /// a harness OTHER than the configured backend — it is never silently re-scoped to make a
    /// mismatch go away. And the refusal names the backend as the value's own harness, not a
    /// `claude` the operator never wrote.
    #[test]
    fn a_legacy_bare_review_model_is_refused_for_a_reviewer_on_a_different_harness() {
        let dir = TempDir::new();
        write_profile(
            &dir,
            "cl",
            "---\nextends: swe\nharness: claude\n---\nStaff.\n",
        );
        let (mut o, _d) = orch_with_review(true);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        o.eff.as_mut().expect("eff").cfg.agent.backend = "opencode".to_string();
        if let Some(teams) = o.teams.as_mut() {
            teams.roster[0].profile = "cl".to_string();
            teams.review.model = HarnessScoped::bare("some-opencode-model");
        }

        let ReviewDispatchOutcome::Refused(why) = o.dispatch_review(review_run("alice", HEAD_A))
        else {
            panic!("a claude reviewer must not be handed the opencode backend's model");
        };
        assert!(
            why.contains("opencode (model some-opencode-model)"),
            "the value's harness must be named as opencode, not claude: {why}"
        );
        assert!(
            why.contains("claude"),
            "must name the reviewer's harness: {why}"
        );
    }

    /// A ticket dispatch is untouched by any of this: no review coordinates, so the worker takes the
    /// existing provisioning path and the agent gets no review env.
    #[test]
    fn a_ticket_dispatch_carries_no_review() {
        let (mut o, dispatched) = orch_with_review(true);
        o.dispatch_issue(
            rhapsody_core::Issue {
                id: "1".into(),
                identifier: "STUDIO-1".into(),
                title: "work".into(),
                state: "Todo".into(),
                ..Default::default()
            },
            None,
            None,
            String::new(),
        );
        let entries = dispatched.lock().expect("dispatched lock");
        assert!(entries[0].review.is_none());
    }

    /// A `pr:` key that reaches `dispatch_issue` WITHOUT its coordinates — the shape a retry or any
    /// future caller could produce — must not be dispatched at all. Dispatching it would take the
    /// ordinary provisioning path and check out the default branch on a `symphony/pr_…` branch,
    /// which is exactly the outcome review mode exists to prevent.
    #[test]
    fn a_review_key_without_coordinates_is_not_dispatched() {
        let (mut o, dispatched) = orch_with_review(true);
        let iss = review_run("alice", HEAD_A).synthetic_issue();

        o.dispatch_issue(iss.clone(), None, None, String::new());

        assert!(
            dispatched.lock().expect("dispatched lock").is_empty(),
            "a review key was dispatched as an ordinary ticket"
        );
        assert!(!o.running.contains_key(&iss.id) && !o.claimed.contains(&iss.id));
    }

    /// Hands a dispatched review its worker exit and returns the run row id it was recorded on.
    fn exit_review(o: &mut Orchestrator, run: &ReviewRun, failed: bool, err_msg: &str) -> i64 {
        exit_review_as(o, run, failed, err_msg, true, "")
    }

    /// [`exit_review`] with the two fields the STUDIO-721 exit path reads spelled out: whether the
    /// agent DECLARED it had finished (false ⇒ the `max_turns` backstop fired) and the verdict the
    /// review branch of `run_turns` puts in the state slot.
    fn exit_review_as(
        o: &mut Orchestrator,
        run: &ReviewRun,
        failed: bool,
        err_msg: &str,
        declared_handoff: bool,
        last_state: &str,
    ) -> i64 {
        let id = run.key();
        let re = o.running.get(&id).expect("the review is running");
        let (started_at, run_id) = (re.started_at, re.run_id);
        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: id,
            failed,
            started_at,
            err_msg: err_msg.to_string(),
            // A synthetic `pr:` issue has no tracker state, so BOTH of the classifier's samples are
            // empty — the exact input that made every clean review exit an OUTCOME_CONTINUED.
            last_state: last_state.to_string(),
            declared_handoff,
            refused: false,
        });
        run_id
    }

    /// Drains everything a review exit has queued for the notification task, as `(pr, approved)`
    /// pairs. Reading the receiver directly is what makes "notified" a fact about the CHANNEL
    /// rather than about a log line.
    fn drain_notifications(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::reviewnotify::ReviewCompletion>,
    ) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push((c.to_string(), c.approved));
        }
        out
    }

    /// STUDIO-723, the acceptance wiring: a DECLARED review completion queues exactly one
    /// author-facing comment, and its verdict decides whether that comment summons. A review that
    /// left findings must re-engage the author — nothing else advances the head, so without this
    /// the re-review loop has no second lap.
    #[test]
    fn a_declared_review_completion_queues_one_comment_carrying_its_verdict() {
        for (verdict, approved) in [
            (REVIEW_STATE_FINDINGS, false),
            (REVIEW_STATE_APPROVED, true),
        ] {
            let (mut o, _d) = orch_with_review(true);
            let mut rx = o.open_review_notify_channel();
            let run = review_run("alice", HEAD_A);
            o.dispatch_review(run.clone());
            exit_review_as(&mut o, &run, false, "", true, verdict);

            assert_eq!(
                drain_notifications(&mut rx),
                vec![("makewhatis/rhapsody#12".to_string(), approved)],
                "verdict {verdict}"
            );
        }
    }

    /// STUDIO-839, the acceptance wiring for the OTHER consequence of the same verdict: the
    /// completion a findings exit queues carries the ticket's route-back, and the one an approved
    /// exit queues does not.
    ///
    /// Pinned at the exit rather than only at `plan_review_changes`, because the planner being
    /// right is worth nothing if nothing calls it — a refactor that drops the wiring here leaves
    /// every route-back test in `reviewchanges` green while no ticket ever moves, which is the
    /// silent-green shape this whole subsystem keeps producing.
    #[test]
    fn a_findings_exit_queues_its_tickets_route_back_and_an_approved_one_does_not() {
        for (verdict, want) in [
            (REVIEW_STATE_FINDINGS, Some("In Progress")),
            (REVIEW_STATE_APPROVED, None),
        ] {
            let (mut o, _d) = orch_with_review(true);
            if let Some(t) = o.teams.as_mut() {
                t.review.changes_state = "In Progress".to_string();
            }
            // The ticket the route-back is for, and the run row its opaque ids are read off.
            let mut run = review_run("bob", HEAD_A);
            run.introduced_by = "handoff:STUDIO-839".to_string();
            o.store()
                .start_run(rhapsody_store::RunStart {
                    issue_id: "ID-839".to_string(),
                    issue_identifier: "STUDIO-839".to_string(),
                    team_id: "TEAM-1".to_string(),
                    ..rhapsody_store::RunStart::default()
                })
                .expect("start run");
            let mut rx = o.open_review_notify_channel();
            o.dispatch_review(run.clone());
            exit_review_as(&mut o, &run, false, "", true, verdict);

            let queued: Vec<Option<String>> = std::iter::from_fn(|| rx.try_recv().ok())
                .map(|c| c.changes.map(|p| p.state))
                .collect();
            assert_eq!(queued, vec![want.map(str::to_string)], "verdict {verdict}");
        }
    }

    /// And the unconfigured default, at the same seam: an installation that has not named
    /// `teams.review.changes_state` queues a completion carrying no route-back at all, so its
    /// behaviour is byte-identical to the daemon before this transition existed.
    #[test]
    fn an_unconfigured_daemon_queues_no_route_back() {
        let (mut o, _d) = orch_with_review(true);
        let mut run = review_run("bob", HEAD_A);
        run.introduced_by = "handoff:STUDIO-839".to_string();
        o.store()
            .start_run(rhapsody_store::RunStart {
                issue_id: "ID-839".to_string(),
                issue_identifier: "STUDIO-839".to_string(),
                team_id: "TEAM-1".to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
        let mut rx = o.open_review_notify_channel();
        o.dispatch_review(run.clone());
        exit_review_as(&mut o, &run, false, "", true, REVIEW_STATE_FINDINGS);

        let queued: Vec<Option<crate::reviewchanges::ReviewChangesPlan>> =
            std::iter::from_fn(|| rx.try_recv().ok())
                .map(|c| c.changes)
                .collect();
        assert_eq!(queued, vec![None]);
    }

    /// The two exits that are NOT completions notify nobody. A crashed round read nothing, so there
    /// are no findings to point the author at; a `max_turns` round is re-armed at the SAME head
    /// (`record_review_truncated`), so telling the author to push fixes would ask them to advance
    /// past a head nobody has finished reading.
    #[test]
    fn a_failed_or_truncated_review_notifies_nobody() {
        for (failed, declared) in [(true, true), (false, false)] {
            let (mut o, _d) = orch_with_review(true);
            let mut rx = o.open_review_notify_channel();
            let run = review_run("alice", HEAD_A);
            o.dispatch_review(run.clone());
            exit_review_as(
                &mut o,
                &run,
                failed,
                "boom",
                declared,
                REVIEW_STATE_FINDINGS,
            );

            assert!(
                drain_notifications(&mut rx).is_empty(),
                "failed={failed} declared_handoff={declared} must queue no comment"
            );
        }
    }

    /// §16, at the exit: a daemon that never opened the channel cannot represent a comment, so a
    /// review exit on any other configuration posts nothing and still records the round normally.
    #[test]
    fn a_review_exit_without_a_notification_channel_still_records_the_round() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());
        exit_review_as(&mut o, &run, false, "", true, REVIEW_STATE_FINDINGS);

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("the row exists");
        assert_eq!(row.last_reviewed_sha, HEAD_A);
        assert_eq!(row.status, REVIEW_STATUS_REVIEWED);
    }

    /// STUDIO-721, the slice-4 nit: an agent that burned its whole turn budget WITHOUT declaring it
    /// had finished read this head partially at best. Recording it `reviewed` at the pinned head is
    /// what ships a partial — or entirely absent — review, because the watcher's edge-trigger then
    /// sees `last_reviewed_sha == head` and never looks at this head again.
    #[test]
    fn a_max_turns_truncated_review_is_recorded_non_terminally() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        exit_review_as(&mut o, &run, false, "", false, REVIEW_STATE_FINDINGS);

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(
            row.status,
            rhapsody_store::REVIEW_STATUS_TRUNCATED,
            "a budget-ended round must not be recorded with a terminal status"
        );
        assert_eq!(
            row.last_reviewed_sha, "",
            "nothing was fully read, so no head was reviewed"
        );
        assert_eq!(
            row.requested_sha, HEAD_A,
            "the head that still needs reviewing is left in place"
        );
    }

    /// STUDIO-894, the acceptance criterion round-tripped through the real exit path: a reviewer
    /// that DID declare a hand-off (so the max_turns branch above does not fire) but whose payload
    /// is not a recognised verdict must not be recorded `reviewed`. The row is left non-terminal
    /// (distinguishable from both `approved` and `reviewed` "in the row"), no author-facing comment
    /// is queued (an undeclared round is not a completion), and the daemon says so loudly rather
    /// than silently ("in the log").
    ///
    /// This is NOT the shape PR #161's round 3 hit — that run's own transcript emitted an explicit
    /// `HANDOFF: findings`, a declared and correctly-parsed rejection, despite prose that concluded
    /// "Approve." (jimmy's STUDIO-894 review, round 1). That is a reviewer-prompt ambiguity — the
    /// binary approved/findings vocabulary has no way to say "approve, with non-blocking nits" — and
    /// is out of scope here; see the pull request body for the follow-up. What this test pins is the
    /// narrower, real gap this ticket is about: a `HANDOFF:` line whose payload cannot be parsed as
    /// either verdict must not be guessed at as a rejection.
    #[test]
    fn an_undeclared_verdict_is_recorded_non_terminally_and_logged_loudly_not_as_changes_requested()
    {
        let (mut o, _d) = orch_with_review(true);
        let mut rx = o.open_review_notify_channel();
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        let (_, events) = crate::testsupport::capture_events(|| {
            exit_review_as(&mut o, &run, false, "", true, REVIEW_STATE_UNDECLARED);
        });

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(
            row.status,
            rhapsody_store::REVIEW_STATUS_TRUNCATED,
            "an undeclared verdict must not be recorded as either approved or reviewed"
        );
        assert_eq!(
            row.last_reviewed_sha, "",
            "nothing this daemon could parse as a verdict was recorded as having been read"
        );
        assert!(
            drain_notifications(&mut rx).is_empty(),
            "an undeclared round is not a completion, so the author is not summoned over it"
        );
        assert!(
            events.iter().any(|e| e.message.contains(
                "declared a hand-off but its payload is neither `approved` nor a recognised \
                 rejection"
            )),
            "the daemon must say loudly, in the log, that it would not guess a verdict: {events:?}"
        );
    }

    /// STUDIO-880: the third dispatch entry point refuses a drain, and refuses it BEFORE it writes.
    ///
    /// `handle_review_sweep` → `dispatch_review` → `dispatch_issue` reaches dispatch from
    /// `Event::ReviewSweep`, past both `on_tick`'s gate and `on_retry`'s park. The refusal has to
    /// come before the watch-set writes: those record the head as requested and mark the row
    /// in-flight, and a watcher that believes a review is in flight is edge-triggered and never
    /// offers this head again. So the assertion is on the ROW as much as on the outcome.
    #[test]
    fn a_draining_daemon_refuses_a_review_dispatch_and_writes_nothing() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.drain
            .arm(chrono::Utc::now(), crate::drain::DrainReason::Update);

        assert_eq!(
            o.dispatch_review(run.clone()),
            ReviewDispatchOutcome::Draining
        );
        assert!(
            o.store()
                .get_review_watch(&run.watch_key())
                .expect("read watch row")
                .is_none(),
            "the refusal happened before the watch-set write, so there is no row claiming a review \
             is in flight at this head"
        );
        assert!(
            !o.running.contains_key(&run.key()) && !o.claimed.contains(&run.key()),
            "and nothing was claimed"
        );

        // Cancelling the drain re-opens the same dispatch: a drain defers review work, it does not
        // consume it.
        o.drain.disarm();
        assert_eq!(
            o.dispatch_review(run.clone()),
            ReviewDispatchOutcome::Dispatched
        );
    }

    /// STUDIO-880: the SAME non-terminal parking, attributed to the right cause.
    ///
    /// A drained review reaches the truncation branch too — the drain ends the turn loop at a
    /// boundary and the agent never gets to declare `HANDOFF:` — so both causes land here and the
    /// bookkeeping is right for both. Only the log line distinguishes them, and "ended on the
    /// max_turns backstop" sends whoever reads it looking for a review that ran away, when what
    /// happened was an operator asking the daemon to settle.
    #[test]
    fn a_drained_review_is_not_logged_as_a_runaway_one() {
        for draining in [false, true] {
            let (mut o, _d) = orch_with_review(true);
            let run = review_run("alice", HEAD_A);
            o.dispatch_review(run.clone());
            if draining {
                o.drain
                    .arm(chrono::Utc::now(), crate::drain::DrainReason::Operator);
            }

            let (_, events) = crate::testsupport::capture_events(|| {
                exit_review_as(&mut o, &run, false, "", false, REVIEW_STATE_FINDINGS);
            });
            let line = events
                .iter()
                .find(|e| e.message.contains("without declaring it had finished"))
                .map(|e| e.message.clone())
                .unwrap_or_else(|| panic!("draining={draining}: the truncation warning is gone"));

            if draining {
                assert!(
                    line.contains("wound down at a turn boundary for an armed drain"),
                    "a drained review must name the drain: {line}"
                );
                assert!(
                    !line.contains("max_turns"),
                    "…and must not blame the turn budget it never spent: {line}"
                );
            } else {
                assert!(
                    line.contains("ended on the max_turns backstop"),
                    "an undrained one still names the backstop: {line}"
                );
            }

            // The classification is identical either way — that is the point of pinning the cause
            // rather than the outcome.
            let row = o
                .store()
                .get_review_watch(&run.watch_key())
                .expect("read watch row")
                .expect("row exists");
            assert_eq!(row.status, rhapsody_store::REVIEW_STATUS_TRUNCATED);
            assert_eq!(row.requested_sha, HEAD_A);
        }
    }

    /// §15-c: a review that found nothing declares `HANDOFF: approved`, and the round is recorded
    /// `approved` at the head it read — the terminal that pauses re-review while the pull request
    /// stays at that head.
    #[test]
    fn an_approving_declaration_is_recorded_approved_at_the_pinned_head() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        exit_review_as(&mut o, &run, false, "", true, REVIEW_STATE_APPROVED);

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(row.status, REVIEW_STATUS_APPROVED);
        assert_eq!(row.last_reviewed_sha, HEAD_A);
    }

    /// The verdict is read off the agent's OWN hand-off line, and only an exact `approved` payload
    /// counts as an approval.
    #[test]
    fn only_an_exact_approved_payload_reads_as_an_approval() {
        for approving in [
            "HANDOFF: approved",
            "posted nothing\n  HANDOFF:  Approved  ",
            "HANDOFF:approved",
        ] {
            assert_eq!(
                review_exit_state(approving),
                REVIEW_STATE_APPROVED,
                "{approving:?}"
            );
        }
    }

    /// ⚠️ STUDIO-894's mutation target, pinned on its own: `HANDOFF: not approved` — the exact
    /// phrase [`crate::automerge`]'s module docs and every reader of this function assume still
    /// works — must keep reading as a DECLARED rejection, never as an approval and never as
    /// undeclared. A change that loosens the approval check would flip this to an approval; a
    /// change that narrows the rejection check to `findings` only would flip it to undeclared.
    /// Either mutation must turn this assertion red on its own.
    #[test]
    fn handoff_not_approved_still_records_as_changes_requested() {
        assert_eq!(
            review_exit_state("HANDOFF: not approved"),
            REVIEW_STATE_FINDINGS
        );
        assert_eq!(
            review_exit_state("  HANDOFF:  Not Approved  "),
            REVIEW_STATE_FINDINGS,
            "case- and whitespace-insensitive, like every other payload comparison here"
        );
    }

    /// The wording `reviewprompt/review-base.md` actually instructs for a rejection — `HANDOFF:
    /// findings` — reads as the same declared rejection as `not approved` does.
    #[test]
    fn handoff_findings_records_as_changes_requested() {
        assert_eq!(
            review_exit_state("posted 2 findings\nHANDOFF: findings"),
            REVIEW_STATE_FINDINGS
        );
    }

    /// STUDIO-894, the acceptance criterion: a `HANDOFF:` line whose payload is neither `approved`
    /// nor a recognised rejection is UNDECLARED, not a silent "changes requested". The old behaviour
    /// defaulted every one of these — including a reviewer's own approval spelled slightly
    /// differently — to [`REVIEW_STATE_FINDINGS`] with no signal that anything had been guessed at.
    /// The literal PR #161 heading below is included for that reason — it pins the function's OWN
    /// domain contract in isolation — not because it is the input that produced #161's incident: that
    /// run's actual payload was a declared, well-formed `HANDOFF: findings`, which is (correctly)
    /// [`REVIEW_STATE_FINDINGS`] both before and after this change (see
    /// `handoff_findings_records_as_changes_requested` above). A result with no `HANDOFF:` line at
    /// all falls in the same UNDECLARED bucket: this function never sees that case in production (the
    /// `declared_handoff` check ahead of it in [`Orchestrator::on_review_exit`] intercepts it first),
    /// but the function's own domain must not silently call it a rejection either.
    #[test]
    fn an_unrecognised_payload_is_undeclared_rather_than_a_guessed_rejection() {
        for undeclared in [
            // No `HANDOFF:` line at all — unreachable in production (`declared_handoff`
            // intercepts it first), pinned here as the function's own domain contract.
            "",
            "## Review round 3 — @symphony: approve",
            "HANDOFF: review-posted",
            "HANDOFF: approved with nits",
            "approved",
            "I approved of the change\nHANDOFF: 3 findings",
        ] {
            assert_eq!(
                review_exit_state(undeclared),
                REVIEW_STATE_UNDECLARED,
                "{undeclared:?}"
            );
        }
    }

    /// F4, the acceptance criterion: a clean review exit is recorded COMPLETED and schedules no
    /// continuation. `classify_clean_exit` would read the two empty state samples as "still active"
    /// and re-dispatch the same review every second, forever, holding the reviewer's slot.
    #[test]
    fn a_clean_review_exit_records_completed_and_schedules_no_continuation() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        let run_id = exit_review(&mut o, &run, false, "");

        assert!(
            o.retry_attempts.is_empty() && o.retry_timers.is_empty(),
            "a review exit scheduled a continuation retry"
        );
        assert!(
            !o.claimed.contains(&run.key()) && !o.completed.contains(&run.key()),
            "the review key stayed claimed, so it can never be reviewed again"
        );
        assert!(!o.running.contains_key(&run.key()));
        let row = o
            .store()
            .get_run(run_id)
            .expect("read run row")
            .expect("run row exists");
        assert_eq!(row.outcome, rhapsody_store::OUTCOME_COMPLETED);
    }

    /// F-SHA: the SHA recorded as reviewed is the one PINNED AT CHECKOUT, carried on the running
    /// entry — never a completion-time reading of where the pull request's head is now. Here the
    /// watch set has already moved on to a newer head (the shape a mid-review push produces), and
    /// the completion must still record the head the reviewer actually read.
    #[test]
    fn a_review_exit_records_the_pinned_head_not_the_newer_one() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());
        // The author pushes while the review runs; the watcher observes the new head.
        o.store()
            .mark_review_requested(&run.watch_key(), HEAD_B)
            .expect("observe the new head");

        exit_review(&mut o, &run, false, "");

        let row = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(
            row.last_reviewed_sha, HEAD_A,
            "recording the live head marks commits reviewed that nobody read"
        );
        assert_eq!(row.status, REVIEW_STATUS_REVIEWED);
        assert_eq!(
            row.requested_sha, HEAD_B,
            "the requested head is not the completion's to move"
        );
    }

    /// The status domain is closed and the WRITER enforces it — `mark_review_completed` takes a
    /// plain string and cannot (the STUDIO-711 review nit). A status the watcher cannot recognise
    /// must not reach the row at all.
    #[test]
    fn an_out_of_domain_completion_status_is_refused() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        for bad in ["", "in_flight", "requested", "dropped", "Reviewed", "done"] {
            o.record_review_completed(&run, bad);
            let row = o
                .store()
                .get_review_watch(&run.watch_key())
                .expect("read watch row")
                .expect("row exists");
            assert_eq!(row.status, REVIEW_STATUS_IN_FLIGHT, "{bad} was written");
            assert!(
                row.last_reviewed_sha.is_empty(),
                "{bad} moved the reviewed head"
            );
        }
        // …and the two in-domain values are accepted.
        for good in [REVIEW_STATUS_REVIEWED, REVIEW_STATUS_APPROVED] {
            o.record_review_completed(&run, good);
            let row = o
                .store()
                .get_review_watch(&run.watch_key())
                .expect("read watch row")
                .expect("row exists");
            assert_eq!(row.status, good);
            assert_eq!(row.last_reviewed_sha, HEAD_A);
        }
    }

    /// A FAILED review run is recorded failed and, like a clean one, schedules no retry: a `pr:`
    /// key can never be re-dispatched through the retry queue (`dispatch_issue` refuses a review
    /// key with no coordinates), so a backoff timer would only hold the claim. Its watch row is
    /// left where the dispatch put it — re-arming a crashed review is the watcher's call, and
    /// recording an unread head as reviewed would be the F-SHA lost update by another route.
    #[test]
    fn a_failed_review_exit_records_failed_and_schedules_no_retry() {
        let (mut o, _d) = orch_with_review(true);
        let run = review_run("alice", HEAD_A);
        o.dispatch_review(run.clone());

        let run_id = exit_review(&mut o, &run, true, "claude startup failed");

        assert!(
            o.retry_attempts.is_empty() && o.retry_timers.is_empty(),
            "a failed review scheduled a backoff retry that could never dispatch"
        );
        assert!(!o.claimed.contains(&run.key()));
        let row = o
            .store()
            .get_run(run_id)
            .expect("read run row")
            .expect("run row exists");
        assert_eq!(row.outcome, rhapsody_store::OUTCOME_FAILED);
        assert_eq!(row.error, "claude startup failed");
        assert!(
            o.store()
                .load_recovery()
                .expect("load recovery")
                .claims
                .is_empty(),
            "the persisted claim row outlives the run and greets boot recovery"
        );
        let watch = o
            .store()
            .get_review_watch(&run.watch_key())
            .expect("read watch row")
            .expect("row exists");
        assert_eq!(watch.status, REVIEW_STATUS_IN_FLIGHT);
        assert!(watch.last_reviewed_sha.is_empty());
    }

    /// Runs a git command in `dir` with a deterministic identity; panics on failure (test helper).
    fn git_run(dir: &str, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A local origin with one commit on `main` and a `refs/pull/<n>/head` pointing at a commit on
    /// no branch — the shape a real pull request head has. Returns the head SHA.
    fn origin_with_pr_head(dir: &TempDir, pr_number: i64) -> String {
        git_run(&dir.path, &["init", "-b", "main"]);
        std::fs::write(dir.child("README.md"), "hello\n").expect("write README");
        git_run(&dir.path, &["add", "README.md"]);
        git_run(&dir.path, &["commit", "-m", "initial"]);
        git_run(&dir.path, &["checkout", "-b", "pr-work"]);
        git_run(&dir.path, &["commit", "--allow-empty", "-m", "pr head"]);
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&dir.path)
            .output()
            .expect("rev-parse");
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        git_run(
            &dir.path,
            &["update-ref", &format!("refs/pull/{pr_number}/head"), &sha],
        );
        git_run(&dir.path, &["checkout", "main"]);
        git_run(&dir.path, &["branch", "-D", "pr-work"]);
        sha
    }

    /// Teardown, end to end and against a real worktree: a review run's exit removes the detached
    /// worktree, on a clean exit and on a failed one alike. Nothing else ever would — a `pr:` id
    /// reaches no terminal tracker state, so `reconcile`'s TerminateCleanup never fires for it and
    /// the tree would leak once per review.
    #[tokio::test]
    async fn a_review_run_exit_removes_its_detached_worktree() {
        for failed in [false, true] {
            review_exit_removes_the_worktree(failed).await;
        }
    }

    async fn review_exit_removes_the_worktree(failed: bool) {
        let origin = TempDir::new();
        let head = origin_with_pr_head(&origin, 12);
        let root = TempDir::new();
        let ws = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: rhapsody_workspace::HookScripts::default(),
                hook_timeout: std::time::Duration::from_secs(30),
            })
            .expect("workspace manager"),
        );
        let (mut o, _d) = orch_with_review(true);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].repo = origin.path.clone();
            eff.projects[0].workspace = Arc::clone(&ws);
        }
        let mut run = review_run("alice", &head);
        run.repo_url = origin.path.clone();

        // Provision exactly as the worker does, then hand the run its exit.
        let provisioned = ws
            .ensure_review_worktree(&run.repo_url, "rhapsody", &run.key(), 12, &head)
            .await
            .expect("provision review worktree");
        assert!(std::fs::metadata(&provisioned.path).is_ok());

        let started_at = chrono::Utc::now();
        let mut re = RunningEntry::empty(run.synthetic_issue());
        re.started_at = started_at;
        re.project_slug = "rhapsody".to_string();
        re.project_repo = run.repo_url.clone();
        re.review = Some(run.clone());
        o.running.insert(run.key(), re);
        o.claimed.insert(run.key());
        let signal = crate::control_loop::CancelSignal::new();
        o.set_ctx(signal.wait());

        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: run.key(),
            failed,
            started_at,
            err_msg: String::new(),
            last_state: String::new(),
            declared_handoff: true,
            refused: false,
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), o.wg.wait())
            .await
            .expect("teardown task finished");
        assert!(
            std::fs::metadata(&provisioned.path).is_err(),
            "the review worktree leaked (failed={failed}): {}",
            provisioned.path
        );
    }

    /// STUDIO-716: `POST /api/v1/runs/{id}/stop` on a review run removes its detached worktree too.
    ///
    /// Stop never reaches `on_worker_exit`'s teardown. `handle_stop_run` -> `terminate` removes the
    /// entry and fires the cancellation, and the worker's later exit event then hits the
    /// stale/absent guard and returns BEFORE the teardown — so a stopped review used to leak its
    /// `pr_<owner>_<repo>_<n>_<reviewer>` tree permanently, with nothing left that could name it.
    #[tokio::test]
    async fn stopping_a_review_run_removes_its_detached_worktree() {
        let origin = TempDir::new();
        let head = origin_with_pr_head(&origin, 12);
        let root = TempDir::new();
        let ws = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: rhapsody_workspace::HookScripts::default(),
                hook_timeout: std::time::Duration::from_secs(30),
            })
            .expect("workspace manager"),
        );
        let (mut o, _d) = orch_with_review(true);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].repo = origin.path.clone();
            eff.projects[0].workspace = Arc::clone(&ws);
        }
        let signal = crate::control_loop::CancelSignal::new();
        o.set_ctx(signal.wait());

        let mut run = review_run("alice", &head);
        run.repo_url = origin.path.clone();
        assert_eq!(
            o.dispatch_review(run.clone()),
            ReviewDispatchOutcome::Dispatched
        );
        let provisioned = ws
            .ensure_review_worktree(&run.repo_url, "rhapsody", &run.key(), 12, &head)
            .await
            .expect("provision review worktree");
        assert!(std::fs::metadata(&provisioned.path).is_ok());
        let run_id = o.running[&run.key()].run_id;

        let plan = o.handle_stop_run(run_id);

        assert!(plan.found, "the stop did not find the live review run");
        tokio::time::timeout(std::time::Duration::from_secs(30), o.wg.wait())
            .await
            .expect("teardown task finished");
        assert!(
            std::fs::metadata(&provisioned.path).is_err(),
            "a stopped review leaked its worktree: {}",
            provisioned.path
        );
    }

    /// The other half: stopping a TICKET run must not touch its workspace. `terminate` is shared
    /// with `reconcile_stalled`, which retries the run straight back into that same tree — removing
    /// it there would delete a stalled run's in-progress work.
    #[tokio::test]
    async fn stopping_a_ticket_run_leaves_its_workspace_alone() {
        let root = TempDir::new();
        let ws = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: rhapsody_workspace::HookScripts::default(),
                hook_timeout: std::time::Duration::from_secs(30),
            })
            .expect("workspace manager"),
        );
        let (mut o, _d) = orch_with_review(true);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].workspace = Arc::clone(&ws);
        }
        let signal = crate::control_loop::CancelSignal::new();
        o.set_ctx(signal.wait());
        let legacy = ws
            .create_for_issue("rhapsody", "STUDIO-1")
            .await
            .expect("legacy workspace");

        let mut re = RunningEntry::empty(rhapsody_core::Issue {
            id: "1".into(),
            identifier: "STUDIO-1".into(),
            title: "work".into(),
            state: "In Progress".into(),
            ..Default::default()
        });
        re.started_at = chrono::Utc::now();
        re.project_slug = "rhapsody".to_string();
        re.run_id = 77;
        // Armed as a real dispatch arms it: since STUDIO-840 an unarmed entry makes `handle_stop_run`
        // REFUSE, and this test would then assert the absence of a teardown that never got the chance
        // to run — a `found` that means "cannot be stopped", not "was stopped".
        re.cancel = crate::control_loop::CancelSignal::new();
        o.running.insert("1".to_string(), re);

        let plan = o.handle_stop_run(77);
        assert!(plan.found, "the stop did not find the live ticket run");
        assert!(
            !plan.kill_undeliverable,
            "this test must exercise the real stop, not the refusal path"
        );

        tokio::time::timeout(std::time::Duration::from_secs(30), o.wg.wait())
            .await
            .expect("no teardown task should be outstanding");
        assert!(
            std::fs::metadata(&legacy.path).is_ok(),
            "a stopped ticket run's workspace was torn down as if it were a review"
        );
    }

    /// The other half of the routing: a TICKET run's exit must not reach the review teardown. Its
    /// worktree belongs to `reconcile`, and removing it at exit would delete a continuation's
    /// in-progress work between segments.
    #[tokio::test]
    async fn a_ticket_run_exit_does_not_tear_down_a_worktree() {
        let root = TempDir::new();
        let ws = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: rhapsody_workspace::HookScripts::default(),
                hook_timeout: std::time::Duration::from_secs(30),
            })
            .expect("workspace manager"),
        );
        let (mut o, _d) = orch_with_review(true);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].workspace = Arc::clone(&ws);
        }
        // A legacy (non-repo) workspace directory, which `remove_worktree` WOULD delete if reached.
        let legacy = ws
            .create_for_issue("rhapsody", "STUDIO-1")
            .await
            .expect("legacy workspace");

        let started_at = chrono::Utc::now();
        let mut re = RunningEntry::empty(rhapsody_core::Issue {
            id: "1".into(),
            identifier: "STUDIO-1".into(),
            title: "work".into(),
            state: "In Progress".into(),
            ..Default::default()
        });
        re.started_at = started_at;
        re.project_slug = "rhapsody".to_string();
        o.running.insert("1".to_string(), re);
        let signal = crate::control_loop::CancelSignal::new();
        o.set_ctx(signal.wait());

        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: "1".to_string(),
            failed: false,
            started_at,
            err_msg: String::new(),
            last_state: "In Progress".into(),
            declared_handoff: false,
            refused: false,
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), o.wg.wait())
            .await
            .expect("no teardown task should be outstanding");
        assert!(
            std::fs::metadata(&legacy.path).is_ok(),
            "a ticket run's workspace was torn down as if it were a review"
        );
    }

    /// **STUDIO-868's review criterion.** A review run wears its REVIEWER's model, not the model of
    /// the run it is reviewing and not the installation-wide one. That is half the point of the
    /// feature — a reviewer on a strong model reading a cheap model's work — and reviews reach the
    /// worker by a different dispatch path than a ticket, which is where a per-identity property is
    /// most likely to be dropped.
    #[test]
    fn a_review_run_wears_its_reviewers_profile_model() {
        let dir = crate::testsupport::TempDir::new();
        let profiles = std::path::PathBuf::from(dir.child("profiles"));
        std::fs::create_dir_all(&profiles).expect("create profiles dir");
        std::fs::write(
            profiles.join("picky.md"),
            "---\nextends: reviewer\nmodel: strong-model\neffort: xhigh\n---\nBe picky.\n",
        )
        .expect("write profile");

        let (mut o, _dispatched) = orch_with_review(true);
        o.teams_profiles_dir = Some(profiles);
        // The roster entry the synthetic issue's `rhapsody:@alice` label routes to.
        if let Some(t) = o.teams.as_mut() {
            t.roster[0].profile = "picky".to_string();
        }

        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_key("makewhatis", "rhapsody", 12, "alice");
        let re = &o.running[&id];
        assert_eq!(re.identity, "alice", "the reviewer's identity is routed");
        assert_eq!(
            re.model_override,
            rhapsody_agent::ModelOverride {
                identity: "alice".to_string(),
                model: "strong-model".to_string(),
                effort: "xhigh".to_string(),
            },
            "the review must be dispatched on the REVIEWER's model"
        );
        assert_eq!(
            re.model, "strong-model",
            "and the run's model label must name it, not the project's"
        );
    }

    /// The inherit half of the same criterion: a reviewer whose profile names nothing dispatches
    /// with no override, so a review on an installation with no profiles is unchanged.
    #[test]
    fn a_reviewer_with_no_profile_model_dispatches_inheriting() {
        let (mut o, _dispatched) = orch_with_review(true);
        assert_eq!(
            o.dispatch_review(review_run("alice", HEAD_A)),
            ReviewDispatchOutcome::Dispatched
        );
        let id = review_key("makewhatis", "rhapsody", 12, "alice");
        assert!(
            o.running[&id].model_override.is_empty(),
            "{:?}",
            o.running[&id].model_override
        );
    }
}
