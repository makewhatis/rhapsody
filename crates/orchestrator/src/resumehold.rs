//! resumehold — the operator's "Human step done → resume" action (STUDIO-1053). A Rhapsody-only
//! addition (Go v0.4.0 has no operator-hold resume).
//!
//! # The incident this exists for
//!
//! STUDIO-1050 was held with `rhapsody:human` while its run waited for B2 credentials. When the
//! operator finished the human step they did the natural thing — they posted "I have added the
//! secrets…" from the ticket's job page. Nothing resumed: the room has no dispatch power by design,
//! the `rhapsody:human` hold makes the dispatcher refuse the ticket anyway, and the manager misread
//! the post (STUDIO-1052). The only way to resume was to edit Linear by hand.
//!
//! The job page is where the operator is when the human step finishes, so the job page gets the
//! action. One confirmed click does three things, in this order:
//!
//! 1. **records the note** where the next run is guaranteed to read it — the same reopen-seed
//!    mechanism a reopening summons uses (`pending_reopen_summons` → [`crate::message::Orchestrator::seed_reopen_summons`],
//!    which wraps it as an OPERATOR MESSAGE, not a teammate one). The room and comments are
//!    deliberately NOT used: neither is guaranteed to reach the next prompt. Done FIRST so a tick
//!    racing the label removal already finds the note waiting;
//! 2. **removes** `rhapsody:human` through the tracker (the wrinkle that actually unblocks
//!    dispatch). If the tracker refuses, the seed is undone (restoring any seed it replaced) so a
//!    refused action leaves no side effect behind;
//! 3. **requeues** the ticket — clears the in-memory suppression so the next tick can dispatch it,
//!    keeping its state when it is already active and otherwise moving it to Todo.
//!
//! # Breaker holds
//!
//! The hold may have been applied by the runaway breaker (STUDIO-1026) rather than by the operator.
//! The durable [`rhapsody_store::BreakerCrossingRow`] is the evidence: when one exists for the
//! ticket, [`ResumeHoldResult::breaker`] names the crossed limit so the confirmation can say what
//! was crossed. Resuming deliberately does NOT delete the crossing row: the breaker's counters are
//! not reset, so crossing again holds again.
//!
//! # Where the work runs
//!
//! The tracker round-trips (the held check, the label removal, the by-id state read and the Todo
//! move) run OFF the control task, on the HTTP request's own task, exactly as
//! [`ControlHandle::stop_run`]/[`ControlHandle::resume_run`] do. Only the two writes that touch
//! loop-owned state — clearing the suppression and inserting into `pending_reopen_summons` — round
//! trip the control channel ([`Event::SeedResumeNote`]).

use std::sync::PoisonError;

use chrono::{DateTime, Utc};
use rhapsody_store::StoreError;
use rhapsody_tracker::Tracker;

use crate::control_loop::{CancelWait, Event};
use crate::dispatch::dispatchable_state;
use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;
use crate::teams::HUMAN_LABEL;

/// The crossed breaker limit a resume would lift, for the confirmation to name. Read from the
/// durable [`rhapsody_store::BreakerCrossingRow`] and never from memory, so it survives a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakerHold {
    /// Completed review runs that had HAPPENED when the last round-crossing notified; `0` when no
    /// round crossing has occurred.
    pub rounds: i64,
    /// The providers whose per-ticket spend cap has already notified.
    pub providers: Vec<String>,
}

impl BreakerHold {
    /// Whether the row names any crossing at all. A persisted row always carries one, but a caller
    /// building one by hand should not have to know that.
    pub fn is_empty(&self) -> bool {
        self.rounds <= 0 && self.providers.is_empty()
    }
}

