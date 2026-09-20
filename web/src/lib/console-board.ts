// The board model — STUDIO-925.
//
// The Jobs worklist's unit of display is the RUN; the board's is the WORK ITEM. One ticket is one
// card, its reviews are folded onto it as chips, and a lane is the ticket's RUN STATUS. The
// regroup needs nothing the console does not already hold: `GET /api/v1/history/issues` serves
// `tracker_state`, `review_run`, `review_of`, `provider` and `assignee` per row (verified against a
// live daemon on 2026-09-18), and `mergeJobs` has already collapsed the live snapshot into one row
// per issue key. So this module is PURE presentation over `ConsoleJobRow[]` — no endpoint, no crate,
// no parity surface.
//
// WHY A REVIEW ROW IS NEVER A CARD. A ticketless review run lives under its own
// `pr:owner/repo#n@reviewer` issue key, so the issue listing genuinely contains it as a row — the
// reason `STUDIO-924` used to read as three jobs. `review_run` is the daemon's own marker for
// exactly that row (STUDIO-826), and the card set is every row WITHOUT it. The review rows are then
// attached to the ticket they name in `review_of`. A review row whose origin did not resolve to a
// ticket is dropped rather than carded: it is not work, and there is no card for it to belong to.
//
// WHY THE LANE IS RUN STATUS, NOT TRACKER STATE (STUDIO-930). STUDIO-925 columned on `tracker_state`
// and designed a capacity rack for an "In Progress" column — but the daemon never sets In Progress.
// It dispatches straight out of `Todo` and moves to `In Review` at the end, so a running ticket sat
// in the Todo lane. The lifecycle the board can actually see is Queued → Running → In Review → Done:
// the tracker decides only DONE (terminal states), and the run status decides the rest.
//
// THE LANE SET IS FIXED. Lanes are never built from the cards present: the board is quietest exactly
// when the pipeline is idle or starved, and an absent lane would hide the very condition the console
// most needs to shout about. All four always exist, empty ones included.
import type { BlockedEntry, HeldForHuman } from "@/lib/api";
import { runOutcomeLabel } from "@/lib/console-job-detail";
import type { ConsoleJobRow, ConsoleJobStatus } from "@/lib/console-jobs";

/**
 * One pull request parsed from a review row's `pr:<owner>/<repo>#<n>@<reviewer>` issue key.
 *
 * The key IS the only place the console can get this today — the Jobs table's PR column has rendered
 * "—" on every row since STUDIO-683 because no endpoint serves a ticket's PR, and the number has
 * been sitting in the review row's identifier the whole time. Parsing it here is deliberately a
 * CLIENT-side display concern: `review_run`/`review_of` stay the daemon's fields, and the console
 * never infers a review from this string (the docs on `IssueRun` are explicit that the key and the
 * title are conventions, not facts).
 */
export interface PullRequestRef {
  owner: string;
  repo: string;
  number: number;
  /** The reviewer named after `@`, or "" when the key carried none. */
  reviewer: string;
  url: string;
}

