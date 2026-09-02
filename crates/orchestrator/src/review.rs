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
}

impl ReviewRun {
    /// The checkout coordinates handed to the worker.
    pub(crate) fn checkout(&self) -> ReviewCheckout {
        ReviewCheckout {
            pr_number: self.number,
            head_sha: self.head_sha.clone(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDispatchOutcome {
    /// A worker was dispatched for this (PR, reviewer).
    Dispatched,
    /// Teams is off, so the whole subsystem is dormant (design §16).
    TeamsOff,
    /// A run for this exact (PR, reviewer) is already running or claimed — THE overwrite guard
    /// (design §14.1 F-DUP). Nothing was touched.
    AlreadyInFlight,
    /// The coordinates cannot produce a review run; the payload names which.
    Refused(&'static str),
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
    pub fn dispatch_review(&mut self, run: ReviewRun) -> ReviewDispatchOutcome {
        // §16: gated on teams.enabled, structurally, before anything is observed or written.
        if !self.teams.as_ref().is_some_and(|t| t.enabled) {
            return ReviewDispatchOutcome::TeamsOff;
        }
        if run.owner.is_empty() || run.repo.is_empty() {
            return ReviewDispatchOutcome::Refused("pull request has no owner/repo");
        }
        if run.number <= 0 {
            return ReviewDispatchOutcome::Refused("pull-request number is not positive");
        }
        if run.reviewer.is_empty() {
            return ReviewDispatchOutcome::Refused("no reviewer");
        }
        if run.head_sha.is_empty() {
            return ReviewDispatchOutcome::Refused("no pinned head SHA");
        }
        // A repo no configured project owns has no workspace, no agent and no prompt to run with —
        // and refusing it also keeps a review confined to repositories this daemon is configured
        // for, which is the trusted-origin property (design §14.1 F-SEC) restated at the dispatch.
        let Some(route) = self.review_route(&run.repo_url) else {
            return ReviewDispatchOutcome::Refused("no configured project owns the PR's repo");
        };
        let id = run.key();
        // THE overwrite guard (F-DUP).
        if self.running.contains_key(&id) || self.claimed.contains(&id) {
            return ReviewDispatchOutcome::AlreadyInFlight;
        }

        // Record the head this run was dispatched against BEFORE the dispatch. Without it the
        // watcher's re-review condition is level-triggered and stays true on every tick between
        // introduction and first completion, which is what produced the duplicate dispatch the guard
        // above refuses. The row is upserted first because `mark_review_requested` is an UPDATE:
        // dispatching a (PR, reviewer) the watch set has never seen would otherwise silently record
        // no `requested_sha` at all. `save_review_watch` preserves both SHAs on a row that already
        // exists, so re-arming an existing row cannot forget what was dispatched or reviewed.
        let watch_key = run.watch_key();
        if let Err(e) = self.store().save_review_watch(ReviewWatchRow {
            key: watch_key.clone(),
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

        let iss = run.synthetic_issue();
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
            .find(|p| !p.disabled && p.repo == repo_url)?;
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
        if e.failed {
            // The watch row is deliberately left exactly where the dispatch put it (`in_flight` at
            // its `requested_sha`): nobody read this head, so recording it as reviewed would be the
            // F-SHA lost update by another route, and clearing the in-flight marker of a crashed
            // review is the watcher's own recovery (design §14.1 F-DUP, "clear on crash").
            let reason = if e.err_msg.is_empty() {
                "worker failed"
            } else {
                &e.err_msg
            };
            self.persist_end_run(re, store::OUTCOME_FAILED, reason);
            self.persist_totals();
            return;
        }
        self.record_review_completed(run, REVIEW_STATUS_REVIEWED);
        self.persist_end_run(re, store::OUTCOME_COMPLETED, "");
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

    use rhapsody_config::teams::{Identity, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_APPROVED, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REVIEWED, Sqlite, StorePath,
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
            team_id: "team-1".to_string(),
            repo_url: REPO_URL.to_string(),
            head_sha: head.to_string(),
            introduced_by: "handoff".to_string(),
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
        let id = run.key();
        let re = o.running.get(&id).expect("the review is running");
        let (started_at, run_id) = (re.started_at, re.run_id);
        o.on_worker_exit(crate::EvWorkerExit {
            issue_id: id,
            failed,
            started_at,
            err_msg: err_msg.to_string(),
            // A synthetic `pr:` issue has no state, so BOTH of the classifier's samples are empty —
            // the exact input that made every clean review exit an OUTCOME_CONTINUED.
            last_state: String::new(),
            declared_handoff: true,
        });
        run_id
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
        o.running.insert("1".to_string(), re);

        assert!(o.handle_stop_run(77).found);

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
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), o.wg.wait())
            .await
            .expect("no teardown task should be outstanding");
        assert!(
            std::fs::metadata(&legacy.path).is_ok(),
            "a ticket run's workspace was torn down as if it were a review"
        );
    }
}
