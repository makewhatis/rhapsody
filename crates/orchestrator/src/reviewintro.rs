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
//! containment is structural first: a slow `gh` parks THIS task, which owns all of introduction's
//! network I/O, and the daemon keeps ticking. It is temporal too since STUDIO-829 — the exec goes
//! to tokio's blocking pool and is capped by `ghsummons::GH_EXEC_TIMEOUT`, where before it had no
//! await point and a bound written here would have read stronger than it was.
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
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_DROPPED, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_REVIEWED,
    ReviewWatchKey, ReviewWatchRow,
};
use rhapsody_workspace::sanitize_key;

use crate::control_loop::{CancelWait, Event};
use crate::ghsummons::OpenPrSource;
use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::review::review_key;
use crate::stop::ControlHandle;

/// The origin recorded on a watch-set row a teammate's own handoff introduced, as
/// `handoff:<identifier>`. Recorded rather than inferred (§14.1 F-SEC): "how did this pull request
/// get here" must be readable off the row, not reconstructed later from what else happens to be
/// true.
pub const REVIEW_ORIGIN_HANDOFF: &str = "handoff";

/// The origin recorded on a row the ADOPTION sweep introduced, as `adopt:<identifier>`
/// (STUDIO-838). Distinct from [`REVIEW_ORIGIN_HANDOFF`] on purpose: a row that reached the watch
/// set by repair rather than at its own handoff is a fact about this installation an operator
/// should be able to read off the row, and [`crate::reviewdone`] keys the merge→Done transition on
/// the identifier this tag carries exactly as it does for a handoff.
pub const REVIEW_ORIGIN_ADOPT: &str = "adopt";

/// The origin recorded on a row an operator introduced through the authenticated console
/// (slice 8). Defined here, beside its sibling, so the two spellings cannot drift apart; nothing
/// writes it until the console surface exists.
pub const REVIEW_ORIGIN_CONSOLE: &str = "console";

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
    /// The ticket this pull request should be ATTACHED to in the tracker, with the pull requests
    /// it already links there, or `None` when there is no ticket to attach to at all (STUDIO-875;
    /// see [`crate::prlink::pr_link_target`]).
    ///
    /// Decided here, on the control task, because only here is the candidate snapshot — and with it
    /// what the ticket already links — in hand. The off-loop task performs the write, beside the
    /// lookup that produced the URL, and never at the cost of the introduction itself: only it
    /// knows WHICH pull request was resolved, which is what "already linked" has to mean.
    pub link: Option<crate::prlink::PrLinkTarget>,
    /// Introduce this pull request ONLY if the watch set holds no row for it at all (STUDIO-838).
    ///
    /// `false` — every handoff and every console introduction — is the ARMING introduction the
    /// subsystem was built on: a second handoff of the same ticket puts its row back to `requested`
    /// while preserving both recorded heads, which is how a re-run gets re-reviewed — except for a
    /// row that has already approved the current head, which is left alone (STUDIO-1006). `true` is
    /// the ADOPTION sweep's, and it is what makes a repair a repair: it may create the row that is
    /// missing and may never disturb one that is not.
    pub only_if_unwatched: bool,
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
    /// The reviewers to introduce. One watch-set row is written per reviewer — EXCEPT when this
    /// pull request already has a live row, where continuity keeps the reviewer(s) it already has
    /// and these are not consulted (STUDIO-1046; see [`Orchestrator::handle_review_introduce`]).
    pub reviewers: Vec<String>,
    /// The pull request's author, recorded on every row this introduction writes (STUDIO-721).
    pub author: String,
    /// The origin tag, e.g. `handoff:STUDIO-720`.
    pub introduced_by: String,
    /// Carried through from [`ReviewIntroRequest::only_if_unwatched`], which is where it is
    /// documented. Re-checked by the handler rather than trusted, as every other field here is.
    pub only_if_unwatched: bool,
}