// `pr:` then owner then repo then `#n`, optionally `@reviewer`. A plain ticket key — "STUDIO-925" —
// never matches, which is the whole guard: the parser is only ever answering "is this a review key".
const PR_IDENTIFIER = /^pr:([^/#\s]+)\/([^/#\s]+)#(\d+)(?:@(\S+))?$/;

/** Parse a review run's issue key, or `undefined` when it is not one. */
export function parsePullRequest(identifier: string): PullRequestRef | undefined {
  const m = PR_IDENTIFIER.exec(identifier);
  if (m === null) return undefined;
  const number = Number(m[3]);
  if (!Number.isInteger(number) || number <= 0) return undefined;
  return {
    owner: m[1],
    repo: m[2],
    number,
    reviewer: m[4] ?? "",
    url: `https://github.com/${m[1]}/${m[2]}/pull/${m[3]}`,
  };
}

/** The short form the table's PR chip shows (`#540`). */
export function pullRequestLabel(pr: PullRequestRef): string {
  return `#${pr.number}`;
}

/** One reviewer's review of a card, folded from a `review_run` row. */
export interface ReviewerChip {
  /** Stable React key — the review row's own issue key. */
  key: string;
  /** The reviewer, from the review key's `@name`, else the row's durable assignee. */
  reviewer: string;
  /** The reviewer's RUN status, which is what colours the chip: `done` green, `failed` red, … */
  status: ConsoleJobStatus;
  /** The run's own outcome, verbatim ("done", "running", "failed") — the chip's word. */
  outcome: string;
  /** The pull request this review is on, when its key names one. */
  pr: PullRequestRef | undefined;
}

/** One ticket's card — the board's unit of display. */
export interface BoardCard {
  /** Stable React key (the row's own, so a live and a history row cannot collide). */
  key: string;
  /** Ticket key — the card's title and the `job/:key` route target. */
  issue: string;
  title: string;
  /** Project display name, or "—". */
  project: string;
  /** Raw project slug — the project Select's value. */
  projectSlug: string;
  status: ConsoleJobStatus;
  statusLabel: string;
  /** The tracker's own workflow-state name — it decides only the Done lane. "" when unresolved. */
  trackerState: string;
  assignee: string;
  provider: string;
  /** True while this ticket's newest run is genuinely in flight. */
  live: boolean;
  /** The pull request the ticket's reviews are on, newest review row first. */
  pr: PullRequestRef | undefined;
  /** One chip per review row the daemon attributed to this ticket, in the rows' own order. */
  reviewers: ReviewerChip[];
  /** Blockers holding this ticket, each "X · State" (`state.blocked`, INF-318/INF-320). */
  dependencies: string[];
  /**
   * True when the dispatcher is deliberately holding this ticket for a person (`rhapsody:human`,
   * STUDIO-949). Distinct from `dependencies`: nobody is blocking it and no agent will ever run it —
   * it is console/legal/physical work, so the board must read it as held, not as mysteriously idle.
   */
  heldForHuman: boolean;
}

/** The four lanes, left to right — the order a ticket travels them. */
export type BoardLaneId = "queued" | "running" | "review" | "done";

/** One lane: a fixed slot in the board, its cards newest-first within the incoming order. */
export interface BoardLane {
  id: BoardLaneId;
  name: string;
  /** One line saying what the lane MEANS. Fixed per lane, so it is true of every card in it. */
  caption: string;
  /** What an empty lane says about the pipeline (an empty lane is information, not absence). */
  empty: string;
  cards: BoardCard[];
}

const LANES: readonly Omit<BoardLane, "cards">[] = [
  {
    id: "queued",
    name: "Queued",
    caption: "waiting for an agent, or held by a blocker",
    empty: "Nothing is waiting for an agent.",
  },
  {
    id: "running",
    name: "Running",
    caption: "an agent has the ticket right now",
    empty: "No agent is running.",
  },
  {
    id: "review",
    name: "In Review",
    caption: "finished work waiting on a reviewer",
    empty: "Nothing is parked for review.",
  },
  {
    id: "done",
    name: "Done",
    caption: "merged or closed",
    empty: "Nothing has finished yet.",
  },
];

/** What a lane says when the filter above the board, not the pipeline, emptied it. */
export const FILTERED_LANE_EMPTY = "No tickets here match the filter.";

/** An empty lane on a truncated page: older jobs are not loaded, so it cannot claim the lane is empty. */
export const TRUNCATED_LANE_EMPTY = "Nothing in the jobs loaded so far; older jobs are not loaded yet.";

/**
 * The run outcomes the board's non-terminal lanes fetch, by `latest_outcome` — an issue is returned
 * only when its NEWEST run carries the outcome, so the fetch is bounded by the pipeline rather than
 * by history (STUDIO-931).
 *
 * WHY THE BOARD CANNOT BUCKET A PAGE. The board used to regroup the same 50-row recency listing the
 * table holds, and completed work dominates recent activity, so a non-terminal ticket aged off the
 * page the longer it sat: on the operator's own daemon STUDIO-877 (open, `stopped`) sat at row 153
 * and its Queued lane read `0` while the header, counting the whole store, read `1`. The lane was
 * least informative about exactly the ticket that had waited longest.
 *
 * WHY `latest_outcome`, NOT `outcome`. `?outcome=X` filters each run before the per-issue partition,
 * so it returns every ticket that has ever had an X run AS that old run: on the operator's daemon 6 of
 * 7 `?outcome=stopped` rows were finished tickets whose latest run had completed, and they piled into
 * the board as stale cards. `?latest_outcome=X` filters after the partition (see the README's
 * Divergences), so only the tickets sitting in X right now come back.
 *
 * WHY `completed` IS ABSENT. It is the outcome an In-Review ticket's last run carries AND the one a
 * Done ticket's last run carries, and the only server-side fact that tells the two apart is the
 * ticket's lifecycle — a post-query decoration, not a filter (trap 1 on the ticket). Fetching every
 * ticket whose latest run completed would pull the whole Done set to find the handful in review. So
 * In Review takes its COUNT from the whole-store tally (see `boardLaneTally`) and its cards from the
 * loaded page, and the lane reports a number it can stand behind either way.
 */
export const BOARD_ACTIVE_OUTCOMES = [
  "running",
  "continued",
  "stopped",
  "failed",
  "interrupted",
] as const;

/**
 * The page the board's active fetch asks for. Because the fetch is filtered to each issue's NEWEST
 * run, its result is bounded by the pipeline rather than by history, so a page this wide is complete
 * in practice. It is a ceiling rather than a promise of unboundedness, and the lane reports its
 * TALLY rather than its card count, so a store that ever exceeded it would read as a lane count with
 * fewer cards — never as a lie.
 */
export const BOARD_ACTIVE_LIMIT = 1000;

/**
 * A lane's whole-store total, from the daemon's per-status tally. `undefined` for the Done lane,
 * whose terminal set is deliberately paged rather than counted, and for a board with no tally yet —
 * the callers render the card count in its place.
 *
 * The tally is the SAME source the Now strip paints, which is what stops the board contradicting
 * the header on one screen. A `blocked` card waits in Queued (see [`boardLaneOf`]), so that lane's
 * total is both buckets together.
 */
export function boardLaneTally(
  laneId: BoardLaneId,
  counts: { running: number; review: number; queued: number; blocked: number },
): number | undefined {
  switch (laneId) {
    case "running":
      return counts.running;
    case "review":
      return counts.review;
    case "queued":
      return counts.queued + counts.blocked;
    case "done":
      return undefined;
  }
}

/**
 * The board's issue rows: the paged listing plus the wide, latest-run-outcome active fetch, one row
 * per issue. The PAGE wins a collision because it is the fresher read — it polls on the live cadence
 * while the active fetch rides the tracker's own, slower one.
 */
export function mergeIssueRows<T extends { issue_identifier: string; id: number }>(
  page: readonly T[],
  active: readonly T[],
): T[] {
  const seen = new Set<string>();
  const out: T[] = [];
  for (const r of [...page, ...active]) {
    // The same partition key `Store::list_issue_runs` uses: an unattributed run has no ticket to
    // group under, so it stays individual rather than collapsing into one row.
    const key = r.issue_identifier === "" ? `run:${r.id}` : r.issue_identifier;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(r);
  }
  return out;
}

// Tracker states the daemon never moves a ticket out of again.
const TERMINAL_STATES: readonly string[] = ["done", "canceled", "cancelled"];

/**
 * The lane a card belongs in. The tracker decides DONE; otherwise the run status does, so a `Todo`
 * ticket with a live run lands in Running. A `blocked` card (a failed run, or a held dependent) is
 * not moving and not finished: it waits in Queued, still wearing its blocked pill and blocker chip.
 */
export function boardLaneOf(card: Pick<BoardCard, "status" | "live" | "trackerState">): BoardLaneId {
  if (TERMINAL_STATES.includes(card.trackerState.trim().toLowerCase())) return "done";
  switch (card.status) {
    case "run":
    case "reviewing":
      return "running";
    case "review":
      return "review";
    case "done":
      return "done";
    default:
      // A live run outranks a stale status word: an agent on the ticket IS running.
      return card.live ? "running" : "queued";
  }
}

/**
 * Regroup the worklist rows into the board's four lanes — always all four, in order.
 *
 * `rows` is exactly what the table renders (one per issue key, review rows included); `blocked` is
 * the live snapshot's held-dependent set, which is the only dependency edge the state payload
 * carries. `heldForHuman` is the live snapshot's `rhapsody:human` hold set (STUDIO-949): it both
 * flags a card the rows already carry and synthesizes a Queued card for a hold that has never run.
 */
export function buildConsoleBoard(
  rows: readonly ConsoleJobRow[],
  blocked: readonly BlockedEntry[] = [],
  heldForHuman: readonly HeldForHuman[] = [],
): BoardLane[] {
  const cards: BoardCard[] = [];
  const byIssue = new Map<string, BoardCard>();
  const held = new Set(heldForHuman.map((h) => h.issue_identifier));
  // The hold entry carries only the project SLUG (the daemon's `HeldForHuman`), while a card's
  // `project` is the display NAME. Recover the name from any row of the same project so a
  // synthesized card's chip matches every other card; fall back to the slug when the project has no
  // row at all. (Through `JobsView` this branch is unreachable — `mergeJobs` synthesizes a row per
  // hold and `buildConsoleJobs` maps 1:1 — so this only matters to a caller that hands the board
  // rows it did not build through that chain.)
  const nameBySlug = new Map<string, string>();
  for (const row of rows) {
    if (row.projectSlug !== "" && row.project !== "") nameBySlug.set(row.projectSlug, row.project);
  }
  for (const row of rows) {
    // A review run is never its own card: it belongs to the ticket it reviews, and an unattributed
    // run (no issue key) has no ticket to group under.
    if (row.reviewRun || row.issue === "") continue;
    const card: BoardCard = {
      key: row.key,
      issue: row.issue,
      title: row.title,
      project: row.project,
      projectSlug: row.projectSlug,
      status: row.status,
      statusLabel: row.statusLabel,
      trackerState: row.trackerState,
      assignee: row.assignee,
      provider: row.provider,
      live: row.live,
      pr: undefined,
      reviewers: [],
      dependencies: [],
      heldForHuman: held.has(row.issue),
    };
    cards.push(card);
    byIssue.set(row.issue, card);
  }

  for (const row of rows) {
    if (!row.reviewRun) continue;
    const card = row.reviewOf === "" ? undefined : byIssue.get(row.reviewOf);
    if (card === undefined) continue;
    const pr = parsePullRequest(row.issue);
    card.reviewers.push({
      key: row.key,
      reviewer: pr?.reviewer || row.assignee || "unknown",
      status: row.status,
      // The row's OWN run outcome, not the ticket status' label: a failed review maps to `blocked`,
      // and the chip's word must say how the run ended, not that a person is now needed (STUDIO-925).
      outcome: runOutcomeLabel(row.runOutcome),
      pr,
    });
    // Newest review row first (the rows arrive newest-first), so the card's PR link is the most
    // recent one — the pull request the ticket is actually parked on.
    if (card.pr === undefined && pr !== undefined) card.pr = pr;
  }

  const heldBy = new Map<string, string[]>();
  for (const b of blocked) {
    if (b.issue_identifier === "") continue;
    const list = heldBy.get(b.issue_identifier) ?? [];
    list.push(`${b.blocker_identifier} · ${b.blocker_state}`);
    heldBy.set(b.issue_identifier, list);
  }
  for (const card of cards) card.dependencies = heldBy.get(card.issue) ?? [];

  // A held-for-human ticket has usually NEVER RUN, so it has no worklist row at all — rows come from
  // run history plus the live snapshot's running/retrying/blocked sets, and a ticket the dispatcher
  // refuses never reaches any of them. Annotating an existing row would therefore leave the hold
  // invisible on the board: the exact silent stall the hold exists to end (STUDIO-949). Synthesize a
  // Queued card for any hold the rows did not already surface, as `mergeJobs` synthesizes a held
  // dependent's row. The chip is the deliberate-hold marker; the pill stays the lane's own word.
  //
  // LOAD-BEARING NOTE: through `JobsView` this loop is unreachable. `mergeJobs` synthesizes a row
  // for every hold (`runs-model.ts`) and `buildConsoleJobs` is a 1:1 map, so `byIssue.has(...)` is
  // always true by the time this runs — a held ticket is surfaced by the ROW, and its status word is
  // decided in `consoleJobStatus`. This branch is belt-and-braces for a caller that hands the board
  // rows it did not build through that chain, and its own test is the only thing that exercises it.
  // Do not delete the `mergeJobs` half believing this one covers it.
  for (const h of heldForHuman) {
    if (h.issue_identifier === "" || byIssue.has(h.issue_identifier)) continue;
    const card: BoardCard = {
      key: `held-${h.issue_identifier}`,
      issue: h.issue_identifier,
      title: h.title,
      project: nameBySlug.get(h.project) ?? h.project,
      projectSlug: h.project,
      status: "queued",
      statusLabel: "queued",
      trackerState: "",
      assignee: "",
      provider: "",
      live: false,
      pr: undefined,
      reviewers: [],
      dependencies: [],
      heldForHuman: true,
    };
    cards.push(card);
    byIssue.set(h.issue_identifier, card);
  }

  const lanes: BoardLane[] = LANES.map((lane) => ({ ...lane, cards: [] }));
  for (const card of cards) {
    const id = boardLaneOf(card);
    lanes.find((lane) => lane.id === id)?.cards.push(card);
  }
  return lanes;
}
