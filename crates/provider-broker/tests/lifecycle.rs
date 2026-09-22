//! Lifecycle acceptance tests for PB1, driven entirely through the public API with an injected
//! [`ManualClock`] and [`ScriptedRandom`]. These pin the binding acceptance bullets: 256-bit
//! digest-only capabilities, one live turn per session, capacity-one receipts, deterministic
//! expiry/drop/finish, and indistinguishable auth failures.

use std::sync::Arc;

use rhapsody_provider_broker::{
    BoundCredentialLease, Broker, BrokerError, BrokerProtocol, BrokerRegistration,
    BrokerRegistrationPlan, Clock, CredentialBinding, DEFAULT_BROKER_LIMITS, ManualClock,
    ScriptedRandom, SessionPolicy, TurnMeta, TurnOutcome,
};

const BASE_URL: &str = "http://127.0.0.1:41234/v1";
const ENDPOINT: &str = "https://api.example.com/v1";

fn plan(provider: &str) -> BrokerRegistrationPlan {
    BrokerRegistrationPlan::new(
        provider,
        BrokerProtocol::OpenAiChatCompletions,
        ENDPOINT,
        false,
        "model-x",
        DEFAULT_BROKER_LIMITS,
    )
    .expect("plan")
}

fn lease(provider: &str) -> BoundCredentialLease {
    let binding = CredentialBinding::new(provider, BrokerProtocol::OpenAiChatCompletions, ENDPOINT)
        .expect("binding");
    BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease")
}

fn broker_with(clock: Arc<ManualClock>, rng: Arc<ScriptedRandom>) -> Broker {
    Broker::new(BASE_URL, clock, rng).expect("broker")
}

fn register(broker: &Broker, provider: &str) -> BrokerRegistration {
    broker
        .register_session(plan(provider), lease(provider), SessionPolicy::default())
        .expect("registration")
}

fn mint(broker: &Broker) -> (BrokerRegistration, String) {
    let mut registration = register(broker, "provider-a");
    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    access.finish();
    assert!(
        receipt.is_finalized(),
        "finish must finalize the receipt synchronously"
    );
    (registration, token)
}

#[test]
fn a_minted_token_is_43_chars_and_resolves_only_its_own_grant() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    assert_eq!(access.base_url, BASE_URL);
    assert_eq!(access.turn_ordinal(), 1);

    let token = access.api_key.expose_for_child(str::to_owned);
    assert_eq!(token.len(), 43, "token is 43 unpadded base64url characters");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );

    let grant = broker.lookup_capability(&token).expect("lookup");
    assert_eq!(grant.model_id(), "model-x");
    assert_eq!(grant.stable_provider_id(), "provider-a");
    assert_eq!(grant.normalized_endpoint(), ENDPOINT);
    assert_eq!(grant.protocol(), BrokerProtocol::OpenAiChatCompletions);
    assert_eq!(grant.turn_ordinal(), 1);
    assert!(!grant.is_revoked());

    access.finish();
    let ledger = receipt.take().expect("finalized on finish");
    assert_eq!(ledger.outcome(), TurnOutcome::Completed);
    assert!(ledger.capability_issued());
    assert_eq!(ledger.turn_ordinal(), 1);

    // A finished capability is immediately dead.
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );
}

#[test]
fn consecutive_turns_use_distinct_tokens_and_one_live_turn_at_a_time() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt1, receipt1) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 1");
    let access1 = attempt1.mint_access().expect("mint 1");
    let token1 = access1.api_key.expose_for_child(str::to_owned);

    // While turn 1 is live, arming turn 2 is refused (capacity-one receipt).
    assert_eq!(
        registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .unwrap_err(),
        BrokerError::TurnAlreadyArmed
    );

    access1.finish();
    let _ = receipt1.take().expect("receipt 1");

    let (attempt2, receipt2) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 2");
    let access2 = attempt2.mint_access().expect("mint 2");
    let token2 = access2.api_key.expose_for_child(str::to_owned);
    assert_ne!(token1, token2, "each outer turn gets a fresh capability");
    assert_eq!(access2.turn_ordinal(), 2);

    assert_eq!(
        broker.lookup_capability(&token1).unwrap_err(),
        BrokerError::Unauthorized
    );
    assert!(broker.lookup_capability(&token2).is_ok());

    access2.finish();
    let ledger2 = receipt2.take().expect("receipt 2");
    assert_eq!(ledger2.turn_ordinal(), 2);
}

