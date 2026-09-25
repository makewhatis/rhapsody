import type { NoteVariant, PillVariant } from "@/components/console";
import type { ManagerState, ReviewActionResponse, ReviewJob } from "@/lib/api";

// reviews-model — the pure logic behind the console's Reviews surface (STUDIO-722, slice 8 of the
// design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §7, §15-e).
//
// Separate from the component, which is this codebase's discipline for anything worth asserting on
// directly (see room-model, teams-model, console-jobs). It matters here because the daemon serves
// STATE, not presentation: a row carries a `status` string, two SHAs and an `open` flag, and every
// question the surface answers — is this still live, may an operator act on it, what does the pill
// say — is derived from those. The status values are declared in `crates/store/src/types.rs`
// (`REVIEW_STATUS_*`); a value added there and not here degrades to the muted default rather than
// blanking the row.

/** The six statuses `rhapsody_review_watch.status` can hold. */
export const REVIEW_STATUSES = [
  "requested",
  "in_flight",
  "reviewed",
  "approved",
  "truncated",
  "dropped",
] as const;

export type ReviewStatus = (typeof REVIEW_STATUSES)[number];

/** Whether the surface is showing everything or only the rows still being watched. */
export type ReviewFilter = "active" | "all";

export interface ReviewRow {
  /** `owner/repo#number@reviewer` — unique per row, and the daemon's own review key. */
  key: string;
  job: ReviewJob;
  /** `owner/repo#number`, the coordinate the controls act on. */
  pr: string;
  /** The GitHub URL of the pull request. */
  url: string;
  /** The pill the status renders as. */
  variant: PillVariant;
  /** The human label beside it. */
  label: string;
  /** The seven-character head SHA the last completed review read; "" when none has. */
  reviewedShort: string;
  /** Still worth watching: open, and not retired. Only these may be acted on. */
  live: boolean;
}

/**
 * How each status reads to an operator. Two pairs deliberately share a variant, because the pill's
 * job is "what is happening", not "which of six strings is stored":
 *
 * * `reviewed` and `approved` are both a round that FINISHED — the difference is whether findings
 *   were posted, which is on the pull request, not here.
 * * `requested` and `truncated` are both a round that is OWED. `truncated` is the one that would
 *   mislead if it were dressed as a finished review: either the reviewer ran out of turns mid-diff,
 *   or it declared a hand-off with no readable verdict (STUDIO-894) — either way the head was not
 *   conclusively read, and the daemon deliberately did not advance `last_reviewed_sha`.
 */
const STATUS_LOOK: Record<ReviewStatus, { variant: PillVariant; label: string }> = {
  requested: { variant: "queued", label: "Queued" },
  in_flight: { variant: "run", label: "Reviewing" },
  reviewed: { variant: "done", label: "Reviewed" },
  approved: { variant: "done", label: "Approved" },
  truncated: { variant: "blocked", label: "No verdict" },
  dropped: { variant: "queued", label: "Dropped" },
};

function isStatus(v: string): v is ReviewStatus {
  return (REVIEW_STATUSES as readonly string[]).includes(v);
}

/** `owner/repo#number` — the form the daemon, the room and the design record all use. */
export function prLabel(job: ReviewJob): string {
  return `${job.owner}/${job.repo}#${job.number}`;
}

/** The first seven characters of a SHA, the length git itself abbreviates to. */
export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

/**
 * Whether a row is still being watched — the ONE predicate that decides whether the controls are
 * offered, mirroring the daemon's own `load_live_review_watch` (`open AND status != 'dropped'`).
 *
 * It is derived rather than served as a flag on purpose: the daemon already publishes both facts,
 * and a third denormalised field could disagree with them.
 */
export function isLive(job: ReviewJob): boolean {
  return job.open && job.status !== "dropped";
}

/**
 * How a row reads to an operator.
 *
 * `open` is consulted BEFORE the status, and that ordering is the point. A pull request dismissed
 * while a review of it was running comes back as `open: false, status: "reviewed"`: the drop parked
 * the status at `dropped`, and then the finishing agent's `mark_review_completed` — whose contract
 * is to write the status and never touch `open` — wrote its own terminal back over it. The row is
 * correctly out of every live read either way, so this is display only, but a "Reviewed" pill on a
 * row the operator just dismissed reads as a click that did not land. `open` is the column no
 * completion can rewrite and nothing but a drop ever clears, so it is the honest one to read.
 */
