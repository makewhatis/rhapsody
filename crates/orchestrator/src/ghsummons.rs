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

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use rhapsody_core::compile_summon_matcher;

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
    /// matcher reuses [`compile_summon_matcher`] so the GitHub path matches identically to the
    /// Linear comment path — including accepting either brand spelling (STUDIO-603); a token that
    /// (impossibly) fails to compile degrades to matching nothing.
    pub fn new(token: &str, run: Option<RunFn>) -> GH {
        GH {
            re: compile_summon_matcher(token).ok(),
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

/// The fallible result of an [`OpenPrSource`] query: the open PR's browser URL, `None` when the
/// branch has no open pull request, or an error when the lookup itself could not be made. The three
/// cases are kept distinct because they mean different things to the quorum — "nothing to review",
/// "nothing to review", and "we do not know" — and only the third is worth a warning.
pub type OpenPrResult = Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>;

/// How many of a branch's open pull requests [`OpenPrSource::open_pr_for_branch`] inspects before
/// giving up. Greater than one because the head filter matches on branch NAME across forks, so a
/// fork's PR is ordered by recency among the repository's own and asking for just the newest would
/// let one hide the agent's; small because a branch with twenty open PRs is a repository anomaly,
/// not a case to reconcile here.
const PR_LIST_LIMIT: &str = "20";

/// Resolves the open pull request whose head is a given branch (STUDIO-674).
///
/// **No Go counterpart** — this is Rhapsody Teams' fallback for an installation whose Linear
/// GitHub integration never attaches PRs to issues, in which case the poller's candidate snapshot
/// carries no `linked_prs` and the review quorum has no PR to hand a reviewer. GitHub itself always
/// knows, because the agent pushed the branch; asking it by head branch is the source of truth the
/// attachment was only ever a cache of.
///
/// Object-safe (held as `dyn OpenPrSource` by the off-loop quorum task), so it is declared via
/// `async_trait`. Kept separate from [`SummonSource`] rather than added to it: `SummonSource` is a
/// field-for-field port of a Go interface, and a Rhapsody-only capability does not belong in it.
#[async_trait]
pub trait OpenPrSource: Send + Sync {
    async fn open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> OpenPrResult;
}

#[async_trait]
impl OpenPrSource for GH {
    /// One bounded `gh pr list --repo <owner>/<repo> --head <branch> --state open --json
    /// url,headRepositoryOwner --limit <PR_LIST_LIMIT>`, answering the newest matching PR whose
    /// head repository belongs to the account that was queried.
    ///
    /// `--state open` is the whole unmerged/unclosed filter, so the caller needs no second check.
    /// An empty owner, repo or branch is not an error and not a query — there is simply nothing to
    /// ask, so it answers `None` without spawning `gh`.
    ///
    /// **A pull request from a fork is rejected.** `--head` filters on the branch's name alone and
    /// matches forks too, so on a public repository anyone may open a PR whose head branch is
    /// called `symphony/<key>`; resolving it would point the quorum's reviewer agents at a
    /// stranger's diff, which the Linear-attachment path could never do. Every candidate's
    /// `headRepositoryOwner.login` is therefore compared against the owner that was asked about
    /// (case-insensitively — GitHub logins are), and a candidate that does not state its head
    /// repository at all (a deleted fork answers `null`) is rejected for the same reason: it cannot
    /// be shown to be ours. Rejected candidates are skipped rather than terminal, which is what
    /// [`PR_LIST_LIMIT`] is for.
    ///
    /// The comparison is on the OWNER and not on the whole `owner/repo` slug, deliberately. A fork
    /// under the same account is inside the same trust boundary — the point of the check is that a
    /// stranger cannot get a URL in here — and matching the slug would additionally break on a
    /// repository RENAME, where `gh` follows GitHub's redirect and answers under the canonical name
    /// while this daemon still asks under the stale one from config.
    async fn open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> OpenPrResult {
        if owner.is_empty() || repo.is_empty() || branch.is_empty() {
            return Ok(None);
        }
        let slug = format!("{owner}/{repo}");
        let args = [
            "pr",
            "list",
            "--repo",
            slug.as_str(),
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "url,headRepositoryOwner",
            "--limit",
            PR_LIST_LIMIT,
        ];
        let body = (self.run)(&args).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("gh pr list --repo {slug} --head {branch}: {e}").into()
        })?;
        let prs: Vec<serde_json::Value> = serde_json::from_slice(&body).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("decode gh pr list --repo {slug} --head {branch}: {e}").into()
            },
        )?;
        Ok(prs.iter().find_map(|p| {
            let url = p
                .get("url")
                .and_then(serde_json::Value::as_str)
                .filter(|u| !u.is_empty())?;
            let head_owner = p
                .get("headRepositoryOwner")
                .and_then(|o| o.get("login"))
                .and_then(serde_json::Value::as_str)?;
            head_owner
                .eq_ignore_ascii_case(owner)
                .then(|| url.to_string())
        }))
    }
}

/// The fallible result of a [`PrBranchSource`] query: the pull request's head branch, `None` when
/// there is no such PR in the queried repository (or its head is a fork's), or an error when the
/// lookup itself could not be made. [`OpenPrResult`]'s three cases and their reason.
pub type PrBranchResult = Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>;

