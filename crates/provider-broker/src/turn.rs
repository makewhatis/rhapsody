//! Turn handles: the move-only attempt/access pair, the retained receipt, and the read-only grant.
//!
//! The worker supervisor arms the capacity-one receipt *synchronously* and retains [`TurnReceipt`];
//! only [`BrokerTurnAttempt`] enters the cancellable adapter future. Minting consumes the attempt and
//! yields at most one [`TurnAccess`]. Attempt/access drop finalizes the receipt and revokes the grant
//! synchronously — neither depends on an async cleanup task (design §3.2, §4.3).

use std::fmt;
use std::sync::Arc;

use crate::clock::MonotonicTime;
use crate::error::BrokerError;
use crate::ledger::{TurnLedger, TurnOutcome};
use crate::policy::{BrokerLimits, BrokerProtocol};
use crate::reservations::{ConcurrencyPermit, ReserveRequest};
use crate::secret::CapabilityToken;
use crate::state::{
    MAX_TOKEN_RETRIES, TOKEN_BYTES, TokenDigest, TurnGateGuard, TurnInner, encode_token, lock,
};

/// Adapter-supplied metadata for one outer turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnMeta {
    deadline: Option<MonotonicTime>,
}

impl TurnMeta {
    /// Carry the adapter's turn deadline (if any) in the broker's monotonic domain. The grant's
    /// absolute expiry is the earlier of this and the broker's configured maximum lifetime.
    pub fn new(deadline: Option<MonotonicTime>) -> Self {
        Self { deadline }
    }

    /// No adapter deadline: the broker's maximum capability lifetime governs.
    pub fn without_deadline() -> Self {
        Self { deadline: None }
    }

    /// The adapter's turn deadline, if supplied.
    pub fn deadline(&self) -> Option<MonotonicTime> {
        self.deadline
    }
}

/// The move-only adapter half. It can mint at most one [`TurnAccess`]; dropping or refusing it
/// before minting finalizes a `no_capability` receipt.
pub struct BrokerTurnAttempt {
    pub(crate) inner: Option<Arc<TurnInner>>,
    pub(crate) gate: Option<TurnGateGuard>,
}

impl BrokerTurnAttempt {
    /// Consume the attempt and mint the one capability it can ever produce.
    ///
    /// All failure paths finalize the armed receipt with `no_capability` and release the session's
    /// capacity-one turn gate, so a refused or failed turn never leaves custody dangling. A turn
    /// whose receipt was already dropped (revoked) fails closed with [`BrokerError::TurnRevoked`]
    /// rather than hand back a dead handle.
    pub fn mint_access(mut self) -> Result<TurnAccess, BrokerError> {
        let inner = self.inner.take().ok_or(BrokerError::AttemptConsumed)?;
        let gate = self.gate.take();
        match mint_token(&inner) {
            Ok((token, _digest)) => Ok(TurnAccess {
                base_url: inner.session.broker.base_url.clone(),
                api_key: token,
                inner,
                gate,
                finished: false,
            }),
            Err(error) => {
                inner.finalize(TurnOutcome::NoCapability);
                Err(error)
            }
        }
    }
}

impl fmt::Debug for BrokerTurnAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<broker turn attempt>")
    }
}

impl Drop for BrokerTurnAttempt {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.finalize(TurnOutcome::NoCapability);
        }
    }
}

