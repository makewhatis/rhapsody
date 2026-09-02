//! Store domain types + string constants — a field-for-field port of Go `internal/store/store.go`.
//!
//! Every struct mirrors its Go counterpart's fields, optionality (Go pointers → [`Option`]), and
//! documented semantics; the string constants reproduce the exact stored values (outcome taxonomy
//! v2, claim states, run-message delivery states). Go's `int`/`int64` map to `i64` (SQLite stores
//! every INTEGER column as an i64 anyway) and Go's `bool` to `bool`. The `json:"…"` field names Go
//! tags on the read-side/wire types are preserved as the snake_case field names here (the HTTP API
//! wire mapping lands with rhapsody-httpapi in a later phase).

// --- outcome taxonomy v2 (INF-272) -----------------------------------------------------------
// Values for runs.outcome. Segment dispositions; the UI derives the four job-level statuses from
// these. The v4->v5 migration rewrites the old strings to exactly this six-value set.

/// live segment
pub const OUTCOME_RUNNING: &str = "running";
/// clean exit, ticket still active → continuation follows
pub const OUTCOME_CONTINUED: &str = "continued";
/// agent-declared hand-off verified by state, or Done-type terminal
pub const OUTCOME_COMPLETED: &str = "completed";
/// Stop button, cancel-type terminal, or external wind-down
pub const OUTCOME_STOPPED: &str = "stopped";
/// error exit (incl. turn timeout) or stall (reason="stalled")
pub const OUTCOME_FAILED: &str = "failed";
/// daemon died mid-segment; boot recovery may continue the job
pub const OUTCOME_INTERRUPTED: &str = "interrupted";

// --- claim states (claims.state) -------------------------------------------------------------

/// A live claim: the issue is actively being worked.
pub const CLAIM_RUNNING: &str = "running";
/// A claim parked in the retry queue.
pub const CLAIM_RETRY_QUEUED: &str = "retry_queued";

// --- run-message delivery states (INF-250) ---------------------------------------------------

/// queued onto the run's mailbox, not yet written to the agent
pub const RUN_MESSAGE_SENT: &str = "sent";
/// actually written to the agent's stdin (delivered_turn set)
pub const RUN_MESSAGE_DELIVERED: &str = "delivered";
/// run ended before the message was written
pub const RUN_MESSAGE_EXPIRED: &str = "expired";

/// RunStart is the dispatch-time record inserted with outcome="running".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStart {
    /// tracker issue id (opaque)
    pub issue_id: String,
    /// human identifier e.g. "MT-12"
    pub issue_identifier: String,
    pub title: String,
    pub attempt: i64,
    pub session_uuid: String,
    pub branch: String,
    /// RFC3339; empty => filled with now at insert
    pub started_at: String,
    pub transcript_path: String,
    /// resolved project slug; "" for legacy single-project
    pub project_slug: String,
    /// project repo URL; "" for legacy hook-clone
    pub repo: String,
    /// tracker team id; needed to move the ticket's state on stop/resume
    pub team_id: String,
}

/// RunEnd is the worker-exit record: final outcome, end time, and final tallies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunEnd {
    pub outcome: String,
    /// RFC3339; empty => filled with now
    pub ended_at: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub error: String,
    /// UsageEstimated marks the token tallies above as a floored ESTIMATE rather than an
    /// authoritative per-turn result total. It is true when the run ended without a clean
    /// `result` event (handoff/timeout/crash) and the persisted total leans on the live
    /// in-flight estimate. The UI surfaces it as an "est." badge (INF-208).
    pub usage_estimated: bool,
    /// TranscriptPath, when non-empty, overwrites runs.transcript_path with the concrete per-run
    /// transcript file (the timestamped *.jsonl, NOT the latest.jsonl alias) so a past run row
    /// resolves to ITS OWN transcript. Empty => leave the column unchanged.
    pub transcript_path: String,
}

/// RunProgress is the per-turn progress update (NOT per-event).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProgress {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    /// Marks the tallies as a floored estimate (see [`RunEnd::usage_estimated`]).
    pub usage_estimated: bool,
    /// When non-empty, overwrites runs.transcript_path (see [`RunEnd::transcript_path`]).
    pub transcript_path: String,
}

/// EventRow is a single captured session event. The field names match the history API's wire
/// shape (Phase 5 /runs/<id>/events => {seq,at,kind,tool,text}).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventRow {
    pub seq: i64,
    /// RFC3339
    pub at: String,
    pub kind: String,
    pub tool: String,
    pub text: String,
}