/// Resolves the head branch of a pull request named by number (STUDIO-678, design §0.13).
///
/// **No Go counterpart**, and the exact inverse of [`OpenPrSource`]. STUDIO-674 established that
/// `symphony/<key>` is the link between a ticket and its pull request on installations whose Linear
/// holds no GitHub attachments; the manager's room reader needs that link read the other way, because
/// an operator pastes a PR URL and the ticket it belongs to is what has to be validated against the
/// team's projects. One `gh` call answers it, and the branch is then parsed back to a key by the
/// caller — the SAME frozen `symphony/<key>` contract, not a second convention.
///
/// Kept out of [`OpenPrSource`] rather than added to it: that trait is the quorum's PR gate and its
/// implementors exist to answer one question. A reader that needs both asks both.
#[async_trait]
pub trait PrBranchSource: Send + Sync {
    async fn head_branch_for_pr(&self, owner: &str, repo: &str, number: i64) -> PrBranchResult;
}

#[async_trait]
impl PrBranchSource for GH {
    /// One bounded `gh pr view <number> --repo <owner>/<repo> --json headRefName,headRepositoryOwner`.
    ///
    /// **A pull request whose head is a fork is rejected**, for the reason
    /// [`OpenPrSource::open_pr_for_branch`] rejects one: a stranger may open a PR against a public
    /// repository from a branch called anything at all, and resolving its head would let a pasted URL
    /// name a ticket key that this daemon then acts on. A candidate that does not state its head
    /// repository (a deleted fork answers `null`) is rejected for the same reason — it cannot be shown
    /// to be ours. The comparison is on the OWNER, case-insensitively, exactly as STUDIO-674's is.
    ///
    /// An empty owner or repo, or a non-positive number, is not an error and not a query: there is
    /// nothing to ask, so it answers `None` without spawning `gh`.
    async fn head_branch_for_pr(&self, owner: &str, repo: &str, number: i64) -> PrBranchResult {
        if owner.is_empty() || repo.is_empty() || number <= 0 {
            return Ok(None);
        }
        let slug = format!("{owner}/{repo}");
        let num = number.to_string();
        let args = [
            "pr",
            "view",
            num.as_str(),
            "--repo",
            slug.as_str(),
            "--json",
            "headRefName,headRepositoryOwner",
        ];
        let body = (self.run)(&args).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("gh pr view {num} --repo {slug}: {e}").into()
        })?;
        let pr: serde_json::Value = serde_json::from_slice(&body).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("decode gh pr view {num} --repo {slug}: {e}").into()
            },
        )?;
        let head_owner = pr
            .get("headRepositoryOwner")
            .and_then(|o| o.get("login"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !head_owner.eq_ignore_ascii_case(owner) {
            return Ok(None);
        }
        Ok(pr
            .get("headRefName")
            .and_then(serde_json::Value::as_str)
            .filter(|b| !b.is_empty())
            .map(str::to_string))
    }
}

/// Where a pull request stands, as GitHub's GraphQL `PullRequestState` reports it. Three values,
/// not two: the watcher retires a MERGED pull request and a CLOSED one for different reasons and
/// records them differently, and `mergedAt` alone cannot be trusted to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrStatus {
    Open,
    Merged,
    Closed,
}

/// What one number-keyed lookup learned about a pull request the daemon is entitled to look at.
///
/// `head_sha` is the reason this type exists: it is the only thing that distinguishes "the author
/// pushed fixes" from "nothing happened since the last poll", and no other `gh` helper in this
/// module returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    /// `headRefOid` — the exact commit at the head of the pull request.
    pub head_sha: String,
    pub status: PrStatus,
    /// `mergedAt`, when GitHub states one it can parse. Informational: [`PrStatus::Merged`] is what
    /// a caller acts on.
    pub merged_at: Option<DateTime<Utc>>,
    /// The head repository as `owner/repo` — the value the trust guard below accepted, kept so a
    /// caller can name it in a log without asking GitHub a second time.
    pub head_repo: String,
}

/// The outcome of a [`PrStateSource`] query, which is three-valued for a reason each case earns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLookup {
    /// GitHub answered, and the head repository is one this daemon may act on.
    Found(PrSnapshot),
    /// GitHub cannot resolve the pull request or its repository (a 404: deleted, transferred, or
    /// never there), so there is nothing left to observe — distinct from [`PrStatus::Closed`],
    /// which is a pull request that still exists and still has a head.
    Gone,
    /// The pull request exists but its head repository is neither the base nor allowlisted. A
    /// deliberate non-answer: see [`PrStateSource`].
    Untrusted,
}

/// The fallible result of a [`PrStateSource`] query. An `Err` is a lookup that could not be MADE —
/// kept distinct from every [`PrLookup`] variant because those are answers and this is not, and
/// because a caller must not retire a watched pull request on a network blip.
pub type PrStateResult = Result<PrLookup, Box<dyn std::error::Error + Send + Sync>>;

/// The head repositories a pull request may come from besides the base repository itself, as
/// lower-cased `owner/repo` slugs.
///
/// Empty by default and empty is the safe value: with no allowlist the only trusted head is the
/// base repository's own owner, which is [`OpenPrSource`]'s and [`PrBranchSource`]'s existing rule.
/// The set is passed per call rather than held on [`GH`] because the source is one shared object
/// while the trust boundary belongs to the caller's configuration (design record §15-a: a PR
/// coordinate is trusted because of where it came from, never because of what it says about
/// itself).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadAllowlist {
    slugs: HashSet<String>,
}