#[test]
fn a_dropped_attempt_finalizes_a_no_capability_receipt() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    drop(attempt);

    assert!(
        receipt.is_finalized(),
        "attempt drop finalizes synchronously"
    );
    let ledger = receipt.take().expect("ledger");
    assert_eq!(ledger.outcome(), TurnOutcome::NoCapability);
    assert!(!ledger.capability_issued());

    // The slot is drained, so a new turn may arm.
    let (attempt2, receipt2) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("re-arm after drain");
    drop(attempt2);
    drop(receipt2);
}

#[test]
fn take_before_finalize_returns_none_and_never_loses_the_slot() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    assert!(receipt.take().is_none(), "not finalized yet");
    assert_eq!(
        registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .unwrap_err(),
        BrokerError::TurnAlreadyArmed,
        "an undrained receipt blocks the next arm"
    );

    drop(attempt);
    let ledger = receipt.take().expect("finalized after attempt drop");
    assert!(!ledger.capability_issued());
}

#[test]
fn a_dropped_access_revokes_and_finalizes_with_revoked() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    assert!(broker.lookup_capability(&token).is_ok());

    drop(access);
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );
    let ledger = receipt.take().expect("finalized on drop");
    assert_eq!(ledger.outcome(), TurnOutcome::Revoked);
    assert!(ledger.capability_issued());
}

#[test]
fn a_dropped_receipt_revokes_the_live_turn_and_frees_the_slot_but_not_the_turn_gate() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    assert!(broker.lookup_capability(&token).is_ok());

    drop(receipt);
    // Dropping the receipt is a caller bug: it revokes the turn so the capability cannot keep
    // spending unaccounted.
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );

    // The receipt is gone, but the live access still holds the session's turn gate.
    assert_eq!(
        registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .unwrap_err(),
        BrokerError::TurnAlreadyActive
    );
    drop(access);
    registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm after access drop");
}

#[test]
fn a_stale_receipt_cannot_steal_a_later_turns_ledger() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt1, receipt1) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 1");
    attempt1.mint_access().expect("mint 1").finish();
    let ledger1 = receipt1.take().expect("turn 1 ledger");
    assert_eq!(ledger1.turn_ordinal(), 1);

    // Turn 1's receipt object is still alive, as a worker loop that holds it to end of scope would.
    let (attempt2, receipt2) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 2");
    attempt2.mint_access().expect("mint 2").finish();

    assert!(
        receipt1.take().is_none(),
        "a stale receipt must not hand back turn 2's ledger"
    );
    let ledger2 = receipt2
        .take()
        .expect("turn 2 ledger must still be drainable");
    assert_eq!(ledger2.turn_ordinal(), 2);
    assert_eq!(ledger2.outcome(), TurnOutcome::Completed);
}

#[test]
fn a_stale_receipt_drop_does_not_erase_a_later_turns_ledger() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt1, receipt1) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 1");
    attempt1.mint_access().expect("mint 1").finish();
    let _ = receipt1.take().expect("turn 1 ledger");

    let (attempt2, receipt2) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm 2");
    attempt2.mint_access().expect("mint 2").finish();
    assert!(receipt2.is_finalized());

    // The stale turn-1 receipt drops after turn 2 finalized; it must not drain turn 2's ledger.
    drop(receipt1);
    let ledger2 = receipt2
        .take()
        .expect("turn 2 ledger survives a stale drop");
    assert_eq!(ledger2.turn_ordinal(), 2);
}

#[test]
fn dropping_the_receipt_before_mint_revokes_the_turn_and_refuses_mint() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    drop(receipt);

    // Cancellation-before-mint leaves no live capability: the mint fails closed.
    assert_eq!(attempt.mint_access().unwrap_err(), BrokerError::TurnRevoked);

    // The slot was drained and the gate released, so the next turn may arm.
    let (attempt2, receipt2) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm after revoked turn");
    let access2 = attempt2
        .mint_access()
        .expect("the next turn mints normally");
    access2.finish();
    let ledger2 = receipt2.take().expect("ledger");
    assert_eq!(ledger2.turn_ordinal(), 2);
    assert!(ledger2.capability_issued());
}

