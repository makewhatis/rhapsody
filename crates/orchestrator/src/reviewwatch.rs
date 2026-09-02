//! reviewwatch — the edge-triggered watcher that turns a watch-set row into a review run
//! (STUDIO-721, slice 5 of the design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`,
//! §14.1, §14.4).
//!
//! **No Go counterpart.** Ticketless review is a Rhapsody addition end to end; this is the slice
//! that makes it fire. Slice 6 fills the watch set, slice 3 knows how to dispatch one review, slice
//! 4 knows how to wind one down — nothing until now decided WHEN.
//!
//! # The edge trigger is the whole design (§14.1 F-DUP)
//!
//! The obvious rule — "review whenever `head != last_reviewed_sha` and the pull request is open" —
//! is level-triggered, and level-triggered is a duplicate-dispatch machine. `last_reviewed_sha` is
//! written only at COMPLETION, so from introduction until the first review finishes the condition
//! is true on every tick: the watcher would dispatch a second agent onto the first one's detached
//! worktree, overwrite the live `running` entry (losing the cancel handle, so the first run can
//! never be stopped) and have the first exit dropped by the stale-guard.
//!
//! So the trigger is an EDGE, and it has three parts:
//!
//! * `requested_sha`, written at DISPATCH by [`Orchestrator::dispatch_review`], is the record that
//!   this head has already been asked about. A head equal to it fires nothing.
//! * `last_reviewed_sha`, written at COMPLETION, is the record that this head was actually READ.
//! * the live `running`/`claimed` sets are the record that a review is happening RIGHT NOW.
//!
//! [`review_round_due`] is those three facts and nothing else, and it is deliberately written as a
//! match over the row's `status` rather than a SHA comparison alone, because the two states that
//! still owe a review of the SAME head are invisible to a SHA comparison:
//!
//! * a row `in_flight` at a head with **no live run** is a CRASHED round — the exit path leaves the
//!   marker exactly where the dispatch put it (§14.1, "clear on crash"), so this is where it gets
//!   cleared, which is what re-surfaces a crashed review without a daemon restart;
//! * a row `truncated` is a round whose agent burned its whole turn budget without finishing
//!   (STUDIO-721's carried slice-4 nit) — the head was read partially at best, and
//!   `last_reviewed_sha` was deliberately not advanced, so nothing but the status distinguishes it
//!   from a row nobody has looked at.
//!
//! # Reviewer selection is re-made at DISPATCH, not trusted from introduction (§14.2)
//!
//! The row names a reviewer, but that name was chosen when the pull request was introduced, from
//! whatever the roster looked like then — and under `review.mode: ticketless` it was chosen against
//! an empty load map, because `quorum_load` is filled by `record_quorum_state`, which returns early
//! when the ticket fan-out is off. Two pull requests introduced in one tick therefore name the same
//! teammate. [`Orchestrator::choose_review_reviewer`] re-decides from a LIVE
//! [`LoadSnapshot`](crate::teams::LoadSnapshot) over `running` — which counts review runs, since
//! they are dispatched wearing the reviewer's identity — and honours each identity's
//! `max_concurrent`.
//!
//! Decision B ("prefer the same reviewer on re-review") is honoured where it means something: a row
//! that HAS a last round keeps its reviewer unless they are at capacity, because continuity is
//! worth something only to somebody who read the previous round. A row that has never been reviewed
//! has no continuity to preserve, so it is selected fresh.
//!
//! # Off the loop, then back onto it (§5, F3)
//!
//! Asking GitHub where a pull request stands is a `gh` call, and [`crate::ghsummons::GH`] shells out
//! through a synchronous `std::process::Command`. [`run_review_watch_task`] owns every one of them,
//! holds no `Orchestrator` and takes no lock the control task takes — the same structural
//! containment [`crate::prstate`] was built for and documents. What comes back crosses to the
//! control task as ONE [`Event::ReviewSweep`], where the watch set stays single-writer beside
//! `dispatch_review` and `handle_review_introduce`, and where `running`/`claimed` can be read
//! without a race.
//!
//! # Teams-gating (§16)
//!
//! Both loop-side handlers short-circuit on `review_ticketless_enabled()`, the task is only spawned
//! on that same condition, and `sweep_pr_states` refuses to spawn a process with Teams off. A
//! Teams-off daemon asks GitHub nothing and writes nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use rhapsody_config::teams::Teams;
use rhapsody_store::{
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_DROPPED, REVIEW_STATUS_REVIEWED, ReviewWatchRow,
};

use crate::control_loop::{CancelWait, Event};
use crate::ghsummons::{HeadAllowlist, PrLookup, PrStateSource, PrStatus};
use crate::orchestrator::Orchestrator;
use crate::prstate::{PrCoord, PrObservation, sweep_pr_states};
use crate::review::{ReviewDispatchOutcome, ReviewRun, review_key};
use crate::stop::ControlHandle;
use crate::teams::{LoadSnapshot, at_capacity};

/// How many review ROUNDS one pull request may be given, ever, in one daemon lifetime — the floor
/// against force-push churn (§14.2, "no approval terminal → unbounded re-review").
///
/// The edge trigger already bounds the RATE: a round cannot start while one is in flight, so a
/// pull request costs at most one review per review's duration however fast its author pushes. What
/// it does not bound is the TOTAL, and an author amending in a loop — a rebase chain, a CI-driven
/// force-push, a `--fixup` habit — would otherwise buy a full agent run per amendment forever.
/// Eight rounds is far above any honest review conversation (a review, fixes, a re-review, more
/// fixes) and far below a runaway.
///
/// Deliberately in memory rather than a column: it is a churn floor, not an audit record, and the
/// churn it guards against happens over minutes inside one daemon lifetime. A restart resets it,
/// which is the correct outcome for an operator who restarted the daemon to unstick something.
pub const REVIEW_ROUNDS_PER_PR_CAP: usize = 8;

