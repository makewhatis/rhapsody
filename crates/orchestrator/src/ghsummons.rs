//! ghsummons — orchestrator-internal port of Go `internal/ghsummons`.
//!
//! Go's package has no dedicated Rust crate, so it lives here. O1 ports only [`parse_repo`] (used
//! by the effective builder to derive `owner`/`repo` from a project's git remote for the
//! GitHub-summons feature's routing labels). The GitHub PR-comment `SummonSource` polling itself is
//! the enrichment ticket's concern (O6) and is not ported here.

use std::sync::LazyLock;

use regex::Regex;

/// Matches an ssh or https GitHub remote URL, capturing `owner` (group 1) and `repo` (group 2).
/// Mirrors Go `ghsummons.repoRe` (`github\.com[:/]([^/]+)/(.+?)(?:\.git)?/?$`). Compiled once;
/// `None` if the static pattern ever fails to compile (it cannot) — the no-panic idiom the sibling
/// crates use for static patterns (`rhapsody_tracker`'s `PR_PARSE_RE`).
static REPO_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"github\.com[:/]([^/]+)/(.+?)(?:\.git)?/?$").ok());

/// Extracts `(owner, repo)` from an ssh or https GitHub remote URL, or `None` when it does not
/// match. Mirrors Go `ghsummons.ParseRepo` (which returns `(owner, repo, ok)`; the caller reads the
/// pair and treats `!ok` as empty owner/repo). Supported forms:
///
///   - `git@github.com:owner/repo.git`
///   - `https://github.com/owner/repo.git`
///   - `https://github.com/owner/repo`
pub fn parse_repo(repo_url: &str) -> Option<(String, String)> {
    let re = REPO_RE.as_ref()?;
    let caps = re.captures(repo_url)?;
    Some((caps[1].to_string(), caps[2].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `ghsummons.TestParseRepo`: ssh + https forms parse, a dotted repo name is kept
    // whole (the optional `.git` suffix must not greedily strip `.repo`), and a non-GitHub host or a
    // non-URL yields `None`.
    #[test]
    fn parse_repo_forms() {
        let cases = [
            (
                "git@github.com:makewhatis/symphony-core.git",
                Some(("makewhatis", "symphony-core")),
            ),
            ("https://github.com/o/r.git", Some(("o", "r"))),
            ("https://github.com/o/r", Some(("o", "r"))),
            // A dotted repo name must be accepted whole (Go FIX 4).
            ("https://github.com/o/my.repo", Some(("o", "my.repo"))),
            ("https://gitlab.com/o/r", None),
            ("not a url", None),
        ];
        for (input, want) in cases {
            let want = want.map(|(o, r)| (o.to_string(), r.to_string()));
            assert_eq!(parse_repo(input), want, "parse_repo({input:?})");
        }
    }
}