function rowLook(job: ReviewJob): { variant: PillVariant; label: string } {
  if (!job.open) return STATUS_LOOK.dropped;
  if (isStatus(job.status)) return STATUS_LOOK[job.status];
  // A status this build has never heard of — the daemon grew one. Show it verbatim rather than
  // guessing what it means or dropping the row: an unrenderable review is worse than an unfamiliar
  // label.
  return { variant: "queued", label: job.status || "unknown" };
}

/** One row, ready to render. */
export function reviewRow(job: ReviewJob): ReviewRow {
  const pr = prLabel(job);
  const look = rowLook(job);
  return {
    key: `${pr}@${job.reviewer}`,
    job,
    pr,
    url: `https://github.com/${job.owner}/${job.repo}/pull/${job.number}`,
    variant: look.variant,
    label: look.label,
    reviewedShort: shortSha(job.last_reviewed_sha),
    live: isLive(job),
  };
}

/**
 * The rows the surface renders, in the order it renders them.
 *
 * Live rows first, because a retired one is history and the daemon's own order (owner, repo,
 * number, reviewer) would otherwise interleave the two. Within each half the daemon's order is
 * kept, so two reviewers of one pull request stay adjacent — `sort` is stable in every engine this
 * ships to (ES2019 requires it).
 */
export function reviewRows(jobs: readonly ReviewJob[], filter: ReviewFilter): ReviewRow[] {
  const rows = jobs.map(reviewRow);
  const visible = filter === "all" ? rows : rows.filter((r) => r.live);
  return visible.sort((a, b) => Number(b.live) - Number(a.live));
}

/** How many rows the "Show retired" toggle would reveal — 0 retires the toggle. */
export function retiredCount(jobs: readonly ReviewJob[]): number {
  return jobs.filter((j) => !isLive(j)).length;
}

/** The summary line above the table: what the daemon is currently doing about reviews. */
export function reviewStats(jobs: readonly ReviewJob[]): {
  pullRequests: number;
  inFlight: number;
  awaiting: number;
} {
  const live = jobs.filter(isLive);
  return {
    // Distinct PULL REQUESTS, not rows: N reviewers of one pull request are one thing being
    // reviewed, and counting rows would make a two-reviewer config look twice as busy.
    pullRequests: new Set(live.map(prLabel)).size,
    inFlight: live.filter((j) => j.status === "in_flight").length,
    awaiting: live.filter((j) => j.status === "requested" || j.status === "truncated").length,
  };
}

/**
 * What a completed control tells the operator.
 *
 * Both controls answer `200 {pr, rows}`, and `rows` is the whole difference between "the daemon did
 * what you asked" and "the daemon accepted the click and changed nothing" — a re-run of a pull
 * request whose reviews are all already in flight is exactly that second case. Discarding the count
 * leaves the operator staring at an unchanged table with no way to tell the two apart, so the
 * surface renders it.
 */
export interface ReviewNotice {
  /** The `Note` variant that carries it: `info` for a change, `warn` for a no-op. */
  tone: NoteVariant;
  text: string;
}

/** "1 reviewer" / "2 reviewers", so the sentence reads. */
function reviewers(n: number): string {
  return `${n} ${n === 1 ? "reviewer" : "reviewers"}`;
}

/**
 * The outcome of a re-run. `rows` counts the reviewers that now owe a round — one that was already
 * `requested` counts too, because the operator's question is "will this be read again" and that is
 * a yes. Zero means every reviewer of the pull request has a round IN FLIGHT: the daemon
 * deliberately leaves those rows alone, since re-arming one would overwrite the `in_flight` marker
 * the watcher's edge trigger reads and point a second agent at the first one's worktree.
 */
export function rerunNotice(res: ReviewActionResponse): ReviewNotice {
  if (res.rows === 0) {
    return {
      tone: "warn",
      text: `A review of ${res.pr} is already running — nothing to re-arm. It will report when it finishes.`,
    };
  }
  return {
    tone: "info",
    text: `${res.pr} is re-armed — ${reviewers(res.rows)} will read the current head again.`,
  };
}

/**
 * The outcome of a dismissal, including the part of it the operator cannot take back.
 *
 * `liveReview` says whether a round of this pull request was `in_flight` when the operator clicked.
 * A dismissal deliberately does NOT stop one — stopping a run is `POST /api/v1/runs/{id}/stop`'s
 * job, and the drop cannot resurrect the row afterwards — so an agent stays checked out on that
 * head and still posts its findings while the row reads `Dropped`. That combination is exactly what
 * makes an operator believe they cancelled a review they did not, so it is said out loud.
 */
