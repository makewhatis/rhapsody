//! Environment scrub + billing guard — parity port of Go `billing.go`.
//!
//! The coding agent must never receive the tracker credential (design §15.5), and — unless the
//! operator disables the billing guard — must never be able to bill the metered API. This module
//! provides the pure pieces: the env-var name sets, [`scrub_env`] (name + value removal), and the
//! [`billing_guard_enabled`] / [`billing_guard_ok`] decisions the runner enforces.

/// Tracker-credential env var names. ALWAYS scrubbed from the claude child's environment —
/// independent of the billing guard — because the coding agent must never receive the tracker
/// credential (design §15.5). A tracker key supplied under a custom var name is additionally
/// withheld by VALUE (see [`scrub_env`] and the runner's `tracker_api_key` value-match).
pub const TRACKER_ENV_VARS: &[&str] = &["LINEAR_API_KEY"];

/// Billing/routing-related env vars removed ONLY when the billing guard is enabled. The
/// `ANTHROPIC_*` / `CLAUDE_CODE_USE_*` entries ensure the daemon can never silently bill the
/// metered API (the `CLAUDE_CODE_USE_*` flags are the load-bearing gate for the Bedrock/Vertex/
/// Foundry backends); `apiKeySource` is then asserted `"none"` from each system/init event. The
/// base-URL and OAuth/bearer-token vars are defense-in-depth. Disabling the guard is the documented
/// API-billing escape hatch and leaves these intact.
pub const BILLING_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_BEARER_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
];

/// The full set of env var names scrubbed when the billing guard is enabled: the always-scrub
/// tracker vars followed by the billing/routing vars (order preserved, no overlap). When the guard
/// is off only [`TRACKER_ENV_VARS`] is scrubbed.
pub fn scrubbed_env_vars() -> Vec<&'static str> {
    TRACKER_ENV_VARS
        .iter()
        .chain(BILLING_ENV_VARS.iter())
        .copied()
        .collect()
}

/// Returns a copy of `env` (entries in `KEY=VALUE` form) with every entry removed whose name (the
/// part before `=`) is in `drop_names` (exact match) OR whose VALUE exactly equals one of the
/// non-empty `drop_values` (so a secret supplied under a custom var name is still withheld). Order
/// is preserved; unrelated vars (e.g. `PATH`) are kept.
pub fn scrub_env(env: &[String], drop_names: &[&str], drop_values: &[&str]) -> Vec<String> {
    let has_value_filter = drop_values.iter().any(|v| !v.is_empty());
    if drop_names.is_empty() && !has_value_filter {
        return env.to_vec();
    }
    env.iter()
        .filter(|&kv| {
            let (name, val) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
            !drop_names.contains(&name)
                && !drop_values.iter().any(|dv| !dv.is_empty() && *dv == val)
        })
        .cloned()
        .collect()
}

/// Reports whether the billing guard is on. An absent knob (`None`) defaults to enabled (`true`);
/// an explicit value is honored verbatim.
pub fn billing_guard_enabled(p: Option<bool>) -> bool {
    p.unwrap_or(true)
}

