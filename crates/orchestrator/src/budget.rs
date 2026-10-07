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

use chrono::{DateTime, Local, LocalResult, SecondsFormat, TimeDelta, TimeZone, Utc};
use rhapsody_config::{Config, Price};
use rhapsody_store::Store;
use serde::Serialize;

/// How long a fetched provider→spend map is reused before the store is asked again. One small
/// indexed query per window rather than one per candidate in a tick; a run that ENDS invalidates the
/// cache immediately (`Orchestrator::persist_end_run`), so the stop reacts to the spend it just
/// recorded without waiting the window out.
const SPEND_CACHE_TTL: Duration = Duration::from_secs(3);

/// The FLOOR for how long a recorded refusal stays on the console without being re-confirmed. A
/// subject still being offered re-refuses on every pass and stays fresh; one that stopped being
/// offered (the ticket was moved to Done, the pull request merged) goes stale and drops, so
/// `/api/v1/state` never carries a refusal that no longer holds.
///
/// It is a floor, not the rule: the real bound is
/// [`Orchestrator::budget_hold_ttl`], which widens to two configured poll cadences whenever the
/// operator's `polling.interval_ms` is longer than this. A fixed 300s was wrong (sol round 1 on
/// PR #199): `polling.interval_ms` has no five-minute ceiling, so on a ten-minute poll a genuinely
/// blocked ticket — refreshed only once per pass — disappeared from `/api/v1/state` for roughly
/// half of every cycle. A REVIEW hold takes the wider of that and the review watcher's own
/// [`CAPACITY_HOLD_TTL`](crate::reviewwatch::CAPACITY_HOLD_TTL) — see [`Entry::ttl`].
const HOLD_TTL_FLOOR: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    PriceTable,
    HarnessReported,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunCost {
    Priced { usd: f64, source: CostSource },
    Unknown,
}

/// Configured prices win. A missing, zero, negative or non-finite harness cost is unknown.
/// In particular OpenCode's subscription `cost: 0` is not evidence of a free API model.
pub fn run_cost_usd(
    model: &str,
    tokens: &TokenCounts,
    harness_reported_usd: Option<f64>,
    prices: &HashMap<String, Price>,
) -> RunCost {
    if let Some(p) = prices.get(model) {
        let usd = (tokens.input.max(0) as f64 * p.input
            + tokens.output.max(0) as f64 * p.output
            + tokens.cache_read.max(0) as f64 * p.cache_read
            + tokens.cache_write.max(0) as f64 * p.cache_write)
            / 1_000_000.0;
        if usd.is_finite() && usd >= 0.0 {
            return RunCost::Priced {
                usd,
                source: CostSource::PriceTable,
            };
        }
    }
    match harness_reported_usd.filter(|v| v.is_finite() && *v > 0.0) {
        Some(usd) => RunCost::Priced {
            usd,
            source: CostSource::HarnessReported,
        },
        None => RunCost::Unknown,
    }
}

/// Actual spend only. Unknown observations or an unreadable store fail closed, never as $0.
pub fn daily_spend_usd(store: &dyn Store, provider: &str, since: &str) -> f64 {
    match store.turn_spend_since(since) {
        Ok(rows) => rows
            .iter()
            .filter(|r| r.provider == provider && r.account == provider)
            .map(|r| {
                r.usd
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .unwrap_or(f64::INFINITY)
            })
            .sum(),
        Err(error) => {
            tracing::warn!(%error, provider, "USD budget: today's spend is unreadable; refusing new dispatch");
            f64::INFINITY
        }
    }
}

/// Subscription equivalents are information only; this function is never a dispatch predicate.
pub fn api_equivalent_usd_today(store: &dyn Store, account: &str) -> f64 {
    match store.turn_spend_since(&local_day_start()) {
        Ok(rows) => rows
            .iter()
            .filter(|r| r.account == account && r.account != r.provider)
            .filter_map(|r| r.usd.filter(|v| v.is_finite() && *v >= 0.0))
            .sum(),
        Err(error) => {
            tracing::warn!(%error, account, "subscription equivalent: reading today's dollars failed");
            0.0
        }
    }
}

pub fn usd_gate(provider: &str, model: &str, cfg: &Config, spent_usd: f64) -> Result<(), String> {
    usd_gate_with_reported_price(provider, model, cfg, spent_usd, false)
}

fn usd_gate_with_reported_price(
    provider: &str,
    model: &str,
    cfg: &Config,
    spent_usd: f64,
    harness_priced: bool,
) -> Result<(), String> {
    let limit = cfg.budgets.get(provider).map_or(0.0, |b| b.daily_usd);
    if limit <= 0.0 || matches!(provider, "claude-subscription" | "chatgpt-subscription") {
        return Ok(());
    }
    if !cfg.prices.contains_key(model) && !harness_priced {
        return Err(format!(
            "no price for {model}; add it under prices: or daily_usd for {provider} cannot be enforced"
        ));
    }
    if !spent_usd.is_finite() {
        return Err(format!(
            "daily_usd for {provider} cannot be enforced: today's spend is unknown"
        ));
    }
    if spent_usd >= limit {
        return Err(format!(
            "daily_usd for {provider} is spent (${spent_usd:.6} of ${limit:.6})"
        ));
    }
    Ok(())
}

/// Dispatch-time billing identity and price snapshot, immune to a later config reload.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunPricing {
    pub provider: String,
    pub account: String,
    pub model: String,
    pub prices: HashMap<String, Price>,
}

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
/// not on UTC.
pub fn local_day_start() -> String {
    day_start(Local::now())
}

