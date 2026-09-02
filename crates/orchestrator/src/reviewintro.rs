//! reviewintro — how a pull request ENTERS the ticketless review watch set, and how the daemon's
//! own re-review signal reaches the control task (STUDIO-720, slice 6 of the design record
//! `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §14.4).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature; this is the additive
//! Rhapsody surface the design record specifies, gated end to end on `teams.enabled` (§16).
//!
//! # This is the security slice
//!
//! §14.1's fatal finding **F-SEC** is not about what a review agent does — it is about how a pull
//! request becomes one. A review run checks out a head SHA and reads its diff under
//! `bypassPermissions`, so whoever decides WHICH pull request that is, decides what code the daemon
//! executes. The room is not entitled to that decision: any loopback caller — including a review
//! agent itself — can append a post reading `from: operator … review github.com/attacker/evil/pull/1`,
//! and the fork guard in [`crate::ghsummons`] does not help, because the attacker names their own
//! repository as the base.
//!
//! So a pull-request coordinate is trusted because of WHERE IT CAME FROM, never because of what a
//! post says about it. Exactly two origins may introduce one (§15-a, §15-e):
//!
//! 1. **A teammate's handoff**, using the run's OWN resolved repository binding — `project_repo`,
//!    falling back to the top-level `repo` — which is the same trusted value the quorum's
//!    `plan_quorum` has always used and which no room text can reach. `plan_review_intro` builds it
//!    at `handle_handoff_run`, the site the quorum already hooks.
//! 2. **An operator through the authenticated console** (slice 8), which arrives at the same
//!    loop-side introduction handler through [`ControlHandle::introduce_review`].
//!
//! And the daemon's own "the author pushed fixes, review again" signal is an **in-process control
//! [`Event`]** ([`ControlHandle::review_head_advanced`]) rather than a room post, so the re-review
//! loop has no forgeable leg either. No `pr:` intent was added to the Linear-anchored room reader,
//! and [`crate::teamsears`] has a standing test that its intent space stays closed.
//!
//! # The allowlist is config, and it is checked twice
//!
//! "Resolvable from config" IS the watched-repo allowlist (§15-a): the repositories of the enabled
//! projects, plus the legacy single-project form's top-level `repo`. Nothing else can be introduced,
//! which mirrors how `find_issue` refuses a ticket key that is not on a project this team works.
//!
//! It is enforced at the PLAN (so a handoff on an unconfigured remote never leaves the loop) and
//! again at the loop-side introduction handler (so a future caller — the console, a test, a slice
//! that has not been written yet — cannot skip it). The second check is the load-bearing one: the
//! handler is the only place in the daemon that writes an introduction into the watch set.
//!
//! # Off the loop, then back onto it
//!
//! Resolving "does this branch have an open pull request, and which number is it" is a `gh` call,
//! and [`crate::ghsummons::GH`] shells out through a synchronous `std::process::Command` — so it
//! must not happen on the control task. [`run_review_intro_task`] owns it, holds no `Orchestrator`
//! and takes no lock the control task takes, exactly as [`crate::quorum`]'s task does. The
//! containment is structural rather than temporal: a `tokio::time::timeout` around that call could
//! never fire (the future has no await point and completes in its first poll), so a bound here
//! would read stronger than it is. A hung `gh` parks THIS task, which owns all of introduction's
//! network I/O, and the daemon keeps ticking.
//!
//! What comes back is a resolved [`IntroducedPr`] handed to the control task as an [`Event`], where
//! the watch-set write happens beside every other one. That keeps the watch set single-writer, the
//! same property [`Orchestrator::dispatch_review`] relies on for its in-flight guard.
//!
//! # What this slice deliberately does not do
//!
//! * **Nothing is dispatched.** Introduction writes a `requested` row and stops; the edge-triggered
//!   watcher that turns one into a review run is slice 5. Nothing sends
//!   [`Event::ReviewHeadAdvanced`] in production yet for the same reason — this slice builds the
//!   in-process channel the design requires INSTEAD of a room post, and slice 5's watcher is its
//!   first sender.
//! * **Reviewer load is the quorum's, and under ticketless it is empty.** `quorum_load` is filled
//!   by `record_quorum_state`, which short-circuits when the ticket fan-out is off — which
//!   `ticketless` makes it. `select_reviewers` therefore ranks an all-zero load and picks
//!   deterministically by roster order (author excluded), which is correct but not load-aware.
//!   Making load count ticketless reviews is slice 5's named item ("review load counting"), and it
//!   changes nothing observable until slice 5 dispatches from these rows.

use std::sync::Arc;

use async_trait::async_trait;

use rhapsody_store::{
    REVIEW_STATUS_DROPPED, REVIEW_STATUS_REQUESTED, ReviewWatchKey, ReviewWatchRow,
};
use rhapsody_workspace::sanitize_key;

use crate::control_loop::{CancelWait, Event};
use crate::ghsummons::OpenPrSource;
use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::quorum::select_reviewers;
use crate::review::review_key;
use crate::stop::ControlHandle;

/// The origin recorded on a watch-set row a teammate's own handoff introduced, as
/// `handoff:<identifier>`. Recorded rather than inferred (§14.1 F-SEC): "how did this pull request
/// get here" must be readable off the row, not reconstructed later from what else happens to be
/// true.
pub const REVIEW_ORIGIN_HANDOFF: &str = "handoff";

/// The origin recorded on a row an operator introduced through the authenticated console
/// (slice 8). Defined here, beside its sibling, so the two spellings cannot drift apart; nothing
/// writes it until the console surface exists.
pub const REVIEW_ORIGIN_CONSOLE: &str = "console";

