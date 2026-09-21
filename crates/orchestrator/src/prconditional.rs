//! prconditional — a [`PrStateSource`] that talks to GitHub's REST API directly and makes an
//! unchanged pull request cost no primary rate limit (STUDIO-974).
//!
//! **No Go v0.4.0 counterpart** — the ticketless PR review subsystem is a Rhapsody addition, and
//! this is a Rhapsody-only transport for one of its reads.
//!
//! # Why this exists beside the `gh`-subprocess [`GH`](crate::ghsummons::GH)
//!
//! The review watcher re-asks GitHub where every watched pull request stands on a timer. Its
//! cadence was pinned at 120s ([`crate::prstate::PR_STATE_POLL_INTERVAL`]) because a full sweep is
//! ~600 requests an hour against the account's 5,000/hour budget, shared with every other `gh`
//! call the daemon and its agents make — and since STUDIO-953 a tick makes up to twice the sweep's
//! calls. The budget is finite and shared, which is why the answer is to attack the COST of a poll
//! rather than merely lower the constant; an earlier draft's claim that the budget was exhausted on
//! 2026-09-21 was retracted (see the STUDIO-974 ticket), so nothing here rests on it. With an
//! unchanged poll now free, the configurable cadence defaults to 15s
//! (`rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS`).
//!
//! GitHub answers a conditional `GET` with `304 Not Modified` when the sent `If-None-Match` ETag
//! still matches, and a 304 does **not** count against the primary rate limit (verified against the
//! live API — see the STUDIO-974 PR body). Almost every tick observes no change, so with ETags the
//! watcher can poll far more often for what 120s costs today.
//!
//! The `gh` CLI cannot do this: it exposes no way to send `If-None-Match` or read the response
//! `ETag` from `gh pr view`. So this module speaks HTTP directly, for this one path only;
//! [`crate::ghsummons`] and every agent `gh` call are untouched.
//!
//! **github.com only.** [`rest_pr_url`] hardcodes `api.github.com` and the token is resolved for the
//! default host, so a GHES/`GH_HOST` install cannot use this transport. That is not silent: the
//! first lookup would answer 401, and [`ConditionalPrState::lookup`] treats a 401 as a signal to
//! re-resolve the credential and then to fall back to the `gh` source — the watcher keeps observing
//! at the old cost instead of going quiet.
//!
//! # The rules that make it safe
//!
//! * **A 304 is an ANSWER ("unchanged"), never a failure and never `Gone`.** It maps to the
//!   [`PrLookup`] last recorded for that coordinate. Getting this backwards retires live pull
//!   requests (STUDIO-950's "a failure is never an answer").
//! * **An ETag miss degrades to a normal 200.** A cold start or an evicted entry sends no
//!   `If-None-Match` and gets an honest 200; a server that simply ignores the header answers 200
//!   anyway. A `412 Precondition Failed` means the recorded ETag no longer applies, so the one
//!   request is retried UNCONDITIONALLY and the answer recorded — never an error, and never a
//!   second conditional request carrying the same stale token.
//! * **A failed lookup keeps the cache entry.** A rate limit or a network blip is not evidence the
//!   pull request changed, and dropping the ETag would make the next attempt a full-cost read.
//! * **The pre-dispatch re-read bypasses the cache.** STUDIO-953's re-read must see a head an
//!   author may have just pushed past, so it goes through
//!   [`PrStateSource::pr_state_unconditional`], which this source implements to send no
//!   `If-None-Match`.
//!
//! # One deliberate difference from the `gh` source
//!
//! [`PrSnapshot::merge_state`] is filled from REST's `mergeable_state` rather than GraphQL's
//! `mergeStateStatus`. GitHub may answer `unknown` while it computes mergeability lazily, so a
//! conflict can take one extra poll to surface; the direction is safe — the conflict route-back
//! acts only on a positively-recognised `DIRTY` (STUDIO-961), and `unknown` acts on nothing. Every
//! other field is parsed to match the `gh` source exactly, including the fork trust guard and the
//! "an unstated field is not an error" rules.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::ghsummons::{
    GH_EXEC_TIMEOUT, HeadAllowlist, PrLookup, PrSnapshot, PrStateResult, PrStateSource, PrStatus,
};
use crate::prstate::PrCoord;

/// One HTTP answer in the only shape this module needs: the status, the (optional) ETag the server
/// returned, and the body. `body` is empty on a 304, which is the whole point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAnswer {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

/// The HTTP transport a [`ConditionalPrState`] drives. Injectable so tests can simulate a 304, a
/// 200 with a new ETag, or a failure without a network — the same reason [`crate::ghsummons::GH`]
/// takes an injectable [`RunFn`](crate::ghsummons::RunFn).
#[async_trait]
pub trait ConditionalTransport: Send + Sync {
    /// `GET url`, sending `If-None-Match: <if_none_match>` when one is given. An `Err` is a lookup
    /// that could not be MADE (network, timeout, TLS) — never a status.
    async fn get(
        &self,
        url: &str,
        if_none_match: Option<&str>,
    ) -> Result<HttpAnswer, Box<dyn std::error::Error + Send + Sync>>;