/// What one watcher tick did, reported rather than logged-and-forgotten so a caller (and a test)
/// can tell the four outcomes apart — they look identical in a silent no-op.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewSweepReport {
    /// Review runs dispatched this tick.
    pub dispatched: usize,
    /// (PR, reviewer) rows dropped out of the watch set — merged, closed, gone, or a head this
    /// daemon is not entitled to read.
    pub retired: usize,
    /// Rows that WANT a review and did not get one: nobody was available under `max_concurrent`,
    /// the pull request hit [`REVIEW_ROUNDS_PER_PR_CAP`], or the repository is no longer
    /// configured. Every one of them is re-considered next tick.
    pub deferred: usize,
    /// Rows re-armed to `requested` by the head-advance signal (design §14.1's in-process Event,
    /// standing in for the room post it forbids).
    pub armed: usize,
}

/// Delivers the watcher's two control-task round-trips. A trait for [`ReviewIntroSink`]'s reason:
/// the task must be testable without a control loop, and the seam is what lets a test assert on the
/// coordinates handed over rather than on a side effect two hops away.
///
/// [`ReviewIntroSink`]: crate::reviewintro::ReviewIntroSink
#[async_trait]
pub trait ReviewWatchSink: Send + Sync {
    /// The pull requests worth asking GitHub about this tick.
    async fn watched(&self) -> Vec<PrCoord>;
    /// Hands one tick's observations to the control task and reports what it decided.
    async fn sweep(&self, observed: Vec<PrObservation>) -> ReviewSweepReport;
}

/// The production [`ReviewWatchSink`]: the control channel, through the same [`ControlHandle`] seam
/// every other off-loop→loop hand-back uses.
pub struct ControlWatchSink {
    control: ControlHandle,
}

impl ControlWatchSink {
    pub fn new(control: ControlHandle) -> ControlWatchSink {
        ControlWatchSink { control }
    }
}

#[async_trait]
impl ReviewWatchSink for ControlWatchSink {
    async fn watched(&self) -> Vec<PrCoord> {
        self.control.review_watch_list().await
    }
    async fn sweep(&self, observed: Vec<PrObservation>) -> ReviewSweepReport {
        self.control.review_sweep(observed).await
    }
}

/// Everything [`run_review_watch_task`] runs against. No `Orchestrator`, no store and no control
/// channel of its own — the off-loop guarantee, in the type.
pub struct ReviewWatchDeps {
    /// Resolves a pull request's head SHA and state by NUMBER. `None` disables the watcher
    /// entirely: a daemon that cannot ask GitHub where a pull request stands has no honest way to
    /// decide anything, and acting on a stale row is exactly the lost update F-SHA describes.
    pub pr_source: Option<Arc<dyn PrStateSource>>,
    /// The head repositories a watched pull request may come from besides the base's own owner.
    pub allow: HeadAllowlist,
    /// The Teams config the §16 gate reads. A snapshot, like every other off-loop task's.
    pub teams: Teams,
    /// Where a tick's observations are handed back to the control task.
    pub sink: Arc<dyn ReviewWatchSink>,
}

/// Polls the watch set on [`PR_STATE_POLL_INTERVAL`](crate::prstate::PR_STATE_POLL_INTERVAL) until
/// `ctx` is cancelled.
///
/// Sleeps BEFORE its first tick, deliberately: the daemon's own boot recovery has to load config
/// and rebuild `running` first, and a tick that arrived before either would refuse everything and
/// achieve nothing but a burst of `gh` calls at start-up.
pub async fn run_review_watch_task(mut ctx: CancelWait, deps: ReviewWatchDeps) {
    let Some(src) = deps.pr_source.as_ref() else {
        tracing::info!(
            "ticketless review watcher: no GitHub source, so no pull request can be watched"
        );
        return;
    };
    tracing::info!(
        interval_secs = crate::prstate::PR_STATE_POLL_INTERVAL.as_secs(),
        "ticketless review watcher started (off-loop; the control task is never blocked on gh)"
    );
    loop {
        tokio::select! {
            _ = ctx.cancelled() => return,
            () = tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL) => {}
        }
        let prs = deps.sink.watched().await;
        if prs.is_empty() {
            continue;
        }
        let sweep = sweep_pr_states(&ctx, &deps.teams, src.as_ref(), &deps.allow, &prs).await;
        if sweep.deferred > 0 || sweep.failed > 0 {
            tracing::debug!(
                observed = sweep.observed.len(),
                budget_deferred = sweep.deferred,
                failed = sweep.failed,
                "ticketless review watcher: not every watched pull request answered this tick"
            );
        }
        if sweep.observed.is_empty() {
            continue;
        }
        let report = deps.sink.sweep(sweep.observed).await;
        if report != ReviewSweepReport::default() {
            tracing::info!(
                dispatched = report.dispatched,
                retired = report.retired,
                deferred = report.deferred,
                armed = report.armed,
                "ticketless review watcher tick"
            );
        }
    }
}

/// Whether this (PR, reviewer) row still owes a review OF `head` — the edge trigger, and the one
/// place the three facts that decide it are combined.
///
/// `in_flight_now` is whether a run for this exact key is live (`running` or `claimed`), which only
/// the control task can answer; it is a parameter rather than a lookup so the rule itself stays a
/// pure function a test can drive through every state.
pub(crate) fn review_round_due(row: &ReviewWatchRow, head: &str, in_flight_now: bool) -> bool {
    if !row.open || row.status == REVIEW_STATUS_DROPPED || head.is_empty() {
        return false;
    }
    // A healthy live review of this pair. Dispatching a second one overwrites its `running` entry
    // and points a second agent at its detached worktree (§14.1 F-DUP) — the single most damaging
    // thing this module can get wrong.
    if in_flight_now {
        return false;
    }
    match row.status.as_str() {
        // A round FINISHED at a head. Both terminals pause re-review identically while the pull
        // request sits at that head — which is §15-c's "approved-pauses" — and both re-arm the
        // moment the author pushes something nobody has read.
        REVIEW_STATUS_REVIEWED | REVIEW_STATUS_APPROVED => row.last_reviewed_sha != head,
        // Everything else still owes a review of this head, INCLUDING at the same SHA: `requested`
        // was never dispatched, `in_flight` without a live run is a crashed round, and `truncated`
        // is a round that ran out of turns mid-review. The last two are why this is a status match
        // and not a SHA comparison — a SHA comparison calls all three "already handled".
        _ => true,
    }
}

