//! `onepassword` — Rhapsody-only 1Password service-account credential bindings (STUDIO-1584). **No Go
//! counterpart** — the frozen reference has no 1Password concept at all, so this whole module is an
//! ADDITIVE divergence (README "Divergences"). Nothing here is reachable unless a workflow writes an
//! `onepassword:` field, which is what keeps an existing installation byte-identical.
//!
//! # A binding is an ID, never a value
//!
//! A workflow binds a service-account credential to a project by a plain, canonical **id** (for
//! example `studio-infra-readonly`). The token itself lives only in the desktop-managed system
//! credential store and is referenced by that id; `WORKFLOW.md` never carries the value. Like the
//! provider id parser, [`canonical_onepassword_id`] is the ONE parser every surface calls rather than
//! re-implementing the rule, and it is deliberately strict enough that a real service-account token
//! (`ops_…`) can never be a valid id — a config that pastes a token in place of an id is REFUSED, not
//! silently accepted.

/// Maximum length of a canonical 1Password credential id, matching the provider id cap so the two
/// id spaces stay comparable.
pub const ONEPASSWORD_ID_MAX_LEN: usize = 64;

/// The 1Password service-account token prefix. A value shaped like a token (`ops_…`) must never be
/// accepted where an id belongs; this is checked explicitly, in addition to the charset rule, so the
/// refusal carries a message that names the actual mistake.
pub const ONEPASSWORD_TOKEN_PREFIX: &str = "ops_";

/// Validates a 1Password credential id against the canonical syntax: 1-64 characters, a lowercase
/// ASCII letter first, then lowercase ASCII letters, digits, `_` and `-`. The exact same shape as a
/// canonical provider id (`crates/config/src/providers.rs`), for the same reasons: the id round-trips
/// identically through YAML, the effective-config view and the CLI, and a non-canonical spelling is
/// REJECTED rather than case-folded.
///
/// A value that looks like a service-account token is refused with a message naming the mistake, so
/// the operator learns to store the token in the desktop app and bind only its id. Returns the reason
/// as a plain string; the caller wraps it in the scope-appropriate validation error.
pub fn canonical_onepassword_id(id: &str) -> Result<(), String> {
    if id.starts_with(ONEPASSWORD_TOKEN_PREFIX) {
        return Err(format!(
            "{id:?} looks like a 1Password service-account token, not an id — bind the credential's \
             id and store the token in the desktop app"
        ));
    }
    if id.is_empty() {
        return Err("1Password credential id must not be empty".to_string());
    }
    if id.len() > ONEPASSWORD_ID_MAX_LEN {
        return Err(format!(
            "1Password credential id must be at most {ONEPASSWORD_ID_MAX_LEN} characters"
        ));
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap_or('\0');
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "1Password credential id {id:?} must start with a lowercase ASCII letter"
        ));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "1Password credential id {id:?} may contain only lowercase ASCII letters, digits, '_' \
             and '-'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_ticket_examples() {
        assert!(canonical_onepassword_id("studio-infra-readonly").is_ok());
        assert!(canonical_onepassword_id("k8s-studio-prod").is_ok());
        assert!(canonical_onepassword_id("a").is_ok());
        assert!(canonical_onepassword_id("team_1").is_ok());
    }

    #[test]
    fn refuses_a_token_shaped_value_with_a_named_reason() {
        // The exact mistake the ticket calls out: pasting the value in place of the id.
        let err = canonical_onepassword_id("ops_eyJhbGciOiJIUzI1NiJ9").expect_err("token refused");
        assert!(err.contains("service-account token"), "reason: {err}");
    }

    #[test]
    fn refuses_non_canonical_ids() {
        assert!(canonical_onepassword_id("").is_err(), "empty");
        assert!(
            canonical_onepassword_id("Studio49").is_err(),
            "uppercase rejected, never case-folded"
        );
        assert!(canonical_onepassword_id("1leading-digit").is_err());
        assert!(canonical_onepassword_id("-leading-dash").is_err());
        assert!(canonical_onepassword_id("has space").is_err());
        assert!(canonical_onepassword_id("has:colon").is_err());
        assert!(canonical_onepassword_id("has.dot").is_err());
        assert!(
            canonical_onepassword_id(&"a".repeat(ONEPASSWORD_ID_MAX_LEN + 1)).is_err(),
            "over length cap"
        );
    }
}
