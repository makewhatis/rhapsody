//! The Linear request budget (STUDIO-1896) — Rhapsody-only; Symphony has no counterpart.
//!
//! Linear allows [`HOURLY_LIMIT`] requests per rolling hour and answers past it with HTTP 429. The
//! daemon builds one [`linear::Client`](crate::linear::Client) per configured project, but every one
//! of them spends the SAME allowance, so the ledger is shared per key ([`ledger_for`]): fourteen
//! projects are one budget, not fourteen.
//!
//! **The single-key assumption.** Linear charges the allowance to the authenticated USER, not to
//! the key: every API key that user holds, and anything else acting as them, draws on one bucket.
//! The ledger can only count what this process sends on the keys it was configured with, so its
//! local count is accurate only when the daemon's key is that user's sole consumer. Two keys for
//! one user get two ledgers here, each under-counting by the other's share, and a 429 blocks only
//! the key that received it. What covers the gap is Linear's own `X-RateLimit-Requests-Remaining`,
//! which reports the user's bucket rather than this process's share and tightens the budget on its
//! own. Give the daemon a Linear user of its own if anything else spends that user's requests.
//!
//! The ledger does three things, and decides nothing on its own:
//!
//! * **Counts every request by the operation that sent it.** The count is taken in the transport
//!   itself (`do_graphql`) under the operation name `traced` scopes, so the per-operation counts add
//!   up to the total by construction — unlike counting HTTP-pool log lines, which hyper sometimes
//!   emits from a spawned task outside any span.
//! * **Says when the budget is tight** — at or above [`TIGHT_PERCENT`] of the limit in the last
//!   hour, or when Linear's own `X-RateLimit-Requests-Remaining` says so. The orchestrator reads
//!   [`BudgetStatus::tight`] to slow the poll and the background sweeps; the transport never
//!   refuses a request for being tight, so dispatch-critical reads keep flowing. One warning is
//!   logged on each transition into tight.
//! * **Remembers a 429's reset** ([`RequestLedger::block_until`]), so every client on the key stops
//!   sending until Linear's window reopens instead of spending more requests on certain refusals.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Linear's documented allowance per authenticated user (shared by all that user's API keys):
/// requests per rolling hour.
pub const HOURLY_LIMIT: u32 = 2_500;

/// The share of [`HOURLY_LIMIT`] at which the budget counts as tight.
pub const TIGHT_PERCENT: u32 = 80;

/// The rolling window the ledger counts over.
pub const WINDOW: Duration = Duration::from_secs(3_600);

/// The operation name a request is counted under when no `traced` operation scoped it. A non-zero
/// count here is a bug: it names a Linear call nobody can attribute.
pub const UNTAGGED: &str = "untagged";

/// The longest a 429 may block the key for, however far away a reset header claims the reset is —
/// a corrupt or misread header must not silence the tracker for a day.
pub const MAX_BLOCK: Duration = WINDOW;

/// How long a remote `X-RateLimit-Requests-Remaining` observation is trusted for.
const REMOTE_FRESH: Duration = Duration::from_secs(300);

/// A point-in-time view of the key's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetStatus {
    /// Requests this daemon sent on the key in the last [`WINDOW`].
    pub used: u32,
    /// The allowance [`used`](Self::used) is measured against.
    pub limit: u32,
    /// At or past [`TIGHT_PERCENT`] (locally counted, or as Linear last reported it).
    pub tight: bool,
    /// How long a 429 still blocks the key, if one does.
    pub blocked_for: Option<Duration>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Request times and their operation, oldest first, pruned to [`WINDOW`].
    window: VecDeque<(Instant, &'static str)>,
    /// When a 429 stops blocking the key.
    blocked_until: Option<Instant>,
    /// Linear's last `(remaining, limit)` report and when it was seen.
    remote: Option<(u32, u32, Instant)>,
    /// Whether the last evaluation was tight, so the warning fires once per transition.
    tight: bool,
}

/// One API key's request ledger. Every method takes `now` so tests can drive the clock.
#[derive(Debug, Default)]
pub struct RequestLedger {
    inner: Mutex<Inner>,
}