/// How many reviewers ONE introduction picks — design decision C, "default 1 reviewer, configurable
/// up" (§13.5, §15-f).
///
/// The configurable half is deliberately absent: `teams.review` carries exactly `mode` today
/// (STUDIO-719), and adding a count key is a config change that belongs with the slice that reads
/// it. The data model is already per-(PR, reviewer) — [`ReviewWatchKey`] carries the reviewer — so
/// raising this later widens the fan without reshaping anything.
pub const DEFAULT_REVIEWERS: usize = 1;

/// One trusted introduction as the CONTROL TASK decided it: which repository, which branch to look
/// for an open pull request on, and who would review it.
///
/// Everything the decision needs is already in memory when this is built — the repository binding
/// comes off the run, the branch name is the frozen `symphony/<key>` contract, the reviewers come
/// from the roster — so the planner touches no network, exactly as `plan_quorum` does not. What is
/// missing is only the pull request's NUMBER, which is the one thing GitHub has to be asked for;
/// that is the off-loop task's whole job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewIntroRequest {
    /// The repository owner, parsed from [`repo_url`](Self::repo_url).
    pub owner: String,
    /// The repository name, parsed from [`repo_url`](Self::repo_url).
    pub repo: String,
    /// The run's OWN resolved repository binding — the trusted origin itself. Carried through so
    /// the loop-side handler re-derives the allowlist decision from the same value.
    pub repo_url: String,
    /// `symphony/<sanitized identifier>`, the branch the run's worktree pushed. A frozen
    /// cross-process contract, which is why the control task can name it without asking anybody.
    pub head_branch: String,
    /// The teammates who would review it, author already excluded.
    pub reviewers: Vec<String>,
    /// The teammate who authored the pull request — the run's own identity. Carried onto the watch
    /// row so the watcher's reviewer substitution can keep excluding them long after this run has
    /// ended (STUDIO-721).
    pub author: String,
    /// The origin tag written onto the watch-set row.
    pub introduced_by: String,
}

/// A pull request RESOLVED to the coordinate the watch set is keyed by, on its way back to the
/// control task.
///
/// This is what an [`Event::ReviewIntroduce`] carries, and the console (slice 8) will build one
/// directly. It is deliberately NOT trusted on arrival: the loop-side `handle_review_introduce`
/// re-checks every field, including the allowlist, because being an in-process type is not the same
/// as being a validated one.
///
/// One field it cannot re-check is `open`: the row is written open, on the CALLER's fresh
/// observation. The handoff path earns that — it reaches here only because
/// [`OpenPrSource::open_pr_for_branch`] answered under `--state open` — and any future caller owes
/// the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntroducedPr {
    /// Owner, repository and NUMBER — the coordinate `gh` and the watch set both key on.
    pub pr: PrCoord,
    /// The repository binding the coordinate came from, re-checked against the configured
    /// allowlist by the handler.
    pub repo_url: String,
    /// One watch-set row is written per reviewer.
    pub reviewers: Vec<String>,
    /// The pull request's author, recorded on every row this introduction writes (STUDIO-721).
    pub author: String,
    /// The origin tag, e.g. `handoff:STUDIO-720`.
    pub introduced_by: String,
}

/// What one introduction attempt did. Returned rather than logged-and-swallowed so the off-loop
/// task can say which of "it worked", "the subsystem is off" and "these coordinates will never
/// work" happened — three situations that look identical in a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewIntroOutcome {
    /// `n` (PR, reviewer) rows entered or re-armed the watch set. `0` means every candidate row was
    /// skipped because a review of it is already in flight.
    Introduced(usize),
    /// Teams is off or the mode is not `ticketless`, so the subsystem is dormant (§16). Nothing was
    /// read and nothing was written.
    Dormant,
    /// The coordinates were refused; the payload names why.
    Refused(&'static str),
}

/// Delivers a resolved introduction from the off-loop task to the control task.
///
/// A trait for the reason [`crate::teamsears::RoomRelay`] is one: the task must be testable without
/// a control loop, and the seam is what lets a test assert on the coordinates that were handed over
/// rather than on a side effect two hops away. [`ControlIntroSink`] is the production
/// implementation and the only one that reaches the daemon.
#[async_trait]
pub trait ReviewIntroSink: Send + Sync {
    async fn introduce(&self, pr: IntroducedPr) -> ReviewIntroOutcome;
}

/// The production [`ReviewIntroSink`]: the control channel, through the same [`ControlHandle`] seam
/// every other off-loop→loop hand-back uses.
pub struct ControlIntroSink {
    control: ControlHandle,
}

impl ControlIntroSink {
    pub fn new(control: ControlHandle) -> ControlIntroSink {
        ControlIntroSink { control }
    }
}

#[async_trait]
impl ReviewIntroSink for ControlIntroSink {
    async fn introduce(&self, pr: IntroducedPr) -> ReviewIntroOutcome {
        self.control.introduce_review(pr).await
    }
}

/// Everything [`run_review_intro_task`] runs against. No `Orchestrator`, no store and no control
/// channel of its own — the off-loop guarantee, in the type, as [`crate::quorum::QuorumDeps`] states
/// it.
pub struct ReviewIntroDeps {
    /// Resolves the open pull request on a head branch. `None` disables introduction entirely: a
    /// daemon that cannot ask GitHub has no way to learn a pull-request number, and guessing one
    /// is precisely the thing this module exists to refuse.
    pub pr_source: Option<Arc<dyn OpenPrSource>>,
    /// Where a resolved introduction is handed back to the control task.
    pub sink: Arc<dyn ReviewIntroSink>,
}

