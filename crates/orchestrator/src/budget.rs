//! budget — per-provider daily token budgets (STUDIO-957).
//!
//! **No Go counterpart.** The frozen Symphony reference has no budget concept, and no meter that can
//! attribute spend to an account at all; every name here is the additive Rhapsody surface the ticket
//! specifies.
//!
//! The incident this exists for: 987M tokens were spent in one day, and the figure an operator could
//! ACT on was not the total but the 361M that touched the constrained Claude account — reconstructed
//! by hand with a SQL join, five days into a weekly allowance, after somebody read a number off a
//! board card. The meter (tokens per provider per day) is served by
//! `GET /api/v1/metrics/providers`; this module is the STOP — a per-provider daily ceiling that
//! refuses NEW dispatch when spent, and reports that refusal where an operator looks.
//!
//! Three properties are load-bearing and each has its own test:
//!
//!   * **Unset is unlimited.** An absent budget, or a non-positive `daily_tokens`, never refuses
//!     anything — byte-identical to a daemon built before this feature. `0 = unlimited` matches the
//!     `max_concurrent` idiom the ticket names.
//!   * **It bounds NEW dispatch only.** An in-flight run is never terminated by a budget: killing a
//!     running agent wastes everything it has already spent, strictly worse than letting it finish.
//!     A retry/continuation of an already-started ticket therefore passes this gate too.
//!   * **A refusal is not a silent stall.** It is recorded in [`BudgetLedger`], surfaced on
//!     `/api/v1/state` as `budget_held`, and named by the reconciliation sweep instead of the false
//!     "nothing has reported it blocked".

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Offset, SecondsFormat, Utc};
use rhapsody_store::Store;
use serde::Serialize;

/// How long a fetched provider→spend map is reused before the store is asked again. One small
/// indexed query per window rather than one per candidate in a tick; a run that ENDS invalidates the
/// cache immediately (`Orchestrator::persist_end_run`), so the stop reacts to the spend it just
/// recorded without waiting the window out.
const SPEND_CACHE_TTL: Duration = Duration::from_secs(3);

/// Reports whether a provider's configured daily budget is SPENT. `limit <= 0` is unlimited (the
/// `max_concurrent` idiom), as is the absence of a limit (the caller only calls this with a
/// configured one). `spent >= limit` — a budget bounds the next token, so spending exactly to the
/// ceiling refuses the run that would exceed it.
pub fn budget_spent(spent: i64, limit: i64) -> bool {
    limit > 0 && spent >= limit
}

/// Midnight of the DAEMON host's current local day, as a UTC RFC3339 instant — the same LOCAL day
/// boundary `/api/v1/history/summary` uses, so the budget and the figures an operator reads beside
/// it describe one day. Local, not UTC: a UTC boundary would silently shift the reset for anyone
/// not on UTC. Panic-free (the instant is the naive local midnight shifted by the host's current
/// offset, so there is no ambiguous `LocalResult` to unwrap); on a DST spring-forward day that skips
/// 00:00 the boundary lands an hour off for that one day rather than the daemon failing.
pub fn local_day_start() -> String {
    let now = chrono::Local::now();
    let shift = chrono::TimeDelta::try_seconds(now.offset().fix().local_minus_utc() as i64)
        .unwrap_or_default();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc() - shift)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Sums a window's tokens per provider from the store. Best-effort: a store that cannot answer
/// yields an empty map, which reads as "nothing spent" — the gate then fails OPEN with a warning
/// rather than refusing every ticket because the history store is unreadable. A budget is a
/// guardrail, not a hard security control, and a daemon whose store is broken has louder problems
/// than an unenforced ceiling.
fn spent_by_provider(store: &dyn Store, since: &str) -> HashMap<String, i64> {
    match store.tokens_by_provider(since) {
        Ok(rows) => rows
            .into_iter()
            .map(|r| (r.provider, r.total_tokens))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "provider budget: reading today's spend failed; budgets not enforced this tick");
            HashMap::new()
        }
    }
}

/// One refused dispatch: the subject (a ticket identifier, or a pull request's `owner/repo#n`), its
/// title (empty for a review), the owning project, the provider whose budget is spent, and the two
/// figures an operator needs to act.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetHeld {
    pub subject: String,
    pub title: String,
    pub project: String,
    pub provider: String,
    pub daily_tokens: i64,
    pub spent_tokens: i64,
}

/// The budget ledger: the CURRENT refused set the console reads off `/api/v1/state`, a once-per-
/// subject log dedupe, and the short-lived spend cache.
///
/// Shared behind an [`Arc`] for the human-hold ledger's reason: the gates that record a refusal run
/// on the control task, and the snapshot assembles the console view from the same cell. It takes
/// `&self`, is never held across an `.await`, and a poisoned lock is recovered rather than
/// propagated.
#[derive(Default)]
pub struct BudgetLedger {
    state: Mutex<LedgerState>,
}