export function dismissNotice(res: ReviewActionResponse, liveReview: boolean): ReviewNotice {
  if (res.rows === 0) {
    return {
      tone: "warn",
      text: `Nothing changed — the daemon dropped no watch row of ${res.pr}.`,
    };
  }
  const dropped = `${res.pr} is out of the watch set — ${res.rows} ${res.rows === 1 ? "row" : "rows"} dropped. Only a new hand-off re-introduces it.`;
  if (liveReview) {
    return {
      tone: "warn",
      text: `${dropped} A review of it is still running: dismissing does not stop it, so it will finish and post its findings. Stop the run itself if that is what you meant.`,
    };
  }
  return { tone: "info", text: dropped };
}

/**
 * The outcome of clearing a pull request's shared review↔author round budget (STUDIO-956).
 *
 * A success is always one row: the daemon answers a pull request with no budget with a `409`
 * refusal, not an `Applied(0)`, so a zero never reaches here — the refusal path is `refused(e)` on
 * the mutation's error, not this notice. So this only ever says the bound is gone and both halves of
 * the loop may run again.
 */
export function clearNotice(res: ReviewActionResponse): ReviewNotice {
  return {
    tone: "info",
    text: `The review↔author round budget for ${res.pr} is cleared — its next round may dispatch.`,
  };
}

// --- the manager's state (STUDIO-1018, design §9/§10.2) ---------------------------------------

/** The manager state, ready to render: a pill plus the reason and decision behind it. */
export interface ManagerView {
  variant: PillVariant;
  label: string;
  /** The §10.2 deferral, unavailable or stop reason; "" when the state carries none. */
  reason: string;
  /** True for an `advise` proposal — recorded, never applied. Rendered DISTINCTLY from a decision. */
  proposal: boolean;
  /** `act` or `advise`. In `advise` the manager is advisory: today's review call stays authoritative. */
  mode: string;
  /** The validated decision behind the state, when one was recorded. */
  decision: {
    kind: string;
    rationale: string;
    /** `alice:F1 @ r1` lines, one per dismissed revision. */
    dismissals: string[];
    /** The specific question an escalation asks, when the decision is one. */
    question: string;
    unapplied: boolean;
  } | null;
}

/**
 * How a manager state reads to an operator, or `null` when there is nothing to render. The state
 * labels are the ticket's list (§10.2 plus §7.2's lifecycle). An `advise` proposal takes the accent
 * `review` variant so it can never be mistaken for an applied decision, which is the ticket's
 * explicit "don't make proposals look like applied decisions".
 */
export function managerView(m: ManagerState | undefined): ManagerView | null {
  if (!m || m.state === "") return null;
  const decision = m.decision
    ? {
        kind: m.decision.kind,
        rationale: m.decision.rationale,
        dismissals: m.decision.dismissals.map((d) => `${d.finding} @ r${d.revision}`),
        question: m.decision.question ?? "",
        unapplied: m.decision.unapplied,
      }
    : null;
  const base = (variant: PillVariant, label: string, reason = ""): ManagerView => ({
    variant,
    label,
    reason,
    proposal: m.proposal,
    mode: m.mode,
    decision,
  });
  switch (m.state) {
    case "queued":
      return base("queued", "Manager queued");
    case "deferred":
      return base("blocked", "Manager deferred", m.reason);
    case "stale":
      return base("queued", "Manager re-queued");
    case "launching":
    case "running":
      return base("run", "Manager working");
    case "decided":
    case "validated":
    case "applying":
      return base("run", "Manager applying");
    case "awaiting_effect":
      return base("run", "Manager awaiting effect");
    case "complete":
      return base("done", "Manager complete");
    case "proposed":
      // An advise proposal: recorded, never applied. Its own accent so it is unmistakably not a
      // decision the daemon acted on.
      return base("review", "Manager proposal");
    case "escalated":
      return base("blocked", "Manager escalated", m.reason);
    case "exhausted":
      return base("blocked", "Manager exhausted");
    case "no_review_gap":
      return base("parked", "Manager: no review gap");
    case "superseded":
      return base("parked", "Manager superseded");
    case "failed_attempt":
      return base("blocked", "Manager retrying");
    case "effect_timeout":
    case "apply_failed":
    case "apply_uncertain":
      return base("blocked", `Manager ${m.state.replace(/_/g, " ")}`);
    case "stopped":
      return base("blocked", "Manager stopped", m.reason);
    case "unavailable":
      return base("blocked", "Manager unavailable", m.reason);
    default:
      // A state this build has never heard of: show it verbatim rather than guessing or hiding it.
      return base("queued", `Manager ${m.state}`);
  }
}

/** The manager state of the first row that carries one — the state is per PR, so all rows agree. */
export function managerFor(jobs: readonly ReviewJob[]): ManagerState | undefined {
  return jobs.find((j) => j.manager !== undefined)?.manager;
}

