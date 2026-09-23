//! rhapsody-provider-broker — the protocol-neutral provider-broker core (PB1), the private
//! OpenAI-compatible loopback adapter (PB2), and the ledger/reservation/budget slice (PB3).
//!
//! This crate is **not** a parity port of a Go package: it realizes the Rhapsody-only
//! provider-broker design (`provider-broker-design.md`, approved 2026-09-19, slices PB1-PB3).
//! PB1 owns the session/turn capability registry, the bound credential-lease contract, the
//! capacity-one turn-receipt ledger, and the atomic reservation primitives. PB2 adds the one private
//! loopback listener, the exact Chat Completions route, the closed request schema, the fixed
//! outbound client with its redirect/proxy/TLS policy, the bounded streaming pipeline, the
//! exact-secret redactor, and the bounded SSE/JSON usage observer. PB3 enforces every finite
//! request/concurrency/byte/output-token/session admission bound atomically, adds the optional
//! durable UTC-day [`CumulativeBudgetAuthority`] contract, settles parsed provider usage exactly
//! once into separate provider-reported/reserved totals, and exposes bounded metrics. It still
//! depends on no `rhapsody-agent`, `rhapsody-orchestrator`, `rhapsody-httpapi`, `rhapsody-config`,
//! `rhapsody-store`, or desktop type, so the security boundary (a harness receives only a bounded
//! per-turn capability, never a reusable provider key) can be tested in isolation.
//!
//! Ownership summary, straight from the binding design (§3.2):
//!
//! * [`Broker`] owns the shared registry; it has no public credential lookup.
//! * [`BrokerSession`] is the opaque, move-only custody handle for one prepared dispatch.
//! * [`BrokerLedgerReceiver`] is the move-only supervisor half; [`BrokerLedgerReceiver::arm_turn`]
//!   synchronously reserves the capacity-one receipt slot *before* any cancellable adapter work.
//! * [`BrokerTurnAttempt`] is the move-only adapter half; it can mint at most one [`TurnAccess`].
//! * [`TurnAccess`] carries the loopback `base_url` and a [`CapabilityToken`]; its `finish`/drop
//!   revoke the grant and finalize the ledger receipt synchronously.
//! * [`TurnReceipt`] is retained by the worker outside the cancellable future and can be taken once
//!   the attempt/access has finished or dropped.
//! * `BrokerListener` is the one private listener: it binds `127.0.0.1:0`, authenticates every
//!   request with the bearer capability, validates the closed schema, forwards exactly once to the
//!   normalized upstream endpoint, and streams the redacted response back (PB2, behind the
//!   `loopback` feature).
//!
//! Secret hygiene is structural: [`CapabilityToken`] and [`BoundCredentialLease`] are non-`Clone`,
//! non-`Serialize`, redact their `Debug`, expose no unrestricted string accessor, and zeroize their
//! primary allocation on drop. Raw capabilities exist only inside the move-only [`TurnAccess`];
//! the registry stores `SHA-256` digests only. The upstream credential is borrowed for exactly one
//! outbound request and its response redactor, never a default header on a reusable client. The
//! honest caveat, recorded in the design and not overstated here: JSON/environment/process libraries
//! can make transient copies Rhapsody cannot prove were wiped — the guarantee is prompt release of
//! Rhapsody's *owned* primary buffer, not allocator-wide forensic erasure.

pub mod authority;
pub mod binding;
pub mod broker;
pub mod clock;
pub mod error;
pub mod ledger;
pub mod metrics;
pub mod policy;
pub mod random;
pub mod secret;
pub mod session;
pub mod turn;
pub mod usage;

// PB2 — the private loopback adapter (listener, upstream client, bounded schema/usage/redaction
// pipeline). Gated behind the `loopback` feature so a PB1-only consumer does not link the HTTP stack.
#[cfg(feature = "loopback")]
pub mod budget;
#[cfg(feature = "loopback")]
pub mod listener;
#[cfg(feature = "loopback")]
pub mod refusal;
#[cfg(feature = "loopback")]
pub mod schema;
#[cfg(feature = "loopback")]
pub mod sse;
#[cfg(feature = "loopback")]
pub mod upstream;

#[cfg(feature = "loopback")]
mod redact;
mod reservations;
mod state;

pub use authority::{CumulativeBudgetAuthority, DayBudgetRefusal, UtcDay};
pub use binding::{
    BindingFingerprint, BoundCredentialLease, CredentialBinding, MAX_API_KEY_BYTES,
    MAX_CREDENTIAL_ENVELOPE_BYTES, validate_api_key_value,
};
pub use broker::{Broker, BrokerRegistration, BrokerRegistrationPlan};
pub use clock::{Clock, ManualClock, MonotonicTime, SystemClock};
pub use error::{BrokerError, CredentialRejection, LimitViolation};
pub use ledger::{TurnLedger, TurnOutcome, UsageAuthority};
pub use metrics::{BrokerMetrics, BrokerMetricsSnapshot};
pub use policy::{
    BrokerLimits, BrokerProtocol, DEFAULT_BROKER_LIMITS, HARD_BROKER_LIMITS, SessionPolicy,
};
pub use random::{OsRandom, RandomError, RandomSource, ScriptedRandom};
pub use reservations::ConcurrencyPermit;
pub use secret::{CapabilityToken, ZeroizingBytes};
pub use session::{BrokerLedgerReceiver, BrokerSession};
pub use turn::{BrokerTurnAttempt, CapabilityGrant, TurnAccess, TurnMeta, TurnReceipt};
pub use usage::UsageObservation;

#[cfg(feature = "loopback")]
pub use budget::{WeightedBudget, WeightedGuard};
#[cfg(feature = "loopback")]
pub use listener::{
    BrokerListener, HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_REQUESTS_PER_CONNECTION,
};
#[cfg(feature = "loopback")]
pub use redact::{REDACTION_MARKER, StreamingRedactor};
#[cfg(feature = "loopback")]
pub use refusal::PolicyRefusal;
#[cfg(feature = "loopback")]
pub use schema::{
    ChatRequest, ChatRequestPolicy, RequestRejection, SchemaError, top_level_field_allowed,
    validate_chat_request,
};
#[cfg(feature = "loopback")]
pub use sse::{MAX_USAGE_JSON_BYTES, SseUsageObserver};
#[cfg(feature = "loopback")]
pub use upstream::{
    EndpointError, NormalizedEndpoint, RHAPSODY_USER_AGENT, UpstreamClient, UpstreamError,
};