/// Reports whether the system/init `apiKeySource` means the agent is using the logged-in
/// subscription rather than a metered API key. Only the exact string `"none"` passes; any other
/// value (user/project/api-key name, or a missing/empty field) fails.
pub fn billing_guard_ok(api_key_source: &str) -> bool {
    api_key_source == "none"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // Mirrors Go `claude.TestScrubbedEnvVarsExactSet`: order-independent exact set.
    #[test]
    fn scrubbed_env_vars_exact_set() {
        let want = [
            // tracker (always scrubbed)
            "LINEAR_API_KEY",
            // billing / routing (scrubbed when guard on)
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
            "ANTHROPIC_BEARER_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
        ];
        let mut got = scrubbed_env_vars();
        let mut want = want.to_vec();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    // Mirrors Go `claude.TestScrubbedEnvVarsIsTrackerPlusBilling`: exactly tracker ++ billing, in
    // order, with no overlap.
    #[test]
    fn scrubbed_env_vars_is_tracker_plus_billing() {
        let want: Vec<&str> = TRACKER_ENV_VARS
            .iter()
            .chain(BILLING_ENV_VARS.iter())
            .copied()
            .collect();
        assert_eq!(scrubbed_env_vars(), want);
        for n in TRACKER_ENV_VARS {
            assert!(
                !BILLING_ENV_VARS.contains(n),
                "var {n} appears in both tracker and billing"
            );
        }
    }

    // Mirrors Go `claude.TestScrubEnvRemovesExactlyDropSetPreservesPath`.
    #[test]
    fn scrub_env_removes_exactly_drop_set_preserves_path() {
        let input = owned(&[
            "PATH=/usr/bin",
            "HOME=/home/x",
            "ANTHROPIC_API_KEY=sk-secret",
            "ANTHROPIC_AUTH_TOKEN=tok",
            "CLAUDE_CODE_USE_BEDROCK=1",
            "CLAUDE_CODE_USE_VERTEX=1",
            "CLAUDE_CODE_USE_FOUNDRY=1",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS=1",
            "LINEAR_API_KEY=lin",
            "ANTHROPIC_API_KEY_LIKE=keep", // not an exact match -> kept
        ]);
        let scrubbed = scrubbed_env_vars();
        let got = scrub_env(&input, &scrubbed, &[]);
        for kv in &got {
            let name = kv.split_once('=').map(|(n, _)| n).unwrap_or(kv.as_str());
            assert!(!scrubbed.contains(&name), "scrubbed var leaked: {kv}");
        }
        assert_eq!(
            got,
            owned(&[
                "PATH=/usr/bin",
                "HOME=/home/x",
                "ANTHROPIC_API_KEY_LIKE=keep"
            ])
        );
    }

    // Mirrors Go `claude.TestScrubEnvEmptyDropIsIdentity`.
    #[test]
    fn scrub_env_empty_drop_is_identity() {
        let input = owned(&["PATH=/usr/bin", "ANTHROPIC_API_KEY=sk"]);
        assert_eq!(scrub_env(&input, &[], &[]), input);
    }

    // Mirrors Go `claude.TestScrubEnvDropsTrackerSecretByNameAndValue`: dropped both by canonical
    // name and by VALUE; an empty secret matches no var.
    #[test]
    fn scrub_env_drops_tracker_secret_by_name_and_value() {
        let secret = "lin_api_supersecret";
        let input = owned(&[
            "PATH=/usr/bin",
            &format!("LINEAR_API_KEY={secret}"), // dropped by name
            &format!("MY_CUSTOM_TOKEN={secret}"), // dropped by value
            "KEEP=ok",
            "EMPTY=", // empty value must not be dropped by an empty secret
        ]);
        let got = scrub_env(&input, TRACKER_ENV_VARS, &[secret]);
        for kv in &got {
            assert!(!kv.contains(secret), "tracker secret leaked: {kv}");
        }
        assert_eq!(got, owned(&["PATH=/usr/bin", "KEEP=ok", "EMPTY="]));
    }

    // Mirrors Go `claude.TestScrubEnvEmptyValueSecretDropsNothingByValue`: an empty drop value never
    // matches an empty-valued env entry.
    #[test]
    fn scrub_env_empty_value_secret_drops_nothing_by_value() {
        let input = owned(&["PATH=/usr/bin", "EMPTY=", "KEEP=ok"]);
        assert_eq!(scrub_env(&input, &[], &[""]), input);
    }

    // Mirrors Go `claude.TestBillingGuardDecision`.
    #[test]
    fn billing_guard_decision() {
        let cases = [
            ("none", true),
            ("user", false),
            ("project", false),
            ("/login managed key", false),
            ("", false),
            ("ANTHROPIC_API_KEY", false),
        ];
        for (source, want) in cases {
            assert_eq!(
                billing_guard_ok(source),
                want,
                "billing_guard_ok({source:?})"
            );
        }
    }

    // The nil-pointer-defaults-true guard knob (Go `billingGuardEnabled`; the runner uses it to pick
    // the scrub set). No dedicated Go test exists — this pins the documented default.
    #[test]
    fn billing_guard_enabled_defaults_true() {
        assert!(
            billing_guard_enabled(None),
            "absent knob defaults to enabled"
        );
        assert!(billing_guard_enabled(Some(true)));
        assert!(!billing_guard_enabled(Some(false)));
    }
}
