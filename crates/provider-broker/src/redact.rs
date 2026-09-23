//! Exact-byte streaming redaction of the upstream provider credential (design §6.3, §7.1).
//!
//! The provider key is removed from every response body before it can reach the child. The redactor
//! is *streaming*: it must find the credential even when its bytes are split across arbitrary
//! network chunk boundaries, and it must retain only the minimum look-behind needed for that match
//! (at most `secret.len() - 1` bytes). The unredacted bytes are never written anywhere — the only
//! copy Rhapsody holds is the zeroizing secret buffer, which zeroizes on drop.
//!
//! This is defense in depth, not protection against a malicious provider that encodes the secret
//! (base64, URL-encoding, split across events); that residual risk is explicit in the design.

use crate::secret::ZeroizingBytes;
use crate::upstream::contains_secret;

/// The fixed marker substituted for the exact upstream credential.
pub const REDACTION_MARKER: &[u8] = b"[redacted-provider-key]";

/// A streaming exact-byte redactor.
///
/// Feed each upstream chunk with [`StreamingRedactor::push`]; it returns the bytes safe to emit. Any
/// suffix that could still become the secret is held until more bytes arrive (or the stream ends).
pub struct StreamingRedactor {
    secret: ZeroizingBytes,
    /// The bytes substituted for each exact secret match. The fixed marker normally, but a single
    /// non-`b64token` byte (`*`) when the marker itself contains the secret (a valid credential such
    /// as `provider` occurs verbatim inside `[redacted-provider-key]`). The replacement must be
    /// non-empty: removing a match entirely would splice its neighbours together, and the joined
    /// bytes could re-form the credential (`pro` + `vider` -> `provider`). `*` is outside the
    /// credential alphabet (`[A-Za-z0-9._~+/-]`), so no window crossing it can equal the secret.
    replacement: Vec<u8>,
    /// Bytes held back because they might be the beginning of the secret.
    pending: Vec<u8>,
    finished: bool,
}

impl StreamingRedactor {
    /// Build a redactor for `secret`. An empty secret (never produced by credential validation)
    /// degrades to a pass-through.
    pub fn new(secret: ZeroizingBytes) -> Self {
        // `contains_secret` is the same exact-byte check used for response headers: if the marker
        // would itself contain the secret, substitute a non-`b64token` byte instead of emitting a
        // leaking marker (leaving the match out would splice its neighbours into a new match).
        let replacement = if contains_secret(REDACTION_MARKER, secret.as_slice()) {
            vec![b'*']
        } else {
            REDACTION_MARKER.to_vec()
        };
        Self {
            secret,
            replacement,
            pending: Vec::new(),
            finished: false,
        }
    }

    /// Whether the redactor is holding any bytes that have not been emitted yet. A non-empty pending
    /// buffer after [`StreamingRedactor::finish`] is the remaining tail (never the secret).
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Feed one upstream chunk, returning the redacted bytes that can be emitted now.
    ///
    /// Returns `None` once the stream was finished (a redactor is single-use).
    pub fn push(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if self.finished {
            return None;
        }
        self.pending.extend_from_slice(chunk);
        let emitted = self.drain();
        Some(emitted)
    }

    /// End the stream. The remaining tail can no longer become the secret, so it is emitted as-is
    /// (it never contains a full credential: a complete match would already have been replaced).
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.secret.is_empty() {
            return std::mem::take(&mut self.pending);
        }
        let mut out = Vec::new();
        {
            let secret = self.secret.as_slice();
            scan(&self.pending, secret, &self.replacement, true, &mut out);
        }
        self.pending.clear();
        out
    }

    /// Replace every complete occurrence in `pending` and retain only the suffix that is a proper
    /// prefix of the secret.
    fn drain(&mut self) -> Vec<u8> {
        if self.secret.is_empty() {
            return std::mem::take(&mut self.pending);
        }
        let mut out = Vec::new();
        let retained_from = {
            let secret = self.secret.as_slice();
            scan(&self.pending, secret, &self.replacement, false, &mut out)
        };
        // Keep only the not-yet-decidable tail.
        let tail = self.pending.split_off(retained_from);
        self.pending = tail;
        out
    }
}

