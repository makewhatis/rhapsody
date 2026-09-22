//! Secret-bearing types.
//!
//! [`ZeroizingBytes`] is the primary zeroing buffer; [`CapabilityToken`] is a turn capability in its
//! child-facing form. Both are deliberately hostile to accidental disclosure: no `Clone`, no
//! `Serialize`, no `Display`, and a redacting hand-written `Debug`. The design's honesty boundary is
//! recorded on [`ZeroizingBytes`]: Rhapsody wipes the buffer it owns, but cannot prove that
//! JSON/environment/process libraries did not make transient copies.

use std::fmt;

use zeroize::Zeroizing;

/// A heap buffer that zeroizes its primary allocation on drop.
///
/// Not `Clone`: a copy would create a second allocation beyond the one Rhapsody can account for.
pub struct ZeroizingBytes(Zeroizing<Vec<u8>>);

impl ZeroizingBytes {
    /// Take ownership of `bytes` as a zeroing buffer.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The current length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the bytes inside the crate (binding validation, token encoding).
    pub(crate) fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl fmt::Debug for ZeroizingBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted secret buffer>")
    }
}

/// An opaque turn capability: exactly 32 CSPRNG bytes encoded as 43 unpadded base64url ASCII
/// characters. It carries no issue key, run id, provider id, model, timestamp, or signed claims —
/// all authority is server-side (design §4.1).
///
/// The raw value is reachable only through [`CapabilityToken::expose_for_child`], which borrows it
/// for the duration of one closure; there is no `Display`, no `to_string`, and no unbounded bytes
/// accessor. It is non-`Clone` and does not serialize.
pub struct CapabilityToken {
    ascii: ZeroizingBytes,
}

impl CapabilityToken {
    /// Wrap already-encoded base64url ASCII bytes. Crate-internal: only the registry mints a token,
    /// and only from 32 freshly-random bytes.
    pub(crate) fn from_encoded(ascii: Vec<u8>) -> Self {
        Self {
            ascii: ZeroizingBytes::new(ascii),
        }
    }

    /// The number of ASCII characters in the bearer value (43 for a well-formed token).
    pub fn len(&self) -> usize {
        self.ascii.len()
    }

    /// Whether the token is empty (never true for a minted token).
    pub fn is_empty(&self) -> bool {
        self.ascii.is_empty()
    }

    /// Borrow the bearer value as a `&str` for the one child-facing construction that needs it. The
    /// borrow cannot escape the closure.
    pub fn expose_for_child<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        let text = std::str::from_utf8(self.ascii.as_slice()).unwrap_or("");
        f(text)
    }
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted turn capability>")
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroize;

    use super::*;

    #[test]
    fn debug_redacts_the_buffer() {
        let bytes = ZeroizingBytes::new(b"super-secret".to_vec());
        assert_eq!(format!("{bytes:?}"), "<redacted secret buffer>");
        assert!(!format!("{bytes:?}").contains("super-secret"));
    }

    #[test]
    fn zeroize_wipes_the_primary_allocation() {
        let mut buffer = ZeroizingBytes::new(vec![7u8; 32]);
        // The buffer is still owned here, so reading it after an explicit `zeroize` is safe and
        // proves the owned allocation is wiped (drop relies on the same `Zeroizing` glue).
        buffer.0.zeroize();
        assert!(buffer.as_slice().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn capability_debug_does_not_leak_the_value() {
        let token =
            CapabilityToken::from_encoded(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklm".to_vec());
        assert_eq!(format!("{token:?}"), "<redacted turn capability>");
        token.expose_for_child(|value| {
            assert!(!format!("{token:?}").contains(value));
        });
    }

    #[test]
    fn expose_for_child_borrows_within_the_closure() {
        let token = CapabilityToken::from_encoded(b"token-value".to_vec());
        let seen = token.expose_for_child(str::to_owned);
        assert_eq!(seen, "token-value");
    }
}
