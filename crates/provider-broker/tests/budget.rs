//! PB3 acceptance: the optional durable UTC-day budget authority and the atomic admission
//! transaction (design §8.1, §8.2).
//!
//! The authority is a *contract* this crate only calls; the store-backed implementation belongs to
//! a later slice. These tests inject a fake authority that owns a controllable day and enforces its
//! cap atomically, and prove the broker charges it once per admission (never in a read-then-write
//! window), refuses before egress when it is exhausted, and resets at a UTC boundary.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use rhapsody_provider_broker::{
    BoundCredentialLease, Broker, BrokerError, BrokerLimits, BrokerProtocol,
    BrokerRegistrationPlan, CredentialBinding, CumulativeBudgetAuthority, DayBudgetRefusal,
    ManualClock, ScriptedRandom, SessionPolicy, TurnMeta,
};

const BASE_URL: &str = "http://127.0.0.1:41234/v1";
const ENDPOINT: &str = "https://api.example.com/v1";
const PROVIDER: &str = "provider-a";

/// A durable-authority stand-in: a day-bucketed counter behind one mutex, so a concurrent charge is
/// atomic exactly as the real store-backed authority must be.
#[derive(Debug)]
struct FakeDayAuthority {
    charged: Mutex<HashMap<(String, i64), u64>>,
    day: AtomicI64,
    charge_calls: AtomicU64,
}

impl FakeDayAuthority {
    fn new(day: i64) -> Arc<Self> {
        Arc::new(Self {
            charged: Mutex::new(HashMap::new()),
            day: AtomicI64::new(day),
            charge_calls: AtomicU64::new(0),
        })
    }

    fn set_day(&self, day: i64) {
        self.day.store(day, Ordering::Release);
    }

    fn charge_calls(&self) -> u64 {
        self.charge_calls.load(Ordering::Acquire)
    }
}

impl CumulativeBudgetAuthority for FakeDayAuthority {
    fn try_charge(&self, provider_id: &str, tokens: u64, cap: u64) -> Result<(), DayBudgetRefusal> {
        self.charge_calls.fetch_add(1, Ordering::AcqRel);
        let day = self.day.load(Ordering::Acquire);
        let mut map = self.charged.lock().expect("authority lock");
        let entry = map.entry((provider_id.to_owned(), day)).or_insert(0);
        let next = entry
            .checked_add(tokens)
            .ok_or(DayBudgetRefusal::Exhausted)?;
        if next > cap {
            return Err(DayBudgetRefusal::Exhausted);
        }
        *entry = next;
        Ok(())
    }

    fn charged_today(&self, provider_id: &str) -> u64 {
        let day = self.day.load(Ordering::Acquire);
        *self
            .charged
            .lock()
            .expect("authority lock")
            .get(&(provider_id.to_owned(), day))
            .unwrap_or(&0)
    }
}

fn plan(limits: BrokerLimits) -> BrokerRegistrationPlan {
    BrokerRegistrationPlan::new(
        PROVIDER,
        BrokerProtocol::OpenAiChatCompletions,
        ENDPOINT,
        false,
        "model-x",
        limits,
    )
    .expect("plan")
}

fn lease() -> BoundCredentialLease {
    let binding = CredentialBinding::new(PROVIDER, BrokerProtocol::OpenAiChatCompletions, ENDPOINT)
        .expect("binding");
    BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease")
}

fn broker() -> Broker {
    Broker::new(
        BASE_URL,
        Arc::new(ManualClock::new()),
        Arc::new(ScriptedRandom::new()),
    )
    .expect("broker")
}

fn limits_with_day_cap(cap: u64) -> BrokerLimits {
    BrokerLimits {
        max_reserved_token_units_per_utc_day: Some(cap),
        ..BrokerLimits::default()
    }
}

fn register(
    broker: &Broker,
    limits: BrokerLimits,
    authority: &Arc<FakeDayAuthority>,
) -> rhapsody_provider_broker::BrokerRegistration {
    let authority_obj: Arc<dyn CumulativeBudgetAuthority> = authority.clone();
    let policy = SessionPolicy::with_day_authority(limits, authority_obj).expect("policy");
    broker
        .register_session(plan(limits), lease(), policy)
        .expect("registration")
}

/// `arm_turn` refuses before child spawn when the durable day budget is already exhausted.
#[test]
fn arm_turn_refuses_when_the_day_budget_is_already_exhausted() {
    let authority = FakeDayAuthority::new(20_000);
    let limits = limits_with_day_cap(100);
    // Pre-charge the day to its cap, as a restarted process re-reading persisted state would see.
    authority
        .try_charge(PROVIDER, 100, 100)
        .expect("pre-charge to the cap");
    let broker = broker();
    let mut registration = register(&broker, limits, &authority);

    assert_eq!(
        registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .unwrap_err(),
        BrokerError::DayBudgetExhausted
    );
    // The abandoned arm released the capacity-one slot: draining/arming is not wedged.
    registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect_err("still exhausted");
}