/// The local-day boundary for `now`, resolved through the ZONE's own transition rules rather than
/// through `now`'s current offset.
///
/// The difference is the whole point (sol round 1 on PR #199): on a DST transition day the offset
/// at local midnight differs from the offset now. After the US spring-forward, applying the current
/// PDT (-07) to midnight yields 07:00Z although that midnight was PST (-08), 08:00Z — an extra hour
/// of yesterday's spend inside today's budget. `Tz::from_local_datetime` consults the zone database
/// and answers the offset midnight actually had. A `None` (a zone whose transition SKIPS 00:00, as
/// `America/Santiago` does) walks forward a bounded few hours to the first local instant that
/// exists; an `Ambiguous` midnight takes the earlier of the two. Panic-free throughout.
fn day_start<Tz: TimeZone>(now: DateTime<Tz>) -> String {
    let tz = now.timezone();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| resolve_midnight(&tz, naive));
    midnight
        .map(|dt| {
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Resolves the instant local `midnight` names, through the zone's own rules. `Single` and the
/// earlier of an `Ambiguous` pair are the day start; a `None` probes forward in one-minute steps to
/// the first local instant the zone accepts (bounded to three hours, far past any real gap).
fn resolve_midnight<Tz: TimeZone>(
    tz: &Tz,
    midnight: chrono::NaiveDateTime,
) -> Option<DateTime<Tz>> {
    match tz.from_local_datetime(&midnight) {
        LocalResult::Single(dt) => Some(dt),
        LocalResult::Ambiguous(earliest, _) => Some(earliest),
        LocalResult::None => (1..=180).find_map(|m| {
            let probe = midnight.checked_add_signed(TimeDelta::minutes(m))?;
            tz.from_local_datetime(&probe).earliest()
        }),
    }
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

/// One refused dispatch: the subject (a ticket identifier, or a review's `pr:owner/repo#n@reviewer`
/// identity), its title (empty for a review), the owning project, the provider whose budget is
/// spent, and the two figures an operator needs to act.
///
/// `pr` is the pull request coordinate of a REVIEW refusal (`owner/repo#n`), empty for a ticket.
/// It is carried separately from `subject` because dispatch is per `(PR, reviewer)` while a
/// divergence is reported per pull request: keying the hold by the review IDENTITY keeps two
/// reviewers of one PR independent (sol round 1 on PR #199 — one reviewer's successful dispatch
/// used to erase a sibling reviewer's still-active hold), and this field is how the reconciliation
/// sweep still finds every hold for a coordinate.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BudgetHeld {
    pub subject: String,
    pub title: String,
    pub project: String,
    pub provider: String,
    pub daily_tokens: i64,
    pub spent_tokens: i64,
    pub pr: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
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

/// A recorded refusal and when it was last confirmed, so a stale one can be dropped.
struct Entry {
    held: BudgetHeld,
    recorded: Instant,
}

impl Entry {
    /// Whether this refusal is still live under the caller's base TTL. A REVIEW hold — one that
    /// carries a pull request coordinate — ages against the wider of the base and the review
    /// watcher's own cadence bound [`CAPACITY_HOLD_TTL`](crate::reviewwatch::CAPACITY_HOLD_TTL),
    /// because that is what refreshes it. See [`Entry::ttl`].
    fn fresh(&self, base: Duration) -> bool {
        self.recorded.elapsed() < self.ttl(base)
    }

    /// The TTL this hold ages against. A ticket is re-offered every `polling.interval_ms` and is
    /// refreshed on that cadence, so the caller's base (which already widens for the poll interval)
    /// bounds it. A REVIEW is refreshed only when the off-loop watcher's rotating cursor next
    /// reaches its pull request: a `PR_STATE_POLL_INTERVAL` sleep plus up to two serial phases of
    /// `gh` lookups, independent of `polling.interval_ms`. A review hold must therefore also clear
    /// `CAPACITY_HOLD_TTL` — the same bound the codebase already gives a review capacity hold for
    /// this exact reason (sol round 1 on PR #199; alice round 2 finding B1). Keying on the
    /// coordinate (a non-empty `pr`) rather than on a caller-supplied flag keeps `held`, `get` and
    /// `get_for_pr` consistent: they all pass the base and let the entry decide.
    fn ttl(&self, base: Duration) -> Duration {
        if self.held.pr.is_empty() {
            base
        } else {
            base.max(crate::reviewwatch::CAPACITY_HOLD_TTL)
        }
    }
}

#[derive(Default)]
struct LedgerState {
    /// Refusals by subject, replaced as the gate re-refuses and removed when the subject dispatches.
    holds: BTreeMap<String, Entry>,
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
        st.holds.insert(
            subject.to_string(),
            Entry {
                held,
                recorded: Instant::now(),
            },
        );
        first
    }

    /// Drops a subject's hold — called when it finally dispatches, so the console stops reporting a
    /// refusal that no longer holds.
    pub fn release(&self, subject: &str) {
        let mut st = self.lock();
        st.holds.remove(subject);
        st.announced.remove(subject);
    }

    /// The hold recorded for a subject, if any and still fresh under `ttl` — the lookup the
    /// reconciliation sweep makes so it can name a budget hold instead of claiming nothing has
    /// reported a divergence blocked. A review hold ages against the review watcher's own cadence;
    /// see [`Entry::ttl`].
    pub fn get(&self, subject: &str, ttl: Duration) -> Option<BudgetHeld> {
        self.lock()
            .holds
            .get(subject)
            .filter(|e| e.fresh(ttl))
            .map(|e| e.held.clone())
    }

    /// The first fresh hold recorded for a pull request coordinate, ordered by subject — the lookup
    /// that finds REVIEW holds, which are keyed by review identity rather than by coordinate. There
    /// can be more than one (a mixed roster with two reviewers out of budget); the sweep names one,
    /// and the console lists them all. A review hold ages against the review watcher's own cadence;
    /// see [`Entry::ttl`].
    pub fn get_for_pr(&self, pr: &str, ttl: Duration) -> Option<BudgetHeld> {
        self.lock()
            .holds
            .values()
            .find(|e| e.fresh(ttl) && e.held.pr == pr)
            .map(|e| e.held.clone())
    }

    /// The current refused set, ordered by subject, for `GET /api/v1/state`. Stale entries (a
    /// subject that stopped being offered without dispatching — moved to Done, merged) are dropped
    /// rather than reported forever. A review hold ages against the review watcher's own cadence;
    /// see [`Entry::ttl`].
    pub fn held(&self, ttl: Duration) -> Vec<BudgetHeld> {
        self.lock()
            .holds
            .values()
            .filter(|e| e.fresh(ttl))
            .map(|e| e.held.clone())
            .collect()
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

    /// Test seam: records a hold as if it were confirmed at `recorded`, so the staleness rule can be
    /// exercised without sleeping out [`HOLD_TTL_FLOOR`].
    #[cfg(test)]
    fn hold_at(&self, subject: &str, held: BudgetHeld, recorded: Instant) {
        self.lock()
            .holds
            .insert(subject.to_string(), Entry { held, recorded });
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
    pub(crate) fn run_pricing_for(
        &self,
        harness: &str,
        model_override: &rhapsody_agent::ModelOverride,
        project: &str,
    ) -> RunPricing {
        let (harness, model) = self.resolved_harness_model(harness, model_override, project);
        let provider = crate::persist::derive_provider(&harness, &model);
        let key = if model.contains('/') {
            model
        } else {
            format!("{provider}/{model}")
        };
        let mut pricing = RunPricing {
            account: provider.clone(),
            provider,
            model: key.clone(),
            ..Default::default()
        };
        if let Some(eff) = &self.eff {
            if let Some(price) = eff.cfg.prices.get(&key) {
                pricing.prices.insert(key, price.clone());
            }
            let guard = eff
                .project_by_slug(project)
                .map_or(eff.cfg.claude.billing_guard, |p| {
                    p.mcfg.claude.billing_guard
                });
            if harness == "claude" && guard.unwrap_or(true) {
                pricing.account = "claude-subscription".into();
            } else if harness == "opencode"
                && pricing.provider == "openai"
                && rhapsody_agent::opencode::state::provider_uses_oauth(
                    &eff.cfg.opencode.auth_source,
                    "openai",
                )
            {
                pricing.account = "chatgpt-subscription".into();
            }
        }
        pricing
    }

    /// USD binds only pay-per-token accounts. A prior positive report for this exact model is
    /// evidence for the harness fallback; an unobserved model needs an operator-maintained price.
    pub(crate) fn usd_budget_hold(&self, pricing: &RunPricing) -> Option<BudgetHeld> {
        let cfg = &self.eff.as_ref()?.cfg;
        let limit = cfg.budgets.get(&pricing.provider)?.daily_usd;
        if limit <= 0.0 || pricing.account != pricing.provider {
            return None;
        }
        let spent = daily_spend_usd(self.store(), &pricing.provider, &local_day_start());
        let known = match self
            .store()
            .model_has_reported_cost(&pricing.provider, &pricing.model)
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "USD budget: model cost history is unreadable");
                false
            }
        };
        let reason =
            usd_gate_with_reported_price(&pricing.provider, &pricing.model, cfg, spent, known)
                .err()
                .or_else(|| {
                    (!self.store().usd_accounting_available()).then(|| {
                format!(
                    "daily_usd for {} cannot be enforced: dollar accounting storage is unavailable",
                    pricing.provider
                )
            })
                })?;
        Some(BudgetHeld {
            provider: pricing.provider.clone(),
            reason,
            daily_usd: Some(limit),
            spent_usd: spent.is_finite().then_some(spent),
            ..Default::default()
        })
    }

    pub(crate) fn note_usd_budget_hold(&self, held: BudgetHeld) {
        let first = self.budget_ledger.hold(&held.subject, held.clone());
        if first {
            tracing::warn!(subject = %held.subject, provider = %held.provider, reason = %held.reason, "skipping dispatch: USD budget refusal");
        }
    }

    /// Latest per-turn usage replaces the live estimate. No turn-total accumulation here: the
    /// store keys by run and turn, so a terminal report cannot count the same spend twice.
    pub(crate) fn observe_run_cost(
        &self,
        re: &crate::orchestrator::RunningEntry,
        ev: &rhapsody_agent::Event,
    ) {
        let Some(u) = ev.usage else {
            return;
        };
        if re.run_id == 0 || re.pricing.provider.is_empty() || re.brokered {
            return;
        }
        let tokens = TokenCounts {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_tokens,
            cache_write: u.cache_creation_tokens,
        };
        let (usd, source) =
            match run_cost_usd(&re.pricing.model, &tokens, ev.cost_usd, &re.pricing.prices) {
                RunCost::Priced { usd, source } => (
                    Some(usd),
                    match source {
                        CostSource::PriceTable => "price_table",
                        CostSource::HarnessReported => "harness_reported",
                    },
                ),
                RunCost::Unknown => (None, "unknown"),
            };
        let spend = rhapsody_store::TurnSpend {
            turn: re.turn_count.max(1),
            at: crate::persist::rfc3339((self.now)()),
            provider: re.pricing.provider.clone(),
            account: re.pricing.account.clone(),
            model: re.pricing.model.clone(),
            usd,
            source: source.into(),
            harness_priced: ev.cost_usd.is_some_and(|v| v.is_finite() && v > 0.0),
        };
        if let Err(error) = self.store().set_turn_spend(re.run_id, &spend) {
            tracing::warn!(%error, run_id = re.run_id, "USD budget: spend persistence failed");
        }
    }

    /// Whether ANY provider has a configured (positive) daily budget. The gates test this first so a
    /// daemon that configures none does no provider resolution at all on the dispatch path — the
    /// strong form of "unset is byte-identical to today".
    pub(crate) fn budgets_configured(&self) -> bool {
        self.eff.as_ref().is_some_and(|e| {
            e.cfg
                .budgets
                .values()
                .any(|b| b.daily_tokens > 0 || b.daily_usd > 0.0)
        })
    }

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

    /// How long an un-refreshed hold may survive before the console stops reporting it. The FLOOR
    /// is [`HOLD_TTL_FLOOR`]; when the operator's poll interval is longer, two cadences, so a hold
    /// re-confirmed once per selection pass can never age out between two passes. Tied to the
    /// configured cadence rather than a fixed wall-clock because `polling.interval_ms` has no
    /// ceiling (sol round 1 on PR #199).
    ///
    /// This is the TICKET bound. A review is not refreshed on this cadence — the off-loop watcher
    /// drives it, and its rotation can take far longer — so a review hold widens further, to
    /// [`CAPACITY_HOLD_TTL`](crate::reviewwatch::CAPACITY_HOLD_TTL). [`Entry::ttl`] applies that per
    /// entry, which is why callers pass this base to every lookup rather than choosing a TTL.
    pub(crate) fn budget_hold_ttl(&self) -> Duration {
        let poll_ms = self
            .eff
            .as_ref()
            .map(|e| e.cfg.polling.interval_ms)
            .unwrap_or(0)
            .max(0) as u64;
        HOLD_TTL_FLOOR.max(Duration::from_millis(poll_ms).saturating_mul(2))
    }

    /// Records a TICKET's refused dispatch, logging the FIRST time this subject is refused (a
    /// refusal that repeats every tick is not a signal anyone reads). Returns whether it was newly
    /// announced; the ledger keeps it either way so `/api/v1/state` and the reconciliation sweep
    /// can name it.
    pub(crate) fn note_budget_hold(
        &self,
        subject: &str,
        title: &str,
        project: &str,
        provider: &str,
        limit: i64,
        spent: i64,
    ) -> bool {
        self.record_budget_hold(BudgetHeld {
            subject: subject.to_string(),
            title: title.to_string(),
            project: project.to_string(),
            provider: provider.to_string(),
            daily_tokens: limit,
            spent_tokens: spent,
            pr: String::new(),
            ..Default::default()
        })
    }

    /// Records a REVIEW's refused dispatch, keyed by the review IDENTITY
    /// (`pr:owner/repo#n@reviewer`) and carrying its pull request coordinate for the sweep. The
    /// identity is the key because dispatch is per `(PR, reviewer)`: two reviewers of one pull
    /// request on different providers must not overwrite each other's hold, and one reviewer's
    /// successful dispatch must not clear the other's still-active refusal (sol round 1 on PR
    /// #199).
    pub(crate) fn note_review_budget_hold(
        &self,
        identity: &str,
        pr: &str,
        project: &str,
        provider: &str,
        limit: i64,
        spent: i64,
    ) -> bool {
        self.record_budget_hold(BudgetHeld {
            subject: identity.to_string(),
            title: String::new(),
            project: project.to_string(),
            provider: provider.to_string(),
            daily_tokens: limit,
            spent_tokens: spent,
            pr: pr.to_string(),
            ..Default::default()
        })
    }

    fn record_budget_hold(&self, held: BudgetHeld) -> bool {
        let subject = held.subject.clone();
        let provider = held.provider.clone();
        let (limit, spent) = (held.daily_tokens, held.spent_tokens);
        let first = self.budget_ledger.hold(&subject, held);
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
        let ttl = self.budget_hold_ttl();
        if let Some(h) = self.budget_ledger.get_for_pr(pr, ttl) {
            return Some(h);
        }
        if !ticket.is_empty() {
            return self.budget_ledger.get(ticket, ttl);
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
        self.review_projected_pricing(iss, project_slug).provider
    }

    pub(crate) fn review_projected_pricing(
        &self,
        iss: &rhapsody_core::Issue,
        project: &str,
    ) -> RunPricing {
        let harness = self.review_harness_for(iss);
        let mut mo = self
            .route_teams(iss)
            .map(|td| td.model_override)
            .unwrap_or_default();
        if let Some(teams) = &self.teams
            && let rhapsody_config::teams::ReviewModelChoice::Use(model) =
                teams.review_model_for(&harness, &self.configured_backend())
        {
            mo.model = model.into();
        }
        self.run_pricing_for(&harness, &mo, project)
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

    fn usd_config(yaml: &str) -> rhapsody_config::Config {
        rhapsody_config::decode(&rhapsody_config::workflow::Definition {
            config: serde_yaml_ng::from_str(yaml).expect("front matter"),
            prompt_template: String::new(),
        })
        .expect("config")
    }

    #[test]
    fn price_table_wins() {
        let cfg = usd_config(
            "prices:\n  fireworks-ai/m: {input: 2, output: 8, cache_read: 0.5, cache_write: 3}\n",
        );
        let tokens = TokenCounts {
            input: 1_000_000,
            output: 250_000,
            cache_read: 2_000_000,
            cache_write: 100_000,
        };
        assert_eq!(
            run_cost_usd("fireworks-ai/m", &tokens, Some(99.0), &cfg.prices),
            RunCost::Priced {
                usd: 5.3,
                source: CostSource::PriceTable
            }
        );
    }

    #[test]
    fn harness_cost_used_when_no_price() {
        assert_eq!(
            run_cost_usd(
                "fireworks-ai/m",
                &TokenCounts::default(),
                Some(0.125),
                &HashMap::new()
            ),
            RunCost::Priced {
                usd: 0.125,
                source: CostSource::HarnessReported
            }
        );
        assert_eq!(
            run_cost_usd(
                "fireworks-ai/m",
                &TokenCounts::default(),
                None,
                &HashMap::new()
            ),
            RunCost::Unknown
        );
    }

    #[test]
    fn zero_harness_cost_on_subscription_is_not_a_price() {
        assert_eq!(
            run_cost_usd(
                "openai/gpt-test",
                &TokenCounts::default(),
                Some(0.0),
                &HashMap::new()
            ),
            RunCost::Unknown
        );
        let cfg = usd_config("budgets:\n  openai: {daily_usd: 10}\n");
        assert!(usd_gate("openai", "openai/gpt-test", &cfg, 0.0).is_err());
    }

    #[test]
    fn unknown_price_with_daily_usd_refuses_with_reason() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        let (mut o, dispatched) = orch_with_budgets(store, &[]);
        o.eff.as_mut().unwrap().cfg = usd_config(
            "agent: {backend: opencode}\nopencode: {model: fireworks-ai/m}\nbudgets:\n  fireworks-ai: {daily_usd: 10}\n",
        );
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert!(dispatched.lock().unwrap().is_empty());
        let held = o
            .budget_ledger
            .get("MT-1", o.budget_hold_ttl())
            .expect("visible hold");
        assert_eq!(
            held.reason,
            "no price for fireworks-ai/m; add it under prices: or daily_usd for fireworks-ai cannot be enforced"
        );
        assert!(!o.claimed.contains("1"));
        let snapshot = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(snapshot["budget_held"][0]["reason"], held.reason);
        assert_eq!(snapshot["budget_held"][0]["daily_usd"], 10.0);
    }

    #[test]
    fn daily_usd_spent_refuses() {
        let cfg = usd_config(
            "prices:\n  fireworks-ai/m: {input: 1, output: 1}\nbudgets:\n  fireworks-ai: {daily_usd: 10}\n",
        );
        assert!(usd_gate("fireworks-ai", "fireworks-ai/m", &cfg, 9.99).is_ok());
        assert_eq!(
            usd_gate("fireworks-ai", "fireworks-ai/m", &cfg, 10.0),
            Err("daily_usd for fireworks-ai is spent ($10.000000 of $10.000000)".into())
        );
    }

    #[test]
    fn tokens_and_usd_tighter_wins() {
        for (tokens, usd) in [(200, 0.5), (100, 1.0), (100, 0.5)] {
            let store = Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
            let id = seed_spend(store.as_ref(), "fireworks-ai", tokens);
            store
                .set_turn_spend(
                    id,
                    &rhapsody_store::TurnSpend {
                        turn: 1,
                        at: local_day_start(),
                        provider: "fireworks-ai".into(),
                        account: "fireworks-ai".into(),
                        model: "fireworks-ai/m".into(),
                        usd: Some(usd),
                        ..Default::default()
                    },
                )
                .unwrap();
            let (mut o, dispatched) = orch_with_budgets(store, &[]);
            o.eff.as_mut().unwrap().cfg = usd_config(
                "agent: {backend: opencode}\nopencode: {model: fireworks-ai/m}\nprices:\n  fireworks-ai/m: {input: 1, output: 1}\nbudgets:\n  fireworks-ai: {daily_usd: 1, daily_tokens: 200}\n",
            );
            o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
            assert_eq!(
                dispatched.lock().unwrap().is_empty(),
                tokens >= 200 || usd >= 1.0
            );
        }
    }

    #[test]
    fn no_budgets_unlimited_unchanged() {
        let cfg = usd_config("{}");
        assert!(cfg.prices.is_empty());
        assert!(cfg.budgets.is_empty());
        assert!(usd_gate("fireworks-ai", "unknown", &cfg, f64::INFINITY).is_ok());
        let encoded = rhapsody_config::encode(&cfg).unwrap();
        assert!(!encoded.config.contains_key("prices"));
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        seed_spend(store.as_ref(), "anthropic", i64::MAX);
        let (mut o, dispatched) = orch_with_budgets(store, &[]);
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().as_slice(), ["1"]);
    }

    #[test]
    fn api_equivalent_never_gates() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let id = store.start_run(RunStart::default()).unwrap();
        store
            .set_turn_spend(
                id,
                &rhapsody_store::TurnSpend {
                    turn: 1,
                    at: local_day_start(),
                    provider: "anthropic".into(),
                    account: "claude-subscription".into(),
                    model: "anthropic/claude-opus-4-8".into(),
                    usd: Some(100.0),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            api_equivalent_usd_today(store.as_ref(), "claude-subscription"),
            100.0
        );
        assert_eq!(
            daily_spend_usd(store.as_ref(), "anthropic", &local_day_start()),
            0.0
        );
        let (mut o, dispatched) = orch_with_budgets(store, &[]);
        o.eff.as_mut().unwrap().cfg.budgets.insert(
            "anthropic".into(),
            ProviderBudget {
                daily_usd: 1.0,
                ..Default::default()
            },
        );
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().as_slice(), ["1"]);
    }

    #[test]
    fn cost_observations_replace_live_totals_and_pin_dispatch_prices() {
        use crate::agentupdate::AgentUpdate;
        use rhapsody_agent::{
            EVENT_NOTIFICATION, EVENT_SESSION_STARTED, EVENT_TURN_COMPLETED, Event, Usage,
        };
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let (mut o, dispatched) = orch_with_budgets(store.clone(), &[]);
        o.eff.as_mut().unwrap().cfg = usd_config(
            "agent: {backend: opencode}\nopencode: {model: fireworks-ai/m}\nprices:\n  fireworks-ai/m: {input: 2, output: 8, cache_read: 0.5, cache_write: 3}\n",
        );
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().len(), 1);
        o.eff.as_mut().unwrap().cfg.prices.clear(); // A reload never rewrites what this run costs.
        let mut update = |kind: &str, usage: Option<Usage>, cost_usd| {
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev: Event {
                    event_type: kind.into(),
                    usage,
                    cost_usd,
                    ..Default::default()
                },
            });
        };
        update(EVENT_SESSION_STARTED, None, None);
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 250_000,
            cache_creation_tokens: 100_000,
            cache_read_tokens: 2_000_000,
            total_tokens: 3_350_000,
        };
        update(EVENT_NOTIFICATION, Some(usage), Some(99.0));
        update(EVENT_TURN_COMPLETED, Some(usage), Some(99.0));
        update(EVENT_SESSION_STARTED, None, None);
        update(EVENT_TURN_COMPLETED, Some(usage), Some(99.0));
        assert_eq!(
            daily_spend_usd(store.as_ref(), "fireworks-ai", &local_day_start()),
            10.6
        );
        let rows = store.turn_spend_since(&local_day_start()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.source == "price_table"));
    }

    #[test]
    fn positive_harness_history_enables_only_the_exact_model() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let id = store.start_run(RunStart::default()).unwrap();
        store
            .set_turn_spend(
                id,
                &rhapsody_store::TurnSpend {
                    turn: 1,
                    at: local_day_start(),
                    provider: "fireworks-ai".into(),
                    account: "fireworks-ai".into(),
                    model: "fireworks-ai/m".into(),
                    usd: Some(0.25),
                    harness_priced: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let (mut o, dispatched) = orch_with_budgets(store, &[]);
        o.eff.as_mut().unwrap().cfg = usd_config(
            "agent: {backend: opencode}\nopencode: {model: fireworks-ai/m}\nbudgets:\n  fireworks-ai: {daily_usd: 1}\n",
        );
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().as_slice(), ["1"]);
        o.eff.as_mut().unwrap().cfg.opencode.model = "fireworks-ai/other".into();
        o.dispatch_issue(issue("2", "MT-2", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().as_slice(), ["1"]);
    }

    #[test]
    fn unknown_spend_and_disabled_store_do_not_read_as_zero_dollars() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let id = store.start_run(RunStart::default()).unwrap();
        store
            .set_turn_spend(
                id,
                &rhapsody_store::TurnSpend {
                    turn: 1,
                    at: local_day_start(),
                    provider: "fireworks-ai".into(),
                    account: "fireworks-ai".into(),
                    model: "fireworks-ai/m".into(),
                    usd: None,
                    ..Default::default()
                },
            )
            .unwrap();
        let (mut o, dispatched) = orch_with_budgets(store, &[]);
        o.eff.as_mut().unwrap().cfg = usd_config(
            "agent: {backend: opencode}\nopencode: {model: fireworks-ai/m}\nprices:\n  fireworks-ai/m: {input: 1, output: 1}\nbudgets:\n  fireworks-ai: {daily_usd: 1}\n",
        );
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert!(dispatched.lock().unwrap().is_empty());
        assert!(
            o.budget_ledger
                .get("MT-1", o.budget_hold_ttl())
                .unwrap()
                .reason
                .contains("today's spend is unknown")
        );
        o.set_store(Arc::new(rhapsody_store::Noop));
        o.dispatch_issue(issue("2", "MT-2", "Todo"), None, None, String::new());
        assert!(dispatched.lock().unwrap().is_empty());
        assert!(
            o.budget_ledger
                .get("MT-2", o.budget_hold_ttl())
                .unwrap()
                .reason
                .contains("storage is unavailable")
        );
    }

    #[test]
    fn historical_tokens_without_dollar_observations_are_unknown_spend() {
        let store = Sqlite::open(StorePath::InMemory).unwrap();
        seed_spend(&store, "fireworks-ai", 100);
        assert!(
            daily_spend_usd(&store, "fireworks-ai", &local_day_start()).is_infinite(),
            "an upgrade or newly enabled USD budget must not erase today's unpriced spend"
        );
    }

    #[test]
    fn invalid_costs_are_unknown_but_explicit_zero_prices_are_valid() {
        for cost in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::INFINITY),
            Some(f64::NAN),
        ] {
            assert_eq!(
                run_cost_usd("p/m", &TokenCounts::default(), cost, &HashMap::new()),
                RunCost::Unknown
            );
        }
        let prices = HashMap::from([("p/m".into(), Price::default())]);
        assert_eq!(
            run_cost_usd(
                "p/m",
                &TokenCounts {
                    input: i64::MAX,
                    ..Default::default()
                },
                None,
                &prices
            ),
            RunCost::Priced {
                usd: 0.0,
                source: CostSource::PriceTable
            }
        );
    }

    #[test]
    fn configured_prices_and_usd_round_trip_and_reject_invalid_numbers() {
        let cfg = usd_config(
            "prices:\n  p/m: {input: 2, output: 8, cache_read: 0.5, cache_write: 3}\nbudgets:\n  p: {daily_usd: 2.5, daily_tokens: 100, per_ticket: 10}\n",
        );
        let encoded = rhapsody_config::encode(&cfg).unwrap();
        let decoded = rhapsody_config::decode(&encoded).unwrap();
        assert_eq!(decoded.prices, cfg.prices);
        assert_eq!(decoded.budgets, cfg.budgets);
        for yaml in [
            "budgets:\n  p: {daily_usd: .nan}",
            "budgets:\n  p: {daily_usd: -1}",
            "prices:\n  p/m: {input: .inf}",
            "prices:\n  p/m: {cache_read: -1}",
            "prices:\n  m: {input: 1}",
        ] {
            let def = rhapsody_config::workflow::Definition {
                config: serde_yaml_ng::from_str(yaml).unwrap(),
                prompt_template: String::new(),
            };
            assert!(rhapsody_config::decode(&def).is_err(), "must reject {yaml}");
        }
    }

    #[test]
    fn chatgpt_equivalents_never_gate_even_with_a_table_and_a_spent_usd_limit() {
        use rhapsody_agent::{EVENT_SESSION_STARTED, EVENT_TURN_COMPLETED, Event, Usage};
        let dir = crate::testsupport::TempDir::new();
        let auth = dir.child("auth.json");
        std::fs::write(&auth, br#"{"openai":{"type":"oauth"}}"#).unwrap();
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let (mut o, dispatched) = orch_with_budgets(store.clone(), &[]);
        o.eff.as_mut().unwrap().cfg = usd_config(
            "agent: {backend: opencode}\nopencode: {model: openai/gpt-test}\nprices:\n  openai/gpt-test: {input: 2, output: 8}\nbudgets:\n  openai: {daily_usd: 1}\n",
        );
        o.eff.as_mut().unwrap().cfg.opencode.auth_source = auth;
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        for event in [
            Event {
                event_type: EVENT_SESSION_STARTED.into(),
                ..Default::default()
            },
            Event {
                event_type: EVENT_TURN_COMPLETED.into(),
                cost_usd: Some(0.0),
                usage: Some(Usage {
                    input_tokens: 1_000_000,
                    total_tokens: 1_000_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ] {
            o.on_agent_update(crate::agentupdate::AgentUpdate {
                issue_id: "1".into(),
                ev: event,
            });
        }
        assert_eq!(
            api_equivalent_usd_today(store.as_ref(), "chatgpt-subscription"),
            2.0
        );
        assert_eq!(
            daily_spend_usd(store.as_ref(), "openai", &local_day_start()),
            0.0
        );
        o.dispatch_issue(issue("2", "MT-2", "Todo"), None, None, String::new());
        assert_eq!(dispatched.lock().unwrap().as_slice(), ["1", "2"]);
        assert!(o.budget_ledger.held(o.budget_hold_ttl()).is_empty());
    }

    /// Seeds a completed run billed to `provider` with `tokens` total, starting NOW (so it falls in
    /// today's local window the budget reads).
    fn seed_spend(store: &dyn Store, provider: &str, tokens: i64) -> i64 {
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
        id
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
                    per_ticket: 0,
                    ..Default::default()
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
        let ttl = o.budget_hold_ttl();
        assert!(
            o.budget_ledger.get("MT-1", ttl).is_some(),
            "the refusal must be recorded where the console and sweep read it"
        );
        assert_eq!(
            o.budget_ledger.get("MT-1", ttl).expect("hold").provider,
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
        assert!(o.budget_ledger.held(o.budget_hold_ttl()).is_empty());
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
        assert!(o.budget_ledger.held(o.budget_hold_ttl()).is_empty());
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
            o.budget_ledger.get("MT-1", o.budget_hold_ttl()).is_none(),
            "no hold is recorded for a run that was allowed"
        );
    }

    // A dispatched ticket clears a stale hold, so the console stops reporting a refusal that no
    // longer holds.
    #[test]
    fn a_successful_dispatch_clears_a_stale_hold() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        // A configured budget is what arms the gate's release path; the ceiling is unspent, so the
        // dispatch itself is allowed.
        let (mut o, _) = orch_with_budgets(store, &[("anthropic", 1_000_000)]);
        o.note_budget_hold("MT-1", "t", "core", "anthropic", 200, 250);
        assert!(o.budget_ledger.get("MT-1", o.budget_hold_ttl()).is_some());

        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());

        assert!(
            o.budget_ledger.get("MT-1", o.budget_hold_ttl()).is_none(),
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
            pr: String::new(),
            ..Default::default()
        };
        assert!(l.hold("MT-1", h.clone()), "first refusal announces");
        assert!(!l.hold("MT-1", h.clone()), "a repeat does not re-announce");
        assert_eq!(l.get("MT-1", HOLD_TTL_FLOOR).as_ref(), Some(&h));
        assert_eq!(l.held(HOLD_TTL_FLOOR).len(), 1);
        l.release("MT-1");
        assert!(l.get("MT-1", HOLD_TTL_FLOOR).is_none());
        assert!(l.held(HOLD_TTL_FLOOR).is_empty());
    }

    /// A subject that stopped being offered without dispatching must not sit on the console forever:
    /// an unrefreshed hold goes stale and drops. A subject still being refused every tick refreshes
    /// its hold and stays.
    #[test]
    fn a_stale_hold_drops_but_a_refreshed_one_stays() {
        let l = BudgetLedger::default();
        let h = BudgetHeld {
            subject: "MT-1".into(),
            title: "t".into(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 250,
            pr: String::new(),
            ..Default::default()
        };
        let old = Instant::now()
            .checked_sub(HOLD_TTL_FLOOR + Duration::from_secs(1))
            .expect("instant arithmetic");
        l.hold_at("MT-1", h.clone(), old);
        assert!(
            l.get("MT-1", HOLD_TTL_FLOOR).is_none(),
            "a hold not re-confirmed within TTL must drop"
        );
        assert!(l.held(HOLD_TTL_FLOOR).is_empty());

        l.hold("MT-1", h);
        assert!(
            l.get("MT-1", HOLD_TTL_FLOOR).is_some(),
            "a fresh hold stays"
        );
    }

    /// **sol round 1 on PR #199, finding 2.** The hold TTL is a FLOOR widened to two configured
    /// poll cadences, so a hold refreshed once per pass cannot age out between two passes on a poll
    /// interval longer than 300s. A fixed 300s dropped a genuinely-held subject mid-cycle on a
    /// ten-minute poll, and the sweep then fell back to an unexplained divergence.
    ///
    /// Mutation: make `budget_hold_ttl` return the floor unconditionally and the 5-minute-old hold
    /// reds as stale.
    #[test]
    fn a_hold_outlives_a_poll_interval_longer_than_the_floor() {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        let (mut o, _) = orch_with_budgets(store, &[("anthropic", 200)]);
        o.eff.as_mut().expect("eff").cfg.polling.interval_ms = 600_000;
        assert_eq!(o.budget_hold_ttl(), Duration::from_secs(1_200));

        let h = BudgetHeld {
            subject: "MT-1".into(),
            title: "t".into(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 250,
            pr: String::new(),
            ..Default::default()
        };
        let five_minutes_ago = Instant::now()
            .checked_sub(Duration::from_secs(300 + 1))
            .expect("instant arithmetic");
        o.budget_ledger.hold_at("MT-1", h, five_minutes_ago);

        assert!(
            o.budget_ledger.get("MT-1", o.budget_hold_ttl()).is_some(),
            "a hold refreshed once per ten-minute pass must survive a five-minute gap"
        );
        assert!(
            o.budget_ledger.get("MT-1", HOLD_TTL_FLOOR).is_none(),
            "the floor alone would have dropped it — that is the bug this pins"
        );
    }

    /// **sol round 1 on PR #199, finding 1.** Review holds are keyed by the review IDENTITY, so
    /// one reviewer's successful dispatch cannot release a sibling reviewer's still-active hold on
    /// the same pull request. Before the fix both held under `owner/repo#n`, and dispatching the
    /// second reviewer erased the first.
    #[test]
    fn two_reviewer_holds_on_one_pr_are_independent() {
        let l = BudgetLedger::default();
        let held = |provider: &str| BudgetHeld {
            subject: format!("pr:o/r#12@{provider}"),
            title: String::new(),
            project: "core".into(),
            provider: provider.into(),
            daily_tokens: 200,
            spent_tokens: 300,
            pr: "o/r#12".into(),
            ..Default::default()
        };
        let alice = held("anthropic");
        let jerry = held("fireworks-ai");
        l.hold(&alice.subject, alice.clone());
        l.hold(&jerry.subject, jerry.clone());

        // jerry dispatches successfully and releases only his own identity.
        l.release(&jerry.subject);
        assert_eq!(
            l.get_for_pr("o/r#12", HOLD_TTL_FLOOR).as_ref(),
            Some(&alice),
            "alice's hold must survive jerry's dispatch"
        );
        l.release(&alice.subject);
        assert!(l.get_for_pr("o/r#12", HOLD_TTL_FLOOR).is_none());
    }

    /// **alice round 2 on PR #199, blocker B1.** A REVIEW hold is refreshed on the review watcher's
    /// cadence — a `PR_STATE_POLL_INTERVAL` sleep plus up to two serial `gh` phases, not
    /// `polling.interval_ms` — so tying its expiry to the poll TTL dropped a genuinely-held review
    /// on the very path the incident was about (reviews). A review hold now also clears
    /// [`CAPACITY_HOLD_TTL`](crate::reviewwatch::CAPACITY_HOLD_TTL); a ticket hold still ages against
    /// the poll TTL alone.
    ///
    /// Mutation: age every entry against the caller's base (drop the `pr.is_empty()` branch in
    /// [`Entry::ttl`]) and the review assertion reds at 301s old while `CAPACITY_HOLD_TTL` is fresh.
    #[test]
    fn a_review_hold_outlives_the_poll_ttl_on_the_watchers_cadence() {
        let l = BudgetLedger::default();
        let review = BudgetHeld {
            subject: "pr:o/r#12@alice".into(),
            title: String::new(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 300,
            pr: "o/r#12".into(),
            ..Default::default()
        };
        let ticket = BudgetHeld {
            subject: "MT-1".into(),
            title: "t".into(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 300,
            pr: String::new(),
            ..Default::default()
        };
        let old = Instant::now()
            .checked_sub(HOLD_TTL_FLOOR + Duration::from_secs(1))
            .expect("instant arithmetic");
        l.hold_at(&review.subject, review.clone(), old);
        l.hold_at(&ticket.subject, ticket.clone(), old);

        assert!(
            l.get_for_pr("o/r#12", HOLD_TTL_FLOOR).is_some(),
            "a review hold aged past the poll TTL must stay: the watcher, not the poll, refreshes it"
        );
        assert!(
            l.get("pr:o/r#12@alice", HOLD_TTL_FLOOR).is_some(),
            "the review hold is visible by identity too"
        );
        assert!(
            l.get("MT-1", HOLD_TTL_FLOOR).is_none(),
            "a ticket hold of the same age still drops at the poll TTL"
        );
        let listed = l.held(HOLD_TTL_FLOOR);
        assert!(
            listed.iter().any(|h| h.subject == review.subject),
            "the console lists the live review hold"
        );
        assert!(
            !listed.iter().any(|h| h.subject == ticket.subject),
            "the console drops the stale ticket hold"
        );
    }

    /// `get_for_pr` matches the REQUESTED coordinate, not merely any hold that carries one.
    /// Mutation: replace `e.held.pr == pr` with `!pr.is_empty()` and this reds, because the sweep
    /// would then name an unrelated pull request's budget as a divergence's cause.
    #[test]
    fn get_for_pr_matches_only_the_requested_coordinate() {
        let l = BudgetLedger::default();
        let held = |coordinate: &str, subject: &str| BudgetHeld {
            subject: subject.into(),
            title: String::new(),
            project: "core".into(),
            provider: "anthropic".into(),
            daily_tokens: 200,
            spent_tokens: 300,
            pr: coordinate.into(),
            ..Default::default()
        };
        let one = held("o/r#1", "pr:o/r#1@alice");
        let two = held("o/r#2", "pr:o/r#2@bob");
        l.hold(&one.subject, one.clone());
        l.hold(&two.subject, two.clone());

        assert_eq!(l.get_for_pr("o/r#1", HOLD_TTL_FLOOR).as_ref(), Some(&one));
        assert_eq!(l.get_for_pr("o/r#2", HOLD_TTL_FLOOR).as_ref(), Some(&two));
        assert!(
            l.get_for_pr("o/r#3", HOLD_TTL_FLOOR).is_none(),
            "a coordinate with no hold must answer None, not some other PR's hold"
        );
    }

    /// A boundary computed through the zone's own rules uses MIDNIGHT's offset, not `now`'s. On the
    /// 2026-03-08 US spring-forward, noon local is PDT (-07) but midnight was PST (-08), so the day
    /// starts at 08:00Z — NOT the 07:00Z that applying noon's offset to midnight yields, which folded
    /// an extra hour of yesterday's spend into today (sol round 1 on PR #199, finding 3).
    ///
    /// Mutation: compute the boundary from `now.date_naive()` shifted by `now.offset()` and this reds
    /// on `2026-03-08T07:00:00Z`.
    #[test]
    fn the_day_boundary_uses_midnights_own_offset_across_a_transition() {
        use chrono::{FixedOffset, NaiveDate, NaiveDateTime, Offset};

        /// An offset carrying whole seconds, the minimal [`Offset`] the fake zone needs.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Off(i32);
        impl Offset for Off {
            fn fix(&self) -> FixedOffset {
                FixedOffset::east_opt(self.0).expect("valid offset seconds")
            }
        }

        /// One spring-forward zone: local 2026-03-08T02:00–03:00 does not exist, and the offset goes
        /// from -08 to -07 across it.
        #[derive(Clone, Debug)]
        struct SpringForward;
        impl TimeZone for SpringForward {
            type Offset = Off;
            fn from_offset(_o: &Off) -> Self {
                SpringForward
            }
            fn offset_from_local_date(&self, d: &NaiveDate) -> LocalResult<Off> {
                self.offset_from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Off> {
                let gap_start = NaiveDate::from_ymd_opt(2026, 3, 8)
                    .expect("date")
                    .and_hms_opt(2, 0, 0)
                    .expect("gap start");
                let gap_end = gap_start + TimeDelta::hours(1);
                if *local >= gap_start && *local < gap_end {
                    LocalResult::None
                } else if *local >= gap_end {
                    LocalResult::Single(Off(-7 * 3600))
                } else {
                    LocalResult::Single(Off(-8 * 3600))
                }
            }
            fn offset_from_utc_date(&self, d: &NaiveDate) -> Off {
                self.offset_from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Off {
                let switch = NaiveDate::from_ymd_opt(2026, 3, 8)
                    .expect("date")
                    .and_hms_opt(10, 0, 0)
                    .expect("switch");
                if *utc < switch {
                    Off(-8 * 3600)
                } else {
                    Off(-7 * 3600)
                }
            }
        }

        let noon = NaiveDate::from_ymd_opt(2026, 3, 8)
            .expect("date")
            .and_hms_opt(12, 0, 0)
            .expect("noon");
        let now = SpringForward
            .from_local_datetime(&noon)
            .single()
            .expect("noon is unambiguous");
        assert_eq!(*now.offset(), Off(-7 * 3600), "sanity: noon is on PDT");
        assert_eq!(
            day_start(now),
            "2026-03-08T08:00:00Z",
            "the day starts at midnight's PST offset, not noon's PDT offset"
        );
    }

    /// A zone whose transition SKIPS midnight (as `America/Santiago`'s spring-forward does) must not
    /// lose the day: the boundary walks forward to the first local instant that exists.
    #[test]
    fn the_day_boundary_survives_a_zone_that_skips_midnight() {
        use chrono::{FixedOffset, NaiveDate, NaiveDateTime, Offset};

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Off(i32);
        impl Offset for Off {
            fn fix(&self) -> FixedOffset {
                FixedOffset::east_opt(self.0).expect("valid offset seconds")
            }
        }

        /// Local 2026-09-06T00:00–01:00 does not exist; the day begins at 01:00.
        #[derive(Clone, Debug)]
        struct SkipsMidnight;
        impl TimeZone for SkipsMidnight {
            type Offset = Off;
            fn from_offset(_o: &Off) -> Self {
                SkipsMidnight
            }
            fn offset_from_local_date(&self, d: &NaiveDate) -> LocalResult<Off> {
                self.offset_from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Off> {
                let gap_start = NaiveDate::from_ymd_opt(2026, 9, 6)
                    .expect("date")
                    .and_hms_opt(0, 0, 0)
                    .expect("gap start");
                let gap_end = gap_start + TimeDelta::hours(1);
                if *local >= gap_start && *local < gap_end {
                    LocalResult::None
                } else {
                    LocalResult::Single(Off(-3 * 3600))
                }
            }
            fn offset_from_utc_date(&self, d: &NaiveDate) -> Off {
                self.offset_from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_utc_datetime(&self, _utc: &NaiveDateTime) -> Off {
                Off(-3 * 3600)
            }
        }

        let noon = NaiveDate::from_ymd_opt(2026, 9, 6)
            .expect("date")
            .and_hms_opt(12, 0, 0)
            .expect("noon");
        let now = SkipsMidnight
            .from_local_datetime(&noon)
            .single()
            .expect("noon exists");
        assert_eq!(
            day_start(now),
            "2026-09-06T04:00:00Z",
            "a midnight that does not exist must resolve to the first instant that does (01:00 -03)"
        );
    }

    /// An AMBIGUOUS midnight (a fall-back that repeats the hour around 00:00) resolves to the
    /// EARLIER instant, matching the "earliest" arm of `resolve_midnight`. Alice round 2 noted this
    /// choice was unpinned: swapping in `latest` left the suite green. The fall-back is rare in
    /// practice, but the day boundary is the budget's own input, so the tie-break is worth a pin.
    ///
    /// Mutation: take the later half of the `Ambiguous` pair and this reds on `05:00:00Z`.
    #[test]
    fn an_ambiguous_midnight_resolves_to_the_earliest_instant() {
        use chrono::{FixedOffset, NaiveDate, NaiveDateTime, Offset};

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Off(i32);
        impl Offset for Off {
            fn fix(&self) -> FixedOffset {
                FixedOffset::east_opt(self.0).expect("valid offset seconds")
            }
        }

        /// Every local instant maps to two: -04 first, then -05 (a fall-back landing on midnight).
        #[derive(Clone, Debug)]
        struct AmbiguousMidnight;
        impl TimeZone for AmbiguousMidnight {
            type Offset = Off;
            fn from_offset(_o: &Off) -> Self {
                AmbiguousMidnight
            }
            fn offset_from_local_date(&self, d: &NaiveDate) -> LocalResult<Off> {
                self.offset_from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_local_datetime(&self, _local: &NaiveDateTime) -> LocalResult<Off> {
                LocalResult::Ambiguous(Off(-4 * 3600), Off(-5 * 3600))
            }
            fn offset_from_utc_date(&self, d: &NaiveDate) -> Off {
                self.offset_from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            }
            fn offset_from_utc_datetime(&self, _utc: &NaiveDateTime) -> Off {
                Off(-5 * 3600)
            }
        }

        let noon = NaiveDate::from_ymd_opt(2026, 11, 1)
            .expect("date")
            .and_hms_opt(12, 0, 0)
            .expect("noon");
        let now = AmbiguousMidnight
            .from_local_datetime(&noon)
            .earliest()
            .expect("noon is ambiguous but present in both halves");
        assert_eq!(
            day_start(now),
            "2026-11-01T04:00:00Z",
            "an ambiguous midnight must resolve to the earlier instant (-04), not the later (-05)"
        );
    }

    /// STUDIO-979 introduces a brokered provider day budget keyed by the UTC day, DELIBERATELY
    /// separate from this meter's LOCAL-day view. This pins that separation for one instant:
    /// 2024-01-01T10:30:00Z is 2024-01-01 in UTC, but the +14 zone's local day has already rolled to
    /// 2024-01-02 (its midnight is 2024-01-01T10:00:00Z) while the -11 zone is still on 2023-12-31
    /// (its day started at 2023-12-31T11:00:00Z). `providerbudget.rs`'s
    /// `the_day_key_is_the_utc_day_not_the_callers_zone` asserts the SAME instant buckets to the UTC
    /// day — so the two quantities are proven distinct, and this local view is unchanged by the new
    /// authority.
    #[test]
    fn the_local_day_view_stays_local_for_the_brokered_authoritys_instant() {
        use chrono::{FixedOffset, Utc};

        // 2024-01-01T10:30:00Z.
        let instant = DateTime::<Utc>::from_timestamp(1_704_105_000, 0).expect("valid instant");
        let east = FixedOffset::east_opt(14 * 3_600).expect("valid +14");
        let west = FixedOffset::west_opt(11 * 3_600).expect("valid -11");
        assert_eq!(
            day_start(instant.with_timezone(&east)),
            "2024-01-01T10:00:00Z",
            "the +14 local day starts at its own midnight, one calendar day later than UTC"
        );
        assert_eq!(
            day_start(instant.with_timezone(&west)),
            "2023-12-31T11:00:00Z",
            "the -11 local day started a calendar day earlier than UTC"
        );
    }
}
