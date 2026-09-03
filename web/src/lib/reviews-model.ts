import type { PillVariant } from "@/components/console";
import type { ReviewJob } from "@/lib/api";

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
 *   mislead if it were dressed as a finished review: the reviewer ran out of turns mid-diff, so
 *   the head was only partly read and the daemon deliberately did not advance `last_reviewed_sha`.
 */
const STATUS_LOOK: Record<ReviewStatus, { variant: PillVariant; label: string }> = {
  requested: { variant: "queued", label: "Queued" },
  in_flight: { variant: "run", label: "Reviewing" },
  reviewed: { variant: "done", label: "Reviewed" },
  approved: { variant: "done", label: "Approved" },
  truncated: { variant: "blocked", label: "Ran out of turns" },
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

/** One row, ready to render. */
export function reviewRow(job: ReviewJob): ReviewRow {
  const pr = prLabel(job);
  const look = isStatus(job.status)
    ? STATUS_LOOK[job.status]
    : // A status this build has never heard of — the daemon grew one. Show it verbatim rather than
      // guessing what it means or dropping the row: an unrenderable review is worse than an
      // unfamiliar label.
      { variant: "queued" as PillVariant, label: job.status || "unknown" };
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
  return visible.slice().sort((a, b) => Number(b.live) - Number(a.live));
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
