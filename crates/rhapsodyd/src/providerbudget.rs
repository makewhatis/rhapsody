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
#[cfg(test)]
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, FixedOffset, Utc};
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
    utc_day_of(Utc::now().timestamp())
}

/// The authority's source of "now": an absolute instant carrying the zone the caller considers
/// current. Production reads the system clock; a test injects a fixed instant (and may hand it a
/// far-offset zone) so the UTC-day key is pinned WITHOUT waiting for midnight and WITHOUT depending
/// on the host's timezone. The authority reads only the instant's ABSOLUTE timestamp — the zone rides
/// along precisely so a test can prove the wall-clock date it would show is ignored.
type NowFn = Arc<dyn Fn() -> DateTime<FixedOffset> + Send + Sync>;

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
    /// The clock the day key is read from. Production is the system UTC clock; tests inject a fixed
    /// instant so the UTC day (and only the UTC day) is pinned.
    now: NowFn,
}

impl fmt::Debug for StoreDayAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreDayAuthority")
            .field("durable", &self.durable)
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
            now: Arc::new(|| Utc::now().fixed_offset()),
        }
    }

    /// Test seam: an authority whose "today" is externally controlled, so a UTC rollover can be
    /// exercised without waiting for midnight. The returned cell holds a whole UTC DAY; the injected
    /// clock places it at that day's noon UTC, so these tests stay about day arithmetic rather than
    /// the midnight floor (which [`Self::with_instant`] pins).
    #[cfg(test)]
    pub(crate) fn with_day(
        store: Arc<dyn Store + Send + Sync>,
        durable: bool,
        day: i64,
    ) -> (Self, Arc<AtomicI64>) {
        let cell = Arc::new(AtomicI64::new(day));
        let day_cell = Arc::clone(&cell);
        let now: NowFn = Arc::new(move || {
            let secs = day_cell
                .load(Ordering::Acquire)
                .saturating_mul(86_400)
                .saturating_add(12 * 3_600);
            fixed_instant(
                secs,
                FixedOffset::east_opt(0).expect("UTC is a valid offset"),
            )
        });
        (
            Self {
                store,
                durable,
                now,
            },
            cell,
        )
    }

    /// Test seam: an authority whose clock is a FIXED instant in a CHOSEN zone, so a test can prove
    /// the day key follows the absolute UTC instant and not the wall-clock date that zone shows.
    #[cfg(test)]
    pub(crate) fn with_instant(
        store: Arc<dyn Store + Send + Sync>,
        durable: bool,
        at: DateTime<FixedOffset>,
    ) -> Self {
        let secs = at.timestamp();
        let offset = *at.offset();
        Self {
            store,
            durable,
            now: Arc::new(move || fixed_instant(secs, offset)),
        }
    }

    fn today(&self) -> i64 {
        utc_day_of((self.now)().timestamp())
    }
}

