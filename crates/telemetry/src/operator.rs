//! Fleet-attribution `operator` derivation. Mirrors the operator helpers in Go `telemetry.go`.

/// Picks the first non-empty of configured → OS user → host name. Split out so the precedence —
/// including the empty-string guard on each source (`os.Hostname` may return `("", nil)` on a
/// misconfigured host) — is unit-testable without mocking the OS lookups. Mirrors Go
/// `deriveOperator`.
pub fn derive_operator(configured: &str, os_user: &str, host: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    if !os_user.is_empty() {
        return os_user.to_string();
    }
    if !host.is_empty() {
        return host.to_string();
    }
    String::new()
}

/// Returns the configured operator, or derives one: OS user, then host name. Returns `""` only if
/// all are empty (rare; the attribute is still emitted). Mirrors Go `resolveOperator`.
pub fn resolve_operator(configured: &str) -> String {
    derive_operator(configured, &os_user(), &hostname())
}

/// The OS login user, from `$USER` (falling back to `$LOGNAME`) — the Rust parity of Go's
/// `user.Current().Username` on the Unix hosts the daemon targets. Empty when neither is set.
fn os_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_default()
}

/// The host name via `gethostname(2)`, mirroring Go's `os.Hostname()`. Empty on failure. Shared with
/// [`crate::resource`] for the `host.name` resource attribute (Go's `resource.WithHost()`).
pub(crate) fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into `buf` and NUL-terminates when it
    // fits; the pointer/length describe a live, correctly sized buffer for the duration of the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `TestDeriveOperatorPrecedence`: configured → OS user → host, with the empty-string
    // guard on each source (an empty host must not win over a later non-empty source; all-empty →
    // "").
    #[test]
    fn derive_operator_precedence() {
        let cases = [
            ("configured wins", "david", "alice", "host1", "david"),
            ("os user when no configured", "", "alice", "host1", "alice"),
            ("host when no configured/user", "", "", "host1", "host1"),
            (
                "empty host does not win over user",
                "",
                "alice",
                "",
                "alice",
            ),
            ("all empty yields empty", "", "", "", ""),
        ];
        for (name, configured, os_user, host, want) in cases {
            assert_eq!(derive_operator(configured, os_user, host), want, "{name}");
        }
    }

    // Mirrors Go `TestBuildResourceOperatorDefaultsToOSUser` (the derivation half): an empty
    // configured operator never yields "" — it falls to the OS user, else the host name.
    #[test]
    fn resolve_operator_defaults_non_empty() {
        // On any real host at least one of $USER/hostname resolves, so the fleet-attribution key is
        // never empty (Go's invariant).
        assert!(
            !resolve_operator("").is_empty(),
            "operator must default to a non-empty value (OS user / host)"
        );
        assert_eq!(resolve_operator("fleet-7"), "fleet-7", "configured wins");
    }
}