#[test]
fn dropping_the_receipt_after_mint_revokes_the_live_capability() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    let grant = broker.lookup_capability(&token).expect("live");

    drop(receipt);

    // The live grant is revoked: it leaves the registry and refuses all further spend.
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );
    assert!(grant.is_revoked());
    assert_eq!(
        grant.reserve_request(1, 1, 1).unwrap_err(),
        BrokerError::Unauthorized
    );
    drop(access);
}

#[test]
fn session_revoke_invalidates_live_turns_refuses_new_turns_and_releases_custody() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    assert!(registration.session.has_custody());

    registration.session.revoke();
    assert!(registration.session.is_revoked());
    assert!(
        !registration.session.has_custody(),
        "revocation releases the credential lease"
    );
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );

    drop(access);
    let ledger = receipt.take().expect("finalized");
    assert_eq!(ledger.outcome(), TurnOutcome::Revoked);

    assert_eq!(
        registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .unwrap_err(),
        BrokerError::SessionRevoked
    );
}

#[test]
fn expiry_is_deterministic_across_lookup_and_drop() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);

    // Default capability lifetime is one hour; a turn deadline shortens it and is never extended.
    clock.advance(std::time::Duration::from_secs(2 * 60 * 60));
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );
    drop(access);
    let ledger = receipt.take().expect("finalized");
    assert_eq!(ledger.outcome(), TurnOutcome::Expired);
}

#[test]
fn a_turn_deadline_shorter_than_the_maximum_lifetime_governs() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let deadline = clock
        .now()
        .saturating_add(std::time::Duration::from_secs(30));
    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::new(Some(deadline)))
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    assert_eq!(access.not_after(), deadline);
    assert!(broker.lookup_capability(&token).is_ok());

    clock.advance(std::time::Duration::from_secs(31));
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );
    drop(access);
    assert_eq!(
        receipt.take().expect("ledger").outcome(),
        TurnOutcome::Expired
    );
}

#[test]
fn a_binding_mismatch_refuses_registration() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));

    let result = broker.register_session(
        plan("provider-a"),
        lease("provider-b"),
        SessionPolicy::default(),
    );
    assert_eq!(result.unwrap_err(), BrokerError::BindingMismatch);
}

#[test]
fn a_session_id_collision_fails_closed_without_aliasing_the_existing_session() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new().with_fallback(false));
    rng.push_bytes(vec![9u8; 16]);
    rng.push_bytes(vec![9u8; 16]);
    rng.push_bytes(vec![3u8; 32]); // the surviving session's first token
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));

    let mut first = register(&broker, "provider-a");
    let second = broker.register_session(
        plan("provider-a"),
        lease("provider-a"),
        SessionPolicy::default(),
    );
    assert_eq!(second.unwrap_err(), BrokerError::SessionIdCollision);

    // The original session is untouched.
    let (attempt, _receipt) = first
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("first session still works");
    attempt.mint_access().expect("mint");
}

#[test]
fn a_token_digest_collision_retries_then_mints_a_distinct_capability() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new().with_fallback(false));
    let candidate = vec![7u8; 32];
    rng.push_bytes(vec![1u8; 16]); // session id A
    rng.push_bytes(candidate.clone()); // first token in A
    rng.push_bytes(vec![2u8; 16]); // session id B
    rng.push_bytes(candidate.clone()); // collides with A's digest
    rng.push_bytes(vec![8u8; 32]); // retry succeeds
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));

    let mut a = register(&broker, "provider-a");
    let (attempt_a, _receipt_a) = a
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm a");
    let access_a = attempt_a.mint_access().expect("mint a");
    let token_a = access_a.api_key.expose_for_child(str::to_owned);

    let mut b = register(&broker, "provider-a");
    let (attempt_b, receipt_b) = b
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm b");
    let access_b = attempt_b.mint_access().expect("retry must succeed");
    let token_b = access_b.api_key.expose_for_child(str::to_owned);

    assert_ne!(token_a, token_b);
    assert!(broker.lookup_capability(&token_a).is_ok());
    assert!(broker.lookup_capability(&token_b).is_ok());
    assert!(receipt_b.take().is_none(), "still live");
}