/// Each request is charged against the day authority exactly once, with the cap enforced
/// atomically; a refusal returns before any construction and backs out the session reservation.
#[test]
fn the_day_cap_is_charged_per_request_and_enforced_atomically() {
    let authority = FakeDayAuthority::new(20_000);
    let limits = limits_with_day_cap(150);
    let broker = broker();
    let mut registration = register(&broker, limits, &authority);
    let (attempt, _receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    let grant = broker.lookup_capability(&token).expect("grant");

    assert_eq!(grant.remaining_day_tokens(), Some(150));

    // token_cost = request_bytes + output_tokens.
    grant
        .reserve_request(100, 0, 0)
        .expect("the first 100-unit reservation fits");
    assert_eq!(authority.charged_today(PROVIDER), 100);
    assert_eq!(grant.remaining_day_tokens(), Some(50));

    assert_eq!(
        grant.reserve_request(100, 0, 0).unwrap_err(),
        BrokerError::DayBudgetExhausted,
        "the second 100-unit reservation would exceed the 150 cap"
    );
    assert_eq!(
        authority.charged_today(PROVIDER),
        100,
        "a refused admission charges the day authority nothing"
    );

    grant.reserve_request(40, 0, 0).expect("40 more fits");
    assert_eq!(authority.charged_today(PROVIDER), 140);
    assert_eq!(
        grant.reserve_request(20, 0, 0).unwrap_err(),
        BrokerError::DayBudgetExhausted
    );

    access.finish();
}

/// Concurrent runs sharing one durable authority cannot oversubscribe it under a barrier race.
#[test]
fn concurrent_runs_cannot_oversubscribe_a_shared_day_authority() {
    let authority = FakeDayAuthority::new(20_000);
    let limits = limits_with_day_cap(1_000);
    let broker = broker();
    let cost = 100u64;
    let threads = 40usize;
    let barrier = Arc::new(Barrier::new(threads));
    let successes = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..threads {
        let authority = Arc::clone(&authority);
        let broker = broker.clone();
        let barrier = Arc::clone(&barrier);
        let successes = Arc::clone(&successes);
        handles.push(thread::spawn(move || {
            let mut registration = register(&broker, limits, &authority);
            let (attempt, _receipt) = registration
                .ledgers
                .arm_turn(TurnMeta::without_deadline())
                .expect("arm");
            let access = attempt.mint_access().expect("mint");
            let token = access.api_key.expose_for_child(str::to_owned);
            let grant = broker.lookup_capability(&token).expect("grant");
            barrier.wait();
            if grant.reserve_request(cost, 0, 0).is_ok() {
                successes.fetch_add(1, Ordering::AcqRel);
            }
            access.finish();
        }));
    }
    for handle in handles {
        handle.join().expect("thread");
    }

    assert_eq!(successes.load(Ordering::Acquire), 10);
    assert_eq!(
        authority.charged_today(PROVIDER),
        1_000,
        "the shared cap is never oversubscribed"
    );
    assert_eq!(
        authority.charge_calls(),
        threads as u64,
        "every admission attempt went through the authority, so none could slip past a stale read"
    );
}

/// The authority is consulted per admission against durable state, so a fresh session sees the
/// budget already spent by an earlier run and a new UTC day resets it.
#[test]
fn a_spent_day_budget_survives_a_new_session_and_resets_at_a_utc_boundary() {
    let authority = FakeDayAuthority::new(20_000);
    let limits = limits_with_day_cap(100);
    let broker = broker();

    // Run 1 spends the day.
    {
        let mut registration = register(&broker, limits, &authority);
        let (attempt, _receipt) = registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .expect("arm");
        let access = attempt.mint_access().expect("mint");
        let token = access.api_key.expose_for_child(str::to_owned);
        let grant = broker.lookup_capability(&token).expect("grant");
        grant.reserve_request(100, 0, 0).expect("spends the day");
        access.finish();
        drop(registration);
    }

    // Run 2 (a restarted daemon would read the same durable state) is refused before child spawn.
    {
        let mut registration = register(&broker, limits, &authority);
        assert_eq!(
            registration
                .ledgers
                .arm_turn(TurnMeta::without_deadline())
                .unwrap_err(),
            BrokerError::DayBudgetExhausted
        );
    }

    // A new UTC day resets the bucket.
    authority.set_day(20_001);
    let mut registration = register(&broker, limits, &authority);
    registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("a new UTC day admits the next turn");
}
