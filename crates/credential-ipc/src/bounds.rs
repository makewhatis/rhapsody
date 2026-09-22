//! The broker's size/syntax bounds, shared by the credential owner (P1, design §2.4/§2.5) and the
//! provider broker (PB1, `provider-broker-design.md` §3.2).
//!
//! The owner must refuse an out-of-bounds credential *before* it is stored, and it must never trim
//! or rewrite a value to make it fit. Rather than duplicate PB1's rule, this module re-uses the
//! broker's own validator and constants, so the pre-storage check and the broker's registration-time
//! check can never drift apart:
//!
//! * [`validate_api_key_value`] is PB1's RFC 6750 `b64token` shape + [`MAX_API_KEY_BYTES`] check.
//! * [`validate_envelope_size`] enforces the independently-capped serialized envelope size
//!   ([`MAX_CREDENTIAL_ENVELOPE_BYTES`]).
//!
//! Both return the broker's own [`CredentialRejection`], so a caller acts on one typed refusal
//! vocabulary across the owner and the broker.

pub use rhapsody_provider_broker::{
    CredentialRejection, MAX_API_KEY_BYTES, MAX_CREDENTIAL_ENVELOPE_BYTES, validate_api_key_value,
};

/// Refuse a serialized envelope larger than the broker's independently-capped
/// [`MAX_CREDENTIAL_ENVELOPE_BYTES`].
///
/// An envelope over the cap is reported as [`CredentialRejection::TooLong`] — the envelope is
/// dominated by the credential value it wraps, and the caller's action (refuse to store) is
/// identical either way. The check is on the exact serialized bytes about to be persisted, so a
/// value that passes [`validate_api_key_value`] can still be refused here if its binding pushes the
/// envelope past the cap; the value is never trimmed to fit.
pub fn validate_envelope_size(raw: &str) -> Result<(), CredentialRejection> {
    if raw.len() > MAX_CREDENTIAL_ENVELOPE_BYTES {
        Err(CredentialRejection::TooLong)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pre-storage bound and the broker's registration-time bound are the SAME rule: drive one
    // table through both and assert identical verdicts. If PB1's validator is ever loosened or
    // tightened, this test sees the change immediately rather than letting the owner drift.
    #[test]
    fn the_owner_bound_is_pb1s_bound_for_every_shape() {
        let too_long = vec![b'a'; MAX_API_KEY_BYTES + 1];
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", b"".to_vec()),
            ("plain", b"sk-proj-1234._~+/".to_vec()),
            ("trailing padding", b"trailing==".to_vec()),
            ("space", b"sk key".to_vec()),
            ("control", b"sk\nkey".to_vec()),
            ("non-ascii", vec![0xC3, 0xA9]),
            ("mid padding", b"abc=def".to_vec()),
            ("only padding", b"===".to_vec()),
            ("too long", too_long),
        ];
        for (name, value) in cases {
            let owner = validate_api_key_value(&value);
            let broker = rhapsody_provider_broker::BoundCredentialLease::new(
                rhapsody_provider_broker::CredentialBinding::new(
                    "p",
                    rhapsody_provider_broker::BrokerProtocol::OpenAiChatCompletions,
                    "https://example/v1",
                )
                .expect("binding"),
                value.clone(),
            )
            .err()
            .map(|e| match e {
                rhapsody_provider_broker::BrokerError::InvalidCredential(rejection) => rejection,
                other => panic!("unexpected broker error {other:?}"),
            });
            assert_eq!(
                owner.err(),
                broker,
                "{name}: owner and broker verdicts must agree"
            );
        }
    }

    #[test]
    fn an_envelope_over_the_cap_is_refused_and_never_trimmed() {
        let at_cap = "x".repeat(MAX_CREDENTIAL_ENVELOPE_BYTES);
        assert_eq!(validate_envelope_size(&at_cap), Ok(()));

        let over = "x".repeat(MAX_CREDENTIAL_ENVELOPE_BYTES + 1);
        assert_eq!(
            validate_envelope_size(&over),
            Err(CredentialRejection::TooLong)
        );
        // The check is pure — it returns a verdict and leaves the input untouched.
        assert_eq!(over.len(), MAX_CREDENTIAL_ENVELOPE_BYTES + 1);
    }
}
