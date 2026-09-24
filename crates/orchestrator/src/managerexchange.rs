//! managerexchange — the manager's EXCHANGE AUTHORIZATIONS, and the review-side gates that honour
//! them (STUDIO-1012, design record `manager-agent-design.md` §7.8, M5).
//!
//! **No Go v0.4.0 counterpart.** The whole ticketless review loop and the manager that adjudicates
//! it are Rhapsody additions.
//!
//! # The claim, narrowed to what M5 enforces
//!
//! In `act` mode, after a pull request's round threshold is reached, a **review round** happens
//! only under an active manager exchange authorization. This module owns the review-side half of
//! that: the watcher's arming (`reviewwatch.rs`), re-introduction on handoff (`reviewintro.rs`),
//! and the daemon's automatic route-back on a findings verdict (`reviewchanges.rs` and its
//! completion comment in `reviewnotify.rs`).
//!
//! The WRITER of an authorization — the manager's activation transaction, §7.7 — is a later slice.
//! Until it lands no authorization exists, so an `act` install arms no gated round; that is exactly
//! why the ticket requires `manager.review_authority: act` to be inert until the rest of the
//! program is in place, and why `off`/`advise` are byte-identical to a daemon built before this
//! ticket. Nothing here runs before the threshold, and nothing here runs in `off` or `advise`.
//!
//! # What "after the threshold" means
//!
//! STUDIO-1004's count of answered exchanges: the per-pull-request round counter, in rounds, against
//! `review.adjudicate_after_rounds`. It is evaluated **when arming** — the same predicate the
//! adjudication branch already uses ([`Orchestrator::adjudication_threshold`] and the counter
//! beside it). An exchange already in flight when the threshold is crossed was armed before the
//! crossing and completes normally: it is a live run, not an arming, so it never reaches this gate.
//!
//! # Retries and continuations consume nothing new
//!
//! An authorization is consumed by the ACCEPTANCE of a review dispatch
//! ([`Orchestrator::consume_review_round_authorization`], called from `commit_review_watch` — the
//! same acceptance point that charges the round budget), and a retry or continuation of an
//! already-dispatched run is the same exchange: it goes through `retry.rs`'s `attempt` path, never
//! through the watcher's arm. There is therefore no second consumption for one exchange.

use rhapsody_config::teams::ReviewAuthority;
use rhapsody_store::{
    MANAGER_EXCHANGE_ACTIVE, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_CONSUMED,
    MANAGER_EXCHANGE_INVALIDATED, MANAGER_EXCHANGE_REVIEW_ROUND, ManagerExchange,
};

use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;

impl Orchestrator {
    /// The installation's manager review authority, or [`ReviewAuthority::Off`] when Teams is off or
    /// review is not on the ticketless path (see [`Teams::manager_review_authority`]).
    ///
    /// [`Teams::manager_review_authority`]: rhapsody_config::teams::Teams::manager_review_authority
    pub(crate) fn manager_review_authority(&self) -> ReviewAuthority {
        self.teams
            .as_ref()
            .map_or(ReviewAuthority::Off, |t| t.manager_review_authority())
    }

    /// Whether the §7.8 review-side gate applies to `pr` right now: `act` mode, and the round
    /// threshold reached (STUDIO-1004's count of answered exchanges). `false` before the threshold
    /// and in every mode but `act`, which is what makes those paths byte-identical.
    pub(crate) fn review_exchange_gate_active(&self, pr: &PrCoord) -> bool {
        self.manager_review_authority() == ReviewAuthority::Act
            && self
                .adjudication_threshold()
                .is_some_and(|threshold| self.rounds_used(pr) >= threshold)
    }

