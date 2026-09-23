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
/// The run exceeded its configured per-run token ceiling and was stopped mid-turn (STUDIO-967).
/// **Rhapsody-only**, beyond Go's six-value set — the v4->v5 migration never produces it, and Go has
/// no such bound. It is a distinct value on purpose: a ceiling stop is neither a run that failed nor
/// one an operator stopped nor one the daemon was interrupted on, and borrowing any of those labels
/// would be a lie in the history table. The console's `runOutcomeLabel` prints an unrecognised
/// outcome verbatim, so this reads as itself everywhere.
pub const OUTCOME_TOKEN_CEILING: &str = "token_ceiling";
/// A zero-turn refusal: provider/credential preparation was refused before any claim, workspace,
/// or agent attempt (STUDIO-988). **Rhapsody-only**, beyond Go's six-value set. Like
/// [`OUTCOME_TOKEN_CEILING`] it is a distinct value on purpose: a refusal is not a failed agent
/// attempt (no agent ran), not queued work, and not an operator stop, and the API/UI/metrics treat
/// it as its own state so a refused ticket is never rendered as failed work. The run row carries
/// zero turns and zero tokens.
pub const OUTCOME_REFUSED: &str = "refused";

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
    /// Only meaningful for the ISSUE-paged listing ([`Store::list_issue_runs`]): keep an issue only
    /// when its NEWEST run carries this outcome. Distinct from [`RunFilter::outcome`], which filters
    /// BEFORE the per-issue partition and so returns each issue's newest run *with that outcome* —
    /// often an old run of a finished ticket (STUDIO-931). "the issue's newest run" is not a
    /// run-paged concept, so [`Store::list_runs`] ignores this. "" => no filter.
    pub latest_outcome: String,
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

/// RunProvenance is what a run ACTUALLY ran on — the harness, the model and the provider — plus the
/// origin of each configurable value, recorded at dispatch (STUDIO-909). Rhapsody-only: the frozen
/// Go reference records none of it.
///
/// It lives in its own [`rhapsody_run_provenance`](crate::Store) table keyed by `run_id` rather than
/// as columns on `runs`, because the `runs` DDL is byte-pinned to Go by the schema golden and that
/// golden is recapturable ONLY from the real Go daemon — which can never emit these columns. Adding
/// them to `runs` would turn `schema_matches_committed_golden` permanently red with no honest fix,
/// so the store uses the documented Rhapsody-only mechanism (a `rhapsody_`-prefixed table) instead.
/// See the README "Divergences" entry.
///
/// The values are a PROVENANCE RECORD, not a config echo: they are read once from the run that
/// actually executed and persisted, so a later WORKFLOW.md hot-reload cannot rewrite history. A run
/// started before this existed has no row and renders as unknown.
///
/// `harness_origin`/`model_origin` name the config key the value came from — `profile`,
/// `review.model.opencode`, `agent.backend`, `claude.model` — so an invisible override becomes
/// visible. `provider` is DERIVED once, at the same moment, from the recorded harness and model
/// string, and never re-derived at render time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProvenance {
    pub harness: String,
    /// The config key `harness` resolved from (e.g. `profile`, `agent.backend`).
    pub harness_origin: String,
    pub model: String,
    /// The config key `model` resolved from (e.g. `profile`, `review.model.opencode`, `claude.model`).
    pub model_origin: String,
    /// Derived from the recorded harness + model, not from live config.
    pub provider: String,
}

/// The windowed token tally for ONE provider — the cost question STUDIO-909 exists to answer
/// ("what did Fireworks save us this week?"), which is unanswerable while a run's tokens cannot be
/// attributed to a provider. Aggregated in SQL over the `runs` ⋈ `rhapsody_run_provenance` join so
/// the figure never depends on which page a client happened to fetch, and bounded by the same
/// `since` as `day_totals` so the split and the total describe one window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderTokens {
    /// The recorded provider; empty for a run that recorded none (a legacy row, or a harness whose
    /// provider could not be determined). Empty is reported as its own bucket rather than dropped.
    pub provider: String,
    pub runs: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

/// One (issue key, provider) bucket of the whole-store token ledger — the raw material of the
/// per-ticket cost split (STUDIO-926). The key is the run's OWN `issue_identifier`, so a review run
/// (`pr:owner/repo#n@reviewer`) is a bucket of its own; folding it into the ticket it reviewed needs
/// the review watch set and happens in the HTTP layer, not here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunCostBucket {
    pub issue_identifier: String,
    /// The recorded provider; empty for a run that recorded none (a legacy row), reported as its
    /// own bucket rather than dropped, exactly like [`ProviderTokens::provider`].
    pub provider: String,
    pub total_tokens: i64,
    /// True when ANY run in the bucket ended without a clean `result` event (a floored figure).
    pub usage_estimated: bool,
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
    /// Optional run filter; `<=0` means every run. Narrows the search to ONE run's slice of the
    /// ledger, which [`Store::run_events`] also does but unbounded — this keeps the `LIMIT` and the
    /// `kind` filter, so a caller asking "did THIS run record a routing decision?" reads one indexed
    /// row rather than the run's whole event history (STUDIO-735).
    pub run: i64,
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

