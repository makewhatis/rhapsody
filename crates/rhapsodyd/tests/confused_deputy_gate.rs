//! STUDIO-1003 (PB8) — the deterministic half of the confused-deputy gate.
//!
//! Design §1.2/§17: the harness can execute the same signed sidecar binary, so a binary ACL is not
//! enough. A `rhapsodyd` that was NOT launched by the desktop supervisor never receives the one-shot
//! bootstrap frame, and without it a provider read must stay `owner_unavailable` and broker
//! registration must remain impossible — so a confused-deputy child cannot initialize another
//! credential owner or mint grants. P0c's packaged measurement covers the signing/ACL half
//! (`desktop/src-tauri/tests/credential_bootstrap_e2e.rs`, gated behind `RHAPSODY_CREDENTIAL_BOOTSTRAP_E2E=1`)
//! and the structural half is pinned by `tests/no_direct_keychain_dependency.rs`; this file pins the
//! daemon-side consequence DETERMINISTICALLY (no packaging, no Keychain): a no-owner launch refuses
//! and mints zero broker sessions.
//!
//! MUTATION GUARD: make `DaemonProviderSource::open_provider` invent a lease for an owner that never
//! answered, or let `CredentialResolver::new` (the no-bootstrap state) resolve `Present`, and the
//! refusal assertion (and the zero-session assertion) red.

use rhapsody_agent::{ProviderLimits, ProviderOrigins, ProviderProtocol, ResolvedProviderPlan};
use rhapsody_orchestrator::{PreparedProviderSource, RefusalReason};
use rhapsodyd::broker::BrokerRuntime;
use rhapsodyd::providers::{DaemonProviderSource, unavailable_owner};

/// The plan a workflow with one `openai-compatible` provider resolves to. Only the fields the source
/// reads matter; the credential binding is derived from the endpoint.
fn plan() -> ResolvedProviderPlan {
    ResolvedProviderPlan {
        stable_id: "fireworks".to_string(),
        protocol: ProviderProtocol::OpenAiCompatible,
        normalized_endpoint: "https://fireworks.example/v1".to_string(),
        allow_insecure_http: false,
        credential_binding: String::new(),
        credential_ref: "keychain".to_string(),
        limits: ProviderLimits::default(),
        model: "m".to_string(),
        origins: ProviderOrigins::default(),
    }
}

/// `unavailable_owner()` is exactly the resolver a direct invocation (or a confused-deputy process
/// that `exec`s the signed sidecar) has: no bootstrap frame was ever read. The real broker is bound
/// and available, so a refusal here can only be owner-derived — and no session may be minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_no_owner_launch_refuses_and_mints_no_broker_session() {
    let runtime = BrokerRuntime::bind().expect("bind the loopback broker");
    let broker = runtime.broker_handle();
    assert_eq!(
        broker.live_session_count(),
        0,
        "a fresh broker holds no session"
    );

    let source = DaemonProviderSource::new(unavailable_owner(), runtime.registrar());
    let err = source
        .open_provider(&plan())
        .await
        .expect_err("a no-owner launch must refuse, never fall back to a direct key");

    assert_eq!(err.reason, RefusalReason::OwnerUnavailable);
    assert!(
        err.revision.is_empty(),
        "a read that never reached an owner carries no owner revision: {:?}",
        err.revision
    );
    assert!(
        runtime.registrar().is_available(),
        "the broker is up, so the refusal is owner-derived, not a down-broker refusal"
    );
    assert_eq!(
        broker.live_session_count(),
        0,
        "a confused-deputy/no-owner launch must mint no broker session"
    );
}
