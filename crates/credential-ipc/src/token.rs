//! Generates the per-launch bootstrap token (ticket STUDIO-981). A fresh token is minted for every
//! `rhapsodyd` spawn — never reused across a restart — so a token captured from one daemon launch
//! (e.g. by a confused-deputy process that observed it some other way) cannot authenticate a
//! connection to a later launch.

use rand::RngCore;

/// 32 CSPRNG bytes, hex-encoded (64 chars) — long enough that guessing is infeasible and short
/// enough to fit comfortably inside one frame.
pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_tokens_of_the_expected_length() {
        let a = generate();
        let b = generate();
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);
        assert_ne!(
            a, b,
            "two generated tokens collided — CSPRNG source is broken"
        );
    }
}
