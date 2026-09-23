//! The daemon's provider-broker lifecycle (STUDIO-999, PB4; design record
//! `~/.rhapsody/docs/provider-broker-design.md` §11). Rhapsody-only; no Go parity.
//!
//! `rhapsodyd` is the composition root: it binds the ONE private IPv4 loopback broker
//! (`127.0.0.1:0`) unconditionally at startup, injects its cloneable registration handle into the
//! orchestrator, serves it under a daemon-lifetime cancellation signal, and supervises the serving
//! task. An unexpected exit marks the broker unavailable and revokes every grant; a clean shutdown
//! revokes the remaining registry entries and drains the server under the daemon's bounded window.
//!
//! The broker's port is private implementation state. This module — and `run.rs` — never read
//! [`BrokerListener::local_addr`] on a production path, so the ephemeral port cannot reach
//! `runtime.json`, the banner, `/api/v1/version`, the dashboard, or the desktop proxy. The address
//! is reported only through the in-process [`BrokerListener`] handle itself.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::{AbortHandle, JoinHandle};

use rhapsody_orchestrator::CancelWait;
use rhapsody_provider_broker::{Broker, BrokerListener, BrokerRegistrar, OsRandom, SystemClock};

/// Test seam over the daemon's broker wiring (STUDIO-999, PB4). Production passes `None`; the
/// `run.rs` integration tests inject a `bind` that can fail and an `observe` that captures the live
/// handles, so the composition — bind → inject → serve → supervise → shutdown — is directly
/// exercised rather than only the pieces it calls.
pub(crate) struct BrokerSeam {
    /// How the composition root binds the broker. Production uses [`BrokerRuntime::bind`].
    pub bind: Box<dyn Fn() -> std::io::Result<BrokerRuntime> + Send + Sync>,
    /// Called once, immediately after the serving task is spawned. `None` in production.
    pub observe: Option<Box<dyn FnOnce(BrokerObservation) + Send>>,
}

/// The live handles a [`BrokerSeam`] observer receives once the broker is serving (STUDIO-999,
/// PB4): the shared broker, the create-only handle `run` injected into the orchestrator, and the
/// serving task's abort handle (so a test can inject an unexpected exit).
///
/// Only the integration tests read these fields, so the non-test lib target has no reader:
/// `allow(dead_code)` is scoped to exactly that build.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BrokerObservation {
    pub broker: Broker,
    pub registrar: BrokerRegistrar,
    pub serve_abort: AbortHandle,
}

/// The daemon-side composition of the one private broker: its shared handle, the cloneable
/// registration handle injected into preparation, and the bound listener until it is handed to the
/// serving task.
pub struct BrokerRuntime {
    broker: Broker,
    registrar: BrokerRegistrar,
    listener: Option<BrokerListener>,
}

impl BrokerRuntime {
    /// Bind the one private IPv4 loopback broker at an ephemeral port. Production always uses this.
    pub fn bind() -> std::io::Result<Self> {
        Self::bind_at(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// Bind the broker at an explicit loopback IPv4 address. Production passes `127.0.0.1:0`; the
    /// daemon integration tests use it to force a real `EADDRINUSE` bind failure. A non-loopback or
    /// non-IPv4 address is refused by [`BrokerListener::bind_at`].
    pub fn bind_at(addr: SocketAddr) -> std::io::Result<Self> {
        let (listener, broker) = BrokerListener::bind_at(
            addr,
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom::new()),
        )?;
        let registrar = broker.registrar();
        Ok(Self {
            broker,
            registrar,
            listener: Some(listener),
        })
    }

    /// The cloneable, create-only registration handle injected into the orchestrator.
    pub fn registrar(&self) -> BrokerRegistrar {
        self.registrar.clone()
    }

    /// A shared handle over the broker, for supervision.
    pub fn broker_handle(&self) -> Broker {
        self.broker.clone()
    }

    /// Take the bound listener to hand to the serving task. `None` once taken.
    pub fn take_listener(&mut self) -> Option<BrokerListener> {
        self.listener.take()
    }

    /// The bound listener's address, in-process only. Never a publication surface: the daemon
    /// integration tests read it to connect to the private broker; no production path calls it, so
    /// the non-test lib target has no caller (`allow(dead_code)` scoped to that build).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn bound_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().map(BrokerListener::local_addr)
    }

    /// Revoke every registry entry still live (design §11.3). Called after the control loop has
    /// stopped the workers, before the listener is drained.
    pub fn revoke_all(&self) {
        self.broker.revoke_all();
    }
}

/// Supervise the broker serving task for the daemon's lifetime.
///
/// * On a clean daemon shutdown (`shutdown` fires first), the serving task is already draining —
///   its own shutdown signal is the same one — so this only bounds the wait.
/// * If the serving task ends **while the daemon is still live** (an accept error, or a task that
///   died silently), the broker is atomically marked unavailable and every grant is revoked, and
///   one bounded error is logged. The daemon keeps running: legacy native-login work may continue,
///   and provider status surfaces unavailable (design §11.2).
///
/// MUTATION GUARD: an implementation that merely dropped the serve handle without calling
/// `mark_unavailable` would leave availability `true` and a live grant registered.
pub async fn supervise(
    serve: JoinHandle<std::io::Result<()>>,
    broker: Broker,
    mut shutdown: CancelWait,
    drain: Duration,
) {
    let mut serve = serve;
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            // The daemon is shutting down: revoke/drain happened in `run`'s tail; bound the join so
            // a stuck connection cannot hold the process open.
            let _ = tokio::time::timeout(drain, &mut serve).await;
        }
        result = &mut serve => {
            match result {
                Ok(Err(error)) => tracing::error!(
                    err = %error,
                    "provider broker serving task failed unexpectedly; broker marked unavailable"
                ),
                Ok(Ok(())) => tracing::error!(
                    "provider broker serving task exited unexpectedly; broker marked unavailable"
                ),
                Err(error) => tracing::error!(
                    err = %error,
                    "provider broker serving task was aborted; broker marked unavailable"
                ),
            }
            broker.mark_unavailable();
        }
    }
}

