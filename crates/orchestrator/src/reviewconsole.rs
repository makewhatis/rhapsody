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
//! room Intent at all**; the two operator actions arrive as in-process control [`Event`]s from the
//! loopback HTTP API instead, which is the trusted path §15-e means.
//!
//! [`Event`]: crate::Event
//!
//! # Trusted in-process, still re-validated
//!
//! Being an in-process type is not the same as being a validated one — the rule
//! [`crate::reviewintro`] states and this module inherits. Both handlers re-check the coordinates
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
//! All three entry points run on the control task, for the reason
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
            // round back rather than half of one. Saturating: a counter below one round's cost just
            // returns to zero.
            let round = self
                .teams
                .as_ref()
                .map_or(1, |t| t.review.effective_reviewers().max(1));
            if let Some(spent) = self.review_rounds.get_mut(&churn_key(pr)) {
                *spent = spent.saturating_sub(round);
            }
            tracing::info!(pr = %pr, rows = armed, "ticketless review: operator re-ran a review");
        }
        ReviewControlOutcome::Applied(armed)
    }

    /// **Dismiss** (`Event::ReviewDismiss`) — the operator taking a pull request out of the watch
    /// set, §15-e's other lever. Drops every row of `pr` through
    /// [`rhapsody_store::Store::drop_review_watch`], the same terminal the watcher uses for a merged
    /// or closed pull request, so a dismissal and a merge leave identical state.
    ///
    /// A dismissal is a soft delete: both SHAs stay as the record of what was reviewed, and the row
    /// keeps its place in the console's list as a `dropped` one. It is idempotent, so dismissing
    /// twice is not an error, and it is deliberately NOT allowlist-gated (see the module doc).
    ///
    /// A review that is running right now is left to finish rather than killed — stopping a run is
    /// `POST /api/v1/runs/{id}/stop`'s job, and this control's contract is the watch set. Its
    /// completion cannot resurrect the row: `mark_review_completed` writes the two SHAs and the
    /// status and never touches `open`, so the row stays closed and out of every live read.
    pub(crate) fn handle_review_dismiss(&mut self, pr: &PrCoord) -> ReviewControlOutcome {
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
            .collect();
        if mine.is_empty() {
            return ReviewControlOutcome::Refused("no watched review of that pull request");
        }
        let mut dropped = 0usize;
        for row in mine {
            match self.store().drop_review_watch(&row.key) {
                Ok(()) => dropped += 1,
                Err(e) => {
                    let id = review_key(
                        &row.key.owner,
                        &row.key.repo,
                        row.key.number,
                        &row.key.reviewer,
                    );
                    tracing::warn!(review = %id, err = %e, "ticketless review: the operator's dismissal could not drop the watch row")
                }
            }
        }
        if dropped > 0 {
            // The churn budget goes with the rows, for `retire_review_pr`'s reason: a re-introduced
            // pull request should not inherit the spent budget of the one that was dismissed.
            self.review_rounds.remove(&churn_key(pr));
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
    /// reason.
    pub async fn dismiss_review(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.review_control(|reply| Event::ReviewDismiss { pr, reply })
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

    // ── dismiss ──────────────────────────────────────────────────────────────────────────────────

    /// **Acceptance 3.** Dismiss drops the pull request out of the watch set — every row of it, to
    /// the same terminal a merge or a close reaches, so nothing polls or dispatches it again.
    #[test]
    fn a_dismissal_drops_every_row_of_the_pull_request() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        watch(&mut o, "carol", REVIEW_STATUS_IN_FLIGHT, HEAD_B, "");

        assert_eq!(
            o.handle_review_dismiss(&pr()),
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

    /// Dismissing twice is not an error. The second call finds only rows that are already `dropped`
    /// and refuses rather than reporting a change it did not make.
    #[test]
    fn a_second_dismissal_changes_nothing_and_says_so() {
        let mut o = ticketless();
        watch(&mut o, "bob", REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A);
        assert_eq!(
            o.handle_review_dismiss(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.handle_review_dismiss(&pr()),
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
            o.handle_review_dismiss(&pr()),
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
            o.handle_review_dismiss(&pr()),
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
            o.handle_review_dismiss(&pr()),
            ReviewControlOutcome::Applied(1)
        );
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), None);
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
                o.handle_review_dismiss(&pr()),
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
            o.handle_review_dismiss(&PrCoord::new("MAKEWHATIS", "RHAPSODY", 12)),
            ReviewControlOutcome::Applied(1)
        );
    }
}