/// Consumes [`ReviewIntroRequest`]s until `ctx` is cancelled or every sender is dropped.
///
/// One at a time, serially and with no `spawn`, for [`crate::quorum::run_quorum_task`]'s reason: the
/// `gh` call blocks, so concurrency here would occupy several runtime workers and multiply the
/// rate-limit pressure of a subsystem nobody is waiting on.
///
/// There is no back-off, deliberately, and the difference from the quorum is real rather than an
/// omission: the quorum retries nothing but pays for a Linear outage with a burst of failing
/// writes, whereas this task performs exactly one read per handoff and writes nothing itself. A
/// failed lookup costs one introduction, and the next handoff of the same run — the only thing that
/// should ever ask again — asks again.
pub async fn run_review_intro_task(
    mut ctx: CancelWait,
    deps: ReviewIntroDeps,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ReviewIntroRequest>,
) {
    tracing::info!(
        "ticketless review introduction task started (off-loop; a handoff is never blocked on it)"
    );
    loop {
        let req = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                None => return,
            },
        };
        let Some(src) = deps.pr_source.as_ref() else {
            tracing::debug!(
                repo = %req.repo_url,
                "ticketless review: no GitHub source, so no pull request can be resolved; nothing \
                 is introduced"
            );
            continue;
        };
        // `--state open` and the fork guard both live inside this call, so what comes back is
        // already an OPEN pull request whose head repository belongs to the owner asked about.
        let url = match src
            .open_pr_for_branch(&req.owner, &req.repo, &req.head_branch)
            .await
        {
            Ok(Some(url)) => url,
            Ok(None) => {
                tracing::debug!(
                    owner = %req.owner, repo = %req.repo, branch = %req.head_branch,
                    "ticketless review: no open pull request on the run's branch; nothing to review yet"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    owner = %req.owner, repo = %req.repo, branch = %req.head_branch, err = %e,
                    "ticketless review: the open-pull-request lookup failed; nothing is introduced"
                );
                continue;
            }
        };
        let Some(pr) = pr_from_url(&url, &req.owner, &req.repo) else {
            tracing::warn!(
                url = %url, owner = %req.owner, repo = %req.repo,
                "ticketless review: the resolved pull-request URL does not name the repository that \
                 was asked about; nothing is introduced"
            );
            continue;
        };
        let outcome = deps
            .sink
            .introduce(IntroducedPr {
                pr: pr.clone(),
                repo_url: req.repo_url.clone(),
                reviewers: req.reviewers.clone(),
                author: req.author.clone(),
                introduced_by: req.introduced_by.clone(),
            })
            .await;
        match outcome {
            // `Introduced(0)` is not a failure and not an introduction: every candidate row had a
            // review already in flight, so the watch set already says what needs saying.
            ReviewIntroOutcome::Introduced(0) => tracing::debug!(
                pr = %pr,
                "ticketless review: every reviewer of this pull request is already mid-review; its \
                 watch rows were left as they are"
            ),
            ReviewIntroOutcome::Introduced(n) => tracing::info!(
                pr = %pr, rows = n, origin = %req.introduced_by,
                "ticketless review: pull request introduced into the watch set"
            ),
            ReviewIntroOutcome::Dormant => tracing::debug!(
                pr = %pr,
                "ticketless review: the subsystem is off, so nothing was introduced"
            ),
            ReviewIntroOutcome::Refused(why) => tracing::warn!(
                pr = %pr, reason = why,
                "ticketless review: the introduction was refused"
            ),
        }
    }
}

/// The pull request a resolved URL names, but ONLY when it is in the repository that was asked
/// about.
///
/// The owner/repo check is not belt-and-braces: [`OpenPrSource::open_pr_for_branch`] guards the
/// HEAD repository, and a URL that disagreed with the base would mean the answer is about some
/// other repository than the one the trusted binding named — which is the coordinate this module
/// refuses to take on trust. Case-insensitive, because GitHub logins and repository names are.
fn pr_from_url(url: &str, owner: &str, repo: &str) -> Option<PrCoord> {
    crate::teamsears::extract_pr_urls(url)
        .into_iter()
        .find(|p| p.owner.eq_ignore_ascii_case(owner) && p.repo.eq_ignore_ascii_case(repo))
        .map(|p| PrCoord {
            owner: p.owner,
            repo: p.repo,
            number: p.number,
        })
}

impl Orchestrator {
    /// Opens the introduction task's channel, storing the sender and handing back the receiver for
    /// [`run_review_intro_task`].
    ///
    /// A method rather than a public field for [`open_quorum_channel`](Orchestrator::open_quorum_channel)'s
    /// reason, sharpened: injecting a [`ReviewIntroRequest`] is naming the repository a
    /// `bypassPermissions` agent will check out. A daemon that never calls this has
    /// `review_intro_tx: None`, which makes introduction from a handoff unrepresentable rather than
    /// merely skipped. Call it BEFORE [`control`](Orchestrator::control), and only when the
    /// ticketless path is on.
    pub fn open_review_intro_channel(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ReviewIntroRequest> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.review_intro_tx = Some(tx);
        rx
    }