    /// A lookup just answered 401: the credential this transport holds is no longer accepted (an
    /// expired PAT, or a `gh auth refresh`/relogin that rotated the keyring token). Re-resolve the
    /// credential — bounded, off-task — and return whether a *different* credential is now in
    /// place, so the caller can retry that one lookup. Returns `false` when there is nothing to
    /// renew (no token to resolve, or the resolved token is byte-identical to the current one), and
    /// the caller then answers through its `gh` source instead of going quiet.
    ///
    /// The default declines: a transport with no credential to renew.
    async fn renew(&self) -> bool {
        false
    }
}

/// The one entry the cache keeps per coordinate: the last ETag GitHub returned with an answer, and
/// the answer itself so a 304 can be served without re-reading the body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedPr {
    etag: String,
    lookup: PrLookup,
}

/// A [`PrStateSource`] over GitHub's REST API that sends `If-None-Match` and answers 304s from a
/// per-coordinate ETag store.
///
/// The cache is keyed by coordinate and only a 404 (or a 200 with no ETag) removes an entry, so it
/// grows with every pull request the daemon has ever watched rather than shrinking as the watch set
/// does. In practice that is a handful of small entries, and it is bounded per daemon lifetime — a
/// restart clears it — so it is not pruned.
pub struct ConditionalPrState {
    transport: Arc<dyn ConditionalTransport>,
    /// Per-exec bound, [`GH_EXEC_TIMEOUT`] in production. A field so a test can observe the bound
    /// firing without waiting a real minute.
    exec_timeout: Duration,
    /// The ETag + answer per coordinate. A plain `Mutex<HashMap>` — never held across the request
    /// `.await`, only read and written around it (the lock is taken, the entry cloned, the guard
    /// dropped, and only then is the request awaited).
    cache: Mutex<HashMap<PrCoord, CachedPr>>,
    /// What a lookup is answered through when the REST transport cannot be used — a 401 that a
    /// renewed token did not fix. `None` in a test or a caller that wired no fallback, in which
    /// case a 401 is an ordinary failure. The `gh`-subprocess source in production, so a rotated
    /// or expired credential degrades the watcher to its old (paid) behaviour rather than stopping
    /// it.
    fallback: Option<Arc<dyn PrStateSource>>,
}

impl ConditionalPrState {
    /// Builds a source over `transport`. The cache starts empty: the first lookup of each
    /// coordinate is a normal 200, which is the correct cold-start behavior.
    pub fn new(transport: Arc<dyn ConditionalTransport>) -> ConditionalPrState {
        ConditionalPrState {
            transport,
            exec_timeout: GH_EXEC_TIMEOUT,
            cache: Mutex::new(HashMap::new()),
            fallback: None,
        }
    }

    /// Attaches the source a 401 falls back to when the credential cannot be renewed. Production
    /// passes the `gh`-subprocess [`PrStateSource`]; see [`Self::lookup`].
    pub fn with_fallback(mut self, fallback: Arc<dyn PrStateSource>) -> ConditionalPrState {
        self.fallback = Some(fallback);
        self
    }

