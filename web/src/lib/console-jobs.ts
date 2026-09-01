// The Jobs worklist model — STUDIO-681 §3, built by STUDIO-683.
//
// The console's Jobs view is the operator's queue: one row per ticket the daemon is working.
// The row-per-issue merge itself is NOT re-implemented here — `runs-model.mergeJobs` already
// folds the live snapshot (`/api/v1/state`), the pending retries and the issue-level history
// (`/api/v1/history/issues`) into one row per ticket, and it is the tested source of a job's
// status. This module is the console's PRESENTATION layer over that: it renames the daemon's
// run-centric statuses into the five the spec's Pill speaks, and derives the Now-strip counts,
// the two filters and the project list from them.
//
// DEPENDENCY (§9/§11): the spec maps this view to `GET /api/v1/issues`, which the daemon does
// not serve. There is therefore no TRACKER state, no assignee and no PR link per ticket. What
// is used instead, and what it costs:
//   - Status  — derived from the daemon's own job status (`jobStatus`), not the Linear state.
//               `done` consequently never occurs: nothing tells the dashboard a ticket merged.
//   - Assignee— resolved from the Teams roster's LIVE tickets (`GET /api/v1/teams`), so a
//               finished run shows "—": no teammate identity is recorded on a stored run row.
//   - PR      — no endpoint carries one; the column renders "—" until one does.
import type { RunSummary, TeamsOverview } from "@/lib/api";
import type { JobRow, JobStatusKey } from "@/lib/runs-model";

/** The five states the console's Pill paints (§1.3). */
export type ConsoleJobStatus = "run" | "review" | "queued" | "done" | "blocked";

export type ConsoleJobFilterId = "all" | ConsoleJobStatus;

/** The status Seg of §3, in the prototype's order. */
export const CONSOLE_JOB_FILTERS: readonly { id: ConsoleJobFilterId; label: string }[] = [
  { id: "all", label: "All" },
  { id: "review", label: "In review" },
  { id: "run", label: "Running" },
  { id: "queued", label: "Queued" },
  { id: "done", label: "Done" },
];

/** The Pill's text per status — the prototype's wording. */
export const CONSOLE_STATUS_LABELS: Record<ConsoleJobStatus, string> = {
  run: "running",
  review: "in review",
  queued: "queued",
  done: "done",
  blocked: "blocked",
};

/**
 * Maps the daemon's job status onto the console's vocabulary.
 *
 * `completed → review` is the pipeline's own rule, not a guess: a run that finishes cleanly
 * hands its ticket to the configured review state, so a completed run means "waiting on a
 * reviewer". `failed` and `waiting` both mean a human has to act, which is what `blocked`
 * says; `stopped` leaves the ticket idle awaiting its next dispatch, which is `queued`.
 *
 * `done` is unreachable from run outcomes alone — see this module's DEPENDENCY note.
 */
export function consoleJobStatus(status: JobStatusKey): ConsoleJobStatus {
  switch (status) {
    case "running":
      return "run";
    case "completed":
      return "review";
    case "failed":
    case "waiting":
      return "blocked";
    default:
      return "queued";
  }
}

/** One row of the §3 worklist, fully derived so the table stays presentational. */
export interface ConsoleJobRow {
  /** Stable React key. */
  key: string;
  /** Ticket key — also the `job/:key` route target (§10 box 2.8). */
  issue: string;
  title: string;
  /** Project display name, or "—" when the daemon runs single-project. */
  project: string;
  /** Raw project slug — the project Select's value. */
  projectSlug: string;
  status: ConsoleJobStatus;
  statusLabel: string;
  /** Teammate name, or "" when solo/unassigned (the table renders "—"). */
  assignee: string;
  /** PR reference, or "" when none is known. */
  pr: string;
  /** Relative "6m ago", or "—" when the ticket has never run. */
  updated: string;
  /** Sort key: ms since epoch of the newest activity, 0 when unknown. */
  updatedAtMs: number;
  /** Held/failed detail, e.g. "waiting on STUDIO-1 · In Progress". */
  subLabel?: string;
}

/**
 * Ticket key → teammate name, from the roster's LIVE tickets. Only a running ticket resolves:
 * a stored run row carries no teammate identity (`store::RunSummary` has none), so history
 * cannot be attributed after the fact.
 */
export function ticketAssignees(overview: TeamsOverview | undefined): Map<string, string> {
  const byTicket = new Map<string, string>();
  for (const mate of overview?.roster ?? []) {
    for (const ticket of mate.tickets ?? []) {
      if (ticket !== "" && !byTicket.has(ticket)) byTicket.set(ticket, mate.name);
    }
  }
  return byTicket;
}

