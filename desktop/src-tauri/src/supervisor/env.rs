//! Child-process environment for the `rhapsodyd` sidecar. Parity port of
//! `$REF/desktop/internal/supervisor/env.go`.

use std::collections::HashSet;
use std::path::Path;

/// Builds the environment for the `rhapsodyd` child process. It augments `base` (a slice of
/// `KEY=VALUE` entries, typically the process environment) so the daemon — which shells out to
/// claude, gh, gt, and git — can find those tools even when launched from a GUI app with a minimal
/// PATH (the launchd/Finder PATH gotcha, design §2):
///
///   - PATH is rebuilt as `known_good_dirs` (in order) followed by the base PATH entries, with
///     duplicates and empty segments removed (first occurrence wins).
///   - `LINEAR_API_KEY` is set to `linear_api_key` when non-empty (replacing any inherited value),
///     so the daemon resolves `api_key: $LINEAR_API_KEY` from the Keychain-fed value. An empty
///     `linear_api_key` leaves whatever the base environment carried untouched.
///   - `GIT_CONFIG_*` is injected so all github.com git operations authenticate through the gh CLI
///     over HTTPS instead of SSH (see [`git_config_env`]).
///
/// `base` is never mutated; the returned vector is freshly allocated.
pub fn child_env(base: &[String], known_good_dirs: &[String], linear_api_key: &str) -> Vec<String> {
    // Copy through the base env, replacing PATH (and optionally LINEAR_API_KEY / GIT_CONFIG_*) while
    // preserving order and every other variable.
    let mut out: Vec<String> = Vec::with_capacity(base.len() + 1);
    let mut base_path: &str = "";
    for kv in base {
        if let Some(v) = kv.strip_prefix("PATH=") {
            base_path = v;
            continue; // re-emitted below, rebuilt
        }
        if !linear_api_key.is_empty() && kv.starts_with("LINEAR_API_KEY=") {
            continue; // replaced below
        }
        if kv.starts_with("GIT_CONFIG_COUNT=")
            || kv.starts_with("GIT_CONFIG_KEY_")
            || kv.starts_with("GIT_CONFIG_VALUE_")
        {
            continue; // dropped; replaced below with our github->gh-auth config
        }
        out.push(kv.clone());
    }
    out.push(format!("PATH={}", merge_path(known_good_dirs, base_path)));
    if !linear_api_key.is_empty() {
        out.push(format!("LINEAR_API_KEY={linear_api_key}"));
    }
    out.extend(git_config_env());
    out
}