    /// Plans a trusted introduction for a handoff, or `None` when this handoff introduces nothing.
    ///
    /// Runs ON the control task at `handle_handoff_run` — the site `plan_quorum` hooks, and for the
    /// same reason (§0.12): the handoff is the moment the daemon EXECUTES rather than infers that a
    /// pull request is ready to be read. Every gate is a comparison over data already in memory.
    ///
    /// The gates:
    ///
    /// 1. Teams is on AND `review.mode: ticketless` (§16).
    /// 2. The run was dispatched AS a roster identity — there is otherwise no author to exclude and
    ///    no teammate to ask.
    /// 3. The run's own repository binding parses to an `owner/repo`.
    /// 4. **That repository is one this daemon is configured for** — the allowlist, and the F-SEC
    ///    anchor restated where the coordinate is born.
    /// 5. The roster holds somebody other than the author.
    ///
    /// A ticket's team id is deliberately NOT a gate, unlike `plan_quorum`'s: that gate exists
    /// because `create_issue` needs a team to create in, and a ticketless review creates nothing.
    pub(crate) fn plan_review_intro(
        &self,
        re: &crate::orchestrator::RunningEntry,
    ) -> Option<ReviewIntroRequest> {
        if !self.review_ticketless_enabled() || re.identity.is_empty() {
            return None;
        }
        // THE trusted origin: the remote this run's worktree pushed to, with `bind_teams_run`'s
        // fallback and for its reason — a legacy single-project config never populates
        // `project_repo` and carries the repo top-level instead. Neither value can be reached from
        // room text, which is the whole security property (§14.1 F-SEC, §16).
        let repo_url = if re.project_repo.is_empty() {
            self.eff
                .as_ref()
                .map(|eff| eff.cfg.repo.clone())
                .unwrap_or_default()
        } else {
            re.project_repo.clone()
        };
        let Some((owner, repo)) = crate::ghsummons::parse_repo(&repo_url) else {
            tracing::debug!(
                issue = %re.issue.identifier,
                repo = %repo_url,
                "ticketless review: the run's repository binding names no GitHub owner/repo, so \
                 there is no pull request to introduce"
            );
            return None;
        };
        if !self.review_repo_is_configured(&owner, &repo) {
            tracing::warn!(
                issue = %re.issue.identifier,
                owner = %owner, repo = %repo,
                "ticketless review: refusing to introduce a pull request in a repository no \
                 configured project owns"
            );
            return None;
        }
        let teams = self.teams.as_ref()?;
        let mut reviewers = select_reviewers(teams, &re.identity, &self.quorum_load);
        reviewers.truncate(DEFAULT_REVIEWERS);
        if reviewers.is_empty() {
            tracing::warn!(
                issue = %re.issue.identifier,
                author = %re.identity,
                "ticketless review: the roster holds nobody but the author, so no review was \
                 requested; add a teammate to `teams.yaml`"
            );
            return None;
        }
        Some(ReviewIntroRequest {
            owner,
            repo,
            repo_url,
            // The frozen `symphony/<key>` branch-naming contract the worktree was created on
            // (`rhapsody_workspace::Manager::ensure_*`), which is what makes this decision
            // network-free.
            head_branch: format!("symphony/{}", sanitize_key(&re.issue.identifier)),
            reviewers,
            author: re.identity.clone(),
            introduced_by: format!("{REVIEW_ORIGIN_HANDOFF}:{}", re.issue.identifier),
        })
    }

    /// Writes a trusted introduction into the watch set. **The only introduction site in the
    /// daemon**, and therefore where the F-SEC guarantee is enforced rather than assumed.
    ///
    /// Runs ON the control task (`evReviewIntroduce`), which is what keeps the watch set
    /// single-writer alongside [`dispatch_review`](Orchestrator::dispatch_review) and lets the
    /// in-flight check below read `running`/`claimed` without a race.
    ///
    /// Every field is re-validated even though the caller is in-process. Being an
    /// [`IntroducedPr`] means it was CONSTRUCTED by trusted code, not that its contents were
    /// checked — and the callers are about to multiply (the console in slice 8, the watcher in
    /// slice 5). The allowlist check in particular is duplicated from
    /// [`plan_review_intro`](Orchestrator::plan_review_intro) on purpose: a guard that lives only
    /// at the far end of a channel is a guard the next sender can forget.
    pub(crate) fn handle_review_introduce(&mut self, pr: &IntroducedPr) -> ReviewIntroOutcome {
        if !self.review_ticketless_enabled() {
            return ReviewIntroOutcome::Dormant;
        }
        if pr.pr.owner.is_empty() || pr.pr.repo.is_empty() {
            return ReviewIntroOutcome::Refused("pull request has no owner/repo");
        }
        if pr.pr.number <= 0 {
            return ReviewIntroOutcome::Refused("pull-request number is not positive");
        }
        if pr.reviewers.iter().all(|r| r.trim().is_empty()) {
            return ReviewIntroOutcome::Refused("no reviewer");
        }
        if !self.review_repo_is_configured(&pr.pr.owner, &pr.pr.repo) {
            return ReviewIntroOutcome::Refused("no configured project owns the PR's repo");
        }
        let mut written = 0usize;
        for reviewer in pr.reviewers.iter().filter(|r| !r.trim().is_empty()) {
            let id = review_key(&pr.pr.owner, &pr.pr.repo, pr.pr.number, reviewer);
            // A review of this exact (PR, reviewer) is live. Re-arming its row to `requested` would
            // overwrite the `in_flight` marker the F-DUP edge-trigger reads, so the watcher would
            // dispatch a second agent onto the first one's detached worktree. The row is already
            // saying what needs saying; leave it alone.
            if self.running.contains_key(&id) || self.claimed.contains(&id) {
                tracing::debug!(
                    review = %id,
                    "ticketless review: a review of this pull request is already in flight; its \
                     watch row is left as it is"
                );
                continue;
            }
            let row = ReviewWatchRow {
                author: pr.author.clone(),
                key: ReviewWatchKey {
                    owner: pr.pr.owner.clone(),
                    repo: pr.pr.repo.clone(),
                    number: pr.pr.number,
                    reviewer: reviewer.clone(),
                },
                introduced_by: pr.introduced_by.clone(),
                // Both empty, and only meaningful on a row this call CREATES: `save_review_watch`
                // preserves an existing row's two SHAs, so re-introducing a pull request cannot
                // forget which head was dispatched or reviewed (§14.1 F-DUP, F-SHA).
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.to_string(),
                open: true,
            };
            match self.store().save_review_watch(row) {
                Ok(()) => written += 1,
                Err(e) => {
                    tracing::warn!(review = %id, err = %e, "ticketless review: the watch-set write failed")
                }
            }
        }
        ReviewIntroOutcome::Introduced(written)
    }

