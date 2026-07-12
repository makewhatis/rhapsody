//! SCAFFOLDING for the deferred "Connect Linear" OAuth flow (spec §7, out-of-scope for live
//! exchange). Parity port of `$REF/desktop/internal/linearoauth/linearoauth.go`.
//!
//! Linear supports PKCE (no client_secret), so the app can build the authorize URL and run a loopback
//! callback later — but the whole flow is gated behind a configured client_id, which does not exist in
//! v1. Until then the working credential path is paste-a-token (see [`crate::credential`]); the token
//! exchange/validation here is intentionally not implemented.

/// Linear's OAuth authorize endpoint.
const AUTHORIZE_ENDPOINT: &str = "https://linear.app/oauth/authorize";

/// Reports whether an OAuth client_id is present. In v1 it is always absent, so the "Connect Linear"
/// button is shown but its flow stays inert. Mirrors `Configured`.
pub fn configured(client_id: &str) -> bool {
    !client_id.is_empty()
}

/// Builds the PKCE authorize URL the loopback flow would open. It is pure and covered by tests; the
/// exchange of the returned code for tokens is deferred until the OAuth app (client_id + redirect URIs)
/// is minted by a Linear admin. Mirrors `AuthorizeURL`.
pub fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "read,write,issues:create")
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("actor", "user")
        .finish();
    format!("{AUTHORIZE_ENDPOINT}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors TestConfiguredGate: OAuth is gated off without a client_id (v1), on once one exists.
    #[test]
    fn configured_gate() {
        assert!(
            !configured(""),
            "OAuth must be gated off without a client_id (v1)"
        );
        assert!(
            configured("client-123"),
            "want true once a client_id exists"
        );
    }

    // Mirrors TestAuthorizeURL: builds Linear's PKCE authorize URL (no client_secret; S256). The
    // exchange/validation stays deferred — only the URL scaffolding is exercised.
    #[test]
    fn authorize_url_builds_pkce_query() {
        let u = authorize_url(
            "client-123",
            "http://127.0.0.1:51789/oauth/callback",
            "state-xyz",
            "challenge-abc",
        );
        for want in [
            "https://linear.app/oauth/authorize?",
            "client_id=client-123",
            "code_challenge=challenge-abc",
            "code_challenge_method=S256",
            "response_type=code",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A51789%2Foauth%2Fcallback",
            "state=state-xyz",
        ] {
            assert!(u.contains(want), "authorize_url = {u:?}; missing {want:?}");
        }
    }
}
