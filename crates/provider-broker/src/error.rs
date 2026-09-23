//! Typed broker refusals.
//!
//! Every failure a caller can act on is a returned [`BrokerError`]; nothing in this crate panics on
//! a production path. Error values carry closed, non-secret context only — no token bytes, no key
//! bytes, no upstream body or auth header (design §13).

use thiserror::Error;

/// Why a candidate API-key value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CredentialRejection {
    /// The v1 API-key value must be non-empty.
    #[error("credential value is empty")]
    Empty,
    /// The value exceeds [`MAX_API_KEY_BYTES`](crate::MAX_API_KEY_BYTES).
    #[error("credential value exceeds the maximum API-key size")]
    TooLong,
    /// The value is not an RFC 6750 `b64token` (`[A-Za-z0-9._~+/-]+=*`, `=` only trailing).
    #[error("credential value is not a valid bearer token")]
    InvalidShape,
}

/// Why a limit block was refused. Every reason is actionable and names the offending field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LimitViolation {
    /// A limit must be strictly positive.
    #[error("broker limit `{0}` must be positive")]
    Zero(&'static str),
    /// A limit exceeds the compile-time daemon hard ceiling.
    #[error("broker limit `{0}` exceeds the compile-time hard ceiling")]
    AboveCeiling(&'static str),
    /// A per-request limit exceeds its per-turn aggregate.
    #[error("per-request limit `{0}` exceeds its per-turn aggregate")]
    PerRequestExceedsAggregate(&'static str),
    /// Concurrency exceeds the forwarded-request count.
    #[error("max_concurrent_requests exceeds max_forwarded_requests")]
    ConcurrencyExceedsRequests,
    /// A per-turn token reservation exceeds the per-session/run cap.
    #[error("max_reserved_tokens_turn exceeds max_reserved_tokens_session")]
    TurnTokensExceedSession,
    /// The optional UTC-day cap is configured but no durable authority was injected. Configuring a
    /// day cap while durable budget storage is unavailable is a refusal, never a best-effort cap.
    #[error("max_reserved_token_units_per_utc_day requires a durable budget authority")]
    DayCapWithoutAuthority,
    /// A durable authority was injected without a configured `max_reserved_token_units_per_utc_day`.
    /// An authority with no cap has no boundary to enforce, so the pairing is refused.
    #[error("a durable budget authority was injected without max_reserved_token_units_per_utc_day")]
    AuthorityWithoutDayCap,
}

/// The broker's typed error. Refusals are values; callers act on the variant, never on a string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BrokerError {
    /// The plan's canonical binding differs from the lease's binding fingerprint.
    #[error("credential binding does not match the registration plan")]
    BindingMismatch,
    /// The candidate credential value was refused before registration.
    #[error("credential rejected: {0}")]
    InvalidCredential(CredentialRejection),
    /// The plan's limits are not a valid bounded block.
    #[error("invalid broker limits: {0}")]
    InvalidLimits(LimitViolation),
    /// A 128-bit session id collided with a live session; the incoming lease was dropped.
    #[error("session id collision")]
    SessionIdCollision,
    /// Eight token-digest candidates all collided; no capability became live.
    #[error("capability digest collision retries exhausted")]
    TokenCollisionExhausted,
    /// The injected random source failed; no capability became live.
    #[error("random source failure")]
    RandomSourceFailure,
    /// A turn was armed while the prior capacity-one receipt was still undrained.
    #[error("the previous turn receipt has not been drained")]
    TurnAlreadyArmed,
    /// The session already has an armed attempt or a live `TurnAccess`.
    #[error("the session already has an armed attempt or live turn")]
    TurnAlreadyActive,
    /// The parent session was revoked.
    #[error("the broker session is revoked")]
    SessionRevoked,
    /// The turn was released or revoked before the attempt could mint a capability.
    ///
    /// Dropping the retained [`TurnReceipt`](crate::TurnReceipt) without draining it is a caller
    /// bug; it revokes the armed turn so a capability can neither be minted nor keep spending.
    #[error("the turn was revoked before a capability could be minted")]
    TurnRevoked,
    /// The presented capability is missing, malformed, unknown, expired, or revoked.
    ///
    /// All of those cases collapse to this one variant on purpose: the design requires that an
    /// unauthenticated caller cannot distinguish token states (design §5.2, §14.1).
    #[error("the turn capability is not valid")]
    Unauthorized,
    /// The per-session/run reserved-token cap is already exhausted.
    #[error("the session token budget is exhausted")]
    SessionBudgetExhausted,
    /// The optional durable UTC-day reserved-token budget is exhausted (or its store was
    /// unreachable). Non-retryable: a caller must not retry the same request.
    #[error("the daily token budget is exhausted")]
    DayBudgetExhausted,
    /// A per-turn limit would be exceeded by the attempted reservation.
    #[error("the turn budget is exhausted: {0}")]
    TurnBudgetExhausted(&'static str),
    /// The move-only turn attempt was already consumed.
    #[error("the turn attempt was already consumed")]
    AttemptConsumed,
    /// The loopback base URL configured on the broker is not a usable capability base.
    #[error("invalid broker base url")]
    InvalidBaseUrl,
    /// A credential binding carried an empty provider id or endpoint.
    #[error("invalid credential binding")]
    InvalidBinding,
    /// The turn's absolute capability expiry had already passed when minting was attempted.
    #[error("the turn capability has expired")]
    TurnExpired,
    /// The registration plan's limit block and the session policy's block disagree.
    #[error("registration plan limits and session policy limits disagree")]
    LimitsMismatch,
    /// The broker's serving task exited unexpectedly and the broker was marked unavailable
    /// (design §11.2, the typed `provider_broker_unavailable` refusal). Registration refuses until
    /// the process restarts; there is no direct-key fallback.
    #[error("the provider broker is unavailable")]
    Unavailable,
}
