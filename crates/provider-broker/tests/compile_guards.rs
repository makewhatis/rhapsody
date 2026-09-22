//! Compile-time and API guards for the secret-bearing types (PB1 "Required verification").
//!
//! The negative-impl assertions fail to compile if a future change adds `Clone` or `Serialize` to a
//! move-only custody handle, or `Display` to the capability token. This is the mutation guard from
//! the ticket: "secret types are non-serializing, redacting, and non-`Clone`".

use static_assertions::assert_not_impl_any;

use rhapsody_provider_broker::{
    BoundCredentialLease, BrokerLedgerReceiver, BrokerRegistration, BrokerSession,
    BrokerTurnAttempt, CapabilityToken, TurnAccess, TurnReceipt,
};

assert_not_impl_any!(CapabilityToken: Clone, serde::Serialize, std::fmt::Display);
assert_not_impl_any!(BoundCredentialLease: Clone, serde::Serialize);
assert_not_impl_any!(BrokerSession: Clone);
assert_not_impl_any!(BrokerLedgerReceiver: Clone);
assert_not_impl_any!(BrokerTurnAttempt: Clone);
assert_not_impl_any!(TurnAccess: Clone);
assert_not_impl_any!(TurnReceipt: Clone);
assert_not_impl_any!(BrokerRegistration: Clone);

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn custody_handles_are_thread_safe() {
    assert_send::<CapabilityToken>();
    assert_send::<BoundCredentialLease>();
    assert_send::<BrokerSession>();
    assert_send::<BrokerLedgerReceiver>();
    assert_send::<BrokerTurnAttempt>();
    assert_send::<TurnAccess>();
    assert_send::<TurnReceipt>();

    assert_sync::<CapabilityToken>();
    assert_sync::<BrokerSession>();
    assert_sync::<BrokerLedgerReceiver>();
}