/// Mint one turn capability: 32 CSPRNG bytes, encoded and reserved by digest only.
fn mint_token(inner: &Arc<TurnInner>) -> Result<(CapabilityToken, TokenDigest), BrokerError> {
    let session = &inner.session;
    if session.is_revoked() {
        return Err(BrokerError::SessionRevoked);
    }
    if inner.is_revoked() || inner.is_finalized() {
        return Err(BrokerError::TurnRevoked);
    }
    if inner.is_expired(session.broker.clock.now()) {
        return Err(BrokerError::TurnExpired);
    }
    for _ in 0..MAX_TOKEN_RETRIES {
        let mut raw = [0u8; TOKEN_BYTES];
        session
            .broker
            .rng
            .fill(&mut raw)
            .map_err(|_| BrokerError::RandomSourceFailure)?;
        let encoded = encode_token(&raw);
        let digest = TokenDigest::of(&encoded);
        {
            let mut registry = lock(&session.broker.registry);
            // Re-check under the registry lock (which a concurrent `revoke_grant` also takes)
            // so a released receipt cannot race into a live grant.
            if inner.is_revoked() || inner.is_finalized() {
                return Err(BrokerError::TurnRevoked);
            }
            if registry.grants.contains_key(&digest) {
                continue;
            }
            registry.grants.insert(digest, Arc::clone(inner));
        }
        let token = CapabilityToken::from_encoded(encoded);
        inner.mark_issued(digest);
        inner.slot.set_access_live();
        return Ok((token, digest));
    }
    Err(BrokerError::TokenCollisionExhausted)
}

/// The adapter-facing handle for one live turn: the loopback base URL and the single-use capability.
///
/// Move-only. [`TurnAccess::finish`] declares normal completion; dropping without `finish` records a
/// revocation. Either way the grant is removed from the registry and the receipt is finalized
/// before `drop` returns.
pub struct TurnAccess {
    /// The loopback base URL the child must call (`http://127.0.0.1:<ephemeral>/v1`). The exact
    /// address is intentionally not published anywhere else.
    pub base_url: String,
    /// The turn capability. The raw value is reachable only through
    /// [`CapabilityToken::expose_for_child`].
    pub api_key: CapabilityToken,
    inner: Arc<TurnInner>,
    gate: Option<TurnGateGuard>,
    finished: bool,
}

impl TurnAccess {
    /// Declare normal completion. The drop that follows finalizes the receipt with `completed`
    /// (unless the session was revoked or the capability had already expired).
    pub fn finish(mut self) {
        self.finished = true;
    }

    /// The monotonic ordinal of this turn within its session.
    pub fn turn_ordinal(&self) -> u64 {
        self.inner.ordinal
    }

    /// The absolute monotonic expiry of this capability.
    pub fn not_after(&self) -> MonotonicTime {
        self.inner.not_after
    }
}

impl fmt::Debug for TurnAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted turn access>")
    }
}

impl Drop for TurnAccess {
    fn drop(&mut self) {
        let declared = if self.finished {
            TurnOutcome::Completed
        } else {
            TurnOutcome::Revoked
        };
        self.inner.revoke_and_finalize(declared);
        // Releasing the gate last keeps "one live turn per session" true for the whole drop.
        let _ = self.gate.take();
    }
}

/// The supervisor half, retained outside the cancellable turn future. Once the attempt/access has
/// finished or dropped, the receipt is guaranteed finalized and can be taken without waiting.
///
/// The receipt acts only on the ledger for *its own* turn ordinal, so keeping it alive past `take`
/// cannot steal or erase a later turn's ledger. Dropping a receipt without taking it still finalizes
/// the slot, and it also revokes the armed turn (a caller bug) so no capability can be minted or
/// keep spending unwatched.
pub struct TurnReceipt {
    inner: Arc<TurnInner>,
}

impl TurnReceipt {
    pub(crate) fn new(inner: Arc<TurnInner>) -> Self {
        Self { inner }
    }

    /// Whether the receipt has been finalized and is ready to take.
    pub fn is_finalized(&self) -> bool {
        self.inner.is_finalized()
    }

    /// Drain the finalized ledger for this receipt's turn. `None` if the attempt/access has not
    /// finished or dropped yet, or if the slot currently holds a different turn's ledger; the slot
    /// is left intact so the caller can take it later.
    pub fn take(&self) -> Option<TurnLedger> {
        self.inner.slot.take(self.inner.ordinal)
    }

    /// The monotonic ordinal of the turn this receipt accounts for.
    pub fn turn_ordinal(&self) -> u64 {
        self.inner.ordinal
    }
}