    /// Re-arms the watch rows of a pull request whose head has ADVANCED — the daemon's own
    /// re-review signal, delivered as an in-process [`Event`] rather than a room post (§14.1 F-SEC's
    /// fix, §15-e). Returns how many rows were re-armed.
    ///
    /// **It can only ever re-arm a row that is already watched.** An advance reported for a pull
    /// request nobody introduced writes nothing at all — no row, no dispatch — which is what keeps
    /// the re-review loop from becoming a second, weaker introduction path. That is why this reads
    /// the watch set and updates matching rows instead of upserting the coordinate it was handed.
    ///
    /// Nothing sends this in production yet: the head-advance observation is the edge-triggered
    /// watcher's, which is slice 5. What this slice fixes is the CHANNEL — the design forbids that
    /// signal being a room post, and this is the shape it takes instead.
    pub(crate) fn handle_review_head_advanced(&mut self, pr: &PrCoord, head_sha: &str) -> usize {
        if !self.review_ticketless_enabled() || head_sha.is_empty() {
            return 0;
        }
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(pr = %pr, err = %e, "ticketless review: the watch set could not be read; no re-review was armed");
                return 0;
            }
        };
        let mut armed = 0usize;
        for row in rows {
            if !row.key.owner.eq_ignore_ascii_case(&pr.owner)
                || !row.key.repo.eq_ignore_ascii_case(&pr.repo)
                || row.key.number != pr.number
            {
                continue;
            }
            // A closed/merged/gone pull request left the watch set for good; a head advance on one
            // is a stale observation, not a reason to start watching it again.
            if !row.open || row.status == REVIEW_STATUS_DROPPED {
                continue;
            }
            // Already handled at this exact head: reviewed at it, or dispatched against it. Both
            // are the edge-trigger's own record, and re-arming either is the level-triggered
            // duplicate dispatch §14.1 F-DUP describes.
            if head_sha == row.last_reviewed_sha || head_sha == row.requested_sha {
                continue;
            }
            let id = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            if self.running.contains_key(&id) || self.claimed.contains(&id) {
                continue;
            }
            let armed_row = ReviewWatchRow {
                status: REVIEW_STATUS_REQUESTED.to_string(),
                open: true,
                ..row
            };
            match self.store().save_review_watch(armed_row) {
                Ok(()) => armed += 1,
                Err(e) => {
                    tracing::warn!(review = %id, err = %e, "ticketless review: re-arming the watch row failed")
                }
            }
        }
        armed
    }

    /// Whether `owner/repo` is a repository this daemon is configured for — the watched-repo
    /// allowlist (§15-a), and the reason a pull-request coordinate is never taken on trust.
    ///
    /// Every ENABLED project's `repo`, plus the top-level `repo` that the legacy single-project
    /// config form carries instead of a `projects:` block. Comparison is on the parsed
    /// `owner/repo` rather than on the URL text, so `git@github.com:o/r.git` and
    /// `https://github.com/o/r` are the same repository — which they are — and case-insensitively,
    /// because GitHub logins and repository names are.
    fn review_repo_is_configured(&self, owner: &str, repo: &str) -> bool {
        let Some(eff) = self.eff.as_ref() else {
            return false;
        };
        let matches = |url: &str| {
            crate::ghsummons::parse_repo(url)
                .is_some_and(|(o, r)| o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo))
        };
        eff.projects.iter().any(|p| !p.disabled && matches(&p.repo)) || matches(&eff.cfg.repo)
    }
}

impl ControlHandle {
    /// Hands a planned introduction to the off-loop task. A no-op when the handoff introduces
    /// nothing, when no task is running, or when that task has already stopped — none of which is
    /// worth failing the handoff over, exactly as a missed quorum fan-out is not: the ticket has
    /// moved and the run is winding down either way.
    pub(crate) fn request_review_intro(&self, req: Option<ReviewIntroRequest>) {
        let (Some(req), Some(tx)) = (req, self.review_intro.as_ref()) else {
            return;
        };
        let origin = req.introduced_by.clone();
        if tx.send(req).is_err() {
            tracing::warn!(
                origin = %origin,
                "handoff: the ticketless review introduction task is gone; no review was requested"
            );
        }
    }

