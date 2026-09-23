//! The finalized turn ledger.
//!
//! A [`TurnLedger`] is the bounded, non-secret receipt the worker drains once per turn for adapter
//! reconciliation (design §3.2). It carries closed counters only: no token, no key, no session id
//! used as a label. The turn ordinal makes reconciliation idempotent when normal completion races
//! cancellation.

/// The authority behind a provider usage observation (design §7.3).
///
/// V1's generic OpenAI-compatible adapter is measurement only, never admission authority: a
/// syntactically valid report can still under-report, so it is marked unverified and never releases
/// a token reservation. A future provider-specific trusted-settlement adapter would require its own
/// reviewed identity/trust contract and cannot silently change this generic adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageAuthority {
    /// A syntactically valid generic provider report; never exact measured authority in v1.
    ProviderReportedUnverified,
}

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
///
/// Provider-reported and admission-reservation totals are separate fields on purpose: the worker
/// replaces brokered-turn token/cache counts from the provider-reported total while the conservative
/// reservation total remains the enforceable charge. `usage_authority` is `None` only when no
/// request produced a usable report; `usage_incomplete` records that at least one request never did.
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
    provider_reported_tokens: Option<u64>,
    reported_requests: u64,
    inconsistent_usage_requests: u64,
    usage_authority: Option<UsageAuthority>,
}

impl TurnLedger {
    pub(crate) fn new(
        turn_ordinal: u64,
        outcome: TurnOutcome,
        capability_issued: bool,
        counters: ReservationCounters,
        usage: UsageCounters,
    ) -> Self {
        let usage_authority = if usage.reported_requests > 0 {
            Some(UsageAuthority::ProviderReportedUnverified)
        } else {
            None
        };
        Self {
            turn_ordinal,
            outcome,
            capability_issued,
            forwarded_requests: counters.forwarded_requests,
            denied_requests: counters.denied_requests,
            unknown_usage_requests: usage.unknown_usage_requests,
            request_bytes: counters.request_bytes,
            response_bytes: counters.response_bytes,
            reserved_tokens: counters.reserved_tokens,
            provider_reported_tokens: usage.provider_reported_tokens,
            reported_requests: usage.reported_requests,
            inconsistent_usage_requests: usage.inconsistent_usage_requests,
            usage_authority,
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

    /// Token units reserved against the turn/session/optional day caps. This is the conservative
    /// admission charge and is never reduced by a provider report in the generic adapter.
    pub fn reserved_tokens(&self) -> u64 {
        self.reserved_tokens
    }

    /// The conservative provider-reported total, `None` when no request produced usable usage.
    /// Measurement only in v1: it is never presented as exact measured usage.
    pub fn provider_reported_tokens(&self) -> Option<u64> {
        self.provider_reported_tokens
    }

    /// How many forwarded requests settled with a syntactically valid provider report.
    pub fn reported_requests(&self) -> u64 {
        self.reported_requests
    }

    /// How many valid reports disagreed internally (a bounded diagnostic; settlement used the
    /// larger conservative value).
    pub fn inconsistent_usage_requests(&self) -> u64 {
        self.inconsistent_usage_requests
    }

    /// The authority behind the provider-reported total, if any.
    pub fn usage_authority(&self) -> Option<UsageAuthority> {
        self.usage_authority
    }

    /// Whether at least one forwarded request never produced usable usage (missing, malformed, or
    /// aborted). Those requests remain conservatively charged and are not relabeled as reported.
    pub fn usage_incomplete(&self) -> bool {
        self.unknown_usage_requests > 0
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

/// A copy of the settled usage totals used to build a ledger, kept separate from the reservation
/// internals for the same reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UsageCounters {
    pub provider_reported_tokens: Option<u64>,
    pub reported_requests: u64,
    pub unknown_usage_requests: u64,
    pub inconsistent_usage_requests: u64,
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
                reserved_tokens: 500,
                ..ReservationCounters::default()
            },
            UsageCounters {
                unknown_usage_requests: 2,
                ..UsageCounters::default()
            },
        );
        assert_eq!(ledger.unknown_usage_requests(), 2);
        assert!(ledger.usage_incomplete());
        assert_eq!(ledger.provider_reported_tokens(), None);
        assert_eq!(ledger.usage_authority(), None);
        assert_eq!(ledger.reserved_tokens(), 500);
        assert_eq!(ledger.turn_ordinal(), 3);
        assert!(ledger.capability_issued());
    }

    #[test]
    fn a_provider_report_is_unverified_measurement_kept_apart_from_the_reservation() {
        let ledger = TurnLedger::new(
            1,
            TurnOutcome::Completed,
            true,
            ReservationCounters {
                forwarded_requests: 2,
                reserved_tokens: 9_000,
                ..ReservationCounters::default()
            },
            UsageCounters {
                provider_reported_tokens: Some(42),
                reported_requests: 1,
                unknown_usage_requests: 1,
                ..UsageCounters::default()
            },
        );
        assert_eq!(ledger.unknown_usage_requests(), 1);
        assert!(ledger.usage_incomplete(), "one request never reported");
        assert_eq!(ledger.provider_reported_tokens(), Some(42));
        assert_eq!(ledger.reported_requests(), 1);
        assert_eq!(
            ledger.usage_authority(),
            Some(UsageAuthority::ProviderReportedUnverified)
        );
        assert_eq!(
            ledger.reserved_tokens(),
            9_000,
            "the reservation is separate from and larger than the report"
        );
    }
}
