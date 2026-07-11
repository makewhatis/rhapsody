//! claim — parity port of Go `internal/orchestrator/claim.go` (INF-477).
//!
//! The pool-mode single-claimant protocol: when several daemons share one unassigned candidate
//! pool, each posts a machine-readable claim comment, waits out a jittered settle window so
//! concurrent claims propagate, then runs a deterministic election (earliest server `created_at`,
//! tie-break on the lexicographically smallest comment id) and — if it won — assigns the ticket to
//! itself (the durable lock) and re-reads the assignee to confirm it holds the claim uncontested.
//!
//! Deviations from the Go source, all behavior-preserving (the claim tests assert them):
//!   * The async [`Tracker`] trait models cancellation IMPLICITLY (a dropped future), so the claim
//!     methods take no `ctx`: the settle wait is a plain [`tokio::time::sleep`] and
//!     `delete_claim_comment` calls the tracker directly. Go's shutdown-mid-settle cleanup path
//!     (`sleepCtx → false → delete`) is subsumed by the control task dropping the in-flight claim
//!     future; the daemon lifecycle that drives that is O7's.
//!   * [`Orchestrator::claim_winners`] awaits each pick's protocol SEQUENTIALLY rather than fanning
//!     them out across goroutines. The claim protocol touches no orchestrator state, so the
//!     observable result (input-order winners, the promoted-state stamp) is identical; only the
//!     control-loop settle-freeze is longer under many simultaneous pool claims — a latency
//!     property, not a correctness one. (Follow-up: parallelize once the loop owns a runtime, O7.)
//!   * `claimTTLOrDefault`/`claimSettleOrDefault` already live in `effective.rs` (their first caller,
//!     the effective builder); the effective materializes the defaults, so this module receives
//!     already-defaulted `ttl`/`settle` and does not re-port them.
//!   * The settle jitter draws from the standard library's OS-seeded RNG (`orchestrator::random_u64`)
//!     instead of `math/rand` — it is anti-lockstep only, never load-bearing.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use regex::Regex;
use rhapsody_core::{Comment, Issue, normalize_state};
use rhapsody_tracker::Tracker;

use crate::effective::{Effective, ResolvedProject};
use crate::orchestrator::{Orchestrator, random_u64};
use crate::select::TaggedIssue;

/// Versions the machine-readable claim marker so a future format change can be distinguished from v1
/// in the wild. Mirrors Go `claimSchemaVersion`.
const CLAIM_SCHEMA_VERSION: u32 = 1;

/// Matches the machine-readable claim marker embedded in a pool-mode claim comment:
///
/// ```text
/// <!-- symphony-claim v1 daemon=<viewerID>/<uuid> -->
/// ```
///
/// The marker deliberately does NOT contain the `@symphony` summon token, so claim comments never
/// register as summons in candidate-query summon detection. Group 1 is the schema version, group 2
/// the daemon identity (`viewerID/uuid`). `None` if the static pattern ever fails to compile (it
/// cannot) — the no-panic idiom the sibling crates use for static patterns (`ghsummons`'s
/// `REPO_RE`). Mirrors Go `claimMarkerRe`.
static CLAIM_MARKER_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"symphony-claim v(\d+) daemon=(\S+)").ok());

/// A parsed pool-mode claim: the immutable server `created_at` is the election's ordering key,
/// `comment_id` the tie-break, `daemon_id` the audit/identity token. Mirrors Go `claimComment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimComment {
    pub comment_id: String,
    pub daemon_id: String,
    pub created_at: DateTime<Utc>,
}

/// Renders a pool-mode claim comment: a human-readable line plus the machine marker parsed by
/// [`parse_claim_comment`]. `viewer_id` is the API-key owner the winner will assign the ticket to;
/// `daemon_id` is this process's identity. Mirrors Go `buildClaimBody`.
pub(crate) fn build_claim_body(viewer_id: &str, daemon_id: &str) -> String {
    format!(
        "🤖 Symphony is claiming this ticket for automated work.\n\n<!-- symphony-claim v{CLAIM_SCHEMA_VERSION} daemon={viewer_id}/{daemon_id} -->"
    )
}

