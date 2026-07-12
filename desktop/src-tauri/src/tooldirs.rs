//! The daemon's agent-launch PATH dirs (the "tool-dir resolution + override masking" of P7-D2).
//!
//! Parity port of two Go pieces the desktop app composes:
//!   - `agent_tool_dirs` <- `$REF/desktop/app.go`'s `agentToolDirs`
//!   - `override_dirs`   <- `$REF/desktop/internal/toolcheck/dirs.go`'s `OverrideDirs`
//!
//! The supervisor prepends these to the child PATH (first-wins), so a tool the user pointed at via a
//! per-tool override is resolved by the spawned agent ahead of a stock install of the same tool.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// Builds the daemon's agent-launch PATH dirs: per-tool override dirs FIRST, then the known-good
/// defaults ([`crate::supervisor::default_tool_dirs`]). Override dirs must take precedence — the
/// supervisor prepends `ToolDirs` to PATH first-wins, so otherwise a user override would be shadowed
/// by a stock install of the same tool, defeating the override (spec §6). Mirrors `agentToolDirs`.
pub fn agent_tool_dirs(home: &str, overrides: &HashMap<String, String>) -> Vec<String> {
    let mut dirs = override_dirs(overrides);
    dirs.extend(crate::supervisor::default_tool_dirs(home));
    dirs
}

/// Returns the unique parent directories of the (non-empty) override paths, sorted for determinism.
/// The supervisor prepends these to the daemon's agent-launch PATH so a tool the user pointed at via
/// an override is resolvable by the spawned agent (spec §6). Mirrors `OverrideDirs`.
pub fn override_dirs(overrides: &HashMap<String, String>) -> Vec<String> {
    // A BTreeSet dedups AND sorts lexically in one pass — the Go code dedups during (random) map
    // iteration then `sort.Strings` at the end; the sorted result is identical.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for p in overrides.values() {
        if p.is_empty() {
            continue;
        }
        // `Path::parent` yields Some("") for a bare filename (Go's `filepath.Dir` yields "."); both
        // are dropped by the empty/"." guard below.
        let dir = match Path::new(p).parent() {
            Some(d) => d.to_string_lossy().into_owned(),
            None => continue,
        };
        if dir.is_empty() || dir == "." {
            continue;
        }
        dirs.insert(dir);
    }
    dirs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::default_tool_dirs;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    // Mirrors TestOverrideDirs (dirs_test.go): unique sorted parent dirs; same-dir tools dedupe; an
    // empty override is ignored.
    #[test]
    fn override_dirs_returns_unique_sorted_parents() {
        let got = override_dirs(&map(&[
            ("gh", "/opt/homebrew/bin/gh"),
            ("gt", "/opt/homebrew/bin/gt"), // same dir as gh -> deduped
            ("claude", "/Users/x/.local/bin/claude"),
            ("git", ""), // empty override ignored
        ]));
        assert_eq!(got, vec!["/Users/x/.local/bin", "/opt/homebrew/bin"]);
    }

    // Mirrors TestOverrideDirsEmpty.
    #[test]
    fn override_dirs_empty() {
        assert!(override_dirs(&HashMap::new()).is_empty());
    }

    // Mirrors TestAgentToolDirsOverridesFirst: override dirs are prepended ahead of the defaults.
    #[test]
    fn agent_tool_dirs_overrides_first() {
        let dirs = agent_tool_dirs("/Users/x", &map(&[("claude", "/custom/bin/claude")]));
        assert_eq!(
            dirs.first().map(String::as_str),
            Some("/custom/bin"),
            "want the override dir /custom/bin first; got {dirs:?}"
        );
        // The defaults still follow, starting at len(dirs) - len(defaults).
        let defaults = default_tool_dirs("/Users/x");
        assert_eq!(dirs[dirs.len() - defaults.len()], defaults[0], "{dirs:?}");
    }

    // Mirrors TestProbeToolsSearchOrderMatchesDaemon: the override dir must PRECEDE the stock
    // defaults so the doctor and daemon resolve the same binary for an un-overridden tool.
    #[test]
    fn agent_tool_dirs_override_precedes_defaults() {
        let dirs = agent_tool_dirs("/Users/x", &map(&[("claude", "/custom/bin/claude")]));
        let defaults = default_tool_dirs("/Users/x");
        let override_at = dirs.iter().position(|d| d == "/custom/bin");
        let default_at = dirs.iter().position(|d| *d == defaults[0]);
        assert!(
            matches!((override_at, default_at), (Some(o), Some(d)) if o < d),
            "override dir must precede defaults; got {dirs:?} (override={override_at:?} default={default_at:?})"
        );
    }
}