    /// §7.8: consumes the author half of an active [`MANAGER_EXCHANGE_AUTHOR_ROUND`] when the wake
    /// admission dispatches the author it covers (`active` → `consumed`). The authorization then
    /// stays live for its review half — the round answering the author's push — and nothing else may
    /// consume it.
    ///
    /// Called by [`crate::managerwake`]'s admission, which is the `ROUTE_TO_AUTHOR` path §7.8 names.
    /// A retry or continuation is the SAME exchange and never reaches here.
    pub(crate) fn consume_author_round_authorization(&self, pr: &str) {
        let exchanges = match self.store().manager_exchanges(pr) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    pr = %pr, err = %e,
                    "manager exchange: the authorization store could not be read; the author-round \
                     authorization was not consumed"
                );
                return;
            }
        };
        for exchange in exchanges {
            if exchange.kind != MANAGER_EXCHANGE_AUTHOR_ROUND
                || exchange.state != MANAGER_EXCHANGE_ACTIVE
            {
                continue;
            }
            if let Err(e) = self
                .store()
                .set_manager_exchange_state(&exchange.id, MANAGER_EXCHANGE_CONSUMED)
            {
                tracing::warn!(
                    pr = %pr, id = %exchange.id, err = %e,
                    "manager exchange: consuming the author-round authorization failed"
                );
                return;
            }
            tracing::info!(
                pr = %pr, id = %exchange.id,
                "manager exchange: the author dispatch consumed its author-round authorization"
            );
            return;
        }
    }

    /// The pull request this ticket's review loop is on, from the daemon's OWN record: the first
    /// open live watch row whose origin ticket is `iss`, falling back to the tracker's linkage for
    /// an install whose repository is not connected (`linked_prs` empty).
    ///
    /// `None` when the ticket is on no open pull request — nothing to gate.
    pub(crate) fn manager_issue_pr(&self, iss: &rhapsody_core::Issue) -> Option<PrCoord> {
        if let Ok(rows) = self.store().load_live_review_watch() {
            for r in rows {
                if !r.open {
                    continue;
                }
                if crate::reviewdone::origin_ticket(&r.introduced_by)
                    .is_some_and(|t| t.eq_ignore_ascii_case(&iss.identifier))
                {
                    return Some(PrCoord::new(&r.key.owner, &r.key.repo, r.key.number));
                }
            }
        }
        iss.linked_prs
            .iter()
            .flatten()
            .filter(|p| !p.merged)
            .map(|p| PrCoord::new(&p.owner, &p.repo, p.number))
            .next()
    }

    /// Whether `pr` carries an ACTIVE [`MANAGER_EXCHANGE_AUTHOR_ROUND`] — an author dispatch the
    /// manager authorized after the threshold, not yet consumed.
    fn active_author_round(&self, pr: &PrCoord) -> bool {
        let key = crate::reviewwatch::churn_key(pr);
        match self.store().manager_exchanges(&key) {
            Ok(rows) => rows.iter().any(|e| {
                e.kind == MANAGER_EXCHANGE_AUTHOR_ROUND && e.state == MANAGER_EXCHANGE_ACTIVE
            }),
            Err(e) => {
                // Fail CLOSED: an unreadable store might hold the authorization this dispatch needs,
                // and dispatching without one is the direction §7.8 forbids.
                tracing::warn!(
                    pr = %pr, err = %e,
                    "manager exchange: the authorization store could not be read; the author \
                     dispatch waits"
                );
                false
            }
        }
    }

    /// §7.8 path 3: whether ordinary selection may dispatch `iss` as an author round right now.
    ///
    /// `true` unless the gate is active for the ticket's pull request — `act` mode, past the round
    /// threshold — in which case an active `author_round` authorization is required. `off`/`advise`
    /// and every pre-threshold dispatch are therefore byte-identical.
    ///
    /// **The wake admission owns consumption.** This is a permission check only: the activation
    /// transaction writes the authorization, and [`crate::managerwake`]'s admission consumes it when
    /// it wakes the author through the obligation. Ordinary selection is a safety net that refuses a
    /// post-threshold dispatch with no authorization, never a second consumer.
    pub(crate) fn author_dispatch_authorized(&self, iss: &rhapsody_core::Issue) -> bool {
        if self.manager_review_authority() != ReviewAuthority::Act
            || self.adjudication_threshold().is_none()
        {
            return true;
        }
        let Some(pr) = self.manager_issue_pr(iss) else {
            return true;
        };
        if !self.review_exchange_gate_active(&pr) {
            return true;
        }
        self.active_author_round(&pr)
    }

    /// Whether a review round may ARM for `pr` at `head` under the §7.8 gate.
    ///
    /// Returns `true` when arming is allowed:
    /// * the gate is inactive (off/advise, or before the threshold) — nothing is gated; or
    /// * a live manager exchange authorization covers this round.
    ///
    /// Returns `false` when the round must be deferred: the gate is active and no usable
    /// authorization exists. A live authorization that has been invalidated — a new generation, a
    /// hold, or a `review_round`'s patch-id move before it was consumed — is marked `invalidated`
    /// before returning, so it arms nothing even after the triggering condition clears.
    ///
    /// **This PEEKS; it does not consume.** Consumption happens once the dispatch is ACCEPTED
    /// ([`Self::consume_review_round_authorization`], called from `commit_review_watch`), so a round
    /// refused further down — by a provider budget, a model refusal or a drain — does not burn the
    /// authorization it never used.
    ///
    /// `held` is whether the pull request's origin ticket currently wears the `rhapsody:human` hold;
    /// `head_patch_id` is the current head's patch-id, or empty when unknown (which fails closed on
    /// the patch-id check but still requires a head match).
    pub(crate) fn review_round_arm_authorized(
        &self,
        pr: &PrCoord,
        head: &str,
        head_patch_id: &str,
        held: bool,
    ) -> bool {
        if !self.review_exchange_gate_active(pr) {
            return true;
        }
        let key = crate::reviewwatch::churn_key(pr);
        let generation = self
            .store()
            .review_bound(&key)
            .ok()
            .flatten()
            .map_or(0, |bound| bound.generation);
        let exchanges = match self.store().manager_exchanges(&key) {
            Ok(rows) => rows,
            Err(e) => {
                // Fail CLOSED: a store that cannot be read might hold the authorization this round
                // needs, and arming without one is the one direction the bound forbids.
                tracing::warn!(
                    pr = %pr, err = %e,
                    "manager exchange: the authorization store could not be read; the round waits"
                );
                return false;
            }
        };
        for exchange in exchanges {
            if exchange.state != MANAGER_EXCHANGE_ACTIVE
                && exchange.state != MANAGER_EXCHANGE_CONSUMED
            {
                continue;
            }
            // Invalidation (§7.8): a new generation, or a hold. The pull request itself is open by
            // construction here — this is only reached for a live watch row.
            if exchange.generation != generation || held {
                self.invalidate_exchange(&exchange, pr);
                continue;
            }
            match exchange.kind.as_str() {
                MANAGER_EXCHANGE_REVIEW_ROUND => {
                    // A `review_round` covers exactly the change it was granted for.
                    if exchange.authorized_head != head {
                        continue;
                    }
                    if exchange.state == MANAGER_EXCHANGE_ACTIVE
                        && !exchange.authorized_patch_id.is_empty()
                        && exchange.authorized_patch_id != head_patch_id
                    {
                        // A patch-id move before it was consumed invalidates it.
                        self.invalidate_exchange(&exchange, pr);
                        continue;
                    }
                    return true;
                }
                MANAGER_EXCHANGE_AUTHOR_ROUND => {
                    // The review half of an author-round authorization answers whatever head the
                    // author's push produced, so the recorded head is deliberately NOT compared. It
                    // is usable only once the author dispatch has CONSUMED it — an `active`
                    // author_round means the author has not been woken yet, and arming the review
                    // ahead of them is exactly the exchange ordering §7.8 forbids.
                    if exchange.state != MANAGER_EXCHANGE_CONSUMED {
                        continue;
                    }
                    return true;
                }
                _ => continue,
            }
        }
        false
    }

    /// Consumes the review-round authorization an accepted arm used (active → consumed), when the
    /// gate is active. Called from `commit_review_watch`, the moment a review dispatch is ACCEPTED —
    /// so the authorization is spent only by a round that is actually going out, and the other rows
    /// of one round leave it `consumed` beside them.
    ///
    /// An `author_round` authorization is deliberately NOT consumed here: its author-dispatch half
    /// consumes it, and this is only the review half answering the author's push.
    pub(crate) fn consume_review_round_authorization(
        &self,
        pr: &PrCoord,
        head: &str,
        head_patch_id: &str,
    ) {
        if !self.review_exchange_gate_active(pr) {
            return;
        }
        let key = crate::reviewwatch::churn_key(pr);
        let exchanges = match self.store().manager_exchanges(&key) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    pr = %pr, err = %e,
                    "manager exchange: the authorization store could not be read at acceptance; \
                     the review-round authorization was not consumed"
                );
                return;
            }
        };
        for exchange in exchanges {
            if exchange.kind != MANAGER_EXCHANGE_REVIEW_ROUND
                || exchange.state != MANAGER_EXCHANGE_ACTIVE
                || exchange.authorized_head != head
            {
                continue;
            }
            if !exchange.authorized_patch_id.is_empty()
                && exchange.authorized_patch_id != head_patch_id
            {
                continue;
            }
            if let Err(e) = self
                .store()
                .set_manager_exchange_state(&exchange.id, MANAGER_EXCHANGE_CONSUMED)
            {
                tracing::warn!(
                    pr = %pr, id = %exchange.id, err = %e,
                    "manager exchange: consuming the review-round authorization failed"
                );
                return;
            }
            tracing::info!(
                pr = %pr, id = %exchange.id, head = %head,
                "manager exchange: the review round was armed under its authorization"
            );
            return;
        }
    }

    /// Marks one live authorization `invalidated`, logging the transition. Best-effort like every
    /// other control-task store write: a failed write leaves the row live, and the next arm attempt
    /// re-checks and tries again rather than arming on it.
    fn invalidate_exchange(&self, exchange: &ManagerExchange, pr: &PrCoord) {
        if let Err(e) = self
            .store()
            .set_manager_exchange_state(&exchange.id, MANAGER_EXCHANGE_INVALIDATED)
        {
            tracing::warn!(
                pr = %pr, id = %exchange.id, err = %e,
                "manager exchange: invalidating an authorization failed; it will be re-checked on \
                 the next arm attempt"
            );
            return;
        }
        tracing::info!(
            pr = %pr, id = %exchange.id, kind = %exchange.kind,
            "manager exchange: an authorization was invalidated and arms nothing"
        );
    }

    /// Invalidates every live authorization for `pr` — the pull request leaving the watch set
    /// (merged, closed or dismissed). The generation and hold triggers are handled lazily at arming
    /// ([`Self::review_round_arm_authorized`]); this is the one trigger with no arming attempt left to
    /// observe it.
    pub(crate) fn invalidate_manager_exchanges(&self, pr: &PrCoord) {
        let key = crate::reviewwatch::churn_key(pr);
        if let Err(e) = self.store().invalidate_manager_exchanges(&key) {
            tracing::warn!(
                pr = %pr, err = %e,
                "manager exchange: invalidating the pull request's authorizations failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Manager, Review, ReviewAuthority, ReviewMode, Teams};
    use rhapsody_store::{
        MANAGER_EXCHANGE_ACTIVE, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_COMPLETED,
        MANAGER_EXCHANGE_CONSUMED, MANAGER_EXCHANGE_INVALIDATED, MANAGER_EXCHANGE_REVIEW_ROUND,
        ManagerExchange, ReviewWatchKey, ReviewWatchRow, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{empty_effective, issue};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PATCH: &str = "patch-one";
    const OTHER_PATCH: &str = "patch-two";

    fn teams(authority: ReviewAuthority, threshold: i64) -> Teams {
        Teams {
            enabled: true,
            manager: Manager {
                review_authority: authority,
                ..Manager::default()
            },
            review: Review {
                mode: ReviewMode::Ticketless,
                adjudicate_after_rounds: threshold,
                ..Review::default()
            },
            ..Teams::disabled()
        }
    }

    /// `pr`'s churn key, with the round counter charged `dispatches` times (one dispatch per round
    /// at the default one reviewer).
    fn pr() -> PrCoord {
        PrCoord::new("makewhatis", "rhapsody", 64)
    }

    fn with_rounds(authority: ReviewAuthority, threshold: i64, dispatches: usize) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(empty_effective(Arc::new(Fake::new())));
        o.teams = Some(teams(authority, threshold));
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        if dispatches > 0 {
            o.review_rounds
                .insert(crate::reviewwatch::churn_key(&pr()), dispatches);
        }
        o
    }

    /// Establishes generation 1 for `pr` and writes one authorization, returning its id.
    fn authorize(o: &Orchestrator, kind: &str, state: &str) -> String {
        let key = crate::reviewwatch::churn_key(&pr());
        o.store()
            .ensure_review_generation(&key)
            .expect("generation");
        let id = format!("{kind}-{state}");
        o.store()
            .save_manager_exchange(ManagerExchange {
                id: id.clone(),
                intervention_id: "iv-1".to_string(),
                pr: key,
                generation: 1,
                kind: kind.to_string(),
                authorized_head: HEAD.to_string(),
                authorized_patch_id: PATCH.to_string(),
                state: state.to_string(),
            })
            .expect("save exchange");
        id
    }

    fn state_of(o: &Orchestrator, id: &str) -> String {
        o.store()
            .manager_exchanges(&crate::reviewwatch::churn_key(&pr()))
            .expect("exchanges")
            .into_iter()
            .find(|e| e.id == id)
            .map(|e| e.state)
            .expect("exchange row")
    }

    /// The gate's headline acceptance: in `act` mode past the threshold, a round arms only under an
    /// authorization — with none, the arm is refused.
    #[test]
    fn an_act_round_past_the_threshold_needs_an_authorization() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        assert!(
            !o.review_round_arm_authorized(&pr(), HEAD, PATCH, false),
            "no authorization exists, so the round must not arm"
        );
    }

    /// The authorized half: an active `review_round` at this head lets the round arm, and ACCEPTING
    /// the dispatch consumes it (active → consumed). A peek alone consumes nothing.
    #[test]
    fn an_active_review_round_authorization_arms_and_is_consumed() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);

        assert!(o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
        assert_eq!(
            state_of(&o, &id),
            MANAGER_EXCHANGE_ACTIVE,
            "peeking must not consume; only acceptance does"
        );
        o.consume_review_round_authorization(&pr(), HEAD, PATCH);
        assert_eq!(
            state_of(&o, &id),
            MANAGER_EXCHANGE_CONSUMED,
            "acceptance consumes the authorization"
        );
    }

    /// The other rows of one round: once consumed, the same authorization still covers an arm at
    /// the same head, and a second acceptance does not change its state.
    #[test]
    fn a_consumed_authorization_still_covers_the_rest_of_the_round() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_CONSUMED);

        assert!(o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
        assert!(o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
        o.consume_review_round_authorization(&pr(), HEAD, PATCH);
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_CONSUMED);
    }

    /// An `author_round` authorization covers the review half at whatever head the author's push
    /// produced — the recorded head is deliberately not compared, and accepting the review round
    /// does not consume it (its author-dispatch half already did).
    #[test]
    fn an_author_round_authorization_covers_the_review_half_at_any_head() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_CONSUMED);

        assert!(o.review_round_arm_authorized(
            &pr(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "",
            false
        ));
        o.consume_review_round_authorization(&pr(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "");
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_CONSUMED);
    }

    /// An `author_round` that the author dispatch has NOT yet consumed arms no review: the review
    /// half answers the author's push, so arming ahead of the author is the ordering §7.8 forbids.
    #[test]
    fn an_active_author_round_does_not_arm_the_review_half() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        authorize(&o, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_ACTIVE);

        assert!(!o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
    }

    /// Nothing is gated before the threshold: the same fixture with the counter below the threshold
    /// arms with no authorization at all.
    #[test]
    fn an_act_round_before_the_threshold_needs_no_authorization() {
        let o = with_rounds(ReviewAuthority::Act, 3, 1);
        assert!(o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
    }

    /// `off` and `advise` add no gating — the byte-identical half, with the counter PAST the
    /// threshold and an authorization available-but-unneeded.
    ///
    /// MUTATION: gate in `off` (or `advise`) and this reds.
    #[test]
    fn off_and_advise_arm_without_consuming_an_authorization() {
        for authority in [ReviewAuthority::Off, ReviewAuthority::Advise] {
            let o = with_rounds(authority, 1, 1);
            let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);
            assert!(
                o.review_round_arm_authorized(&pr(), HEAD, PATCH, false),
                "{authority:?} must not gate"
            );
            o.consume_review_round_authorization(&pr(), HEAD, PATCH);
            assert_eq!(
                state_of(&o, &id),
                MANAGER_EXCHANGE_ACTIVE,
                "{authority:?} must not consume an authorization"
            );
        }
    }

    /// Invalidation: a patch-id move before a `review_round` was consumed invalidates it, and it
    /// arms nothing.
    #[test]
    fn a_patch_id_move_before_consumption_invalidates() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);

        assert!(
            !o.review_round_arm_authorized(&pr(), HEAD, OTHER_PATCH, false),
            "a different patch-id at the same head is not the authorized change"
        );
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_INVALIDATED);
    }

    /// Invalidation: a new generation invalidates every live authorization.
    #[test]
    fn a_new_generation_invalidates() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);
        o.store()
            .increment_review_generation(&crate::reviewwatch::churn_key(&pr()))
            .expect("clear");

        assert!(!o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_INVALIDATED);
    }

    /// Invalidation: a hold invalidates every live authorization.
    #[test]
    fn a_hold_invalidates() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);

        assert!(!o.review_round_arm_authorized(&pr(), HEAD, PATCH, true));
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_INVALIDATED);
    }

    /// A terminal authorization (completed) arms nothing and is left exactly as it is.
    #[test]
    fn a_terminal_authorization_arms_nothing() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(
            &o,
            MANAGER_EXCHANGE_REVIEW_ROUND,
            MANAGER_EXCHANGE_COMPLETED,
        );

        assert!(!o.review_round_arm_authorized(&pr(), HEAD, PATCH, false));
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_COMPLETED);
    }

    /// A `review_round` at a DIFFERENT head than this arm is not a usable authorization: a round
    /// nobody authorized at this change must not arm.
    #[test]
    fn a_review_round_at_another_head_arms_nothing() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);

        assert!(!o.review_round_arm_authorized(
            &pr(),
            "cccccccccccccccccccccccccccccccccccccccc",
            PATCH,
            false
        ));
    }

    /// The pull request leaving the watch set invalidates every live authorization, so a rebuilt or
    /// reopened pull request never inherits one.
    #[test]
    fn leaving_the_watch_set_invalidates_every_live_authorization() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        let id = authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);
        o.invalidate_manager_exchanges(&pr());
        assert_eq!(state_of(&o, &id), MANAGER_EXCHANGE_INVALIDATED);
    }

    // --- STUDIO-1017: the author-dispatch half of §7.8 path 3 --------------------------------

    /// An open watch row whose origin ticket is `ticket`, so `manager_issue_pr` resolves the ticket's
    /// pull request from the daemon's own record.
    fn seed_watch(o: &Orchestrator, ticket: &str) {
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".to_string(),
                    repo: "rhapsody".to_string(),
                    number: 64,
                    reviewer: "alice".to_string(),
                },
                author: "bob".to_string(),
                introduced_by: format!("adopt:{ticket}"),
                requested_sha: HEAD.to_string(),
                last_reviewed_sha: String::new(),
                status: "reviewed".to_string(),
                open: true,
            })
            .expect("watch");
    }

    fn author() -> rhapsody_core::Issue {
        issue("ID-1", "STUDIO-1", "In Progress")
    }

    /// After the threshold in `act` mode, an author dispatch needs an active `author_round`
    /// authorization. MUTATION: answer `true` unconditionally and the first assert reds.
    #[test]
    fn an_act_post_threshold_author_dispatch_needs_an_authorization() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        seed_watch(&o, "STUDIO-1");
        assert!(
            !o.author_dispatch_authorized(&author()),
            "no authorization exists, so the author must not be dispatched"
        );
        let id = authorize(&o, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_ACTIVE);
        assert!(
            o.author_dispatch_authorized(&author()),
            "an active author_round authorization permits the dispatch"
        );
        assert_eq!(
            state_of(&o, &id),
            MANAGER_EXCHANGE_ACTIVE,
            "the permission check must not consume the authorization (the wake admission does)"
        );
    }

    /// A `review_round` authorization does NOT authorize an author dispatch: the kinds are distinct.
    #[test]
    fn a_review_round_authorization_does_not_authorize_the_author() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        seed_watch(&o, "STUDIO-1");
        authorize(&o, MANAGER_EXCHANGE_REVIEW_ROUND, MANAGER_EXCHANGE_ACTIVE);
        assert!(!o.author_dispatch_authorized(&author()));
    }

    /// Before the threshold, and in `off`/`advise`, the author dispatch is not gated at all.
    #[test]
    fn off_advise_and_before_the_threshold_authorize_the_dispatch() {
        for o in [
            with_rounds(ReviewAuthority::Act, 3, 1),
            with_rounds(ReviewAuthority::Off, 1, 1),
            with_rounds(ReviewAuthority::Advise, 1, 1),
        ] {
            seed_watch(&o, "STUDIO-1");
            assert!(
                o.author_dispatch_authorized(&author()),
                "the gate must be inert here"
            );
        }
    }

    /// A ticket on no open pull request is not gated: there is nothing to bound.
    #[test]
    fn a_ticket_without_a_pull_request_is_not_gated() {
        let o = with_rounds(ReviewAuthority::Act, 1, 1);
        assert!(o.author_dispatch_authorized(&author()));
    }

    /// §7.8 "Retries?": a retry or continuation of a run already dispatched under an authorization
    /// is the SAME exchange (`attempt` is `Some`), goes through the retry path, and consumes nothing
    /// new. MUTATION: make the retry path consume an authorization and this reds.
    #[tokio::test]
    async fn a_retry_consumes_no_authorization() {
        let mut o = with_rounds(ReviewAuthority::Act, 1, 1);
        seed_watch(&o, "STUDIO-1");
        let id = authorize(&o, MANAGER_EXCHANGE_AUTHOR_ROUND, MANAGER_EXCHANGE_ACTIVE);

        // The retry path (`attempt` is `Some`) — the same entry `on_retry` uses.
        o.dispatch_issue(author(), Some(2), None, String::new());

        assert_eq!(
            state_of(&o, &id),
            MANAGER_EXCHANGE_ACTIVE,
            "a retry is the same exchange and consumes nothing new"
        );
    }
}