/// The per-PR churn key: `owner/repo#number`, case-folded so two spellings of one repository cannot
/// each get their own budget.
fn churn_key(pr: &PrCoord) -> String {
    format!("{}/{}#{}", pr.owner, pr.repo, pr.number).to_ascii_lowercase()
}

impl Orchestrator {
    /// The pull requests the watcher asks GitHub about this tick: every distinct coordinate the
    /// watch set still considers live.
    ///
    /// Distinct by coordinate rather than by row: N reviewers of one pull request share one head,
    /// and asking GitHub N times for it would spend the per-tick call budget on an answer already
    /// in hand.
    pub(crate) fn review_watch_coords(&self) -> Vec<PrCoord> {
        if !self.review_ticketless_enabled() {
            return Vec::new(); // §16
        }
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "ticketless review: the watch set could not be read; nothing is polled this tick");
                return Vec::new();
            }
        };
        let mut seen: HashSet<(String, String, i64)> = HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            if !row.open || row.status == REVIEW_STATUS_DROPPED {
                continue;
            }
            let k = (
                row.key.owner.to_ascii_lowercase(),
                row.key.repo.to_ascii_lowercase(),
                row.key.number,
            );
            if seen.insert(k) {
                out.push(PrCoord::new(&row.key.owner, &row.key.repo, row.key.number));
            }
        }
        out
    }

    /// Turns one tick's observations into drops, re-arms and review dispatches. **The watcher's
    /// whole decision**, on the control task, where the watch set is single-writer and
    /// `running`/`claimed` cannot race.
    pub(crate) fn handle_review_sweep(&mut self, observed: &[PrObservation]) -> ReviewSweepReport {
        let mut report = ReviewSweepReport::default();
        if !self.review_ticketless_enabled() {
            return report; // §16
        }
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "ticketless review: the watch set could not be read; this tick decides nothing");
                return report;
            }
        };
        for obs in observed {
            match &obs.lookup {
                // GitHub cannot resolve it any more: deleted, transferred, or never there. Nothing
                // is left to observe, so it leaves the watch set for good.
                PrLookup::Gone => report.retired += self.retire_review_pr(&obs.pr, "gone"),
                // The head repository is neither the base's nor allowlisted. A review of it would
                // check out and execute a stranger's code (§14.1 F-SEC), so it can never be
                // dispatched — and re-asking every tick forever is not a plan.
                PrLookup::Untrusted => {
                    report.retired += self.retire_review_pr(&obs.pr, "untrusted head repository")
                }
                PrLookup::Found(snap) if snap.status != PrStatus::Open => {
                    let why = if snap.status == PrStatus::Merged {
                        "merged"
                    } else {
                        "closed"
                    };
                    report.retired += self.retire_review_pr(&obs.pr, why);
                }
                PrLookup::Found(snap) => {
                    self.service_review_pr(&rows, &obs.pr, &snap.head_sha, &mut report)
                }
            }
        }
        report
    }

    /// Drops every live row of one pull request out of the watch set and forgets its churn budget.
    /// Returns how many rows were dropped.
    fn retire_review_pr(&mut self, pr: &PrCoord, why: &str) -> usize {
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(pr = %pr, err = %e, "ticketless review: the watch set could not be read; nothing was dropped");
                return 0;
            }
        };
        let mut dropped = 0usize;
        for row in rows {
            if !row_is(&row, pr) || (!row.open && row.status == REVIEW_STATUS_DROPPED) {
                continue;
            }
            match self.store().drop_review_watch(&row.key) {
                Ok(()) => dropped += 1,
                Err(e) => {
                    tracing::warn!(review = %review_key(&row.key.owner, &row.key.repo, row.key.number, &row.key.reviewer), err = %e, "ticketless review: dropping the watch row failed")
                }
            }
        }
        if dropped > 0 {
            tracing::info!(pr = %pr, reason = why, rows = dropped, "ticketless review: pull request dropped from the watch set");
        }
        // A dropped pull request can be re-introduced later; its old churn budget should not follow
        // it, and leaving the entry would grow this map for the daemon's whole life.
        self.review_rounds.remove(&churn_key(pr));
        dropped
    }

    /// Services one OPEN pull request at `head`: re-arms whatever the advance re-armed, then
    /// dispatches a review round for every row that still owes one.
    fn service_review_pr(
        &mut self,
        rows: &[ReviewWatchRow],
        pr: &PrCoord,
        head: &str,
        report: &mut ReviewSweepReport,
    ) {
        if head.is_empty() {
            return; // an answer with no head is not an answer about a head
        }
        // The design's in-process re-review signal (§14.1 F-SEC's fix for the room post §13.1 had):
        // rows whose head has moved past what they recorded are parked back at `requested`, so the
        // console and the room read the same fact the dispatch below acts on. It can only ever
        // touch rows that already exist, and it changes no field this function's decision reads —
        // `review_round_due` answers identically before and after it — which is why `rows` (loaded
        // once for the whole tick) is still sound to decide from.
        report.armed += self.handle_review_head_advanced(pr, head);

        let mine: Vec<&ReviewWatchRow> = rows.iter().filter(|r| row_is(r, pr)).collect();
        for row in &mine {
            let id = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            let live = self.running.contains_key(&id) || self.claimed.contains(&id);
            if !review_round_due(row, head, live) {
                continue;
            }
            // The churn floor (§14.2). Checked per ROUND rather than per row so N reviewers of one
            // pull request share one budget — the cost this bounds is agent runs, not rows.
            let rounds = self.review_rounds.get(&churn_key(pr)).copied().unwrap_or(0);
            if rounds >= REVIEW_ROUNDS_PER_PR_CAP {
                tracing::debug!(
                    pr = %pr, rounds,
                    "ticketless review: the per-pull-request re-review cap is reached; no further \
                     round is dispatched until the daemon restarts or the pull request closes"
                );
                report.deferred += 1;
                continue;
            }
            // Live, not `quorum_load` — which is always empty on this path (§14.2, the load
            // finding). Rebuilt per round on purpose: a review dispatched a moment ago is already
            // in `running`, so the SECOND round of this tick sees the first one's load and picks
            // somebody else.
            let load = LoadSnapshot::from_running(&self.running);
            let peers: HashSet<&str> = mine
                .iter()
                .map(|r| r.key.reviewer.as_str())
                .filter(|name| *name != row.key.reviewer)
                .collect();
            let Some(chosen) = self.choose_review_reviewer(row, &peers, &load) else {
                tracing::debug!(
                    pr = %pr, reviewer = %row.key.reviewer,
                    "ticketless review: no teammate has capacity to review this pull request; \
                     re-considered next tick"
                );
                report.deferred += 1;
                continue;
            };
            // THE dispatch-side allowlist re-check (the slice-6 F-SEC review's item (a)). The row
            // is stored state, and a project can be disabled or repointed by a config reload
            // between introduction and now; trusting the row would let a review be dispatched
            // against a repository no configured project owns any more. Fails closed.
            let Some(repo_url) = self.review_repo_url(&row.key.owner, &row.key.repo) else {
                tracing::warn!(
                    pr = %pr,
                    "ticketless review: refusing to dispatch a review in a repository no enabled \
                     project owns; the watch row is left alone"
                );
                report.deferred += 1;
                continue;
            };
            let reassigned = chosen != row.key.reviewer;
            let run = ReviewRun {
                owner: row.key.owner.clone(),
                repo: row.key.repo.clone(),
                number: row.key.number,
                reviewer: chosen,
                author: row.author.clone(),
                // A `pr:` key resolves to no tracker ticket, so there is no team to move it in and
                // nothing to carry one for. Left empty deliberately: a non-empty `team_id` is what
                // would make the worker's hand-off auto-park call `move_issue_state("pr:…")`, a
                // guaranteed 404 on every review (design §14.2, "team_id is a red herring").
                team_id: String::new(),
                repo_url,
                head_sha: head.to_string(),
                introduced_by: row.introduced_by.clone(),
            };
            match self.dispatch_review(run) {
                ReviewDispatchOutcome::Dispatched => {
                    report.dispatched += 1;
                    if reassigned {
                        // The round moved to a substitute, so the incumbent's row leaves the watch
                        // set rather than staying beside theirs: it is the SAME required review,
                        // and two rows would make the pull request owe two of them forever —
                        // `review_round_due` would go on answering true for the incumbent at every
                        // head, for a reviewer nobody is waiting on.
                        //
                        // Retired only AFTER the dispatch succeeded. Doing it first would leave the
                        // pull request with no row at all for this required review on any refusal,
                        // and nothing would ever ask for it again.
                        tracing::info!(
                            pr = %pr, from = %row.key.reviewer,
                            "ticketless review: the round was reassigned — the incumbent was at capacity"
                        );
                        if let Err(e) = self.store().drop_review_watch(&row.key) {
                            tracing::warn!(review = %id, err = %e, "ticketless review: retiring the reassigned watch row failed");
                        }
                    }
                    let counter = self.review_rounds.entry(churn_key(pr)).or_default();
                    *counter += 1;
                    if *counter == REVIEW_ROUNDS_PER_PR_CAP {
                        tracing::warn!(
                            pr = %pr, rounds = *counter,
                            "ticketless review: this pull request has now had its whole re-review \
                             budget; further pushes will not be reviewed"
                        );
                    }
                }
                // Not a failure: something claimed the key between the check above and here, which
                // is precisely what the guard exists for. Next tick.
                ReviewDispatchOutcome::AlreadyInFlight => report.deferred += 1,
                ReviewDispatchOutcome::TeamsOff => report.deferred += 1,
                ReviewDispatchOutcome::Refused(why) => {
                    report.deferred += 1;
                    tracing::warn!(pr = %pr, reason = why, "ticketless review: the dispatch was refused");
                }
            }
        }
    }

    /// Who reviews this round: the incumbent where continuity means something, otherwise the
    /// least-loaded available non-author. `None` when nobody can take it right now, which defers
    /// the round rather than forcing it onto somebody at their cap.
    ///
    /// `peers` are the reviewers of this pull request's OTHER rows, excluded so a substitution
    /// cannot hand one teammate two of the same pull request's required reviews.
    ///
    /// **An empty `author` fails closed.** It means the row predates the column or came from a
    /// caller that did not supply one, and "nobody is the author" is the one reading that would
    /// hand a teammate their own pull request to review. So an author-less row may only ever be
    /// serviced by its incumbent — who introduction already excluded the author from being.
    fn choose_review_reviewer(
        &self,
        row: &ReviewWatchRow,
        peers: &HashSet<&str>,
        load: &LoadSnapshot,
    ) -> Option<String> {
        let teams = self.teams.as_ref()?;
        let incumbent = row.key.reviewer.as_str();
        let has_capacity = |name: &str| {
            teams
                .roster
                .iter()
                .find(|i| i.name == name)
                .is_some_and(|i| !at_capacity(i, load))
        };
        if row.author.trim().is_empty() {
            return has_capacity(incumbent).then(|| incumbent.to_string());
        }
        let candidates: Vec<String> =
            crate::quorum::rank_reviewers(teams, row.author.trim(), load.counts())
                .into_iter()
                .filter(|name| !peers.contains(name.as_str()) && has_capacity(name))
                .collect();
        // Decision B, applied where it earns its keep: a reviewer who READ the previous round knows
        // the pull request and their own findings, so they keep it unless they are at capacity. A
        // row with no last round has no such continuity, so it takes the ranking's answer — which
        // is what makes two same-tick introductions land on two different reviewers.
        if !row.last_reviewed_sha.is_empty() && candidates.iter().any(|name| name == incumbent) {
            return Some(incumbent.to_string());
        }
        candidates.into_iter().next()
    }

    /// The clone URL of the ENABLED project that owns `owner/repo`, or `None` — the watched-repo
    /// allowlist, re-checked at dispatch time against the CURRENT configuration.
    ///
    /// Returns the project's own `repo` string rather than reconstructing a URL, because
    /// `dispatch_review`'s routing matches a project by that exact string: anything else would
    /// resolve the allowlist and then fail to route.
    ///
    /// Only `projects` are searched, not the legacy top-level `repo`, for the same reason —
    /// `review_route` can only route to a project, so accepting a top-level binding here would
    /// promise a dispatch that the next line refuses.
    fn review_repo_url(&self, owner: &str, repo: &str) -> Option<String> {
        let eff = self.eff.as_ref()?;
        eff.projects
            .iter()
            .find(|p| {
                !p.disabled
                    && crate::ghsummons::parse_repo(&p.repo).is_some_and(|(o, r)| {
                        o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
                    })
            })
            .map(|p| p.repo.clone())
    }
}

