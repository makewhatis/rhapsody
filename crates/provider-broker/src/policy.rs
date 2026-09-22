//! Protocol-neutral broker policy.
//!
//! [`BrokerLimits`] is the finite, always-on limit block every turn grant carries (design §8.1),
//! with the v1 defaults and compile-time hard ceilings as constants. [`BrokerProtocol`] is the
//! normalized adapter axis (v1: OpenAI Chat Completions only). Nothing here is negotiated with the
//! harness: the block is validated at registration and can only be tightened per provider.

use std::time::Duration;

use crate::error::LimitViolation;

/// The normalized broker protocol. V1 supports exactly one; a future protocol is a separate adapter,
/// never a widened handler (design §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerProtocol {
    /// OpenAI-compatible Chat Completions, used by OpenCode.
    OpenAiChatCompletions,
}

impl BrokerProtocol {
    /// The stable, non-secret canonical identifier hashed into a credential binding. It is the
    /// reviewed adapter identity (`provider-auth-design.md` §2.2 / `provider-broker-design.md`
    /// §3.1), and must equal `rhapsody_config::ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1` and
    /// `rhapsody_agent::ProviderProtocol::adapter_id()`: one binding is used identically by desktop
    /// storage, credential reads, and broker registration.
    pub const fn canonical_id(self) -> &'static str {
        match self {
            BrokerProtocol::OpenAiChatCompletions => "openai-chat-completions-bearer-v1",
        }
    }
}

/// The finite limits every turn grant carries. Even when the operator configures no dollar budget,
/// these are never infinite (design §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerLimits {
    /// Forwarded upstream requests allowed per outer turn.
    pub max_forwarded_requests: u32,
    /// Locally denied authenticated requests before the turn token is revoked.
    pub max_denied_requests: u32,
    /// Concurrent upstream requests allowed per turn.
    pub max_concurrent_requests: u32,
    /// JSON request bytes allowed for one request.
    pub max_request_bytes: u64,
    /// Aggregate request bytes allowed per outer turn.
    pub max_request_bytes_turn: u64,
    /// Response bytes allowed across one request.
    pub max_response_bytes: u64,
    /// Aggregate response bytes allowed per outer turn.
    pub max_response_bytes_turn: u64,
    /// Requested output tokens allowed per request.
    pub max_output_tokens_request: u64,
    /// Reserved token units allowed per outer turn.
    pub max_reserved_tokens_turn: u64,
    /// Reserved token units allowed per Rhapsody session/run.
    pub max_reserved_tokens_session: u64,
    /// Maximum capability lifetime; the turn's absolute expiry never exceeds the earlier of this and
    /// the adapter's turn deadline (design §4.3).
    pub max_capability_lifetime: Duration,
}

/// The v1 defaults materialized when no explicit limit block is configured (design §8.1).
pub const DEFAULT_BROKER_LIMITS: BrokerLimits = BrokerLimits {
    max_forwarded_requests: 64,
    max_denied_requests: 16,
    max_concurrent_requests: 4,
    max_request_bytes: 8 * 1024 * 1024,
    max_request_bytes_turn: 32 * 1024 * 1024,
    max_response_bytes: 16 * 1024 * 1024,
    max_response_bytes_turn: 64 * 1024 * 1024,
    max_output_tokens_request: 32_000,
    max_reserved_tokens_turn: 1_000_000,
    max_reserved_tokens_session: 20_000_000,
    max_capability_lifetime: Duration::from_secs(60 * 60),
};

/// Compile-time daemon hard ceilings. Raising a limit past one of these is not a configuration
/// action (design §8.1).
pub const HARD_BROKER_LIMITS: BrokerLimits = BrokerLimits {
    max_forwarded_requests: 256,
    max_denied_requests: 64,
    max_concurrent_requests: 8,
    max_request_bytes: 16 * 1024 * 1024,
    max_request_bytes_turn: 128 * 1024 * 1024,
    max_response_bytes: 32 * 1024 * 1024,
    max_response_bytes_turn: 256 * 1024 * 1024,
    max_output_tokens_request: 131_072,
    max_reserved_tokens_turn: 32_000_000,
    max_reserved_tokens_session: 640_000_000,
    max_capability_lifetime: Duration::from_secs(60 * 60),
};

