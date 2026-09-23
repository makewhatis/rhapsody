//! The bound credential lease and its non-secret binding fingerprint.
//!
//! PB1 defines the protocol-neutral, move-only [`BoundCredentialLease`] contract (design §3.2). The
//! selected credential-owner adapter (a later slice) is the only production path that constructs a
//! lease from owner-held bytes; tests inject fake leases through [`BoundCredentialLease::new`]. The
//! broker never reads a Keychain or IPC backend.
//!
//! Registration compares the plan's derived binding fingerprint with the lease's fingerprint before
//! taking custody. A fingerprint is non-secret and has no serialization surface; the lease is
//! move-only and has no key accessor.

use std::fmt;

use sha2::{Digest, Sha256};

use crate::error::{BrokerError, CredentialRejection};
use crate::policy::BrokerProtocol;
use crate::secret::ZeroizingBytes;

/// The v1 API-key value bound (design §3.2). It also caps the streaming redactor's look-behind.
pub const MAX_API_KEY_BYTES: usize = 8 * 1024;

/// The independently-capped serialized credential envelope the owner hands to the daemon.
pub const MAX_CREDENTIAL_ENVELOPE_BYTES: usize = 64 * 1024;

const BINDING_DOMAIN: &[u8] = b"rhapsody-provider-binding-v1\0";

/// The canonical, non-secret identity a stored credential is bound to: the provider id, the
/// protocol/auth adapter version, and the normalized base URL. Model and budget changes never
/// rebind; an endpoint/protocol change requires an explicit Rebind (design §1.3, §12).
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialBinding {
    provider_id: String,
    protocol: BrokerProtocol,
    normalized_endpoint: String,
}

impl CredentialBinding {
    /// Build a canonical binding. Empty provider id or endpoint is refused.
    pub fn new(
        provider_id: impl Into<String>,
        protocol: BrokerProtocol,
        normalized_endpoint: impl Into<String>,
    ) -> Result<Self, BrokerError> {
        let provider_id = provider_id.into();
        let normalized_endpoint = normalized_endpoint.into();
        if provider_id.is_empty() || normalized_endpoint.is_empty() {
            return Err(BrokerError::InvalidBinding);
        }
        Ok(Self {
            provider_id,
            protocol,
            normalized_endpoint,
        })
    }

    /// The stable provider id (provenance/diagnostics only).
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The normalized protocol.
    pub fn protocol(&self) -> BrokerProtocol {
        self.protocol
    }

    /// The normalized base URL.
    pub fn normalized_endpoint(&self) -> &str {
        &self.normalized_endpoint
    }

    /// The non-secret fingerprint registration compares against the lease.
    pub fn fingerprint(&self) -> BindingFingerprint {
        let mut hasher = Sha256::new();
        hasher.update(BINDING_DOMAIN);
        hasher.update(self.provider_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.protocol.canonical_id().as_bytes());
        hasher.update([0u8]);
        hasher.update(self.normalized_endpoint.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        BindingFingerprint(digest)
    }
}

impl fmt::Debug for CredentialBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialBinding")
            .field("provider_id", &self.provider_id)
            .field("protocol", &self.protocol)
            .field("normalized_endpoint", &"<redacted>")
            .finish()
    }
}

/// A domain-separated SHA-256 of a [`CredentialBinding`]. Non-secret; it has **no** public byte or
/// serialization surface, and its `Debug` does not reveal the digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindingFingerprint([u8; 32]);

impl BindingFingerprint {
    /// Compare against a stored fingerprint. This is the only public operation.
    pub fn matches(&self, other: &BindingFingerprint) -> bool {
        self.0 == other.0
    }
}

impl fmt::Debug for BindingFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<non-secret binding fingerprint>")
    }
}

/// The move-only bound credential lease.
///
/// Owns the primary credential allocation, which zeroizes on drop. It has no key accessor: the one
/// adapter that eventually needs the bytes borrows them only while constructing the upstream
/// request, through a crate-internal scope.
pub struct BoundCredentialLease {
    binding: CredentialBinding,
    fingerprint: BindingFingerprint,
    secret: ZeroizingBytes,
}

impl BoundCredentialLease {
    /// Construct a lease from an already-bound value, validating the v1 API-key shape first.
    ///
    /// This is the test/fake seam; the production credential-owner adapter also calls it with
    /// owner-held bytes.
    pub fn new(binding: CredentialBinding, value: impl Into<Vec<u8>>) -> Result<Self, BrokerError> {
        let secret = ZeroizingBytes::new(value.into());
        validate_api_key_value(secret.as_slice()).map_err(BrokerError::InvalidCredential)?;
        let fingerprint = binding.fingerprint();
        Ok(Self {
            binding,
            fingerprint,
            secret,
        })
    }

    /// The binding this lease was issued for.
    pub fn binding(&self) -> &CredentialBinding {
        &self.binding
    }

    /// The lease's binding fingerprint.
    pub fn fingerprint(&self) -> BindingFingerprint {
        self.fingerprint
    }