/// Scan `input` left to right: replace each exact occurrence of `secret` with `marker`, emit other
/// bytes, and stop at the first suffix that is a *proper* prefix of the secret (unless `flush`, in
/// which case the tail is emitted unchanged). Returns the index from which the tail was retained.
fn scan(input: &[u8], secret: &[u8], marker: &[u8], flush: bool, out: &mut Vec<u8>) -> usize {
    debug_assert!(!secret.is_empty());
    let mut i = 0;
    while i < input.len() {
        let remaining = &input[i..];
        if remaining.len() >= secret.len() && remaining[..secret.len()] == *secret {
            out.extend_from_slice(marker);
            i += secret.len();
            continue;
        }
        // A shorter suffix that is a prefix of the secret must be held for the next chunk.
        if remaining.len() < secret.len() && secret.starts_with(remaining) && !flush {
            return i;
        }
        out.push(input[i]);
        i += 1;
    }
    input.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_all(secret: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
        let mut redactor = StreamingRedactor::new(ZeroizingBytes::new(secret.to_vec()));
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&redactor.push(chunk).expect("push before finish"));
        }
        out.extend_from_slice(&redactor.finish());
        out
    }

    #[test]
    fn replaces_a_whole_chunk() {
        let out = redact_all(b"sk-secret", &[b"hello sk-secret world"]);
        assert_eq!(out, b"hello [redacted-provider-key] world");
    }

    #[test]
    fn replaces_across_every_split_boundary() {
        let secret = b"sk-live-abcdef";
        let body = b"AAAsk-live-abcdefBBBB";
        for split in 0..=body.len() {
            let out = redact_all(secret, &[&body[..split], &body[split..]]);
            assert_eq!(
                out, b"AAA[redacted-provider-key]BBBB",
                "split at {split} must still redact"
            );
            assert!(
                !out.windows(secret.len()).any(|window| window == secret),
                "split at {split} leaked the key"
            );
        }
    }

    #[test]
    fn one_byte_at_a_time_never_leaks() {
        let secret = b"sk-xy";
        let body = b"sk-xy--sk-xy";
        let chunks: Vec<&[u8]> = body.chunks(1).collect();
        let out = redact_all(secret, &chunks);
        assert_eq!(out, b"[redacted-provider-key]--[redacted-provider-key]");
    }

    #[test]
    fn overlapping_prefixes_are_not_a_secret() {
        // "sk-sk-abc" with secret "sk-abc": the first four bytes are a prefix but never complete.
        let out = redact_all(b"sk-abc", &[b"sk-sk-abc"]);
        assert_eq!(out, b"sk-[redacted-provider-key]");
    }

    #[test]
    fn finish_flushes_a_partial_prefix_unchanged() {
        let mut redactor = StreamingRedactor::new(ZeroizingBytes::new(b"sk-secret".to_vec()));
        assert_eq!(redactor.push(b"sk-").expect("push"), b"");
        assert_eq!(redactor.finish(), b"sk-");
    }

    #[test]
    fn non_matching_bytes_are_emitted_without_excess_lookbehind() {
        let mut redactor = StreamingRedactor::new(ZeroizingBytes::new(b"sk-secret".to_vec()));
        let out = redactor
            .push(b"a very long ordinary sentence")
            .expect("push");
        assert_eq!(out.len(), "a very long ordinary sentence".len());
        assert_eq!(redactor.pending_len(), 0);
    }

    #[test]
    fn a_replacement_that_contains_the_secret_never_emits_the_secret() {
        // A credential such as `provider` occurs verbatim inside `[redacted-provider-key]`, and a
        // credential such as `d-p` occurs inside `redacted-provider-key`. Emitting the marker would
        // leak it; *deleting* the match would splice its neighbours into a new occurrence
        // (`pro` + `vider` -> `provider`). The fallback replacement is a single non-`b64token` byte,
        // so neither the containment case nor the splice case can re-emit the secret.
        let bodies: [&[u8]; 6] = [
            b"before provider after",
            b"proprovidervider",
            b"d-d-pp",
            b"aaaaa",
            b"redacted-provider-key",
            b"xkeykeyy",
        ];
        for secret in [
            &b"provider"[..],
            b"key",
            b"redacted-provider-key",
            b"d-p",
            b"a",
        ] {
            for body in bodies {
                // The whole body and every single-byte split boundary.
                for split in 0..=body.len() {
                    let out = redact_all(secret, &[&body[..split], &body[split..]]);
                    assert!(
                        !out.windows(secret.len()).any(|window| window == secret),
                        "secret {secret:?} leaked from {body:?} split at {split}: {:?}",
                        String::from_utf8_lossy(&out)
                    );
                }
            }
        }
    }

    #[test]
    fn a_short_secret_split_across_chunks_is_still_removed() {
        let out = redact_all(b"prov", &[b"xpro", b"videry"]);
        assert!(!out.windows(4).any(|window| window == b"prov"));
    }
}