/// Test-only: a valid registration plan for `provider`, matching [`test_lease`]'s fingerprint.
#[cfg(test)]
pub(crate) fn test_plan(provider: &str) -> rhapsody_provider_broker::BrokerRegistrationPlan {
    use rhapsody_provider_broker::{BrokerProtocol, BrokerRegistrationPlan, DEFAULT_BROKER_LIMITS};
    BrokerRegistrationPlan::new(
        provider,
        BrokerProtocol::OpenAiChatCompletions,
        "https://api.example.com/v1",
        false,
        "model-x",
        DEFAULT_BROKER_LIMITS,
    )
    .expect("test plan")
}

/// Test-only: a bound credential lease for `provider`, matching [`test_plan`]'s binding.
#[cfg(test)]
pub(crate) fn test_lease(provider: &str) -> rhapsody_provider_broker::BoundCredentialLease {
    use rhapsody_provider_broker::{BoundCredentialLease, BrokerProtocol, CredentialBinding};
    let binding = CredentialBinding::new(
        provider,
        BrokerProtocol::OpenAiChatCompletions,
        "https://api.example.com/v1",
    )
    .expect("test binding");
    BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("test lease")
}

/// Test-only: register a session through `registrar` and mint one live capability; returns the
/// token and the handles that must be kept alive for the grant to stay registered.
#[cfg(test)]
pub(crate) fn mint_live(
    registrar: &BrokerRegistrar,
) -> (
    String,
    rhapsody_provider_broker::TurnAccess,
    rhapsody_provider_broker::TurnReceipt,
    rhapsody_provider_broker::BrokerSession,
) {
    use rhapsody_provider_broker::{SessionPolicy, TurnMeta};
    let mut registration = registrar
        .register_session(
            test_plan("provider-a"),
            test_lease("provider-a"),
            SessionPolicy::default(),
        )
        .expect("registration");
    let (attempt, receipt) = registration
        .ledgers
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm");
    let access = attempt.mint_access().expect("mint");
    let token = access.api_key.expose_for_child(str::to_owned);
    (token, access, receipt, registration.session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_orchestrator::CancelSignal;
    use rhapsody_provider_broker::BrokerError;

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_an_occupied_port_is_an_explicit_error() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupy");
        let port = occupied.local_addr().expect("addr").port();
        assert!(
            BrokerRuntime::bind_at(SocketAddr::from(([127, 0, 0, 1], port))).is_err(),
            "an occupied loopback port must be an explicit bind failure"
        );
    }

    /// MUTATION GUARD: drop the serve task without calling `mark_unavailable` and this test fails
    /// (availability stays `true`, the minted grant stays live).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unexpected_serve_exit_marks_unavailable_and_revokes_grants() {
        let runtime = BrokerRuntime::bind().expect("bind");
        let (token, access, receipt, session) = mint_live(&runtime.registrar());
        assert!(
            runtime.broker_handle().lookup_capability(&token).is_ok(),
            "the minted capability must be live before the failure"
        );

        // A serving task that never accepts; aborting it is the injected unexpected exit.
        let serve = tokio::spawn(std::future::pending::<std::io::Result<()>>());
        let abort = serve.abort_handle();
        let shutdown = CancelSignal::new();
        let monitor = tokio::spawn(supervise(
            serve,
            runtime.broker_handle(),
            shutdown.wait(),
            Duration::from_secs(2),
        ));
        abort.abort();
        tokio::time::timeout(Duration::from_secs(5), monitor)
            .await
            .expect("the supervisor must observe the exit")
            .expect("supervisor join");

        assert!(
            !runtime.broker_handle().is_available(),
            "an unexpected serve exit must mark the broker unavailable"
        );
        assert_eq!(
            runtime
                .broker_handle()
                .lookup_capability(&token)
                .unwrap_err(),
            BrokerError::Unauthorized,
            "an unexpected serve exit must revoke the live grant"
        );
        drop(access);
        drop(receipt);
        drop(session);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_clean_shutdown_drains_without_marking_unavailable() {
        let mut runtime = BrokerRuntime::bind().expect("bind");
        let listener = runtime.take_listener().expect("the bound listener");
        let shutdown = CancelSignal::new();
        let serve = {
            let mut wait = shutdown.wait();
            tokio::spawn(async move {
                listener
                    .run_with_shutdown(async move { wait.cancelled().await })
                    .await
            })
        };
        let monitor = tokio::spawn(supervise(
            serve,
            runtime.broker_handle(),
            shutdown.wait(),
            Duration::from_secs(2),
        ));
        // Let the serving task enter its accept loop, then shut the daemon down.
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), monitor)
            .await
            .expect("the supervisor must drain within its bounded window")
            .expect("supervisor join");
        assert!(
            runtime.broker_handle().is_available(),
            "a clean daemon shutdown must not mark the broker unavailable"
        );
    }
}
