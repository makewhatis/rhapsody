//! Non-secret and secret-bearing domain types shared by the desktop credential owner and the
//! daemon-side reader (design record `provider-auth-design.md` §2.4/§2.5, ticket STUDIO-981).
//!
//! Every type here is safe to depend on from both sides of the IPC boundary: the non-secret types
//! (`CredentialRef`, `Binding`, `Revision`, the state tags) derive `Debug`/`Serialize`/`Deserialize`
//! freely, while [`BoundCredentialLease`] hand-writes `Debug` to redact and is neither `Clone` nor
//! `Copy`, nor `Display`/`Serialize`/`Deref`/`AsRef`/`Borrow` (see the crate's compile guards), so a
//! lease can only ever be moved — never silently duplicated or read out through an ordinary getter.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// The Keychain service namespace for provider credentials — deliberately distinct from
/// `desktop::credential::DEFAULT_SERVICE` (Linear's `is.makewhat.rhapsody`), so a provider
/// definition can never resolve to the Linear item even if it names the same account string.
pub const PROVIDER_SERVICE: &str = "is.makewhat.rhapsody.providers";

/// Identifies which provider credential is being asked for. The account string is derived and
/// validated (never taken verbatim from YAML) — see [`CredentialRef::for_provider`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef {
    account: String,
}

/// A provider ID is invalid as a credential reference input (§2.4: "Neither string is accepted
/// from YAML" — this is the validation gate in front of that rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidProviderId(pub String);

impl fmt::Display for InvalidProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid provider id for a credential reference: {:?}",
            self.0
        )
    }
}

impl std::error::Error for InvalidProviderId {}

impl CredentialRef {
    /// Derives the versioned, validated account name (`v1:<provider_id>`) from a canonical
    /// provider ID. Rejects anything that is not a short lowercase slug so a provider definition
    /// can never point the Keychain lookup at an arbitrary account string (e.g. `linear-api-token`).
    pub fn for_provider(provider_id: &str) -> Result<CredentialRef, InvalidProviderId> {
        let valid = !provider_id.is_empty()
            && provider_id.len() <= 64
            && provider_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            && provider_id.as_bytes()[0].is_ascii_lowercase();
        if !valid {
            return Err(InvalidProviderId(provider_id.to_string()));
        }
        Ok(CredentialRef {
            account: format!("v1:{provider_id}"),
        })
    }

    /// The derived Keychain account string (e.g. `v1:anthropic`). Not a secret — safe to log.
    pub fn account(&self) -> &str {
        &self.account
    }
}

/// The canonical binding a stored credential is pinned to (§2.4): the exact normalized endpoint
/// and the versioned protocol/auth adapter. Only an explicit Rebind may change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub provider_id: String,
    pub adapter: String,
    pub base_url: String,
}

/// An opaque, non-secret owner-state generation counter (§2.5). It advances on every successful
/// Connect/Replace/Rebind/Remove and on owner availability/authorization transitions, and it is
/// carried by every `CredentialRead` — including `Absent`, `DeniedOrLocked`, and `OwnerUnavailable`
/// — so a caller can always tell whether the owner state has moved since it last looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision(pub u64);

impl Revision {
    pub const INITIAL: Revision = Revision(0);

    pub fn next(self) -> Revision {
        Revision(self.0 + 1)
    }
}

/// The canonical versioned adapter label for the one v1 protocol (`provider-auth-design.md` §2.4).
/// A stored envelope's binding carries this exact string; every other value is unknown and has no
/// protocol, so a lease bound to one can never be moved into the broker.
pub const OPENAI_CHAT_COMPLETIONS_BEARER_V1: &str = "openai-chat-completions-bearer-v1";

/// Maps a versioned adapter label to the broker's protocol axis. Anything but the one reviewed v1
/// adapter returns `None`, which the broker-lease conversion turns into [`rhapsody_provider_broker::BrokerError::InvalidBinding`]
/// — a new protocol must be a new adapter ID and an explicit Rebind, never a widened match here.
fn broker_protocol_for_adapter(adapter: &str) -> Option<rhapsody_provider_broker::BrokerProtocol> {
    match adapter {
        OPENAI_CHAT_COMPLETIONS_BEARER_V1 => {
            Some(rhapsody_provider_broker::BrokerProtocol::OpenAiChatCompletions)
        }
        _ => None,
    }
}

