//! The broker and its registration entry point.
//!
//! [`Broker`] owns the shared capability registry, the injected clock, and the injected random
//! source. It exposes registration and digest-only capability lookup; it has **no** public
//! credential lookup. The listener and outbound HTTP client arrive in a later slice (PB2); PB1 is
//! protocol-neutral custody, capability, and receipt machinery only.
//!
//! Dependency direction (design §3.3): this crate depends on no `rhapsody-agent`,
//! `rhapsody-orchestrator`, `rhapsody-httpapi`, or desktop crate. A
//! [`BrokerRegistration`](crate::BrokerRegistration) carries the move-only session and its
//! non-secret ledger receiver; a later slice's cloneable registrar can only create sessions — it
//! cannot inspect credentials or enumerate unrelated ones.

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::binding::{BoundCredentialLease, CredentialBinding};
use crate::clock::Clock;
use crate::error::BrokerError;
use crate::ledger::TurnOutcome;
use crate::metrics::BrokerMetrics;
use crate::policy::{BrokerLimits, BrokerProtocol, SessionPolicy};
use crate::random::RandomSource;
use crate::session::{BrokerLedgerReceiver, BrokerSession};
use crate::state::{
    BrokerInner, Registry, SESSION_ID_BYTES, SessionId, TokenDigest, is_valid_token_shape, lock,
};
use crate::turn::CapabilityGrant;

/// The broker-owned, protocol-neutral registration plan: the normalized adapter, the fixed upstream
/// endpoint, the exact model, the stable diagnostic id, and the validated hard-bounded limits.
///
/// Later slices lower the operator-facing `ResolvedProviderPlan` into this type once, after all pure
/// validation; the broker never imports config-layer types or invents config defaults.
pub struct BrokerRegistrationPlan {
    stable_provider_id: String,
    protocol: BrokerProtocol,
    normalized_endpoint: String,
    allow_insecure_http: bool,
    model_id: String,
    limits: BrokerLimits,
}

impl BrokerRegistrationPlan {
    /// Build and validate a registration plan. Limits are validated as a whole here.
    pub fn new(
        stable_provider_id: impl Into<String>,
        protocol: BrokerProtocol,
        normalized_endpoint: impl Into<String>,
        allow_insecure_http: bool,
        model_id: impl Into<String>,
        limits: BrokerLimits,
    ) -> Result<Self, BrokerError> {
        let plan = Self {
            stable_provider_id: stable_provider_id.into(),
            protocol,
            normalized_endpoint: normalized_endpoint.into(),
            allow_insecure_http,
            model_id: model_id.into(),
            limits,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// The stable provider id (provenance/diagnostics only).
    pub fn stable_provider_id(&self) -> &str {
        &self.stable_provider_id
    }

    /// The normalized protocol.
    pub fn protocol(&self) -> BrokerProtocol {
        self.protocol
    }

    /// The fixed, operator-approved upstream endpoint (already normalized by an earlier slice).
    pub fn normalized_endpoint(&self) -> &str {
        &self.normalized_endpoint
    }

    /// Whether plaintext HTTP to the upstream is explicitly allowed.
    pub fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http
    }

    /// The exact accepted model id.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The validated limit block.
    pub fn limits(&self) -> &BrokerLimits {
        &self.limits
    }

    /// The canonical binding derived from the plan. Registration compares its fingerprint against
    /// the lease's before taking custody.
    pub fn binding(&self) -> Result<CredentialBinding, BrokerError> {
        CredentialBinding::new(
            self.stable_provider_id.clone(),
            self.protocol,
            self.normalized_endpoint.clone(),
        )
    }

    fn validate(&self) -> Result<(), BrokerError> {
        if self.stable_provider_id.is_empty() || self.model_id.is_empty() {
            return Err(BrokerError::InvalidBinding);
        }
        self.limits.validate().map_err(BrokerError::InvalidLimits)
    }
}

impl fmt::Debug for BrokerRegistrationPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokerRegistrationPlan")
            .field("stable_provider_id", &self.stable_provider_id)
            .field("protocol", &self.protocol)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// A successful registration: the move-only custody handle plus its non-secret ledger receiver.
pub struct BrokerRegistration {
    /// The session custody handle.
    pub session: BrokerSession,
    /// The turn supervisor / receipt receiver.
    pub ledgers: BrokerLedgerReceiver,
}

impl fmt::Debug for BrokerRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<broker registration>")
    }
}

/// The broker: shared registry plus injected clock and randomness. Cloneable as a handle (it holds
/// no credential of its own), but with no credential-lookup method.
#[derive(Clone)]
pub struct Broker {
    inner: Arc<BrokerInner>,
}

