//! providerbudget — the durable UTC-day budget authority the daemon injects into brokered
//! provider admission (STUDIO-979, `provider-broker-design.md` §8.1). Rhapsody-only; no Go
//! counterpart (the frozen reference has no broker).
//!
//! The provider broker owns the [`CumulativeBudgetAuthority`] contract and calls it once per
//! admission (and once before a turn is armed); it holds no store dependency. This module is the
//! OTHER half: the store-backed implementation the composition root injects. The durable counter
//! itself lives in `rhapsody-store` (`charge_provider_day_tokens` / `provider_day_tokens`), keyed by
//! `(stable_provider_id, UTC day)`, so concurrent runs and daemon restarts cannot oversubscribe the
//! configured cap.
//!
//! This is deliberately NOT the STUDIO-957 local-day dispatch meter (`orchestrator/budget.rs`):
//! that meter keeps its local-day window and reports/refuses whole dispatches, while this authority
//! is a UTC-day, per-admission reservation the broker enforces before egress. The two coexist
//! without sharing state, and a native (non-brokered) harness keeps using the STUDIO-957 meter and
//! the STUDIO-967 per-run ceiling unchanged.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use rhapsody_provider_broker::{CumulativeBudgetAuthority, DayBudgetRefusal};
use rhapsody_store::Store;

/// The UTC day bucket for a Unix timestamp — whole days since the epoch. Floor division is
/// `div_euclid`, so an instant before UTC midnight stays on the previous day (and a pre-epoch
/// instant floors down rather than toward zero).
pub fn utc_day_of(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(86_400)
}

/// Whole UTC days since the Unix epoch — the broker's `UtcDay` bucket key. Uses the UTC clock, NOT
/// the daemon host's local day: the brokered authority is deliberately UTC (STUDIO-957's local-day
/// dispatch meter is the separate local view and is unchanged by this).
pub fn utc_day_now() -> i64 {
    utc_day_of(chrono::Utc::now().timestamp())
}

/// The daemon's durable, non-secret [`CumulativeBudgetAuthority`] over the history store.
///
/// It charges atomically through the store's single-statement conditional increment; a store error
/// is the typed [`DayBudgetRefusal::Unavailable`] (a configured cap is a hard boundary, never a
/// best-effort one), and a charge that would exceed the cap is [`DayBudgetRefusal::Exhausted`]
/// with nothing recorded.
pub struct StoreDayAuthority {
    store: Arc<dyn Store + Send + Sync>,
    /// True only for the on-disk store. A non-durable handle (`Noop`, `:memory:`, a failed open)
    /// cannot be a durable day authority, so every charge fails CLOSED with
    /// [`DayBudgetRefusal::Unavailable`] without touching it — a configured cap over such a store is
    /// refused at startup, and this is the defense-in-depth answer if one ever appears at runtime.
    durable: bool,
    /// Test seam: a fixed UTC day. Production leaves this `None` and reads the system UTC clock.
    day_override: Option<Arc<AtomicI64>>,
}

impl fmt::Debug for StoreDayAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let day = self
            .day_override
            .as_ref()
            .map(|d| d.load(Ordering::Acquire));
        f.debug_struct("StoreDayAuthority")
            .field("day_override", &day)
            .finish_non_exhaustive()
    }
}

impl StoreDayAuthority {
    /// Builds the authority over the daemon's store handle. `durable` is the boot's durability
    /// verdict (`open_store`'s second return): only the on-disk `Sqlite` is `true`.
    pub fn new(store: Arc<dyn Store + Send + Sync>, durable: bool) -> Self {
        Self {
            store,
            durable,
            day_override: None,
        }
    }

    /// Test seam: an authority whose "today" is externally controlled, so a UTC rollover can be
    /// exercised without waiting for midnight. Returns the shared day cell the caller mutates.
    #[cfg(test)]
    pub(crate) fn with_day(
        store: Arc<dyn Store + Send + Sync>,
        durable: bool,
        day: i64,
    ) -> (Self, Arc<AtomicI64>) {
        let cell = Arc::new(AtomicI64::new(day));
        (
            Self {
                store,
                durable,
                day_override: Some(Arc::clone(&cell)),
            },
            cell,
        )
    }