#[derive(Default)]
struct LedgerState {
    /// Refusals by subject, replaced as the gate re-refuses and removed when the subject dispatches.
    holds: BTreeMap<String, BudgetHeld>,
    /// Subjects whose refusal has already been logged, so a refusal repeated every tick is one line.
    announced: std::collections::HashSet<String>,
    /// The cached provider→spend map and the instant it was fetched.
    spend: Option<(Instant, HashMap<String, i64>)>,
}

impl BudgetLedger {
    /// Records a refusal, returning `true` the FIRST time this subject is refused (the caller logs
    /// once on that edge). The hold is keyed by subject so a later successful dispatch can clear it.
    pub fn hold(&self, subject: &str, held: BudgetHeld) -> bool {
        let mut st = self.lock();
        let first = st.announced.insert(subject.to_string());
        st.holds.insert(subject.to_string(), held);
        first
    }

    /// Drops a subject's hold — called when it finally dispatches, so the console stops reporting a
    /// refusal that no longer holds.
    pub fn release(&self, subject: &str) {
        let mut st = self.lock();
        st.holds.remove(subject);
        st.announced.remove(subject);
    }

    /// The hold recorded for a subject, if any — the lookup the reconciliation sweep makes so it can
    /// name a budget hold instead of claiming nothing has reported a divergence blocked.
    pub fn get(&self, subject: &str) -> Option<BudgetHeld> {
        self.lock().holds.get(subject).cloned()
    }

    /// The current refused set, ordered by subject, for `GET /api/v1/state`.
    pub fn held(&self) -> Vec<BudgetHeld> {
        self.lock().holds.values().cloned().collect()
    }

    /// Today's spend per provider, reused for [`SPEND_CACHE_TTL`]. The store is only consulted when
    /// a caller actually has a configured budget to check, so a daemon that sets none never pays.
    pub fn spend_map(&self, store: &dyn Store, since: &str) -> HashMap<String, i64> {
        {
            let st = self.lock();
            if let Some((at, map)) = &st.spend
                && at.elapsed() < SPEND_CACHE_TTL
            {
                return map.clone();
            }
        }
        let map = spent_by_provider(store, since);
        self.lock().spend = Some((Instant::now(), map.clone()));
        map
    }