impl Broker {
    /// Construct a broker whose turn capabilities carry `base_url`. The base URL is the loopback
    /// `/v1` address a later slice binds; PB1 validates only that it is non-empty.
    pub fn new(
        base_url: impl Into<String>,
        clock: Arc<dyn Clock>,
        rng: Arc<dyn RandomSource>,
    ) -> Result<Self, BrokerError> {
        let base_url = base_url.into();
        if base_url.is_empty() {
            return Err(BrokerError::InvalidBaseUrl);
        }
        Ok(Self {
            inner: Arc::new(BrokerInner {
                base_url,
                clock,
                rng,
                registry: Mutex::new(Registry::default()),
                metrics: BrokerMetrics::new(),
                #[cfg(test)]
                mint_race: crate::state::MintRaceGate::default(),
            }),
        })
    }

    /// The broker's bounded, non-secret counters. Shared with every session; carries no session,
    /// run, capability, or key identifier (design §13).
    pub fn metrics(&self) -> Arc<BrokerMetrics> {
        Arc::clone(&self.inner.metrics)
    }

    /// Register a session from a bound credential lease.
    ///
    /// The plan's binding fingerprint must match the lease's, or the lease is dropped and no session
    /// is created ([`BrokerError::BindingMismatch`]). Session-id collisions fail closed without
    /// replacing or aliasing an existing session.
    pub fn register_session(
        &self,
        plan: BrokerRegistrationPlan,
        lease: BoundCredentialLease,
        policy: SessionPolicy,
    ) -> Result<BrokerRegistration, BrokerError> {
        plan.validate()?;
        if plan.limits() != policy.limits() {
            return Err(BrokerError::LimitsMismatch);
        }
        if !plan.binding()?.fingerprint().matches(&lease.fingerprint()) {
            // Drop the incoming lease; create no session.
            return Err(BrokerError::BindingMismatch);
        }

        let mut raw = [0u8; SESSION_ID_BYTES];
        self.inner
            .rng
            .fill(&mut raw)
            .map_err(|_| BrokerError::RandomSourceFailure)?;
        let id = SessionId::from_bytes(raw);

        let session = Arc::new(crate::state::SessionInner::new(
            id,
            Arc::clone(&self.inner),
            plan,
            policy,
            lease,
        ));
        {
            let mut registry = lock(&self.inner.registry);
            if registry.sessions.contains_key(&id) {
                // Dropping `session` here also drops (and zeroizes) the lease.
                return Err(BrokerError::SessionIdCollision);
            }
            registry.sessions.insert(id, Arc::downgrade(&session));
        }

        let ledgers = BrokerLedgerReceiver {
            session: Arc::clone(&session),
        };
        Ok(BrokerRegistration {
            session: BrokerSession { inner: session },
            ledgers,
        })
    }

    /// The loopback base URL this broker's turn capabilities carry
    /// (`http://127.0.0.1:<ephemeral>/v1`). Non-secret: it is the child-facing address, not a
    /// credential.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Resolve a presented bearer value to its live grant.
    ///
    /// Missing, malformed, unknown, expired, and revoked values all return
    /// [`BrokerError::Unauthorized`] — the caller cannot distinguish token states. Lookup validates
    /// the exact length and alphabet, then hashes the complete presented value and indexes by the
    /// complete fixed-length digest; the raw value is never a registry key.
    pub fn lookup_capability(&self, presented: &str) -> Result<CapabilityGrant, BrokerError> {
        if !is_valid_token_shape(presented) {
            return Err(BrokerError::Unauthorized);
        }
        let digest = TokenDigest::of(presented.as_bytes());
        let inner = {
            let registry = lock(&self.inner.registry);
            registry.grants.get(&digest).cloned()
        }
        .ok_or(BrokerError::Unauthorized)?;

        if inner.revoked.load(Ordering::Acquire) || inner.session.is_revoked() {
            return Err(BrokerError::Unauthorized);
        }
        if inner.is_expired(self.inner.clock.now()) {
            // Observe expiry deterministically: revoke the grant and finalize its receipt now. The
            // metric is recorded only by the call that actually finalizes, so a concurrent lookup
            // cannot double-count the same expiry.
            if inner.revoke_and_finalize(TurnOutcome::Expired).is_some() {
                self.inner.metrics.record_revocation();
            }
            return Err(BrokerError::Unauthorized);
        }
        Ok(CapabilityGrant::new(inner))
    }
}

impl fmt::Debug for Broker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider broker>")
    }
}