    fn today(&self) -> i64 {
        match &self.day_override {
            Some(cell) => cell.load(Ordering::Acquire),
            None => utc_day_now(),
        }
    }
}

impl CumulativeBudgetAuthority for StoreDayAuthority {
    fn try_charge(&self, provider_id: &str, tokens: u64, cap: u64) -> Result<(), DayBudgetRefusal> {
        if !self.durable {
            return Err(DayBudgetRefusal::Unavailable);
        }
        match self
            .store
            .charge_provider_day_tokens(provider_id, self.today(), tokens, cap)
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(DayBudgetRefusal::Exhausted),
            Err(_) => Err(DayBudgetRefusal::Unavailable),
        }
    }

    fn charged_today(&self, provider_id: &str) -> u64 {
        self.store
            .provider_day_tokens(provider_id, self.today())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{Noop, Sqlite, StorePath};
    use std::sync::atomic::AtomicU32;

    fn scratch_dir() -> std::path::PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::AcqRel);
        let dir = std::env::temp_dir().join(format!(
            "rhapsody-providerbudget-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn disk_store(dir: &std::path::Path) -> Sqlite {
        Sqlite::open(StorePath::Disk(dir.join("rhapsody.db"))).expect("open disk store")
    }

    // The authority charges the store's durable counter and refuses at the cap. MUTATION: read
    // `charged_today` then charge in two calls and the concurrent test below reds.
    #[test]
    fn charges_accumulate_and_refuse_at_the_cap() {
        let dir = scratch_dir();
        let (authority, _day) =
            StoreDayAuthority::with_day(Arc::new(disk_store(&dir)), true, 20_000);

        authority.try_charge("p", 60, 100).expect("60 fits");
        assert_eq!(authority.charged_today("p"), 60);
        assert_eq!(
            authority.try_charge("p", 50, 100).unwrap_err(),
            DayBudgetRefusal::Exhausted
        );
        assert_eq!(
            authority.charged_today("p"),
            60,
            "a refused charge records nothing"
        );
        authority.try_charge("p", 40, 100).expect("totals 100");
        assert_eq!(authority.charged_today("p"), 100);
        assert_eq!(
            authority.try_charge("p", 1, 100).unwrap_err(),
            DayBudgetRefusal::Exhausted
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The day key is UTC and the provider key is the stable provider id: two providers do not share
    // a bucket, and a UTC rollover resets today's spend. MUTATION: use the local day and the
    // rollover assert fails on a machine whose local day differs from UTC.
    #[test]
    fn per_provider_isolation_and_a_utc_rollover() {
        let dir = scratch_dir();
        let (authority, day) =
            StoreDayAuthority::with_day(Arc::new(disk_store(&dir)), true, 20_000);

        authority.try_charge("a", 100, 100).expect("a's day");
        authority
            .try_charge("b", 100, 100)
            .expect("b has its own day");

        day.store(20_001, Ordering::Release);
        authority
            .try_charge("a", 100, 100)
            .expect("a new UTC day resets a");

        day.store(20_000, Ordering::Release);
        assert_eq!(
            authority.try_charge("a", 1, 100).unwrap_err(),
            DayBudgetRefusal::Exhausted,
            "yesterday's charge is still enforced when the day cell returns to it"
        );
        assert_eq!(authority.charged_today("b"), 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A charge survives a process restart because it lives in the durable store, not in memory.
    #[test]
    fn a_charge_survives_a_restart() {
        let dir = scratch_dir();
        {
            let authority = StoreDayAuthority::new(Arc::new(disk_store(&dir)), true);
            authority.try_charge("p", 80, 80).expect("spends the day");
        }
        let (authority, _day) =
            StoreDayAuthority::with_day(Arc::new(disk_store(&dir)), true, utc_day_now());
        assert_eq!(
            authority.try_charge("p", 1, 80).unwrap_err(),
            DayBudgetRefusal::Exhausted,
            "the restarted daemon reads the same day authority"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Over a store that cannot answer, the authority fails CLOSED with `Unavailable` rather than
    // admitting the request. (A configured cap over such a store is refused at startup; this is the
    // defense-in-depth answer.)
    #[test]
    fn a_disabled_store_fails_closed() {
        let authority = StoreDayAuthority::new(Arc::new(Noop), false);
        assert_eq!(
            authority.try_charge("p", 1, 100).unwrap_err(),
            DayBudgetRefusal::Unavailable
        );
        assert_eq!(authority.charged_today("p"), 0);
    }

    // End-to-end: the authority backs a real broker session's day charge. A turn that reserves the
    // whole day cap and then ends WITHOUT settling (an aborted/unknown request) keeps the full
    // reservation, so a fresh session (a restarted daemon reading the same store) is refused before
    // spawn. MUTATION: release the day charge on abort/unknown, or key it per session, and run 2
    // admits the next turn.
    #[test]
    fn a_brokered_day_charge_is_not_released_by_an_aborted_request() {
        use rhapsody_provider_broker::{
            BoundCredentialLease, Broker, BrokerError, BrokerLimits, BrokerProtocol,
            BrokerRegistrationPlan, CredentialBinding, ManualClock, ScriptedRandom, SessionPolicy,
            TurnMeta,
        };

        const ENDPOINT: &str = "https://api.example.com/v1";
        let broker = Broker::new(
            "http://127.0.0.1:41234/v1",
            Arc::new(ManualClock::new()),
            Arc::new(ScriptedRandom::new()),
        )
        .expect("broker");
        let dir = scratch_dir();
        let store: Arc<dyn Store + Send + Sync> = Arc::new(disk_store(&dir));
        let (authority, _day) = StoreDayAuthority::with_day(Arc::clone(&store), true, 20_000);
        let authority: Arc<dyn CumulativeBudgetAuthority> = Arc::new(authority);

        let limits = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(100),
            ..BrokerLimits::default()
        };
        let register = |broker: &Broker| {
            let plan = BrokerRegistrationPlan::new(
                "p",
                BrokerProtocol::OpenAiChatCompletions,
                ENDPOINT,
                false,
                "model-x",
                limits,
            )
            .expect("plan");
            let lease = BoundCredentialLease::new(
                CredentialBinding::new("p", BrokerProtocol::OpenAiChatCompletions, ENDPOINT)
                    .expect("binding"),
                b"sk-fake-provider-key".to_vec(),
            )
            .expect("lease");
            let policy =
                SessionPolicy::with_day_authority(limits, Arc::clone(&authority)).expect("policy");
            broker
                .register_session(plan, lease, policy)
                .expect("registration")
        };

        // Run 1: reserve the whole day, then drop the grant without settling.
        {
            let mut registration = register(&broker);
            let (attempt, _receipt) = registration
                .ledgers
                .arm_turn(TurnMeta::without_deadline())
                .expect("arm");
            let access = attempt.mint_access().expect("mint");
            let token = access.api_key.expose_for_child(str::to_owned);
            let grant = broker.lookup_capability(&token).expect("grant");
            grant
                .reserve_request(100, 0, 0)
                .expect("token_cost = request_bytes + output_tokens = 100");
            assert_eq!(authority.charged_today("p"), 100);
            drop(grant);
            access.finish();
        }

        // Run 2: the day is still spent; arm refuses before any child spawn.
        let mut registration = register(&broker);
        assert_eq!(
            registration
                .ledgers
                .arm_turn(TurnMeta::without_deadline())
                .unwrap_err(),
            BrokerError::DayBudgetExhausted
        );
        assert_eq!(authority.charged_today("p"), 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The bucket is the UTC day: the boundary is exactly 86_400 seconds, and an instant just before
    // it stays on the previous day. MUTATION: divide by the LOCAL day (or round instead of floor)
    // and `utc_day_of(86_399)` becomes 1.
    #[test]
    fn utc_day_of_floors_at_the_utc_midnight_boundary() {
        assert_eq!(utc_day_of(0), 0);
        assert_eq!(utc_day_of(86_399), 0, "one second before the boundary");
        assert_eq!(utc_day_of(86_400), 1, "the boundary itself is the next day");
        assert_eq!(utc_day_of(2 * 86_400 + 5), 2);
        assert_eq!(utc_day_of(-1), -1, "pre-epoch instants floor down");
        // 2024-01-01T00:00:00Z is 1_704_067_200.
        assert_eq!(utc_day_of(1_704_067_200), 19_723);
        assert!(utc_day_now() > 19_723, "the clock only moves forward");
    }
}