/// RetryRow is a persisted retry-queue entry. `due_at_ms` is WALL-CLOCK unix-ms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryRow {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: i64,
    pub due_at_ms: i64,
    pub error: String,
    /// resolved project slug; "" for legacy
    pub project_slug: String,
}

/// ClaimRow is a persisted claim (running | retry_queued).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimRow {
    pub issue_id: String,
    pub state: String,
    /// RFC3339
    pub claimed_at: String,
    /// resolved project slug; "" for legacy
    pub project_slug: String,
}

/// Recovery is the boot snapshot loaded into the actor's in-memory state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    pub retries: Vec<RetryRow>,
    pub claims: Vec<ClaimRow>,
}

/// Totals mirrors the orchestrator's cumulative tally for cross-restart continuity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub seconds_running: i64,
}

/// RunFilter selects/pages history runs (Phase 5 /api/v1/history).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// matches issue_identifier (exact)
    pub issue: String,
    pub outcome: String,
    /// RFC3339 lower bound on started_at
    pub since: String,
    /// exact match on runs.project_slug; "" => no project filter
    pub project: String,
    /// <=0 => default page
    pub limit: i64,
    pub offset: i64,
}

/// RunSummary is the read-side projection of a run row (Phase 5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub id: i64,
    pub issue_id: String,
    pub issue_identifier: String,
    pub title: String,
    pub attempt: i64,
    pub session_uuid: String,
    pub branch: String,
    pub started_at: String,
    pub ended_at: String,
    pub outcome: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    /// Reports that the token tallies are a floored estimate rather than an authoritative result
    /// total (run ended without a clean `result`; see [`RunEnd`]) (INF-208).
    pub usage_estimated: bool,
    pub error: String,
    pub transcript_path: String,
    pub project_slug: String,
    pub repo: String,
    pub team_id: String,
}

/// EventQuery is a cross-run text search over events (Phase 5 /api/v1/events).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventQuery {
    /// substring match on events.text (LIKE)
    pub text: String,
    /// optional issue_identifier filter
    pub issue: String,
    /// optional kind filter
    pub kind: String,
    /// <=0 => default
    pub limit: i64,
}

/// EventHit is a search result row: the event plus its owning run's identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventHit {
    pub run_id: i64,
    pub issue_identifier: String,
    pub seq: i64,
    pub at: String,
    pub kind: String,
    pub tool: String,
    pub text: String,
}

/// DayRollup is one row of the per-day metrics aggregation (Phase 5 /api/v1/metrics).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayRollup {
    /// YYYY-MM-DD (UTC)
    pub date: String,
    pub runs: i64,
    pub completed: i64,
    pub failed: i64,
    pub total_tokens: i64,
}

/// DayTotals is the whole-store aggregation over the runs that STARTED within a window — the
/// header "today" figures (TRA-320). Computed in SQL over every matching row so the numbers never
/// depend on which page of `/api/v1/history` a client happened to fetch.
///
/// `seconds` mirrors the dashboard's per-run rule exactly: an in-flight (`outcome = "running"`) run
/// contributes its elapsed time against the caller-supplied `now`, a finished run contributes
/// `ended_at - started_at`, and a row whose timestamps don't parse contributes 0. Rows are unique by
/// run id, so the de-duplication the client used to do by hand is structural here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DayTotals {
    /// Runs that started within the window.
    pub runs: i64,
    /// Of those, the ones whose stored outcome is `completed`.
    pub completed: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// The cache-INCLUSIVE billed total (`input + output + cache_creation + cache_read`), NOT
    /// `input + output` — the same meaning the `runs.total_tokens` column carries per row.
    pub total_tokens: i64,
    /// Whole seconds of run time in the window (in-flight runs counted as elapsed-so-far).
    pub seconds: i64,
}

/// RunMessage is one operator message sent to a run's agent (INF-250). `body` is the operator's
/// ORIGINAL text; the prompt-side labeled wrapper is applied at admission and is NOT stored.
/// `status` is sent | delivered | expired; `delivered_turn` is set only once the runner actually
/// writes the message to the live turn's stdin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMessage {
    pub id: i64,
    pub run_id: i64,
    pub body: String,
    pub created_at_ms: i64,
    pub status: String,
    pub delivered_turn: Option<i64>,
}

