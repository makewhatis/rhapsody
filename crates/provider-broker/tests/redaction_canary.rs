//! STUDIO-1003 (PB8) — a secret canary over every redacting surface the broker exposes.
//!
//! Design §13/§14.1: no capability, key, or key digest may appear through `Debug`, `Display`,
//! serialization, logs, errors, or metrics. The type-level guarantees are pinned separately
//! (`compile_guards.rs`'s `assert_not_impl_any!`, the unit redaction tests, `secret.rs`). This file
//! is the RECURSIVE canary the ticket names: it plants a canary key, mints a real capability, and
//! renders every `Debug`/`Display` surface a caller can reach, asserting none carries either.
//!
//! MUTATION GUARD: replace any redacting `Debug` impl (on the lease, fingerprint, session, access,
//! grant, ledger, or the registrar) with `#[derive(Debug)]`, or give an error variant a wrapped
//! lower-level error carrying the value, and the assertion for that surface reds.

use std::sync::Arc;

use rhapsody_provider_broker::{
    BoundCredentialLease, Broker, BrokerError, BrokerProtocol, BrokerRegistrationPlan,
    CredentialRejection, DEFAULT_BROKER_LIMITS, LimitViolation, ManualClock, ScriptedRandom,
    SessionPolicy, TurnMeta,
};

/// A distinctive key no other fixture shares; the fragment lets a render that leaked only a prefix
/// still be caught.
const CANARY_KEY: &str = "sk-CANARY-UPSTREAM-KEY-0123456789abcdef";
const CANARY_FRAGMENT: &str = "CANARY-UPSTREAM-KEY";
const ENDPOINT: &str = "https://api.example.com/v1";

/// Assert `rendered` carries none of the needles (empty needles — "no capability here" — are skipped,
/// since an empty string is a substring of everything).
fn assert_clean(label: &str, rendered: &str, needles: &[&str]) {
    for needle in needles.iter().filter(|needle| !needle.is_empty()) {
        assert!(
            !rendered.contains(needle),
            "{label} leaked {needle:?}: {rendered}"
        );
    }
}

#[test]
fn no_redacting_surface_carries_the_key_or_capability() {
    let broker = Broker::new(
        "http://127.0.0.1:41234/v1",
        Arc::new(ManualClock::new()),
        Arc::new(ScriptedRandom::new()),
    )
    .expect("broker");
    let registrar = broker.registrar();
    let plan = BrokerRegistrationPlan::new(
        "provider-a",
        BrokerProtocol::OpenAiChatCompletions,
        ENDPOINT,
        false,
        "model-x",
        DEFAULT_BROKER_LIMITS,
    )
    .expect("plan");
    let binding = plan.binding().expect("binding");

    // The fingerprint/binding/lease are the only pre-registration surfaces that hold the key.
    assert_clean(
        "BindingFingerprint Debug",
        &format!("{:?}", binding.fingerprint()),
        &[CANARY_FRAGMENT],
    );
    assert_clean(
        "CredentialBinding Debug",
        &format!("{binding:?}"),
        &[CANARY_FRAGMENT],
    );

    let lease =
        BoundCredentialLease::new(binding.clone(), CANARY_KEY.as_bytes().to_vec()).expect("lease");
    assert_clean(
        "BoundCredentialLease Debug",
        &format!("{lease:?}"),
        &[CANARY_FRAGMENT],
    );

    let mut registration = broker
        .register_session(plan, lease, SessionPolicy::default())
        .expect("register");
    assert_clean(
        "BrokerRegistrar Debug",
        &format!("{registrar:?}"),
        &[CANARY_FRAGMENT],
    );
    assert_clean(
        "BrokerSession Debug",
        &format!("{:?}", registration.session),
        &[CANARY_FRAGMENT],
    );
    assert_clean(
        "BrokerLedgerReceiver Debug",
        &format!("{:?}", registration.ledgers),
        &[CANARY_FRAGMENT],
    );

    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    assert_clean(
        "BrokerTurnAttempt Debug",
        &format!("{attempt:?}"),
        &[CANARY_FRAGMENT],
    );
    let access = attempt.mint_access().expect("mint");
    let capability = access.api_key.expose_for_child(str::to_owned);
    assert!(!capability.is_empty(), "a minted capability is non-empty");

    assert_clean("TurnAccess Debug", &format!("{access:?}"), &[&capability]);
    assert_clean(
        "CapabilityGrant Debug",
        &format!(
            "{:?}",
            broker.lookup_capability(&capability).expect("grant")
        ),
        &[&capability],
    );

    access.finish();
    let ledger = receipt.take().expect("finalized ledger");
    assert_clean("TurnLedger Debug", &format!("{ledger:?}"), &[&capability]);
    assert_clean(
        "BrokerMetricsSnapshot Debug",
        &format!("{:?}", broker.metrics().snapshot()),
        &[CANARY_FRAGMENT, &capability],
    );

    // Errors are closed values carrying no wrapped lower-level error; render every reachable one.
    for err in [
        BrokerError::BindingMismatch,
        BrokerError::Unavailable,
        BrokerError::Unauthorized,
        BrokerError::InvalidCredential(CredentialRejection::InvalidShape),
        BrokerError::InvalidLimits(LimitViolation::Zero("max_forwarded_requests")),
    ] {
        assert_clean(
            "BrokerError Debug",
            &format!("{err:?}"),
            &[CANARY_FRAGMENT, &capability],
        );
        assert_clean(
            "BrokerError Display",
            &format!("{err}"),
            &[CANARY_FRAGMENT, &capability],
        );
    }
}
