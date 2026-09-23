//! The cloneable broker registration handle (design §3.3, §11.1).
//!
//! `rhapsodyd` binds and serves the one private broker, then injects a [`BrokerRegistrar`] into
//! preparation. It can create move-only sessions and observe availability; it deliberately exposes
//! no credential accessor, no capability lookup, and no registry-wide revocation — so a holder of
//! the handle cannot inspect credentials or enumerate/revoke unrelated sessions.
//!
//! When the broker has been marked unavailable (its serving task failed unexpectedly, design §11.2)
//! registration refuses with [`BrokerError::Unavailable`], the typed `provider_broker_unavailable`
//! refusal. There is no direct-key fallback.

use std::fmt;

use crate::binding::BoundCredentialLease;
use crate::broker::{Broker, BrokerRegistration, BrokerRegistrationPlan};
use crate::error::BrokerError;
use crate::policy::SessionPolicy;

/// A cloneable, create-only handle over a live broker. Handed to the orchestrator's preparation
/// path; PB4 only carries it.
#[derive(Clone)]
pub struct BrokerRegistrar {
    broker: Broker,
}

impl BrokerRegistrar {
    pub(crate) fn new(broker: Broker) -> Self {
        Self { broker }
    }

    /// Whether the broker is still serving and able to register sessions.
    pub fn is_available(&self) -> bool {
        self.broker.is_available()
    }

    /// Register a session, refusing with [`BrokerError::Unavailable`] once the broker's serving
    /// task has failed. Otherwise the behavior is exactly [`Broker::register_session`].
    pub fn register_session(
        &self,
        plan: BrokerRegistrationPlan,
        lease: BoundCredentialLease,
        policy: SessionPolicy,
    ) -> Result<BrokerRegistration, BrokerError> {
        if !self.broker.is_available() {
            // Drop the incoming lease without creating a session: a down broker never falls back.
            return Err(BrokerError::Unavailable);
        }
        self.broker.register_session(plan, lease, policy)
    }
}

impl fmt::Debug for BrokerRegistrar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider broker registrar>")
    }
}