impl RequestLedger {
    /// A fresh, empty ledger (tests; production goes through [`ledger_for`]).
    pub fn new() -> RequestLedger {
        RequestLedger::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one request sent by `op` at `now`, logging a single warning if it tips the budget
    /// into tight.
    pub fn record(&self, op: &'static str, now: Instant) {
        let mut inner = self.lock();
        prune(&mut inner.window, now);
        inner.window.push_back((now, op));
        let was_tight = inner.tight;
        let tight = is_tight(&inner, now);
        inner.tight = tight;
        if tight && !was_tight {
            let used = inner.window.len();
            let breakdown = format_breakdown(&per_op_of(&inner.window));
            drop(inner);
            tracing::warn!(
                used,
                limit = HOURLY_LIMIT,
                breakdown = %breakdown,
                "Linear request budget is tight (at or above {TIGHT_PERCENT}% of the hourly limit); \
                 slowing the poll and background sweeps"
            );
        } else if !tight && was_tight {
            drop(inner);
            tracing::info!("Linear request budget has relaxed; normal poll cadence resumes");
        }
    }

    /// Records Linear's own view of the key (`X-RateLimit-Requests-Remaining` / `-Limit`).
    pub fn observe_remote(&self, remaining: u32, limit: u32, now: Instant) {
        if limit == 0 {
            return;
        }
        let mut inner = self.lock();
        inner.remote = Some((remaining, limit, now));
    }

    /// Blocks the key until `until` (a 429's reset), clamped to [`MAX_BLOCK`] from `now`. A later
    /// block never shortens an earlier one.
    pub fn block_until(&self, until: Instant, now: Instant) {
        let until = until.min(now + MAX_BLOCK);
        let mut inner = self.lock();
        if inner.blocked_until.is_none_or(|cur| cur < until) {
            inner.blocked_until = Some(until);
        }
    }

    /// How long the key is still blocked at `now`, if it is.
    pub fn blocked_for(&self, now: Instant) -> Option<Duration> {
        let inner = self.lock();
        inner
            .blocked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// The budget at `now`.
    pub fn status(&self, now: Instant) -> BudgetStatus {
        let mut inner = self.lock();
        prune(&mut inner.window, now);
        let blocked_for = inner
            .blocked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero());
        BudgetStatus {
            used: u32::try_from(inner.window.len()).unwrap_or(u32::MAX),
            limit: HOURLY_LIMIT,
            tight: is_tight(&inner, now) || blocked_for.is_some(),
            blocked_for,
        }
    }

    /// Requests in the last [`WINDOW`] by operation. The values sum to [`BudgetStatus::used`].
    pub fn per_op(&self, now: Instant) -> BTreeMap<&'static str, u32> {
        let mut inner = self.lock();
        prune(&mut inner.window, now);
        per_op_of(&inner.window)
    }
}