/// One day's token/run rollup for ONE provider — the per-day decomposition of [`DayRollup`] by the
/// account a run actually billed (STUDIO-957). A single undifferentiated daily total hides the only
/// figure an operator can act on: implementation on Fireworks and reviews on Anthropic draw on
/// different accounts, so "987M tokens" is one number over two budgets. Aggregated in SQL over the
/// `runs` ⋈ `rhapsody_run_provenance` join, LEFT-joined so a run with no recorded provenance still
/// counts (in the empty-provider bucket) rather than vanishing from the series.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayProviderRollup {
    /// YYYY-MM-DD (UTC), the same bucket [`DayRollup::date`] uses.
    pub date: String,
    /// The recorded provider; empty for a run that recorded none (a legacy row, or a harness whose
    /// provider could not be determined). Reported as its own bucket rather than dropped.
    pub provider: String,
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
/// The reviewer run ENDED without the agent ever declaring it had finished: it burned its whole
/// turn budget (`max_turns`) mid-review. **Deliberately non-terminal** (STUDIO-721): the head was
/// read only partially, so `last_reviewed_sha` is NOT advanced and the watcher re-reviews this same
/// head. Recording such a round as [`REVIEW_STATUS_REVIEWED`] is how a partial — or entirely
/// absent — review ships as if it had happened.
pub const REVIEW_STATUS_TRUNCATED: &str = "truncated";

// --- per-run review verdicts (STUDIO-1020) ----------------------------------------------------
// Values for rhapsody_review_verdicts.verdict. NOT a Go port: the frozen reference has no review
// feature. Unlike ReviewWatchRow.status, which holds the LATEST state per (PR, reviewer) and so
// cannot describe an older round, one of these is recorded against each review RUN id and never
// changes — the run detail's strip colours each round by its own outcome.

/// The reviewer finished and declared `HANDOFF: approved` — nothing to fix.
pub const REVIEW_VERDICT_APPROVED: &str = "approved";
/// The reviewer finished and declared findings (`HANDOFF: findings` / `not approved`).
pub const REVIEW_VERDICT_CHANGES_REQUESTED: &str = "changes_requested";

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
    /// The teammate whose handoff produced the pull request — the ONE identity that must never be
    /// selected to review it (design §13.1, "picks a teammate … who is not the author").
    ///
    /// Persisted rather than re-derived because the watcher substitutes reviewers long after the
    /// authoring run has ended: `runs` carries no identity column (the routing decision lives in
    /// the `teams.route` event, not on the row), so by the time a capped reviewer needs replacing
    /// there is nothing left in the daemon that knows who wrote the pull request. Empty means
    /// UNKNOWN, and a consumer must fail closed on it rather than treat it as "nobody is the
    /// author" — see `Orchestrator::choose_review_reviewer`.
    pub author: String,
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

/// The newest summons Rhapsody has ever OBSERVED for one ticket — one row per ticket identifier,
/// and the durable half of the summons re-engagement decision (STUDIO-885). No Go counterpart.
///
/// A summons is a durable fact: an `@symphony` comment that still exists on the pull request. The
/// daemon nevertheless only ever SAW it as a transient one — GitHub enrichment asks for comments
/// newer than `now - ghLookback` (five minutes), so `Issue::latest_summon_at` is re-derived from
/// scratch every poll and reverts to unset the moment the comment ages out of that window. A
/// ticket whose summons landed while the board was at its concurrency cap therefore lost the only
/// thing that lifts `pr_suppressed`, and stayed suppressed for as long as the daemon ran.
///
/// Remembering the observation decouples the two: the comparison `pr_suppressed` actually makes —
/// summons versus the ticket's LAST RUN START — is between two durable facts, so it keeps its
/// meaning however long the ticket waits for a slot. It does NOT weaken the suppression: a ticket
/// whose newest summons predates its last run start is still suppressed, which is the whole point
/// of the rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummonWatermark {
    /// The ticket's tracker identifier (`STUDIO-879`) — the row's primary key. Identifier, not
    /// opaque id, because that is the key `last_run_started_at` compares against.
    pub identifier: String,
    /// When the summons was posted, RFC3339 UTC at SECONDS precision with a `Z` suffix — the same
    /// canonical form every other timestamp column in this store uses. Produced by
    /// [`format_summon_at`](crate::format_summon_at) and by nothing else, so the column is
    /// fixed-width and a lexicographic comparison (the retention cutoff in `prune`) is a
    /// chronological one.
    pub at: String,
    /// The body of that SAME comment (INF-448 keeps time and body describing one comment), so a
    /// re-engagement the watermark triggers can still seed the run with what the reviewer wrote.
    /// Empty when the source could not surface one.
    pub body: String,
}

