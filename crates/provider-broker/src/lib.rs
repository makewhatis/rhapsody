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
//! * [`BrokerListener`] is the one private listener: it binds `127.0.0.1:0`, authenticates every
//!   request with the bearer capability, validates the closed schema, forwards exactly once to the
//!   normalized upstream endpoint, and streams the redacted response back (PB2).
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
pub mod budget;
pub mod clock;
pub mod error;
pub mod ledger;
pub mod listener;
pub mod metrics;
pub mod policy;
pub mod random;
pub mod refusal;
pub mod schema;
pub mod secret;
pub mod session;
pub mod sse;
pub mod turn;
pub mod upstream;

mod redact;
mod reservations;
mod state;

pub use authority::{CumulativeBudgetAuthority, DayBudgetRefusal, UtcDay};
pub use binding::{
    BindingFingerprint, BoundCredentialLease, CredentialBinding, MAX_API_KEY_BYTES,
    MAX_CREDENTIAL_ENVELOPE_BYTES, validate_api_key_value,
};
pub use broker::{Broker, BrokerRegistration, BrokerRegistrationPlan};
pub use budget::{WeightedBudget, WeightedGuard};
pub use clock::{Clock, ManualClock, MonotonicTime, SystemClock};
pub use error::{BrokerError, CredentialRejection, LimitViolation};
pub use ledger::{TurnLedger, TurnOutcome, UsageAuthority};
pub use listener::{
    BrokerListener, HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_REQUESTS_PER_CONNECTION,
};
pub use metrics::{BrokerMetrics, BrokerMetricsSnapshot};
pub use policy::{
    BrokerLimits, BrokerProtocol, DEFAULT_BROKER_LIMITS, HARD_BROKER_LIMITS, SessionPolicy,
};
pub use random::{OsRandom, RandomError, RandomSource, ScriptedRandom};
pub use redact::{REDACTION_MARKER, StreamingRedactor};
pub use refusal::PolicyRefusal;
pub use reservations::ConcurrencyPermit;
pub use schema::{
    ChatRequest, ChatRequestPolicy, RequestRejection, SchemaError, top_level_field_allowed,
    validate_chat_request,
};
pub use secret::{CapabilityToken, ZeroizingBytes};
pub use session::{BrokerLedgerReceiver, BrokerSession};
pub use sse::{SseUsageObserver, UsageObservation};
pub use turn::{BrokerTurnAttempt, CapabilityGrant, TurnAccess, TurnMeta, TurnReceipt};
pub use upstream::{
    EndpointError, NormalizedEndpoint, RHAPSODY_USER_AGENT, UpstreamClient, UpstreamError,
};
