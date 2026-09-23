//! PB4 acceptance: the daemon broker lifecycle handle (design §11, slice PB4).
//!
//! These pin the provider-broker half of the acceptance contract: one IPv4 loopback ephemeral
//! listener; an explicit bind failure; the cloneable registration handle; and the atomic
//! unavailable/revoke-all transition a failed serving task triggers. The daemon's own wiring
//! (supervision, shutdown ordering, no publication) is pinned in `crates/rhapsodyd`.

#![cfg(feature = "loopback")]

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use rhapsody_provider_broker::{
    BoundCredentialLease, Broker, BrokerError, BrokerListener, BrokerProtocol, BrokerRegistrar,
    BrokerRegistration, BrokerRegistrationPlan, CredentialBinding, DEFAULT_BROKER_LIMITS,
    ManualClock, OsRandom, ScriptedRandom, SessionPolicy, SystemClock, TurnMeta,
};

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

fn test_broker() -> Broker {
    Broker::new(
        "http://127.0.0.1:41234/v1",
        Arc::new(ManualClock::new()),
        Arc::new(ScriptedRandom::new()),
    )
    .expect("broker")
}

fn register(broker: &Broker, provider: &str) -> BrokerRegistration {
    broker
        .register_session(plan(provider), lease(provider), SessionPolicy::default())
        .expect("registration")
}

/// Mint a live capability and return its encoded bearer value plus the still-live access and
/// receipt handles. Both must be kept alive: dropping either revokes the grant (design §3.2), so
/// the caller holds them to observe the revocation.
fn mint_live(
    broker: &Broker,
) -> (
    BrokerRegistration,
    String,
    rhapsody_provider_broker::TurnAccess,
    rhapsody_provider_broker::TurnReceipt,
) {
    let mut registration = register(broker, "provider-a");
    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    (registration, token, access, receipt)
}

#[tokio::test]
async fn bind_at_assigns_an_ipv4_loopback_ephemeral_port() {
    let (listener, _broker) = BrokerListener::bind_at(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom::new()),
    )
    .expect("bind");
    let addr = listener.local_addr();
    assert!(
        addr.ip().is_loopback(),
        "listener must bind loopback: {addr}"
    );
    assert!(addr.is_ipv4(), "listener must bind IPv4: {addr}");
    assert_ne!(addr.port(), 0, "an ephemeral bind must resolve a real port");
}

#[tokio::test]
async fn bind_at_refuses_a_non_loopback_address() {
    // The one private listener can never be pointed at a routable interface.
    let result = BrokerListener::bind_at(
        SocketAddr::from(([0, 0, 0, 0], 0)),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom::new()),
    );
    assert!(result.is_err(), "a non-loopback bind must be refused");
}

#[tokio::test]
async fn bind_at_a_taken_port_is_an_explicit_error() {
    // A *real* bind failure (EADDRINUSE), with no broker created and no fallback.
    let occupied = TcpListener::bind(("127.0.0.1", 0)).expect("occupy a loopback port");
    let port = occupied.local_addr().expect("addr").port();
    let result = BrokerListener::bind_at(
        SocketAddr::from(([127, 0, 0, 1], port)),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom::new()),
    );
    assert!(
        result.is_err(),
        "binding an occupied loopback port must be an explicit error"
    );
}

#[tokio::test]
async fn listener_debug_redacts_the_address() {
    let (listener, _broker) = BrokerListener::bind_at(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom::new()),
    )
    .expect("bind");
    let port = listener.local_addr().port().to_string();
    let rendered = format!("{listener:?}");
    assert!(
        !rendered.contains(&port),
        "the listener Debug must not publish its port: {rendered}"
    );
}

#[test]
fn a_registrar_refuses_and_revokes_once_the_broker_is_unavailable() {
    let broker = test_broker();
    let registrar: BrokerRegistrar = broker.registrar();
    assert!(registrar.is_available());

    let (_registration, token, _access, _receipt) = mint_live(&broker);
    assert!(
        broker.lookup_capability(&token).is_ok(),
        "the minted capability must be live before the failure"
    );

    broker.mark_unavailable();

    assert!(
        !broker.is_available(),
        "the broker must be marked unavailable"
    );
    assert!(!registrar.is_available(), "the registrar shares the state");
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized,
        "an unexpected serve failure must revoke the live grant"
    );
    // MUTATION GUARD: a registrar that skipped the availability check would register here.
    assert_eq!(
        registrar
            .register_session(
                plan("provider-b"),
                lease("provider-b"),
                SessionPolicy::default()
            )
            .unwrap_err(),
        BrokerError::Unavailable,
        "preparation must refuse with the typed unavailable error, with no direct-key fallback"
    );
}

#[test]
fn revoke_all_revokes_a_live_grant_and_session() {
    let broker = test_broker();
    let (registration, token, _access, _receipt) = mint_live(&broker);
    assert!(broker.lookup_capability(&token).is_ok());

    broker.revoke_all();

    assert!(
        registration.session.is_revoked(),
        "revoke_all must revoke the registered session"
    );
    assert_eq!(
        broker.lookup_capability(&token).unwrap_err(),
        BrokerError::Unauthorized,
        "revoke_all must remove the live grant from the registry"
    );
}

#[test]
fn revoke_all_is_idempotent_on_an_empty_registry() {
    let broker = test_broker();
    broker.revoke_all();
    broker.revoke_all();
    assert!(
        broker.is_available(),
        "a clean revoke does not mark unavailable"
    );
}