/// The credential value plus its bound endpoint, held only long enough to build a broker session.
/// Not `Clone`/`Copy` — it can be moved but never duplicated — and `Debug` never prints `value`.
/// `Drop` zeroizes the value bytes.
///
/// There is deliberately **no** ordinary string accessor (`expose_secret`/`Deref`/`AsRef`) and no
/// `Serialize`: the value cannot be read out through a `&self` getter and cannot be written to a
/// serialization surface. The two ways a value legitimately moves onward are both *consuming*,
/// purpose-specific transfers that take `self` by value:
/// [`into_broker_lease`](Self::into_broker_lease) (a move into PB1's protocol-neutral, move-only
/// lease) and [`into_lease_payload`](Self::into_lease_payload) (the IPC hop's wire form). Because
/// both consume the lease, neither can be called twice on a live lease to mint repeated owned copies.
pub struct BoundCredentialLease {
    pub binding: Binding,
    value: String,
}

impl BoundCredentialLease {
    pub fn new(binding: Binding, value: String) -> BoundCredentialLease {
        BoundCredentialLease { binding, value }
    }

    /// Zeroize the primary value buffer in place, preserving its length. `Drop` calls exactly this,
    /// so a test can call it and observe the wipe without reading freed memory.
    fn wipe(&mut self) {
        // `String`'s own `Zeroize` impl routes through `Vec<u8>`, whose impl zeroes the initialized
        // elements and then *clears* the vector (`zeroize-1.9.0` `Vec<Z>::zeroize`). That leaves an
        // empty string, so any `bytes().all(|b| b == 0)` assertion over it is vacuously true and a
        // mutation that merely `clear()`s would pass. `zeroize`'s `str` impl (safe) writes NULs into
        // the fixed-length slice instead, so every original byte becomes a NUL and the wipe is
        // observable at the value's real size — no `unsafe` needed (alice's review of rhapsody#221).
        self.value.as_mut_str().zeroize();
    }

    /// Consume the lease and move its value into PB1's protocol-neutral, move-only
    /// [`BoundCredentialLease`](rhapsody_provider_broker::BoundCredentialLease) — the "move into the
    /// broker" path. The broker binding is **derived from this lease's own binding**, never accepted
    /// from the caller: a lease read under binding A can therefore never become a broker lease for
    /// binding B, which is exactly the boundary the bound lease exists to preserve. The value is moved
    /// straight into the broker's own zeroizing buffer, which validates the API-key shape on the way
    /// in; this lease's buffer is emptied first, so the secret is never duplicated in two live
    /// allocations.
    ///
    /// An unknown adapter (no protocol axis) or an empty provider id/endpoint is a typed
    /// [`rhapsody_provider_broker::BrokerError::InvalidBinding`]; a value that violates the broker's shape bound is
    /// [`rhapsody_provider_broker::BrokerError::InvalidCredential`]. On either refusal this lease drops and wipes its value.
    pub fn into_broker_lease(
        mut self,
    ) -> Result<rhapsody_provider_broker::BoundCredentialLease, rhapsody_provider_broker::BrokerError>
    {
        let protocol = broker_protocol_for_adapter(&self.binding.adapter)
            .ok_or(rhapsody_provider_broker::BrokerError::InvalidBinding)?;
        let binding = rhapsody_provider_broker::CredentialBinding::new(
            self.binding.provider_id.as_str(),
            protocol,
            self.binding.base_url.as_str(),
        )?;
        let value = std::mem::take(&mut self.value);
        rhapsody_provider_broker::BoundCredentialLease::new(binding, value)
    }

    /// The one purpose-specific transfer for the IPC hop: consume the lease and produce the wire
    /// payload the desktop side sends to the daemon. It is deliberately *consuming* rather than a
    /// `&self` accessor, so there is no way to hold a live lease and repeatedly read the value out;
    /// the allocation itself is moved into the payload, not copied.
    pub fn into_lease_payload(mut self) -> crate::wire::LeasePayload {
        crate::wire::LeasePayload {
            binding: self.binding.clone(),
            value: std::mem::take(&mut self.value),
        }
    }
}