/// Returns `GIT_CONFIG_*` environment entries (git's env-based config, read by every git
/// invocation) that route github.com git access through the gh CLI over HTTPS:
///
///   - `url."https://github.com/".insteadOf` rewrites SSH github remotes (`git@github.com:` and
///     `ssh://git@github.com/`) to HTTPS at transport time, so a repo configured with an SSH URL is
///     still fetched/pushed over HTTPS — no SSH key (or YubiKey touch) involved.
///   - `credential."https://github.com".helper` is reset (empty value) then set to
///     `!gh auth git-credential`, so git asks the gh CLI for the token.
///
/// Only github.com is affected; non-github remotes (GHE, GitLab, …) keep their own auth.
fn git_config_env() -> Vec<String> {
    [
        "GIT_CONFIG_COUNT=4",
        "GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf",
        "GIT_CONFIG_VALUE_0=git@github.com:",
        "GIT_CONFIG_KEY_1=url.https://github.com/.insteadOf",
        "GIT_CONFIG_VALUE_1=ssh://git@github.com/",
        "GIT_CONFIG_KEY_2=credential.https://github.com.helper",
        "GIT_CONFIG_VALUE_2=",
        "GIT_CONFIG_KEY_3=credential.https://github.com.helper",
        "GIT_CONFIG_VALUE_3=!gh auth git-credential",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Prepends `known_good_dirs` to the colon-separated `base_path`, dropping empty segments and
/// de-duplicating while preserving first-seen order.
fn merge_path(known_good_dirs: &[String], base_path: &str) -> String {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut dirs: Vec<&str> = Vec::new();
    for d in known_good_dirs
        .iter()
        .map(String::as_str)
        .chain(base_path.split(':'))
    {
        // `insert` returns false when `d` was already present; skip empties and duplicates.
        if d.is_empty() || !seen.insert(d) {
            continue;
        }
        dirs.push(d);
    }
    dirs.join(":")
}

/// Returns the known-good directories where the external CLIs (claude, gh, gt, git) commonly live on
/// macOS, given the user's home directory. These are prepended to the child's PATH so a GUI launch
/// resolves them; the Tool-doctor's per-tool path overrides (D4) augment this set.
pub fn default_tool_dirs(home: &str) -> Vec<String> {
    let mut dirs: Vec<String> = [
        "/opt/homebrew/bin", // Homebrew (Apple Silicon)
        "/opt/homebrew/sbin",
        "/usr/local/bin", // Homebrew (Intel) / manual installs
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    if !home.is_empty() {
        let h = Path::new(home);
        dirs.push(h.join(".local").join("bin").to_string_lossy().into_owned());
        dirs.push(h.join("bin").to_string_lossy().into_owned());
        // common claude (npm -g) prefix
        dirs.push(
            h.join(".npm-global")
                .join("bin")
                .to_string_lossy()
                .into_owned(),
        );
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extracts KEY's value from a `KEY=VALUE` environment slice (last occurrence wins, matching
    /// os/exec semantics). Mirror of `envValue` in `env_test.go`.
    fn env_value<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
        let prefix = format!("{key}=");
        let mut val = None;
        for kv in env {
            if let Some(v) = kv.strip_prefix(&prefix) {
                val = Some(v);
            }
        }
        val
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // Mirrors TestChildEnvPrependsKnownGoodPATH: prepend known-good tool dirs so a GUI-launched
    // child resolves claude/gh/gt/git, without duplicating dirs already present.
    #[test]
    fn child_env_prepends_known_good_path() {
        let base = owned(&["PATH=/usr/bin:/bin", "HOME=/Users/x"]);
        let env = child_env(&base, &owned(&["/opt/homebrew/bin", "/usr/bin"]), "");
        let path = env_value(&env, "PATH").expect("PATH missing from child env");
        let dirs: Vec<&str> = path.split(':').collect();
        assert_eq!(
            dirs[0], "/opt/homebrew/bin",
            "PATH must start with the prepended known-good dir; got {path}"
        );
        // /usr/bin appeared in both the known-good list and the base PATH — it must not duplicate.
        assert_eq!(
            dirs.iter().filter(|d| **d == "/usr/bin").count(),
            1,
            "/usr/bin duplicated in {path}"
        );
        // The base entry /bin is preserved (after the prepended dirs).
        assert!(
            dirs.contains(&"/bin"),
            "base entry /bin dropped from {path}"
        );
        // HOME is carried through untouched.
        assert_eq!(env_value(&env, "HOME"), Some("/Users/x"));
    }

    // Mirrors TestChildEnvSetsLinearAPIKey: a non-empty key is injected (overriding any inherited
    // value), exactly once.
    #[test]
    fn child_env_sets_linear_api_key() {
        let base = owned(&["PATH=/usr/bin", "LINEAR_API_KEY=stale"]);
        let env = child_env(&base, &[], "lin_api_fresh");
        assert_eq!(env_value(&env, "LINEAR_API_KEY"), Some("lin_api_fresh"));
        let n = env
            .iter()
            .filter(|kv| kv.starts_with("LINEAR_API_KEY="))
            .count();
        assert_eq!(n, 1, "LINEAR_API_KEY present {n} times; want exactly 1");
    }

    // Mirrors TestChildEnvEmptyKeyLeavesBase: an empty key must not clobber an inherited value.
    #[test]
    fn child_env_empty_key_leaves_base() {
        let base = owned(&["PATH=/usr/bin", "LINEAR_API_KEY=inherited"]);
        let env = child_env(&base, &[], "");
        assert_eq!(env_value(&env, "LINEAR_API_KEY"), Some("inherited"));
    }

    // Mirrors TestChildEnvDoesNotMutateBase: `base` is borrowed, never mutated.
    #[test]
    fn child_env_does_not_mutate_base() {
        let base = owned(&["PATH=/usr/bin"]);
        let _ = child_env(&base, &owned(&["/opt/homebrew/bin"]), "k");
        assert_eq!(base, owned(&["PATH=/usr/bin"]), "base mutated: {base:?}");
    }

    // Mirrors TestChildEnvRoutesGitHubGitThroughGh.
    #[test]
    fn child_env_routes_github_git_through_gh() {
        let env = child_env(&owned(&["HOME=/h"]), &[], "");
        assert_eq!(env_value(&env, "GIT_CONFIG_COUNT"), Some("4"));
        let joined = env.join("\n");
        for want in [
            "GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf",
            "GIT_CONFIG_VALUE_0=git@github.com:",
            "GIT_CONFIG_VALUE_1=ssh://git@github.com/",
            "GIT_CONFIG_KEY_2=credential.https://github.com.helper",
            "GIT_CONFIG_VALUE_3=!gh auth git-credential",
        ] {
            assert!(joined.contains(want), "child_env missing {want}\n{joined}");
        }
    }

    // Mirrors TestChildEnvReplacesInheritedGitConfig: a stray GIT_CONFIG_* in the base env is
    // dropped so ours is authoritative.
    #[test]
    fn child_env_replaces_inherited_git_config() {
        let base = owned(&[
            "GIT_CONFIG_COUNT=1",
            "GIT_CONFIG_KEY_0=user.name",
            "GIT_CONFIG_VALUE_0=x",
            "HOME=/h",
        ]);
        let env = child_env(&base, &[], "");
        let counts = env
            .iter()
            .filter(|kv| kv.starts_with("GIT_CONFIG_COUNT="))
            .count();
        assert_eq!(counts, 1, "want exactly one GIT_CONFIG_COUNT, got {counts}");
        assert_eq!(env_value(&env, "GIT_CONFIG_COUNT"), Some("4"));
        assert!(
            !env.join("\n").contains("GIT_CONFIG_VALUE_0=x"),
            "inherited GIT_CONFIG entry should have been dropped"
        );
    }

    // Mirrors TestDefaultToolDirsIncludesCommonLocations.
    #[test]
    fn default_tool_dirs_includes_common_locations() {
        let dirs = default_tool_dirs("/Users/x");
        for want in ["/opt/homebrew/bin", "/usr/local/bin", "/Users/x/.local/bin"] {
            assert!(
                dirs.iter().any(|d| d == want),
                "default_tool_dirs missing {want}; got {dirs:?}"
            );
        }
    }

    // An empty home yields no user-local dirs (matches Go's `if home != ""` guard).
    #[test]
    fn default_tool_dirs_without_home_has_only_system_dirs() {
        let dirs = default_tool_dirs("");
        assert_eq!(dirs.len(), 8);
        assert!(!dirs.iter().any(|d| d.contains(".local")));
    }
}
