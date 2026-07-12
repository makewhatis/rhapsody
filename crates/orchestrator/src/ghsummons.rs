//! ghsummons — orchestrator-internal port of Go `internal/ghsummons`.
//!
//! Go's package has no dedicated Rust crate, so it lives here. O1 ported [`parse_repo`] (used by the
//! effective builder to derive `owner`/`repo` from a project's git remote for the GitHub-summons
//! feature's routing labels). O6 added the [`SummonSource`] abstraction + its [`SummonHit`] result —
//! the summons-enrichment source the poll path queries per repo ([`crate::ghenrich`]).
//!
//! P6-T1 completes the port by adding the concrete `gh`-exec [`GH`] (Go `ghsummons.GH` / `NewGH` /
//! `SummonsSince` — two `gh api` calls per repo per tick) that O6 deferred, implementing O6's async
//! [`SummonSource`] trait. It mirrors the `SummonsSince` tests. The daemon-wiring phase (F1)
//! constructs `GH::new(token, None)` as `o.gh_source`.

use std::collections::HashMap;
use std::sync::LazyLock;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use rhapsody_core::compile_summon_re;

/// Matches an ssh or https GitHub remote URL, capturing `owner` (group 1) and `repo` (group 2).
/// Mirrors Go `ghsummons.repoRe` (`github\.com[:/]([^/]+)/(.+?)(?:\.git)?/?$`). Compiled once;
/// `None` if the static pattern ever fails to compile (it cannot) — the no-panic idiom the sibling
/// crates use for static patterns (`rhapsody_tracker`'s `PR_PARSE_RE`).
static REPO_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"github\.com[:/]([^/]+)/(.+?)(?:\.git)?/?$").ok());

/// Matches an issue/PR number in a GitHub API `issue_url` / `pull_request_url`, capturing the digits
/// (group 1). Mirrors Go `ghsummons.numRe` (`/(?:issues|pulls)/(\d+)`); `\d` is spelled `[0-9]` so
/// it stays ASCII-only like Go's RE2 (Rust's `\d` is Unicode by default). `None` if the static
/// pattern ever fails to compile (it cannot) — the no-panic static-pattern idiom.
static NUM_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"/(?:issues|pulls)/([0-9]+)").ok());

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

/// Result of a single `gh` invocation: stdout bytes, or any error (Go's open `error`). Injectable so
/// tests can return a fixed body or a sentinel error, exactly as Go's `RunFunc` returns `([]byte,
/// error)`.
pub type RunResult = Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

/// Executes `gh` with args and returns stdout. Injectable for tests. Mirrors Go `ghsummons.RunFunc`;
/// the Rust port drops the explicit `context` — the poll path already bounds the whole query with a
/// `tokio::time::timeout` ([`crate::ghenrich`]).
pub type RunFn = Box<dyn Fn(&[&str]) -> RunResult + Send + Sync>;

/// A GitHub-backed [`SummonSource`]. Mirrors Go `ghsummons.GH`.
pub struct GH {
    re: Option<Regex>,
    run: RunFn,
}

impl GH {
    /// Creates a `GH` summon source for `token` (e.g. `"@symphony"`). Pass `None` for `run` to shell
    /// out to the real `gh` binary. Mirrors Go `ghsummons.NewGH` (nil run → `defaultRun`). The token
    /// matcher reuses [`compile_summon_re`] so the GitHub path matches identically to the Linear
    /// comment path; a token that (impossibly) fails to compile degrades to matching nothing.
    pub fn new(token: &str, run: Option<RunFn>) -> GH {
        GH {
            re: compile_summon_re(token).ok(),
            run: run.unwrap_or_else(|| Box::new(default_run)),
        }
    }
}

/// The default runner: `gh <args>`, returning stdout. A non-zero exit is an error (Go's
/// `exec.Command(...).Output()` returns an `*ExitError`). Only used in production — every test
/// injects its own runner. The exec is synchronous (Go's is too, run on a goroutine): it blocks the
/// calling executor worker for the two `gh` calls. Enrichment runs at most once per project per tick
/// and tokio's multi-worker runtime absorbs it, but because the call does not yield, `ghenrich`'s
/// surrounding `tokio::time::timeout` cannot interrupt a hung `gh` mid-call — a fully non-blocking
/// runner (`spawn_blocking` / `tokio::process`) is a daemon-wiring (F1) refinement when it first
/// constructs `o.gh_source`.
fn default_run(args: &[&str]) -> RunResult {
    let out = std::process::Command::new("gh").args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "gh {} exited with {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(out.stdout)
}

