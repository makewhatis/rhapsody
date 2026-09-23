//! Session custody and the capacity-one ledger receiver.
//!
//! [`BrokerSession`] is the opaque, move-only custody handle for one prepared dispatch: it owns the
//! bound credential lease and revokes it (plus every child grant) on explicit [`BrokerSession::revoke`]
//! or on drop. [`BrokerLedgerReceiver`] is the move-only supervisor half;
//! [`BrokerLedgerReceiver::arm_turn`] synchronously reserves the capacity-one receipt before any
//! cancellable adapter work begins.

use std::fmt;
use std::sync::Arc;

use crate::error::BrokerError;
use crate::state::{SessionInner, TurnInner};
use crate::turn::{BrokerTurnAttempt, TurnMeta, TurnReceipt};

/// The opaque custody handle for one provider-broker session. It holds the credential lease and a
/// registration; it is not `Clone` and exposes no credential accessor.
pub struct BrokerSession {
    pub(crate) inner: Arc<SessionInner>,
}

impl BrokerSession {
    /// Revoke the session: release custody, drop every child grant, and refuse new turns.
    /// Idempotent, and also performed on drop.
    pub fn revoke(&self) {
        self.inner.revoke();
    }

    /// Whether this session has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.inner.is_revoked()
    }

    /// Whether the session still holds its credential lease (non-secret presence check).
    pub fn has_custody(&self) -> bool {
        self.inner.has_custody()
    }
}

impl fmt::Debug for BrokerSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted broker session>")
    }
}

impl Drop for BrokerSession {
    fn drop(&mut self) {
        self.inner.revoke();
    }
}

/// The move-only turn supervisor, retained by the worker outside the cancellable turn future. It
/// owns the capacity-one receipt slot and arms one turn at a time.
pub struct BrokerLedgerReceiver {
    pub(crate) session: Arc<SessionInner>,
}

impl BrokerLedgerReceiver {
    /// Synchronously arm the next turn: reserve the capacity-one receipt slot, take the session's
    /// single turn gate, and compute the absolute expiry as the earlier of the adapter's deadline
    /// and the broker's maximum capability lifetime.
    ///
    /// Refuses with [`BrokerError::TurnAlreadyArmed`] when the prior receipt has not been drained,
    /// with [`BrokerError::TurnAlreadyActive`] when an attempt/access is still live,
    /// [`BrokerError::SessionBudgetExhausted`] when the session/run token cap is already spent, and
    /// [`BrokerError::DayBudgetExhausted`] when the configured durable UTC-day budget is exhausted.
    pub fn arm_turn(
        &mut self,
        meta: TurnMeta,
    ) -> Result<(BrokerTurnAttempt, TurnReceipt), BrokerError> {
        let session = &self.session;
        if session.is_revoked() {
            return Err(BrokerError::SessionRevoked);
        }

        // Capacity-one receipt: a new attempt cannot arm until the prior receipt is drained.
        session.receipt_slot.begin_armed()?;

        // Capacity-one turn gate: at most one armed attempt or live TurnAccess.
        let gate = match session.turn_gate.try_acquire() {
            Some(gate) => gate,
            None => {
                session.receipt_slot.abandon_armed();
                return Err(BrokerError::TurnAlreadyActive);
            }
        };

        if session.session_reservations.remaining() == 0 {
            drop(gate);
            session.receipt_slot.abandon_armed();
            return Err(BrokerError::SessionBudgetExhausted);
        }

        // Refuse before child spawn when the configured durable UTC-day budget is already exhausted.
        // This is the early refusal; the atomic per-request charge is what actually prevents
        // first-turn and concurrent-run oversubscription.
        if let (Some(authority), Some(cap)) = (
            session.policy.day_authority(),
            session.policy.limits().max_reserved_token_units_per_utc_day,
        ) && authority.charged_today(session.plan.stable_provider_id()) >= cap
        {
            drop(gate);
            session.receipt_slot.abandon_armed();
            return Err(BrokerError::DayBudgetExhausted);
        }

        let ordinal = session.next_ordinal();
        let now = session.broker.clock.now();
        let max_life = session.policy.limits().max_capability_lifetime;
        let not_after = match meta.deadline() {
            Some(deadline) => deadline.min(now.saturating_add(max_life)),
            None => now.saturating_add(max_life),
        };

        let slot = Arc::clone(&session.receipt_slot);
        let inner = Arc::new(TurnInner::new(
            Arc::clone(session),
            slot,
            ordinal,
            not_after,
        ));
        let receipt = TurnReceipt::new(Arc::clone(&inner));
        Ok((
            BrokerTurnAttempt {
                inner: Some(inner),
                gate: Some(gate),
            },
            receipt,
        ))
    }
}

impl fmt::Debug for BrokerLedgerReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<broker ledger receiver>")
    }
}
