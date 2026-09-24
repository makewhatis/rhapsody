//! managerwake — WAKING THE AUTHOR from a durable obligation, never from a comment (STUDIO-1017,
//! design record `manager-agent-design.md` §7.9). **No Go v0.4.0 counterpart**: the whole ticketless
//! review loop and the manager that adjudicates it are Rhapsody additions.
//!
//! # The claim
//!
//! `ROUTE_TO_AUTHOR`'s wake-up is a **wake obligation** (`rhapsody_manager_wake`) written by the
//! activation transaction (§7.7). No manager comment carries a summon token, so no comment can wake
//! anyone: only a row in that table causes a manager-authorized dispatch. This module owns the
//! admission side M9 wrote rows for.
//!
//! # The admission contract
//!
//! [`Orchestrator::pump_manager_wakes`] runs on the control task, each tick, **before** ordinary
//! selection. For each `pending` row it rechecks current state and then admits the wake through
//! **today's path**: [`Orchestrator::dispatch_or_prepare`] (the same entry point ordinary
//! selection uses, so a provider installation is PREPARED rather than silently dispatched on the
//! native login) followed by the STUDIO-649 reopen seed for an author with no live run, or the
//! existing mailbox admission for one with a live run. The row is marked `admitted` with the
//! `run_id` synchronously, before the control task yields, and `delivered` the moment the seed is
//! written as the run's "sent" `run_messages` row — only then is it spent.
//!
//! While any unspent obligation exists for a ticket, ordinary selection skips it
//! ([`Orchestrator::manager_wake_blocks_selection`]), so the author is dispatched once, with the
//! seed, and never without it.
//!
//! # Crash recovery
//!
//! An `admitted` row whose seed was never delivered is a run that died with the daemon. At boot
//! ([`Orchestrator::recover_manager_wakes`]) every `admitted` row goes back to `pending` and is
//! admitted again on a later tick. The per-run watermark and the single obligation per intervention
//! mean the author is woken **at most once per live run, and never lost**.
//!
//! # Recheck (§7.9)
//!
//! Before admitting, the row is refused when the intervention no longer exists, is not
//! `awaiting_effect`, its generation is no longer current, there is a hold (or the hold set is not
//! known), or the pull request is not open. A change to `review_authority` **after** activation does
//! NOT refuse it: the decision was committed under `act`.

use rhapsody_core::Issue;
use rhapsody_store::{
    MANAGER_INTERVENTION_AWAITING_EFFECT, MANAGER_WAKE_ADMITTED, MANAGER_WAKE_DELIVERED,
    MANAGER_WAKE_PENDING, MANAGER_WAKE_REFUSED, ManagerWakeRow, RUN_MESSAGE_SENT,
};

use crate::orchestrator::Orchestrator;
use crate::retry::DispatchRoute;

/// One candidate a wake obligation can be admitted against: the ticket as this tick fetched it, and
/// the route its owning project resolves to (`None` for the legacy single-project path).
pub(crate) type WakeCandidate<'a> = (&'a Issue, Option<DispatchRoute>);