impl BrokerLimits {
    /// Validate the block as a whole. Every refusal names the exact field; the broker never
    /// silently clamps a value to make it fit.
    pub fn validate(&self) -> Result<(), LimitViolation> {
        self.check_positive()?;
        self.check_ceilings()?;
        if self.max_request_bytes > self.max_request_bytes_turn {
            return Err(LimitViolation::PerRequestExceedsAggregate(
                "max_request_bytes",
            ));
        }
        if self.max_response_bytes > self.max_response_bytes_turn {
            return Err(LimitViolation::PerRequestExceedsAggregate(
                "max_response_bytes",
            ));
        }
        if self.max_concurrent_requests > self.max_forwarded_requests {
            return Err(LimitViolation::ConcurrencyExceedsRequests);
        }
        if self.max_reserved_tokens_turn > self.max_reserved_tokens_session {
            return Err(LimitViolation::TurnTokensExceedSession);
        }
        Ok(())
    }

    fn check_positive(&self) -> Result<(), LimitViolation> {
        macro_rules! positive {
            ($($field:ident),+ $(,)?) => {
                $(
                    if self.$field == 0 {
                        return Err(LimitViolation::Zero(stringify!($field)));
                    }
                )+
            };
        }
        positive!(
            max_forwarded_requests,
            max_denied_requests,
            max_concurrent_requests,
            max_request_bytes,
            max_request_bytes_turn,
            max_response_bytes,
            max_response_bytes_turn,
            max_output_tokens_request,
            max_reserved_tokens_turn,
            max_reserved_tokens_session,
        );
        if self.max_capability_lifetime.is_zero() {
            return Err(LimitViolation::Zero("max_capability_lifetime"));
        }
        Ok(())
    }

    fn check_ceilings(&self) -> Result<(), LimitViolation> {
        macro_rules! bounded {
            ($($field:ident),+ $(,)?) => {
                $(
                    if self.$field > HARD_BROKER_LIMITS.$field {
                        return Err(LimitViolation::AboveCeiling(stringify!($field)));
                    }
                )+
            };
        }
        bounded!(
            max_forwarded_requests,
            max_denied_requests,
            max_concurrent_requests,
            max_request_bytes,
            max_request_bytes_turn,
            max_response_bytes,
            max_response_bytes_turn,
            max_output_tokens_request,
            max_reserved_tokens_turn,
            max_reserved_tokens_session,
            max_capability_lifetime,
        );
        Ok(())
    }
}

impl Default for BrokerLimits {
    fn default() -> Self {
        DEFAULT_BROKER_LIMITS
    }
}

/// The session-scoped policy snapshot taken at registration. It is non-secret metadata: later slices
/// may layer an optional durable day-budget authority onto it, which this crate only calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    limits: BrokerLimits,
}

impl SessionPolicy {
    /// Validate and freeze the session's limit block.
    pub fn new(limits: BrokerLimits) -> Result<Self, LimitViolation> {
        limits.validate()?;
        Ok(Self { limits })
    }

    /// The validated limit block.
    pub fn limits(&self) -> &BrokerLimits {
        &self.limits
    }
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            limits: DEFAULT_BROKER_LIMITS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert_eq!(DEFAULT_BROKER_LIMITS.validate(), Ok(()));
    }

    #[test]
    fn zero_limit_is_refused_by_name() {
        let limits = BrokerLimits {
            max_forwarded_requests: 0,
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::Zero("max_forwarded_requests"))
        );
    }

    #[test]
    fn above_ceiling_is_refused() {
        let limits = BrokerLimits {
            max_forwarded_requests: HARD_BROKER_LIMITS.max_forwarded_requests + 1,
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::AboveCeiling("max_forwarded_requests"))
        );
    }

    #[test]
    fn per_request_larger_than_aggregate_is_refused() {
        let limits = BrokerLimits {
            max_request_bytes: 100,
            max_request_bytes_turn: 50,
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::PerRequestExceedsAggregate(
                "max_request_bytes"
            ))
        );
    }

    #[test]
    fn concurrency_above_forwarded_requests_is_refused() {
        let limits = BrokerLimits {
            max_concurrent_requests: 8,
            max_forwarded_requests: 4,
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::ConcurrencyExceedsRequests)
        );
    }

    #[test]
    fn turn_tokens_above_session_cap_is_refused() {
        let limits = BrokerLimits {
            max_reserved_tokens_turn: 2_000_000,
            max_reserved_tokens_session: 1_000_000,
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::TurnTokensExceedSession)
        );
    }

    #[test]
    fn protocol_has_stable_canonical_id() {
        assert_eq!(
            BrokerProtocol::OpenAiChatCompletions.canonical_id(),
            "openai-chat-completions-bearer-v1"
        );
    }
}