/// Extracts a [`ClaimComment`] from a comment iff its body carries the claim marker. Non-claim
/// comments (plain discussion, summons) return `None` and are excluded from the election. Mirrors Go
/// `parseClaimComment`.
pub(crate) fn parse_claim_comment(c: &Comment) -> Option<ClaimComment> {
    let re = CLAIM_MARKER_RE.as_ref()?;
    let caps = re.captures(&c.body)?;
    Some(ClaimComment {
        comment_id: c.id.clone(),
        daemon_id: caps.get(2)?.as_str().to_string(),
        created_at: c.created_at,
    })
}

/// Maps a comment list to the claim comments among them (marker-bearing only). Mirrors Go
/// `parseClaims`.
pub(crate) fn parse_claims(comments: &[Comment]) -> Vec<ClaimComment> {
    comments.iter().filter_map(parse_claim_comment).collect()
}

/// Decides the single-claimant election among the given claims: it keeps only claims still FRESH
/// within `ttl` (created after `now - ttl`, so a crashed daemon's stale claim expires and the ticket
/// is reclaimable), then picks the earliest `created_at`, tie-breaking on the lexicographically
/// smallest comment id (Linear's ids are stable, so the tie-break is deterministic across daemons
/// even under equal timestamps). Returns `None` when no fresh claim exists. Mirrors Go
/// `electClaimWinner`.
///
/// The freshness filter compares Linear's server `created_at` to the daemon's local `now` — two
/// clock domains. Ordering/tie-break are pure server-time and skew-immune; only freshness is
/// skew-sensitive, and a generous `ttl` (default 2m) relative to NTP skew keeps a live claim from
/// being judged stale.
pub(crate) fn elect_claim_winner(
    claims: &[ClaimComment],
    now: DateTime<Utc>,
    ttl: Duration,
) -> Option<ClaimComment> {
    let cutoff = now - chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::zero());
    let mut best: Option<&ClaimComment> = None;
    for c in claims {
        if c.created_at <= cutoff {
            continue; // stale (not strictly after the freshness window)
        }
        let better = match best {
            None => true,
            Some(b) => {
                c.created_at < b.created_at
                    || (c.created_at == b.created_at && c.comment_id < b.comment_id)
            }
        };
        if better {
            best = Some(c);
        }
    }
    best.cloned()
}

/// The settle delay plus a random jitter of up to +50%, so concurrent claimants don't read the claim
/// set in lockstep. Mirrors Go `jitteredSettle` (its `rand.N(base/2+1)` — a uniform draw over
/// `[0, base/2]`).
fn jittered_settle(base: Duration) -> Duration {
    if base.is_zero() {
        return Duration::ZERO;
    }
    let half_nanos = (base / 2).as_nanos() as u64;
    let extra = random_u64() % (half_nanos + 1); // [0, base/2]
    base + Duration::from_nanos(extra)
}

/// Returns the tracker and best-effort promote state for a pick's project. `rp == None` is the
/// legacy single-tracker path (top-level `eff`). The promote state is the `review_promote_state`
/// (default "In Progress") ONLY when it is one of the project's active states — so the claim's
/// visibility move never pushes the ticket out of the active set (which reconcile would treat as a
/// terminate/complete). When it is not active, `""` skips the move (the assignee alone is the lock).
/// Mirrors Go `claimDepsFor` (which reads `o.eff`; here `eff` is passed in by the sole caller,
/// [`Orchestrator::claim_winners`], which already resolved it).
fn claim_deps_for(eff: &Effective, rp: Option<&ResolvedProject>) -> (Arc<dyn Tracker>, String) {
    let (tracker, active) = match rp {
        Some(p) => (Arc::clone(&p.tracker), &p.active_states),
        None => (Arc::clone(&eff.tracker), &eff.active_states),
    };
    let ps = &eff.review_promote_state;
    let promote = if !ps.is_empty() && active.contains(&normalize_state(ps)) {
        ps.clone()
    } else {
        String::new()
    };
    (tracker, promote)
}