/// The HTTP-layer result of a human-step resume (`POST /api/v1/runs/{id}/resume-hold`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeHoldResult {
    /// No run row has this id ⇒ 404 `run_not_found`.
    pub not_found: bool,
    /// The run's ticket does NOT wear `rhapsody:human` ⇒ 409 `not_held`. The action is refused on a
    /// ticket that is not held, exactly as the console's button only shows on one that is.
    pub not_held: bool,
    /// Human ticket id, e.g. `"STUDIO-1050"`.
    pub identifier: String,
    /// The note was handed to the next run's seed.
    pub note_recorded: bool,
    /// `rhapsody:human` was removed through the tracker.
    pub label_removed: bool,
    /// The ticket is a dispatch candidate again: its state is active, or it was moved to Todo.
    pub queued: bool,
    /// The state name the ticket was moved to (`""` when its state was already active).
    pub moved_to: String,
    /// Non-empty when the requeue MOVE failed (the note and label removal landed, but the ticket
    /// stayed in a non-active state). The response is still a 200 — the failure is stated, never
    /// hidden — but `queued` is false, so nothing claims the ticket is a candidate.
    pub move_err: String,
    /// The breaker limit that was crossed, when the hold came from the runaway breaker.
    pub breaker: Option<BreakerHold>,
}

/// A refusal surfaced to the HTTP layer. Only a control round-trip cancellation, a store read or a
/// tracker read is an `Err`; the business outcomes (`not_found`, `not_held`, a failed requeue move)
/// travel inside [`ResumeHoldResult`], following the crate's "business outcomes are not errors"
/// convention.
#[derive(Debug, thiserror::Error)]
pub enum ResumeHoldError {
    /// The request or lifetime ctx was cancelled before the operation could commit.
    #[error("canceled")]
    Canceled,
    /// A store read failed (`get_run`, or the breaker-crossing read).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A tracker read failed (the held check or the by-id state read).
    #[error("tracker read failed: {0}")]
    Tracker(String),
    /// The tracker rejected the `rhapsody:human` removal. Nothing else was committed, so the caller
    /// reports a failure rather than a partial success.
    #[error("the hold could not be lifted: {0}")]
    LabelRemovalFailed(String),
}

/// The read side behind `GET /api/v1/runs/{id}/hold`: whether this run's ticket is held, and if so
/// whether the hold came from the breaker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunHoldView {
    /// No run row has this id ⇒ 404.
    pub not_found: bool,
    /// The ticket currently wears `rhapsody:human`.
    pub held: bool,
    pub identifier: String,
    /// The crossed limit, when the durable crossing row names one.
    pub breaker: Option<BreakerHold>,
}

impl Orchestrator {
    /// Runs ON the control task for `evSeedResumeNote`: records the operator's note for the next
    /// run and clears the in-memory suppression so the ticket is a candidate again.
    ///
    /// `pending_reopen_summons` is consumed by `dispatch_issue` the moment any run for this issue
    /// becomes live — the same funnel every dispatch path shares — so the note reaches the fresh
    /// run's operator mailbox regardless of how the ticket was requeued (an active-state dispatch or
    /// a review reopen). An existing seed for the issue is REPLACED: the operator's newest statement
    /// of what they did supersedes an older summons for a ticket they have just resumed. The
    /// replaced entry is RETURNED so a later refused label removal can restore it rather than
    /// silently losing a pending reopening summons.
    pub(crate) fn handle_seed_resume_note(
        &mut self,
        issue_id: &str,
        note: &str,
        at: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, String)> {
        self.claimed.remove(issue_id);
        let prior = self
            .pending_reopen_summons
            .insert(issue_id.to_string(), (at, note.to_string()));
        tracing::info!(
            issue_id,
            "human-step resume: the operator's note is seeded into the next run and the ticket is \
             requeued"
        );
        prior
    }

    /// Runs ON the control task for `evClearResumeNote`: undoes [`handle_seed_resume_note`] after a
    /// LATER step of the action failed (STUDIO-1053). `prior` is the seed the action replaced, or
    /// `None` when the issue had none; the prior entry is restored when present, so a refused resume
    /// does not destroy a pending reopening summons. `claimed` is deliberately left as the seed set
    /// it — the ticket still wears the label, so it cannot dispatch, and re-claiming a ticket the
    /// operator later un-hands by hand would strand it.
    pub(crate) fn handle_clear_resume_note(
        &mut self,
        issue_id: &str,
        prior: Option<(DateTime<Utc>, String)>,
    ) {
        match prior {
            Some(entry) => {
                self.pending_reopen_summons
                    .insert(issue_id.to_string(), entry);
            }
            None => {
                self.pending_reopen_summons.remove(issue_id);
            }
        }
    }
}