/// The `decision` column of a SETTLED "ship it" adjudication (STUDIO-956).
pub const REVIEW_ADJUDICATION_SHIP: &str = "ship";
/// The `decision` column of a SETTLED "escalate" adjudication (STUDIO-956).
pub const REVIEW_ADJUDICATION_ESCALATE: &str = "escalate";

/// What the manager DECIDED about one pull request's review loop, durably (STUDIO-956). No Go
/// counterpart — the whole review feature is a Rhapsody addition.
///
/// Only SETTLED decisions are representable here. The in-flight marker the control task uses to
/// stop re-asking while a turn is out is deliberately NOT persisted: a marker that outlived the
/// process that was going to land it would stop every further round forever, with no turn left
/// anywhere to clear it. An adjudication interrupted by a restart is therefore simply re-asked,
/// which costs one turn and cannot deadlock the loop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewAdjudication {
    /// [`REVIEW_ADJUDICATION_SHIP`] or [`REVIEW_ADJUDICATION_ESCALATE`]. A row whose decision is
    /// neither (including the empty string a counter-only row carries) records no decision at all.
    pub decision: String,
    /// The head the loop stopped at.
    pub head: String,
    /// How many review↔author ROUNDS had been spent when the decision was made.
    pub rounds: i64,
    /// The open findings named to the manager, one human-readable line each. Stored NEWLINE-JOINED
    /// in one column: every finding this daemon produces is a single line by construction, and the
    /// writer folds any embedded newline to a space rather than let one split a finding in two.
    pub findings: Vec<String>,
    /// The manager's own words for an escalation. Empty for a ship.
    pub reason: String,
}

/// One pull request's durable REVIEW BOUND: how much of its review↔author loop has been spent, and
/// what the manager decided about it (STUDIO-956). No Go counterpart.
///
/// It exists because the bound it carries was in memory and therefore was not a bound at all: on
/// 2026-09-20 five daemon restarts — every one of them to apply a boot-only `teams.yaml` change —
/// handed seven in-flight pull requests a fresh budget each, and one pull request ran 46 review
/// rounds against a nominal cap of 16. A restart must not refund a spent budget, and it must not
/// forget a decision the manager already made.
///
/// Keyed by the PULL REQUEST (`owner/repo#number`, case-folded — `reviewwatch::churn_key`'s
/// spelling) so the row means "rounds spent on this pull request", never "rounds since some daemon
/// booted". The row is DELETED when the pull request leaves the watch set, so a re-introduced,
/// reopened or rebuilt pull request starts from zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewBoundRow {
    /// `owner/repo#number`, case-folded. The primary key.
    pub pr: String,
    /// The round counter in DISPATCHES (one round costs one dispatch per required reviewer), which
    /// is the unit `REVIEW_ROUNDS_PER_PR_CAP` and the adjudication threshold are both compared in.
    pub dispatches: i64,
    /// The manager's settled decision, or `None` when there is none.
    pub adjudication: Option<ReviewAdjudication>,
}

/// One ticket's durable runaway-loop-breaker crossings (STUDIO-1026). No Go counterpart — the
/// breaker is a Rhapsody addition end to end.
///
/// It exists so a crossing NOTIFIES ONCE and survives a restart: the maintainer wants to be told
/// when a loop crosses a limit, not reminded every tick, and a daemon restart must not re-tell them
/// about a crossing they have already seen. The row records the highest round count at which a
/// round-crossing fired and the providers whose per-ticket cap has fired; the next round-crossing
/// fires only at `notified_rounds + hold_after_rounds`, and a provider fires only once. An operator
/// who un-holds the ticket by removing the label therefore gets the next crossing (for example
/// round ten after five) rather than the same one again.
///
/// Keyed by the TICKET identifier. A ticket's PR can change (a rebuilt or re-introduced pull
/// request), and "the ticket's spend" is what the maintainer wants bounded, so the key is the
/// ticket and not the pull request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BreakerCrossingRow {
    /// The human ticket id, e.g. `STUDIO-988`. The primary key.
    pub ticket: String,
    /// How many COMPLETED review runs had HAPPENED when the last round-crossing notified. `0`
    /// before the first crossing. The next crossing fires at
    /// `notified_rounds + review.hold_after_rounds`.
    pub notified_rounds: i64,
    /// The providers whose per-ticket cap has already notified. Newline-joined in one column, like
    /// [`ReviewAdjudication::findings`]' reasons; a provider fires at most once per row.
    pub notified_providers: Vec<String>,
}