fn prune(window: &mut VecDeque<(Instant, &'static str)>, now: Instant) {
    while let Some((at, _)) = window.front() {
        if now.saturating_duration_since(*at) >= WINDOW {
            window.pop_front();
        } else {
            break;
        }
    }
}

fn is_tight(inner: &Inner, now: Instant) -> bool {
    let threshold = HOURLY_LIMIT * TIGHT_PERCENT / 100;
    if inner.window.len() >= threshold as usize {
        return true;
    }
    match inner.remote {
        Some((remaining, limit, seen)) if now.saturating_duration_since(seen) < REMOTE_FRESH => {
            // Remaining at or below (100 - TIGHT_PERCENT)% of Linear's own limit.
            u64::from(remaining) * 100 <= u64::from(limit) * u64::from(100 - TIGHT_PERCENT)
        }
        _ => false,
    }
}

fn per_op_of(window: &VecDeque<(Instant, &'static str)>) -> BTreeMap<&'static str, u32> {
    let mut out = BTreeMap::new();
    for (_, op) in window {
        *out.entry(*op).or_insert(0) += 1;
    }
    out
}

/// `op=count` pairs, largest first — the shape the tight warning logs.
pub fn format_breakdown(per_op: &BTreeMap<&'static str, u32>) -> String {
    let mut rows: Vec<(&str, u32)> = per_op.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    rows.iter()
        .map(|(op, n)| format!("{op}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The ledger every client on `api_key` shares. Keyed by a hash so the registry never holds the key
/// itself; a collision would merely merge two keys' counts.
///
/// Keyed by API KEY because that is all a client knows, though Linear's quota is per USER: two keys
/// for one user are one bucket at Linear and two ledgers here. See the module doc's single-key
/// assumption for what covers that gap.
pub fn ledger_for(api_key: &str) -> Arc<RequestLedger> {
    static LEDGERS: OnceLock<Mutex<HashMap<u64, Arc<RequestLedger>>>> = OnceLock::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    api_key.hash(&mut hasher);
    let key = hasher.finish();
    let mut map = LEDGERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(map.entry(key).or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_by_operation_and_the_counts_add_up() {
        let ledger = RequestLedger::new();
        let t0 = Instant::now();
        for i in 0..5 {
            ledger.record("fetch_candidates", t0 + Duration::from_secs(i));
        }
        ledger.record("fetch_issue_states", t0);
        let per_op = ledger.per_op(t0 + Duration::from_secs(10));
        assert_eq!(per_op.get("fetch_candidates"), Some(&5));
        assert_eq!(per_op.get("fetch_issue_states"), Some(&1));
        let status = ledger.status(t0 + Duration::from_secs(10));
        assert_eq!(per_op.values().sum::<u32>(), status.used);
    }

    #[test]
    fn requests_older_than_the_window_fall_out() {
        let ledger = RequestLedger::new();
        let t0 = Instant::now();
        ledger.record("a", t0);
        ledger.record("a", t0 + Duration::from_secs(1_800));
        assert_eq!(ledger.status(t0 + WINDOW).used, 1);
        assert_eq!(ledger.status(t0 + WINDOW + WINDOW).used, 0);
    }

    #[test]
    fn tight_at_eighty_percent_and_not_before() {
        let ledger = RequestLedger::new();
        let t0 = Instant::now();
        let threshold = HOURLY_LIMIT * TIGHT_PERCENT / 100;
        for _ in 0..threshold - 1 {
            ledger.record("fetch_candidates", t0);
        }
        assert!(!ledger.status(t0).tight);
        ledger.record("fetch_candidates", t0);
        assert!(ledger.status(t0).tight);
        // An hour later the window has drained and the budget relaxes.
        assert!(!ledger.status(t0 + WINDOW).tight);
    }

    #[test]
    fn linears_own_remaining_count_tightens_the_budget() {
        let ledger = RequestLedger::new();
        let t0 = Instant::now();
        ledger.observe_remote(400, 2_500, t0);
        assert!(ledger.status(t0).tight, "400/2500 left is under 20%");
        ledger.observe_remote(1_000, 2_500, t0);
        assert!(!ledger.status(t0).tight);
        ledger.observe_remote(100, 2_500, t0);
        assert!(
            !ledger.status(t0 + REMOTE_FRESH).tight,
            "a stale remote report is not trusted"
        );
    }

    #[test]
    fn a_block_expires_never_shortens_and_is_clamped() {
        let ledger = RequestLedger::new();
        let t0 = Instant::now();
        ledger.block_until(t0 + Duration::from_secs(30), t0);
        ledger.block_until(t0 + Duration::from_secs(10), t0);
        assert_eq!(ledger.blocked_for(t0), Some(Duration::from_secs(30)));
        assert!(ledger.status(t0).tight, "a blocked key is tight");
        assert_eq!(ledger.blocked_for(t0 + Duration::from_secs(30)), None);
        ledger.block_until(t0 + Duration::from_secs(86_400), t0);
        assert_eq!(ledger.blocked_for(t0), Some(MAX_BLOCK));
    }

    #[test]
    fn clients_on_one_key_share_a_ledger() {
        let a = ledger_for("budget-test-key-shared");
        let b = ledger_for("budget-test-key-shared");
        let other = ledger_for("budget-test-key-other");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &other));
    }

    #[test]
    fn breakdown_lists_the_largest_caller_first() {
        let mut per_op = BTreeMap::new();
        per_op.insert("fetch_issue_states", 2);
        per_op.insert("fetch_candidates", 9);
        assert_eq!(
            format_breakdown(&per_op),
            "fetch_candidates=9 fetch_issue_states=2"
        );
    }
}