#[test]
fn collision_exhaustion_fails_closed_with_a_no_capability_receipt() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new().with_fallback(false));
    let candidate = vec![7u8; 32];
    rng.push_bytes(vec![1u8; 16]);
    rng.push_bytes(candidate.clone());
    rng.push_bytes(vec![2u8; 16]);
    for _ in 0..8 {
        rng.push_bytes(candidate.clone());
    }
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));

    let mut a = register(&broker, "provider-a");
    // Keep session A's turn live (named binding) so its digest still occupies the registry.
    let (attempt_a, _receipt_a) = a
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm a");
    let access_a = attempt_a.mint_access().expect("mint a");
    let token_a = access_a.api_key.expose_for_child(str::to_owned);
    assert!(broker.lookup_capability(&token_a).is_ok());

    let mut b = register(&broker, "provider-a");
    let (attempt_b, receipt_b) = b
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm b");
    assert_eq!(
        attempt_b.mint_access().unwrap_err(),
        BrokerError::TokenCollisionExhausted
    );
    let ledger = receipt_b.take().expect("finalized");
    assert_eq!(ledger.outcome(), TurnOutcome::NoCapability);
    assert!(!ledger.capability_issued());
}

#[test]
fn random_source_failure_fails_closed_with_a_no_capability_receipt() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new().with_fallback(false));
    rng.push_bytes(vec![1u8; 16]);
    rng.push_failure();
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));

    let mut registration = register(&broker, "provider-a");
    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    assert_eq!(
        attempt.mint_access().unwrap_err(),
        BrokerError::RandomSourceFailure
    );
    let ledger = receipt.take().expect("finalized");
    assert_eq!(ledger.outcome(), TurnOutcome::NoCapability);
    assert!(!ledger.capability_issued());
}

#[test]
fn every_authentication_failure_is_indistinguishable() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let (_registration, token) = mint(&broker);

    let malformed_short = "A".repeat(42);
    let malformed_alphabet = format!("{}+", "A".repeat(42));
    let unknown = "A".repeat(43);
    for (name, value) in [
        ("malformed short", malformed_short),
        ("malformed alphabet", malformed_alphabet),
        ("unknown", unknown),
    ] {
        assert_eq!(
            broker.lookup_capability(&value).unwrap_err(),
            BrokerError::Unauthorized,
            "{name} must be indistinguishable"
        );
    }
    // The real token is dead too now that its turn finished, and still returns the same error.
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized
    );

    // Expired and revoked also collapse to `Unauthorized`.
    let mut expired_registration = register(&broker, "provider-a");
    let (attempt, _receipt) = expired_registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let expired_token = access.api_key.expose_for_child(str::to_owned);
    clock.advance(std::time::Duration::from_secs(2 * 60 * 60));
    assert_eq!(
        broker.lookup_capability(&expired_token).unwrap_err(),
        BrokerError::Unauthorized
    );
    let _ = access;
}

#[test]
fn ledger_reconciliation_take_is_idempotent() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    drop(attempt);
    assert!(receipt.take().is_some());
    assert!(receipt.take().is_none(), "a drained receipt is empty");
}

#[test]
fn reservations_via_a_grant_are_bounded() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, _receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    let grant = broker.lookup_capability(&token).expect("lookup");

    assert!(grant.reserve_request(1_000, 1_000, 1_000).is_ok());
    let permit = grant.acquire_concurrency().expect("concurrency permit");
    drop(permit);
    assert!(grant.record_denied().is_ok());
    // A request larger than the turn's per-request byte cap is refused.
    let over = DEFAULT_BROKER_LIMITS.max_request_bytes + 1;
    assert!(grant.reserve_request(over, 0, 0).is_err());
}

#[test]
fn secret_debug_outputs_never_carry_the_capability() {
    let clock = Arc::new(ManualClock::new());
    let rng = Arc::new(ScriptedRandom::new());
    let broker = broker_with(Arc::clone(&clock), Arc::clone(&rng));
    let mut registration = register(&broker, "provider-a");

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    let token_debug = format!("{:?}", access.api_key);

    assert!(!token_debug.contains(&token));
    assert!(!format!("{access:?}").contains(&token));
    assert!(!format!("{registration:?}").contains(&token));
    assert!(!format!("{receipt:?}").contains(&token));
    assert!(!format!("{:?}", registration.session).contains(&token));
}