    /// The cached ETag and answer for `pr`, cloned out from under the lock.
    fn cached(&self, pr: &PrCoord) -> Option<CachedPr> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(pr)
            .cloned()
    }

    /// Records the ETag GitHub returned with an answer. A 200 with no ETag leaves nothing to
    /// condition on, so the entry is DROPPED rather than kept: the next lookup is then an honest
    /// unconditional 200, instead of carrying a token the server no longer vouches for.
    fn record(&self, pr: &PrCoord, etag: Option<&str>, lookup: &PrLookup) {
        let Some(etag) = etag.filter(|e| !e.is_empty()) else {
            self.forget(pr);
            return;
        };
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
            pr.clone(),
            CachedPr {
                etag: etag.to_string(),
                lookup: lookup.clone(),
            },
        );
    }

    /// Drops a coordinate's entry: a 404 means there is nothing left to watch, so the ETag is dead
    /// weight.
    fn forget(&self, pr: &PrCoord) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pr);
    }

    /// One request, with `if_none_match` when a cached ETag is available, bounded by
    /// [`Self::exec_timeout`].
    async fn fetch(
        &self,
        pr: &PrCoord,
        if_none_match: Option<&str>,
    ) -> Result<HttpAnswer, Box<dyn std::error::Error + Send + Sync>> {
        let url = rest_pr_url(&pr.owner, &pr.repo, pr.number);
        let get = self.transport.get(&url, if_none_match);
        match tokio::time::timeout(self.exec_timeout, get).await {
            Ok(answer) => answer,
            // Deliberately not phrased as a 404: `is_gone_message` and this module's own 404
            // handling read status, and a bound that looked like a missing pull request would
            // retire every watched one during an outage.
            Err(_) => Err(format!(
                "conditional pr-state GET {pr} timed out after {:?}",
                self.exec_timeout
            )
            .into()),
        }
    }

    /// Applies a final (non-304) answer: a 200 is parsed and recorded, a 404 means the pull request
    /// is gone and its entry is dropped, and every other status is a lookup that could not be MADE —
    /// an `Err` that deliberately KEEPS the entry, because dropping the ETag would make the next
    /// attempt a full-cost read. Shared by the first answer and a 412's unconditional retry so the
    /// two cannot drift.
    fn apply(&self, pr: &PrCoord, allow: &HeadAllowlist, answer: &HttpAnswer) -> PrStateResult {
        match answer.status {
            200 => {
                let lookup = parse_rest_pr(&answer.body, &pr.owner, allow).map_err(
                    |e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("decode GET {pr}: {e}").into()
                    },
                )?;
                self.record(pr, answer.etag.as_deref(), &lookup);
                Ok(lookup)
            }
            404 => {
                self.forget(pr);
                Ok(PrLookup::Gone)
            }
            // Anything else — a rate limit (403/429), a 5xx, an unexpected status — is a lookup
            // that could not be MADE. The entry is deliberately KEPT: the wrong direction here
            // would be losing the ETag and paying full price on the next attempt.
            other => Err(format!("conditional pr-state GET {pr} answered HTTP {other}").into()),
        }
    }

    /// The lookup for `pr`, either serving a 304 from the cache or (re)reading and recording a 200.
    /// `use_cache` false is the pre-dispatch re-read: send no `If-None-Match` and overwrite the
    /// entry with what comes back.
    async fn lookup(&self, pr: &PrCoord, allow: &HeadAllowlist, use_cache: bool) -> PrStateResult {
        // Empty coordinates answer `Gone`, exactly as [`GH`](crate::ghsummons::GH) does: nothing can
        // ever be observed there, and an error would make the caller retry something that cannot
        // improve.
        if pr.owner.is_empty() || pr.repo.is_empty() || pr.number <= 0 {
            return Ok(PrLookup::Gone);
        }
        let cached = use_cache.then(|| self.cached(pr)).flatten();
        let mut answer = self
            .fetch(pr, cached.as_ref().map(|c| c.etag.as_str()))
            .await?;

        // A 401 is the credential no longer being accepted (an expired PAT, or a rotated keyring
        // token), not the pull request changing. Renew the credential and retry that one lookup
        // against the REST source; if it still cannot be made, answer through the `gh` source so
        // the watcher keeps observing instead of going quiet until a restart (STUDIO-974 review,
        // finding 2).
        if answer.status == 401 {
            tracing::warn!(
                pr = %pr,
                "conditional pr-state GET answered HTTP 401; re-resolving the GitHub token"
            );
            if self.transport.renew().await {
                answer = self
                    .fetch(pr, cached.as_ref().map(|c| c.etag.as_str()))
                    .await?;
            }
            if answer.status == 401
                && let Some(fallback) = self.fallback.as_ref()
            {
                tracing::warn!(
                    pr = %pr,
                    "conditional pr-state GET still 401 after renewing; answering through the \
                     `gh` source"
                );
                return if use_cache {
                    fallback
                        .pr_state(&pr.owner, &pr.repo, pr.number, allow)
                        .await
                } else {
                    fallback
                        .pr_state_unconditional(&pr.owner, &pr.repo, pr.number, allow)
                        .await
                };
            }
        }

        match answer.status {
            // Unchanged: the recorded answer, NOT a failure and NOT `Gone`.
            304 => match cached {
                Some(c) => Ok(c.lookup),
                // A 304 with no entry to serve is a server/bookkeeping mismatch, not something the
                // daemon can act on. Say so rather than inventing a state.
                None => Err(format!(
                    "conditional pr-state GET {pr}: server answered 304 with no cached answer"
                )
                .into()),
            },
            // A stale precondition: the ETag we sent no longer applies, so "ask properly this
            // time" — retry the ONE request unconditionally and apply what it returns. The entry
            // is not dropped first, so a retry that itself fails leaves the old ETag in place
            // (the safe direction). A second 412 is a failure, not a loop.
            412 => {
                let retried = self.fetch(pr, None).await?;
                if retried.status == 412 {
                    return Err(
                        format!("conditional pr-state GET {pr} answered HTTP 412 twice").into(),
                    );
                }
                self.apply(pr, allow, &retried)
            }
            _ => self.apply(pr, allow, &answer),
        }
    }
}

#[async_trait]
impl PrStateSource for ConditionalPrState {
    async fn pr_state(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        allow: &HeadAllowlist,
    ) -> PrStateResult {
        self.lookup(&PrCoord::new(owner, repo, number), allow, true)
            .await
    }

    /// STUDIO-953's pre-dispatch re-read: sends no `If-None-Match`, so a head an author pushed
    /// after the sweep cannot be hidden behind a cached "unchanged".
    async fn pr_state_unconditional(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        allow: &HeadAllowlist,
    ) -> PrStateResult {
        self.lookup(&PrCoord::new(owner, repo, number), allow, false)
            .await
    }
}

/// `https://api.github.com/repos/{owner}/{repo}/pulls/{number}`.
fn rest_pr_url(owner: &str, repo: &str, number: i64) -> String {
    format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}")
}