/// Builds the UTC instant `secs` in `offset`, for the test clock seams. A seconds value off the
/// representable calendar (impossible for any real `now`) falls back to the system clock.
#[cfg(test)]
fn fixed_instant(secs: i64, offset: FixedOffset) -> DateTime<FixedOffset> {
    DateTime::from_timestamp(secs, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&offset)
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
        let day = self.today();
        match self.store.provider_day_tokens(provider_id, day) {
            Ok(tokens) => tokens,
            Err(err) => {
                // This is NOT fail-open: it only weakens the pre-spawn EARLY refusal, because the
                // admission charge itself still fails closed (`Unavailable`). Log it so a broken
                // store is visible rather than silently reading as "nothing spent".
                tracing::warn!(
                    provider_id,
                    utc_day = day,
                    error = %err,
                    "could not read the provider day budget; reporting zero spent"
                );
                0
            }
        }
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

    // The authority charges the store's durable counter and refuses at the cap. MUTATION: replace the
    // single-statement charge with a `charged_today`-then-charge read-then-write and the
    // `concurrent_authority_charges_cannot_oversubscribe` test below reds.
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

    // The authority is what the broker actually calls, so its own cap enforcement must be atomic:
    // the store serializes each call, but a `charged_today`-then-charge read-then-write makes TWO
    // store calls with a gap a concurrent admission can slip through. MUTATION: read `charged_today`
    // then charge with an unbounded cap and a round oversubscribes. The window is short, so the
    // rounds are repeated (each against a fresh provider) to make the defect surface reliably; the
    // atomic charge is exactly `cap/cost` every round.
    #[test]
    fn concurrent_authority_charges_cannot_oversubscribe() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicU64;

        let dir = scratch_dir();
        let store: Arc<dyn Store + Send + Sync> = Arc::new(disk_store(&dir));
        let (authority, _day) = StoreDayAuthority::with_day(Arc::clone(&store), true, 20_000);
        let authority = Arc::new(authority);

        let cost = 100u64;
        let cap = 1_000u64;
        let threads = 64usize;
        for round in 0..6 {
            let provider = format!("p{round}");
            let barrier = Arc::new(Barrier::new(threads));
            let successes = Arc::new(AtomicU64::new(0));
            let mut handles = Vec::new();
            for _ in 0..threads {
                let authority = Arc::clone(&authority);
                let barrier = Arc::clone(&barrier);
                let successes = Arc::clone(&successes);
                let provider = provider.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    if authority.try_charge(&provider, cost, cap).is_ok() {
                        successes.fetch_add(1, Ordering::AcqRel);
                    }
                }));
            }
            for handle in handles {
                handle.join().expect("thread");
            }
            assert_eq!(
                successes.load(Ordering::Acquire),
                cap / cost,
                "round {round}: exactly cap/cost concurrent admissions may succeed"
            );
            assert_eq!(
                authority.charged_today(&provider),
                cap,
                "round {round}: the shared cap is never oversubscribed"
            );
        }
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

    // `utc_day_of` is the pure UTC-day floor: the boundary is exactly 86_400 seconds, an instant
    // just before it stays on the previous day, and a pre-epoch instant floors DOWN rather than
    // toward zero. MUTATION: round toward zero instead of flooring and the `-1` assert reds. This
    // function's input is already a plain UTC timestamp, so it cannot see a local-vs-UTC bug in the
    // CLOCK; `the_day_key_is_the_utc_day_not_the_callers_zone` pins that.
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

    // The day key is the ABSOLUTE instant's UTC day, never the wall-clock date the caller's zone
    // shows. At 2024-01-01T10:30:00Z a +14 zone (Pacific/Kiritimati) is already 2024-01-02 while a
    // -11 zone (Pacific/Midway) is still 2023-12-31, so BOTH differ from the UTC day — and neither
    // depends on the host's ambient timezone, because the zone rides on the injected instant. This is
    // the STUDIO-979 guard for the ticket's "use local midnight" mutation: derive the key from the
    // injected zone's LOCAL date (`now.naive_local().and_utc()`) and the +14 arm reds; read
    // `chrono::Local::now()` and BOTH arms red (the real system day is not 2024-01-01).
    #[test]
    fn the_day_key_is_the_utc_day_not_the_callers_zone() {
        use chrono::{NaiveDate, Utc};

        // 2024-01-01T10:30:00Z.
        let instant = DateTime::<Utc>::from_timestamp(1_704_105_000, 0).expect("valid instant");
        let utc_day = utc_day_of(instant.timestamp());

        let east = FixedOffset::east_opt(14 * 3_600).expect("valid +14");
        let west = FixedOffset::west_opt(11 * 3_600).expect("valid -11");
        // Sanity: the same instant really is three different calendar days in these zones.
        assert_eq!(
            instant.with_timezone(&east).date_naive(),
            NaiveDate::from_ymd_opt(2024, 1, 2).expect("date"),
            "the +14 zone is already tomorrow"
        );
        assert_eq!(
            instant.with_timezone(&west).date_naive(),
            NaiveDate::from_ymd_opt(2023, 12, 31).expect("date"),
            "the -11 zone is still yesterday"
        );

        let dir = scratch_dir();
        let store: Arc<dyn Store + Send + Sync> = Arc::new(disk_store(&dir));
        assert_eq!(
            StoreDayAuthority::with_instant(Arc::clone(&store), true, instant.with_timezone(&east))
                .today(),
            utc_day,
            "a +14 zone is already tomorrow, but the key must be the UTC day"
        );
        assert_eq!(
            StoreDayAuthority::with_instant(Arc::clone(&store), true, instant.with_timezone(&west))
                .today(),
            utc_day,
            "a -11 zone is still yesterday, but the key must be the UTC day"
        );
        assert_eq!(
            StoreDayAuthority::with_instant(store, true, instant.fixed_offset()).today(),
            utc_day,
            "the UTC view of the same instant agrees"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