    /// Drops the spend cache. Called when a run ENDS, because that is the one moment the metered
    /// total moves — the stop must see the spend it just recorded, not a stale window.
    pub fn invalidate_spend(&self) {
        self.lock().spend = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Convenience alias for the shared handle the orchestrator carries.
pub type SharedBudgetLedger = Arc<BudgetLedger>;

impl crate::orchestrator::Orchestrator {
    /// The configured daily token limit for a provider, or `None` when there is no budget for it or
    /// it is non-positive. `None` is the whole "unset is unlimited" property — every gate is a no-op
    /// on it, so a daemon that configures no budget schedules byte-identically to one built before
    /// this feature.
    pub(crate) fn provider_daily_limit(&self, provider: &str) -> Option<i64> {
        let eff = self.eff.as_ref()?;
        let b = eff.cfg.budgets.get(provider)?;
        (b.daily_tokens > 0).then_some(b.daily_tokens)
    }

    /// `Some((limit, spent))` when `provider`'s configured daily budget is spent. `None` when there
    /// is no limit, or spend is still below it. Best-effort: an unreadable store reads as zero spend
    /// and this answers `None` (the gate fails open) — see [`spent_by_provider`].
    pub(crate) fn provider_budget_spent(&self, provider: &str) -> Option<(i64, i64)> {
        let limit = self.provider_daily_limit(provider)?;
        let since = local_day_start();
        let spent = self
            .budget_ledger
            .spend_map(self.store(), &since)
            .get(provider)
            .copied()
            .unwrap_or(0);
        budget_spent(spent, limit).then_some((limit, spent))
    }

    /// Records a refused dispatch, logging the FIRST time this subject is refused (a refusal that
    /// repeats every tick is not a signal anyone reads). Returns the hold when it was newly
    /// announced, so the caller can decide whether to log again; the ledger keeps it either way so
    /// `/api/v1/state` and the reconciliation sweep can name it.
    pub(crate) fn note_budget_hold(
        &self,
        subject: &str,
        title: &str,
        project: &str,
        provider: &str,
        limit: i64,
        spent: i64,
    ) -> bool {
        let held = BudgetHeld {
            subject: subject.to_string(),
            title: title.to_string(),
            project: project.to_string(),
            provider: provider.to_string(),
            daily_tokens: limit,
            spent_tokens: spent,
        };
        let first = self.budget_ledger.hold(subject, held);
        if first {
            tracing::warn!(
                subject,
                provider,
                daily_tokens = limit,
                spent_tokens = spent,
                "skipping dispatch: the provider's daily token budget is spent; other providers are unaffected"
            );
        }
        first
    }

    /// Drops a subject's budget hold — called when it finally dispatches, so the console stops
    /// reporting a refusal that no longer holds.
    pub(crate) fn release_budget_hold(&self, subject: &str) {
        self.budget_ledger.release(subject);
    }

    /// The budget hold that explains a reported divergence, if one exists — looked up under the
    /// pull request's coordinate (a deferred review round) and, failing that, under the origin
    /// ticket (a ticket whose re-run is budget-held). The reconciliation sweep calls this so a
    /// budget-held divergence is named by its cause rather than reported as an unexplained stall
    /// (STUDIO-957).
    pub(crate) fn budget_hold_for(&self, pr: &str, ticket: &str) -> Option<BudgetHeld> {
        if let Some(h) = self.budget_ledger.get(pr) {
            return Some(h);
        }
        if !ticket.is_empty() {
            return self.budget_ledger.get(ticket);
        }
        None
    }

    /// The provider a ticketless REVIEW dispatched to `iss` will bill, resolved exactly the way
    /// [`dispatch_issue`](crate::retry::Orchestrator::dispatch_issue) resolves it: the reviewer's
    /// actual harness, their profile's model unless the operator's `review.model` override wins. The
    /// review budget gate's key — the incident's whole Claude bill was reviews, so a gate that could
    /// not see them would have refused nothing.
    pub(crate) fn review_projected_provider(
        &self,
        iss: &rhapsody_core::Issue,
        project_slug: &str,
    ) -> String {
        let harness = self.review_harness_for(iss);
        let fallback = self.configured_backend();
        // The reviewer's own profile model first (the same `route_teams` value `dispatch_issue`
        // stamps), then the operator's `review.model` override when it wins — and, when BOTH are
        // empty (the common install), `projected_provider` falls back to the harness's configured
        // model exactly as `run_provenance_for` will, so the gate and the recorded row agree.
        let mut mo = self
            .route_teams(iss)
            .map(|td| td.model_override)
            .unwrap_or_default();
        if let Some(teams) = self.teams.as_ref()
            && let rhapsody_config::teams::ReviewModelChoice::Use(m) =
                teams.review_model_for(&harness, &fallback)
        {
            mo.model = m.to_string();
        }
        self.projected_provider(&harness, &mo, project_slug)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::ProviderBudget;
    use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunProvenance, RunStart, Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{issue, orch_for_retry};

    /// Seeds a completed run billed to `provider` with `tokens` total, starting NOW (so it falls in
    /// today's local window the budget reads).
    fn seed_spend(store: &dyn Store, provider: &str, tokens: i64) {
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let id = store
            .start_run(RunStart {
                issue_identifier: "MT-seed".into(),
                started_at: started.clone(),
                ..Default::default()
            })
            .expect("start");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    total_tokens: tokens,
                    ended_at: started,
                    ..Default::default()
                },
            )
            .expect("end");
        store
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: provider.into(),
                    harness: "claude".into(),
                    model: "claude-opus-4-8".into(),
                    ..Default::default()
                },
            )
            .expect("provenance");
    }

    /// An orchestrator on a real store whose configured claude model resolves to `anthropic`, with
    /// the given `budgets` and a recording spawn seam.
    fn orch_with_budgets(
        store: Arc<dyn Store + Send + Sync>,
        budgets: &[(&str, i64)],
    ) -> (
        crate::orchestrator::Orchestrator,
        crate::testsupport::DispatchedIds,
    ) {
        let (mut o, dispatched) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.set_store(store);
        let eff = o.eff.as_mut().expect("eff");
        eff.cfg.claude.model = "claude-opus-4-8".into();
        for (provider, limit) in budgets {
            eff.cfg.budgets.insert(
                (*provider).to_string(),
                ProviderBudget {
                    daily_tokens: *limit,
                },
            );
        }
        (o, dispatched)
    }

    // STUDIO-957 acceptance: a provider whose daily budget is spent dispatches NO new run on it, and
    // the refusal is recorded (the console/sweep read it). Mutation: drop the gate in
    // `dispatch_issue` and the first assertion reds.
    #[test]
    fn a_spent_budget_refuses_new_dispatch() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        seed_spend(store.as_ref(), "anthropic", 300);
        let (mut o, dispatched) = orch_with_budgets(store, &[("anthropic", 200)]);

        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());

        assert!(
            dispatched.lock().expect("lock").is_empty(),
            "a spent anthropic budget must refuse the dispatch"
        );
        assert!(
            !o.claimed.contains("1"),
            "a refusal must not claim the ticket"
        );
        assert!(
            o.budget_ledger.get("MT-1").is_some(),
            "the refusal must be recorded where the console and sweep read it"
        );
        assert_eq!(
            o.budget_ledger.get("MT-1").expect("hold").provider,
            "anthropic"
        );
    }

    // Other providers are unaffected: a budget spent on one account must not stop work billed to
    // another. Mutation: key the limit lookup by something other than the run's provider (e.g. any
    // spent budget refusing everything) and this reds.
    #[test]
    fn a_budget_spent_on_another_provider_does_not_refuse() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        seed_spend(store.as_ref(), "openai", 10_000);
        let (mut o, dispatched) = orch_with_budgets(store, &[("openai", 1)]);

        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());

        assert_eq!(
            dispatched.lock().expect("lock").as_slice(),
            ["1".to_string()],
            "a run billed to anthropic is unaffected by a spent openai budget"
        );
        assert!(o.budget_ledger.held().is_empty());
    }

    // THE behaviour-preservation test: an unset budget never refuses anything, however much was
    // already spent. Mutation: default an unset provider to some finite number and this reds.
    #[test]
    fn an_unset_budget_refuses_nothing() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        seed_spend(store.as_ref(), "anthropic", 999_999_999);
        let (mut o, dispatched) = orch_with_budgets(store, &[]);

        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());

        assert_eq!(
            dispatched.lock().expect("lock").as_slice(),
            ["1".to_string()],
            "no configured budget ⇒ byte-identical to a daemon built before this feature"
        );
        assert!(o.budget_ledger.held().is_empty());
    }

    // A budget bounds NEW dispatch only. A retry/continuation is the SAME ticket's already-started
    // work; refusing it would waste what it has already spent, strictly worse than letting it
    // finish. Mutation: gate retries too (drop the `attempt.is_none()` guard) and this reds.
    #[test]
    fn a_spent_budget_never_blocks_a_continuation() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        seed_spend(store.as_ref(), "anthropic", 300);
        let (mut o, dispatched) = orch_with_budgets(store, &[("anthropic", 200)]);

        // A continuation (`attempt = Some`) of an already-started ticket.
        o.dispatch_issue(
            issue("1", "MT-1", "In Progress"),
            Some(1),
            None,
            String::new(),
        );

        assert_eq!(
            dispatched.lock().expect("lock").as_slice(),
            ["1".to_string()],
            "a continuation must pass even when the budget is spent"
        );
        assert!(
            o.budget_ledger.get("MT-1").is_none(),
            "no hold is recorded for a run that was allowed"
        );
    }

    // A dispatched ticket clears a stale hold, so the console stops reporting a refusal that no
    // longer holds.
    #[test]
    fn a_successful_dispatch_clears_a_stale_hold() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        let (mut o, _) = orch_with_budgets(store, &[]);
        o.note_budget_hold("MT-1", "t", "core", "anthropic", 200, 250);
        assert!(o.budget_ledger.get("MT-1").is_some());

        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());

        assert!(
            o.budget_ledger.get("MT-1").is_none(),
            "dispatching clears the hold"
        );
    }

    // The review path's provider resolution falls back to the harness's configured model, the SAME
    // value the run's provenance records — so the review budget gate can never check a different
    // provider than the one the review bills (the incident's whole Claude bill was reviews).
    #[test]
    fn review_projected_provider_matches_the_configured_harness_model() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        let (o, _) = orch_with_budgets(store, &[]);
        let iss = rhapsody_core::Issue {
            id: "pr:o/r#3@alice".into(),
            identifier: "pr:o/r#3@alice".into(),
            ..Default::default()
        };
        assert_eq!(o.review_projected_provider(&iss, ""), "anthropic");
    }

    #[test]
    fn unset_or_nonpositive_limit_is_unlimited() {
        assert!(!budget_spent(1_000_000_000, 0), "0 = unlimited");
        assert!(!budget_spent(1_000_000_000, -5), "negative = unlimited");
        assert!(!budget_spent(0, 0));
    }

    #[test]
    fn a_spent_limit_refuses_at_the_ceiling() {
        assert!(!budget_spent(199, 200));
        assert!(
            budget_spent(200, 200),
            "spending exactly to the ceiling refuses the next run"
        );
        assert!(budget_spent(201, 200));
    }

    #[test]
    fn ledger_records_dedupes_and_releases() {
        let l = BudgetLedger::default();
        let h = BudgetHeld {
            subject: "MT-1".into(),
            title: "t".into(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 250,
        };
        assert!(l.hold("MT-1", h.clone()), "first refusal announces");
        assert!(!l.hold("MT-1", h.clone()), "a repeat does not re-announce");
        assert_eq!(l.get("MT-1").as_ref(), Some(&h));
        assert_eq!(l.held().len(), 1);
        l.release("MT-1");
        assert!(l.get("MT-1").is_none());
        assert!(l.held().is_empty());
    }
}