impl ControlHandle {
    /// Whether this run's ticket is held for a human, and — when the durable crossing row names one
    /// — the breaker limit that was crossed. Read-only, so no control round-trip: the tracker read
    /// is the authority (it sees a label the daemon's own ledger may never have primed).
    pub async fn run_hold(&self, run_id: i64) -> Result<RunHoldView, ResumeHoldError> {
        let run = match self.store.get_run(run_id)? {
            Some(r) => r,
            None => {
                return Ok(RunHoldView {
                    not_found: true,
                    ..Default::default()
                });
            }
        };
        let Some(tracker) = self.effective_tracker() else {
            return Err(ResumeHoldError::Tracker("no effective tracker".into()));
        };
        let held = self.is_held(&tracker, &run.issue_id).await?;
        Ok(RunHoldView {
            not_found: false,
            held,
            identifier: run.issue_identifier.clone(),
            breaker: self.breaker_hold(&run.issue_identifier),
        })
    }

    /// The operator's "Human step done → resume" action (STUDIO-1053): remove `rhapsody:human`,
    /// record the operator's note for the next run, and requeue the ticket.
    ///
    /// The order is deliberate. The label removal runs FIRST because it is the one step the tracker
    /// can refuse, and a refusal must leave nothing half-done: the response then reports a failure
    /// and the note was never seeded. Once the label is gone, the note seed and the requeue land;
    /// a requeue MOVE the tracker rejects is a reported partial failure (`queued: false` plus
    /// `move_err`), never a silent success.
    pub async fn resume_hold(
        &self,
        req_ctx: CancelWait,
        run_id: i64,
        note: &str,
    ) -> Result<ResumeHoldResult, ResumeHoldError> {
        if req_ctx.is_cancelled() {
            return Err(ResumeHoldError::Canceled);
        }
        let note = note.trim();
        let run = match self.store.get_run(run_id)? {
            Some(r) => r,
            None => {
                return Ok(ResumeHoldResult {
                    not_found: true,
                    ..Default::default()
                });
            }
        };
        let Some(tracker) = self.effective_tracker() else {
            return Err(ResumeHoldError::Tracker("no effective tracker".into()));
        };
        if !self.is_held(&tracker, &run.issue_id).await? {
            return Ok(ResumeHoldResult {
                not_held: true,
                identifier: run.issue_identifier,
                ..Default::default()
            });
        }
        // The ticket's current state, read BEFORE anything is committed: the requeue step needs it,
        // and a read that fails must leave the action with nothing to undo. Read here rather than
        // after the label removal so a tracker that can answer for labels but not states reports an
        // error before the note is seeded.
        let states = self
            .reads
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .states
            .clone();
        let state = tracker
            .fetch_issue_states_by_ids(std::slice::from_ref(&run.issue_id))
            .await
            .map_err(|e| ResumeHoldError::Tracker(e.to_string()))?
            .into_iter()
            .next()
            .map(|i| i.state)
            .unwrap_or_default();
        let active = dispatchable_state(
            &rhapsody_core::normalize_state(&state),
            &states.active,
            &states.terminal,
        );
        // 1. Seed the note FIRST, and clear the suppression with it. The note must be in place
        //    BEFORE the label comes off: the instant the hold is gone an unclaimed ticket is a
        //    dispatch candidate, and a tick racing the label removal must find the note waiting for
        //    it. Seeding after the removal would let that run start without it.
        let at = Utc::now();
        let prior = match self.seed_resume_note(&run.issue_id, note, at).await {
            Some(prior) => prior,
            None => return Err(ResumeHoldError::Canceled),
        };
        // 2. Remove the label. A refusal here undoes the seed (restoring any seed it replaced), so
        //    nothing is left committed and the response reports a failure rather than a partial
        //    success.
        if let Err(e) = tracker
            .remove_issue_label(&run.issue_id, &run.team_id, HUMAN_LABEL)
            .await
        {
            let _ = self.clear_resume_note(&run.issue_id, prior).await;
            tracing::error!(
                issue_identifier = %run.issue_identifier,
                err = %e,
                "human-step resume: the rhapsody:human label could not be removed; the seed was \
                 undone and the ticket is unchanged"
            );
            return Err(ResumeHoldError::LabelRemovalFailed(e.to_string()));
        }
        // 3. Requeue. An already-active ticket is a candidate the moment the label is gone; a
        //    non-active one has to be moved to Todo or the next tick will not select it.
        let mut res = ResumeHoldResult {
            identifier: run.issue_identifier.clone(),
            note_recorded: true,
            label_removed: true,
            queued: true,
            breaker: self.breaker_hold(&run.issue_identifier),
            ..Default::default()
        };
        if !active {
            match tracker
                .move_issue_to_type(&run.issue_id, &run.team_id, "unstarted")
                .await
            {
                Ok(name) => res.moved_to = name,
                Err(e) => {
                    res.queued = false;
                    res.move_err = e.to_string();
                    tracing::error!(
                        issue_identifier = %run.issue_identifier,
                        err = %res.move_err,
                        "human-step resume: the note was recorded and the hold lifted, but the \
                         move to Todo failed — the ticket is not a dispatch candidate"
                    );
                }
            }
        }
        Ok(res)
    }

