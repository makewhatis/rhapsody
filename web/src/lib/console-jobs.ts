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
// not serve. There is therefore no assignee and no PR link per ticket. What is used instead,
// and what it costs:
//   - Status  — the TICKET's lifecycle when the daemon resolved one (STUDIO-702), else the
//               daemon's own job status. See `consoleJobStatus`.
//   - Assignee— the DURABLE assignee the daemon resolves per history row (STUDIO-735), falling
//               back to the Teams roster's LIVE tickets (`GET /api/v1/teams`) only for a row that
//               has none — a run that started before its routing record landed. See
//               `durableAssignees`.
//   - PR      — no endpoint carries one; the column renders "—" until one does.
import type { IssueLifecycle, IssueRun, RunSummary, TeamsOverview } from "@/lib/api";
import type { JobRow } from "@/lib/runs-model";

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
 * Maps a run OUTCOME onto the console's vocabulary — the answer used when the daemon could not
 * resolve the ticket's real state.
 *
 * `completed → review` is the pipeline's own rule: a run that finishes cleanly hands its ticket
 * to the configured review state, so a just-completed run means "waiting on a reviewer".
 * `failed` and `waiting` both mean a human has to act, which is what `blocked` says; `stopped`
 * leaves the ticket idle awaiting its next dispatch, which is `queued`.
 *
 * Takes a plain string rather than `JobStatusKey`: `JobRow.status` is typed as the wider
 * `StatusKey`, and narrowing it with a cast would hide exactly the case the default arm is
 * here to survive.
 */