    /// Introduces a RESOLVED pull request into the watch set through the control task — the
    /// trusted-origin entry point, shared by the off-loop handoff task and (slice 8) the
    /// authenticated console.
    ///
    /// The wait is bounded by the daemon lifetime rather than by a timer, as
    /// [`handoff_run`](ControlHandle::handoff_run)'s is: nothing here is answering an agent's MCP
    /// call, so a busy tick should delay the introduction rather than turn it into a false failure.
    ///
    /// A gone or cancelled control task answers `Refused`, NOT `Dormant`: "the daemon is shutting
    /// down" and "this installation has the subsystem off" are different facts, and the second is
    /// the one an operator reads as "working as configured".
    pub async fn introduce_review(&self, pr: IntroducedPr) -> ReviewIntroOutcome {
        const GONE: ReviewIntroOutcome = ReviewIntroOutcome::Refused("the control task is gone");
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewIntroduce { pr, reply: tx })
            .is_err()
        {
            return GONE;
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or(GONE),
            _ = lifetime.cancelled() => GONE,
        }
    }

    /// Reports that a WATCHED pull request's head advanced, arming one more review round. Returns
    /// how many (PR, reviewer) rows were re-armed.
    ///
    /// This is the design's in-process control Event standing in for the room post §14.1 F-SEC
    /// rules out. It cannot introduce a pull request — the loop-side `handle_review_head_advanced`
    /// only ever updates rows that already exist — so an observation about an unwatched coordinate
    /// is inert by construction rather than by the caller's care.
    pub async fn review_head_advanced(&self, pr: PrCoord, head_sha: &str) -> usize {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ev = Event::ReviewHeadAdvanced {
            pr,
            head_sha: head_sha.to_string(),
            reply: tx,
        };
        if self.events.send(ev).is_err() {
            return 0; // the loop is gone: there is nothing to re-review into.
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or(0),
            _ = lifetime.cancelled() => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_APPROVED, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REVIEWED, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::ghsummons::OpenPrResult;
    use crate::orchestrator::RunningEntry;
    use crate::testsupport::{empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const OTHER_URL: &str = "https://github.com/attacker/evil.git";
    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn ident(name: &str) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            ..Identity::default()
        }
    }

    fn teams_with(enabled: bool, mode: ReviewMode, names: &[&str]) -> Teams {
        Teams {
            enabled,
            review: Review { mode },
            roster: names.iter().map(|n| ident(n)).collect(),
            ..Teams::disabled()
        }
    }

    /// An orchestrator with one enabled project owning [`REPO_URL`] and an in-memory store — the
    /// configured-repository allowlist, in the only form the daemon has one.
    fn orch(teams: Teams) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo"]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams);
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    /// A teammate's live run, as `handle_handoff_run` would find it: an identity, and the run's own
    /// resolved repository binding.
    fn teammate_run(identity: &str, project_repo: &str) -> RunningEntry {
        let mut re = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-1".to_string(),
            identifier: "STUDIO-720".to_string(),
            team_id: "team-1".to_string(),
            ..Default::default()
        });
        re.identity = identity.to_string();
        re.project_repo = project_repo.to_string();
        re
    }

    fn introduced(owner: &str, repo: &str, number: i64, reviewers: &[&str]) -> IntroducedPr {
        IntroducedPr {
            pr: PrCoord::new(owner, repo, number),
            repo_url: format!("https://github.com/{owner}/{repo}.git"),
            reviewers: reviewers.iter().map(|r| r.to_string()).collect(),
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-720".to_string(),
        }
    }

    fn watch_key(reviewer: &str) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: reviewer.to_string(),
        }
    }

    // ── the plan: the trusted origin ─────────────────────────────────────────────────────────────

    /// The acceptance path: a teammate's handoff under `ticketless` plans an introduction whose
    /// coordinates come from the RUN'S OWN repository binding and the frozen `symphony/<key>` branch
    /// contract — never from anything anybody typed.
    #[test]
    fn a_ticketless_handoff_plans_an_introduction_from_the_runs_own_repo_binding() {
        let o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let plan = o
            .plan_review_intro(&teammate_run("alice", REPO_URL))
            .expect("a ticketless handoff introduces its own pull request");

        assert_eq!(
            (plan.owner.as_str(), plan.repo.as_str()),
            ("makewhatis", "rhapsody")
        );
        assert_eq!(
            plan.repo_url, REPO_URL,
            "the trusted binding is carried, not re-derived"
        );
        assert_eq!(plan.head_branch, "symphony/STUDIO-720");
        assert_eq!(
            plan.reviewers,
            vec!["bob".to_string()],
            "one reviewer by default, and never the author"
        );
        assert_eq!(
            plan.introduced_by, "handoff:STUDIO-720",
            "the origin is recorded, not inferred"
        );
    }

    /// The legacy single-project config form populates no `project_repo`; the top-level `repo` is
    /// the binding there, and it is equally trusted — `plan_quorum` falls back the same way and for
    /// the same reason (STUDIO-674).
    #[test]
    fn a_legacy_single_project_run_introduces_from_the_top_level_repo() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        if let Some(eff) = o.eff.as_mut() {
            eff.projects.clear();
            eff.cfg.repo = "https://github.com/o/legacy".to_string();
        }
        let plan = o
            .plan_review_intro(&teammate_run("alice", ""))
            .expect("the legacy form introduces too");
        assert_eq!((plan.owner.as_str(), plan.repo.as_str()), ("o", "legacy"));
    }

    /// §16, the dormancy acceptance: Teams off, or any mode but `ticketless`, plans nothing at all.
    #[test]
    fn teams_off_or_a_non_ticketless_mode_introduces_nothing() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (false, ReviewMode::Off),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let o = orch(teams_with(enabled, mode, &["alice", "bob"]));
            assert!(
                o.plan_review_intro(&teammate_run("alice", REPO_URL))
                    .is_none(),
                "enabled={enabled} mode={mode:?}"
            );
        }
    }

    /// The other plan-time gates: a run wearing no roster identity has no author to exclude and no
    /// teammate to ask, a binding that names no GitHub repository has no pull request to find, and a
    /// roster holding only the author has nobody to ask.
    #[test]
    fn a_handoff_that_cannot_name_a_reviewable_pull_request_plans_nothing() {
        let o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(
            o.plan_review_intro(&teammate_run("", REPO_URL)).is_none(),
            "no identity ⇒ no introduction"
        );
        assert!(
            o.plan_review_intro(&teammate_run("alice", "")).is_none(),
            "no resolvable repository ⇒ no introduction"
        );

        let solo = orch(teams_with(true, ReviewMode::Ticketless, &["alice"]));
        assert!(
            solo.plan_review_intro(&teammate_run("alice", REPO_URL))
                .is_none(),
            "a roster of one has nobody but the author to ask"
        );
    }

    /// **F-SEC at the plan.** A run bound to a repository no configured project owns introduces
    /// nothing — the allowlist is the configuration, exactly as `find_issue` refuses a key that is
    /// not on a project this team works.
    #[test]
    fn an_off_allowlist_repository_binding_plans_no_introduction() {
        let o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(
            o.plan_review_intro(&teammate_run("alice", OTHER_URL))
                .is_none()
        );
        // A DISABLED project's repository is not on the allowlist either: it is configuration this
        // daemon was told to stop acting on.
        let mut disabled = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        if let Some(eff) = disabled.eff.as_mut() {
            eff.projects[0].disabled = true;
        }
        assert!(
            disabled
                .plan_review_intro(&teammate_run("alice", REPO_URL))
                .is_none()
        );
    }

    /// The two URL spellings of one repository are one repository. The allowlist compares the parsed
    /// `owner/repo`, so an SSH remote in config admits an HTTPS coordinate and the other way round.
    #[test]
    fn the_allowlist_compares_repositories_not_url_text() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(o.review_repo_is_configured("makewhatis", "rhapsody"));
        assert!(
            o.review_repo_is_configured("MakeWhatIs", "Rhapsody"),
            "logins are case-insensitive"
        );
        assert!(!o.review_repo_is_configured("attacker", "evil"));
        if let Some(eff) = o.eff.as_mut() {
            eff.projects[0].repo = "https://github.com/makewhatis/rhapsody".to_string();
        }
        assert!(
            o.review_repo_is_configured("makewhatis", "rhapsody"),
            "the same repository, spelled the other way"
        );
    }

    // ── the loop-side introduction: the only writer ──────────────────────────────────────────────

    /// A resolved introduction becomes exactly one `requested` watch-set row per reviewer, carrying
    /// the origin and no SHA — the dispatch and the completion are what fill those in.
    #[test]
    fn an_introduction_writes_a_requested_row_per_reviewer() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"])),
            ReviewIntroOutcome::Introduced(1)
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("bob watches the PR");
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED);
        assert!(row.open);
        assert_eq!(row.introduced_by, "handoff:STUDIO-720");
        assert!(row.requested_sha.is_empty() && row.last_reviewed_sha.is_empty());
    }

    /// **F-SEC at the only writer.** An in-process event naming a repository no configured project
    /// owns is refused, and the watch set stays empty. This is the check that has to hold even when
    /// the sender is trusted code, because the senders are about to multiply.
    #[test]
    fn an_off_allowlist_introduction_is_refused_and_writes_nothing() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_introduce(&introduced("attacker", "evil", 1, &["bob"])),
            ReviewIntroOutcome::Refused("no configured project owns the PR's repo")
        );
        assert!(
            o.store().load_review_watch().expect("read").is_empty(),
            "no watch-set entry may exist for an off-allowlist repository"
        );
    }

    /// The remaining refusals, each with nothing written: coordinates that cannot name a pull
    /// request, and a request with no reviewer to key a row by.
    #[test]
    fn unusable_coordinates_are_refused_and_write_nothing() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        for (why, pr) in [
            ("no owner", introduced("", "rhapsody", 12, &["bob"])),
            ("no repo", introduced("makewhatis", "", 12, &["bob"])),
            (
                "number 0",
                introduced("makewhatis", "rhapsody", 0, &["bob"]),
            ),
            (
                "negative number",
                introduced("makewhatis", "rhapsody", -3, &["bob"]),
            ),
            ("no reviewer", introduced("makewhatis", "rhapsody", 12, &[])),
            (
                "blank reviewer",
                introduced("makewhatis", "rhapsody", 12, &["  "]),
            ),
        ] {
            assert!(
                matches!(
                    o.handle_review_introduce(&pr),
                    ReviewIntroOutcome::Refused(_)
                ),
                "{why}"
            );
        }
        assert!(o.store().load_review_watch().expect("read").is_empty());
    }

    /// §16 at the writer: with Teams off — or on any mode but `ticketless` — the handler reads
    /// nothing and writes nothing, whatever it is handed.
    #[test]
    fn a_dormant_daemon_introduces_nothing() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = orch(teams_with(enabled, mode, &["alice", "bob"]));
            assert_eq!(
                o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"])),
                ReviewIntroOutcome::Dormant,
                "enabled={enabled} mode={mode:?}"
            );
            assert!(o.store().load_review_watch().expect("read").is_empty());
        }
    }

    /// Re-introducing a pull request re-arms its row and CANNOT forget the two SHAs. Forgetting
    /// `last_reviewed_sha` would send a reviewer back over a head they already read; forgetting
    /// `requested_sha` is §14.1 F-DUP's level-trigger, one agent per tick onto one worktree.
    #[test]
    fn re_introducing_a_pull_request_preserves_both_recorded_heads() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(1)
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(row.requested_sha, HEAD_A);
        assert_eq!(row.last_reviewed_sha, HEAD_A);
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED, "re-armed");
    }

    /// A review of this exact (PR, reviewer) is LIVE. Re-arming its row would overwrite the
    /// `in_flight` marker the edge-trigger reads, so the watcher would put a second agent on the
    /// first one's detached worktree (§14.1 F-DUP). The row is left exactly as it is.
    #[test]
    fn an_introduction_leaves_an_in_flight_reviews_row_alone() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.claimed
            .insert(review_key("makewhatis", "rhapsody", 12, "bob"));

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(0),
            "nothing was written while a review is in flight"
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(row.status, REVIEW_STATUS_IN_FLIGHT);
        assert_eq!(row.requested_sha, HEAD_A);
    }

    // ── the in-process re-review event ───────────────────────────────────────────────────────────

    /// The head-advance acceptance: the daemon's own "the author pushed fixes" signal arms one more
    /// review round on a WATCHED pull request, in process, with no room post anywhere in the path.
    #[test]
    fn a_head_advance_re_arms_a_watched_pull_requests_row() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");

        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_B),
            1
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED);
        assert_eq!(
            (row.requested_sha.as_str(), row.last_reviewed_sha.as_str()),
            (HEAD_A, HEAD_A),
            "arming records no head — the dispatch and the completion do"
        );
    }

    /// **F-SEC on the re-review leg.** The head-advance event is not a second introduction path: an
    /// advance reported for a pull request nobody introduced writes NOTHING — no row appears, and
    /// nothing becomes dispatchable.
    #[test]
    fn a_head_advance_on_an_unwatched_pull_request_introduces_nothing() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("attacker", "evil", 1), HEAD_B),
            0
        );
        // Not even for an allowlisted repository: only an INTRODUCED (PR, reviewer) is watched.
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 99), HEAD_B),
            0
        );
        assert!(o.store().load_review_watch().expect("read").is_empty());
    }

    /// The rows a head advance must NOT re-arm: one already reviewed at that head, one already
    /// dispatched against it, one whose pull request has left the watch set, and one whose review is
    /// live right now.
    #[test]
    fn a_head_advance_skips_rows_that_are_already_at_that_head_or_gone() {
        let advance = |o: &mut Orchestrator| {
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_A)
        };

        // Already reviewed at HEAD_A.
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_APPROVED)
            .expect("completed");
        assert_eq!(advance(&mut o), 0, "already reviewed at this head");
        assert_eq!(
            o.store()
                .get_review_watch(&watch_key("bob"))
                .expect("read")
                .expect("row")
                .status,
            REVIEW_STATUS_APPROVED,
            "an approved row stays approved — approved-pauses (§15-c)"
        );

        // Already dispatched against HEAD_A.
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        assert_eq!(advance(&mut o), 0, "already dispatched against this head");

        // Dropped: merged, closed or gone.
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .drop_review_watch(&watch_key("bob"))
            .expect("drop");
        assert_eq!(advance(&mut o), 0, "a dropped pull request is not re-armed");

        // A live review of that row.
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.claimed
            .insert(review_key("makewhatis", "rhapsody", 12, "bob"));
        assert_eq!(
            advance(&mut o),
            0,
            "a live review is not re-armed under itself"
        );
    }

    /// §16 on the re-review leg, and the empty-SHA guard: a dormant daemon and an advance that names
    /// no head both do nothing.
    #[test]
    fn a_dormant_daemon_or_an_empty_head_arms_nothing() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), ""),
            0
        );
        o.teams = Some(teams_with(false, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_B),
            0
        );
    }

    // ── the off-loop task ────────────────────────────────────────────────────────────────────────

    struct FakeOpenPr(Box<dyn Fn() -> OpenPrResult + Send + Sync>);
    #[async_trait]
    impl OpenPrSource for FakeOpenPr {
        async fn open_pr_for_branch(&self, _o: &str, _r: &str, _b: &str) -> OpenPrResult {
            (self.0)()
        }
    }

    /// A sink recording what the task handed the control task.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<IntroducedPr>>);

    #[async_trait]
    impl ReviewIntroSink for RecordingSink {
        async fn introduce(&self, pr: IntroducedPr) -> ReviewIntroOutcome {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(pr);
            ReviewIntroOutcome::Introduced(1)
        }
    }

    impl RecordingSink {
        fn seen(&self) -> Vec<IntroducedPr> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    fn request() -> ReviewIntroRequest {
        ReviewIntroRequest {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            repo_url: REPO_URL.to_string(),
            head_branch: "symphony/STUDIO-720".to_string(),
            reviewers: vec!["bob".to_string()],
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-720".to_string(),
        }
    }

    /// Drives the task over one request and returns what reached the sink.
    async fn run_once(answer: OpenPrResult) -> Vec<IntroducedPr> {
        let sink = Arc::new(RecordingSink::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = CancelSignal::new();
        let deps = ReviewIntroDeps {
            pr_source: Some(Arc::new(FakeOpenPr(Box::new(move || match &answer {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.to_string().into()),
            })))),
            sink: Arc::clone(&sink) as Arc<dyn ReviewIntroSink>,
        };
        let task = tokio::spawn(run_review_intro_task(signal.wait(), deps, rx));
        tx.send(request()).expect("send");
        drop(tx);
        task.await.expect("task");
        sink.seen()
    }

    /// The task's whole job: resolve the branch's open pull request to a NUMBER and hand the
    /// coordinate back, with the trusted binding, the reviewers and the origin intact.
    #[tokio::test]
    async fn the_task_resolves_the_open_pull_request_and_hands_it_back() {
        let seen = run_once(Ok(Some(
            "https://github.com/makewhatis/rhapsody/pull/91".to_string(),
        )))
        .await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].pr, PrCoord::new("makewhatis", "rhapsody", 91));
        assert_eq!(seen[0].repo_url, REPO_URL);
        assert_eq!(seen[0].reviewers, vec!["bob".to_string()]);
        assert_eq!(seen[0].introduced_by, "handoff:STUDIO-720");
    }

    /// Nothing is introduced when there is nothing to introduce, and a failed lookup is not an
    /// answer: neither reaches the control task at all.
    #[tokio::test]
    async fn no_open_pull_request_or_a_failed_lookup_introduces_nothing() {
        assert!(run_once(Ok(None)).await.is_empty());
        assert!(run_once(Err("gh: rate limited".into())).await.is_empty());
    }

    /// **F-SEC in the resolver.** A URL that names some OTHER repository than the trusted binding
    /// asked about is dropped rather than followed. The coordinate the daemon acts on has to be the
    /// one config named, not one an answer redirected it to.
    #[tokio::test]
    async fn a_resolved_url_naming_another_repository_is_dropped() {
        for url in [
            "https://github.com/attacker/evil/pull/1",
            "https://github.com/makewhatis/other/pull/1",
            "https://github.com/makewhatis/rhapsody/pull/0",
            "https://example.com/makewhatis/rhapsody/pull/7",
            "not a url at all",
        ] {
            assert!(
                run_once(Ok(Some(url.to_string()))).await.is_empty(),
                "{url} must not become a review coordinate"
            );
        }
    }

    /// A daemon with no GitHub source introduces nothing rather than guessing a number — the number
    /// is the one thing that cannot be derived from configuration.
    #[tokio::test]
    async fn the_task_without_a_github_source_introduces_nothing() {
        let sink = Arc::new(RecordingSink::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = CancelSignal::new();
        let deps = ReviewIntroDeps {
            pr_source: None,
            sink: Arc::clone(&sink) as Arc<dyn ReviewIntroSink>,
        };
        let task = tokio::spawn(run_review_intro_task(signal.wait(), deps, rx));
        tx.send(request()).expect("send");
        drop(tx);
        task.await.expect("task");
        assert!(sink.seen().is_empty());
    }
}