impl Orchestrator {
    /// §7.9: the per-tick wake admission pass. Runs on the control task BEFORE ordinary selection,
    /// so a ticket with a pending obligation is dispatched by this pass (or skipped by selection)
    /// rather than twice.
    ///
    /// The immutable phase reads the store and plans; the mutable phase dispatches. Nothing here
    /// awaits, so the row is marked `admitted` synchronously before the control task yields.
    pub(crate) fn pump_manager_wakes(&mut self, candidates: &[WakeCandidate<'_>]) {
        if self.manager_review_authority() != rhapsody_config::teams::ReviewAuthority::Act {
            return;
        }
        let rows = match self.store().load_manager_wakes() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "manager wake: the obligations could not be read; admission waits");
                return;
            }
        };
        // Planned admissions (immutable phase output): the row plus the candidate to dispatch.
        let mut planned: Vec<(ManagerWakeRow, Issue, Option<DispatchRoute>)> = Vec::new();
        // Admitted rows to re-deliver or recover in the mutable phase.
        let mut redo: Vec<ManagerWakeRow> = Vec::new();
        for row in rows {
            match row.state.as_str() {
                MANAGER_WAKE_ADMITTED => {
                    // Already dispatched: deliver if the seed is a "sent" row, else hand the row to
                    // the mutable phase, which re-seeds a still-live run or returns it to `pending`.
                    if row
                        .run_id
                        .is_some_and(|id| self.wake_seed_sent(id, &row.body))
                    {
                        self.set_wake(&row, MANAGER_WAKE_DELIVERED, row.run_id, "");
                    } else {
                        redo.push(row);
                    }
                }
                MANAGER_WAKE_PENDING => match self.recheck_manager_wake(&row) {
                    Err(reason) => self.set_wake(&row, MANAGER_WAKE_REFUSED, None, &reason),
                    Ok(()) => {
                        if let Some((iss, route)) = find_candidate(candidates, &row.issue_id) {
                            planned.push((row, iss.clone(), route));
                        }
                        // No candidate this tick: the row stays `pending` and is admitted later.
                    }
                },
                _ => {}
            }
        }
        // Mutable phase: recover/re-deliver the admitted rows, then admit the pending ones.
        for row in redo {
            self.redeliver_or_recover_manager_wake(&row);
        }
        for (row, iss, route) in planned {
            self.admit_manager_wake(&row, iss, route);
        }
    }

    /// §7.9 recovery at boot: every `admitted` obligation whose run died with the daemon goes back
    /// to `pending` and is admitted again on a later tick — never lost, and never a second time for
    /// one live run.
    pub(crate) fn recover_manager_wakes(&mut self) {
        let rows = match self.store().load_manager_wakes() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "manager wake: recovery could not read the obligations");
                return;
            }
        };
        for row in rows {
            if row.state == MANAGER_WAKE_ADMITTED {
                self.set_wake(
                    &row,
                    MANAGER_WAKE_PENDING,
                    None,
                    "the run died with the daemon before the seed was delivered",
                );
            }
        }
    }

    /// §7.9: the per-row recheck, run at admission. `Ok(())` means the decision still holds and the
    /// author may be woken; `Err(reason)` refuses the row.
    fn recheck_manager_wake(&self, row: &ManagerWakeRow) -> Result<(), String> {
        let Some(iv) = self
            .store()
            .manager_intervention(&row.intervention_id)
            .ok()
            .flatten()
        else {
            return Err("the intervention no longer exists".to_string());
        };
        if iv.state != MANAGER_INTERVENTION_AWAITING_EFFECT {
            return Err(format!(
                "the intervention is '{}', not awaiting its effect",
                iv.state
            ));
        }
        // The generation must still be current. An unreadable bound fails closed: the wake must not
        // fire into a loop whose generation this process cannot confirm.
        match self.store().review_bound(&row.pr).ok().flatten() {
            Some(b) if b.generation != row.generation => {
                return Err("the review generation moved".to_string());
            }
            None => return Err("the pull request's generation is unknown".to_string()),
            _ => {}
        }
        let (labelled, hold_known) = self.human_holds.labelled_and_primed();
        if !hold_known {
            return Err("the hold set is not known".to_string());
        }
        if self.manager_pr_held(&row.pr, &labelled) {
            return Err("a hold is applied".to_string());
        }
        match self.manager_pr_open(&row.pr) {
            Some(true) => Ok(()),
            Some(false) => Err("the pull request is not open".to_string()),
            None => Err("the pull request's state could not be read".to_string()),
        }
    }

    /// Whether `run_id`'s "sent" `run_messages` row is the wake's seed. The one observable fact that
    /// means the obligation is delivered (§7.9 step 3).
    fn wake_seed_sent(&self, run_id: i64, body: &str) -> bool {
        self.store()
            .list_run_messages(run_id)
            .map(|msgs| {
                msgs.iter()
                    .any(|m| m.status == RUN_MESSAGE_SENT && m.body == body)
            })
            .unwrap_or(false)
    }

    /// §7.9 step 2: admit one pending obligation through today's path. No live author run ⇒ the
    /// SAME dispatch entry point ordinary selection uses ([`Orchestrator::dispatch_or_prepare`],
    /// so a provider install is PREPARED rather than silently dispatched on the native login —
    /// STUDIO-1002 review B3), then the STUDIO-649 reopen seed with `body`; live author run ⇒ the
    /// existing mailbox admission.
    ///
    /// The row is marked `admitted` with the `run_id` synchronously the moment the run becomes
    /// live (inline here, or from [`Orchestrator::admit_pending_manager_wake`] when an
    /// asynchronous preparation is accepted), and `delivered` in the same admission once the seed
    /// is a "sent" `run_messages` row. A seed write that did not land leaves the row `admitted`
    /// and boot recovery returns it to `pending`.
    fn admit_manager_wake(
        &mut self,
        row: &ManagerWakeRow,
        iss: Issue,
        route: Option<DispatchRoute>,
    ) {
        let key = iss.id.clone();
        // Any obligation held for this ticket from a previous tick is superseded by this admission:
        // it is either about to be admitted below, or the ticket already has a live run and the
        // held copy is stale.
        self.pending_manager_wakes.remove(&key);
        // The author already has a live run: reuse the INF-250 mailbox admission rather than a second
        // dispatch path. A full mailbox leaves the row `pending` for the next tick.
        if let Some(re) = self.running.get(&key) {
            let run_id = re.run_id;
            let (_, ok) =
                self.admit_to_mailbox(re, &crate::message::operator_wrap(&row.body), &row.body);
            if ok {
                self.mark_wake_admitted(row, run_id);
            }
            return;
        }
        // No live run: hold the obligation while the SAME dispatch path ordinary selection uses
        // creates the run. `dispatch_or_prepare` either dispatches inline (the hook in
        // `dispatch_issue_prepared` admits and seeds immediately) or begins a preparation, in which
        // case the row stays `pending` and the hook runs when that preparation is accepted. Never a
        // second dispatch path, and never marked spent before the dispatch is recoverable.
        self.pending_manager_wakes.insert(key, row.clone());
        self.dispatch_or_prepare(iss, None, route, String::new());
    }

    /// §7.9 step 2: the moment a wake-driven dispatch makes its author run live. Called from
    /// `dispatch_issue_prepared` (after the running entry exists and the reopen seed point), it
    /// writes the obligation's body as the run's STUDIO-649 seed and admits the row. A seed write
    /// the mailbox rejects leaves the row `admitted` (with no "sent" row), so boot recovery can
    /// return it to `pending` rather than losing it.
    pub(crate) fn admit_pending_manager_wake(&mut self, row: &ManagerWakeRow, issue_id: &str) {
        // The held copy is only admitted while the obligation is STILL `pending` in the store: a
        // recheck since it was held may have `refused` it, or another path may have spent it. A
        // stale entry therefore seeds nothing and changes no state.
        if self
            .store()
            .manager_wake(&row.intervention_id)
            .ok()
            .flatten()
            .is_none_or(|r| r.state != MANAGER_WAKE_PENDING)
        {
            tracing::debug!(
                intervention_id = %row.intervention_id,
                "manager wake: the held obligation is no longer pending; dropping the held admission"
            );
            return;
        }
        self.seed_reopen_summons(issue_id, (self.now)(), &row.body);
        if let Some(run_id) = self.running.get(issue_id).map(|re| re.run_id) {
            self.mark_wake_admitted(row, run_id);
        }
    }

    /// Mark one obligation `admitted` with its run id, charge the author half of its exchange
    /// authorization (§7.8), and spend it (`delivered`) the moment the seed is the run's "sent"
    /// `run_messages` row. The seed is written in the same no-await admission — inline dispatch, or
    /// the [`Orchestrator::admit_pending_manager_wake`] hook — so a healthy store marks it
    /// `delivered` immediately; a seed write that did NOT land leaves the row `admitted`, and boot
    /// recovery returns it to `pending` without the author being lost or woken twice.
    fn mark_wake_admitted(&self, row: &ManagerWakeRow, run_id: i64) {
        self.set_wake(row, MANAGER_WAKE_ADMITTED, Some(run_id), "");
        // §7.8: the author dispatch consumes the author half of its exchange authorization;
        // the review half answering the author's push is then covered by it.
        self.consume_author_round_authorization(&row.pr);
        if self.wake_seed_sent(run_id, &row.body) {
            self.set_wake(row, MANAGER_WAKE_DELIVERED, Some(run_id), "");
        }
    }

    /// §7.9 recovery for one `admitted` row: a still-live run gets the seed re-attempted (the
    /// watermark was not advanced by a failed admission); a dead run returns the row to `pending`.
    fn redeliver_or_recover_manager_wake(&mut self, row: &ManagerWakeRow) {
        let Some(run_id) = row.run_id else {
            self.set_wake(
                row,
                MANAGER_WAKE_PENDING,
                None,
                "the admission recorded no run",
            );
            return;
        };
        let live = self
            .running
            .values()
            .find(|re| re.run_id == run_id)
            .map(|re| (re.issue.id.clone(), re.issue.identifier.clone()));
        let Some((key, identifier)) = live else {
            self.set_wake(
                row,
                MANAGER_WAKE_PENDING,
                None,
                "the run ended before the seed was delivered",
            );
            return;
        };
        self.seed_reopen_summons(&key, (self.now)(), &row.body);
        if self.wake_seed_sent(run_id, &row.body) {
            self.set_wake(row, MANAGER_WAKE_DELIVERED, Some(run_id), "");
            tracing::info!(issue_identifier = %identifier, run_id, "manager wake: the seed was redelivered to the live run");
        }
    }

    /// Best-effort write of a wake row's state. A failed write leaves the row as it was; the next
    /// tick re-reads and retries, and the same no-await discipline the seed path documents applies.
    fn set_wake(&self, row: &ManagerWakeRow, state: &str, run_id: Option<i64>, reason: &str) {
        if let Err(e) =
            self.store()
                .set_manager_wake_state(&row.intervention_id, state, run_id, reason)
        {
            tracing::warn!(
                intervention_id = %row.intervention_id,
                pr = %row.pr,
                err = %e,
                "manager wake: recording the obligation's state failed; it is re-read next tick"
            );
        }
    }
}

