//! reviewconsole — the ticketless review watch set as the AUTHENTICATED CONSOLE sees and steers it
//! (STUDIO-722, slice 8 of the design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`,
//! §14.4).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature; this is the additive
//! Rhapsody surface the design record specifies, dormant end to end unless Teams is on and the mode
//! is `ticketless` (§16).
//!
//! # Why the operator's lever lives here and not in the room
//!
//! §14.1's fatal **F-SEC** finding killed room-based control of reviews outright, and §15-e names
//! the replacement: *"Re-run/dismiss live on the console Reviews surface, never the room."* The room
//! reader ([`crate::teamsears`]) is Linear-anchored — `resolve_keys`/`validate_targets`/`find_issue`
//! all demand a fetched Linear issue, so a `pr:` key resolves to nothing there — and making it
//! understand pull requests would be a second addressing subsystem whose targets come out of
//! forgeable post text (§14.2, "room control is Linear-anchored"). So **this slice adds no `pr:`
//! room Intent at all**; the operator's controls arrive as in-process control [`Event`]s from the
//! loopback HTTP API instead, which is the trusted path §15-e means.
//!
//! [`Event`]: crate::Event
//!
//! # Trusted in-process, still re-validated
//!
//! Being an in-process type is not the same as being a validated one — the rule
//! [`crate::reviewintro`] states and this module inherits. Every handler re-checks the coordinates
//! they are handed, and [`Orchestrator::handle_review_rerun`] re-checks the watched-repo allowlist
//! as well, because a re-run is a step towards checking that repository out.
//!
//! [`Orchestrator::handle_review_dismiss`] deliberately does NOT check the allowlist, and the
//! asymmetry is the point: dismissal only ever RETIRES a row. Gating it on the allowlist would make
//! the rows left behind by a repointed or paused project — exactly the rows an operator most wants
//! gone — the only ones that could never be removed.
//!
//! # Everything is loop-confined
//!
//! All four entry points run on the control task, for the reason
//! [`Orchestrator::handle_review_introduce`] does: the watch set stays single-writer, and the
//! in-flight guard the two writers depend on reads `running`/`claimed`, which only the control task
//! owns. The read is loop-confined too, following [`crate::Event::ReviewWatchList`] — the console's
//! HTTP task never touches the store the control task writes.

use rhapsody_store::{REVIEW_STATUS_DROPPED, REVIEW_STATUS_REQUESTED, ReviewWatchRow, StoreError};
use serde::Serialize;

use crate::control_loop::Event;
use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::review::review_key;
use crate::reviewwatch::churn_key;
use crate::stop::ControlHandle;

/// One watch-set row as the console renders it: the pull request, who is reviewing it, and the
/// four facts that say where that review has got to.
///
/// A flat projection of [`ReviewWatchRow`] rather than the row itself, because the row is store
/// state whose field names are free to change and this is a wire shape the dashboard reads. The
/// nesting is dropped for the same reason `RosterRow` is flat: a table renders columns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewJobRow {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// The teammate holding this review.
    pub reviewer: String,
    /// The teammate whose handoff produced the pull request; empty ⇒ unknown.
    pub author: String,
    /// How the pull request entered the watch set, e.g. `handoff:STUDIO-720`.
    pub introduced_by: String,
    /// The head SHA a reviewer run was dispatched against; empty until one has been.
    pub requested_sha: String,
    /// The head SHA a completed review actually read; empty until this reviewer finished a round.
    pub last_reviewed_sha: String,
    /// One of the store's `REVIEW_STATUS_*` values — `requested`, `in_flight`, `reviewed`,
    /// `approved`, `truncated` or `dropped`.
    pub status: String,
    /// Whether the pull request is still open. `false` is a merged, closed, gone or dismissed row.
    pub open: bool,
}

impl From<ReviewWatchRow> for ReviewJobRow {
    fn from(row: ReviewWatchRow) -> ReviewJobRow {
        ReviewJobRow {
            owner: row.key.owner,
            repo: row.key.repo,
            number: row.key.number,
            reviewer: row.key.reviewer,
            author: row.author,
            introduced_by: row.introduced_by,
            requested_sha: row.requested_sha,
            last_reviewed_sha: row.last_reviewed_sha,
            status: row.status,
            open: row.open,
        }
    }
}

/// `GET /api/v1/reviews` — the whole watch set, and whether the subsystem is awake at all.
///
/// `enabled` is carried in the BODY rather than being inferred from an HTTP status, unlike the
/// `teams_*` routes which answer `teams_disabled` (409). The difference is what the field is FOR:
/// the console already knows whether Teams is on (`teams_enabled` on `/api/v1/version`) but has no
/// way to learn the review MODE, so this read is the surface's own capability probe and has to
/// answer to be one. A dormant daemon therefore serves `{enabled: false, reviews: []}` — the
/// "surface absent/empty" §16 asks for — and the console renders nothing rather than an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReviewsView {
    /// Teams is on AND the review mode is `ticketless` (§16). False ⇒ `reviews` is empty and no
    /// control is offered.
    pub enabled: bool,
    /// Every row, live and retired, in the store's deterministic order (owner, repo, number,
    /// reviewer). Retired rows are included because `open` and `dropped` are columns the operator
    /// reads — a dismissed pull request that simply vanished would look like one that was never
    /// there.
    pub reviews: Vec<ReviewJobRow>,
}

/// What one console control did. Modelled on [`crate::reviewintro::ReviewIntroOutcome`], with one
/// variant it does not need: a store failure is reported rather than folded into a count, because
/// an operator who clicked a button is owed the difference between "nothing matched" and "the write
/// did not land".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewControlOutcome {
    /// `n` watch-set rows were changed by this action.
    Applied(usize),
    /// Teams is off or the mode is not `ticketless`, so the subsystem is dormant (§16). Nothing was
    /// read and nothing was written.
    Dormant,
    /// The request was refused; the payload names why.
    Refused(&'static str),
    /// The watch set could not be read or written; the payload is the store's own complaint.
    Failed(String),
}

/// Coordinates every console control validates before it does anything — the "in-process is not
/// validated" rule, applied once.
fn check_coords(pr: &PrCoord) -> Option<&'static str> {
    if pr.owner.trim().is_empty() || pr.repo.trim().is_empty() {
        return Some("pull request has no owner/repo");
    }
    if pr.number <= 0 {
        return Some("pull-request number is not positive");
    }
    None
}

/// Whether a watch row is one of `pr`'s. Case-insensitive on owner and repository, because GitHub
/// logins and repository names are — the same comparison [`crate::reviewwatch`] makes.
fn row_is(row: &ReviewWatchRow, pr: &PrCoord) -> bool {
    row.key.owner.eq_ignore_ascii_case(&pr.owner)
        && row.key.repo.eq_ignore_ascii_case(&pr.repo)
        && row.key.number == pr.number
}

impl Orchestrator {
    /// The console's read of the watch set (`Event::ReviewConsoleList`), loop-confined for
    /// [`Orchestrator::review_watch_coords`]'s reason.
    ///
    /// Dormant ⇒ `enabled: false` and an EMPTY list, never the rows: a daemon whose mode was
    /// switched back to `tickets` still has whatever the watch set held, and serving those to a
    /// surface that offers no control over them would advertise a subsystem that is not running.
    pub(crate) fn review_console_list(&self) -> Result<ReviewsView, StoreError> {
        if !self.review_ticketless_enabled() {
            return Ok(ReviewsView::default()); // §16
        }
        let rows = self.store().load_review_watch()?;
        Ok(ReviewsView {
            enabled: true,
            reviews: rows.into_iter().map(ReviewJobRow::from).collect(),
        })
    }