#[async_trait]
impl SummonSource for GH {
    /// Makes exactly two `gh api` calls (issues/comments + pulls/comments), matches the summon
    /// token, maps each matching comment to its PR number, and returns the maximum-time comment
    /// (with its body) per PR across both endpoints. Mirrors Go `GH.SummonsSince`.
    ///
    /// `--paginate --slurp` makes `gh` wrap all pages in a single outer array (`[[page1…],[page2…]]`,
    /// even for one page), so the body decodes as `[[comment…]…]` and every page is walked. Each
    /// summons is stamped at `updated_at` — the field the `since` listing filters on — so a token
    /// edited into an existing comment is timed at edit, not original creation; `created_at` is the
    /// fallback when `updated_at` is absent (never-edited comments have both equal).
    async fn summons_since(&self, owner: &str, repo: &str, since: DateTime<Utc>) -> SummonResult {
        let mut out: HashMap<i64, SummonHit> = HashMap::new();
        // An unbuildable token/number pattern (impossible) matches nothing.
        let (Some(re), Some(num_re)) = (self.re.as_ref(), NUM_RE.as_ref()) else {
            return Ok(out);
        };
        let since_str = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let endpoints = [
            format!("repos/{owner}/{repo}/issues/comments?since={since_str}&per_page=100"),
            format!("repos/{owner}/{repo}/pulls/comments?since={since_str}&per_page=100"),
        ];
        for ep in &endpoints {
            let body = (self.run)(&["api", "--paginate", "--slurp", ep.as_str()]).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("gh api {ep}: {e}").into()
                },
            )?;
            // `--slurp` wraps all pages into `[[page1…],[page2…]]`, even for a single page.
            let pages: Vec<Vec<serde_json::Value>> = serde_json::from_slice(&body).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("decode {ep}: {e}").into()
                },
            )?;
            for page in &pages {
                for c in page {
                    let str_field =
                        |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let body_s = str_field("body");
                    if !re.is_match(&body_s) {
                        continue;
                    }
                    // issue-comments carry `issue_url`; review-comments carry `pull_request_url`.
                    let issue_url = str_field("issue_url");
                    let url = if issue_url.is_empty() {
                        str_field("pull_request_url")
                    } else {
                        issue_url
                    };
                    let Some(n) = num_re
                        .captures(&url)
                        .and_then(|caps| caps.get(1))
                        .and_then(|m| m.as_str().parse::<i64>().ok())
                    else {
                        continue;
                    };
                    // Stamp at updated_at (edit time; the `since` filter's field), else created_at.
                    let updated = str_field("updated_at");
                    let ts = if updated.is_empty() {
                        str_field("created_at")
                    } else {
                        updated
                    };
                    let Ok(parsed) = DateTime::parse_from_rfc3339(&ts) else {
                        continue;
                    };
                    let t = parsed.with_timezone(&Utc);
                    // Keep the max-time comment per PR; ties keep the first seen (Go `t.After`).
                    match out.get(&n) {
                        Some(cur) if t <= cur.at => {}
                        _ => {
                            out.insert(
                                n,
                                SummonHit {
                                    at: t,
                                    body: body_s,
                                },
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeZone;

    use super::*;

    /// `time.Date(y, mo, d, h, mi, s, 0, UTC)` — a UTC instant for assertions.
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    // Mirrors Go `ghsummons.TestParseRepo`: ssh + https forms parse, a dotted repo name is kept
    // whole (the optional `.git` suffix must not greedily strip `.repo`), and a non-GitHub host or a
    // non-URL yields `None`.
    #[test]
    fn parse_repo_forms() {
        let cases = [
            (
                "git@github.com:example/example-core.git",
                Some(("example", "example-core")),
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

    /// A runner that answers by endpoint substring, counting invocations (the Go tests' `run`
    /// closure + `calls` counter). Unknown endpoints get an empty slurp `[[]]`.
    fn run_by_endpoint(
        issues: &'static str,
        pulls: &'static str,
        calls: Arc<AtomicUsize>,
    ) -> RunFn {
        Box::new(move |args| {
            calls.fetch_add(1, Ordering::SeqCst);
            for a in args {
                if a.contains("issues/comments") {
                    return Ok(issues.as_bytes().to_vec());
                }
                if a.contains("pulls/comments") {
                    return Ok(pulls.as_bytes().to_vec());
                }
            }
            Ok(b"[[]]".to_vec())
        })
    }

    // Mirrors Go `TestSummonsSinceMatchesAndPicksMax`: two endpoints (single slurped page each),
    // token-only matching, max-across-endpoints per PR.
    #[tokio::test]
    async fn summons_since_matches_and_picks_max() {
        // With `--slurp`, gh wraps even a single page as `[[…]]`.
        let issues = r#"[[
            {"body":"looks good","created_at":"2026-06-25T16:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/5335"},
            {"body":"@symphony fix CI","created_at":"2026-06-25T16:55:09Z","issue_url":"https://api.github.com/repos/o/r/issues/5335"},
            {"body":"@symphony do other","created_at":"2026-06-25T17:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/9999"}
        ]]"#;
        let pulls = r#"[[
            {"body":"@symphony also this line","created_at":"2026-06-25T16:58:00Z","pull_request_url":"https://api.github.com/repos/o/r/pulls/5335"}
        ]]"#;
        let calls = Arc::new(AtomicUsize::new(0));
        let src = GH::new(
            "@symphony",
            Some(run_by_endpoint(issues, pulls, calls.clone())),
        );
        let got = src
            .summons_since("o", "r", utc(2026, 6, 25, 15, 0, 0))
            .await
            .expect("summons_since");

        assert_eq!(calls.load(Ordering::SeqCst), 2, "issues + pulls");
        // max(16:55:09 issue, 16:58 inline)
        assert_eq!(
            got.get(&5335).map(|h| h.at),
            Some(utc(2026, 6, 25, 16, 58, 0))
        );
        assert_eq!(
            got.get(&9999).map(|h| h.at),
            Some(utc(2026, 6, 25, 17, 0, 0))
        );
    }

    // Mirrors Go `TestSummonsSinceUsesUpdatedAt`: stamp at updated_at (edit time), fall back to
    // created_at when absent.
    #[tokio::test]
    async fn summons_since_uses_updated_at() {
        let issues = r#"[[
            {"body":"@symphony fix this","created_at":"2026-06-25T16:00:00Z","updated_at":"2026-06-25T16:59:00Z","issue_url":"https://api.github.com/repos/o/r/issues/10"},
            {"body":"@symphony also","created_at":"2026-06-25T16:30:00Z","issue_url":"https://api.github.com/repos/o/r/issues/11"}
        ]]"#;
        let calls = Arc::new(AtomicUsize::new(0));
        let src = GH::new("@symphony", Some(run_by_endpoint(issues, "[[]]", calls)));
        let got = src
            .summons_since("o", "r", utc(2026, 6, 25, 15, 0, 0))
            .await
            .expect("summons_since");
        assert_eq!(
            got.get(&10).map(|h| h.at),
            Some(utc(2026, 6, 25, 16, 59, 0)),
            "edited-in summon stamped at edit time"
        );
        assert_eq!(
            got.get(&11).map(|h| h.at),
            Some(utc(2026, 6, 25, 16, 30, 0)),
            "fallback to created_at when updated_at absent"
        );
    }

    // Mirrors Go `TestSummonsSinceTwoPageSlurp` (FIX 1): a two-page slurped response
    // (`[[page1],[page2]]`) is fully walked — a tokened comment on page 2 registers.
    #[tokio::test]
    async fn summons_since_two_page_slurp() {
        let issues = r#"[
            [
                {"body":"no token here","created_at":"2026-06-25T10:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/1"}
            ],
            [
                {"body":"@symphony page two comment","created_at":"2026-06-25T11:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/2"}
            ]
        ]"#;
        let calls = Arc::new(AtomicUsize::new(0));
        let src = GH::new("@symphony", Some(run_by_endpoint(issues, "[[]]", calls)));
        let got = src
            .summons_since("o", "r", utc(2026, 6, 25, 9, 0, 0))
            .await
            .expect("summons_since");
        assert!(!got.contains_key(&1), "PR 1 has no summon token");
        assert_eq!(
            got.get(&2).map(|h| h.at),
            Some(utc(2026, 6, 25, 11, 0, 0)),
            "page-2 comment must register"
        );
    }

    // Mirrors Go `TestSummonsSinceReturnsBody` (INF-448): each hit carries the newest matching
    // comment's BODY, paired with its time; the newest wins for both fields.
    #[tokio::test]
    async fn summons_since_returns_body() {
        let issues = r#"[[
            {"body":"@symphony old ask","created_at":"2026-06-03T13:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/42"},
            {"body":"@symphony fix the MTU","created_at":"2026-06-03T14:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/42"}
        ]]"#;
        let calls = Arc::new(AtomicUsize::new(0));
        let src = GH::new("@symphony", Some(run_by_endpoint(issues, "[[]]", calls)));
        let got = src
            .summons_since("o", "r", utc(1, 1, 1, 0, 0, 0))
            .await
            .expect("summons_since");
        let h = got.get(&42).expect("a hit for PR 42");
        assert_eq!(
            h.at,
            utc(2026, 6, 3, 14, 0, 0),
            "newest tokened comment's time"
        );
        assert_eq!(
            h.body, "@symphony fix the MTU",
            "newest tokened comment's body"
        );
    }

    // Mirrors Go `TestSummonsSinceRunError` (FIX 5): a runner error is propagated (not swallowed),
    // wrapping the underlying cause with the offending endpoint.
    #[tokio::test]
    async fn summons_since_run_error() {
        let run: RunFn = Box::new(|_args| Err("gh: command not found".into()));
        let src = GH::new("@symphony", Some(run));
        let err = src
            .summons_since("o", "r", utc(2026, 6, 25, 15, 0, 0))
            .await
            .expect_err("expected an error");
        let msg = err.to_string();
        assert!(
            msg.contains("issues/comments"),
            "error names the first endpoint: {msg}"
        );
        assert!(
            msg.contains("gh: command not found"),
            "error wraps the underlying cause: {msg}"
        );
    }
}