impl Drop for BoundCredentialLease {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for BoundCredentialLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundCredentialLease")
            .field("binding", &self.binding)
            .field("value", &"***")
            .finish()
    }
}

/// The outcome of an owner read (§2.5). `Present` is the only variant carrying a lease, and only
/// ever when the caller's expected binding matched the stored one exactly. The non-secret variants
/// carry no diagnostic detail beyond their tag — `DeniedOrLocked`/`Malformed`/`BindingMismatch` must
/// never leak the stored endpoint/binding or key on a mismatch.
#[derive(Debug)]
pub enum CredentialState {
    Present(BoundCredentialLease),
    Absent,
    DeniedOrLocked,
    Malformed,
    BindingMismatch,
    OwnerUnavailable,
    OwnerUnauthorized,
}

impl CredentialState {
    /// The wire tag for this state — used by the IPC layer, which never serializes the lease value
    /// itself (a `Present` response carries the value in a separate, explicitly-named field).
    pub fn tag(&self) -> CredentialStateTag {
        match self {
            CredentialState::Present(_) => CredentialStateTag::Present,
            CredentialState::Absent => CredentialStateTag::Absent,
            CredentialState::DeniedOrLocked => CredentialStateTag::DeniedOrLocked,
            CredentialState::Malformed => CredentialStateTag::Malformed,
            CredentialState::BindingMismatch => CredentialStateTag::BindingMismatch,
            CredentialState::OwnerUnavailable => CredentialStateTag::OwnerUnavailable,
            CredentialState::OwnerUnauthorized => CredentialStateTag::OwnerUnauthorized,
        }
    }
}

/// The non-secret tag half of [`CredentialState`], safe to serialize/log/derive `Debug` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialStateTag {
    Present,
    Absent,
    DeniedOrLocked,
    Malformed,
    BindingMismatch,
    OwnerUnavailable,
    OwnerUnauthorized,
}

/// One atomic owner snapshot (§2.5): a caller can never observe a new revision paired with an old
/// secret, or vice versa, because both are read together under the owner's single lock.
#[derive(Debug)]
pub struct CredentialRead {
    pub revision: Revision,
    pub state: CredentialState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ref_derives_versioned_account() {
        let r = CredentialRef::for_provider("anthropic").expect("valid id");
        assert_eq!(r.account(), "v1:anthropic");
    }

    #[test]
    fn provider_ref_rejects_malformed_ids() {
        assert!(CredentialRef::for_provider("").is_err());
        assert!(
            CredentialRef::for_provider("Anthropic").is_err(),
            "uppercase rejected"
        );
        assert!(CredentialRef::for_provider("has space").is_err());
        assert!(CredentialRef::for_provider("-leading-dash").is_err());
        assert!(
            CredentialRef::for_provider(&"a".repeat(65)).is_err(),
            "over length cap"
        );
    }

    // The exact attack the design calls out: a provider definition naming `linear-api-token` to
    // try to point the provider lookup at the Linear credential. Even though that string is a
    // syntactically valid slug, the derived account can never collide with Linear's raw
    // `linear-api-token` account because it always carries the `v1:` prefix — and in production
    // the two live under entirely different Keychain *service* strings (`is.makewhat.rhapsody` for
    // Linear vs `is.makewhat.rhapsody.providers` for providers), so this can never resolve to the
    // same item even before the prefix is considered.
    #[test]
    fn provider_ref_can_never_collide_with_the_linear_account_string() {
        let r = CredentialRef::for_provider("linear-api-token").expect("syntactically valid slug");
        assert_ne!(r.account(), "linear-api-token");
        assert_eq!(r.account(), "v1:linear-api-token");
    }

    #[test]
    fn revision_advances_monotonically() {
        let r = Revision::INITIAL;
        assert_eq!(r.next(), Revision(1));
        assert!(r.next() > r);
    }

    // The redaction contract every new secret-bearing type in this ticket must uphold (design
    // §2.5: "No credential value may appear in Debug, Display, ... or error strings").
    #[test]
    fn bound_credential_lease_debug_redacts_the_value() {
        let lease = BoundCredentialLease::new(
            Binding {
                provider_id: "fireworks".into(),
                adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
                base_url: "https://api.fireworks.ai/inference/v1".into(),
            },
            "sk-super-secret-value".to_string(),
        );
        let debug = format!("{lease:?}");
        assert!(
            !debug.contains("sk-super-secret-value"),
            "Debug leaked: {debug}"
        );
        // The only way to read the value is a consuming transfer; this one is the wire payload.
        let payload = lease.into_lease_payload();
        assert_eq!(payload.value, "sk-super-secret-value");
    }