    /// **Re-run** (`Event::ReviewRerun`) — the operator asking for one more review round of a
    /// WATCHED pull request, §15-e's trusted lever. Re-arms every live row of `pr` back to
    /// `requested`, which is the status [`crate::reviewwatch::review_round_due`] answers "due" for
    /// at any head, so the next watcher tick dispatches.
    ///
    /// It re-arms rows and introduces none, exactly like
    /// [`Orchestrator::handle_review_head_advanced`]: a pull request nobody introduced is refused
    /// rather than watched, so this control cannot become a second, weaker introduction path.
    ///
    /// Two things it does beyond a head advance, both because a human asked rather than a poller:
    ///
    /// * It re-arms **regardless of SHA**. An advance is only meaningful when the head moved; an
    ///   operator re-running a pull request nobody has pushed to is asking for a second read of the
    ///   same code, which is a coherent thing to want after a reviewer crashed or came back thin.
    /// * It **refunds ONE round of the per-pull-request churn budget**. That cap (§14.2) exists as a
    ///   floor against force-push loops and, once reached, defers every further round "until the
    ///   daemon restarts or the pull request closes" — so leaving it in place would let a capped
    ///   pull request accept this click and then silently never review. An authenticated operator IS
    ///   the escape hatch the cap defers to. A refund and not a reset, though: the operator asked
    ///   for one re-read, and clearing the counter would hand an already-runaway pull request the
    ///   whole budget again UNATTENDED, which is the exact cost the cap is there to bound.
    pub(crate) fn handle_review_rerun(&mut self, pr: &PrCoord) -> ReviewControlOutcome {
        if !self.review_ticketless_enabled() {
            return ReviewControlOutcome::Dormant; // §16
        }
        if let Some(why) = check_coords(pr) {
            return ReviewControlOutcome::Refused(why);
        }
        // The allowlist, re-checked here for `handle_review_head_advanced`'s reason: re-arming a row
        // is the first step towards checking its repository out, and the configuration can have been
        // repointed or the project paused since the row was written. Fails closed.
        //
        // It is `review_repo_is_configured` and NOT the dispatch's own `review_repo_url`, and the
        // two are not quite the same predicate: this one falls back to the top-level `cfg.repo` when
        // `eff.projects` is empty, and that one searches `eff.projects` only. Where they diverged, a
        // re-run would answer `Applied` and then defer on every tick forever — a permanently queued
        // review. They cannot diverge today, because `build_effective` fills `projects` from
        // `resolve_projects`, which synthesises an entry for the single-project case, so the
        // empty-`projects` branch is unreachable outside tests. Named here rather than silently
        // relied on: collapsing the two spellings of one allowlist is worth doing, and it belongs
        // with the predicates themselves, which predate this control.
        if !self.review_repo_is_configured(&pr.owner, &pr.repo) {
            tracing::warn!(
                pr = %pr,
                "ticketless review: refusing an operator re-run in a repository no configured \
                 project owns"
            );
            return ReviewControlOutcome::Refused("no configured project owns the PR's repo");
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => return ReviewControlOutcome::Failed(e.to_string()),
        };
        let mine: Vec<ReviewWatchRow> = rows.into_iter().filter(|r| row_is(r, pr)).collect();
        if mine.is_empty() {
            return ReviewControlOutcome::Refused("no live review of that pull request is watched");
        }
        let mut armed = 0usize;
        for row in mine {
            let id = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            // A review of this exact (PR, reviewer) is live. Re-arming its row would overwrite the
            // `in_flight` marker the F-DUP edge trigger reads, and the watcher would then point a
            // second agent at the first one's detached worktree — the single most damaging thing
            // this subsystem can do. The round the operator wants is already running.
            if self.running.contains_key(&id) || self.claimed.contains(&id) {
                tracing::debug!(
                    review = %id,
                    "ticketless review: a review of this pull request is already in flight; the \
                     operator's re-run leaves its watch row alone"
                );
                continue;
            }
            // Already owes a review of whatever the head turns out to be, so there is nothing to
            // arm. Counted anyway: `armed` answers the operator's question — "will this be reviewed
            // again" — and a row that was already going to be is a yes. Not written, because
            // rewriting a row to the status it already holds is a store write that changes nothing.
            if row.status == REVIEW_STATUS_REQUESTED {
                armed += 1;
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
                    tracing::warn!(review = %id, err = %e, "ticketless review: the operator's re-run could not re-arm the watch row")
                }
            }
        }
        if armed > 0 {
            // One ROUND back, in the dispatches the counter is kept in — the same scaling
            // `service_review_pr` applies to the cap, so a two-reviewer config gets a two-dispatch
            // round back rather than half of one. Read from the one helper that defines the unit, so
            // the refund and the charge can never disagree. Saturating: a counter below one round's
            // cost just returns to zero.
            let round = self.reviewers_per_round();
            let key = churn_key(pr);
            if let Some(spent) = self.review_rounds.get_mut(&key) {
                *spent = spent.saturating_sub(round);
                // The refund is durable too (STUDIO-956): a re-run whose refunded round only
                // existed in memory would be undone by the next restart, which is the same defect
                // as the charge only existing in memory — in the operator's face rather than the
                // budget's.
                self.persist_review_rounds(&key);
            }
            // The operator's re-run overrides a manager adjudication too (STUDIO-956): otherwise a
            // settled `ship`/`escalate` would keep the loop stopped and the refunded round would
            // never dispatch.
            if let Some(ledger) = self.adjudication_ledger.as_ref() {
                ledger.clear(pr);
            }
            tracing::info!(pr = %pr, rows = armed, "ticketless review: operator re-ran a review");
        }
        ReviewControlOutcome::Applied(armed)
    }

    /// **Clear the round budget** (`Event::ReviewClear`) — the operator's deliberate reset of a
    /// pull request's review round budget — and any manager adjudication of it (STUDIO-956).
    ///
    /// §15-e's third lever, and the answer to that lever's own failure mode. A spent budget defers
    /// every further review AND every author re-dispatch "until the daemon restarts or the pull
    /// request closes" — which is a bound an operator cannot lift in place, and which is why three
    /// pull requests sat unreviewable until an upgrade forced a restart. Re-run *refunds one round*
    /// (its own test pins that), which is the right size for a pull request the cap merely reached;
    /// this is for the one an operator has decided the budget itself was wrong about.
    ///
    /// It clears the COUNTER and touches no row: unlike re-run it does not re-arm anything, so
    /// nothing is dispatched that was not already due. Dropping the entry is the whole of it, so a
    /// cleared pull request starts its next round from zero exactly as a re-introduced one does.
    ///
    /// Deliberately NOT allowlist-gated, for [`Self::handle_review_dismiss`]'s reason: it performs
    /// no checkout and no dispatch (dispatch re-checks the allowlist itself), and gating it would
    /// make the budgets an operator most wants gone — the ones a paused or repointed project left
    /// behind — the only ones that could never be cleared.
    pub(crate) fn handle_review_clear(&mut self, pr: &PrCoord) -> ReviewControlOutcome {
        if !self.review_ticketless_enabled() {
            return ReviewControlOutcome::Dormant; // §16
        }
        if let Some(why) = check_coords(pr) {
            return ReviewControlOutcome::Refused(why);
        }
        // The ledger is cleared BEFORE the counter is read, so the two writes this function's doc
        // binds together can never come apart. A decision genuinely can land after a Clear: the
        // turn runs off-loop, `mark_in_flight` is on the control task, and `record` fires only after
        // an un-timed comment POST. An operator who clears inside that window leaves a settled
        // decision and no counter, and the old ordering then refused every later Clear as "no
        // budget" — with the only stated recovery (the WARN and the README both name this POST) a
        // `409`. Dropping a decision is as much a clear as dropping a counter.
        let cleared_decision = self
            .adjudication_ledger
            .as_ref()
            .is_some_and(|ledger| ledger.clear(pr));
        // A refusal, not an `Applied(0)`: the operator asked to clear a bound and there was none,
        // which is a different fact from "the budget is now clear" and worth saying.
        let cleared_counter = self.review_rounds.remove(&churn_key(pr)).is_some();
        // The author rounds still awaiting a reviewer's answer (STUDIO-1004) go with the budget.
        // They are not a bound — nothing is charged for them yet — but a clear promises a state
        // where both halves may run, and an un-answered author round left behind would charge a
        // round against the fresh budget the moment some queued review finally landed.
        self.author_rounds_pending.remove(&churn_key(pr));
        // Durably, and unconditionally: the deliberate clear is the documented way to lift a bound
        // now that a restart no longer does it (STUDIO-956), so it must leave nothing behind for a
        // later boot to rehydrate — including a row this process never saw.
        self.forget_review_bound(pr);
        if !cleared_counter && !cleared_decision {
            return ReviewControlOutcome::Refused(
                "no review budget to clear for that pull request",
            );
        }
        tracing::info!(
            pr = %pr,
            "ticketless review: operator cleared the pull request's review budget and any manager \
             adjudication of it"
        );
        ReviewControlOutcome::Applied(1)
    }

    /// **Dismiss** (`Event::ReviewDismiss`) — the operator taking a pull request out of the watch
    /// set, §15-e's other lever. Drops every row of `pr` through
    /// [`rhapsody_store::Store::drop_review_watch`], the same terminal the watcher uses for a merged
    /// or closed pull request, so a dismissal and a merge leave identical state.
    ///
    /// `reviewer` narrows it to ONE row (STUDIO-1022): with `Some(name)`, only that reviewer's row
    /// of the pull request is dropped, and the pull request keeps its place in the watch set on its
    /// remaining rows. That is the in-place lever the incident had no answer for — a review whose
    /// named reviewer can never serve it now that the roster or `review.reviewers` changed, without
    /// the whole-pull-request dismissal that would also stop the reviews somebody is still waiting
    /// on. `None` is the original behaviour verbatim: every row of the pull request goes.
    ///
    /// A dismissal is a soft delete: both SHAs stay as the record of what was reviewed, and the row
    /// keeps its place in the console's list as a `dropped` one. It is idempotent, so dismissing
    /// twice is not an error, and it is deliberately NOT allowlist-gated (see the module doc).
    ///
    /// A review that is running right now is left to finish rather than killed — stopping a run is
    /// `POST /api/v1/runs/{id}/stop`'s job, and this control's contract is the watch set. Its
    /// completion cannot resurrect the row: `mark_review_completed` writes the two SHAs and the
    /// status and never touches `open`, so the row stays closed and out of every live read.
    pub(crate) fn handle_review_dismiss(
        &mut self,
        pr: &PrCoord,
        reviewer: Option<&str>,
    ) -> ReviewControlOutcome {
        if !self.review_ticketless_enabled() {
            return ReviewControlOutcome::Dormant; // §16
        }
        if let Some(why) = check_coords(pr) {
            return ReviewControlOutcome::Refused(why);
        }
        // The FULL set, not the live one: a row that is already closed but not yet `dropped` — a
        // pull request the watcher observed as merged mid-tick — is exactly the kind an operator
        // clears by hand, and `load_live_review_watch` filters it out.
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => return ReviewControlOutcome::Failed(e.to_string()),
        };
        let mine: Vec<ReviewWatchRow> = rows
            .into_iter()
            .filter(|r| row_is(r, pr) && !(r.status == REVIEW_STATUS_DROPPED && !r.open))
            // A named reviewer narrows the dismissal to that row alone. Matched
            // case-insensitively, because GitHub logins are and the console may not spell the
            // stored name identically; `None` keeps every row, byte-identical to before STUDIO-1022.
            .filter(|r| {
                reviewer.is_none_or(|name| r.key.reviewer.eq_ignore_ascii_case(name.trim()))
            })
            .collect();
        if mine.is_empty() {
            return ReviewControlOutcome::Refused("no watched review of that pull request");
        }
        // The dismissal's coordinate, taken from a MATCHED ROW rather than from the request, so the
        // records removed below are keyed by the same source the watcher inserted them from. The
        // operator's own coordinate is unnormalized (`check_coords` only rejects empties) while
        // `PrCoord`'s derived `Eq` is case-sensitive, so removing `pr` directly would miss a record
        // the watcher stored under the store row's casing — a dismissal typed `MakeWhatIs` matched
        // this row case-insensitively but left its unreadability record behind (STUDIO-950 round 16).
        // `mine` is non-empty by the check above, and every row in it is the same pull request.
        let dismissed = PrCoord::new(&mine[0].key.owner, &mine[0].key.repo, mine[0].key.number);
        let mut dropped = 0usize;
        for row in mine {
            let id = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            // STUDIO-891: whether or not the drop below succeeds, this row is one the operator has
            // said they are not waiting on. Its consecutive-deferral count must not outlive it —
            // left standing it would keep `REVIEW_UNASSIGNABLE_WARNING` lit on every project for
            // the rest of the daemon's life, which is a warning that only ever latches.
            self.review_unassignable.remove(&id);
            // STUDIO-950: same reasoning for the capacity hold, whose job is to annotate the
            // reconciliation sweep's report of an OWED round. A dismissed pull request owes none,
            // and the hold survives unreached ticks by design, so it must be dropped here rather
            // than left to the TTL.
            self.review_capacity_held.remove(&id);
            match self.store().drop_review_watch(&row.key) {
                Ok(()) => dropped += 1,
                Err(e) => {
                    tracing::warn!(review = %id, err = %e, "ticketless review: the operator's dismissal could not drop the watch row")
                }
            }
        }
        // The PR-level records below are cleared only for a WHOLE-pull-request dismissal
        // (STUDIO-1022). A named-reviewer dismissal leaves the pull request watched on its other
        // rows, so its churn budget, its durable bound, its unreadability and observed-head records
        // are all still facts about a live watch — clearing them would refund rounds for a pull
        // request the operator was NOT taking out of the set, and drop the head memo a still-watched
        // escalation may be compared against.
        if dropped > 0 && reviewer.is_none() {
            // The churn budget goes with the rows, for `retire_review_pr`'s reason: a re-introduced
            // pull request should not inherit the spent budget of the one that was dismissed.
            self.review_rounds.remove(&churn_key(pr));
            // ...and the author rounds awaiting an answer (STUDIO-1004), for the same reason.
            self.author_rounds_pending.remove(&churn_key(pr));
            // ...and its durable counterpart, so a restart cannot resurrect the spent budget of
            // a dismissed pull request (STUDIO-956). `churn_key` lowercases, so the operator's own
            // casing is safe here in a way the coordinate-keyed record below is not.
            self.forget_review_bound(pr);
            // ...and the unreadability record, keyed by coordinate for `retire_review_pr`'s reason:
            // left behind it would outlive the pull request it names (STUDIO-950 round 14).
            //
            // Sits under `dropped > 0`, unlike the per-row removals above: those run whether or not
            // the store drop succeeds (a row the operator is not waiting on must not keep
            // `REVIEW_UNASSIGNABLE_WARNING` latched, or its hold annotated), while this record,
            // the churn budget and its durable bound are keyed by coordinate rather than by row and
            // so cannot be retired per row. A dismissal whose every store drop FAILED therefore
            // leaves the failure count standing, which is still a live fact about a pull request
            // the daemon continues to poll; once AT LEAST one row is gone the operator has said they are not waiting on it.
            // In the mixed case — some rows dropped, some failed — the surviving row is still polled
            // but loses the record, restarting its one-attempt grace period. That can only DELAY a
            // denial, never invent one (the count climbs again from zero), so it is preferred to
            // keeping a dismissed pull request's record named forever. It also drops the
            // `capacity_unreadable` ANNOTATION from the surviving row's report until the count
            // climbs back to `UNREADABLE_ATTEMPTS_TO_DROP_HOLD`, so that row reads as an ordinary
            // divergence for one grace period while `gh` still refuses the coordinate — a delay of
            // the same page, which is why this is the smaller harm, not a harm-free choice.
            self.review_watch_unreadable.remove(&dismissed);
            // ...and the observed-head memo (STUDIO-1005 review round 1), keyed by coordinate for
            // the same reason: left behind it would leak one entry per dismissed pull request, and a
            // later reintroduction of the same coordinate would transiently inherit the previous
            // watch lifecycle's head — which the reconciliation sweep would read as a supersession
            // of a fresh escalation until the rotating watcher reached it.
            self.review_observed_head.remove(&dismissed);
            // ...and any in-flight preparation for the coordinate: the sweep will never observe it
            // again, so a completion that started before the dismissal must not dispatch a review of
            // work the operator took out of the watch set (STUDIO-988 review round 4, alice #2).
            self.cancel_review_preparations_for(&dismissed);
            tracing::info!(pr = %pr, rows = dropped, "ticketless review: operator dismissed a pull request from the watch set");
        }
        ReviewControlOutcome::Applied(dropped)
    }
}