function fromRunOutcome(status: string): ConsoleJobStatus {
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

/**
 * The row's status: the TICKET's lifecycle when the daemon resolved one, else the run outcome.
 *
 * The ticket's state is the truer signal and outranks the outcome, because an outcome never
 * expires. Every completed run used to read "in review" for as long as the store kept it, so the
 * count grew monotonically with history and `done` was unreachable — STUDIO-702.
 *
 * Two rules are not simply "lifecycle wins", and both are deliberate:
 *   - A LIVE run outranks the ticket. A mid-run handoff parks the ticket in a review state while
 *     the agent is still working, and the worklist must keep saying "running".
 *   - An `open` ticket keeps a `failed`/`waiting` outcome's `blocked`. Those describe the RUN, and
 *     a human still has to act on them; what `open` does override is `completed → review`, since a
 *     ticket that went back to open work is not awaiting a reviewer.
 *
 * An absent or unrecognized `lifecycle` falls back to the outcome mapping unchanged, so a console
 * talking to a daemon that predates the field behaves exactly as it did before.
 */
export function consoleJobStatus(status: string, lifecycle?: string): ConsoleJobStatus {
  const fromRun = fromRunOutcome(status);
  if (fromRun === "run") return "run";
  switch (lifecycle) {
    case "done":
    case "canceled":
      return "done";
    case "in_review":
      return "review";
    case "open":
      return fromRun === "review" ? "queued" : fromRun;
    default:
      return fromRun;
  }
}

/**
 * Whether this ticket is waiting on the OPERATOR — the "Needs you" count the design record's §6
 * adds to the Now strip. Derived from state the worklist already holds; no new endpoint.
 *
 * WHAT IT CLAIMS, EXACTLY. A ticket parked in review awaits a person's verdict or merge, and a
 * failed run awaits a person's decision about what happens next. That is the whole claim. On a
 * healthy tracker it makes "needs you" and "in review" close neighbours — the failed runs are the
 * difference — and that convergence is the honest answer rather than a defect: if the tracker says
 * twelve tickets are parked for review, twelve tickets really are waiting on a human. What is NOT
 * claimed is any sharper discrimination among them; the facts that would allow one are named at
 * the bottom of this comment and none of them is served to this view.
 *
 * `blocked` qualifies only when it is a FAILED run. The other thing that reads blocked is a held
 * dependent (`runs-model`'s synthetic `waiting` row), and that one waits on its predecessor rather
 * than on the operator. If the predecessor needs a human it is counted on its OWN row; counting
 * the dependent too would bill one decision twice.
 *
 * WHY IT DOES NOT SPLIT ON A LIFECYCLE'S PRESENCE, which is the mistake worth keeping written
 * down. An earlier shape counted a review row only when `lifecycle === "in_review"` came back,
 * reading an ABSENT lifecycle as "inferred from a stale outcome, so nobody is waiting". But
 * `StateProvider::issue_lifecycles` (crates/httpapi/src/server.rs) answers off a TTL cache and the
 * reads cell's tracker AT REQUEST TIME, and a missing tracker, a failed round-trip and an unknown
 * id are all *no answer*. Absence is therefore a LIVENESS condition of the daemon, not a property
 * of the ticket — the same ticket answers on a warm cache and does not answer on a cold one. The
 * rows that split were simply uncached, and once they warmed the split became a silent no-op.
 *
 * That liveness question is real, but it belongs to the PAYLOAD rather than to a row, and
 * [`consoleJobCounts`] is where it is answered.
 *
 * WHAT WOULD MAKE IT SHARPER, flagged rather than guessed at (§9/§11). "Needs you" ought to mean
 * "your merge is the next move", and the two facts that would say so are not served here: no
 * endpoint carries a ticket's PR or its checks (the PR column renders "—" for the same reason),
 * and no per-ticket record says whether a REVIEWER — human or agent — already holds it. A
 * threshold guessed from a timestamp would look like a narrowing without being one, so until a
 * supporting endpoint lands this stays the coarse claim it can actually defend.
 *
 * Takes the run status as a plain string for the reason `fromRunOutcome` does: `JobRow.status` is
 * the wider `StatusKey`, and narrowing it with a cast would hide the case this has to survive.
 */
export function needsOperator(status: ConsoleJobStatus, runStatus: string): boolean {
  if (status === "review") return true;
  return status === "blocked" && runStatus !== "waiting";
}

/** One row of the §3 worklist, fully derived so the table stays presentational. */
export interface ConsoleJobRow {
  /** Stable React key. */
  key: string;
  /** Ticket key — also the `job/:key` route target (§10 box 2.8). */
  issue: string;
  /** The run the row's trace-sparkline previews; 0 when persistence is off (there is none). */
  runId: number;
  /** True while this ticket's newest run is genuinely in flight — the sparkline's playhead. */
  live: boolean;
  title: string;
  /** Project display name, or "—" when the daemon runs single-project. */
  project: string;
  /** Raw project slug — the project Select's value. */
  projectSlug: string;
  status: ConsoleJobStatus;
  statusLabel: string;
  /** The tracker's own workflow-state name behind `status`, or "" when the daemon had no answer. */
  trackerState: string;
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
  /** Whether the ticket's next move is the OPERATOR's — the Now strip's "Needs you" (§6). */
  needsYou: boolean;
  /**
   * Whether the daemon actually answered a tracker lifecycle for this ticket on THIS request.
   *
   * Not a property of the ticket — `issue_lifecycles` resolves per request off a TTL cache, so
   * this says only "the tracker spoke for this row just now". It exists so [`consoleJobCounts`]
   * can tell a healthy payload from the stripped one a cold cache serves; see [`needsOperator`].
   */
  lifecycleResolved: boolean;
}

/**
 * Ticket key → teammate name, from the roster's LIVE tickets. Only a RUNNING ticket resolves: the
 * roster lists what each teammate is working on now, so a ticket drops out of it the moment its run
 * ends. That is exactly why it is the fallback rather than the source — see `durableAssignees`.
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

/** One ticket's resolved state: the normalized bucket plus the tracker's own name for it. */
export interface TicketLifecycle {
  lifecycle: IssueLifecycle;
  trackerState: string;
}

/**
 * Ticket key -> resolved lifecycle, from the issue-level listing's per-row fields (STUDIO-702).
 *
 * A row the daemon could not resolve carries neither field and is SKIPPED rather than mapped to a
 * default — an absent key is what makes `consoleJobStatus` fall back to the run outcome, so a
 * placeholder here would silently defeat the fallback. The listing is one row per issue, but the
 * first answer wins if that ever stops being true.
 */
export function lifecycleByIssue(rows: readonly IssueRun[]): Map<string, TicketLifecycle> {
  const byIssue = new Map<string, TicketLifecycle>();
  for (const r of rows) {
    if (r.issue_identifier === "" || r.lifecycle === undefined || byIssue.has(r.issue_identifier)) {
      continue;
    }
    byIssue.set(r.issue_identifier, {
      lifecycle: r.lifecycle,
      trackerState: r.tracker_state ?? "",
    });
  }
  return byIssue;
}

/**
 * Ticket key → teammate name, from the issue-level listing's own `assignee` field (STUDIO-735).
 *
 * This is the historical record — who the run was dispatched under — so unlike `ticketAssignees` it
 * still answers once the job has moved to in review or done, which is the whole point. A row
 * carrying no `assignee` is SKIPPED rather than mapped to "", so an absent key is what makes
 * `buildConsoleJobs` consult the live roster; a placeholder here would defeat that fallback exactly
 * as it would for `lifecycleByIssue`. The listing is one row per issue, but the first answer wins if
 * that ever stops being true.
 */
export function durableAssignees(rows: readonly IssueRun[]): Map<string, string> {
  const byIssue = new Map<string, string>();
  for (const r of rows) {
    if (r.issue_identifier === "" || !r.assignee || byIssue.has(r.issue_identifier)) continue;
    byIssue.set(r.issue_identifier, r.assignee);
  }
  return byIssue;
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
  issueRows: readonly IssueRun[],
  overview: TeamsOverview | undefined,
  nowMs: number,
): ConsoleJobRow[] {
  const durable = durableAssignees(issueRows);
  const live = ticketAssignees(overview);
  const activity = lastActivityByIssue(issueRows);
  const lifecycles = lifecycleByIssue(issueRows);

  const out = jobs.map((job): ConsoleJobRow => {
    const ticket = lifecycles.get(job.issue);
    const status = consoleJobStatus(job.status, ticket?.lifecycle);
    const updatedAtMs = activity.get(job.issue) ?? job.startedAtMs;
    return {
      key: job.key,
      issue: job.issue,
      runId: job.runId,
      live: job.live,
      title: job.title,
      project: job.projectShort,
      projectSlug: job.project,
      status,
      statusLabel: CONSOLE_STATUS_LABELS[status],
      trackerState: ticket?.trackerState ?? "",
      // The durable record first: it is the only one that survives the run. The live roster is the
      // fallback for the gap at the other end — a run dispatched moments ago, whose history row the
      // daemon has not yet decorated.
      assignee: durable.get(job.issue) ?? live.get(job.issue) ?? "",
      pr: "",
      updated: relativeSince(updatedAtMs, nowMs),
      updatedAtMs,
      subLabel: job.subLabel,
      needsYou: needsOperator(status, job.status),
      lifecycleResolved: ticket !== undefined,
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

/**
 * The Now-strip stat pills: §3's original four, plus the "Needs you" the design record's §6 adds.
 *
 * `needsYou` deliberately CUTS ACROSS the other four rather than partitioning with them — see
 * [`needsOperator`] — so the five numbers do not sum to the row count and are not meant to.
 */
export interface ConsoleJobCounts {
  running: number;
  review: number;
  queued: number;
  blocked: number;
  /**
   * How many tickets are waiting on the operator, or `null` for "the console cannot tell" — which
   * the Now strip renders as "—" rather than as a number.
   *
   * WHY THIS ONE STAT IS NULLABLE AND THE OTHER FOUR ARE NOT. The four above are counts of rows
   * the daemon definitely served. This one is a claim about the OUTSIDE world — what a human still
   * owes — and it is only answerable while the tracker is answering. When `issue_lifecycles`
   * resolves nothing for the payload (a cold cache, a missing tracker, a Linear round-trip that
   * failed), `consoleJobStatus` falls back to inferring "in review" from every `completed` outcome,
   * so the review pill INFLATES at the exact moment the console knows least. Reporting a number
   * derived from that would be at best a coincidence and at worst — measured on the live listing —
   * "350 in review, 0 need you", the one stat whose job is to say what is waiting announcing that
   * nothing is, precisely when it cannot see. A zero is a claim; "—" is the truth.
   *
   * The gate is all-or-nothing over the payload on purpose: the daemon resolves lifecycles for a
   * request as a batch, so a page where NOT ONE row got an answer is the outage shape, while a
   * page where some did is a healthy tracker that merely does not know every ticket. An empty
   * worklist counts as knowable — nothing is waiting because there is nothing.
   */
  needsYou: number | null;
}

export function consoleJobCounts(rows: readonly ConsoleJobRow[]): ConsoleJobCounts {
  const counts = { running: 0, review: 0, queued: 0, blocked: 0 };
  let needsYou = 0;
  let heard = false;
  for (const row of rows) {
    if (row.status === "run") counts.running += 1;
    else if (row.status === "review") counts.review += 1;
    else if (row.status === "queued") counts.queued += 1;
    else if (row.status === "blocked") counts.blocked += 1;
    if (row.needsYou) needsYou += 1;
    if (row.lifecycleResolved) heard = true;
  }
  return { ...counts, needsYou: heard || rows.length === 0 ? needsYou : null };
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