impl fmt::Debug for TurnReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnReceipt")
            .field("turn_ordinal", &self.inner.ordinal)
            .finish()
    }
}

impl Drop for TurnReceipt {
    fn drop(&mut self) {
        // Revoke first so a receipt dropped before the attempt/access finishes cannot leave a
        // capability live and unaccounted; `supervisor_release` finalizes exactly once.
        self.inner.supervisor_release();
        self.inner.slot.drain(self.inner.ordinal);
    }
}

/// A read-only, non-secret view of a live turn grant, returned by
/// [`Broker::lookup_capability`](crate::Broker::lookup_capability). It exposes only bounded metadata
/// and the reservation entry points; it has no token, key, or session accessor.
#[derive(Clone)]
pub struct CapabilityGrant {
    inner: Arc<TurnInner>,
}

impl CapabilityGrant {
    pub(crate) fn new(inner: Arc<TurnInner>) -> Self {
        Self { inner }
    }

    /// The stable provider id (provenance/diagnostics only).
    pub fn stable_provider_id(&self) -> &str {
        self.inner.session.plan.stable_provider_id()
    }

    /// The normalized protocol.
    pub fn protocol(&self) -> BrokerProtocol {
        self.inner.session.plan.protocol()
    }

    /// The fixed, operator-approved upstream endpoint.
    pub fn normalized_endpoint(&self) -> &str {
        self.inner.session.plan.normalized_endpoint()
    }

    /// The exact accepted model id.
    pub fn model_id(&self) -> &str {
        self.inner.session.plan.model_id()
    }

    /// The grant's finite limits.
    pub fn limits(&self) -> &BrokerLimits {
        self.inner.session.policy.limits()
    }

    /// The monotonic ordinal of this turn within its session.
    pub fn turn_ordinal(&self) -> u64 {
        self.inner.ordinal
    }

    /// This capability's absolute monotonic expiry.
    pub fn not_after(&self) -> MonotonicTime {
        self.inner.not_after
    }

    /// Whether the grant or its parent session has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.inner
            .revoked
            .load(std::sync::atomic::Ordering::Acquire)
            || self.inner.session.is_revoked()
    }

    /// Atomically reserve one forwarded request against the turn and session limits. Reserves
    /// nothing if any limit would be exceeded.
    pub fn reserve_request(
        &self,
        request_bytes: u64,
        response_bytes: u64,
        output_tokens: u64,
    ) -> Result<(), BrokerError> {
        self.check_live()?;
        self.inner.reservations.try_reserve(
            &self.inner.session.session_reservations,
            ReserveRequest {
                request_bytes,
                response_bytes,
                output_tokens,
            },
        )
    }

    /// Acquire one of the turn's concurrent-request permits; the permit releases the slot on drop.
    pub fn acquire_concurrency(&self) -> Result<ConcurrencyPermit, BrokerError> {
        self.check_live()?;
        self.inner.reservations.try_acquire_concurrency()
    }

    /// Count one locally denied authenticated request. Reaching the configured threshold refuses
    /// further denials so the caller can revoke the turn.
    pub fn record_denied(&self) -> Result<(), BrokerError> {
        self.check_live()?;
        self.inner.reservations.record_denied()
    }

    fn check_live(&self) -> Result<(), BrokerError> {
        if self.is_revoked() {
            return Err(BrokerError::Unauthorized);
        }
        if self.inner.is_expired(self.inner.session.broker.clock.now()) {
            return Err(BrokerError::Unauthorized);
        }
        Ok(())
    }
}

impl fmt::Debug for CapabilityGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityGrant")
            .field("stable_provider_id", &self.stable_provider_id())
            .field("turn_ordinal", &self.inner.ordinal)
            .field("capability", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_meta_carries_an_optional_deadline() {
        assert_eq!(TurnMeta::without_deadline().deadline(), None);
        let at = MonotonicTime::from_nanos(5);
        assert_eq!(TurnMeta::new(Some(at)).deadline(), Some(at));
    }
}
