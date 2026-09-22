//! Non-secret and secret-bearing domain types shared by the desktop credential owner and the
//! daemon-side reader (design record `provider-auth-design.md` §2.4/§2.5, ticket STUDIO-981).
//!
//! Every type here is safe to depend on from both sides of the IPC boundary: the non-secret types
//! (`CredentialRef`, `Binding`, `Revision`, the state tags) derive `Debug`/`Serialize`/`Deserialize`
//! freely, while [`BoundCredentialLease`] hand-writes `Debug`/`Display` to redact and is neither
//! `Clone` nor `Copy` so a lease can only ever be moved, never silently duplicated.

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

/// The credential value plus its bound endpoint, held only long enough to build a broker session.
/// Not `Clone`/`Copy` — it can be moved but never duplicated — and `Debug`/`Display` never print
/// `value`. `Drop` zeroizes the value bytes.
pub struct BoundCredentialLease {
    pub binding: Binding,
    value: String,
}

impl BoundCredentialLease {
    pub fn new(binding: Binding, value: String) -> BoundCredentialLease {
        BoundCredentialLease { binding, value }
    }

    /// The raw secret. Named distinctly from a `Deref`/`AsRef` impl so a future `{:?}`/`{}` on the
    /// lease itself can never reach it by accident — only an explicit call does.
    pub fn expose_secret(&self) -> &str {
        &self.value
    }
}

impl Drop for BoundCredentialLease {
    fn drop(&mut self) {
        self.value.zeroize();
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

impl fmt::Display for BoundCredentialLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundCredentialLease(***)")
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
    fn bound_credential_lease_debug_and_display_redact_the_value() {
        let lease = BoundCredentialLease::new(
            Binding {
                provider_id: "fireworks".into(),
                adapter: "openai-chat-completions-bearer-v1".into(),
                base_url: "https://api.fireworks.ai/inference/v1".into(),
            },
            "sk-super-secret-value".to_string(),
        );
        let debug = format!("{lease:?}");
        let display = format!("{lease}");
        assert!(
            !debug.contains("sk-super-secret-value"),
            "Debug leaked: {debug}"
        );
        assert!(
            !display.contains("sk-super-secret-value"),
            "Display leaked: {display}"
        );
        assert_eq!(lease.expose_secret(), "sk-super-secret-value");
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