/// Find this tick's candidate for a wake obligation's ticket. The obligation carries either the
/// opaque tracker issue id (what ordinary selection keys by) or, when no run row was available, the
/// human identifier — so match either.
fn find_candidate<'a>(
    candidates: &'a [WakeCandidate<'a>],
    issue_id: &str,
) -> Option<(&'a Issue, Option<DispatchRoute>)> {
    if issue_id.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|(iss, _)| iss.id == issue_id || iss.identifier.eq_ignore_ascii_case(issue_id))
        .map(|(iss, route)| (*iss, route.clone()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Manager, Review, ReviewAuthority, ReviewMode, Teams};
    use rhapsody_store::{
        MANAGER_EXCHANGE_ACTIVE, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_CONSUMED,
        MANAGER_INTERVENTION_AWAITING_EFFECT, MANAGER_MODE_ACT, MANAGER_WAKE_ADMITTED,
        MANAGER_WAKE_DELIVERED, MANAGER_WAKE_PENDING, MANAGER_WAKE_REFUSED, ManagerExchange,
        ManagerInterventionRow, ManagerWakeRow, RUN_MESSAGE_SENT, ReviewWatchKey, ReviewWatchRow,
        Sqlite, Store, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{
        HangResolver, empty_effective, empty_resolved_project, issue, set_of,
    };

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const PR_KEY: &str = "makewhatis/rhapsody#12";

    fn wake_orch() -> (Orchestrator, Arc<dyn Store + Send + Sync>) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.review_states = set_of(&["in review"]);
        eff.review_promote_state = "In Progress".to_string();
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        // The project's own scheduling sets, so the MULTI ladder (the one a real install runs) can
        // be exercised beside the legacy one.
        proj.active_states = set_of(&["todo", "in progress"]);
        proj.terminal_states = set_of(&["done"]);
        proj.review_states = set_of(&["in review"]);
        proj.max_concurrent = 10;
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                adjudicate_after_rounds: 1,
                ..Review::default()
            },
            manager: Manager {
                review_authority: ReviewAuthority::Act,
                ..Manager::default()
            },
            ..Teams::disabled()
        });
        let st: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"));
        o.set_store(Arc::clone(&st));
        // A no-op spawn, so dispatch records the running entry and its mailbox without launching a
        // real worker that would drain the mailbox receiver.
        o.spawn = Some(Box::new(|_iss, _attempt, _re| {}));
        (o, st)
    }

    fn seed_watch(o: &Orchestrator, introduced_by: &str, open: bool) {
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".to_string(),
                    repo: "rhapsody".to_string(),
                    number: 12,
                    reviewer: "alice".to_string(),
                },
                author: "bob".to_string(),
                introduced_by: introduced_by.to_string(),
                requested_sha: "deadbeef".to_string(),
                last_reviewed_sha: String::new(),
                status: "reviewed".to_string(),
                open,
            })
            .expect("save watch");
    }

    /// A live PR (generation 1), an intervention `awaiting_effect`, and a `pending` wake for
    /// STUDIO-1 — the state a `ROUTE_TO_AUTHOR` activation leaves behind.
    fn seed_pending_wake(o: &Orchestrator, id: &str, body: &str, issue_id: &str) {
        seed_watch(o, "adopt:STUDIO-1", true);
        o.store()
            .ensure_review_generation(PR_KEY)
            .expect("generation");
        o.store()
            .save_manager_intervention(ManagerInterventionRow {
                id: id.to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                mode: MANAGER_MODE_ACT.to_string(),
                state: MANAGER_INTERVENTION_AWAITING_EFFECT.to_string(),
                ..ManagerInterventionRow::default()
            })
            .expect("intervention");
        o.store()
            .save_manager_wake(ManagerWakeRow {
                intervention_id: id.to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                issue_id: issue_id.to_string(),
                body: body.to_string(),
                state: MANAGER_WAKE_PENDING.to_string(),
                ..ManagerWakeRow::default()
            })
            .expect("wake");
        o.human_holds.begin_pass(true);
    }

    fn wake(o: &Orchestrator, id: &str) -> ManagerWakeRow {
        o.store().manager_wake(id).expect("wake").expect("row")
    }

    fn candidate() -> Issue {
        issue("ID-1", "STUDIO-1", "In Progress")
    }

    fn pump(o: &mut Orchestrator) {
        let cand = candidate();
        o.pump_manager_wakes(&[(&cand, None)]);
    }

    // The headline acceptance: a pending obligation is admitted through today's path — the author is
    // dispatched, the seed is written as the run's "sent" run_message, and the obligation is spent.
    #[test]
    fn a_pending_wake_dispatches_seeds_and_delivers() {
        let (mut o, st) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");

        pump(&mut o);

        let run_id = o
            .running
            .get("ID-1")
            .expect("the author was dispatched")
            .run_id;
        let w = wake(&o, "iv-1");
        assert_eq!(w.state, MANAGER_WAKE_DELIVERED);
        assert_eq!(w.run_id, Some(run_id));
        let msgs = st.list_run_messages(run_id).expect("run messages");
        assert!(
            msgs.iter()
                .any(|m| m.status == RUN_MESSAGE_SENT && m.body == "route instructions"),
            "the seed must be the run's sent message: {msgs:?}"
        );
        assert!(
            !o.manager_wake_blocks_selection("ID-1"),
            "a delivered obligation no longer blocks selection"
        );
    }

    // §7.9 (STUDIO-1017 review B2): on a provider install the wake must dispatch through the SAME
    // prepared-dispatch path ordinary selection uses, never inline on the native login — the
    // STUDIO-1002 B3 defect. The row stays `pending` while the preparation is in flight.
    // MUTATION: call `dispatch_issue` directly from `admit_manager_wake` and this reds (the run
    // would be live and no preparation in flight).
    #[tokio::test]
    async fn the_wake_routes_through_prepared_dispatch() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        o.prepare_resolver = Some(Arc::new(HangResolver));

        pump(&mut o);

        assert!(
            o.running.is_empty(),
            "a provider install must not dispatch the author inline"
        );
        assert!(
            !o.preparing.is_empty(),
            "the wake began a preparation instead"
        );
        assert_eq!(
            wake(&o, "iv-1").state,
            MANAGER_WAKE_PENDING,
            "the obligation stays unspent while the preparation is in flight"
        );
    }

    // §7.9: the seed body is the validated decision's, never a comment.
    #[test]
    fn the_seed_is_the_obligation_body() {
        let (mut o, st) = wake_orch();
        seed_pending_wake(&o, "iv-1", "fix alice:F1 @ r1", "STUDIO-1");
        pump(&mut o);
        let run_id = o.running.get("ID-1").expect("running").run_id;
        let msgs = st.list_run_messages(run_id).expect("run messages");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "fix alice:F1 @ r1");
    }

    // §7.9 recheck: a hold applied before admission refuses the row, and nothing is dispatched.
    #[test]
    fn a_hold_before_admission_refuses_the_wake() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        o.human_holds.note_human_label("STUDIO-1");

        pump(&mut o);

        assert!(o.running.is_empty(), "a held ticket is not woken");
        let w = wake(&o, "iv-1");
        assert_eq!(w.state, MANAGER_WAKE_REFUSED);
        assert!(w.reason.contains("hold"), "reason = {:?}", w.reason);
    }

    // §7.9 recheck: a generation change before admission refuses the row.
    #[test]
    fn a_generation_change_before_admission_refuses_the_wake() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        o.store()
            .increment_review_generation(PR_KEY)
            .expect("new generation");

        pump(&mut o);

        assert!(o.running.is_empty());
        assert_eq!(wake(&o, "iv-1").state, MANAGER_WAKE_REFUSED);
    }

    // §7.9 recheck: a closed pull request refuses the row.
    #[test]
    fn a_closed_pull_request_refuses_the_wake() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        seed_watch(&o, "adopt:STUDIO-1", false);

        pump(&mut o);

        assert!(o.running.is_empty());
        assert_eq!(wake(&o, "iv-1").state, MANAGER_WAKE_REFUSED);
    }

    // §7.9 crash recovery: a real admission marks the obligation `admitted` — NOT `delivered` when
    // the seed did not land — and a crash before delivery returns it to `pending` so the author is
    // admitted again in the new run: woken at most once per live run, never lost.
    // MUTATION: mark the row `delivered` at admission and the first assert reds (the obligation
    // would never reset at boot).
    #[test]
    fn a_crash_between_admission_and_delivery_returns_the_wake_to_pending() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");

        // Admission: the author is dispatched, then the REAL admission hook runs, but the run's
        // mailbox is absent so the seed write does not land. The obligation is marked `admitted`
        // with its run id and no "sent" row exists — exactly the crash window the boot recovery
        // exists for.
        o.dispatch_issue(candidate(), None, None, String::new());
        let run_id = o.running.get("ID-1").expect("running").run_id;
        o.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove("ID-1");
        let row = wake(&o, "iv-1");
        o.admit_pending_manager_wake(&row, "ID-1");

        let w = wake(&o, "iv-1");
        assert_eq!(
            w.state, MANAGER_WAKE_ADMITTED,
            "a seed that did not land must leave the obligation `admitted`, never `delivered`"
        );
        assert_eq!(w.run_id, Some(run_id));

        // The daemon crashes: the run died with it before the seed was delivered.
        o.running.clear();
        o.recover_manager_wakes();

        let w = wake(&o, "iv-1");
        assert_eq!(w.state, MANAGER_WAKE_PENDING);
        assert_eq!(w.run_id, None, "the dead run is forgotten");

        // …and the re-admission works: the author is woken exactly once in the new run.
        pump(&mut o);
        let w = wake(&o, "iv-1");
        assert_eq!(w.state, MANAGER_WAKE_DELIVERED);
        assert_eq!(o.running.len(), 1, "exactly one author run");
    }

    // A second tick after a delivery dispatches nothing: the author is woken once per live run.
    #[test]
    fn a_delivered_wake_is_never_admitted_twice() {
        let (mut o, st) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        pump(&mut o);
        let run_id = o.running.get("ID-1").expect("running").run_id;

        pump(&mut o);

        assert_eq!(o.running.len(), 1, "one run, not two");
        assert_eq!(
            st.list_run_messages(run_id).expect("msgs").len(),
            1,
            "one seed, not two"
        );
    }

    // §7.8: the author dispatch consumes the author half of its `author_round` authorization, so the
    // review round answering the author's push is covered by it and nothing re-uses it.
    // MUTATION: drop the consumption and the review half never arms (this assert reds).
    #[test]
    fn admission_consumes_the_author_round_authorization() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        o.store()
            .save_manager_exchange(ManagerExchange {
                id: "iv-1-author_round".to_string(),
                intervention_id: "iv-1".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                kind: MANAGER_EXCHANGE_AUTHOR_ROUND.to_string(),
                authorized_head: "deadbeef".to_string(),
                authorized_patch_id: "patch-1".to_string(),
                state: MANAGER_EXCHANGE_ACTIVE.to_string(),
            })
            .expect("exchange");

        pump(&mut o);

        let rows = o.store().manager_exchanges(PR_KEY).expect("exchanges");
        let e = rows
            .iter()
            .find(|e| e.id == "iv-1-author_round")
            .expect("row");
        assert_eq!(e.state, MANAGER_EXCHANGE_CONSUMED);
    }

    // §7.9: a live author run is admitted through the existing mailbox path, never a second dispatch.
    #[test]
    fn a_live_author_run_is_admitted_through_the_mailbox() {
        let (mut o, st) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        // A live author run already exists (a mid-run route).
        o.dispatch_issue(candidate(), None, None, String::new());
        let run_id = o.running.get("ID-1").expect("running").run_id;

        pump(&mut o);

        assert_eq!(o.running.len(), 1, "no second dispatch");
        assert_eq!(wake(&o, "iv-1").state, MANAGER_WAKE_DELIVERED);
        let msgs = st.list_run_messages(run_id).expect("msgs");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "route instructions");
    }

    // §7.9: the row is spent ONLY once the seed is written. A full author mailbox rejects the
    // admission, so the row stays `pending` for the next tick and is never marked delivered.
    // MUTATION: mark the row delivered at admission (before the seed is written) and this reds.
    #[test]
    fn a_full_mailbox_leaves_the_wake_pending_and_unspent() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        o.dispatch_issue(candidate(), None, None, String::new());
        let re = o.running.get("ID-1").expect("running").clone();
        for i in 0..crate::message::OPERATOR_MAILBOX_CAP {
            assert!(
                o.deliver_to_mailbox(&re, "fill").1,
                "fill {i} rejected before cap"
            );
        }

        pump(&mut o);

        let w = wake(&o, "iv-1");
        assert_eq!(
            w.state, MANAGER_WAKE_PENDING,
            "a rejected seed must not spend the obligation"
        );
        assert_eq!(w.run_id, None);
    }

    // §7.9: while the obligation is unspent, ordinary selection does not dispatch the ticket.
    #[test]
    fn selection_skips_a_ticket_with_an_unspent_wake() {
        let (o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        let picked = o.select_dispatch(vec![candidate()]);
        assert!(
            picked.is_empty(),
            "a pending wake obligation owns the dispatch: {picked:?}"
        );
    }

    /// The candidate tagged with the project at index 0, for the MULTI ladder — the pass a real
    /// installation actually runs (`loop.rs` takes `select_dispatch_multi_after_fetch` whenever
    /// `eff.projects` is non-empty, and `effective.rs` gives even a single-project WORKFLOW one
    /// resolved project).
    fn multi_candidate() -> crate::select::TaggedIssue {
        crate::select::TaggedIssue {
            iss: candidate(),
            proj: Some(0),
        }
    }

    fn pr_coord() -> crate::prstate::PrCoord {
        crate::prstate::PrCoord::new("makewhatis", "rhapsody", 12)
    }

    // §15.4: ordinary selection skips a ticket with a pending wake obligation — on the MULTI ladder
    // too, which is the one production runs. MUTATION: remove `manager_wake_blocks_selection` from
    // the multi ladder's active branch and this reds.
    #[test]
    fn the_multi_ladder_skips_a_ticket_with_an_unspent_wake() {
        let (o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
        let (picked, reopen, _) = o.select_dispatch_multi_with_reopens(vec![multi_candidate()]);
        let identifiers: Vec<&str> = picked.iter().map(|t| t.iss.identifier.as_str()).collect();
        assert!(
            picked.is_empty(),
            "a pending wake obligation owns the dispatch: {identifiers:?}"
        );
        assert!(reopen.is_empty());
    }

    // §7.8 path 3 (STUDIO-1017 review B1): after the threshold in `act` mode, the MULTI ladder — the
    // one production runs — refuses an author dispatch with no `author_round` authorization, and
    // permits it with one. MUTATION: drop the `author_dispatch_authorized` gate from
    // `select_dispatch_multi_after_fetch` and the first assert reds.
    #[test]
    fn the_multi_ladder_gates_a_post_threshold_author_dispatch() {
        let (mut o, _) = wake_orch();
        seed_watch(&o, "adopt:STUDIO-1", true);
        o.store()
            .ensure_review_generation(PR_KEY)
            .expect("generation");
        o.review_rounds
            .insert(crate::reviewwatch::churn_key(&pr_coord()), 1);

        let (picked, _, _) = o.select_dispatch_multi_with_reopens(vec![multi_candidate()]);
        let identifiers: Vec<&str> = picked.iter().map(|t| t.iss.identifier.as_str()).collect();
        assert!(
            picked.is_empty(),
            "no authorization, so the post-threshold author must not be dispatched: {identifiers:?}"
        );

        o.store()
            .save_manager_exchange(ManagerExchange {
                id: "iv-1-author_round".to_string(),
                intervention_id: "iv-1".to_string(),
                pr: PR_KEY.to_string(),
                generation: 1,
                kind: MANAGER_EXCHANGE_AUTHOR_ROUND.to_string(),
                authorized_head: "deadbeef".to_string(),
                authorized_patch_id: "patch-1".to_string(),
                state: MANAGER_EXCHANGE_ACTIVE.to_string(),
            })
            .expect("exchange");

        let (picked, _, _) = o.select_dispatch_multi_with_reopens(vec![multi_candidate()]);
        assert_eq!(picked.len(), 1, "the authorization permits the dispatch");
    }

    // `off`/`advise` never admit anything (byte-identical): no expense is charged.
    #[test]
    fn off_and_advise_admit_nothing() {
        for authority in [ReviewAuthority::Off, ReviewAuthority::Advise] {
            let (mut o, _) = wake_orch();
            o.teams.as_mut().expect("teams").manager.review_authority = authority;
            seed_pending_wake(&o, "iv-1", "route instructions", "STUDIO-1");
            pump(&mut o);
            assert!(
                o.running.is_empty(),
                "{authority:?} must not wake the author"
            );
            assert_eq!(wake(&o, "iv-1").state, MANAGER_WAKE_PENDING);
        }
    }

    // A malformed/empty wake issue id never matches a candidate, so the row stays pending.
    #[test]
    fn a_wake_with_no_matching_candidate_stays_pending() {
        let (mut o, _) = wake_orch();
        seed_pending_wake(&o, "iv-1", "route instructions", "OTHER-9");
        pump(&mut o);
        assert!(o.running.is_empty());
        assert_eq!(wake(&o, "iv-1").state, MANAGER_WAKE_PENDING);
    }
}