/// What one introduction attempt did. Returned rather than logged-and-swallowed so the off-loop
/// task can say which of "it worked", "the subsystem is off" and "these coordinates will never
/// work" happened — three situations that look identical in a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewIntroOutcome {
    /// `n` (PR, reviewer) rows entered or re-armed the watch set. `0` means every candidate row was
    /// skipped — a review of it is already in flight, or it has already approved the current head
    /// (STUDIO-1006) — so the watch set already says what needs saying.
    Introduced(usize),
    /// Teams is off or the mode is not `ticketless`, so the subsystem is dormant (§16). Nothing was
    /// read and nothing was written.
    Dormant,
    /// An `only_if_unwatched` introduction — an ADOPTION — found the watch set already holding a
    /// row for this pull request, so it wrote nothing (STUDIO-838). Not a refusal and not an
    /// introduction: the repair had nothing to repair, which is the ordinary outcome of a sweep
    /// that runs every tick over a healthy installation.
    AlreadyWatched,
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
    /// Writes the Linear↔GitHub attachment for a pull request this task has just resolved
    /// (STUDIO-875). `None` disables the write; nothing else changes, and an installation whose
    /// Linear GitHub integration already links pull requests never notices either way.
    pub linker: Option<Arc<dyn crate::prlink::PrLinker>>,
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
        // Only the URL is wanted here: the watch row records no head until the watcher's own
        // `gh pr view` sweep reports one (§14.1 F-SHA), so `OpenPr::head_sha` is deliberately
        // dropped rather than half-trusted.
        let url = match src
            .open_pr_for_branch(&req.owner, &req.repo, &req.head_branch)
            .await
        {
            Ok(Some(pr)) => pr.url,
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
        // STUDIO-875, and deliberately BEFORE the introduction rather than after it: the
        // attachment is what lets the review's eventual findings comment reach this ticket, and
        // that matters whether or not the watch-set write ends up introducing anything (a row
        // already mid-review introduces nothing and still gets reviewed). Best-effort throughout —
        // `link_pr_best_effort` reports and returns, so a Linear refusal costs the next summons,
        // never this introduction.
        crate::prlink::link_pr_best_effort(deps.linker.as_deref(), req.link.as_ref(), &url).await;
        let outcome = deps
            .sink
            .introduce(IntroducedPr {
                pr: pr.clone(),
                repo_url: req.repo_url.clone(),
                reviewers: req.reviewers.clone(),
                author: req.author.clone(),
                introduced_by: req.introduced_by.clone(),
                only_if_unwatched: req.only_if_unwatched,
            })
            .await;
        match outcome {
            // `Introduced(0)` is not a failure and not an introduction: every candidate row had a
            // review already in flight or already carried an approval at the current head, so the
            // watch set already says what needs saying.
            ReviewIntroOutcome::Introduced(0) => tracing::debug!(
                pr = %pr,
                "ticketless review: every reviewer of this pull request is already mid-review or \
                 has already approved the current head; its watch rows were left as they are"
            ),
            ReviewIntroOutcome::Introduced(n) => tracing::info!(
                pr = %pr, rows = n, origin = %req.introduced_by,
                "ticketless review: pull request introduced into the watch set"
            ),
            ReviewIntroOutcome::AlreadyWatched => tracing::debug!(
                pr = %pr, origin = %req.introduced_by,
                "ticketless review: this pull request is already in the watch set, so the \
                 adoption had nothing to repair"
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

/// What one head-advance did to a pull request's watch rows (STUDIO-960).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewHeadAdvance {
    /// Rows re-armed to `requested` for a fresh review round — the author pushed something nobody
    /// has read.
    pub armed: usize,
    /// Rows whose head move was PROVEN to carry no new work, so their `last_reviewed_sha` was
    /// advanced to the new head and no round was armed. Returned as keys rather than a count
    /// because the caller's dispatch loop decides from this hand-back's opening snapshot of the
    /// rows, which still holds the pre-advance SHA: it must skip these explicitly or it re-dispatches
    /// exactly the rounds the advance just avoided.
    pub skipped: Vec<ReviewWatchKey>,
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
        // Ranked on LIVE runs, not `quorum_load` (STUDIO-721; design §14.2, "reviewer load blind to
        // ticketless reviews"). `quorum_load` is filled by `record_quorum_state`, which returns
        // early when the ticket fan-out is off — which `ticketless` makes it — so on this path it
        // is ALWAYS empty and the ranking degenerates to roster order: every pull request
        // introduced by every teammate names the same first teammate. `LoadSnapshot::from_running`
        // counts what is actually in flight, review runs included (they are stamped with the
        // reviewer's identity), so a second introduction in the same tick sees the first one.
        let load = crate::teams::LoadSnapshot::from_running(&self.running);
        let mut reviewers = crate::quorum::rank_reviewers(
            teams,
            &re.identity,
            load.counts(),
            &self.reviewer_exclusions(teams),
        );
        reviewers.truncate(teams.review.effective_reviewers());
        if reviewers.is_empty() {
            tracing::warn!(
                issue = %re.issue.identifier,
                author = %re.identity,
                "ticketless review: the roster holds nobody but the author, so no review was \
                 requested; add a teammate to `teams.yaml`"
            );
            return None;
        }
        // Decided before the struct literal moves `owner`/`repo` into it.
        let link = crate::prlink::pr_link_target(&re.issue, &owner, &repo);
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
            link,
            // A handoff ARMS the rows that still need a review (STUDIO-838): a second handoff of the
            // same ticket puts a row back to `requested`, which is how a re-run gets re-reviewed —
            // except for a row that has already approved the current head, which
            // `handle_review_introduce` leaves alone (STUDIO-1006). Only the adoption sweep refuses
            // to touch a row that already exists.
            only_if_unwatched: false,
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
    ///
    /// **It is a no-op for a row that has already approved the change at the current head**
    /// (STUDIO-1006, the F9 defect). A handoff puts a pull request back into the watch set, and
    /// today's unconditional re-arm reset every reviewer's row to `requested` — so three reviewers
    /// who had approved an UNCHANGED head were sent back over code nobody had touched, and
    /// `auto_merge_verdict` refused `RoundInFlight` until an operator cleared the pull request by
    /// hand. A row still OWING a round, or one that has never approved this head, is armed exactly
    /// as before; only a settled approval at the current change is left alone.
    ///
    /// **A handoff bringing back a pull request the watch set ALREADY holds keeps the reviewer it
    /// already has** (STUDIO-1046), rather than introducing the reviewers a fresh selection names.
    /// Selection is load-ranked, so a second round can name a different teammate; the new row then
    /// sits beside the incumbent's, and the surplus reconciler retired whichever the roster-order
    /// tie-break happened to lose — often the row holding the review history and its open findings,
    /// so the pull request got a full cold read from somebody who had never seen it. The
    /// `pr.reviewers` a handoff carries are therefore only used when this pull request has no live
    /// row yet. A reviewer who has left the roster is still reassigned — by the watcher's dispatch
    /// loop, under STUDIO-988's deferred-retirement protocol, which logs the swap — so continuity
    /// never strands an obligation on somebody who can no longer serve it.
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
        // The ADOPTION guard, and deliberately the LAST gate rather than the first (STUDIO-838):
        // every check above is one an adoption must pass exactly as a handoff does, so putting the
        // cheap exit first would be a path that skipped them. What it adds is the repair's own
        // precondition — there is something to repair — and it is keyed on the PULL REQUEST rather
        // than on the (PR, reviewer) rows below, because "one row and one reviewer, never two"
        // means a second reviewer is a duplicate too.
        if pr.only_if_unwatched && self.review_pr_is_watched(&pr.pr) {
            return ReviewIntroOutcome::AlreadyWatched;
        }
        // The daemon's last observation of this pull request's head (STUDIO-1005's memo, written on
        // the control task by the review sweep; STUDIO-988 widened the memo's value to carry the
        // open/closed bit beside the head). It is what tells an approval of the CURRENT head from one
        // the author has since pushed past; `None` means the watcher has not observed this coordinate
        // yet, and the loop below then treats a recorded approval as still current. Only the HEAD is
        // read here. The memo stores the real head for any `Found` result — an OPEN, MERGED or CLOSED
        // pull request all carry their head, with `open` carrying that bit (`reviewwatch.rs` records
        // it for every `Found` whose snapshot names a head) — so only a coordinate the watcher never
        // observed, or one it observed without a head (`Gone`, `Untrusted`, a headless `Found`),
        // yields `None`.
        let observed_head = self
            .review_observed_head
            .get(&pr.pr)
            .map(|observed| observed.head.as_str());
        // STUDIO-1012 (§7.8, path 2): after the round threshold, in `act` mode, a re-introduction
        // can never ARM a round on its own — the F9 path (a handoff resetting settled rows back to
        // `requested`) is closed structurally here. What arms a round past the threshold is an
        // active manager exchange authorization, consumed by the watcher. Off/advise and every
        // pre-threshold introduction behave exactly as before, so this is a no-op for them.
        //
        // MUTATION: delete this check and `an_act_reintroduction_past_the_threshold_arms_nothing`
        // reds (the settled row is reset to `requested`).
        let gate_active = self.review_exchange_gate_active(&pr.pr);
        // STUDIO-1046, reviewer continuity. A pull request the watch set already holds keeps the
        // reviewer(s) it already has: selection is load-ranked, so re-running it on a handoff can
        // name a different teammate each round, and the surplus reconciler then retired the
        // incumbent — the row carrying the history and the open findings. Selection runs only when
        // this pull request has no live row yet (a first handoff, or a rebuilt pull request whose
        // old row is gone). The departed-reviewer case is deliberately NOT handled here: the
        // dispatch loop reassigns it under STUDIO-988's deferred-retirement protocol and logs the
        // swap (`commit_review_watch`), which is where the acceptance for that already lives.
        let reviewers: Vec<String> = match self.existing_watch_reviewers(&pr.pr) {
            Ok(existing) if !existing.is_empty() => {
                tracing::debug!(
                    pr = %pr.pr, reviewers = ?existing,
                    "ticketless review: this pull request is already in the watch set, so its \
                     existing reviewer(s) are kept and no new selection is made"
                );
                existing
            }
            Ok(_) => pr.reviewers.clone(),
            Err(e) => {
                // Fail CLOSED, exactly as the per-row read below does: a watch set that cannot be
                // read might already hold a reviewer, and introducing a second one is the duplicate
                // this ticket exists to prevent. The next handoff asks again.
                tracing::warn!(
                    pr = %pr.pr, err = %e,
                    "ticketless review: the watch set could not be read, so no reviewer is \
                     introduced for this pull request"
                );
                return ReviewIntroOutcome::Refused("the watch set could not be read");
            }
        };
        let mut written = 0usize;
        for reviewer in reviewers.iter().filter(|r| !r.trim().is_empty()) {
            let key = ReviewWatchKey {
                owner: pr.pr.owner.clone(),
                repo: pr.pr.repo.clone(),
                number: pr.pr.number,
                reviewer: reviewer.clone(),
            };
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
            if gate_active {
                tracing::debug!(
                    review = %id, pr = %pr.pr,
                    "manager exchange: act mode past the round threshold, so a re-introduction \
                     arms nothing; the row is left exactly as it is"
                );
                continue;
            }
            // STUDIO-1006 (the F9 defect): a re-introduction must NOT reset a row whose last
            // COMPLETED review already approved the change at the current head. The write below
            // would put such a row back to `requested`, the watcher would dispatch a second review
            // of code nobody has changed, and `auto_merge_verdict` would refuse `RoundInFlight`
            // until an operator cleared the pull request by hand. Leave the settled row exactly as
            // it is and arm only the rows that have not approved this head.
            //
            // The verdict is read off the STORED row — the recorded completed review (its
            // `status` and the `last_reviewed_sha` it was recorded against) — and never off the
            // `requested` status this call is about to write, which is exactly the value the bug
            // overwrote the verdict with. `last_reviewed_sha` is non-empty only after a review
            // actually completed (F-SHA), so an `approved` row with no recorded SHA is not a
            // verdict and is still armed.
            //
            // The comparison is exact-SHA and needs no second patch-id implementation: STUDIO-977's
            // carry-over already advances `last_reviewed_sha` to the new head on a patch-preserving
            // move and keeps the terminal status, so an approval that survives a base-branch merge
            // arrives here with `last_reviewed_sha == observed_head` and is left alone. A move that
            // CHANGED the patch re-arms the row through `handle_review_head_advanced` first, so by
            // the time a re-introduction reads it the status is no longer `approved`.
            let stored = match self.store().get_review_watch(&key) {
                Ok(row) => row,
                Err(e) => {
                    // Fail CLOSED: a row that cannot be read might carry a live approval, and
                    // arming it is the one direction that discards a review that already happened.
                    tracing::warn!(
                        review = %id, err = %e,
                        "ticketless review: a watch row could not be read, so it is left as it is"
                    );
                    continue;
                }
            };
            let approved_at_current_head = stored.as_ref().is_some_and(|row| {
                row.status == REVIEW_STATUS_APPROVED
                    && !row.last_reviewed_sha.is_empty()
                    && observed_head.is_none_or(|head| head == row.last_reviewed_sha)
            });
            if approved_at_current_head {
                tracing::debug!(
                    review = %id,
                    "ticketless review: this reviewer already approved the current head; the \
                     re-introduction leaves its watch row as it is"
                );
                continue;
            }
            let row = ReviewWatchRow {
                author: pr.author.clone(),
                key,
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
        // STUDIO-1009 (§5.1): the pull request's FIRST introduction into the watch set establishes
        // its loop generation at 1. A re-introduction (the F9 handoff path) is a no-op — the store's
        // `ensure_review_generation` is INSERT-DO-NOTHING — so this cannot reset a budget or
        // invalidate an approval the way bumping it would. Best-effort like every other store write
        // on this path.
        if let Err(e) = self
            .store()
            .ensure_review_generation(&crate::reviewwatch::churn_key(&pr.pr))
        {
            tracing::warn!(
                pr = %pr.pr, err = %e,
                "ticketless review: establishing the pull request's review generation failed"
            );
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
    pub(crate) fn handle_review_head_advanced(
        &mut self,
        pr: &PrCoord,
        head_sha: &str,
        unchanged_from: &[String],
    ) -> ReviewHeadAdvance {
        if !self.review_ticketless_enabled() || head_sha.is_empty() {
            return ReviewHeadAdvance::default();
        }
        // The allowlist, re-checked HERE and not only at introduction (STUDIO-721, the slice-6
        // F-SEC review's item (a)). Re-arming reads a STORED row, and the configuration can have
        // moved since it was written: a project disabled or repointed by a hot reload leaves rows
        // behind for a repository this daemon is no longer entitled to read, and re-arming one is
        // the first step towards checking that repository out. Fails closed — the rows are left
        // exactly as they are, and the watcher's own dispatch-side check refuses them too.
        if !self.review_repo_is_configured(&pr.owner, &pr.repo) {
            tracing::warn!(
                pr = %pr,
                "ticketless review: refusing to re-arm a review in a repository no configured \
                 project owns"
            );
            return ReviewHeadAdvance::default();
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(pr = %pr, err = %e, "ticketless review: the watch set could not be read; no re-review was armed");
                return ReviewHeadAdvance::default();
            }
        };
        let mut advance = ReviewHeadAdvance::default();
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
            // Already waiting to be reviewed at no particular head — a freshly introduced row, on
            // this pull request's first tick. Rewriting it to the status it already holds costs a
            // store write per row per tick until the first dispatch, and would report `armed` for a
            // pull request nobody has pushed to (STUDIO-727). `armed` answers "did the author
            // push?", so a row that was never disarmed must not count.
            if row.status == REVIEW_STATUS_REQUESTED && row.requested_sha.is_empty() {
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
            // STUDIO-960, widened by STUDIO-977: the head moved, but the CHANGE it carries against
            // the base is the same (patch-id) as the one this row's VERDICT was made against, so
            // there is no new work to read — a merge from the base branch included, which rewrites
            // the hunk line numbers while carrying the same change. The
            // verdict is carried forward by advancing `last_reviewed_sha` to the new head and
            // KEEPING the terminal status — the alternative, re-arming, would discard an approval
            // the diff still justifies and bill the whole round again.
            //
            // Restricted to the two terminal statuses on purpose. A `requested`/`in_flight`/
            // `truncated` row still OWES a review of this head — its partial or absent round is not
            // a verdict a rebase can carry — and advancing a re-armed row that had already lost its
            // terminal status would leave a `requested` row the dispatcher still owes, which is the
            // opposite of the saving. The proof itself is the caller's: `unchanged_from` is empty
            // unless the off-loop watcher compared the two diffs and found them identical, so a
            // failed comparison degrades here to the plain re-arm below.
            if matches!(
                row.status.as_str(),
                REVIEW_STATUS_REVIEWED | REVIEW_STATUS_APPROVED
            ) && unchanged_from
                .iter()
                .any(|old| old == &row.last_reviewed_sha)
            {
                let reviewed_sha = row.last_reviewed_sha.clone();
                let carried_status = row.status.clone();
                let key = row.key.clone();
                // `save_review_watch` deliberately never touches `last_reviewed_sha` (F-SHA), and
                // the row's status is already the terminal one we want to keep, so the write is
                // `mark_review_completed` re-stamping the SAME verdict at the new head. A dedicated
                // method would say "carried" more loudly; reusing this one keeps the F-SHA guard in
                // one place and makes the resulting row byte-identical to a verdict recorded at
                // `head_sha` directly.
                match self
                    .store()
                    .mark_review_completed(&key, head_sha, &carried_status)
                {
                    Ok(()) => {
                        tracing::info!(
                            pr = %pr, reviewer = %key.reviewer, from = %reviewed_sha,
                            head = head_sha, status = %carried_status,
                            "ticketless review: the head moved but the change against the base is \
                             the same as the reviewed one (a patch-id comparison); the verdict \
                             carries forward and no review round is armed"
                        );
                        advance.skipped.push(key);
                    }
                    Err(e) => {
                        tracing::warn!(review = %id, err = %e, "ticketless review: carrying a verdict across an unchanged head move failed; a normal round is armed")
                    }
                }
                continue;
            }
            // Racing a watcher tick is safe TODAY, and only for a reason worth writing down: the
            // tick decides from a snapshot loaded before this write, so it may still be holding the
            // pre-arm row — but `review_round_due` returns the same verdict for every status this
            // re-arm can replace (`requested`/`in_flight`/`truncated` already answer "due", and the
            // two terminals are re-armed only when `last_reviewed_sha != head`, which is exactly
            // what they answer "due" on). A future status that keys off `requested_sha` would break
            // that invariance and needs a re-read here, not a stale snapshot.
            let armed_row = ReviewWatchRow {
                status: REVIEW_STATUS_REQUESTED.to_string(),
                open: true,
                ..row
            };
            match self.store().save_review_watch(armed_row) {
                Ok(()) => advance.armed += 1,
                Err(e) => {
                    tracing::warn!(review = %id, err = %e, "ticketless review: re-arming the watch row failed")
                }
            }
        }
        advance
    }

    /// The reviewers this watch set already holds for `pr`'s LIVE rows (STUDIO-1046) — the set a
    /// re-introduction must keep rather than replace. Empty means the pull request has no reviewer
    /// yet, so a fresh selection is the right answer.
    ///
    /// LIVE only ([`load_live_review_watch`](rhapsody_store::Store::load_live_review_watch)): a
    /// merged, closed or dismissed row is not live and must not pin a reviewer, so a rebuilt pull
    /// request under the same number starts over — the same rule
    /// [`review_pr_is_watched`](Self::review_pr_is_watched) applies to a dropped row for adoption.
    ///
    /// Owner and repo are compared EXACTLY, not case-insensitively: `handle_review_introduce`
    /// rebuilds the watch key from `pr`'s own spelling, so a case-insensitive match that returned a
    /// differently-cased coordinate would be written back as a SECOND row rather than re-arming the
    /// existing one. Every row for a coordinate is written from the same `pr_from_url` resolution,
    /// so the spellings agree.
    fn existing_watch_reviewers(
        &self,
        pr: &PrCoord,
    ) -> Result<Vec<String>, rhapsody_store::StoreError> {
        let rows = self.store().load_live_review_watch()?;
        Ok(rows
            .into_iter()
            .filter(|r| {
                r.key.number == pr.number && r.key.owner == pr.owner && r.key.repo == pr.repo
            })
            .map(|r| r.key.reviewer)
            .collect())
    }

    /// Whether the watch set holds ANY row for this pull request — live or retired, whoever the
    /// reviewer is (STUDIO-838).
    ///
    /// The whole set and not [`load_live_review_watch`](rhapsody_store::Store::load_live_review_watch):
    /// a merged, closed or dismissed pull request stays in the set as a `dropped` row rather than
    /// being deleted, so reading only the live rows would call every pull request this daemon has
    /// ever finished reviewing an orphan, and re-adopt each of them.
    ///
    /// **A store that cannot be read answers `true`.** There is no safe way to introduce without
    /// knowing what is already there — the failure mode is a duplicate reviewer on a real pull
    /// request — and the repair is the caller that can afford to wait: the sweep runs again next
    /// tick.
    pub(crate) fn review_pr_is_watched(&self, pr: &PrCoord) -> bool {
        match self.store().load_review_watch() {
            Ok(rows) => rows.iter().any(|row| {
                row.key.number == pr.number
                    && row.key.owner.eq_ignore_ascii_case(&pr.owner)
                    && row.key.repo.eq_ignore_ascii_case(&pr.repo)
            }),
            Err(e) => {
                tracing::warn!(
                    pr = %pr, err = %e,
                    "ticketless review: the watch set could not be read, so this pull request is \
                     treated as already watched and nothing is adopted"
                );
                true
            }
        }
    }

    /// Whether `owner/repo` is a repository this daemon is configured for — the watched-repo
    /// allowlist (§15-a), and the reason a pull-request coordinate is never taken on trust.
    ///
    /// Every ENABLED project's `repo`, and — only for a configuration that resolved to no projects
    /// at all — the top-level `repo`. Comparison is on the parsed `owner/repo` rather than on the
    /// URL text, so `git@github.com:o/r.git` and `https://github.com/o/r` are the same repository
    /// — which they are — and case-insensitively, because GitHub logins and repository names are.
    ///
    /// The top-level fallback is narrow on purpose (STUDIO-725). `resolve_projects` inherits the
    /// top-level `repo:` into every project that declares none, and synthesizes a project even for
    /// the legacy single-project form, so whenever `projects` is non-empty the ONLY state in which
    /// `cfg.repo` matches while no enabled project does is a PAUSED project — and re-admitting a
    /// repository the operator explicitly paused is the one thing this guard exists to refuse.
    pub(crate) fn review_repo_is_configured(&self, owner: &str, repo: &str) -> bool {
        let Some(eff) = self.eff.as_ref() else {
            return false;
        };
        let matches = |url: &str| {
            crate::ghsummons::parse_repo(url)
                .is_some_and(|(o, r)| o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo))
        };
        eff.projects.iter().any(|p| !p.disabled && matches(&p.repo))
            || (eff.projects.is_empty() && matches(&eff.cfg.repo))
    }
}

/// Whether two repository bindings name the SAME GitHub repository — the spelling-insensitive
/// comparison the watched-repo allowlist uses, lifted so the dispatch-side router can share it
/// (STUDIO-725, jimmy's finding #6). `git@github.com:o/r.git` and `https://github.com/o/r` are one
/// repository, and logins and repository names are case-insensitive.
///
/// A binding neither side can parse (a non-GitHub remote) falls back to exact text equality, so a
/// caller never loses a match it used to have.
pub(crate) fn same_repository(a: &str, b: &str) -> bool {
    match (
        crate::ghsummons::parse_repo(a),
        crate::ghsummons::parse_repo(b),
    ) {
        (Some((ao, ar)), Some((bo, br))) => {
            ao.eq_ignore_ascii_case(&bo) && ar.eq_ignore_ascii_case(&br)
        }
        _ => a == b,
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

    /// Reports that a WATCHED pull request's head advanced, arming one more review round or
    /// carrying an existing verdict forward when the diff is unchanged (STUDIO-960).
    ///
    /// This is the design's in-process control Event standing in for the room post §14.1 F-SEC
    /// rules out. It cannot introduce a pull request — the loop-side `handle_review_head_advanced`
    /// only ever updates rows that already exist — so an observation about an unwatched coordinate
    /// is inert by construction rather than by the caller's care.
    ///
    /// This event carries no diff proof, so a caller using it never skips: it is the console's and
    /// the introduction path's direct signal, not the watcher's compared observation, and skipping
    /// requires a comparison this seam cannot make.
    pub async fn review_head_advanced(&self, pr: PrCoord, head_sha: &str) -> ReviewHeadAdvance {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ev = Event::ReviewHeadAdvanced {
            pr,
            head_sha: head_sha.to_string(),
            reply: tx,
        };
        if self.events.send(ev).is_err() {
            // The loop is gone: there is nothing to re-review into.
            return ReviewHeadAdvance::default();
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or_default(),
            _ = lifetime.cancelled() => ReviewHeadAdvance::default(),
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
    /// A second, ENABLED project's repository — the live half of a multi-project allowlist.
    const LIVE_URL: &str = "https://github.com/makewhatis/podium.git";
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
            review: Review {
                mode,
                ..Review::default()
            },
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

    /// The multi-project shape the paused-project allowlist property needs: the project owning
    /// [`REPO_URL`] is PAUSED, a second project owning another repository is live, and the
    /// top-level `repo:` still names [`REPO_URL`] exactly as `resolve_projects` would have
    /// inherited it into a project that declares none.
    fn orch_with_paused_owner(teams: Teams) -> Orchestrator {
        let mut o = orch(teams);
        let mut live = empty_resolved_project("podium", Arc::new(Fake::new()));
        live.repo = LIVE_URL.to_string();
        if let Some(eff) = o.eff.as_mut() {
            eff.cfg.repo = REPO_URL.to_string();
            eff.projects[0].disabled = true;
            eff.projects.push(live);
        }
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
            only_if_unwatched: false,
        }
    }

    /// The same coordinate as [`introduced`], as the ADOPTION sweep hands it over: the repair
    /// origin, and the "only if nothing is watching it" flag that makes it a repair rather than a
    /// second way to request a review (STUDIO-838).
    fn adoption(owner: &str, repo: &str, number: i64, reviewers: &[&str]) -> IntroducedPr {
        IntroducedPr {
            introduced_by: format!("{REVIEW_ORIGIN_ADOPT}:STUDIO-836"),
            only_if_unwatched: true,
            ..introduced(owner, repo, number, reviewers)
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

    /// **F-SEC at the plan.** A run bound to a repository no ENABLED project owns introduces
    /// nothing — the allowlist is the configuration, exactly as `find_issue` refuses a key that is
    /// not on a project this team works.
    ///
    /// The paused arm is the one that has to be pinned on a REAL configuration shape (STUDIO-725).
    /// `resolve_projects` inherits the top-level `repo:` into a project that declares none, so on
    /// any live multi-project config `cfg.repo` equals some project's repository — which means an
    /// unconditional `cfg.repo` fallback re-admits precisely the project the operator paused. A
    /// fixture that leaves `cfg.repo` empty asserts the property and proves nothing, so
    /// [`orch_with_paused_owner`] populates it.
    #[test]
    fn an_off_allowlist_repository_binding_plans_no_introduction() {
        let o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(
            o.plan_review_intro(&teammate_run("alice", OTHER_URL))
                .is_none()
        );
        // A PAUSED project's repository is not on the allowlist either: it is configuration this
        // daemon was told to stop acting on, and the still-populated top-level `repo:` naming it
        // does not put it back.
        let paused =
            orch_with_paused_owner(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(
            !paused.eff.as_ref().expect("effective").cfg.repo.is_empty(),
            "the fixture must exercise a POPULATED top-level repo, else it proves nothing"
        );
        assert!(
            paused
                .plan_review_intro(&teammate_run("alice", REPO_URL))
                .is_none()
        );
        assert!(
            !paused.review_repo_is_configured("makewhatis", "rhapsody"),
            "a paused project's repository is off the allowlist"
        );
        assert!(
            paused.review_repo_is_configured("makewhatis", "podium"),
            "the live project alongside it still is"
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

    /// **STUDIO-1046 acceptance: a handoff keeps the pull request's existing reviewer.** The pull
    /// request already has a watch row, so re-running selection — which is load-ranked and can name
    /// a different teammate every round — must NOT create a second row. The existing reviewer's row
    /// is re-armed for the next round instead, so it keeps the review history and the findings.
    ///
    /// MUTATION: iterate `pr.reviewers` instead of the existing rows and this reds — a row for
    /// `alice` (the reviewer selection named) appears beside `bob`'s.
    #[test]
    fn a_re_introduction_keeps_the_existing_reviewer_instead_of_re_selecting() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        // `bob` reviewed and asked for changes at the current head.
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");

        // The author hands off the fixes. Selection would name `alice`, but this pull request
        // already has a reviewer, so no selection is made.
        assert_eq!(
            o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["alice"])),
            ReviewIntroOutcome::Introduced(1),
            "the existing reviewer's row is re-armed for the next round"
        );

        let rows = o.store().load_live_review_watch().expect("read");
        assert_eq!(rows.len(), 1, "no second row is created: {rows:?}");
        assert_eq!(rows[0].key.reviewer, "bob");
        assert_eq!(rows[0].status, REVIEW_STATUS_REQUESTED, "armed for round 2");
        assert_eq!(
            rows[0].last_reviewed_sha, HEAD_A,
            "the history is preserved"
        );
        assert!(
            o.store()
                .get_review_watch(&watch_key("alice"))
                .expect("read")
                .is_none(),
            "the reviewer selection named is never introduced"
        );
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

    // ── STUDIO-1006 (F9): a re-introduction does not reset an approval at the current head ───────

    /// Record a COMPLETED approval for `reviewer`'s row of PR 12 at `sha` — the recorded verdict the
    /// re-introduction reads, stamped exactly as `mark_review_completed` stamps a real one.
    fn approve_row(o: &Orchestrator, reviewer: &str, sha: &str) {
        o.store()
            .mark_review_requested(&watch_key(reviewer), sha)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key(reviewer), sha, REVIEW_STATUS_APPROVED)
            .expect("completed");
    }

    /// **The incident replay (`makewhatis/rhapsody#216`, F9).** Three reviewers approved the
    /// CURRENT, unchanged head; two handoff re-introductions followed at that same head. Before
    /// this fix each re-introduction reset every row to `requested`, so the auto-merge gate refused
    /// `RoundInFlight` on every poll and the pull request deadlocked behind an operator
    /// `POST /api/v1/reviews/clear`. After it, all three stay approved and the gate clears with no
    /// operator action.
    ///
    /// MUTATION: restore the unconditional re-arm in `handle_review_introduce` and this reds twice —
    /// the rows come back `requested`, and `auto_merge_verdict_with_proof` refuses.
    #[test]
    fn a_re_introduction_leaves_rows_approved_at_the_current_head_alone() {
        let mut o = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            &["alice", "bob", "carol"],
        ));
        let pr = introduced("makewhatis", "rhapsody", 12, &["alice", "bob", "carol"]);
        o.handle_review_introduce(&pr);
        for reviewer in ["alice", "bob", "carol"] {
            approve_row(&o, reviewer, HEAD_A);
        }
        // The watcher has observed the head these approvals were recorded at.
        o.review_observed_head.insert(
            pr.pr.clone(),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: HEAD_A.to_string(),
            },
        );

        // Both re-introductions at this unchanged head write nothing.
        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(0)
        );
        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(0)
        );

        let rows = o.store().load_live_review_watch().expect("read");
        assert_eq!(rows.len(), 3, "no row was added or removed");
        for row in &rows {
            assert_eq!(
                row.status, REVIEW_STATUS_APPROVED,
                "{} was reset by a re-introduction at the head it approved",
                row.key.reviewer
            );
        }
        let mine: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            crate::automerge::auto_merge_verdict_with_proof(&mine, HEAD_A, &[HEAD_A]),
            Ok(vec![
                "alice".to_string(),
                "bob".to_string(),
                "carol".to_string()
            ]),
            "auto-merge proceeds with no operator action"
        );
    }

    /// A handoff after a REAL push still arms every row that has NOT approved the new head. The
    /// approvals are at `HEAD_A`; the daemon has since observed `HEAD_B`, so the approval is about a
    /// change nobody read and the row is armed exactly as it always was.
    ///
    /// MUTATION: skip every non-approved row, or decide the skip from the approval's status alone
    /// (ignoring the head it was recorded against), and this reds — the rows come back unarmed while
    /// a review is genuinely owed.
    #[test]
    fn a_re_introduction_after_a_real_head_move_still_arms_every_row() {
        let mut o = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            &["alice", "bob", "carol"],
        ));
        let pr = introduced("makewhatis", "rhapsody", 12, &["alice", "bob", "carol"]);
        o.handle_review_introduce(&pr);
        for reviewer in ["alice", "bob", "carol"] {
            approve_row(&o, reviewer, HEAD_A);
        }
        o.review_observed_head.insert(
            pr.pr.clone(),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: HEAD_B.to_string(),
            },
        );

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(3),
            "an approval of a head the author has pushed past is not an approval of the new head"
        );
        for row in o.store().load_live_review_watch().expect("read") {
            assert_eq!(
                row.status, REVIEW_STATUS_REQUESTED,
                "{} must be armed for the new head",
                row.key.reviewer
            );
        }
    }

    /// STUDIO-1012 (§7.8 path 2): in `act` mode after the round threshold a re-introduction arms
    /// NOTHING — the same head move that arms every row above leaves them untouched. This closes the
    /// F9 path structurally: a handoff cannot start a post-threshold exchange on its own, only a
    /// manager exchange authorization can.
    ///
    /// MUTATION: delete the `gate_active` check in `handle_review_introduce` and this reds (three
    /// rows come back `requested`).
    #[test]
    fn an_act_reintroduction_past_the_threshold_arms_nothing() {
        let mut teams = teams_with(true, ReviewMode::Ticketless, &["alice", "bob", "carol"]);
        teams.review.adjudicate_after_rounds = 1;
        teams.manager.review_authority = rhapsody_config::teams::ReviewAuthority::Act;
        let mut o = orch(teams);
        let pr = introduced("makewhatis", "rhapsody", 12, &["alice", "bob", "carol"]);
        o.handle_review_introduce(&pr);
        for reviewer in ["alice", "bob", "carol"] {
            approve_row(&o, reviewer, HEAD_A);
        }
        o.review_observed_head.insert(
            pr.pr.clone(),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: HEAD_B.to_string(),
            },
        );
        // The round threshold is reached: STUDIO-1004's answered-exchange count, one round here.
        o.review_rounds
            .insert(crate::reviewwatch::churn_key(&pr.pr), 1);

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(0),
            "act mode past the threshold arms no round from a re-introduction"
        );
        for row in o.store().load_live_review_watch().expect("read") {
            assert_eq!(
                row.status, REVIEW_STATUS_APPROVED,
                "{} was armed by a re-introduction past the threshold",
                row.key.reviewer
            );
        }
    }

    /// Only an APPROVAL at the current head is left alone. A `reviewed` row (findings) has not
    /// approved the head and is armed exactly as before, so the fix cannot quietly swallow a round
    /// a reviewer asked for. This is the ticket's "a row that hasn't approved it must still be
    /// armed".
    ///
    /// MUTATION: widen the skip to any terminal row and this reds.
    #[test]
    fn a_reviewed_row_at_the_current_head_is_still_armed() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");
        o.review_observed_head.insert(
            pr.pr.clone(),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: HEAD_A.to_string(),
            },
        );

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(1),
            "findings are not an approval"
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED);
    }

    /// The observed-head memo is in-memory (STUDIO-1005), so a restart leaves it empty while the
    /// approvals persist. With no observation to contradict a recorded approval the re-introduction
    /// leaves it alone — the failure direction that never discards a review that already happened.
    /// A later sweep that sees a moved head arms the row through the head-advance path regardless,
    /// so no review of a real push is lost.
    #[test]
    fn an_approval_with_no_observed_head_is_still_left_alone() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        approve_row(&o, "bob", HEAD_A);
        assert!(
            !o.review_observed_head.contains_key(&pr.pr),
            "the fixture must model a daemon that has not observed this coordinate"
        );

        assert_eq!(
            o.handle_review_introduce(&pr),
            ReviewIntroOutcome::Introduced(0)
        );
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(row.status, REVIEW_STATUS_APPROVED, "the approval survived");
    }

    /// §16 on the new path, and the ticket's "passes on old and new code" guard: with Teams off — or
    /// on any mode but `ticketless` — the handler is dormant BEFORE it reads or writes anything, so
    /// a re-introduction that would have reset an approved row on a ticketless install cannot touch
    /// it here. Nothing about a non-ticketless or Teams-off installation changed.
    #[test]
    fn a_dormant_daemon_never_resets_an_approved_row() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = orch(teams_with(enabled, mode, &["alice", "bob"]));
            o.store()
                .save_review_watch(ReviewWatchRow {
                    key: watch_key("bob"),
                    author: "alice".to_string(),
                    introduced_by: "handoff:STUDIO-720".to_string(),
                    requested_sha: HEAD_A.to_string(),
                    last_reviewed_sha: HEAD_A.to_string(),
                    status: REVIEW_STATUS_APPROVED.to_string(),
                    open: true,
                })
                .expect("seed an approved row");

            assert_eq!(
                o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"])),
                ReviewIntroOutcome::Dormant,
                "enabled={enabled} mode={mode:?}"
            );
            let row = o
                .store()
                .get_review_watch(&watch_key("bob"))
                .expect("read")
                .expect("row");
            assert_eq!(
                row.status, REVIEW_STATUS_APPROVED,
                "enabled={enabled} mode={mode:?}"
            );
        }
    }

    // ── adoption: introducing a pull request nothing is watching (STUDIO-838) ────────────────────

    /// The idempotency acceptance. An adoption is a REPAIR, so it may only ever create the row that
    /// is missing: a pull request the watch set already holds is left exactly as it was found, and
    /// the reviewers the adoption named are not written beside the ones already there. Two rows
    /// would be two review runs — "a duplicate wakes a real agent against a real PR for no reason".
    #[test]
    fn adopting_an_already_watched_pull_request_writes_no_second_row() {
        let mut o = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            &["alice", "bob", "carol"],
        ));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_APPROVED)
            .expect("completed");

        assert_eq!(
            o.handle_review_introduce(&adoption("makewhatis", "rhapsody", 12, &["carol"])),
            ReviewIntroOutcome::AlreadyWatched,
        );
        let rows = o.store().load_review_watch().expect("read");
        assert_eq!(rows.len(), 1, "one row, one reviewer: {rows:?}");
        assert_eq!(rows[0].key.reviewer, "bob");
        assert_eq!(
            rows[0].status, REVIEW_STATUS_APPROVED,
            "the settled round was not re-armed"
        );
    }

    /// A RETIRED row still counts as watched. The watch set keeps a merged, closed or dismissed
    /// pull request as a `dropped` row rather than deleting it (that is what the console renders),
    /// so reading "no LIVE row" as "orphaned" would re-adopt every pull request this daemon has
    /// ever finished reviewing — and dispatch a reviewer at each of them.
    #[test]
    fn adopting_a_pull_request_whose_row_is_retired_writes_nothing() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.handle_review_introduce(&introduced("makewhatis", "rhapsody", 12, &["bob"]));
        o.store()
            .drop_review_watch(&watch_key("bob"))
            .expect("drop");

        assert_eq!(
            o.handle_review_introduce(&adoption("makewhatis", "rhapsody", 12, &["bob"])),
            ReviewIntroOutcome::AlreadyWatched,
        );
        let rows = o.store().load_review_watch().expect("read");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, REVIEW_STATUS_DROPPED, "left retired");
    }

    /// **The gate pin STUDIO-838 names.** An adoption of a pull request in a repository no
    /// configured project owns must FAIL TO ADOPT — every guard a handoff-time introduction passes,
    /// an adoption passes, and the allowlist is F-SEC's anchor.
    ///
    /// It is a distinct test from `an_off_allowlist_introduction_is_refused_and_writes_nothing`
    /// because the adopt path added a gate BELOW this one (`only_if_unwatched`), and an ordering
    /// that let the cheap new exit run first would be a path that skipped every check above it. The
    /// watch set here is empty, so the new gate cannot answer, and the refusal can only come from
    /// the allowlist.
    #[test]
    fn an_adoption_in_an_unconfigured_repository_is_refused() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_introduce(&adoption("attacker", "evil", 1, &["bob"])),
            ReviewIntroOutcome::Refused("no configured project owns the PR's repo")
        );
        assert!(
            o.store().load_review_watch().expect("read").is_empty(),
            "no watch-set entry may exist for an off-allowlist repository"
        );
    }

    /// The same, for the rest of the gate battery: an adoption carrying coordinates that cannot
    /// name a pull request, or no reviewer, is refused exactly as a handoff's would be. Pinned
    /// separately from the handoff case for the ordering reason above.
    #[test]
    fn an_adoption_with_unusable_coordinates_is_refused() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        for (why, pr) in [
            ("no owner", adoption("", "rhapsody", 12, &["bob"])),
            ("no repo", adoption("makewhatis", "", 12, &["bob"])),
            ("number 0", adoption("makewhatis", "rhapsody", 0, &["bob"])),
            ("no reviewer", adoption("makewhatis", "rhapsody", 12, &[])),
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

    /// §16 for the adopt path: a dormant daemon adopts nothing, whatever it is handed.
    #[test]
    fn a_dormant_daemon_adopts_nothing() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = orch(teams_with(enabled, mode, &["alice", "bob"]));
            assert_eq!(
                o.handle_review_introduce(&adoption("makewhatis", "rhapsody", 144, &["bob"])),
                ReviewIntroOutcome::Dormant,
                "enabled={enabled} mode={mode:?}"
            );
            assert!(o.store().load_review_watch().expect("read").is_empty());
        }
    }

    /// The other half of the same property, and the one the ticket is actually about: a pull
    /// request NOTHING is watching is exactly what an adoption may write. The fixture starts from
    /// the orphaned state — an empty watch set — because a test that introduces normally first
    /// would pass without the adopt path existing at all.
    #[test]
    fn adopting_an_unwatched_pull_request_writes_its_row() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        assert!(
            o.store().load_review_watch().expect("read").is_empty(),
            "the fixture must start orphaned"
        );

        assert_eq!(
            o.handle_review_introduce(&adoption("makewhatis", "rhapsody", 144, &["bob"])),
            ReviewIntroOutcome::Introduced(1),
        );
        let rows = o.store().load_review_watch().expect("read");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].key.number, 144);
        assert_eq!(rows[0].key.reviewer, "bob");
        assert_eq!(rows[0].introduced_by, "adopt:STUDIO-836");
        assert_eq!(rows[0].status, REVIEW_STATUS_REQUESTED);
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
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_B, &[])
                .armed,
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
            o.handle_review_head_advanced(&PrCoord::new("attacker", "evil", 1), HEAD_B, &[])
                .armed,
            0
        );
        // Not even for an allowlisted repository: only an INTRODUCED (PR, reviewer) is watched.
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 99), HEAD_B, &[])
                .armed,
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
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_A, &[])
                .armed
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
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), "", &[])
                .armed,
            0
        );
        o.teams = Some(teams_with(false, ReviewMode::Ticketless, &["alice", "bob"]));
        assert_eq!(
            o.handle_review_head_advanced(&PrCoord::new("makewhatis", "rhapsody", 12), HEAD_B, &[])
                .armed,
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
            only_if_unwatched: false,
            link: None,
        }
    }

    /// Records every attachment write the task asks for (STUDIO-875).
    #[derive(Default)]
    struct RecordingLinker(std::sync::Mutex<Vec<(String, String)>>);

    impl RecordingLinker {
        fn seen(&self) -> Vec<(String, String)> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl crate::prlink::PrLinker for RecordingLinker {
        async fn link_pull_request(
            &self,
            issue_id: &str,
            url: &str,
        ) -> Result<(), rhapsody_tracker::TrackerError> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((issue_id.to_string(), url.to_string()));
            Ok(())
        }
    }

    /// Drives the task over `req` with a recording linker and returns what it was asked to attach.
    async fn link_once(req: ReviewIntroRequest, answer: OpenPrResult) -> Vec<(String, String)> {
        let linker = Arc::new(RecordingLinker::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = CancelSignal::new();
        let deps = ReviewIntroDeps {
            pr_source: Some(Arc::new(FakeOpenPr(Box::new(move || match &answer {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.to_string().into()),
            })))),
            sink: Arc::new(RecordingSink::default()) as Arc<dyn ReviewIntroSink>,
            linker: Some(Arc::clone(&linker) as Arc<dyn crate::prlink::PrLinker>),
        };
        let task = tokio::spawn(run_review_intro_task(signal.wait(), deps, rx));
        tx.send(req).expect("send");
        drop(tx);
        task.await.expect("task");
        linker.seen()
    }

    fn request_wanting_a_link() -> ReviewIntroRequest {
        ReviewIntroRequest {
            link: Some(crate::prlink::PrLinkTarget {
                issue_id: "iss-uuid".to_string(),
                identifier: "STUDIO-720".to_string(),
                owner: "makewhatis".to_string(),
                repo: "rhapsody".to_string(),
                linked: Vec::new(),
            }),
            ..request()
        }
    }

    fn resolved_pr() -> OpenPrResult {
        Ok(Some(crate::ghsummons::OpenPr {
            url: "https://github.com/makewhatis/rhapsody/pull/154".to_string(),
            head_sha: "abc".to_string(),
        }))
    }

    // ── the attachment the summons routing needs (STUDIO-875) ───────────────────────────────────

    /// The fix itself: the moment the task resolves a pull request for a ticket, it writes the
    /// GitHub attachment that lets a later summons on that pull request reach the ticket. Without
    /// it `apply_github_summons` walks an empty `linked_prs` and drops the reviewer's findings
    /// comment on every poll, forever.
    #[tokio::test]
    async fn a_resolved_pull_request_is_attached_to_its_ticket() {
        let seen = link_once(request_wanting_a_link(), resolved_pr()).await;
        assert_eq!(
            seen,
            vec![(
                "iss-uuid".to_string(),
                "https://github.com/makewhatis/rhapsody/pull/154".to_string()
            )]
        );
    }

    /// No ticket to attach to — an issue the planner saw with no tracker id — is not a write.
    #[tokio::test]
    async fn a_request_with_no_ticket_is_never_attached() {
        assert!(link_once(request(), resolved_pr()).await.is_empty());
    }

    /// What keeps this free on an installation whose GitHub integration works, end to end through
    /// the task: the ticket already links the pull request the lookup just resolved, so there is
    /// nothing to write. The gate is the RESOLVED number and not the attachment's `merged` flag,
    /// which on the installations this exists for is a field nothing refreshes.
    #[tokio::test]
    async fn a_ticket_already_linking_the_resolved_pull_request_is_not_attached_again() {
        let req = ReviewIntroRequest {
            link: Some(crate::prlink::PrLinkTarget {
                linked: vec![154],
                ..request_wanting_a_link().link.expect("a link target")
            }),
            ..request_wanting_a_link()
        };
        assert!(link_once(req, resolved_pr()).await.is_empty());
    }

    /// And the round-2 sequence: the ticket links #154 (merged long ago, still reading unmerged
    /// because nothing maintains that field), the branch has pushed again and #157 is what
    /// resolved. #157 must be attached, or the review about to be requested on it files findings
    /// that reach nobody.
    #[tokio::test]
    async fn a_second_pull_request_on_a_stale_linked_ticket_is_attached() {
        let req = ReviewIntroRequest {
            link: Some(crate::prlink::PrLinkTarget {
                linked: vec![154],
                ..request_wanting_a_link().link.expect("a link target")
            }),
            ..request_wanting_a_link()
        };
        let seen = link_once(
            req,
            Ok(Some(crate::ghsummons::OpenPr {
                url: "https://github.com/makewhatis/rhapsody/pull/157".to_string(),
                head_sha: "abc".to_string(),
            })),
        )
        .await;
        assert_eq!(
            seen,
            vec![(
                "iss-uuid".to_string(),
                "https://github.com/makewhatis/rhapsody/pull/157".to_string()
            )]
        );
    }

    /// No pull request, no attachment: there is nothing to link, and inventing a URL is the one
    /// thing this module exists to refuse.
    #[tokio::test]
    async fn nothing_is_attached_when_no_pull_request_resolves() {
        assert!(
            link_once(request_wanting_a_link(), Ok(None))
                .await
                .is_empty()
        );
        assert!(
            link_once(request_wanting_a_link(), Err("boom".into()))
                .await
                .is_empty()
        );
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
            linker: None,
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
        let seen = run_once(Ok(Some(crate::ghsummons::OpenPr {
            url: "https://github.com/makewhatis/rhapsody/pull/91".to_string(),
            head_sha: "head-a".to_string(),
        })))
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
                run_once(Ok(Some(crate::ghsummons::OpenPr {
                    url: url.to_string(),
                    head_sha: String::new(),
                })))
                .await
                .is_empty(),
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
            linker: None,
        };
        let task = tokio::spawn(run_review_intro_task(signal.wait(), deps, rx));
        tx.send(request()).expect("send");
        drop(tx);
        task.await.expect("task");
        assert!(sink.seen().is_empty());
    }

    // ── STUDIO-1009: the review evidence ledger ─────────────────────────────────────────────────

    fn gen_of(o: &Orchestrator, reviewer: &str) -> i64 {
        o.review_generation(&watch_key(reviewer))
    }

    /// A `/clear` bumps the generation; a handoff re-introduction, a head change and a restart do
    /// not (design §5.1, §7.4).
    #[test]
    fn only_a_clear_bumps_the_generation() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        // First introduction establishes generation 1.
        o.handle_review_introduce(&pr);
        assert_eq!(gen_of(&o, "bob"), 1, "first introduction establishes 1");

        // A head change is NOT a generation event.
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&watch_key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");
        assert_eq!(gen_of(&o, "bob"), 1, "a head change does not bump it");

        // A handoff re-introduction is NOT a generation event (the F9 path).
        o.handle_review_introduce(&pr);
        assert_eq!(gen_of(&o, "bob"), 1, "a re-introduction does not bump it");

        // A restart (a fresh boot read) is NOT a generation event: the durable value is what it is.
        assert_eq!(
            o.store()
                .review_bound(&crate::reviewwatch::churn_key(&pr.pr))
                .expect("read")
                .expect("bound")
                .generation,
            1
        );

        // The operator's clear DOES bump it.
        let outcome = o.handle_review_clear(&pr.pr);
        assert!(
            matches!(
                outcome,
                crate::reviewconsole::ReviewControlOutcome::Applied(_)
            ),
            "the clear applied, got {outcome:?}"
        );
        assert_eq!(gen_of(&o, "bob"), 2, "the operator's /clear bumps it");
    }

    /// **The F9 replay at the predicate (STUDIO-1009).** A completed approval survives a
    /// re-introduction that resets the row's `status` to `requested`, because the predicate reads
    /// the COMPLETED review's verdict and generation, never `status`.
    ///
    /// MUTATION: read `row.status` (or bump the generation on re-introduction, or take
    /// `current_generation` from anywhere but the bound) and this test fails.
    #[test]
    fn an_approval_survives_an_f9_reintroduction_at_the_predicate() {
        use crate::reviewevidence::{RowEvidence, row_approved_at_current_patch};
        use rhapsody_store::{REVIEW_COMPLETION_APPROVE, ReviewCompleted};

        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        let generation = gen_of(&o, "bob");
        // The completed, approving review at HEAD_A carrying the head's patch-id.
        o.store()
            .record_review_completion(
                &watch_key("bob"),
                REVIEW_STATUS_APPROVED,
                &ReviewCompleted {
                    generation,
                    sha: HEAD_A.to_string(),
                    patch_id: "pid-A".to_string(),
                    verdict: REVIEW_COMPLETION_APPROVE.to_string(),
                },
            )
            .expect("completion");

        // The F9 reset: the row's transient status goes back to `requested` (which is what the old
        // unconditional re-arm did), while the recorded completion is untouched.
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: watch_key("bob"),
                author: "alice".to_string(),
                introduced_by: "handoff".to_string(),
                status: REVIEW_STATUS_REQUESTED.to_string(),
                open: true,
                ..Default::default()
            })
            .expect("reset");
        let row = o
            .store()
            .get_review_watch(&watch_key("bob"))
            .expect("read")
            .expect("row");
        assert_eq!(
            row.status, REVIEW_STATUS_REQUESTED,
            "the reset put the transient status back to requested"
        );
        let completed = o
            .store()
            .review_completed(&watch_key("bob"))
            .expect("read completion");
        let ev = RowEvidence {
            row: &row,
            completed: completed.as_ref(),
        };
        assert!(
            row_approved_at_current_patch(&ev, gen_of(&o, "bob"), "pid-A"),
            "the recorded approval still satisfies the predicate at the same patch and generation"
        );
    }

    /// **A truncated review leaves `last_completed_*` untouched.** Only a run that completed with a
    /// verdict writes the four columns; a round parked `truncated` writes none of them.
    #[test]
    fn a_truncated_review_leaves_the_completed_record_empty() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        o.store()
            .mark_review_requested(&watch_key("bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_truncated(&watch_key("bob"))
            .expect("truncated");
        let completed = o.store().review_completed(&watch_key("bob")).expect("read");
        assert!(
            completed.is_none(),
            "a truncated round records no completed review, got {completed:?}"
        );
    }

    /// The evidence revision moves when the HEAD moves, and the first observation after a restart is
    /// a baseline (not a change). In-memory only — no `gh`.
    #[test]
    fn the_evidence_revision_moves_on_a_head_change_and_baselines_after_a_restart() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let pr = introduced("makewhatis", "rhapsody", 12, &["bob"]);
        o.handle_review_introduce(&pr);
        let rows = o.store().load_live_review_watch().expect("read");
        let key = crate::reviewwatch::churn_key(&pr.pr);

        o.record_review_evidence(&pr.pr, &rows, HEAD_A, Some(false), Some(false));
        let after_first = o
            .store()
            .review_bound(&key)
            .expect("read")
            .expect("bound")
            .evidence_rev;
        // Same inputs: no change.
        o.record_review_evidence(&pr.pr, &rows, HEAD_A, Some(false), Some(false));
        assert_eq!(
            o.store()
                .review_bound(&key)
                .expect("read")
                .expect("bound")
                .evidence_rev,
            after_first,
            "an unchanged observation does not move the revision"
        );
        // A moved head: increments.
        o.record_review_evidence(&pr.pr, &rows, HEAD_B, Some(false), Some(false));
        assert_eq!(
            o.store()
                .review_bound(&key)
                .expect("read")
                .expect("bound")
                .evidence_rev,
            after_first + 1,
            "a head change moves the revision"
        );
        // A restart: the in-memory fingerprint is gone, so the first observation is a baseline.
        let rev_before = after_first + 1;
        o.review_evidence.clear();
        o.record_review_evidence(&pr.pr, &rows, HEAD_B, Some(false), Some(false));
        assert_eq!(
            o.store()
                .review_bound(&key)
                .expect("read")
                .expect("bound")
                .evidence_rev,
            rev_before,
            "a restart re-baselines rather than treating everything as changed"
        );
    }
}
