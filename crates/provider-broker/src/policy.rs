//! Protocol-neutral broker policy.
//!
//! [`BrokerLimits`] is the finite, always-on limit block every turn grant carries (design §8.1),
//! with the v1 defaults and compile-time hard ceilings as constants. [`BrokerProtocol`] is the
//! normalized adapter axis (v1: OpenAI Chat Completions only). Nothing here is negotiated with the
//! harness: the block is validated at registration and can only be tightened per provider.

use std::sync::Arc;
use std::time::Duration;

use crate::authority::CumulativeBudgetAuthority;
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
    /// Locally denied authenticated requests before the turn token is revoked. A refusal caused by
    /// broker-wide budget contention — another turn briefly exhausting the shared request-memory or
    /// buffered-response budget — counts the same as one the child caused (see `listener::deny`).
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
    /// Optional reserved token units allowed per UTC day. `None` (the default) means no Rhapsody
    /// daily cap and UI/docs must say so; there is no permissive implicit default. When `Some`, the
    /// daemon must inject a [`CumulativeBudgetAuthority`] into the session policy.
    pub max_reserved_token_units_per_utc_day: Option<u64>,
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
    // Absent means no Rhapsody daily cap; the operator opts in with a checked positive value.
    max_reserved_token_units_per_utc_day: None,
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
    // The optional daily value is a checked positive `u64` and may deliberately be lower than one
    // run cap; it has no compile-time hard ceiling, so `None` here means "not bounded by the block".
    max_reserved_token_units_per_utc_day: None,
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
        // The optional daily value is a checked positive `u64`; absent is the only way to say
        // "no daily cap". `Some(0)` is a refusal, never a silent unlock.
        if self.max_reserved_token_units_per_utc_day == Some(0) {
            return Err(LimitViolation::Zero("max_reserved_token_units_per_utc_day"));
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

/// The session-scoped policy snapshot taken at registration. It is non-secret metadata carrying the
/// validated limit block and, when the optional UTC-day cap is configured, the injected durable
/// [`CumulativeBudgetAuthority`] this crate only calls.
#[derive(Debug, Clone)]
pub struct SessionPolicy {
    limits: BrokerLimits,
    day_authority: Option<Arc<dyn CumulativeBudgetAuthority>>,
}

impl SessionPolicy {
    /// Validate and freeze the session's limit block. Refuses when the optional UTC-day cap is set
    /// without a durable authority (a configured cap must be backed by storage).
    pub fn new(limits: BrokerLimits) -> Result<Self, LimitViolation> {
        Self::build(limits, None)
    }

    /// Validate and freeze the session's limit block together with the injected durable day-budget
    /// authority. Refuses when either is present without the other, so a day cap is never
    /// configured without storage and an authority never exists with no cap to enforce.
    pub fn with_day_authority(
        limits: BrokerLimits,
        authority: Arc<dyn CumulativeBudgetAuthority>,
    ) -> Result<Self, LimitViolation> {
        Self::build(limits, Some(authority))
    }

    fn build(
        limits: BrokerLimits,
        day_authority: Option<Arc<dyn CumulativeBudgetAuthority>>,
    ) -> Result<Self, LimitViolation> {
        limits.validate()?;
        match (
            limits.max_reserved_token_units_per_utc_day,
            day_authority.as_ref(),
        ) {
            (Some(_), None) => Err(LimitViolation::DayCapWithoutAuthority),
            (None, Some(_)) => Err(LimitViolation::AuthorityWithoutDayCap),
            _ => Ok(Self {
                limits,
                day_authority,
            }),
        }
    }

    /// The validated limit block.
    pub fn limits(&self) -> &BrokerLimits {
        &self.limits
    }

    /// The injected durable day-budget authority, present exactly when the UTC-day cap is set.
    pub fn day_authority(&self) -> Option<&Arc<dyn CumulativeBudgetAuthority>> {
        self.day_authority.as_ref()
    }
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            limits: DEFAULT_BROKER_LIMITS,
            day_authority: None,
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

    /// Pin the exact §8.1 default and hard-ceiling table. A drift in any number (in particular the
    /// 20M default / 640M hard ceiling for the session/run cap) must redden this test rather than
    /// quietly change what a grant enforces.
    #[test]
    fn the_default_and_hard_limit_blocks_materialize_the_design_table() {
        assert_eq!(
            DEFAULT_BROKER_LIMITS,
            BrokerLimits {
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
                max_reserved_token_units_per_utc_day: None,
                max_capability_lifetime: Duration::from_secs(60 * 60),
            }
        );
        assert_eq!(
            HARD_BROKER_LIMITS,
            BrokerLimits {
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
                max_reserved_token_units_per_utc_day: None,
                max_capability_lifetime: Duration::from_secs(60 * 60),
            }
        );
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

    #[test]
    fn the_optional_daily_cap_has_no_implicit_default() {
        assert_eq!(
            DEFAULT_BROKER_LIMITS.max_reserved_token_units_per_utc_day,
            None
        );
        let policy = SessionPolicy::default();
        assert!(policy.day_authority().is_none());
        assert_eq!(policy.limits().max_reserved_token_units_per_utc_day, None);
    }

    #[test]
    fn a_zero_daily_cap_is_refused_by_name() {
        let limits = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(0),
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            limits.validate(),
            Err(LimitViolation::Zero("max_reserved_token_units_per_utc_day"))
        );
    }

    #[test]
    fn a_daily_cap_may_be_lower_than_one_run_cap() {
        // Deliberately lower than `max_reserved_tokens_session`: the design allows it.
        let limits = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(1),
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(limits.validate(), Ok(()));
    }

    #[test]
    fn a_daily_cap_without_a_durable_authority_is_refused() {
        let limits = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(1_000),
            ..DEFAULT_BROKER_LIMITS
        };
        assert_eq!(
            SessionPolicy::new(limits).unwrap_err(),
            LimitViolation::DayCapWithoutAuthority
        );
    }

    #[test]
    fn a_durable_authority_without_a_daily_cap_is_refused() {
        let authority = Arc::new(super::tests::FakeAuthority::new());
        assert_eq!(
            SessionPolicy::with_day_authority(DEFAULT_BROKER_LIMITS, authority).unwrap_err(),
            LimitViolation::AuthorityWithoutDayCap
        );
    }

    /// A minimal in-memory authority used only to pin the policy pairing rules.
    #[derive(Debug)]
    struct FakeAuthority;

    impl FakeAuthority {
        fn new() -> Self {
            FakeAuthority
        }
    }

    impl CumulativeBudgetAuthority for FakeAuthority {
        fn try_charge(
            &self,
            _provider_id: &str,
            _tokens: u64,
            _cap: u64,
        ) -> Result<(), crate::authority::DayBudgetRefusal> {
            Ok(())
        }

        fn charged_today(&self, _provider_id: &str) -> u64 {
            0
        }
    }

    #[test]
    fn a_daily_cap_with_a_durable_authority_validates() {
        let limits = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(1_000),
            ..DEFAULT_BROKER_LIMITS
        };
        let policy = SessionPolicy::with_day_authority(limits, Arc::new(FakeAuthority::new()))
            .expect("policy");
        assert!(policy.day_authority().is_some());
    }
}