    // The "move into the broker" contract (P1 acceptance): the lease's value moves into PB1's
    // move-only lease with no intermediate `String` and no ordinary accessor, and the broker binding
    // is DERIVED from the lease's own binding rather than accepted from the caller — so a lease read
    // under binding A can never become a broker lease for binding B (sol's review of rhapsody#221).
    #[test]
    fn a_lease_moves_into_the_broker_lease_under_its_own_binding() {
        let lease = BoundCredentialLease::new(
            Binding {
                provider_id: "fireworks".into(),
                adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
                base_url: "https://api.fireworks.ai/inference/v1".into(),
            },
            "sk-fake-key".to_string(),
        );
        let broker_lease = lease.into_broker_lease().expect("move into broker");
        // PB1's lease has no key accessor, so the observable here is the DERIVED binding.
        assert_eq!(broker_lease.binding().provider_id(), "fireworks");
        assert_eq!(
            broker_lease.binding().protocol(),
            rhapsody_provider_broker::BrokerProtocol::OpenAiChatCompletions
        );
        assert_eq!(
            broker_lease.binding().normalized_endpoint(),
            "https://api.fireworks.ai/inference/v1"
        );
    }

    // An adapter without a broker protocol axis cannot be moved into the broker at all — the exact
    // "adding another auth scheme or route requires a new adapter ID and an explicit Rebind"
    // boundary (§2.4). There is no caller-supplied binding to retarget, so a mismatched endpoint or
    // provider is impossible by construction; an unlabeled adapter is the remaining refusal.
    #[test]
    fn an_unknown_adapter_cannot_move_into_the_broker() {
        let lease = BoundCredentialLease::new(
            Binding {
                provider_id: "fireworks".into(),
                adapter: "some-future-adapter-v2".into(),
                base_url: "https://api.fireworks.ai/inference/v1".into(),
            },
            "sk-fake-key".to_string(),
        );
        assert_eq!(
            lease.into_broker_lease().err(),
            Some(rhapsody_provider_broker::BrokerError::InvalidBinding)
        );
    }

    // Pins the mechanism `Drop` uses: the primary buffer is wiped in place, preserving its length so
    // the assertion observes every original byte (a vector-length-losing `clear()` on the mutation
    // `String::zeroize` performs must turn this red — that was alice's review of rhapsody#221).
    #[test]
    fn the_lease_wipes_its_primary_buffer() {
        let secret = "sk-super-secret";
        let mut lease = BoundCredentialLease::new(
            Binding {
                provider_id: "p".into(),
                adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
                base_url: "https://x/v1".into(),
            },
            secret.to_string(),
        );
        lease.wipe();
        assert_eq!(
            lease.value.len(),
            secret.len(),
            "the wipe must not shorten the buffer, or the byte check below is vacuous"
        );
        assert!(
            lease.value.bytes().all(|b| b == 0),
            "the primary buffer must be zeroed in place"
        );
    }

    #[test]
    fn moving_a_malformed_value_into_the_broker_is_refused() {
        let lease = BoundCredentialLease::new(
            Binding {
                provider_id: "p".into(),
                adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
                base_url: "https://x/v1".into(),
            },
            "has a space".to_string(),
        );
        assert!(lease.into_broker_lease().is_err());
    }

    #[test]
    fn credential_state_tag_never_carries_the_lease() {
        let present = CredentialState::Present(BoundCredentialLease::new(
            Binding {
                provider_id: "p".into(),
                adapter: "a".into(),
                base_url: "https://example".into(),
            },
            "secret".into(),
        ));
        assert_eq!(present.tag(), CredentialStateTag::Present);
        // The tag type itself has no field that could hold a value — this compiles only because
        // CredentialStateTag is a plain enum of unit variants, which is the property under test.
        let serialized = serde_json::to_string(&present.tag()).expect("tag serializes");
        assert!(!serialized.to_lowercase().contains("secret"));
    }
}