impl HeadAllowlist {
    /// The base repository and nothing else — the default trust boundary.
    pub fn none() -> HeadAllowlist {
        HeadAllowlist::default()
    }

    /// An allowlist over `owner/repo` slugs, folded to lower case so the comparison can be too.
    pub fn from_slugs<I, S>(slugs: I) -> HeadAllowlist
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        HeadAllowlist {
            slugs: slugs
                .into_iter()
                .map(|s| s.as_ref().trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Whether `slug` (an `owner/repo`) is explicitly allowed.
    pub fn allows(&self, slug: &str) -> bool {
        !self.slugs.is_empty() && self.slugs.contains(&slug.trim().to_ascii_lowercase())
    }
}

/// Resolves the head SHA and merge state of a pull request named by NUMBER (STUDIO-710; design
/// record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §14.2 and §14.4 slice 1).
///
/// **No Go counterpart** — Rhapsody Teams' ticketless PR review is a Rhapsody addition end to end,
/// and this is its foundation. The two existing number/branch helpers answer neither question the
/// review watcher asks: [`OpenPrSource::open_pr_for_branch`] returns a URL keyed by BRANCH, and
/// [`PrBranchSource::head_branch_for_pr`] returns a branch NAME keyed by number. `headRefOid`
/// appears nowhere. Re-review is triggered by the head ADVANCING past the SHA that was last
/// reviewed, and a merged or closed pull request must be dropped from the watch set, so the
/// watcher needs exactly `headRefOid` + `state` + `mergedAt`, per PR number, and nothing else.
///
/// **A head repository that is not the base's, and not allowlisted, is refused** — the security
/// property the whole subsystem rests on (§14.1 F-SEC). What a later slice does with the answer is
/// check out the head SHA and run an agent over its diff, so resolving a stranger's fork here
/// would be arbitrary code execution driven by a pull request anyone can open against a public
/// repository. The guard therefore lives inside the primitive, where it cannot be forgotten by a
/// caller, and it fails closed: a head that cannot be SHOWN to be trusted (a deleted fork's `null`,
/// an absent field) is [`PrLookup::Untrusted`] just like a stranger's.
///
/// The base side of that comparison is the OWNER the caller asked about, for the reason
/// [`OpenPrSource::open_pr_for_branch`] gives at length: an owner match keeps a same-account fork
/// inside the trust boundary and survives a repository rename, which a whole-slug match would not.
/// The allowlist, being explicit operator configuration, is matched on the full slug instead.
///
/// Object-safe (a later slice holds it as `dyn PrStateSource` on its off-loop task), so it is
/// declared via `async_trait`. Kept out of the other two traits for the reason they are kept apart:
/// each answers one question, and a caller that needs two asks twice.
#[async_trait]
pub trait PrStateSource: Send + Sync {
    async fn pr_state(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        allow: &HeadAllowlist,
    ) -> PrStateResult;
}

/// The `gh` failure messages that mean "this pull request is not there", as opposed to "the lookup
/// failed". `gh pr view` goes through GraphQL, which answers a missing PR and a missing repository
/// with two different `Could not resolve to …` messages; a caller injecting a REST runner sees
/// `HTTP 404` instead.
///
/// Matching on message text is not lovely, and it is what [`RunFn`] permits: the seam returns an
/// opaque error and keeps neither the exit status nor the stderr stream separately. The classifier
/// is therefore deliberately NARROW and fails closed — anything it does not recognise stays an
/// error. [`PrLookup::Gone`] is a caller's instruction to stop watching a pull request forever, so
/// a mis-classified rate limit or expired token would silently retire a live review, while a
/// mis-classified 404 only costs one more poll on the next tick.
///
/// One ambiguity it cannot resolve, named rather than hidden: GitHub answers a repository the
/// caller may not SEE with the same not-found as one that does not exist, so a token that loses
/// access to a repository reads as [`PrLookup::Gone`] for every pull request in it. That is the
/// right default — the alternative is erroring forever over a repository that really was deleted —
/// but it is why a caller should say out loud which pull request it retired and why.
fn is_gone_message(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("could not resolve to a pullrequest")
        || e.contains("could not resolve to a repository")
        || e.contains("http 404")
}

#[async_trait]
impl PrStateSource for GH {
    /// One bounded `gh pr view <number> --repo <owner>/<repo> --json
    /// headRefOid,state,mergedAt,headRepository,headRepositoryOwner`.
    ///
    /// An empty owner or repo, or a non-positive number, is not an error and not a query: nothing
    /// can ever be observed at a coordinate like that, so it answers [`PrLookup::Gone`] — the same
    /// "there is nothing here to watch" a deleted pull request gets — without spawning `gh`. An
    /// error would be worse, because a caller retries an error and this cannot improve.
    ///
    /// `headRefOid` and `state` have no safe default and a missing or unrecognised one is an error:
    /// an empty head SHA compares unequal to every SHA, which would make the watcher re-review the
    /// same pull request on every tick forever. `mergedAt` does have a safe default — `state`
    /// already carries the decision — so an unparseable timestamp degrades to `None`.
    async fn pr_state(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        allow: &HeadAllowlist,
    ) -> PrStateResult {
        if owner.is_empty() || repo.is_empty() || number <= 0 {
            return Ok(PrLookup::Gone);
        }
        let slug = format!("{owner}/{repo}");
        let num = number.to_string();
        let args = [
            "pr",
            "view",
            num.as_str(),
            "--repo",
            slug.as_str(),
            "--json",
            "headRefOid,state,mergedAt,headRepository,headRepositoryOwner",
        ];
        let body = match (self.run)(&args) {
            Ok(b) => b,
            Err(e) => {
                let msg = e.to_string();
                if is_gone_message(&msg) {
                    return Ok(PrLookup::Gone);
                }
                return Err(format!("gh pr view {num} --repo {slug}: {msg}").into());
            }
        };
        let pr: serde_json::Value = serde_json::from_slice(&body).map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("decode gh pr view {num} --repo {slug}: {e}").into()
            },
        )?;

        // The trust guard first: an untrusted head is a non-answer, so nothing else about the pull
        // request is worth parsing or reporting.
        let head_owner = pr
            .get("headRepositoryOwner")
            .and_then(|o| o.get("login"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let head_name = pr
            .get("headRepository")
            .and_then(|r| r.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // `nameWithOwner` is what `gh` returns; the pair is the fallback for a caller that asked
        // for fewer fields, and an empty owner leaves the slug empty rather than a bare `/repo`.
        let head_repo = pr
            .get("headRepository")
            .and_then(|r| r.get("nameWithOwner"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if head_owner.is_empty() || head_name.is_empty() {
                    String::new()
                } else {
                    format!("{head_owner}/{head_name}")
                }
            });
        let trusted = (!head_owner.is_empty() && head_owner.eq_ignore_ascii_case(owner))
            || (!head_repo.is_empty() && allow.allows(&head_repo));
        if !trusted {
            return Ok(PrLookup::Untrusted);
        }

        let head_sha = pr
            .get("headRefOid")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
                format!("gh pr view {num} --repo {slug}: no headRefOid in the answer").into()
            })?
            .to_string();
        let raw_state = pr
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let status = match raw_state.to_ascii_uppercase().as_str() {
            "OPEN" => PrStatus::Open,
            "MERGED" => PrStatus::Merged,
            "CLOSED" => PrStatus::Closed,
            other => {
                return Err(format!(
                    "gh pr view {num} --repo {slug}: unrecognised state {other:?}"
                )
                .into());
            }
        };
        let merged_at = pr
            .get("mergedAt")
            .and_then(serde_json::Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        Ok(PrLookup::Found(PrSnapshot {
            head_sha,
            status,
            merged_at,
            head_repo,
        }))
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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

    // STUDIO-603: the GitHub PR-comment path accepts EITHER brand spelling, whichever is
    // configured — proving `GH::new` wires `compile_summon_matcher`, so the two summon paths stay
    // identical (the Linear path has the mirror of this test).
    #[tokio::test]
    async fn summons_since_detects_either_brand_token() {
        let issues = r#"[[
            {"body":"@symphony fix CI","created_at":"2026-06-25T16:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/1"},
            {"body":"@rhapsody fix CI","created_at":"2026-06-25T17:00:00Z","issue_url":"https://api.github.com/repos/o/r/issues/2"}
        ]]"#;
        for configured in ["@symphony", "@rhapsody"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let src = GH::new(
                configured,
                Some(run_by_endpoint(issues, "[[]]", calls.clone())),
            );
            let got = src
                .summons_since("o", "r", utc(2026, 6, 25, 15, 0, 0))
                .await
                .expect("summons_since");
            assert_eq!(
                got.get(&1).map(|h| h.at),
                Some(utc(2026, 6, 25, 16, 0, 0)),
                "configured {configured:?} must detect the @symphony spelling"
            );
            assert_eq!(
                got.get(&2).map(|h| h.at),
                Some(utc(2026, 6, 25, 17, 0, 0)),
                "configured {configured:?} must detect the @rhapsody spelling"
            );
        }
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

    // ── head_branch_for_pr (STUDIO-678, §0.13; no Go counterpart) ──────────────────────────────

    /// The happy path: one bounded `gh pr view`, and the head branch read out of it. The argv is
    /// asserted in full because it IS the contract with GitHub.
    #[tokio::test]
    async fn head_branch_for_pr_returns_the_head_branch() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefName":"symphony/STUDIO-654","headRepositoryOwner":{"login":"o"}}"#,
                Arc::clone(&seen),
            )),
        );

        let got = src
            .head_branch_for_pr("o", "r", 230)
            .await
            .expect("head_branch_for_pr");

        assert_eq!(got.as_deref(), Some("symphony/STUDIO-654"));
        assert_eq!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec!["pr view 230 --repo o/r --json headRefName,headRepositoryOwner".to_string()],
        );
    }

    /// A fork's pull request resolves to NOTHING. `gh pr view` will happily describe a stranger's
    /// PR against a public repository, and its head branch could be called `symphony/STUDIO-1` —
    /// which would let a pasted URL name one of this team's tickets. The owner check is what stops
    /// a URL from choosing its own answer.
    #[tokio::test]
    async fn head_branch_for_pr_rejects_a_forks_pull_request() {
        for body in [
            r#"{"headRefName":"symphony/STUDIO-654","headRepositoryOwner":{"login":"stranger"}}"#,
            r#"{"headRefName":"symphony/STUDIO-654","headRepositoryOwner":null}"#,
            r#"{"headRefName":"symphony/STUDIO-654"}"#,
        ] {
            let src = GH::new(
                "@symphony",
                Some(run_recording(body, Arc::new(Mutex::new(Vec::new())))),
            );
            assert_eq!(
                src.head_branch_for_pr("o", "r", 230).await.expect("lookup"),
                None,
                "a head this daemon cannot show is its own must not resolve: {body}"
            );
        }
    }

    /// A login differing only in case is the same account — GitHub logins are case-insensitive, and
    /// config spells the owner however the operator typed it.
    #[tokio::test]
    async fn head_branch_for_pr_matches_the_owner_case_insensitively() {
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefName":"symphony/STUDIO-654","headRepositoryOwner":{"login":"O"}}"#,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        assert_eq!(
            src.head_branch_for_pr("o", "r", 230).await.expect("lookup"),
            Some("symphony/STUDIO-654".to_string())
        );
    }

    /// Nothing to ask ⇒ no `gh` process. An empty repo or a zero number is a caller that parsed
    /// nothing, not a query.
    #[tokio::test]
    async fn head_branch_for_pr_without_a_target_asks_nothing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new("@symphony", Some(run_recording("[]", Arc::clone(&seen))));
        for (owner, repo, n) in [("", "r", 1), ("o", "", 1), ("o", "r", 0), ("o", "r", -3)] {
            assert_eq!(
                src.head_branch_for_pr(owner, repo, n)
                    .await
                    .expect("lookup"),
                None
            );
        }
        assert!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "no gh call should have been made"
        );
    }

    /// A lookup that could not be MADE is an error, distinct from "no such PR" — the caller warns
    /// on one and stays quiet on the other.
    #[tokio::test]
    async fn head_branch_for_pr_propagates_a_failed_lookup() {
        let src = GH::new(
            "@symphony",
            Some(Box::new(|_: &[&str]| Err("gh: not logged in".into()))),
        );
        assert!(src.head_branch_for_pr("o", "r", 1).await.is_err());

        let undecodable = GH::new(
            "@symphony",
            Some(run_recording("not json", Arc::new(Mutex::new(Vec::new())))),
        );
        assert!(undecodable.head_branch_for_pr("o", "r", 1).await.is_err());
    }

    // ── open_pr_for_branch (STUDIO-674; no Go counterpart) ──────────────────────────────────────

    /// One open PR on the branch, opened from the queried repository itself.
    const PR_LIST_OWN: &str = r#"[{"url":"https://github.com/o/r/pull/64",
        "headRepositoryOwner":{"login":"o"}}]"#;

    /// A runner that records the argv it was handed and answers with `body`.
    fn run_recording(body: &'static str, seen: Arc<Mutex<Vec<String>>>) -> RunFn {
        Box::new(move |args| {
            seen.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(args.join(" "));
            Ok(body.as_bytes().to_vec())
        })
    }

    // The happy path: one bounded `gh pr list` scoped to the repo AND the head branch, and the
    // browser URL is read out of it. The argv is asserted in full because it IS the contract with
    // GitHub — a dropped `--state open` would hand a reviewer a merged PR.
    #[tokio::test]
    async fn open_pr_for_branch_returns_the_open_prs_url() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(PR_LIST_OWN, Arc::clone(&seen))),
        );

        let got = src
            .open_pr_for_branch("o", "r", "symphony/MT-1")
            .await
            .expect("open_pr_for_branch");

        assert_eq!(got.as_deref(), Some("https://github.com/o/r/pull/64"));
        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            calls,
            vec![
                "pr list --repo o/r --head symphony/MT-1 --state open --json \
                 url,headRepositoryOwner --limit 20"
                    .to_string()
            ],
            "exactly one gh call, scoped to the repo and the head branch"
        );
    }

    // A branch with no open PR is `None`, not an error: the quorum treats it as "nothing to
    // review", which is a normal outcome and not a failure to report.
    #[tokio::test]
    async fn open_pr_for_branch_no_open_pr_is_none() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new("@symphony", Some(run_recording("[]", Arc::clone(&seen))));
        assert_eq!(
            src.open_pr_for_branch("o", "r", "symphony/MT-1")
                .await
                .expect("open_pr_for_branch"),
            None
        );
    }

    // An empty owner/repo/branch asks GitHub nothing at all — a `gh pr list --repo /` would be a
    // guaranteed error, and "we were never told the repo" is not an error worth reporting.
    #[tokio::test]
    async fn open_pr_for_branch_without_a_repo_asks_nothing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(PR_LIST_OWN, Arc::clone(&seen))),
        );
        for (owner, repo, branch) in [
            ("", "r", "symphony/MT-1"),
            ("o", "", "symphony/MT-1"),
            ("o", "r", ""),
        ] {
            assert_eq!(
                src.open_pr_for_branch(owner, repo, branch)
                    .await
                    .expect("open_pr_for_branch"),
                None,
                "({owner:?}, {repo:?}, {branch:?})"
            );
        }
        assert!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "no gh call is made without a repo and a branch"
        );
    }

    // A runner error is propagated (not swallowed as "no PR"), wrapping the cause and naming the
    // repo/branch — `summons_since`'s contract, for its reason: the caller must be able to tell
    // "GitHub says there is none" from "we could not ask".
    #[tokio::test]
    async fn open_pr_for_branch_run_error_is_propagated() {
        let run: RunFn = Box::new(|_args| Err("gh: command not found".into()));
        let src = GH::new("@symphony", Some(run));
        let msg = src
            .open_pr_for_branch("o", "r", "symphony/MT-1")
            .await
            .expect_err("expected an error")
            .to_string();
        assert!(msg.contains("o/r"), "error names the repo: {msg}");
        assert!(
            msg.contains("symphony/MT-1"),
            "error names the branch: {msg}"
        );
        assert!(
            msg.contains("gh: command not found"),
            "error wraps the underlying cause: {msg}"
        );
    }

    // Unparseable output is an error too, for the same reason: silently reading it as "no PR"
    // would turn a broken `gh` into a permanently quiet quorum.
    #[tokio::test]
    async fn open_pr_for_branch_undecodable_output_is_an_error() {
        let run: RunFn = Box::new(|_args| Ok(b"not json".to_vec()));
        let src = GH::new("@symphony", Some(run));
        let msg = src
            .open_pr_for_branch("o", "r", "symphony/MT-1")
            .await
            .expect_err("expected an error")
            .to_string();
        assert!(msg.contains("decode"), "error says what failed: {msg}");
    }

    // A public repository's branch-name filter also matches FORKS, so a third party can open a PR
    // whose head branch is `symphony/<key>`. Resolving it would aim the quorum's reviewer agents at
    // a stranger's diff — a review and prompt-injection surface the Linear-attachment path never
    // had — so a PR whose head repository is not the queried one is not a match at all.
    #[tokio::test]
    async fn open_pr_for_branch_rejects_a_forks_pull_request() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"[{"url":"https://github.com/stranger/r/pull/9",
                    "headRepositoryOwner":{"login":"stranger"}}]"#,
                Arc::clone(&seen),
            )),
        );
        assert_eq!(
            src.open_pr_for_branch("o", "r", "symphony/MT-1")
                .await
                .expect("open_pr_for_branch"),
            None,
            "a fork's PR on the same branch name is not this repo's PR"
        );
    }

    // …and rejecting it must not cost the agent its OWN review: `gh` orders by recency, so a fork's
    // PR opened after the agent's would come first. The scan continues past it.
    #[tokio::test]
    async fn open_pr_for_branch_skips_a_fork_to_reach_the_repos_own() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"[{"url":"https://github.com/stranger/r/pull/9",
                     "headRepositoryOwner":{"login":"stranger"}},
                    {"url":"https://github.com/o/r/pull/64",
                     "headRepositoryOwner":{"login":"o"}}]"#,
                Arc::clone(&seen),
            )),
        );
        assert_eq!(
            src.open_pr_for_branch("o", "r", "symphony/MT-1")
                .await
                .expect("open_pr_for_branch")
                .as_deref(),
            Some("https://github.com/o/r/pull/64"),
            "a newer fork PR must not hide the repository's own"
        );
    }

    // A candidate that states no head repository (a deleted fork answers `null`) cannot be shown to
    // be ours, so it is rejected on the same rule rather than accepted by omission.
    #[tokio::test]
    async fn open_pr_for_branch_rejects_a_pr_with_no_head_repository() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"[{"url":"https://github.com/o/r/pull/64","headRepositoryOwner":null}]"#,
                Arc::clone(&seen),
            )),
        );
        assert_eq!(
            src.open_pr_for_branch("o", "r", "symphony/MT-1")
                .await
                .expect("open_pr_for_branch"),
            None
        );
    }

    // GitHub logins are case-insensitive, and the owner this daemon asks with comes from a git
    // remote a human typed. Matching case-sensitively would reject the repository's own PR.
    #[tokio::test]
    async fn open_pr_for_branch_matches_the_owner_case_insensitively() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"[{"url":"https://github.com/MakeWhatIs/r/pull/64",
                     "headRepositoryOwner":{"login":"MakeWhatIs"}}]"#,
                Arc::clone(&seen),
            )),
        );
        assert_eq!(
            src.open_pr_for_branch("makewhatis", "r", "symphony/MT-1")
                .await
                .expect("open_pr_for_branch")
                .as_deref(),
            Some("https://github.com/MakeWhatIs/r/pull/64")
        );
    }

    // ── pr_state (STUDIO-710, slice 1; design record §14.2, §15; no Go counterpart) ─────────────

    /// The captured payload of a live open pull request (`gh pr view 86 --repo makewhatis/rhapsody
    /// --json headRefOid,state,mergedAt,headRepository,headRepositoryOwner`, 2026-09-02), with the
    /// owner renamed to the `o/r` the other tests use.
    const PR_VIEW_OPEN: &str = r#"{
        "headRefOid":"93db6e8ec3b7c54071eb031ebac3be71eee1008a",
        "headRepository":{"id":"R_kgDOTcp16A","name":"r","nameWithOwner":"o/r"},
        "headRepositoryOwner":{"id":"MDQ6VXNlcjczMDg0OA==","name":"David Johansen","login":"o"},
        "mergedAt":null,
        "state":"OPEN"
    }"#;

    /// The same capture for a merged pull request (`gh pr view 84`, same run).
    const PR_VIEW_MERGED: &str = r#"{
        "headRefOid":"df574d9a665c6987d7c72d65f052ff5422862bc3",
        "headRepository":{"id":"R_kgDOTcp16A","name":"r","nameWithOwner":"o/r"},
        "headRepositoryOwner":{"id":"MDQ6VXNlcjczMDg0OA==","name":"David Johansen","login":"o"},
        "mergedAt":"2026-09-02T04:37:59Z",
        "state":"MERGED"
    }"#;

    /// The happy path: one bounded `gh pr view`, and the three fields the watcher needs read out of
    /// it. The argv is asserted in full because it IS the contract with GitHub — a dropped
    /// `headRefOid` would leave the watcher unable to see a head advance at all.
    #[tokio::test]
    async fn pr_state_returns_the_head_sha_state_and_merged_at() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new(
            "@symphony",
            Some(run_recording(PR_VIEW_OPEN, Arc::clone(&seen))),
        );

        let got = src
            .pr_state("o", "r", 86, &HeadAllowlist::none())
            .await
            .expect("pr_state");

        assert_eq!(
            got,
            PrLookup::Found(PrSnapshot {
                head_sha: "93db6e8ec3b7c54071eb031ebac3be71eee1008a".to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: "o/r".to_string(),
            })
        );
        assert_eq!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec![
                "pr view 86 --repo o/r --json \
                 headRefOid,state,mergedAt,headRepository,headRepositoryOwner"
                    .to_string()
            ],
        );
    }

    /// Merged and closed are DIFFERENT terminal states and the watcher records them differently, so
    /// the one field that distinguishes them (`state`, which `gh` answers as the GraphQL enum) is
    /// read rather than inferred from `mergedAt` being present.
    #[tokio::test]
    async fn pr_state_distinguishes_merged_from_closed() {
        let merged = GH::new(
            "@symphony",
            Some(run_recording(
                PR_VIEW_MERGED,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        let got = merged
            .pr_state("o", "r", 84, &HeadAllowlist::none())
            .await
            .expect("pr_state");
        assert_eq!(
            got,
            PrLookup::Found(PrSnapshot {
                head_sha: "df574d9a665c6987d7c72d65f052ff5422862bc3".to_string(),
                status: PrStatus::Merged,
                merged_at: Some(utc(2026, 9, 2, 4, 37, 59)),
                head_repo: "o/r".to_string(),
            })
        );

        let closed = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefOid":"abc","state":"CLOSED","mergedAt":null,
                     "headRepository":{"nameWithOwner":"o/r"},
                     "headRepositoryOwner":{"login":"o"}}"#,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        assert_eq!(
            closed
                .pr_state("o", "r", 12, &HeadAllowlist::none())
                .await
                .expect("pr_state"),
            PrLookup::Found(PrSnapshot {
                head_sha: "abc".to_string(),
                status: PrStatus::Closed,
                merged_at: None,
                head_repo: "o/r".to_string(),
            }),
            "a closed-unmerged PR must not be reported as merged"
        );
    }

    /// A pull request (or repository) GitHub can no longer resolve is `Gone` — a distinct answer
    /// from `Closed`, because the watcher retires a gone PR without ever learning a terminal state
    /// for it. Both of `gh`'s GraphQL not-found messages count, and so does a REST `HTTP 404` for a
    /// caller that injects one.
    #[tokio::test]
    async fn pr_state_maps_a_missing_pull_request_to_gone() {
        for stderr in [
            "gh pr view 999999 --repo o/r exited with exit status 1: GraphQL: Could not resolve \
             to a PullRequest with the number of 999999. (repository.pullRequest)",
            "GraphQL: Could not resolve to a Repository with the name 'o/r'. (repository)",
            "gh: Not Found (HTTP 404)",
        ] {
            let src = GH::new(
                "@symphony",
                Some(Box::new(move |_: &[&str]| Err(stderr.into()))),
            );
            assert_eq!(
                src.pr_state("o", "r", 999_999, &HeadAllowlist::none())
                    .await
                    .expect("a not-found PR is an answer, not a failure"),
                PrLookup::Gone,
                "{stderr}"
            );
        }
    }

    /// Any OTHER failure stays an error. `Gone` means "stop watching this pull request", so
    /// classifying a network blip or an expired token as gone would silently retire a live PR from
    /// review — the lookup fails closed instead, and the caller retries on its own cadence.
    #[tokio::test]
    async fn pr_state_does_not_mistake_a_failed_lookup_for_gone() {
        for stderr in [
            "gh: not logged in to any GitHub hosts",
            "dial tcp: lookup api.github.com: no such host",
            "gh: API rate limit exceeded (HTTP 403)",
        ] {
            let src = GH::new(
                "@symphony",
                Some(Box::new(move |_: &[&str]| Err(stderr.into()))),
            );
            assert!(
                src.pr_state("o", "r", 86, &HeadAllowlist::none())
                    .await
                    .is_err(),
                "{stderr} must not retire a watched PR"
            );
        }
        let undecodable = GH::new(
            "@symphony",
            Some(run_recording("not json", Arc::new(Mutex::new(Vec::new())))),
        );
        assert!(
            undecodable
                .pr_state("o", "r", 86, &HeadAllowlist::none())
                .await
                .is_err()
        );
    }

    /// A pull request whose head is a fork is `Untrusted` — never `Found`. The head SHA of a
    /// stranger's branch is what a review run would check out and execute, so a head this daemon
    /// cannot show is its own (a different owner, a deleted fork's `null`, or an absent field)
    /// resolves to nothing reviewable.
    #[tokio::test]
    async fn pr_state_rejects_a_fork_or_off_allowlist_head() {
        for body in [
            r#"{"headRefOid":"abc","state":"OPEN","mergedAt":null,
                 "headRepository":{"nameWithOwner":"stranger/r"},
                 "headRepositoryOwner":{"login":"stranger"}}"#,
            r#"{"headRefOid":"abc","state":"OPEN","mergedAt":null,
                 "headRepository":null,"headRepositoryOwner":null}"#,
            r#"{"headRefOid":"abc","state":"OPEN","mergedAt":null}"#,
        ] {
            let src = GH::new(
                "@symphony",
                Some(run_recording(body, Arc::new(Mutex::new(Vec::new())))),
            );
            assert_eq!(
                src.pr_state("o", "r", 7, &HeadAllowlist::none())
                    .await
                    .expect("lookup"),
                PrLookup::Untrusted,
                "{body}"
            );
        }
    }

    /// The base repository is trusted without an allowlist, and an explicitly allowlisted head repo
    /// is trusted too — that is the whole "base / an allowlisted repo" rule. Both comparisons are
    /// case-insensitive, because GitHub logins are and the operator types the allowlist by hand.
    #[tokio::test]
    async fn pr_state_trusts_the_base_owner_and_an_allowlisted_head() {
        let own = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefOid":"abc","state":"OPEN","mergedAt":null,
                     "headRepository":{"nameWithOwner":"O/R"},
                     "headRepositoryOwner":{"login":"O"}}"#,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        assert!(matches!(
            own.pr_state("o", "r", 7, &HeadAllowlist::none())
                .await
                .expect("lookup"),
            PrLookup::Found(_)
        ));

        let allowed = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefOid":"abc","state":"OPEN","mergedAt":null,
                     "headRepository":{"nameWithOwner":"Partner/Fork"},
                     "headRepositoryOwner":{"login":"Partner"}}"#,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        let allow = HeadAllowlist::from_slugs(["partner/fork"]);
        assert!(
            matches!(
                allowed.pr_state("o", "r", 7, &allow).await.expect("lookup"),
                PrLookup::Found(_)
            ),
            "an allowlisted head repo is trusted"
        );
        assert_eq!(
            allowed
                .pr_state("o", "r", 7, &HeadAllowlist::from_slugs(["partner/other"]))
                .await
                .expect("lookup"),
            PrLookup::Untrusted,
            "a DIFFERENT repo under an allowlisted owner is not allowlisted"
        );
    }

    /// A head SHA is the one field with no safe default: without it the watcher cannot tell a head
    /// advance from a re-poll. An answer missing it is a shape this code does not understand, so it
    /// fails rather than reporting an empty SHA that would compare unequal to everything.
    #[tokio::test]
    async fn pr_state_without_a_head_sha_is_an_error() {
        for body in [
            r#"{"state":"OPEN","mergedAt":null,"headRepositoryOwner":{"login":"o"}}"#,
            r#"{"headRefOid":"","state":"OPEN","headRepositoryOwner":{"login":"o"}}"#,
            r#"{"headRefOid":"abc","state":"WAT","headRepositoryOwner":{"login":"o"}}"#,
        ] {
            let src = GH::new(
                "@symphony",
                Some(run_recording(body, Arc::new(Mutex::new(Vec::new())))),
            );
            assert!(
                src.pr_state("o", "r", 7, &HeadAllowlist::none())
                    .await
                    .is_err(),
                "{body}"
            );
        }
    }

    /// An unparseable `mergedAt` is not fatal: `state` already carries the merged/closed decision
    /// the watcher acts on, so the timestamp degrades to absent rather than failing a lookup that
    /// otherwise answered everything asked of it.
    #[tokio::test]
    async fn pr_state_tolerates_an_unparseable_merged_at() {
        let src = GH::new(
            "@symphony",
            Some(run_recording(
                r#"{"headRefOid":"abc","state":"MERGED","mergedAt":"yesterday",
                     "headRepositoryOwner":{"login":"o"},
                     "headRepository":{"nameWithOwner":"o/r"}}"#,
                Arc::new(Mutex::new(Vec::new())),
            )),
        );
        assert_eq!(
            src.pr_state("o", "r", 7, &HeadAllowlist::none())
                .await
                .expect("lookup"),
            PrLookup::Found(PrSnapshot {
                head_sha: "abc".to_string(),
                status: PrStatus::Merged,
                merged_at: None,
                head_repo: "o/r".to_string(),
            })
        );
    }

    /// Nothing to ask ⇒ no `gh` process. A coordinate with no owner, no repo or no number can never
    /// be observed at all, so it answers `Gone` — the same "stop watching this" a deleted pull
    /// request gets — rather than an error the caller would retry forever.
    #[tokio::test]
    async fn pr_state_without_a_target_asks_nothing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let src = GH::new("@symphony", Some(run_recording("{}", Arc::clone(&seen))));
        for (owner, repo, n) in [("", "r", 1), ("o", "", 1), ("o", "r", 0), ("o", "r", -3)] {
            assert_eq!(
                src.pr_state(owner, repo, n, &HeadAllowlist::none())
                    .await
                    .expect("lookup"),
                PrLookup::Gone
            );
        }
        assert!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "no gh call should have been made"
        );
    }
}