impl Orchestrator {
    /// Runs the pool-mode single-claimant protocol for ONE picked issue and reports whether this
    /// daemon won and should dispatch. It touches no orchestrator state (only tracker calls + a
    /// sleep). Mirrors Go `claimPool`.
    ///
    /// Steps: resolve viewer → post a claim comment → jittered settle → read claims and elect →
    /// if lost, delete own comment and yield → if won, assign self (the durable lock) → best-effort
    /// move to `promote_state` → RE-READ the assignee (read-back gate) → dispatch only if the
    /// assignee is still us. A LOSER always deletes its own comment; the WINNER RETAINS it (deleting
    /// it would re-open the comment-visibility hole for a laggard reader, who would then see only its
    /// own claim and wrongly elect itself — the won ticket is now assigned and out of the pool, so
    /// the retained comment simply ages out of the `ttl` window). A read-back ERROR ABORTS (yields):
    /// Linear's assign is last-write-wins and we cannot confirm we hold the lock, so preserving the
    /// single-claimant invariant is worth stranding the ticket (recoverable by manual un-assign).
    ///
    /// The second return value is the ticket's post-claim state: on a win where the visibility move
    /// SUCCEEDED it is `promote_state` (so the caller dispatches with the up-to-date state and
    /// per-state cap accounting is correct immediately); it is `""` on any loss/abort OR when the
    /// move was skipped/failed.
    pub(crate) async fn claim_pool(
        &self,
        tr: &dyn Tracker,
        iss: &Issue,
        promote_state: &str,
        ttl: Duration,
        settle: Duration,
    ) -> (bool, String) {
        let viewer = match tr.resolve_viewer().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(issue_identifier = %iss.identifier, err = %e, "pool claim: resolve viewer failed; skipping this tick");
                return (false, String::new());
            }
        };
        let my_comment = match tr
            .create_comment(&iss.id, &build_claim_body(&viewer.id, &self.daemon_id))
            .await
        {
            Ok(c) => c,
            // A failed claim comment is a failed claim: skip this tick (re-picked next poll).
            Err(e) => {
                tracing::warn!(issue_identifier = %iss.identifier, err = %e, "pool claim: comment create failed; skipping this tick");
                return (false, String::new());
            }
        };

        tokio::time::sleep(jittered_settle(settle)).await;

        let comments = match tr.list_comments(&iss.id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(issue_identifier = %iss.identifier, err = %e, "pool claim: list comments failed; abandoning claim");
                self.delete_claim_comment(tr, &my_comment, &iss.identifier)
                    .await;
                return (false, String::new());
            }
        };
        let winner = elect_claim_winner(&parse_claims(&comments), (self.now)(), ttl);
        if winner.as_ref().map(|w| w.comment_id.as_str()) != Some(my_comment.as_str()) {
            let winner_comment = winner.map(|w| w.comment_id).unwrap_or_default();
            tracing::info!(issue_identifier = %iss.identifier, my_comment, winner_comment, "pool claim: lost election; yielding");
            self.delete_claim_comment(tr, &my_comment, &iss.identifier)
                .await;
            return (false, String::new());
        }

        // Won the election → assign self (the durable lock).
        if let Err(e) = tr.assign_issue(&iss.id, &viewer.id).await {
            tracing::warn!(issue_identifier = %iss.identifier, err = %e, "pool claim: assign failed; abandoning claim");
            self.delete_claim_comment(tr, &my_comment, &iss.identifier)
                .await;
            return (false, String::new());
        }

        // Best-effort visibility move (Todo → the promote/active state). The assignee is the lock, so
        // a failed move is non-fatal. On success we report the new state so the run is stamped with it.
        let mut moved_state = String::new();
        if !promote_state.is_empty() && !iss.team_id.is_empty() {
            match tr
                .move_issue_state(&iss.id, &iss.team_id, promote_state)
                .await
            {
                Ok(()) => moved_state = promote_state.to_string(),
                Err(e) => {
                    tracing::warn!(issue_identifier = %iss.identifier, promote_state, err = %e, "pool claim: state move failed; proceeding (assignee holds the lock)");
                }
            }
        }

        // Read-back gate: re-read the assignee to catch a concurrent assign that overwrote us.
        let assignee = match tr.fetch_issue_assignee(&iss.id).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(issue_identifier = %iss.identifier, err = %e, "pool claim: read-back failed; aborting to preserve single-claimant");
                self.delete_claim_comment(tr, &my_comment, &iss.identifier)
                    .await;
                return (false, String::new());
            }
        };
        if assignee != viewer.id {
            tracing::info!(issue_identifier = %iss.identifier, assignee, "pool claim: lost on read-back (assignee is another daemon); yielding");
            self.delete_claim_comment(tr, &my_comment, &iss.identifier)
                .await;
            return (false, String::new());
        }

        // Won: RETAIN the claim comment (see the doc comment) so a laggard reader cannot see a
        // depleted claim set and wrongly elect itself.
        tracing::info!(issue_identifier = %iss.identifier, assignee = %viewer.id, "pool claim: won");
        (true, moved_state)
    }

    /// Best-effort removes this daemon's own claim comment (won or lost). A delete failure only
    /// leaves a stale comment that `claim_ttl` will expire, so it is logged, not fatal. Mirrors Go
    /// `deleteClaimComment` (which uses `o.ctx` so cleanup survives a cancelled dispatch ctx; the
    /// Rust `Tracker` takes no ctx, so the tracker is called directly — see the module docs).
    async fn delete_claim_comment(&self, tr: &dyn Tracker, comment_id: &str, identifier: &str) {
        if comment_id.is_empty() {
            return;
        }
        if let Err(e) = tr.delete_comment(comment_id).await {
            tracing::warn!(issue_identifier = %identifier, comment_id, err = %e, "pool claim: failed to delete own claim comment (claim_ttl will expire it)");
        }
    }

    /// Runs the pool-mode claim protocol for every pick and returns the picks this daemon won, in the
    /// input order (deterministic dispatch). A successful visibility move is reflected onto the
    /// winning pick's `iss.state` so the run is dispatched with the promoted state (correct per-state
    /// cap accounting immediately). Mirrors Go `claimWinners` (awaited sequentially rather than fanned
    /// out — see the module docs).
    ///
    /// `pub` (the pool-mode dispatch entry point the control loop, O7, drives), which keeps its
    /// call tree — [`Orchestrator::claim_pool`], the election helpers — reachable ahead of that loop.
    pub async fn claim_winners(&self, picks: Vec<TaggedIssue>) -> Vec<TaggedIssue> {
        let Some(eff) = self.eff.as_ref() else {
            return Vec::new();
        };
        let mut winners = Vec::new();
        for mut ti in picks {
            let rp = ti.proj.and_then(|i| eff.projects.get(i));
            let (tracker, promote) = claim_deps_for(eff, rp);
            let (won, new_state) = self
                .claim_pool(
                    tracker.as_ref(),
                    &ti.iss,
                    &promote,
                    eff.claim_ttl,
                    eff.claim_settle_delay,
                )
                .await;
            if !won {
                continue;
            }
            // Reflect a successful visibility move onto the dispatched issue so per-state accounting
            // keys off the promoted state right away; "" means the state was not moved.
            if !new_state.is_empty() {
                ti.iss.state = new_state;
            }
            winners.push(ti);
        }
        winners
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rhapsody_core::{Comment, Viewer};
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::empty_effective;

    fn boom() -> TrackerError {
        TrackerError::Other("boom".to_string())
    }

    // --- pure election tests (claim_test.go) --------------------------------------------------

    /// A claim comment `age_sec` seconds before `now` (Go test helper `cc`).
    fn cc(id: &str, age_sec: i64, now: DateTime<Utc>) -> ClaimComment {
        ClaimComment {
            comment_id: id.to_string(),
            daemon_id: String::new(),
            created_at: now - chrono::Duration::seconds(age_sec),
        }
    }

    // Mirrors Go `TestElectClaimWinner`.
    #[test]
    fn elect_claim_winner_ordering() {
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap();
        let ttl = Duration::from_secs(60);

        // Earliest createdAt wins (c-old 30s old, c-new 5s old).
        let w = elect_claim_winner(&[cc("c-new", 5, now), cc("c-old", 30, now)], now, ttl);
        assert_eq!(
            w.map(|w| w.comment_id),
            Some("c-old".to_string()),
            "earliest createdAt wins"
        );

        // Tie on createdAt → lexicographically smallest id wins.
        let tie = now - chrono::Duration::seconds(10);
        let w = elect_claim_winner(
            &[
                ClaimComment {
                    comment_id: "c-bbb".into(),
                    daemon_id: String::new(),
                    created_at: tie,
                },
                ClaimComment {
                    comment_id: "c-aaa".into(),
                    daemon_id: String::new(),
                    created_at: tie,
                },
            ],
            now,
            ttl,
        );
        assert_eq!(
            w.map(|w| w.comment_id),
            Some("c-aaa".to_string()),
            "tie breaks on smallest id"
        );

        // A stale claim (older than ttl) is ignored; a fresh one wins even if newer.
        let w = elect_claim_winner(&[cc("c-stale", 120, now), cc("c-fresh", 5, now)], now, ttl);
        assert_eq!(
            w.map(|w| w.comment_id),
            Some("c-fresh".to_string()),
            "stale filtered out"
        );

        // All stale → no winner.
        assert!(elect_claim_winner(&[cc("c1", 200, now), cc("c2", 300, now)], now, ttl).is_none());

        // Empty → no winner.
        assert!(elect_claim_winner(&[], now, ttl).is_none());
    }

    // Mirrors Go `TestParseClaimComment`.
    #[test]
    fn parse_claim_comment_marker() {
        let body = build_claim_body("viewer-9", "daemon-abc");
        let got = parse_claim_comment(&Comment {
            id: "c1".into(),
            body: body.clone(),
            created_at: Utc::now(),
        })
        .expect("claim marker parses");
        assert_eq!(got.daemon_id, "viewer-9/daemon-abc");
        assert_eq!(got.comment_id, "c1");

        // A plain comment is not a claim.
        assert!(
            parse_claim_comment(&Comment {
                id: "c2".into(),
                body: "just a normal comment".into(),
                created_at: Utc::now()
            })
            .is_none(),
            "a non-marker comment must not parse as a claim"
        );

        // The marker must NOT contain the default @symphony summon token (no collision).
        assert!(
            !body.to_lowercase().contains("@symphony"),
            "claim body must not contain the @symphony summon token: {body}"
        );
    }

    // --- claim_pool state-machine tests (claim_test.go) ---------------------------------------

    fn claim_issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "P-1".into(),
            state: "Todo".into(),
            team_id: "team-1".into(),
            ..Default::default()
        }
    }

    fn fake_with_viewer() -> Fake {
        let mut f = Fake::new();
        f.viewer = Viewer {
            id: "me".into(),
            ..Default::default()
        };
        f
    }

    const MIN: Duration = Duration::from_secs(60);
    const MS: Duration = Duration::from_millis(1);

    // Mirrors Go `TestClaimPoolWin`.
    #[tokio::test]
    async fn claim_pool_win() {
        let tr = Arc::new(fake_with_viewer());
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, new_state) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(won, "expected to WIN the uncontested claim");
        assert_eq!(
            new_state, "In Progress",
            "winner reports the promoted state"
        );
        assert_eq!(tr.assign_calls().len(), 1, "winner assigns itself once");
        assert_eq!(tr.assign_calls()[0].assignee_id, "me");
        assert_eq!(
            tr.move_calls().len(),
            1,
            "winner moves to the promote state"
        );
        assert_eq!(tr.move_calls()[0].state_name, "In Progress");
        assert_eq!(
            tr.delete_comment_calls().len(),
            0,
            "winner RETAINS its claim comment"
        );
    }

    // Mirrors Go `TestClaimPoolLoseElection`.
    #[tokio::test]
    async fn claim_pool_lose_election() {
        let tr = Arc::new(fake_with_viewer());
        // A competitor claim posted EARLIER wins the election.
        tr.seed_comment(
            "iss-1",
            Comment {
                id: "cmt-0000".into(),
                body: build_claim_body("other", "d2"),
                created_at: Utc::now() - chrono::Duration::seconds(2),
            },
        );
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, _) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(!won, "expected to LOSE to the earlier competitor");
        assert_eq!(tr.assign_calls().len(), 0, "election loser must NOT assign");
        assert_eq!(
            tr.delete_comment_calls().len(),
            1,
            "loser deletes its own claim comment"
        );
    }

    // Mirrors Go `TestClaimPoolLoseReadBack`.
    #[tokio::test]
    async fn claim_pool_lose_read_back() {
        let mut f = fake_with_viewer();
        // We win the election and assign, but a concurrent daemon owns the assignee on read-back.
        f.assignee_read_override
            .insert("iss-1".into(), "someone-else".into());
        let tr = Arc::new(f);
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, _) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(!won, "expected to LOSE on the read-back gate");
        assert_eq!(
            tr.assign_calls().len(),
            1,
            "we assigned before the read-back"
        );
        assert_eq!(
            tr.delete_comment_calls().len(),
            1,
            "read-back loser cleans up its comment"
        );
    }

    // Mirrors Go `TestClaimPoolReadBackErrorAborts`.
    #[tokio::test]
    async fn claim_pool_read_back_error_aborts() {
        let mut f = fake_with_viewer();
        f.assignee_err = Some(boom()); // read-back cannot be verified
        let tr = Arc::new(f);
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, _) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(!won, "a read-back ERROR must ABORT the dispatch (yield)");
        assert_eq!(
            tr.assign_calls().len(),
            1,
            "we assigned before the read-back error"
        );
    }

    // Mirrors Go `TestClaimPoolCommentCreateFails`.
    #[tokio::test]
    async fn claim_pool_comment_create_fails() {
        let mut f = fake_with_viewer();
        f.create_comment_err = Some(boom());
        let tr = Arc::new(f);
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, _) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(!won, "a failed claim comment is a failed claim");
        assert_eq!(tr.list_comments_calls(), 0, "short-circuit before list");
        assert_eq!(tr.assign_calls().len(), 0, "short-circuit before assign");
    }

    // Mirrors Go `TestClaimPoolAssignFails`.
    #[tokio::test]
    async fn claim_pool_assign_fails() {
        let mut f = fake_with_viewer();
        f.assign_err = Some(boom());
        let tr = Arc::new(f);
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, _) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(!won, "a failed assign is a failed claim");
        assert_eq!(tr.assign_calls().len(), 1, "assign attempted once");
        assert_eq!(
            tr.delete_comment_calls().len(),
            1,
            "assign-failure cleans up the comment"
        );
    }

    // Mirrors Go `TestClaimPoolStateMoveFailsButProceeds`.
    #[tokio::test]
    async fn claim_pool_state_move_fails_but_proceeds() {
        let mut f = fake_with_viewer();
        f.move_err = Some(boom()); // the visibility move fails; the assignee is still the lock
        let tr = Arc::new(f);
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, new_state) = o
            .claim_pool(tr.as_ref(), &claim_issue(), "In Progress", MIN, MS)
            .await;
        assert!(
            won,
            "a failed state move must NOT abort the claim (assignee holds the lock)"
        );
        assert_eq!(
            new_state, "",
            "a failed move must not report a promoted state"
        );
        assert_eq!(tr.move_calls().len(), 1, "the state move was attempted");
    }

    // Mirrors Go `TestClaimPoolNoPromoteWhenEmpty`.
    #[tokio::test]
    async fn claim_pool_no_promote_when_empty() {
        let tr = Arc::new(fake_with_viewer());
        let o = Orchestrator::new("WORKFLOW.md");
        let (won, new_state) = o.claim_pool(tr.as_ref(), &claim_issue(), "", MIN, MS).await;
        assert!(won, "expected to win");
        assert_eq!(new_state, "", "no promote state => no reported state");
        assert_eq!(
            tr.move_calls().len(),
            0,
            "no promote state => no state move"
        );
    }

    // Mirrors Go `TestClaimContentionSingleWinner`.
    #[tokio::test]
    async fn claim_contention_single_winner() {
        let tr = Arc::new(fake_with_viewer());
        let o1 = Orchestrator::new("WORKFLOW.md");
        let o2 = Orchestrator::new("WORKFLOW.md");
        assert_ne!(o1.daemon_id, o2.daemon_id, "daemons must have distinct ids");

        let iss = claim_issue();
        let settle = Duration::from_millis(20);
        let (r1, r2) = tokio::join!(
            o1.claim_pool(tr.as_ref(), &iss, "In Progress", MIN, settle),
            o2.claim_pool(tr.as_ref(), &iss, "In Progress", MIN, settle),
        );
        let wins = [r1.0, r2.0].iter().filter(|w| **w).count();
        assert_eq!(wins, 1, "exactly one daemon must win the claim");
        assert_eq!(tr.assign_calls().len(), 1, "exactly one assign must occur");
    }

    // Mirrors Go `TestClaimWinnersStampsPromotedState`.
    #[tokio::test]
    async fn claim_winners_stamps_promoted_state() {
        let tr: Arc<dyn Tracker> = Arc::new(fake_with_viewer());
        let mut o = Orchestrator::new("WORKFLOW.md");
        let mut eff = empty_effective(Arc::clone(&tr));
        eff.active_states = ["todo", "in progress"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        eff.review_promote_state = "In Progress".into();
        eff.claim_ttl = MIN;
        eff.claim_settle_delay = MS;
        o.eff = Some(eff);

        let pick = TaggedIssue {
            iss: Issue {
                id: "iss-1".into(),
                identifier: "P-1".into(),
                state: "Todo".into(),
                team_id: "team-1".into(),
                ..Default::default()
            },
            proj: None,
        };
        let winners = o.claim_winners(vec![pick]).await;
        assert_eq!(winners.len(), 1, "expected 1 winner");
        assert_eq!(
            winners[0].iss.state, "In Progress",
            "winning pick carries the promoted state"
        );
    }
}
