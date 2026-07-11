//! ghsummons — orchestrator-internal port of Go `internal/ghsummons`.
//!
//! Go's package has no dedicated Rust crate, so it lives here. O1 ported [`parse_repo`] (used by the
//! effective builder to derive `owner`/`repo` from a project's git remote for the GitHub-summons
//! feature's routing labels). O6 adds the [`SummonSource`] abstraction + its [`SummonHit`] result —
//! the summons-enrichment source the poll path queries per repo ([`crate::ghenrich`]).
//!
//! Deviation: the concrete `gh`-exec [`SummonSource`] (Go `ghsummons.GH` / `NewGH` — two `gh api`
//! calls per repo per tick) is NOT ported here. It is pure runtime infrastructure with no
//! P5-orchestrator test (every P5 caller, incl. O7's `poll_all_projects`, uses a fake source), so it
//! is deferred to the daemon-wiring phase that first constructs `o.gh_source`. This module ports the
//! trait ([`SummonSource`]) + result type ([`SummonHit`]) the enrichment logic consumes.

use std::collections::HashMap;
use std::sync::LazyLock;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
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

/// The newest summons on a PR: the comment time and its body. The body rides along so a mid-run
/// summons can be delivered to the live run's operator mailbox with the actual instruction, not just
/// a timestamp (INF-448). Mirrors Go `ghsummons.SummonHit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummonHit {
    pub at: DateTime<Utc>,
    pub body: String,
}

/// The fallible result of a [`SummonSource`] query. The error is opaque (boxed) because the only
/// caller ([`crate::ghenrich::fetch_github_summons`]) distinguishes ok-vs-error only — it logs the
/// error and treats a failed fetch as "nothing to apply this tick" — mirroring Go's plain `error`
/// return.
pub type SummonResult = Result<HashMap<i64, SummonHit>, Box<dyn std::error::Error + Send + Sync>>;

/// Returns, per PR number, the newest summons (time + body) whose comment contains the summon token
/// at/after `since`, across BOTH issue-comments and PR review comments for the repo. Mirrors Go
/// `ghsummons.SummonSource` (`SummonsSince`). Object-safe (held as `dyn SummonSource` by the poll
/// path's enrichment source), so it is declared via `async_trait`. The map key is the PR number
/// (Go's `int` → the port's `i64`, matching [`rhapsody_core::LinkedPRRef::number`]).
#[async_trait]
pub trait SummonSource: Send + Sync {
    async fn summons_since(&self, owner: &str, repo: &str, since: DateTime<Utc>) -> SummonResult;
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