/**
 * Newest activity per ticket, from the issue-level history rows: a run's end when it has one,
 * else its start. `mergeJobs` surfaces only the start, and the column says "Updated".
 */
export function lastActivityByIssue(rows: readonly RunSummary[]): Map<string, number> {
  const byIssue = new Map<string, number>();
  for (const r of rows) {
    if (r.issue_identifier === "") continue;
    const at = parseMs(r.ended_at) || parseMs(r.started_at);
    const seen = byIssue.get(r.issue_identifier) ?? 0;
    if (at > seen) byIssue.set(r.issue_identifier, at);
  }
  return byIssue;
}

function parseMs(iso: string): number {
  if (!iso) return 0;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? 0 : ms;
}

/**
 * A compact "6m ago" for the Updated column. Returns "—" for an unknown or future instant —
 * a clock skew must read as "no information", never as a negative age.
 */
export function relativeSince(atMs: number, nowMs: number): string {
  if (atMs <= 0) return "—";
  const secs = Math.floor((nowMs - atMs) / 1000);
  if (secs < 0) return "—";
  if (secs < 60) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** Projects the Select offers — every project present in the rows, de-duplicated, sorted. */
export function consoleJobProjects(
  rows: readonly ConsoleJobRow[],
): { value: string; label: string }[] {
  const bySlug = new Map<string, string>();
  for (const row of rows) {
    if (row.projectSlug !== "" && !bySlug.has(row.projectSlug)) {
      bySlug.set(row.projectSlug, row.project);
    }
  }
  return [...bySlug.entries()]
    .sort((a, b) => a[1].localeCompare(b[1]))
    .map(([value, label]) => ({ value, label }));
}

/** The §3 worklist rows, newest activity first with running tickets pinned to the top. */
export function buildConsoleJobs(
  jobs: readonly JobRow[],
  issueRows: readonly RunSummary[],
  overview: TeamsOverview | undefined,
  nowMs: number,
): ConsoleJobRow[] {
  const assignees = ticketAssignees(overview);
  const activity = lastActivityByIssue(issueRows);

  const out = jobs.map((job): ConsoleJobRow => {
    const status = consoleJobStatus(job.status as JobStatusKey);
    const updatedAtMs = activity.get(job.issue) ?? job.startedAtMs;
    return {
      key: job.key,
      issue: job.issue,
      title: job.title,
      project: job.projectShort,
      projectSlug: job.project,
      status,
      statusLabel: CONSOLE_STATUS_LABELS[status],
      assignee: assignees.get(job.issue) ?? "",
      pr: "",
      updated: relativeSince(updatedAtMs, nowMs),
      updatedAtMs,
      subLabel: job.subLabel,
    };
  });

  out.sort((a, b) => {
    const ar = a.status === "run" ? 0 : 1;
    const br = b.status === "run" ? 0 : 1;
    if (ar !== br) return ar - br;
    return b.updatedAtMs - a.updatedAtMs;
  });
  return out;
}

/** §10 box 2.7 — the status Seg. */
export function matchConsoleFilter(row: ConsoleJobRow, filter: ConsoleJobFilterId): boolean {
  return filter === "all" || row.status === filter;
}

/** §10 box 2.7 — the status Seg and the project Select, applied together. */
export function filterConsoleJobs(
  rows: readonly ConsoleJobRow[],
  filter: ConsoleJobFilterId,
  projectSlug: string,
): ConsoleJobRow[] {
  return rows.filter(
    (row) =>
      matchConsoleFilter(row, filter) && (projectSlug === "" || row.projectSlug === projectSlug),
  );
}

/** The four Now-strip stat pills of §3 (§10 box 2.6). */
export interface ConsoleJobCounts {
  running: number;
  review: number;
  queued: number;
  blocked: number;
}

export function consoleJobCounts(rows: readonly ConsoleJobRow[]): ConsoleJobCounts {
  const counts: ConsoleJobCounts = { running: 0, review: 0, queued: 0, blocked: 0 };
  for (const row of rows) {
    if (row.status === "run") counts.running += 1;
    else if (row.status === "review") counts.review += 1;
    else if (row.status === "queued") counts.queued += 1;
    else if (row.status === "blocked") counts.blocked += 1;
  }
  return counts;
}

/** One teammate's live state in the Now strip (§3). */
export interface MateState {
  name: string;
  /** The ticket they are on, or "idle". */
  task: string;
  running: boolean;
}

export function mateStates(overview: TeamsOverview | undefined): MateState[] {
  return (overview?.roster ?? []).map((mate) => {
    const tickets = mate.tickets ?? [];
    return {
      name: mate.name,
      task: tickets.length > 0 ? tickets.join(", ") : "idle",
      running: mate.live_runs > 0,
    };
  });
}
