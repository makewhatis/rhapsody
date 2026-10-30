//! `onepassword` — the 1Password service-account environment a run's agent child inherits
//! (STUDIO-1584). Rhapsody-only; no Go counterpart.
//!
//! Two variables matter, and this module is the ONE place both are named so the claude runner, the
//! opencode runner and the daemon-side health probe cannot drift apart:
//!
//! * [`OP_SERVICE_ACCOUNT_TOKEN_ENV`] (`OP_SERVICE_ACCOUNT_TOKEN`) — the service-account token a
//!   bound project's runs authenticate with. Set ONLY on the agent subprocess (and anything it
//!   spawns); never in the daemon's own environment, a run's recorded environment, or the transcript.
//!   <https://developer.1password.com/docs/service-accounts/use-with-1password-cli/>
//! * [`OP_BIOMETRIC_UNLOCK_ENABLED_ENV`] (`OP_BIOMETRIC_UNLOCK_ENABLED`) — set to `false` for EVERY
//!   agent run, bound or not. Without it, an `op` call with no service-account token falls back to
//!   the **1Password desktop-app integration** and raises a Touch ID prompt on the operator's Mac
//!   that grants their whole account. Disabling it makes that call fail fast with an auth error
//!   instead — turning an invisible stall into a visible, explainable failure.
//!   <https://developer.1password.com/docs/cli/app-integration/#disable-biometric-unlock>
//!
//! Both names were verified present in the installed CLI (`op` 2.41.0) before this module was
//! written, and the biometric-disable behavior is the one 1Password documents for the CLI's app
//! integration.

/// The service-account token variable. Set only when a project binds a credential.
pub const OP_SERVICE_ACCOUNT_TOKEN_ENV: &str = "OP_SERVICE_ACCOUNT_TOKEN";

/// The biometric-unlock toggle. Set to `false` on EVERY agent run.
pub const OP_BIOMETRIC_UNLOCK_ENABLED_ENV: &str = "OP_BIOMETRIC_UNLOCK_ENABLED";

/// The value [`OP_BIOMETRIC_UNLOCK_ENABLED_ENV`] is pinned to on every run.
pub const OP_BIOMETRIC_UNLOCK_DISABLED: &str = "false";

/// Appends the 1Password variables to an already-built `KEY=VALUE` env vector, in place.
///
/// The biometric-disable variable is ALWAYS set, whether or not a token is bound — that is the
/// ticket's "no silent fallback to David's account" rule. When `token` is `Some` and non-empty it is
/// set as the service-account token; any inherited value of either variable is dropped first, so an
/// operator's own `OP_SERVICE_ACCOUNT_TOKEN` in the daemon env can never leak into a run that binds
/// nothing (and a bound run's token can never be shadowed by an inherited one).
pub fn append_onepassword_env(env: &mut Vec<String>, token: Option<&str>) {
    env.retain(|kv| {
        !kv.starts_with(OP_BIOMETRIC_UNLOCK_ENABLED_ENV)
            && !kv.starts_with(OP_SERVICE_ACCOUNT_TOKEN_ENV)
    });
    env.push(format!(
        "{OP_BIOMETRIC_UNLOCK_ENABLED_ENV}={OP_BIOMETRIC_UNLOCK_DISABLED}"
    ));
    if let Some(token) = token
        && !token.is_empty()
    {
        env.push(format!("{OP_SERVICE_ACCOUNT_TOKEN_ENV}={token}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_disables_biometric_unlock_even_with_no_token() {
        let mut env = vec!["PATH=/usr/bin".to_string()];
        append_onepassword_env(&mut env, None);
        assert!(env.contains(&"OP_BIOMETRIC_UNLOCK_ENABLED=false".to_string()));
        assert!(
            !env.iter()
                .any(|kv| kv.starts_with("OP_SERVICE_ACCOUNT_TOKEN=")),
            "an unbound run must carry no token"
        );
    }

    #[test]
    fn sets_the_token_when_bound() {
        let mut env = vec![];
        append_onepassword_env(&mut env, Some("ops_sentinel"));
        assert!(env.contains(&"OP_SERVICE_ACCOUNT_TOKEN=ops_sentinel".to_string()));
        assert!(env.contains(&"OP_BIOMETRIC_UNLOCK_ENABLED=false".to_string()));
    }

    #[test]
    fn drops_an_inherited_token_and_biometric_value_first() {
        let mut env = vec![
            "OP_SERVICE_ACCOUNT_TOKEN=operator-token".to_string(),
            "OP_BIOMETRIC_UNLOCK_ENABLED=true".to_string(),
        ];
        append_onepassword_env(&mut env, None);
        assert!(
            !env.iter().any(|kv| kv.contains("operator-token")),
            "an unbound run must not inherit the operator's token"
        );
        assert_eq!(
            env.iter()
                .filter(|kv| kv.starts_with("OP_BIOMETRIC_UNLOCK_ENABLED="))
                .count(),
            1,
            "exactly one biometric value, and it is false"
        );
        assert!(env.contains(&"OP_BIOMETRIC_UNLOCK_ENABLED=false".to_string()));
    }

    #[test]
    fn an_empty_token_is_treated_as_unbound() {
        let mut env = vec![];
        append_onepassword_env(&mut env, Some(""));
        assert!(
            !env.iter()
                .any(|kv| kv.starts_with("OP_SERVICE_ACCOUNT_TOKEN="))
        );
    }
}