// --- ticketless review watch set (STUDIO-703 / STUDIO-711) -----------------------------------
// Values for rhapsody_review_watch.status. NOT a Go port: the frozen reference has no review
// feature at all (see the README "Divergences" entry). The set is closed and exhaustive — a row
// is always in exactly one of these five states.

/// The (PR, reviewer) pair is in the watch set and wants a review, but no reviewer run has been
/// dispatched for the current head yet.
pub const REVIEW_STATUS_REQUESTED: &str = "requested";
/// A reviewer run has been dispatched against [`ReviewWatchRow::requested_sha`] and has not
/// finished. This is the in-flight marker the F-DUP edge-trigger gates on (design §14.1).
pub const REVIEW_STATUS_IN_FLIGHT: &str = "in_flight";
/// The reviewer finished and posted findings; [`ReviewWatchRow::last_reviewed_sha`] holds the SHA
/// they actually read. A later head advance re-arms the row.
pub const REVIEW_STATUS_REVIEWED: &str = "reviewed";
/// The reviewer finished and found nothing. Re-review pauses while the PR stays open at this SHA
/// and a head advance re-arms exactly one more review (design §15-c, "approved-pauses").
pub const REVIEW_STATUS_APPROVED: &str = "approved";
/// The PR left the watch set — merged, closed, or gone. Terminal; paired with `open = false`.
pub const REVIEW_STATUS_DROPPED: &str = "dropped";

/// ReviewWatchKey identifies one watch-set row: a pull request and the ONE reviewer watching it.
///
/// Granularity is per-(PR, reviewer) on purpose. A single `last_reviewed_sha` per PR lets the first
/// completer stamp the PR as reviewed-at-head and silently drops a second reviewer whose run
/// crashed (design §14.2, "N reviewers share one per-PR SHA"), so the reviewer is part of the key
/// rather than a column on a per-PR row.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewWatchKey {
    /// GitHub repository owner (the `owner` of `owner/repo#number`).
    pub owner: String,
    /// GitHub repository name.
    pub repo: String,
    /// Pull-request NUMBER — the stable, number-keyed coordinate Slice 1's `gh` primitive takes.
    pub number: i64,
    /// The reviewing teammate's Teams identity (the `rhapsody:@<name>` label's name).
    pub reviewer: String,
}

/// ReviewWatchRow is one durable watch-set entry: a (PR, reviewer) pair, where it came from, the
/// two head SHAs that make the watcher idempotent, and the PR's own liveness.
///
/// Both SHA columns hold a full head commit SHA — the same value Slice 1's number-keyed `gh`
/// primitive returns as `headRefOid` — and they are written at two DIFFERENT moments by two
/// dedicated methods; see [`crate::Store::mark_review_requested`] and
/// [`crate::Store::mark_review_completed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewWatchRow {
    /// The (PR, reviewer) identity of this row.
    pub key: ReviewWatchKey,
    /// Origin — how this PR entered the watch set (a handoff's own resolved `repo_url`, or an
    /// operator introducing it through the authenticated console). A PR coordinate is NEVER
    /// trusted from room text (design §14.1 F-SEC), so the origin is recorded, not inferred.
    pub introduced_by: String,
    /// The head SHA a reviewer run was DISPATCHED against, written at dispatch. Without it the
    /// re-review condition is level-triggered and stays true every tick from introduction until
    /// the first completion, re-dispatching onto a live worktree (design §14.1 F-DUP).
    pub requested_sha: String,
    /// The head SHA a completed review ACTUALLY read — the SHA pinned at checkout, never a
    /// completion-time re-query, which would record fixes pushed mid-review as reviewed
    /// (design §14.1 F-SHA). Empty until this reviewer has completed a round.
    pub last_reviewed_sha: String,
    /// One of the five `REVIEW_STATUS_*` values above.
    pub status: String,
    /// Whether the pull request is still OPEN. Mirrors Slice 1's PR state: its `OPEN` maps to
    /// `true`; `MERGED`, `CLOSED` and gone (404) all map to `false`, which is the watcher's drop
    /// condition. Kept a flag rather than the four-way state because the store's question is only
    /// "is this still worth watching" — WHY it stopped being open is the watcher's, and it lands
    /// on `status` as [`REVIEW_STATUS_DROPPED`].
    pub open: bool,
}