    /// The best tracker the handle can reach: the `control()`-time snapshot, else the live shared
    /// reads cell (the same fallback [`ControlHandle::stop_run`]'s move uses).
    fn effective_tracker(&self) -> Option<std::sync::Arc<dyn Tracker>> {
        self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .tracker
                .clone()
        })
    }

    /// Whether `issue_id` currently wears `rhapsody:human`, read from the tracker (not the
    /// daemon's ledger, which fails closed while un-primed). A failed read is an `Err` rather than a
    /// silent "not held": refusing the action is the safe answer, but it must not read as a fact.
    async fn is_held(
        &self,
        tracker: &std::sync::Arc<dyn Tracker>,
        issue_id: &str,
    ) -> Result<bool, ResumeHoldError> {
        let want = rhapsody_core::normalize_state(HUMAN_LABEL);
        let rows = tracker
            .fetch_issue_labels_by_ids(std::slice::from_ref(&issue_id.to_string()))
            .await
            .map_err(|e| ResumeHoldError::Tracker(e.to_string()))?;
        Ok(rows.iter().any(|iss| {
            iss.labels
                .iter()
                .flatten()
                .any(|l| rhapsody_core::normalize_state(l) == want)
        }))
    }

    /// The durable breaker crossing for a ticket, if one exists. Never mutates it: resuming must
    /// not reset the breaker's counters, so crossing again holds again.
    fn breaker_hold(&self, ticket: &str) -> Option<BreakerHold> {
        self.store
            .load_breaker_crossings()
            .ok()?
            .into_iter()
            .find(|r| r.ticket == ticket)
            .map(|r| BreakerHold {
                rounds: r.notified_rounds,
                providers: r.notified_providers,
            })
    }

    /// Round-trips [`Event::SeedResumeNote`]. Returns `None` when the loop is gone or the lifetime
    /// ctx ended before the reply — the caller then reports cancellation rather than a success — and
    /// otherwise the seed it REPLACED (`None` when the issue had none), which the failure path hands
    /// back to [`clear_resume_note`](Self::clear_resume_note) so a refused action restores it.
    async fn seed_resume_note(
        &self,
        issue_id: &str,
        note: &str,
        at: DateTime<Utc>,
    ) -> Option<Option<(DateTime<Utc>, String)>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ev = Event::SeedResumeNote {
            issue_id: issue_id.to_string(),
            note: note.to_string(),
            at,
            reply: tx,
        };
        if self.events.send(ev).is_err() {
            return None;
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.ok(),
            _ = lifetime.cancelled() => None,
        }
    }

    /// Round-trips [`Event::ClearResumeNote`], the failure path's undo of
    /// [`seed_resume_note`](Self::seed_resume_note), restoring `prior` when the seed had replaced an
    /// earlier entry. Best-effort: a gone loop or an ended lifetime leaves the seed in place, which
    /// is harmless (the ticket is still held by the label).
    async fn clear_resume_note(
        &self,
        issue_id: &str,
        prior: Option<(DateTime<Utc>, String)>,
    ) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ev = Event::ClearResumeNote {
            issue_id: issue_id.to_string(),
            prior,
            reply: tx,
        };
        if self.events.send(ev).is_err() {
            return false;
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.is_ok(),
            _ = lifetime.cancelled() => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use rhapsody_core::Issue;
    use rhapsody_store::{OUTCOME_STOPPED, RunEnd, RunStart, Sqlite, StorePath};
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{empty_effective, empty_resolved_project};

    type SharedStore = Arc<dyn rhapsody_store::Store + Send + Sync>;

    fn store() -> SharedStore {
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory"))
    }

    fn held_issue(id: &str, ident: &str, state: &str) -> Issue {
        Issue {
            id: id.into(),
            identifier: ident.into(),
            title: "t".into(),
            state: state.into(),
            team_id: "TEAM-1".into(),
            labels: Some(vec!["rhapsody:human".into()]),
            ..Default::default()
        }
    }

    fn plain_issue(id: &str, ident: &str, state: &str) -> Issue {
        Issue {
            id: id.into(),
            identifier: ident.into(),
            title: "t".into(),
            state: state.into(),
            team_id: "TEAM-1".into(),
            ..Default::default()
        }
    }

    /// Seeds a run row for `iss`, returning its id.
    fn seed_run(store: &SharedStore, iss: &Issue) -> i64 {
        let run_id = store
            .start_run(RunStart {
                issue_id: iss.id.clone(),
                issue_identifier: iss.identifier.clone(),
                title: iss.title.clone(),
                team_id: iss.team_id.clone(),
                ..Default::default()
            })
            .expect("start_run");
        store
            .end_run(
                run_id,
                RunEnd {
                    outcome: OUTCOME_STOPPED.into(),
                    ended_at: Utc::now().to_rfc3339(),
                    ..Default::default()
                },
            )
            .expect("end_run");
        run_id
    }

    struct Harness {
        handle: ControlHandle,
        signal: CancelSignal,
        tracker: Arc<Fake>,
        store: SharedStore,
        task: tokio::task::JoinHandle<Orchestrator>,
    }

    /// An orchestrator on an in-memory store with a fake tracker whose `by_id` map serves both the
    /// labels and states reads, a disabled poll interval, and a live control loop whose task yields
    /// the orchestrator back so a test can drive `dispatch_issue` race-free after the action.
    async fn start(fake: Fake, iss: &Issue) -> (Harness, i64) {
        let store = store();
        let tracker = Arc::new(fake);
        let mut eff = empty_effective(Arc::clone(&tracker) as Arc<dyn Tracker>);
        eff.active_states = HashSet::from(["todo".to_string(), "in progress".to_string()]);
        eff.terminal_states = HashSet::from(["done".to_string()]);
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.stall_timeout = Duration::from_secs(3600);
        let mut proj = empty_resolved_project("rhapsody", Arc::clone(&tracker) as Arc<dyn Tracker>);
        proj.repo = "git@github.com:makewhatis/rhapsody.git".into();
        eff.projects = vec![proj];
        let run_id = seed_run(&store, iss);
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.set_store(Arc::clone(&store));
        // The reload path publishes the dispatchable-state sets into the reads cell; the off-loop
        // action reads them from there, so a test must publish them the same way.
        o.reads
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states = crate::dispatch::DispatchStates {
            active: HashSet::from(["todo".to_string(), "in progress".to_string()]),
            terminal: HashSet::from(["done".to_string()]),
            ..Default::default()
        };
        o.spawn = Some(Box::new(|_iss, _attempt, _re| {}));
        o.claimed.insert(iss.id.clone());
        let signal = CancelSignal::new();
        o.set_ctx(signal.wait());
        let handle = o.control();
        let loop_ctx = signal.wait();
        let task = tokio::spawn(async move {
            let mut o = o;
            o.run_loaded(loop_ctx).await;
            o
        });
        (
            Harness {
                handle,
                signal,
                tracker,
                store,
                task,
            },
            run_id,
        )
    }

    /// A fake tracker serving `iss` through `by_id` (so both the labels and states by-id reads
    /// answer for it) and resolving `move_issue_to_type` to `"Todo"`.
    fn fake_for(iss: &Issue) -> Fake {
        let mut fake = Fake::new();
        fake.by_id.insert(iss.id.clone(), iss.clone());
        fake.move_to_type_name = "Todo".into();
        fake
    }

    /// Ends the loop and hands the orchestrator back for post-action assertions.
    async fn finish(h: Harness) -> Orchestrator {
        h.signal.cancel();
        tokio::time::timeout(Duration::from_secs(5), h.task)
            .await
            .expect("the loop task must exit once the lifetime is cancelled")
            .expect("loop task join")
    }

    // STUDIO-1053 acceptance: on a held ticket the action records the note where the next run reads
    // it (the reopen seed, wrapped as an operator message), removes `rhapsody:human` through the
    // tracker, and clears the suppression so the ticket is a dispatch candidate again. Any one of
    // the three skipped turns this red.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_hold_seeds_the_note_removes_the_label_and_requeues() {
        let iss = held_issue("iss-1", "STUDIO-1050", "In Progress");
        let (h, run_id) = start(fake_for(&iss), &iss).await;

        let res = h
            .handle
            .resume_hold(CancelWait::default(), run_id, "  added the secrets  ")
            .await
            .expect("resume_hold");

        assert!(
            !res.not_found && !res.not_held,
            "unexpected refusal: {res:?}"
        );
        assert_eq!(res.identifier, "STUDIO-1050");
        assert!(res.note_recorded, "the note must be recorded");
        assert!(res.label_removed, "the hold must be removed");
        assert!(res.queued, "the ticket must be a candidate again");
        assert_eq!(res.moved_to, "", "an active ticket keeps its state");
        assert!(res.move_err.is_empty());
        let calls = h.tracker.remove_label_calls();
        assert_eq!(calls.len(), 1, "one label removal");
        assert_eq!(calls[0].issue_id, "iss-1");
        assert_eq!(calls[0].label_name, "rhapsody:human");

        // The suppression is gone and the seed survives on the loop's own state.
        let mut o = finish(h).await;
        assert!(
            !o.claimed.contains("iss-1"),
            "the resume must clear the in-memory suppression"
        );
        assert!(
            o.pending_reopen_summons.contains_key("iss-1"),
            "the note must be held for the next run"
        );

        // The next dispatch seeds the note into the fresh run's operator mailbox, wrapped as
        // OPERATOR input (not a teammate message).
        o.dispatch_issue(
            plain_issue("iss-1", "STUDIO-1050", "In Progress"),
            None,
            None,
            String::new(),
        );
        let got = o
            .mailbox_try_recv("iss-1")
            .expect("the note must reach the next run");
        assert!(
            got.contains("OPERATOR MESSAGE"),
            "not operator-framed: {got}"
        );
        assert!(
            got.contains("added the secrets"),
            "note body missing: {got}"
        );
    }

    // STUDIO-1053 acceptance: a breaker hold's confirmation names the crossed limit, and resuming
    // does NOT reset the breaker's counters. The durable crossing row survives the resume, so
    // crossing again holds again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_breaker_hold_names_the_limit_and_resuming_keeps_the_counters() {
        let iss = held_issue("iss-1", "STUDIO-988", "In Review");
        let (h, run_id) = start(fake_for(&iss), &iss).await;
        h.store
            .save_breaker_crossing(&rhapsody_store::BreakerCrossingRow {
                ticket: "STUDIO-988".into(),
                notified_rounds: 5,
                notified_providers: vec!["anthropic".into()],
            })
            .expect("save crossing");

        let res = h
            .handle
            .resume_hold(CancelWait::default(), run_id, "done")
            .await
            .expect("resume_hold");

        let breaker = res.breaker.expect("the crossed limit must be named");
        assert_eq!(breaker.rounds, 5);
        assert_eq!(breaker.providers, vec!["anthropic".to_string()]);
        // `In Review` is not an active state, so the ticket is moved to Todo to become a candidate.
        assert!(res.queued);
        assert_eq!(res.moved_to, "Todo");

        // Resuming must not delete or reset the persisted crossing.
        let rows = h.store.load_breaker_crossings().expect("crossings");
        assert_eq!(rows.len(), 1, "the crossing row must survive the resume");
        assert_eq!(rows[0].notified_rounds, 5, "the counter must not reset");
        assert_eq!(rows[0].notified_providers, vec!["anthropic".to_string()]);
    }

    // STUDIO-1053 acceptance: the action is refused on a ticket that is not held — no label removal,
    // no seed, and nothing moves.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_hold_refuses_a_ticket_that_is_not_held() {
        let iss = plain_issue("iss-1", "STUDIO-1051", "In Progress");
        let (h, run_id) = start(fake_for(&iss), &iss).await;

        let res = h
            .handle
            .resume_hold(CancelWait::default(), run_id, "nothing to see")
            .await
            .expect("resume_hold");

        assert!(res.not_held, "an unheld ticket must be refused: {res:?}");
        assert!(!res.note_recorded && !res.label_removed && !res.queued);
        assert!(
            h.tracker.remove_label_calls().is_empty(),
            "an unheld ticket must not have a label removed"
        );
        let o = finish(h).await;
        assert!(
            !o.pending_reopen_summons.contains_key("iss-1"),
            "nothing may be seeded for an unheld ticket"
        );
        assert!(
            o.claimed.contains("iss-1"),
            "an unheld ticket's suppression must be left alone"
        );
    }

    // STUDIO-1053 acceptance: a partial failure (the tracker rejects the label removal) is
    // reported, and nothing claims success — no note is seeded and the ticket is not requeued.
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_hold_reports_a_label_removal_failure_and_claims_nothing() {
        let iss = held_issue("iss-1", "STUDIO-1052", "In Progress");
        let mut fake = fake_for(&iss);
        fake.remove_label_err = Some(TrackerError::Other("linear_move_rejected".into()));
        let (h, run_id) = start(fake, &iss).await;

        let err = h
            .handle
            .resume_hold(CancelWait::default(), run_id, "done")
            .await
            .expect_err("a rejected label removal must be an error");

        assert!(
            matches!(err, ResumeHoldError::LabelRemovalFailed(_)),
            "want LabelRemovalFailed, got {err:?}"
        );
        let o = finish(h).await;
        // The seed is undone, so a failed action leaves nothing committed. (The suppressed set is
        // deliberately not restored: the ticket still wears the label, so it cannot dispatch, and
        // re-claiming a ticket the operator later un-holds by hand would strand it.)
        assert!(
            !o.pending_reopen_summons.contains_key("iss-1"),
            "a failed label removal must not leave the note recorded"
        );
    }

    // The failed-label-removal undo restores a seed it REPLACED, so a pending reopening summons is
    // not silently destroyed by an action the tracker refused (STUDIO-1053 review follow-up).
    #[test]
    fn a_refused_resume_restores_a_pending_reopening_seed() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let t0 = Utc::now();
        o.pending_reopen_summons
            .insert("iss-1".into(), (t0, "prior summons".into()));
        let prior = o.handle_seed_resume_note("iss-1", "operator note", Utc::now());
        assert_eq!(
            prior,
            Some((t0, "prior summons".into())),
            "the seed must hand back what it replaced"
        );
        o.handle_clear_resume_note("iss-1", prior);
        assert_eq!(
            o.pending_reopen_summons.get("iss-1"),
            Some(&(t0, "prior summons".into())),
            "the refused resume must restore the prior seed"
        );
    }

    // With no prior seed the undo removes the note entirely — a refused action leaves nothing.
    #[test]
    fn the_undo_removes_the_note_when_there_was_no_prior() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let prior = o.handle_seed_resume_note("iss-1", "operator note", Utc::now());
        assert!(prior.is_none());
        o.handle_clear_resume_note("iss-1", prior);
        assert!(!o.pending_reopen_summons.contains_key("iss-1"));
    }

    // The read behind the console's action: it reports the held ticket and the crossed limit.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_hold_reports_the_hold_and_the_breaker_limit() {
        let iss = held_issue("iss-1", "STUDIO-988", "In Review");
        let (h, run_id) = start(fake_for(&iss), &iss).await;
        h.store
            .save_breaker_crossing(&rhapsody_store::BreakerCrossingRow {
                ticket: "STUDIO-988".into(),
                notified_rounds: 10,
                notified_providers: Vec::new(),
            })
            .expect("save crossing");

        let view = h.handle.run_hold(run_id).await.expect("run_hold");
        assert!(!view.not_found);
        assert!(view.held);
        assert_eq!(view.identifier, "STUDIO-988");
        assert_eq!(view.breaker.expect("breaker").rounds, 10);

        let unknown = h.handle.run_hold(4242).await.expect("run_hold unknown");
        assert!(unknown.not_found);
        assert!(!unknown.held);
        let _ = finish(h).await;
    }
}