impl ControlHandle {
    /// The console's read of the review watch set (`GET /api/v1/reviews`), answered on the control
    /// task so the HTTP task never touches the store the loop is the single writer of.
    ///
    /// A gone or cancelled control task answers an EMPTY, dormant view rather than an error: the
    /// daemon is shutting down, there is nothing left to steer, and a 500 would send the operator
    /// looking for a fault in the review subsystem.
    pub async fn list_reviews(&self) -> Result<ReviewsView, StoreError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewConsoleList { reply: tx })
            .is_err()
        {
            return Ok(ReviewsView::default());
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or_else(|_| Ok(ReviewsView::default())),
            _ = lifetime.cancelled() => Ok(ReviewsView::default()),
        }
    }

    /// The operator's **re-run** (`POST /api/v1/reviews/rerun`) — §15-e's trusted lever, delivered
    /// as an in-process control Event rather than a room post (§14.1 F-SEC).
    pub async fn rerun_review(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.review_control(|reply| Event::ReviewRerun { pr, reply })
            .await
    }

    /// The operator's **dismiss** (`POST /api/v1/reviews/dismiss`), the same path for the same
    /// reason. `reviewer` drops only that reviewer's row of the pull request; `None` drops every
    /// row of it (STUDIO-1022).
    pub async fn dismiss_review(
        &self,
        pr: PrCoord,
        reviewer: Option<String>,
    ) -> ReviewControlOutcome {
        self.review_control(|reply| Event::ReviewDismiss {
            pr,
            reviewer,
            reply,
        })
        .await
    }

    /// The operator's **clear** (`POST /api/v1/reviews/clear`) — drop a pull request's shared
    /// review↔author round budget so both halves of the loop may run again, without a restart
    /// (STUDIO-956). The same trusted path as the other two.
    pub async fn clear_review(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.review_control(|reply| Event::ReviewClear { pr, reply })
            .await
    }

    /// Sends one console control and waits for the control task's verdict.
    ///
    /// It owns the reply channel and hands the caller only the SENDER, so an event can never be
    /// sent paired with somebody else's receiver — a pairing this would otherwise have to take on
    /// trust from two call sites, and one that fails as a hang rather than as an error.
    ///
    /// The wait is bounded by the daemon lifetime rather than a timer, as
    /// [`ControlHandle::introduce_review`]'s is: a busy tick should delay an operator's click, not
    /// turn it into a false failure. A gone or cancelled loop answers `Refused` and not `Dormant`
    /// for that method's reason too — "the daemon is shutting down" and "this installation has the
    /// subsystem off" are different facts, and only the second reads as working as configured.
    async fn review_control(
        &self,
        ev: impl FnOnce(tokio::sync::oneshot::Sender<ReviewControlOutcome>) -> Event,
    ) -> ReviewControlOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ev = ev(tx);
        const GONE: ReviewControlOutcome =
            ReviewControlOutcome::Refused("the control task is gone");
        if self.events.send(ev).is_err() {
            return GONE;
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or(GONE),
            _ = lifetime.cancelled() => GONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_APPROVED, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REVIEWED,
        REVIEW_STATUS_TRUNCATED, ReviewWatchKey, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::reviewwatch::{REVIEW_ROUNDS_PER_PR_CAP, review_round_due};
    use crate::testsupport::{empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
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

    fn teams_with(enabled: bool, mode: ReviewMode) -> Teams {
        Teams {
            enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster: vec![ident("alice"), ident("bob")],
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

    fn ticketless() -> Orchestrator {
        orch(teams_with(true, ReviewMode::Ticketless))
    }

    fn key(reviewer: &str) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: reviewer.to_string(),
        }
    }

    fn pr() -> PrCoord {
        PrCoord::new("makewhatis", "rhapsody", 12)
    }

    /// Puts one watch row in the set directly, bypassing introduction — the console's inputs are
    /// rows, however they got there.
    ///
    /// It seeds through the store's OWN methods rather than writing the row wholesale, because
    /// which method may move which SHA is the watch set's whole idempotency: `save_review_watch`
    /// cannot move either on an existing row, `mark_review_requested` owns `requested_sha` and
    /// `mark_review_completed` owns `last_reviewed_sha`. A helper that wrote the columns directly
    /// could seed a combination the daemon can never actually reach.
    fn watch(o: &mut Orchestrator, reviewer: &str, status: &str, requested: &str, reviewed: &str) {
        let seed = ReviewWatchRow {
            key: key(reviewer),
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-720".to_string(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.to_string(),
            open: true,
        };
        o.store()
            .save_review_watch(seed.clone())
            .expect("seed the row");
        if !requested.is_empty() {
            o.store()
                .mark_review_requested(&key(reviewer), requested)
                .expect("requested");
        }
        if !reviewed.is_empty() {
            o.store()
                .mark_review_completed(&key(reviewer), reviewed, status)
                .expect("completed");
        } else if status != REVIEW_STATUS_IN_FLIGHT && status != REVIEW_STATUS_REQUESTED {
            // A round that ended without completing — `truncated`. It moves neither SHA, which is
            // exactly why the store gives it its own method.
            assert_eq!(status, REVIEW_STATUS_TRUNCATED, "unseedable status");
            o.store()
                .mark_review_truncated(&key(reviewer))
                .expect("truncated");
        }
    }

    fn row_of(o: &Orchestrator, reviewer: &str) -> ReviewWatchRow {
        o.store()
            .get_review_watch(&key(reviewer))
            .expect("read")
            .expect("the row exists")
    }

    // ── the read surface ─────────────────────────────────────────────────────────────────────────

    /// **Acceptance 1.** The Reviews surface lists each watched pull request with its reviewer,
    /// status and `last_reviewed_sha`.
    #[test]
    fn the_surface_lists_every_watched_pull_request_with_its_reviewer_and_shas() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_IN_FLIGHT, HEAD_B, "");

        let view = o.review_console_list().expect("list");
        assert!(view.enabled);
        assert_eq!(view.reviews.len(), 2);

        let bob = &view.reviews[0];
        assert_eq!(
            (bob.owner.as_str(), bob.repo.as_str(), bob.number),
            ("makewhatis", "rhapsody", 12)
        );
        assert_eq!(bob.reviewer, "bob");
        assert_eq!(bob.status, REVIEW_STATUS_REVIEWED);
        assert_eq!(bob.last_reviewed_sha, HEAD_A);
        assert_eq!(bob.requested_sha, HEAD_A);
        assert_eq!(bob.author, "alice");
        assert_eq!(bob.introduced_by, "handoff:STUDIO-720");
        assert!(bob.open);

        let carol = &view.reviews[1];
        assert_eq!(carol.reviewer, "carol");
        assert_eq!(carol.status, REVIEW_STATUS_IN_FLIGHT);
        assert_eq!(carol.requested_sha, HEAD_B);
        assert!(
            carol.last_reviewed_sha.is_empty(),
            "a round in flight has read nothing yet"
        );
    }

    /// A retired row stays in the list, carrying `open: false` and the `dropped` status. It is what
    /// makes the `open` column mean something — and what stops a dismissal looking like a pull
    /// request that was never introduced.
    #[test]
    fn the_surface_still_lists_a_retired_row() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.store().drop_review_watch(&key("bob")).expect("drop");

        let view = o.review_console_list().expect("list");
        assert_eq!(view.reviews.len(), 1);
        assert_eq!(view.reviews[0].status, REVIEW_STATUS_DROPPED);
        assert!(!view.reviews[0].open);
        assert_eq!(
            view.reviews[0].last_reviewed_sha, HEAD_A,
            "a retirement is a soft delete: what was reviewed stays on the record"
        );
    }

    /// **Acceptance 4, the read half (§16).** Teams off, or any mode but `ticketless`, and the
    /// surface is empty and says it is disabled — even with rows in the store.
    #[test]
    fn a_dormant_daemon_serves_an_empty_disabled_surface() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (false, ReviewMode::Off),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = ticketless();
            watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
            o.teams = Some(teams_with(enabled, mode));

            let view = o.review_console_list().expect("list");
            assert_eq!(
                view,
                ReviewsView::default(),
                "enabled={enabled} mode={mode:?} must serve nothing at all"
            );
        }
    }

    // ── re-run ───────────────────────────────────────────────────────────────────────────────────

    /// **Acceptance 2.** Re-run re-arms a finished review so the watcher's own edge trigger says a
    /// round is due AT THE SAME HEAD — which is the whole point of the lever, since nothing pushed.
    ///
    /// The assertion is on [`review_round_due`], not on the status string: what the operator was
    /// promised is a review, and the watcher's predicate is the thing that decides whether one
    /// happens.
    #[test]
    fn a_rerun_arms_a_finished_review_for_another_round_at_the_same_head() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        assert!(
            !review_round_due(&row_of(&o, "bob"), HEAD_A, false),
            "an approved review at the current head owes nothing until this test's re-run"
        );

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1)
        );

        let row = row_of(&o, "bob");
        assert_eq!(row.status, REVIEW_STATUS_REQUESTED);
        assert!(row.open);
        assert!(
            review_round_due(&row, HEAD_A, false),
            "the re-armed row owes a round at the unchanged head"
        );
        assert_eq!(
            row.last_reviewed_sha, HEAD_A,
            "re-arming must not forget what was already reviewed (F-SHA)"
        );
        assert_eq!(row.requested_sha, HEAD_A, "nor which head was dispatched");
    }

    /// Every live row of the pull request is re-armed — a review is a property of the pull request,
    /// and re-running one reviewer's half of a two-reviewer round is not what the operator asked
    /// for.
    #[test]
    fn a_rerun_arms_every_live_row_of_the_pull_request() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(2)
        );
        for who in ["bob", "carol"] {
            assert_eq!(row_of(&o, who).status, REVIEW_STATUS_REQUESTED, "{who}");
        }
    }

    /// **F-DUP.** A row whose review is RUNNING is left exactly as it is. Re-arming it would
    /// overwrite the `in_flight` marker the edge trigger reads, and the next tick would point a
    /// second agent at the first one's detached worktree.
    #[test]
    fn a_rerun_never_disarms_a_review_that_is_already_running() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_IN_FLIGHT, HEAD_A, "");
        watch(&mut o, "carol", REVIEW_STATUS_IN_FLIGHT, HEAD_A, "");
        o.claimed
            .insert(review_key("makewhatis", "rhapsody", 12, "bob"));

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1),
            "only the row with no live run is re-armed"
        );
        assert_eq!(
            row_of(&o, "bob").status,
            REVIEW_STATUS_IN_FLIGHT,
            "the live round keeps its in-flight marker"
        );
        assert_eq!(row_of(&o, "carol").status, REVIEW_STATUS_REQUESTED);
    }

    /// A row that already owes a round is counted but not rewritten: the operator's question is
    /// "will this be reviewed again", and the honest answer is yes. Rewriting a row to the status
    /// it already holds is a store write that changes nothing.
    #[test]
    fn a_rerun_counts_a_row_that_already_owes_a_round_without_rewriting_it() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_TRUNCATED, HEAD_A, "");
        watch(&mut o, "carol", REVIEW_STATUS_REQUESTED, "", "");

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(2)
        );
        assert_eq!(row_of(&o, "bob").status, REVIEW_STATUS_REQUESTED);
        let carol = row_of(&o, "carol");
        assert_eq!(carol.status, REVIEW_STATUS_REQUESTED);
        assert!(
            carol.requested_sha.is_empty(),
            "an untouched row keeps its empty SHAs"
        );
    }

    /// The churn cap (§14.2) defers every further round of a pull request "until the daemon restarts
    /// or the pull request closes". An authenticated operator IS that escape hatch, so a re-run buys
    /// a round back — otherwise the button would accept the click and never review.
    ///
    /// ONE round back, not the whole budget: the operator asked for one re-read, and a reset would
    /// give a pull request that has already had eight rounds eight more unattended ones.
    #[test]
    fn a_rerun_refunds_one_round_of_the_per_pull_request_churn_budget() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        o.review_rounds.insert(
            "makewhatis/rhapsody#12".to_string(),
            REVIEW_ROUNDS_PER_PR_CAP,
        );

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.review_rounds.get("makewhatis/rhapsody#12"),
            Some(&(REVIEW_ROUNDS_PER_PR_CAP - 1)),
            "a spent budget must not swallow the operator's own request, nor be reset by it"
        );
    }

    /// The refund is a ROUND, and the counter is in DISPATCHES, so a config that requires two
    /// reviewers gets two dispatches back — the same scaling `service_review_pr` applies to the cap
    /// itself (STUDIO-727). Refunding a flat 1 there would give a two-reviewer config half a round
    /// and leave the operator's click still deferred by the cap.
    #[test]
    fn a_rerun_refunds_a_round_scaled_by_the_required_reviewer_count() {
        let mut teams = teams_with(true, ReviewMode::Ticketless);
        teams.review.reviewers = 2;
        let mut o = orch(teams);
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        let budget = REVIEW_ROUNDS_PER_PR_CAP * 2;
        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), budget);

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(2)
        );
        assert_eq!(
            o.review_rounds.get("makewhatis/rhapsody#12"),
            Some(&(budget - 2)),
            "one round of a two-reviewer config costs two dispatches, so two come back"
        );
    }

    /// The refund cannot go negative, and cannot become free budget: a pull request that has spent
    /// less than one round's worth comes back to zero, not below it.
    #[test]
    fn a_rerun_refund_saturates_at_an_unspent_budget() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.review_rounds.get("makewhatis/rhapsody#12"),
            None,
            "a pull request with no entry gains none"
        );

        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), 0);
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), Some(&0));
    }

    /// Re-run re-arms rows and introduces none — the same property
    /// [`Orchestrator::handle_review_head_advanced`] has, and what stops this control becoming a
    /// second, weaker introduction path into the watch set.
    #[test]
    fn a_rerun_of_an_unwatched_pull_request_writes_nothing() {
        let mut o = ticketless();
        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Refused("no live review of that pull request is watched")
        );
        assert!(o.store().load_review_watch().expect("read").is_empty());

        // A row that has already been retired is not a live review either: re-running it would put
        // a merged or dismissed pull request back into the dispatch path.
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.store().drop_review_watch(&key("bob")).expect("drop");
        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Refused("no live review of that pull request is watched")
        );
        assert_eq!(row_of(&o, "bob").status, REVIEW_STATUS_DROPPED);
    }

    /// **F-SEC at the console.** A re-run in a repository no ENABLED project owns is refused, even
    /// though the row is sitting in the watch set: the row is stored state and the configuration can
    /// have been repointed or the project paused since it was written. Fails closed, exactly as the
    /// head-advance and dispatch-side checks do.
    #[test]
    fn an_off_allowlist_rerun_is_refused_and_changes_nothing() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        // The project owning the row's repository is paused; a second one is live, and the
        // top-level `repo:` still names the paused one exactly as `resolve_projects` inherited it.
        let mut live = empty_resolved_project("podium", Arc::new(Fake::new()));
        live.repo = LIVE_URL.to_string();
        if let Some(eff) = o.eff.as_mut() {
            eff.cfg.repo = REPO_URL.to_string();
            eff.projects[0].disabled = true;
            eff.projects.push(live);
        }

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Refused("no configured project owns the PR's repo")
        );
        assert_eq!(
            row_of(&o, "bob").status,
            REVIEW_STATUS_APPROVED,
            "a refused re-run writes nothing"
        );
    }

    /// Coordinates are re-validated even though the caller is in-process: being a [`PrCoord`] means
    /// it was constructed by trusted code, not that its contents were checked.
    #[test]
    fn a_rerun_revalidates_the_coordinates_it_is_handed() {
        let mut o = ticketless();
        assert_eq!(
            o.handle_review_rerun(&PrCoord::new("", "rhapsody", 12)),
            ReviewControlOutcome::Refused("pull request has no owner/repo")
        );
        assert_eq!(
            o.handle_review_rerun(&PrCoord::new("makewhatis", "  ", 12)),
            ReviewControlOutcome::Refused("pull request has no owner/repo")
        );
        assert_eq!(
            o.handle_review_rerun(&PrCoord::new("makewhatis", "rhapsody", 0)),
            ReviewControlOutcome::Refused("pull-request number is not positive")
        );
    }

    // ── clear the round budget (STUDIO-956) ─────────────────────────────────────────────────────

    /// **Acceptance: the budget is clearable without a daemon restart.** An operator clears a spent
    /// review budget and the pull request is unbounded again.
    #[test]
    fn an_operator_clear_lifts_a_spent_round_budget() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.review_rounds.insert(
            "makewhatis/rhapsody#12".to_string(),
            REVIEW_ROUNDS_PER_PR_CAP,
        );
        assert!(o.round_budget_spent(&pr()));

        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.review_rounds.get("makewhatis/rhapsody#12"),
            None,
            "a clear drops the counter outright, unlike re-run's one-round refund"
        );
        assert!(!o.round_budget_spent(&pr()));
    }

    /// **STUDIO-956.** Clear also drops the manager's adjudication of the pull request: a settled
    /// `ship`/`escalate` keeps the loop stopped on its own, so leaving it behind would make the
    /// operator's lever look applied while nothing could dispatch.
    #[test]
    fn an_operator_clear_also_drops_a_manager_adjudication() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let mut o = ticketless();
        let ledger = std::sync::Arc::new(AdjudicationLedger::default());
        ledger.record(
            &pr(),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec!["alice asked for changes".to_string()],
                reason: "needs a human".to_string(),
            },
        );
        o.adjudication_ledger = Some(std::sync::Arc::clone(&ledger));
        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), 3);

        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.adjudication(&pr()),
            None,
            "the decision must go with the budget, or the loop stays stopped"
        );
    }

    /// **A decision that lands AFTER a Clear can still be cleared.** The ordered window is real: the
    /// manager's turn runs off-loop, `mark_in_flight` is on the control task, and `record` fires only
    /// after an un-timed comment POST. An operator who clears inside it leaves a settled decision
    /// with no counter, and the old counter-first check then refused every later Clear as "no
    /// budget" — while the WARN and the README both name this POST as the recovery. Pinned with a
    /// TWO-step clear: a single clear with both present passes under either ordering and would not
    /// discriminate this.
    #[test]
    fn a_clear_after_a_decision_landed_without_a_counter_still_clears_it() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let mut o = ticketless();
        let ledger = std::sync::Arc::new(AdjudicationLedger::default());
        o.adjudication_ledger = Some(std::sync::Arc::clone(&ledger));
        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), 3);

        // The operator clears while the adjudication plan is out: the counter goes, and nothing
        // else is there yet.
        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        // …then the off-loop turn lands its decision, with no counter to pair it with.
        ledger.record(
            &pr(),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec![],
                reason: "needs a human".to_string(),
            },
        );
        assert!(o.adjudication(&pr()).is_some());

        // The second clear must still drop it, or the loop stays stopped with the lever reading 409.
        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Applied(1),
            "dropping a decision is as much a clear as dropping a counter"
        );
        assert_eq!(o.adjudication(&pr()), None);
    }

    /// **A settled decision must not survive an operator re-run.** Re-run refunds one round and
    /// clears the decision so the refunded round can actually dispatch; without the clear the lever
    /// reports `Applied(1)` while the settled `ship`/`escalate` keeps the loop stopped, so nothing
    /// moves. Deleting the `ledger.clear` from the re-run path left every other test green.
    #[test]
    fn an_operator_rerun_also_drops_a_manager_adjudication() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        let ledger = std::sync::Arc::new(AdjudicationLedger::default());
        ledger.record(
            &pr(),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec!["alice asked for changes".to_string()],
                reason: "needs a human".to_string(),
            },
        );
        o.adjudication_ledger = Some(std::sync::Arc::clone(&ledger));
        o.review_rounds.insert(
            "makewhatis/rhapsody#12".to_string(),
            o.reviewers_per_round() * 2,
        );

        assert_eq!(
            o.handle_review_rerun(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.adjudication(&pr()),
            None,
            "a re-run overrides a settled decision, or the refunded round never dispatches"
        );
    }

    /// Clear touches no row: unlike re-run it re-arms nothing, so it dispatches only what was
    /// already due. An approved pull request stays approved after its budget is cleared.
    #[test]
    fn an_operator_clear_does_not_re_arm_a_row() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), 3);

        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            row_of(&o, "bob").status,
            REVIEW_STATUS_APPROVED,
            "clearing a budget is not a re-run"
        );
    }

    /// Clearing a pull request with no budget is a REFUSAL, not an `Applied(0)`: "there was nothing
    /// to clear" is a different fact from "the budget is now clear".
    #[test]
    fn clearing_an_unbudgeted_pull_request_is_refused() {
        let mut o = ticketless();
        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Refused("no review budget to clear for that pull request")
        );
    }

    /// Coordinates are re-validated even though the caller is in-process, and a dormant daemon
    /// refuses without touching anything — the same rules the other two controls obey.
    #[test]
    fn a_clear_revalidates_its_coordinates_and_is_dormant_when_off() {
        let mut o = ticketless();
        assert_eq!(
            o.handle_review_clear(&PrCoord::new("", "rhapsody", 12)),
            ReviewControlOutcome::Refused("pull request has no owner/repo")
        );
        assert_eq!(
            o.handle_review_clear(&PrCoord::new("makewhatis", "rhapsody", 0)),
            ReviewControlOutcome::Refused("pull-request number is not positive")
        );

        o.teams = Some(teams_with(true, ReviewMode::Off));
        assert_eq!(
            o.handle_review_clear(&pr()),
            ReviewControlOutcome::Dormant,
            "mode off ⇒ dormant, even with a budget sitting in the counter"
        );
    }

    // ── dismiss ──────────────────────────────────────────────────────────────────────────────────

    /// **Acceptance 3.** Dismiss drops the pull request out of the watch set — every row of it, to
    /// the same terminal a merge or a close reaches, so nothing polls or dispatches it again.
    #[test]
    fn a_dismissal_drops_every_row_of_the_pull_request() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_IN_FLIGHT, HEAD_B, "");

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(2)
        );
        assert!(
            o.store().load_live_review_watch().expect("read").is_empty(),
            "a dismissed pull request is polled and dispatched no more"
        );
        for who in ["bob", "carol"] {
            let row = row_of(&o, who);
            assert_eq!(row.status, REVIEW_STATUS_DROPPED, "{who}");
            assert!(!row.open, "{who}");
        }
        assert_eq!(
            row_of(&o, "bob").last_reviewed_sha,
            HEAD_A,
            "a soft delete: what was reviewed stays on the record"
        );
    }

    /// **STUDIO-1022: a named dismissal drops ONE reviewer's row and leaves the pull request
    /// watched.** This is the in-place lever the incident had no answer for — the departed
    /// reviewer's slot can be retired without also stopping the reviews the pull request is still
    /// waiting on. The PR-level records stay, because the pull request is still under watch.
    #[test]
    fn a_named_reviewer_dismissal_drops_only_that_row() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_IN_FLIGHT, HEAD_B, "");
        o.review_rounds.insert(churn_key(&pr()), 3);

        assert_eq!(
            o.handle_review_dismiss(&pr(), Some("bob")),
            ReviewControlOutcome::Applied(1)
        );

        assert_eq!(row_of(&o, "bob").status, REVIEW_STATUS_DROPPED);
        assert_eq!(
            row_of(&o, "carol").status,
            REVIEW_STATUS_IN_FLIGHT,
            "carol's row is untouched"
        );
        assert_eq!(
            o.store().load_live_review_watch().expect("read").len(),
            1,
            "the pull request is still watched on carol's row"
        );
        assert_eq!(
            o.review_rounds.get(&churn_key(&pr())),
            Some(&3),
            "a pull request that is still watched keeps its churn budget"
        );
    }

    /// A named dismissal matches the reviewer case-insensitively, and refuses when that reviewer has
    /// no row — the same refusal an unqualified dismissal gives for a pull request with none.
    #[test]
    fn a_named_dismissal_is_case_insensitive_and_refuses_an_unknown_reviewer() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);

        assert_eq!(
            o.handle_review_dismiss(&pr(), Some("BOB")),
            ReviewControlOutcome::Applied(1),
            "GitHub logins are case-insensitive; a differently-cased name still selects the row"
        );
        assert_eq!(
            o.handle_review_dismiss(&pr(), Some("nobody")),
            ReviewControlOutcome::Refused("no watched review of that pull request")
        );
    }

    /// Dismissing twice is not an error. The second call finds only rows that are already `dropped`
    /// and refuses rather than reporting a change it did not make.
    #[test]
    fn a_second_dismissal_changes_nothing_and_says_so() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Refused("no watched review of that pull request")
        );
    }

    /// Dismissal is deliberately NOT allowlist-gated, and this is the case that decides it: the rows
    /// an operator most wants gone are the ones a repointed or paused project left behind, and
    /// gating dismissal on the allowlist would make exactly those undeletable.
    #[test]
    fn a_dismissal_can_retire_a_row_whose_project_is_no_longer_configured() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        if let Some(eff) = o.eff.as_mut() {
            eff.projects.clear();
            eff.cfg.repo = LIVE_URL.to_string();
        }
        assert!(!o.review_repo_is_configured("makewhatis", "rhapsody"));

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(row_of(&o, "bob").status, REVIEW_STATUS_DROPPED);
    }

    /// A review running right now is left to finish rather than killed — stopping a run is
    /// `POST /api/v1/runs/{id}/stop`'s job. What matters is that its completion cannot resurrect the
    /// row: `mark_review_completed` writes the SHAs and the status and never touches `open`, so the
    /// row stays out of every live read.
    #[test]
    fn a_dismissal_survives_the_completion_of_the_run_it_interrupted() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_IN_FLIGHT, HEAD_A, "");
        o.running
            .insert(review_key("makewhatis", "rhapsody", 12, "bob"), {
                let mut re = crate::orchestrator::RunningEntry::empty(rhapsody_core::Issue {
                    id: review_key("makewhatis", "rhapsody", 12, "bob"),
                    ..Default::default()
                });
                re.identity = "bob".to_string();
                re
            });

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        // The interrupted run finishes and records what it read, as it would have anyway.
        o.store()
            .mark_review_completed(&key("bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completion");

        assert!(
            !row_of(&o, "bob").open,
            "a completion must not put a dismissed pull request back under watch"
        );
        assert!(o.store().load_live_review_watch().expect("read").is_empty());
    }

    /// Dismissal retires the churn budget with the rows, for `retire_review_pr`'s reason: a pull
    /// request re-introduced later must not inherit the spent budget of the one that was dismissed.
    #[test]
    fn a_dismissal_retires_the_churn_budget_with_the_rows() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.review_rounds
            .insert("makewhatis/rhapsody#12".to_string(), 3);
        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), None);
    }

    /// STUDIO-891: and the stall counters go with them, for the same reason plus one of its own.
    ///
    /// A dismissed pull request is one nobody is waiting on, so a stalled-round advisory raised
    /// against it must come down with it. Left behind, the counter would keep
    /// [`REVIEW_UNASSIGNABLE_WARNING`](crate::reviewwatch::REVIEW_UNASSIGNABLE_WARNING) lit on
    /// every project for the rest of the daemon's life — a warning that only ever latches, which
    /// is worse than none.
    #[test]
    fn a_dismissal_retires_the_stall_counters_with_the_rows() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.review_unassignable.insert(
            review_key("makewhatis", "rhapsody", 12, "bob"),
            crate::reviewwatch::REVIEW_UNASSIGNABLE_SWEEPS,
        );
        assert!(o.review_rounds_stalled());

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            o.review_unassignable.is_empty(),
            "a dismissed pull request must not leave a stall counter behind"
        );
        assert!(!o.review_rounds_stalled());
    }

    /// STUDIO-950 (round 11, non-blocking B): a dismissal drops the dismissed round's capacity hold.
    /// The hold survives unreached ticks by design, so a dismissed pull request would otherwise keep
    /// naming a capacity wait until the watcher stops sweeping — a WRONG named cause for a pull
    /// request nobody is waiting on. Pin the removal on the dismissal path.
    ///
    /// Mutation check: drop the `review_capacity_held.remove(&id)` in `handle_review_dismiss` and
    /// this reds.
    #[test]
    fn a_dismissal_drops_a_capacity_hold() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        let id = review_key("makewhatis", "rhapsody", 12, "bob");
        o.review_capacity_held.insert(
            id.clone(),
            crate::reviewwatch::CapacityHold {
                holders: 4,
                separate: false,
                recorded: chrono::Utc::now(),
            },
        );

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            !o.review_capacity_held.contains_key(&id),
            "a dismissed pull request must not keep a capacity hold"
        );
    }

    /// STUDIO-950 (round 15, alice's non-blocking 1): a dismissal forgets the dismissed pull
    /// request's unreadability record, keyed by coordinate for `retire_review_pr`'s reason. Left
    /// behind it would outlive the pull request it names and sit in the map for the daemon's whole
    /// life; a re-introduced coordinate could inherit a failure count it never earned and have its
    /// first fresh hold denied.
    ///
    /// Mutation check: drop the `review_watch_unreadable.remove(pr)` in `handle_review_dismiss`
    /// and this reds.
    #[test]
    fn a_dismissal_forgets_the_unreadable_record() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.handle_review_unreadable(&[pr()]);
        assert!(
            o.review_watch_unreadable.contains_key(&pr()),
            "precondition: a failed lookup is recorded"
        );

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            !o.review_watch_unreadable.contains_key(&pr()),
            "a dismissed pull request must not keep an unreadability record"
        );
    }

    /// STUDIO-950 (round 16, jimmy's finding): the dismissal removes the unreadability record by the
    /// MATCHED ROW's coordinate, not the operator's. `PrCoord`'s derived `Eq` is case-sensitive and
    /// `check_coords` never normalizes, while `row_is` matches case-insensitively — so a dismissal
    /// typed the way GitHub prints the repository used to drop the rows and the churn budget but
    /// leave the record behind. Mutation check: remove `pr` instead of the matched row's coordinate
    /// and this reds on the unreadable assertion only.
    #[test]
    fn a_case_mismatched_dismissal_forgets_the_unreadable_record() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        o.handle_review_unreadable(&[pr()]);
        o.review_rounds.insert(churn_key(&pr()), 1);

        // The operator's coordinate, typed in different casing from the store row the watcher
        // keyed the record on.
        let typed = PrCoord::new("MakeWhatIs", "Rhapsody", 12);
        assert_eq!(
            o.handle_review_dismiss(&typed, None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            !o.review_rounds.contains_key(&churn_key(&pr())),
            "the churn budget goes with the rows"
        );
        assert!(
            !o.review_watch_unreadable.contains_key(&pr()),
            "so must the unreadability record, whatever casing the operator typed"
        );
    }

    /// STUDIO-1005 (round 1, sol's blocking 2): a dismissal forgets the observed-head memo too,
    /// keyed by the MATCHED ROW's coordinate for the unreadability record's reason above. Left
    /// behind it would leak one entry per dismissed pull request, and a later reintroduction of the
    /// same coordinate would transiently inherit the previous watch lifecycle's head — which the
    /// reconciliation sweep would read as a supersession of a fresh escalation until the rotating
    /// watcher reached it.
    ///
    /// Mutation check: drop the `review_observed_head.remove(&dismissed)` in
    /// `handle_review_dismiss` and this reds.
    #[test]
    fn a_dismissal_forgets_the_observed_head() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        // Keyed by the store row's coordinate, casing and all; the operator's own spelling is
        // unnormalized, so the removal must use the matched row's coordinate, not the request's.
        o.review_observed_head.insert(
            pr(),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: HEAD_A.to_string(),
            },
        );
        assert!(
            o.review_observed_head.contains_key(&pr()),
            "precondition: the watcher recorded the head it observed"
        );

        let typed = PrCoord::new("MakeWhatIs", "Rhapsody", 12);
        assert_eq!(
            o.handle_review_dismiss(&typed, None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            !o.review_observed_head.contains_key(&pr()),
            "a dismissed pull request must not keep the head memo a reintroduction could inherit"
        );
    }

    /// **Acceptance 4, the control half (§16).** A dormant daemon refuses both controls without
    /// reading or writing anything — and says `Dormant`, which is not the same fact as a refusal.
    #[test]
    fn a_dormant_daemon_performs_neither_control() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (false, ReviewMode::Off),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = ticketless();
            watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
            o.teams = Some(teams_with(enabled, mode));

            assert_eq!(
                o.handle_review_rerun(&pr()),
                ReviewControlOutcome::Dormant,
                "enabled={enabled} mode={mode:?}"
            );
            assert_eq!(
                o.handle_review_dismiss(&pr(), None),
                ReviewControlOutcome::Dormant,
                "enabled={enabled} mode={mode:?}"
            );
            o.teams = Some(teams_with(true, ReviewMode::Ticketless));
            let row = row_of(&o, "bob");
            assert_eq!(row.status, REVIEW_STATUS_APPROVED, "nothing was written");
            assert!(row.open);
        }
    }

    /// Owner and repository are matched case-insensitively, because GitHub logins and repository
    /// names are. A console rendering `MakeWhatIs/Rhapsody` must steer the same rows the watcher
    /// polls.
    #[test]
    fn the_controls_match_a_repository_however_it_is_spelled() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A);
        assert_eq!(
            o.handle_review_rerun(&PrCoord::new("MakeWhatIs", "Rhapsody", 12)),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.handle_review_dismiss(&PrCoord::new("MAKEWHATIS", "RHAPSODY", 12), None),
            ReviewControlOutcome::Applied(1)
        );
    }

    // MUTATION GUARD: a dismissal that leaves an in-flight review preparation alone lets a
    // completion resurrect a live watch row for work the operator removed (STUDIO-988 review round 4,
    // alice #2).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dismissal_cancels_an_in_flight_review_preparation() {
        use crate::prepare::{
            PreparationCompletion, PreparationOutcome, PreparedDispatch, PreparedSelection,
            PreparedTarget, ReviewHeadObservation,
        };
        use crate::review::ReviewRun;
        use crate::testsupport::{DispatchedEntries, HangResolver, record_entries};

        let mut o = ticketless();
        o.prepare_resolver = Some(Arc::new(HangResolver));
        let sink: DispatchedEntries = Arc::new(std::sync::Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);

        let run = ReviewRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "bob".to_string(),
            author: "alice".to_string(),
            repo_url: REPO_URL.to_string(),
            head_sha: HEAD_B.to_string(),
            ..Default::default()
        };
        // The sweep observed the pull request open at the review's head, so revalidation WOULD accept
        // the completion were the preparation left alive.
        o.review_observed_head.insert(
            PrCoord::new("makewhatis", "rhapsody", 12),
            ReviewHeadObservation {
                open: true,
                head: HEAD_B.to_string(),
            },
        );
        let route = o.route_for(Some(0)).expect("a configured project route");
        let target = PreparedTarget::Review {
            issue: run.synthetic_issue(),
            run: Box::new(run.clone()),
            route,
            commit: None,
        };
        assert!(matches!(
            o.begin_preparation(target, false),
            crate::prepare::BeginPreparation::Started(_)
        ));
        let token = o
            .preparing
            .get(&run.key())
            .map(|e| e.token)
            .expect("review reservation");

        assert_eq!(
            o.handle_review_dismiss(&pr(), None),
            ReviewControlOutcome::Applied(1)
        );
        assert!(
            o.preparing.is_empty(),
            "the dismissal must cancel the in-flight review preparation"
        );
        // A late completion for the cancelled reservation is dropped: no run, no live row.
        o.handle_dispatch_prepared(
            run.key(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
                resolved: PreparedSelection::default(),
            },
        )
        .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a dismissed review must not dispatch"
        );
        assert!(
            o.store()
                .load_live_review_watch()
                .expect("live rows")
                .is_empty(),
            "a dismissed review must not come back live"
        );
    }
}
