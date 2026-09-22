//! rhapsody-provider-broker — the protocol-neutral provider-broker core (PB1).
//!
//! This crate is **not** a parity port of a Go package: it realizes the Rhapsody-only
//! provider-broker design (`provider-broker-design.md`, approved 2026-09-19, slice PB1). It owns
//! the session/turn capability registry, the bound credential-lease contract, the capacity-one
//! turn-receipt ledger, and the atomic reservation primitives that later slices build on. It has
//! no HTTP/Axum listener yet and deliberately depends on no `rhapsody-agent`, `rhapsody-orchestrator`,
//! `rhapsody-httpapi`, or desktop type, so the security boundary (a harness receives only a bounded
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
//!
//! Secret hygiene is structural: [`CapabilityToken`] and [`BoundCredentialLease`] are non-`Clone`,
//! non-`Serialize`, redact their `Debug`, expose no unrestricted string accessor, and zeroize their
//! primary allocation on drop. Raw capabilities exist only inside the move-only [`TurnAccess`];
//! the registry stores `SHA-256` digests only. The honest caveat, recorded in the design and not
//! overstated here: JSON/environment/process libraries can make transient copies Rhapsody cannot
//! prove were wiped — the guarantee is prompt release of Rhapsody's *owned* primary buffer, not
//! allocator-wide forensic erasure.

pub mod binding;
pub mod broker;
pub mod clock;
pub mod error;
pub mod ledger;
pub mod policy;
pub mod random;
pub mod secret;
pub mod session;
pub mod turn;

mod reservations;
mod state;

pub use binding::{
    BindingFingerprint, BoundCredentialLease, CredentialBinding, MAX_API_KEY_BYTES,
    MAX_CREDENTIAL_ENVELOPE_BYTES,
};
pub use broker::{Broker, BrokerRegistration, BrokerRegistrationPlan};
pub use clock::{Clock, ManualClock, MonotonicTime, SystemClock};
pub use error::{BrokerError, CredentialRejection, LimitViolation};
pub use ledger::{TurnLedger, TurnOutcome};
pub use policy::{
    BrokerLimits, BrokerProtocol, DEFAULT_BROKER_LIMITS, HARD_BROKER_LIMITS, SessionPolicy,
};
pub use random::{OsRandom, RandomError, RandomSource, ScriptedRandom};
pub use reservations::ConcurrencyPermit;
pub use secret::{CapabilityToken, ZeroizingBytes};
pub use session::{BrokerLedgerReceiver, BrokerSession};
pub use turn::{BrokerTurnAttempt, CapabilityGrant, TurnAccess, TurnMeta, TurnReceipt};