/// Parses GitHub's REST pull-request body into the same [`PrLookup`] the `gh`-based source produces,
/// applying the SAME trust guard and the same "an unstated field is not an error" rules.
///
/// REST spells the state differently from GraphQL: it reports `state: "open" | "closed"` plus a
/// `merged` boolean and a `merged_at` timestamp. A closed-and-merged pull request is
/// [`PrStatus::Merged`]; a closed-and-not-merged one is [`PrStatus::Closed`].
fn parse_rest_pr(
    body: &[u8],
    base_owner: &str,
    allow: &HeadAllowlist,
) -> Result<PrLookup, Box<dyn std::error::Error + Send + Sync>> {
    let pr: serde_json::Value = serde_json::from_slice(body)?;

    // The trust guard first, exactly as the `gh` source: an untrusted head is a non-answer, so
    // nothing else is worth parsing.
    let head = pr.get("head");
    let head_owner = head
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("owner"))
        .and_then(|o| o.get("login"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let head_name = head
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    // `full_name` is what REST returns; the owner/name pair is the fallback. An empty owner leaves
    // the slug empty rather than a bare `/repo`, exactly as the `gh` source does.
    let head_repo = head
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("full_name"))
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
    let trusted = (!head_owner.is_empty() && head_owner.eq_ignore_ascii_case(base_owner))
        || (!head_repo.is_empty() && allow.allows(&head_repo));
    if !trusted {
        return Ok(PrLookup::Untrusted);
    }

    let head_sha = head
        .and_then(|h| h.get("sha"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            "no head.sha in the REST answer".into()
        })?
        .to_string();

    let merged = pr.get("merged").and_then(serde_json::Value::as_bool);
    let merged_at = pr
        .get("merged_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let raw_state = pr
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let status = match raw_state.to_ascii_lowercase().as_str() {
        "open" => PrStatus::Open,
        "closed" if merged == Some(true) || merged_at.is_some() => PrStatus::Merged,
        "closed" => PrStatus::Closed,
        other => {
            return Err(format!("unrecognised state {other:?} in the REST answer").into());
        }
    };
    // `None` when absent or not a boolean — see [`PrSnapshot::is_draft`]; each reader applies its
    // own safe direction, so an unstated answer is not an error here.
    let is_draft = pr.get("draft").and_then(serde_json::Value::as_bool);
    // REST's `mergeable_state` ("clean", "dirty", "unknown", …) is upper-cased to match the
    // GraphQL spelling the rest of the subsystem compares against. Absent or non-string is EMPTY,
    // which is the direction that acts on nothing (STUDIO-961).
    let merge_state = pr
        .get("mergeable_state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();

    Ok(PrLookup::Found(PrSnapshot {
        head_sha,
        status,
        is_draft,
        merged_at,
        head_repo,
        merge_state,
    }))
}

/// A [`ConditionalTransport`] over `reqwest`, sending the daemon's GitHub token as a bearer
/// credential. The client is built once; the token is read from `GH_TOKEN`/`GITHUB_TOKEN` or, when
/// neither is set, from the authenticated `gh` CLI (`gh auth token`), so an installation whose `gh`
/// already works needs no new environment. The token is held behind a `Mutex` rather than copied
/// into the client so [`ConditionalTransport::renew`] can swap it after a 401.
pub struct GitHubRestTransport {
    client: reqwest::Client,
    token: Mutex<String>,
}

impl GitHubRestTransport {
    /// Builds a transport with `token`. The caller resolves the token (see [`resolve_github_token`])
    /// so a failure to find one is a boot decision, not a per-request one.
    pub fn new(token: String) -> GitHubRestTransport {
        GitHubRestTransport {
            client: reqwest::Client::new(),
            token: Mutex::new(token),
        }
    }
}

#[async_trait]
impl ConditionalTransport for GitHubRestTransport {
    async fn get(
        &self,
        url: &str,
        if_none_match: Option<&str>,
    ) -> Result<HttpAnswer, Box<dyn std::error::Error + Send + Sync>> {
        // Cloned out from under the lock, which is dropped before the `.await` below.
        let token = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut req = self
            .client
            .get(url)
            // The REST media type; GitHub returns `mergeable_state` on this endpoint.
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "rhapsody")
            .bearer_auth(&token);
        if let Some(etag) = if_none_match {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        let etag = res
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // A 304 has no body; reading it is harmless and yields empty bytes.
        let body = res.bytes().await?.to_vec();
        Ok(HttpAnswer { status, etag, body })
    }

    /// Re-resolves the token through the same bounded, off-task path boot uses, and reports whether
    /// it actually changed. An unchanged token (for example one still supplied by `GH_TOKEN`) is
    /// `false`, so the caller does not pay a second request to learn what it already knows.
    async fn renew(&self) -> bool {
        let Some(fresh) = resolve_github_token().await else {
            return false;
        };
        let mut current = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if *current == fresh {
            return false;
        }
        *current = fresh;
        true
    }
}

/// Resolves a GitHub token for [`GitHubRestTransport`]: `GH_TOKEN`, then `GITHUB_TOKEN`, then the
/// authenticated `gh` CLI's own token (`gh auth token`). `None` when none can be found, which the
/// caller treats as "keep using the `gh`-subprocess source" — a daemon that cannot build the
/// conditional transport must still be able to watch its pull requests.
///
/// The `gh` exec runs on tokio's BLOCKING pool under [`GH_EXEC_TIMEOUT`], the same containment
/// every other `gh` call in the daemon uses (STUDIO-829). This is called from the async boot path
/// (and again from [`GitHubRestTransport::renew`]), so an exec run inline would hold a tokio worker
/// thread with no yield point, and a hung `gh` — a locked keychain, a keyring prompt under launchd,
/// a stuck credential helper — would stall boot with nothing logged. The bound is what makes a
/// timeout enforceable; on expiry the token is simply absent and the `gh`-source fallback is
/// chosen, so a hung `gh` costs a fallback rather than the daemon.
pub async fn resolve_github_token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    let exec = tokio::task::spawn_blocking(|| {
        std::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
    });
    let out = match tokio::time::timeout(GH_EXEC_TIMEOUT, exec).await {
        Ok(Ok(Ok(out))) => out,
        // Timed out, the blocking task failed to join, or the spawn did not happen: treat all
        // three as "no token", which selects the `gh`-source fallback.
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "`gh auth token` could not be run");
            return None;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "`gh auth token` task did not join");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                timeout = ?GH_EXEC_TIMEOUT,
                "`gh auth token` timed out; falling back to the `gh` pr-state source"
            );
            return None;
        }
    };
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A transport that answers a fixed script of `(status, etag, body)` per call, recording the
    /// `If-None-Match` each call carried. `renew` is scripted too, and counted.
    struct ScriptedTransport {
        answers: Mutex<Vec<HttpAnswer>>,
        seen_etags: Arc<Mutex<Vec<Option<String>>>>,
        calls: Arc<AtomicUsize>,
        renews: Arc<AtomicUsize>,
        renew_result: bool,
    }

    #[async_trait]
    impl ConditionalTransport for ScriptedTransport {
        async fn get(
            &self,
            _url: &str,
            if_none_match: Option<&str>,
        ) -> Result<HttpAnswer, Box<dyn std::error::Error + Send + Sync>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(if_none_match.map(str::to_string));
            let mut answers = self.answers.lock().unwrap_or_else(|e| e.into_inner());
            if answers.is_empty() {
                return Err("script exhausted".into());
            }
            Ok(answers.remove(0))
        }

        async fn renew(&self) -> bool {
            self.renews.fetch_add(1, Ordering::SeqCst);
            self.renew_result
        }
    }

    /// A [`PrStateSource`] standing in for the `gh` fallback: records every call and answers one
    /// fixed lookup.
    struct FakeFallback {
        answer: PrLookup,
        calls: Arc<AtomicUsize>,
        unconditional_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PrStateSource for FakeFallback {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.answer.clone())
        }

        async fn pr_state_unconditional(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            self.unconditional_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.answer.clone())
        }
    }

    fn body(head: &str, state: &str, merged: bool) -> Vec<u8> {
        serde_json::json!({
            "state": state,
            "merged": merged,
            "draft": false,
            "mergeable_state": "clean",
            "merged_at": if merged { Some("2026-09-21T15:25:39Z") } else { None },
            "head": {
                "sha": head,
                "repo": { "full_name": "o/r", "name": "r", "owner": { "login": "o" } }
            }
        })
        .to_string()
        .into_bytes()
    }

    fn answer(status: u16, etag: Option<&str>, body: Vec<u8>) -> HttpAnswer {
        HttpAnswer {
            status,
            etag: etag.map(str::to_string),
            body,
        }
    }

    fn source(answers: Vec<HttpAnswer>) -> (ConditionalPrState, Arc<ScriptedTransport>) {
        source_with(answers, false)
    }

    fn source_with(
        answers: Vec<HttpAnswer>,
        renew_result: bool,
    ) -> (ConditionalPrState, Arc<ScriptedTransport>) {
        let t = Arc::new(ScriptedTransport {
            answers: Mutex::new(answers),
            seen_etags: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            renews: Arc::new(AtomicUsize::new(0)),
            renew_result,
        });
        let src = ConditionalPrState::new(Arc::clone(&t) as Arc<dyn ConditionalTransport>);
        (src, t)
    }

    fn found(lookup: &PrStateResult) -> &PrSnapshot {
        match lookup {
            Ok(PrLookup::Found(s)) => s,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    /// A 200 records the ETag; the NEXT lookup sends it and a 304 serves the recorded answer.
    /// Mutation check: map 304 to an `Err` and the second assert fails; map it to `Gone` and the
    /// `matches!` fails.
    #[tokio::test]
    async fn a_304_serves_the_recorded_answer_without_reporting_a_failure_or_gone() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(304, None, Vec::new()),
        ]);

        let first = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&first).head_sha, "sha1");

        let second = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        let second = second.expect("a 304 is an answer, not a failure");
        assert_eq!(
            second,
            PrLookup::Found(PrSnapshot {
                head_sha: "sha1".to_string(),
                status: PrStatus::Open,
                is_draft: Some(false),
                merged_at: None,
                head_repo: "o/r".to_string(),
                merge_state: "CLEAN".to_string(),
            }),
            "a 304 must serve the recorded state, and never Gone"
        );
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string())],
            "the second request must carry the recorded ETag"
        );
    }

    /// A changed pull request answers 200 with a new head, and the ETag is updated so the next
    /// conditional request uses the NEW token.
    #[tokio::test]
    async fn a_changed_pull_request_returns_200_and_updates_the_etag() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(200, Some("etag-2"), body("sha2", "open", false)),
            answer(304, None, Vec::new()),
        ]);

        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        let moved = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&moved).head_sha, "sha2");

        let after = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&after).head_sha, "sha2");
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string()), Some("etag-2".to_string())],
            "the ETag must advance with the answer"
        );
    }

    /// `pr_state_unconditional` — STUDIO-953's pre-dispatch re-read — sends NO `If-None-Match` even
    /// when a cache entry exists, so a head pushed after the sweep is seen. Mutation check: route it
    /// through `pr_state` and the second request carries `etag-1`.
    #[tokio::test]
    async fn the_unconditional_lookup_bypasses_the_cache() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(200, Some("etag-1"), body("sha2", "open", false)),
        ]);

        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        let fresh = src
            .pr_state_unconditional("o", "r", 1, &HeadAllowlist::none())
            .await;
        assert_eq!(found(&fresh).head_sha, "sha2");
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, None],
            "the pre-dispatch re-read must never send If-None-Match"
        );
    }

    /// An ETag miss (cold start) is a normal 200, not an error.
    #[tokio::test]
    async fn a_cold_start_is_a_normal_200() {
        let (src, t) = source(vec![answer(
            200,
            Some("etag-1"),
            body("sha1", "open", false),
        )]);
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&got).head_sha, "sha1");
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None]
        );
    }

    /// A 404 is `Gone` and the entry is forgotten, so a later re-created pull request cold-starts.
    #[tokio::test]
    async fn a_404_is_gone_and_forgets_the_entry() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(404, None, Vec::new()),
            answer(200, Some("etag-2"), body("sha9", "open", false)),
        ]);
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert!(matches!(
            src.pr_state("o", "r", 1, &HeadAllowlist::none()).await,
            Ok(PrLookup::Gone)
        ));
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string()), None],
            "the forgotten entry must not be sent again"
        );
    }

    /// A failure is an `Err`, never `Gone`, and the ETag survives so the next attempt can still be
    /// conditional.
    #[tokio::test]
    async fn a_failure_is_never_gone_and_keeps_the_etag() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(403, None, b"API rate limit exceeded".to_vec()),
            answer(304, None, Vec::new()),
        ]);
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert!(
            src.pr_state("o", "r", 1, &HeadAllowlist::none())
                .await
                .is_err(),
            "a rate limit is a failure, not an answer"
        );
        // The next attempt is still conditional — the entry survived the failure.
        assert_eq!(
            found(&src.pr_state("o", "r", 1, &HeadAllowlist::none()).await).head_sha,
            "sha1"
        );
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string()), Some("etag-1".to_string())]
        );
    }

    /// A `412 Precondition Failed` is a STALE precondition, not a failure: the ETag we sent no
    /// longer applies, so the one request is retried UNCONDITIONALLY and the answer recorded.
    /// Mutation check: send 412 to the error arm and this reds on the retry carrying `etag-1` and
    /// the lookup coming back an `Err` instead of `sha2`.
    #[tokio::test]
    async fn a_412_retries_unconditionally_and_records_the_answer() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(412, None, Vec::new()),
            answer(200, Some("etag-2"), body("sha2", "open", false)),
        ]);
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(
            found(&got).head_sha,
            "sha2",
            "a 412 must be retried, not reported as a failure"
        );
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string()), None],
            "the retry after a 412 must send no If-None-Match"
        );
    }

    /// A retry that 412s AGAIN is a failure, never an unbounded loop: the second answer is not
    /// retried.
    #[tokio::test]
    async fn a_412_that_survives_the_retry_is_a_failure_not_gone() {
        let (src, _t) = source(vec![
            answer(412, None, Vec::new()),
            answer(412, None, Vec::new()),
        ]);
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert!(got.is_err(), "a repeated 412 is a failure, not an answer");
    }

    /// A merged pull request is `Merged`, from REST's `merged`/`merged_at` (REST reports
    /// `state: "closed"` for a merge, unlike GraphQL's `MERGED`).
    #[tokio::test]
    async fn a_merged_closed_pull_request_is_merged_not_closed() {
        let (src, _t) = source(vec![answer(200, Some("e"), body("sha1", "closed", true))]);
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&got).status, PrStatus::Merged);
        assert!(found(&got).merged_at.is_some());
    }

    /// A fork's head is refused: the trust guard runs before anything else is parsed.
    #[tokio::test]
    async fn a_fork_head_is_untrusted() {
        let fork = serde_json::json!({
            "state": "open",
            "head": { "sha": "sha1", "repo": { "full_name": "stranger/r", "name": "r", "owner": { "login": "stranger" } } }
        })
        .to_string()
        .into_bytes();
        let (src, _t) = source(vec![answer(200, Some("e"), fork)]);
        assert!(matches!(
            src.pr_state("o", "r", 1, &HeadAllowlist::none()).await,
            Ok(PrLookup::Untrusted)
        ));
    }

    /// A 401 is a credential that is no longer accepted, not a failure to report forever: the token
    /// is renewed and the lookup is retried once with the new credential.
    /// Mutation check: drop the `renew` call and the retry, and this is an `Err`.
    #[tokio::test]
    async fn a_401_renews_the_token_and_retries_the_lookup() {
        let (src, t) = source_with(
            vec![
                answer(401, None, b"Bad credentials".to_vec()),
                answer(200, Some("etag-1"), body("sha1", "open", false)),
            ],
            true,
        );
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&got).head_sha, "sha1");
        assert_eq!(
            t.renews.load(Ordering::SeqCst),
            1,
            "a 401 must attempt one credential renewal"
        );
        assert_eq!(
            t.calls.load(Ordering::SeqCst),
            2,
            "the lookup must be retried once after the renewal"
        );
    }

    /// When the credential cannot be renewed (or the retry still 401s), the lookup is answered
    /// through the `gh` fallback, so the watcher keeps observing rather than going quiet until a
    /// restart. Mutation check: remove the fallback branch and this is an `Err`.
    #[tokio::test]
    async fn a_401_that_survives_renewal_falls_back_to_the_gh_source() {
        let (src, t) = source_with(vec![answer(401, None, b"Bad credentials".to_vec())], false);
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Arc::new(FakeFallback {
            answer: PrLookup::Found(PrSnapshot {
                head_sha: "from-gh".to_string(),
                status: PrStatus::Open,
                is_draft: None,
                merged_at: None,
                head_repo: "o/r".to_string(),
                merge_state: String::new(),
            }),
            calls: Arc::clone(&fallback_calls),
            unconditional_calls: Arc::new(AtomicUsize::new(0)),
        });
        let src = src.with_fallback(Arc::clone(&fallback) as Arc<dyn PrStateSource>);

        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&got).head_sha, "from-gh");
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert_eq!(t.renews.load(Ordering::SeqCst), 1);
    }

    /// A `renew()` that SUCCEEDS but hands back a credential GitHub still rejects must also fall
    /// back, not return the second 401 as a failure. The sibling test above pins only the
    /// `renew()`-returned-false path; this one pins the renewed-token-still-rejected path — a
    /// rotated-but-not-yet-propagated keyring token, or an expired `GH_TOKEN` that `gh auth token`
    /// echoes back unchanged. An implementation that falls back only when renewal REPORTED failure
    /// would silence this lookup, which is exactly the regression the review named.
    /// Mutation check: fall back only when `renew()` returned false and this reds (an `Err` instead
    /// of `from-gh`).
    #[tokio::test]
    async fn a_401_that_survives_a_successful_renewal_falls_back_to_the_gh_source() {
        let (src, t) = source_with(
            vec![
                answer(401, None, b"Bad credentials".to_vec()),
                answer(401, None, b"Bad credentials".to_vec()),
            ],
            true,
        );
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Arc::new(FakeFallback {
            answer: PrLookup::Found(PrSnapshot {
                head_sha: "from-gh".to_string(),
                status: PrStatus::Open,
                is_draft: None,
                merged_at: None,
                head_repo: "o/r".to_string(),
                merge_state: String::new(),
            }),
            calls: Arc::clone(&fallback_calls),
            unconditional_calls: Arc::new(AtomicUsize::new(0)),
        });
        let src = src.with_fallback(Arc::clone(&fallback) as Arc<dyn PrStateSource>);

        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(found(&got).head_sha, "from-gh");
        assert_eq!(
            t.renews.load(Ordering::SeqCst),
            1,
            "the 401 must attempt exactly one renewal"
        );
        assert_eq!(
            t.calls.load(Ordering::SeqCst),
            2,
            "the successful renewal must be followed by exactly one retry"
        );
        assert_eq!(
            fallback_calls.load(Ordering::SeqCst),
            1,
            "a still-401 after a SUCCESSFUL renewal must answer through the gh fallback"
        );
    }

    /// Without a fallback a 401 is still a failure, never `Gone` — so a caller cannot retire a live
    /// pull request on a bad credential.
    #[tokio::test]
    async fn a_401_without_a_fallback_is_a_failure_not_gone() {
        let (src, _t) = source_with(vec![answer(401, None, b"Bad credentials".to_vec())], false);
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert!(got.is_err(), "a 401 is a failure, not an answer");
    }

    /// A 401 on the PRE-DISPATCH re-read answers through the fallback's UNCONDITIONAL entry point
    /// (STUDIO-953): routing it through the cached `pr_state` would let the fallback's own caching
    /// hide a head pushed after the sweep.
    /// Mutation check: swap the fallback's `pr_state_unconditional` for `pr_state` and the
    /// `unconditional_calls` assert reds.
    #[tokio::test]
    async fn a_401_on_the_unconditional_lookup_uses_the_unconditional_fallback() {
        let (src, _t) = source_with(vec![answer(401, None, b"Bad credentials".to_vec())], false);
        let calls = Arc::new(AtomicUsize::new(0));
        let unconditional_calls = Arc::new(AtomicUsize::new(0));
        let fallback = Arc::new(FakeFallback {
            answer: PrLookup::Found(PrSnapshot {
                head_sha: "from-gh".to_string(),
                status: PrStatus::Open,
                is_draft: None,
                merged_at: None,
                head_repo: "o/r".to_string(),
                merge_state: String::new(),
            }),
            calls: Arc::clone(&calls),
            unconditional_calls: Arc::clone(&unconditional_calls),
        });
        let src = src.with_fallback(Arc::clone(&fallback) as Arc<dyn PrStateSource>);

        let got = src
            .pr_state_unconditional("o", "r", 1, &HeadAllowlist::none())
            .await;
        assert_eq!(found(&got).head_sha, "from-gh");
        assert_eq!(
            unconditional_calls.load(Ordering::SeqCst),
            1,
            "the pre-dispatch re-read must stay unconditional through the fallback"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A 200 that carries no ETag drops any prior entry: there is no token to condition on, so the
    /// next lookup is an honest unconditional 200 rather than a stale conditional one.
    /// Mutation check: keep the old entry and the second request carries `etag-1`.
    #[tokio::test]
    async fn a_200_without_an_etag_drops_the_prior_entry() {
        let (src, t) = source(vec![
            answer(200, Some("etag-1"), body("sha1", "open", false)),
            answer(200, None, body("sha2", "open", false)),
            answer(200, Some("etag-3"), body("sha3", "open", false)),
        ]);
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(
            found(&src.pr_state("o", "r", 1, &HeadAllowlist::none()).await).head_sha,
            "sha2"
        );
        let _ = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert_eq!(
            t.seen_etags
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec![None, Some("etag-1".to_string()), None],
            "an answer with no ETag must not leave the old token behind"
        );
    }

    /// A 304 with no cached answer is a bookkeeping mismatch, not a state to invent: it is an
    /// `Err`, never `Gone` and never a made-up `Found`.
    #[tokio::test]
    async fn a_304_with_no_cache_entry_is_a_failure_not_gone() {
        let (src, _t) = source(vec![answer(304, None, Vec::new())]);
        let got = src.pr_state("o", "r", 1, &HeadAllowlist::none()).await;
        assert!(got.is_err(), "a 304 with nothing to serve cannot be Gone");
    }

    /// STUDIO-974 review: this module's `gh auth token` exec — the one `gh` exec outside
    /// [`crate::ghsummons`] — must run on the blocking pool under [`GH_EXEC_TIMEOUT`], exactly as
    /// STUDIO-829 requires, and this is the test that keeps it so.
    ///
    /// `ghsummons::every_gh_exec_goes_through_the_blocking_pool` asserts on `ghsummons.rs`'s own
    /// source, so it cannot see a `gh` exec added in another module. The property is architectural
    /// rather than behavioural, which is why it is asserted on source: a synchronous inline exec
    /// compiles and passes every other test in this file, holds a tokio WORKER thread with no await
    /// point, and makes any `timeout` around it unenforceable. Review at STUDIO-974 reproduced
    /// exactly that and watched all 1,712 orchestrator tests stay green.
    #[test]
    fn the_token_resolution_gh_exec_is_contained() {
        let src = include_str!("prconditional.rs");
        // Only the production half: `include_str!` also pulls in this file's test module, whose own
        // text must not count as an occurrence of the thing it forbids.
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        // Assembled at run time so this test's own source is not itself an occurrence.
        let exec: String = ["std", "::process::", "Command"].concat();
        let spawn = production
            .find("tokio::task::spawn_blocking")
            .expect("the `gh auth token` exec must run on the blocking pool (STUDIO-829)");
        let closure_end = spawn
            + production[spawn..]
                .find("})")
                .expect("the spawn_blocking closure is still a braced closure");
        let uses: Vec<usize> = production
            .match_indices(exec.as_str())
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            uses.len(),
            1,
            "expected exactly one `gh` exec in this module, found {}; route any new one through \
             the blocking pool too (STUDIO-829)",
            uses.len()
        );
        assert!(
            (spawn..closure_end).contains(&uses[0]),
            "the `gh auth token` exec is outside the `spawn_blocking` closure: it would hold a \
             tokio worker thread and no timeout could fire (STUDIO-829)"
        );
        assert!(
            production.contains("tokio::time::timeout(GH_EXEC_TIMEOUT"),
            "the blocking-pool handle must be awaited under GH_EXEC_TIMEOUT"
        );
    }
}