/// Whether a watch row belongs to `pr`. Case-insensitive, because GitHub logins and repository
/// names are.
fn row_is(row: &ReviewWatchRow, pr: &PrCoord) -> bool {
    row.key.owner.eq_ignore_ascii_case(&pr.owner)
        && row.key.repo.eq_ignore_ascii_case(&pr.repo)
        && row.key.number == pr.number
}

/// The per-pull-request re-review budget, keyed by `owner/repo#number`.
pub type ReviewRounds = HashMap<String, usize>;

impl ControlHandle {
    /// The coordinates the watcher should ask GitHub about. Empty when the subsystem is off or the
    /// control task is gone — an empty poll list, never a guess.
    pub(crate) async fn review_watch_list(&self) -> Vec<PrCoord> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewWatchList { reply: tx })
            .is_err()
        {
            return Vec::new();
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or_default(),
            _ = lifetime.cancelled() => Vec::new(),
        }
    }

    /// Hands one tick's observations to the control task, which decides every drop, re-arm and
    /// dispatch. The wait is bounded by the daemon lifetime rather than a timer, as every other
    /// off-loop hand-back here is: nothing is answering an agent's MCP call, so a busy tick should
    /// delay this tick's decisions rather than turn them into a false failure.
    pub(crate) async fn review_sweep(&self, observed: Vec<PrObservation>) -> ReviewSweepReport {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewSweep {
                observed,
                reply: tx,
            })
            .is_err()
        {
            return ReviewSweepReport::default();
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or_default(),
            _ = lifetime.cancelled() => ReviewSweepReport::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rhapsody_config::teams::{Identity, Review, ReviewMode};
    use rhapsody_store::{
        REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_TRUNCATED, ReviewWatchKey,
        Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::ghsummons::{PrSnapshot, PrStateResult};
    use crate::orchestrator::RunningEntry;
    use crate::testsupport::{DispatchedEntries, empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const OWNER: &str = "makewhatis";
    const REPO: &str = "rhapsody";
    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn ident(name: &str, max_concurrent: i64) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            max_concurrent,
            ..Identity::default()
        }
    }

    fn teams_with(enabled: bool, mode: ReviewMode, roster: Vec<Identity>) -> Teams {
        Teams {
            enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster,
            ..Teams::disabled()
        }
    }

    /// Teams on, `review.mode: ticketless`, an unlimited-capacity roster — everything the watcher
    /// gates on, with capacity out of the way unless a test puts it back.
    fn ticketless(names: &[&str]) -> Teams {
        teams_with(
            true,
            ReviewMode::Ticketless,
            names.iter().map(|n| ident(n, 0)).collect(),
        )
    }

    /// An orchestrator with one enabled project owning [`REPO_URL`], an in-memory store and a
    /// recording spawn seam — the shape `dispatch_review` needs to reach a worker.
    fn orch(teams: Teams) -> (Orchestrator, DispatchedEntries) {
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
        o.teams = Some(teams);
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&dispatched);
        o.spawn = Some(Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        }));
        (o, dispatched)
    }

    fn key(number: i64, reviewer: &str) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: OWNER.to_string(),
            repo: REPO.to_string(),
            number,
            reviewer: reviewer.to_string(),
        }
    }

    /// A freshly-introduced row: `alice` authored it, `reviewer` was asked, nothing dispatched yet.
    fn row(number: i64, reviewer: &str) -> ReviewWatchRow {
        ReviewWatchRow {
            key: key(number, reviewer),
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-721".to_string(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.to_string(),
            open: true,
        }
    }

    fn introduce(o: &Orchestrator, r: ReviewWatchRow) {
        o.store().save_review_watch(r).expect("introduce");
    }

    fn coord(number: i64) -> PrCoord {
        PrCoord::new(OWNER, REPO, number)
    }

    /// One observation of an OPEN pull request at `head`.
    fn open_at(number: i64, head: &str) -> PrObservation {
        PrObservation {
            pr: coord(number),
            lookup: PrLookup::Found(PrSnapshot {
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
            }),
        }
    }

    fn observed(number: i64, lookup: PrLookup) -> PrObservation {
        PrObservation {
            pr: coord(number),
            lookup,
        }
    }

    fn watch_row(o: &Orchestrator, number: i64, reviewer: &str) -> ReviewWatchRow {
        o.store()
            .get_review_watch(&key(number, reviewer))
            .expect("read watch row")
            .expect("row exists")
    }

    /// The reviewer each dispatched run was given, in dispatch order.
    fn reviewers_of(dispatched: &DispatchedEntries) -> Vec<String> {
        dispatched
            .lock()
            .expect("dispatched lock")
            .iter()
            .map(|re| re.identity.clone())
            .collect()
    }

    /// Ends the live review of `(number, reviewer)` as a clean, DECLARED completion at `head` —
    /// what `on_review_exit` does, without needing a worker.
    fn complete(o: &mut Orchestrator, number: i64, reviewer: &str, head: &str) {
        let id = review_key(OWNER, REPO, number, reviewer);
        o.running.remove(&id);
        o.claimed.remove(&id);
        o.store()
            .mark_review_completed(&key(number, reviewer), head, REVIEW_STATUS_REVIEWED)
            .expect("complete");
    }

    /// Ends the live review of `(number, reviewer)` the way a CRASH does: the run is gone from
    /// `running`, and the watch row is left exactly where the dispatch put it (`in_flight` at its
    /// requested SHA) — the exit path deliberately does not clear it.
    fn crash(o: &mut Orchestrator, number: i64, reviewer: &str) {
        let id = review_key(OWNER, REPO, number, reviewer);
        o.running.remove(&id);
        o.claimed.remove(&id);
    }

    // --- the edge trigger -------------------------------------------------------------------

    /// Acceptance: a healthy in-flight review does NOT re-fire each tick. Level-triggered, this is
    /// the F-DUP double dispatch — a second agent on the first one's detached worktree, and the
    /// first one's cancel handle lost.
    #[test]
    fn a_healthy_in_flight_review_does_not_re_fire() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        let first = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(first.dispatched, 1);
        assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_IN_FLIGHT);

        // Three more ticks at the same head, with the review still running.
        for _ in 0..3 {
            let again = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
            assert_eq!(again.dispatched, 0, "a live review was dispatched again");
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    /// Acceptance: a head advance fires EXACTLY ONE re-review — not one per tick.
    #[test]
    fn a_head_advance_fires_exactly_one_re_review() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        complete(&mut o, 12, "bob", HEAD_A);
        // Same head after the completion: nothing to do.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 0);

        // The author pushes. One review of the new head, and only one however long it takes.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);
        for _ in 0..3 {
            assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
        assert_eq!(watch_row(&o, 12, "bob").requested_sha, HEAD_B);
    }

    /// Acceptance: a crashed review re-surfaces WITHOUT a daemon restart. The exit path leaves the
    /// `in_flight` marker in place on purpose; "no live run for an in-flight row" is what clears it.
    #[test]
    fn a_crashed_review_re_surfaces_without_a_restart() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        crash(&mut o, 12, "bob");
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_IN_FLIGHT,
            "the crashed round's marker is the input this test is about"
        );

        // Same head, same tick cadence, no restart.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
        assert!(o.running.contains_key(&review_key(OWNER, REPO, 12, "bob")));
    }

    /// A review an operator STOPPED is not resurrected two minutes later. `stop_run` leaves the
    /// key in `claimed` when the (impossible for a `pr:` key) tracker move fails, which is exactly
    /// the "dead this session" suppression the edge trigger already reads — worth pinning, because
    /// the watcher is the first thing in the daemon that would re-dispatch on its own initiative.
    #[test]
    fn an_operator_stopped_review_is_not_re_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        let id = review_key(OWNER, REPO, 12, "bob");
        o.running.remove(&id);
        o.claimed.insert(id); // what `stop_run` + a failed finalize leave behind

        for _ in 0..3 {
            assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 0);
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    /// Acceptance: a `max_turns`-truncated round is re-reviewed AT THE SAME HEAD. Nothing but the
    /// status distinguishes it from a completed one — `last_reviewed_sha` was deliberately not
    /// advanced — so a SHA-only trigger would call the partial review sufficient forever.
    #[test]
    fn a_truncated_round_is_re_reviewed_at_the_same_head() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        // The agent burned its budget: non-terminal, both SHAs untouched, run gone.
        o.store()
            .mark_review_truncated(&key(12, "bob"))
            .expect("truncate");
        crash(&mut o, 12, "bob");
        assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_TRUNCATED);

        assert_eq!(
            o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched,
            1,
            "a partially-reviewed head must be reviewed again"
        );
    }

    /// The rule itself, driven through every state — the table a reader can check the prose against.
    #[test]
    fn the_edge_trigger_is_a_status_rule_not_a_sha_comparison() {
        let at = |status: &str, requested: &str, reviewed: &str| ReviewWatchRow {
            status: status.to_string(),
            requested_sha: requested.to_string(),
            last_reviewed_sha: reviewed.to_string(),
            ..row(12, "bob")
        };
        // Live review: never, whatever the row says.
        for status in [
            REVIEW_STATUS_REQUESTED,
            REVIEW_STATUS_IN_FLIGHT,
            REVIEW_STATUS_TRUNCATED,
            REVIEW_STATUS_REVIEWED,
        ] {
            assert!(
                !review_round_due(&at(status, HEAD_B, ""), HEAD_B, true),
                "{status}"
            );
        }
        // Not live: reviewed/approved pause at the head they read and re-arm past it…
        assert!(!review_round_due(
            &at(REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A),
            HEAD_B,
            false
        ));
        assert!(!review_round_due(
            &at(REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A),
            HEAD_B,
            false
        ));
        // …and the three unfinished states owe a review of the SAME head.
        assert!(review_round_due(
            &at(REVIEW_STATUS_REQUESTED, "", ""),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_IN_FLIGHT, HEAD_A, ""),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_TRUNCATED, HEAD_A, ""),
            HEAD_A,
            false
        ));
        // A row that left the watch set is never due, and neither is a headless observation.
        let dropped = ReviewWatchRow {
            open: false,
            ..at(REVIEW_STATUS_DROPPED, HEAD_A, HEAD_A)
        };
        assert!(!review_round_due(&dropped, HEAD_B, false));
        assert!(!review_round_due(
            &at(REVIEW_STATUS_REQUESTED, "", ""),
            "",
            false
        ));
    }

    // --- the drop terminal ------------------------------------------------------------------

    /// Acceptance: a merged, closed or gone pull request is dropped from the watch set — and so is
    /// one whose head this daemon is not entitled to read.
    #[test]
    fn a_retired_pull_request_leaves_the_watch_set() {
        let merged = PrLookup::Found(PrSnapshot {
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Merged,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
        });
        let closed = PrLookup::Found(PrSnapshot {
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Closed,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
        });
        for (n, lookup) in [
            (12, merged),
            (13, closed),
            (14, PrLookup::Gone),
            (15, PrLookup::Untrusted),
        ] {
            let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
            introduce(&o, row(n, "bob"));

            let report = o.handle_review_sweep(&[observed(n, lookup.clone())]);

            assert_eq!(report.retired, 1, "pr #{n}");
            assert_eq!(report.dispatched, 0, "pr #{n}");
            let r = watch_row(&o, n, "bob");
            assert_eq!(r.status, REVIEW_STATUS_DROPPED, "pr #{n}");
            assert!(!r.open, "pr #{n}");
            assert!(dispatched.lock().expect("lock").is_empty(), "pr #{n}");
            // …and it is not polled again.
            assert!(o.review_watch_coords().is_empty(), "pr #{n}");
        }
    }

    /// Every reviewer of an N-reviewer pull request is dropped, not just the first — and dropping
    /// is idempotent, so a second observation of a merged PR is not a second retirement.
    #[test]
    fn retiring_drops_every_reviewer_once() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));

        assert_eq!(
            o.handle_review_sweep(&[observed(12, PrLookup::Gone)])
                .retired,
            2
        );
        assert_eq!(
            o.handle_review_sweep(&[observed(12, PrLookup::Gone)])
                .retired,
            0
        );
    }

    // --- load-aware reviewer selection ------------------------------------------------------

    /// Acceptance: two review requests in ONE tick pick two different reviewers. This is the load
    /// finding: introduction ranks both rows against a load map that is always empty on the
    /// ticketless path, so both name the same teammate; the watcher re-decides from live runs, and
    /// the first dispatch of the tick is already in `running` when the second is decided.
    #[test]
    fn two_review_requests_in_one_tick_pick_two_different_reviewers() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        // Both introduced naming `bob` — exactly what the degenerate ranking produces.
        introduce(&o, row(12, "bob"));
        introduce(&o, row(13, "bob"));

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A), open_at(13, HEAD_A)]);

        assert_eq!(report.dispatched, 2);
        let mut picked = reviewers_of(&dispatched);
        picked.sort();
        assert_eq!(
            picked,
            vec!["bob".to_string(), "carol".to_string()],
            "the second round must see the first one's load and pick somebody else"
        );
        assert!(
            !picked.contains(&"alice".to_string()),
            "the author must never be handed their own pull request"
        );
    }

    /// Acceptance: a capped / at-max reviewer is skipped to the next best. `bob` has read a round
    /// already (decision B would keep him) but is at his `max_concurrent`, so the round is
    /// reassigned — and the reassigned row leaves the watch set rather than sitting beside the
    /// substitute's, which would make the pull request owe two reviews forever.
    #[test]
    fn a_capped_reviewer_is_skipped_to_the_next_best() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("bob", 1), ident("carol", 0)],
        ));
        introduce(&o, row(12, "bob"));
        // bob reviewed #12 once, and is now busy with something else.
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(report.dispatched, 1);
        assert_eq!(reviewers_of(&dispatched), vec!["carol".to_string()]);
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_DROPPED,
            "the reassigned row must leave the watch set"
        );
        assert_eq!(watch_row(&o, 12, "carol").requested_sha, HEAD_B);
    }

    /// Decision B: a reviewer who READ the previous round keeps the pull request while they have
    /// capacity, even when somebody else is idler.
    #[test]
    fn a_re_review_prefers_the_reviewer_who_read_the_last_round() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");
        // `carol` is idle and ranks first on load; continuity must still win.
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(reviewers_of(&dispatched), vec!["bob".to_string()]);
    }

    /// Everybody is at capacity: the round is DEFERRED, not forced onto somebody over their cap and
    /// not silently lost — the next tick considers it again.
    #[test]
    fn a_round_nobody_can_take_is_deferred_and_reconsidered() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("bob", 1)],
        ));
        introduce(&o, row(12, "bob"));
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());

        // bob frees up; the same row is picked up with no new introduction.
        o.running.remove("iss-9");
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
    }

    /// An author-less row (written before the column existed, or by a caller that supplied none)
    /// may only be serviced by its INCUMBENT: with no author to exclude, any substitution could
    /// hand a teammate their own pull request.
    #[test]
    fn an_author_less_row_never_substitutes() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("bob", 1), ident("carol", 0)],
        ));
        introduce(
            &o,
            ReviewWatchRow {
                author: String::new(),
                ..row(12, "bob")
            },
        );
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    // --- the approval terminal and the churn floor ------------------------------------------

    /// Acceptance (§15-c): an approved pull request stops re-reviewing while it stays open, and a
    /// subsequent push re-arms EXACTLY ONE review of the new changes.
    #[test]
    fn an_approved_pull_request_pauses_and_a_push_re_arms_exactly_one() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        o.running.remove(&review_key(OWNER, REPO, 12, "bob"));
        o.claimed.remove(&review_key(OWNER, REPO, 12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_APPROVED)
            .expect("approve");

        for _ in 0..3 {
            assert_eq!(
                o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched,
                0,
                "an approved pull request must stop re-reviewing while it stays at that head"
            );
        }
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
    }

    /// Acceptance: a churning pull request hits the cap. Each round completes instantly and the
    /// author pushes again — the shape a force-push loop produces — and the budget is finite.
    #[test]
    fn a_churning_pull_request_hits_the_cap() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        for round in 0..(REVIEW_ROUNDS_PER_PR_CAP + 4) {
            let head = format!("{round:040}");
            o.handle_review_sweep(&[open_at(12, &head)]);
            complete(&mut o, 12, "bob", &head);
        }

        assert_eq!(
            dispatched.lock().expect("lock").len(),
            REVIEW_ROUNDS_PER_PR_CAP,
            "the per-pull-request re-review budget must be finite"
        );
        let over = o.handle_review_sweep(&[open_at(12, HEAD_B)]);
        assert_eq!((over.dispatched, over.deferred), (0, 1));
    }

    /// The budget belongs to a pull request, not to the daemon: a second pull request gets its own,
    /// and a retired one gives its entry back.
    #[test]
    fn the_churn_budget_is_per_pull_request_and_released_on_retirement() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), Some(&1));

        introduce(&o, row(13, "bob"));
        complete(&mut o, 12, "bob", HEAD_A);
        o.handle_review_sweep(&[open_at(13, HEAD_A)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#13"), Some(&1));

        o.handle_review_sweep(&[observed(12, PrLookup::Gone)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), None);
    }

    // --- N > 1 --------------------------------------------------------------------------------

    /// Acceptance (N>1): a crashed SECOND reviewer's review is re-dispatched — the pull request is
    /// not "reviewed" until every required reviewer has recorded the SHA. Per-(PR, reviewer) rows
    /// are what make that true: the first completer cannot stamp the pull request done.
    #[test]
    fn a_crashed_second_reviewer_is_re_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));

        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 2);
        // bob finishes; carol crashes.
        complete(&mut o, 12, "bob", HEAD_A);
        crash(&mut o, 12, "carol");

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(
            report.dispatched, 1,
            "the crashed second reviewer's round must be re-dispatched at the same head"
        );
        assert_eq!(
            reviewers_of(&dispatched),
            vec!["bob".to_string(), "carol".to_string(), "carol".to_string()],
            "and bob, who finished, must not be asked again"
        );
    }

    /// Two reviewers of one pull request are never the same teammate: a substitution excludes the
    /// other rows' reviewers, so a capped incumbent cannot be replaced by their own peer.
    #[test]
    fn a_substitution_never_doubles_up_one_reviewer() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![
                ident("alice", 0),
                ident("bob", 1),
                ident("carol", 0),
                ident("dave", 0),
            ],
        ));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        let mut picked = reviewers_of(&dispatched);
        picked.sort();
        assert_eq!(picked, vec!["carol".to_string(), "dave".to_string()]);
    }

    // --- gating and the dispatch-side allowlist ----------------------------------------------

    /// Acceptance: Teams off, or a mode that is not `ticketless`, and the watcher is dormant — it
    /// polls nothing and decides nothing, even with a watch set full of live rows.
    #[test]
    fn the_watcher_is_dormant_off_the_ticketless_path() {
        for teams in [
            teams_with(false, ReviewMode::Ticketless, vec![ident("bob", 0)]),
            teams_with(true, ReviewMode::Off, vec![ident("bob", 0)]),
            teams_with(true, ReviewMode::Tickets, vec![ident("bob", 0)]),
        ] {
            let (mut o, dispatched) = orch(teams.clone());
            introduce(&o, row(12, "bob"));

            assert!(o.review_watch_coords().is_empty(), "{teams:?}");
            assert_eq!(
                o.handle_review_sweep(&[open_at(12, HEAD_A)]),
                ReviewSweepReport::default(),
                "{teams:?}"
            );
            assert!(dispatched.lock().expect("lock").is_empty(), "{teams:?}");
            assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_REQUESTED);
        }
    }

    /// Added acceptance (a): the dispatch-side resolver re-checks the watched-repo allowlist. A row
    /// whose repository a config reload has since disabled is NOT dispatched — the stored row is
    /// never taken on trust.
    #[test]
    fn a_row_whose_repo_is_no_longer_configured_is_not_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        // The operator disables the project (or repoints it) between introduction and this tick.
        o.eff.as_mut().expect("eff").projects[0].disabled = true;

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());
        assert_eq!(
            watch_row(&o, 12, "bob").requested_sha,
            "",
            "nothing may be recorded as dispatched"
        );
        // …and the head-advance re-arm is refused for the same reason (item (a)'s other half).
        assert_eq!(report.armed, 0);
    }

    /// The same check, on its own: a coordinate no configured project owns resolves to no clone URL
    /// at all, so there is nothing for `dispatch_review` to route with.
    #[test]
    fn the_dispatch_side_allowlist_resolves_only_enabled_configured_projects() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        assert_eq!(
            o.review_repo_url(OWNER, REPO),
            Some(REPO_URL.to_string()),
            "case-insensitively, on the parsed owner/repo rather than the URL text"
        );
        assert_eq!(
            o.review_repo_url("MAKEWHATIS", "RHAPSODY"),
            Some(REPO_URL.to_string())
        );
        assert_eq!(o.review_repo_url("attacker", "evil"), None);
        o.eff.as_mut().expect("eff").projects[0].disabled = true;
        assert_eq!(o.review_repo_url(OWNER, REPO), None);
    }

    /// The poll list is the live watch set, distinct by COORDINATE: N reviewers of one pull request
    /// cost one `gh` call, not N.
    #[test]
    fn the_poll_list_is_distinct_by_coordinate() {
        let (o, _d) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        introduce(&o, row(13, "bob"));

        assert_eq!(o.review_watch_coords(), vec![coord(12), coord(13)]);
    }

    // --- the off-loop task --------------------------------------------------------------------

    /// A sink recording what the task asked for and handed back.
    struct FakeSink {
        watched: Vec<PrCoord>,
        seen: Arc<Mutex<Vec<Vec<PrObservation>>>>,
        done: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ReviewWatchSink for FakeSink {
        async fn watched(&self) -> Vec<PrCoord> {
            self.watched.clone()
        }
        async fn sweep(&self, observed: Vec<PrObservation>) -> ReviewSweepReport {
            self.seen.lock().expect("seen lock").push(observed);
            self.done.notify_one();
            ReviewSweepReport::default()
        }
    }

    struct FakeSource;

    #[async_trait]
    impl PrStateSource for FakeSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            Ok(PrLookup::Found(PrSnapshot {
                head_sha: format!("{number:040}"),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
            }))
        }
    }

    /// The task's whole shape: it asks the control task what to poll, asks GitHub about exactly
    /// that, and hands the answers back — never touching the store or the orchestrator itself.
    #[tokio::test(start_paused = true)]
    async fn the_task_polls_the_watch_set_and_hands_the_answers_back() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            teams: ticketless(&["alice", "bob"]),
            sink: Arc::new(FakeSink {
                watched: vec![coord(12), coord(13)],
                seen: Arc::clone(&seen),
                done: Arc::clone(&done),
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        done.notified().await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let seen = seen.lock().expect("seen lock");
        assert!(!seen.is_empty(), "the task never handed a tick back");
        let first = &seen[0];
        assert_eq!(
            first.iter().map(|o| o.pr.clone()).collect::<Vec<_>>(),
            vec![coord(12), coord(13)],
            "exactly the coordinates the control task named, and no others"
        );
    }

    /// §16: with Teams off the task spawns no process at all — `sweep_pr_states` refuses — so it
    /// hands nothing back however many rows a stale watch set names.
    #[tokio::test(start_paused = true)]
    async fn the_task_asks_github_nothing_with_teams_off() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            teams: teams_with(false, ReviewMode::Ticketless, vec![ident("bob", 0)]),
            sink: Arc::new(FakeSink {
                watched: vec![coord(12)],
                seen: Arc::clone(&seen),
                done: Arc::clone(&done),
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL * 3).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "a Teams-off daemon must observe nothing and decide nothing"
        );
    }
}
