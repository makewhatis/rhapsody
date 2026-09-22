//! The finalized turn ledger.
//!
//! A [`TurnLedger`] is the bounded, non-secret receipt the worker drains once per turn for adapter
//! reconciliation (design §3.2). It carries closed counters only: no token, no key, no session id
//! used as a label. The turn ordinal makes reconciliation idempotent when normal completion races
//! cancellation.

/// How a turn ended. Every [`BrokerReceiver`](crate::BrokerLedgerReceiver) turn reaches exactly one
/// of these when its receipt finalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// `TurnAccess::finish` was called normally.
    Completed,
    /// No capability ever became live: the attempt was refused, dropped, or minting failed.
    NoCapability,
    /// A live capability was revoked before normal completion (turn error, Stop, session revoke).
    Revoked,
    /// The capability's absolute expiry passed.
    Expired,
    /// The supervisor dropped the receipt without draining it (a caller bug, still accounted).
    SupervisorReleased,
}

/// The finalized counters for one outer turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnLedger {
    turn_ordinal: u64,
    outcome: TurnOutcome,
    capability_issued: bool,
    forwarded_requests: u64,
    denied_requests: u64,
    unknown_usage_requests: u64,
    request_bytes: u64,
    response_bytes: u64,
    reserved_tokens: u64,
    reported_tokens: Option<u64>,
}

impl TurnLedger {
    pub(crate) fn new(
        turn_ordinal: u64,
        outcome: TurnOutcome,
        capability_issued: bool,
        counters: ReservationCounters,
        reported_tokens: Option<u64>,
    ) -> Self {
        let unknown_usage_requests = match reported_tokens {
            Some(_) => 0,
            // PB1 has no usage parser: every forwarded request is conservatively unknown.
            None => counters.forwarded_requests,
        };
        Self {
            turn_ordinal,
            outcome,
            capability_issued,
            forwarded_requests: counters.forwarded_requests,
            denied_requests: counters.denied_requests,
            unknown_usage_requests,
            request_bytes: counters.request_bytes,
            response_bytes: counters.response_bytes,
            reserved_tokens: counters.reserved_tokens,
            reported_tokens,
        }
    }

    /// Monotonic, session-local turn ordinal; the reconciliation idempotency key.
    pub fn turn_ordinal(&self) -> u64 {
        self.turn_ordinal
    }

    /// How the turn ended.
    pub fn outcome(&self) -> TurnOutcome {
        self.outcome
    }

    /// Whether a capability was minted for this turn.
    pub fn capability_issued(&self) -> bool {
        self.capability_issued
    }

    /// Forwarded upstream requests observed.
    pub fn forwarded_requests(&self) -> u64 {
        self.forwarded_requests
    }

    /// Locally denied authenticated requests observed.
    pub fn denied_requests(&self) -> u64 {
        self.denied_requests
    }

    /// Forwarded requests whose provider usage was never reported (conservatively reserved).
    pub fn unknown_usage_requests(&self) -> u64 {
        self.unknown_usage_requests
    }

    /// Bytes of request body admitted.
    pub fn request_bytes(&self) -> u64 {
        self.request_bytes
    }

    /// Bytes of response body emitted.
    pub fn response_bytes(&self) -> u64 {
        self.response_bytes
    }

    /// Token units reserved against the turn/session caps.
    pub fn reserved_tokens(&self) -> u64 {
        self.reserved_tokens
    }

    /// Provider-reported tokens, when a later slice parses them; `None` for PB1.
    pub fn reported_tokens(&self) -> Option<u64> {
        self.reported_tokens
    }
}

/// A copy of the reservation counters used to build a ledger. Kept separate so the ledger module
/// does not depend on the reservation internals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReservationCounters {
    pub forwarded_requests: u64,
    pub denied_requests: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub reserved_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_usage_marks_every_forwarded_request_unknown() {
        let ledger = TurnLedger::new(
            3,
            TurnOutcome::Revoked,
            true,
            ReservationCounters {
                forwarded_requests: 2,
                ..ReservationCounters::default()
            },
            None,
        );
        assert_eq!(ledger.unknown_usage_requests(), 2);
        assert_eq!(ledger.reported_tokens(), None);
        assert_eq!(ledger.turn_ordinal(), 3);
        assert!(ledger.capability_issued());
    }

    #[test]
    fn reported_usage_clears_the_unknown_count() {
        let ledger = TurnLedger::new(
            1,
            TurnOutcome::Completed,
            true,
            ReservationCounters {
                forwarded_requests: 2,
                ..ReservationCounters::default()
            },
            Some(42),
        );
        assert_eq!(ledger.unknown_usage_requests(), 0);
        assert_eq!(ledger.reported_tokens(), Some(42));
    }
}