    /// Borrow the credential bytes for exactly one closure. Crate-internal on purpose: the only
    /// scope that may see these bytes is the adapter constructing the one fixed upstream request and
    /// the exact-secret redactor for its response (design §5.4, §6.3). There is no public key
    /// accessor and no copy that outlives the closure.
    pub(crate) fn expose_for_upstream<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(self.secret.as_slice())
    }
}

impl fmt::Debug for BoundCredentialLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundCredentialLease")
            .field("binding", &"<redacted>")
            .field("fingerprint", &"<redacted>")
            .field("secret", &self.secret)
            .finish()
    }
}

/// Validate a v1 API-key value: non-empty, at most [`MAX_API_KEY_BYTES`], and matching RFC 6750's
/// `b64token` shape `[A-Za-z0-9._~+/-]+=*` with `=` only as a trailing run. The value is never
/// trimmed or rewritten (design §3.2).
///
/// Public so the credential owner (P1, `rhapsody-credential-ipc`) can enforce the broker's exact
/// size/syntax bound *before* storing a value rather than duplicating the rule — the two call sites
/// share one implementation by construction.
pub fn validate_api_key_value(value: &[u8]) -> Result<(), CredentialRejection> {
    if value.is_empty() {
        return Err(CredentialRejection::Empty);
    }
    if value.len() > MAX_API_KEY_BYTES {
        return Err(CredentialRejection::TooLong);
    }
    let mut seen_padding = false;
    let mut has_base = false;
    for &byte in value {
        if seen_padding {
            if byte != b'=' {
                return Err(CredentialRejection::InvalidShape);
            }
        } else if byte == b'=' {
            seen_padding = true;
        } else if is_b64token_char(byte) {
            has_base = true;
        } else {
            return Err(CredentialRejection::InvalidShape);
        }
    }
    if !has_base {
        return Err(CredentialRejection::InvalidShape);
    }
    Ok(())
}

fn is_b64token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'+' | b'/' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> CredentialBinding {
        CredentialBinding::new(
            "provider-a",
            BrokerProtocol::OpenAiChatCompletions,
            "https://api.example.com/v1",
        )
        .expect("binding")
    }

    #[test]
    fn identical_bindings_share_a_fingerprint() {
        assert!(binding().fingerprint().matches(&binding().fingerprint()));
    }

    #[test]
    fn endpoint_change_changes_the_fingerprint() {
        let other = CredentialBinding::new(
            "provider-a",
            BrokerProtocol::OpenAiChatCompletions,
            "https://api.example.com/v2",
        )
        .expect("binding");
        assert!(!binding().fingerprint().matches(&other.fingerprint()));
    }

    #[test]
    fn empty_provider_or_endpoint_is_refused() {
        assert_eq!(
            CredentialBinding::new("", BrokerProtocol::OpenAiChatCompletions, "https://x/v1"),
            Err(BrokerError::InvalidBinding)
        );
        assert_eq!(
            CredentialBinding::new("p", BrokerProtocol::OpenAiChatCompletions, ""),
            Err(BrokerError::InvalidBinding)
        );
    }

    #[test]
    fn fingerprint_debug_does_not_reveal_the_digest() {
        let text = format!("{:?}", binding().fingerprint());
        assert_eq!(text, "<non-secret binding fingerprint>");
    }

    #[test]
    fn lease_debug_redacts_everything() {
        let lease =
            BoundCredentialLease::new(binding(), "sk-fake-key".as_bytes().to_vec()).expect("lease");
        let text = format!("{lease:?}");
        assert!(!text.contains("sk-fake-key"));
        assert!(!text.contains("provider-a"));
    }

    #[test]
    fn valid_api_key_shapes_are_accepted() {
        for value in ["abc", "sk-proj-1234._~+/", "trailing=", "trailing=="] {
            assert!(
                BoundCredentialLease::new(binding(), value.as_bytes().to_vec()).is_ok(),
                "{value} should be accepted"
            );
        }
    }

    #[test]
    fn invalid_api_key_shapes_are_refused() {
        let too_long = vec![b'a'; MAX_API_KEY_BYTES + 1];
        let cases: Vec<(&str, Vec<u8>, CredentialRejection)> = vec![
            ("empty", b"".to_vec(), CredentialRejection::Empty),
            (
                "whitespace",
                b"sk key".to_vec(),
                CredentialRejection::InvalidShape,
            ),
            (
                "control",
                b"sk\nkey".to_vec(),
                CredentialRejection::InvalidShape,
            ),
            (
                "non-ascii",
                vec![0xC3, 0xA9],
                CredentialRejection::InvalidShape,
            ),
            (
                "equals in middle",
                b"abc=def".to_vec(),
                CredentialRejection::InvalidShape,
            ),
            (
                "only padding",
                b"===".to_vec(),
                CredentialRejection::InvalidShape,
            ),
            ("too long", too_long, CredentialRejection::TooLong),
        ];
        for (name, value, expected) in cases {
            let result = BoundCredentialLease::new(binding(), value);
            assert_eq!(
                result.err(),
                Some(BrokerError::InvalidCredential(expected)),
                "{name} should be refused with {expected:?}"
            );
        }
    }
}
