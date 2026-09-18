// The board model — STUDIO-925.
//
// The Jobs worklist's unit of display is the RUN; the board's is the WORK ITEM. One ticket is one
// card, its reviews are folded onto it as chips, and a column is the ticket's TRACKER STATE. The
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
// `tracker_state` is ABSENT — not blank — when the daemon could not ask the tracker (STUDIO-702),
// and only a review row is expected to be absent in a healthy payload. A non-review row the daemon
// could not resolve still has to appear SOMEWHERE, so it lands in an explicit "state unknown" column
// rather than being renamed after a Linear state the daemon never read.
import type { BlockedEntry } from "@/lib/api";
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
const PR_IDENTIFIER = /^pr:([^/#\s]+)\/([^#\s]+)#(\d+)(?:@(\S+))?$/;

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
  /** The tracker's own workflow-state name — the column this card belongs to. "" when unresolved. */
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
}

/** A column is a tracker state, its cards newest-first within the incoming order. */
export interface BoardColumn {
  /** The tracker state's own name, or `UNKNOWN_STATE`. */
  key: string;
  name: string;
  /** One line saying what the column MEANS, from the status of the cards in it. */
  subtitle: string;
  cards: BoardCard[];
}

/**
 * The column key for a ticket the daemon could not resolve a tracker state for. A sentinel, not "",
 * so it cannot collide with a real (if bizarre) empty workflow-state name, and rendered as an
 * explicit "state unknown" column rather than being renamed after a Linear state nobody read.
 */
export const UNKNOWN_STATE = "\u0000unknown";

/** The order the console knows workflow states in; anything else sorts after, alphabetically. */
const KNOWN_STATE_ORDER: readonly string[] = [
  "backlog",
  "todo",
  "in progress",
  "in review",
  "done",
  "canceled",
  "cancelled",
];

/**
 * What each status means, in one line — the column's subtitle.
 *
 * The ticket's own example is the standard: "an agent is working this in a worktree" says more than
 * "In Progress" does. A column of mixed statuses takes the most active one, so the subtitle always
 * describes the reason to look at that column rather than the average of what is in it.
 */
const SUBTITLE_BY_STATUS: Record<ConsoleJobStatus, string> = {
  run: "an agent is working this in a worktree",
  reviewing: "an agent is reviewing a pull request here",
  blocked: "waiting on a blocker or a person",
  review: "finished work waiting on a reviewer",
  queued: "dispatched next — no agent on it yet",
  done: "merged or closed",
};

// Most-active first: a column holding both a live run and finished work is described by the live run.
const STATUS_PRIORITY: readonly ConsoleJobStatus[] = [
  "run",
  "reviewing",
  "blocked",
  "review",
  "queued",
  "done",
];

/** The column's subtitle: the meaning of the most active status among its cards. */
export function boardColumnSubtitle(statuses: readonly ConsoleJobStatus[]): string {
  for (const status of STATUS_PRIORITY) {
    if (statuses.includes(status)) return SUBTITLE_BY_STATUS[status];
  }
  return "";
}

/** Where a column sorts: known states in workflow order, unknown states alphabetically after. */
export function boardStateRank(state: string): number {
  const known = KNOWN_STATE_ORDER.indexOf(state.trim().toLowerCase());
  return known === -1 ? KNOWN_STATE_ORDER.length : known;
}

/**
 * Regroup the worklist rows into board columns.
 *
 * `rows` is exactly what the table renders (one per issue key, review rows included); `blocked` is
 * the live snapshot's held-dependent set, which is the only dependency edge the state payload
 * carries.
 */
export function buildConsoleBoard(
  rows: readonly ConsoleJobRow[],
  blocked: readonly BlockedEntry[] = [],
): BoardColumn[] {
  const cards: BoardCard[] = [];
  const byIssue = new Map<string, BoardCard>();
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
      outcome: row.statusLabel,
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

  const columns = new Map<string, BoardColumn>();
  for (const card of cards) {
    const state = card.trackerState;
    const key = state === "" ? UNKNOWN_STATE : state;
    let col = columns.get(key);
    if (col === undefined) {
      col = {
        key,
        name: state === "" ? "State unknown" : state,
        subtitle: "",
        cards: [],
      };
      columns.set(key, col);
    }
    col.cards.push(card);
  }

  const out = [...columns.values()];
  for (const col of out) {
    col.subtitle = boardColumnSubtitle(col.cards.map((c) => c.status));
  }
  out.sort((a, b) => {
    const ar = a.key === UNKNOWN_STATE ? Number.MAX_SAFE_INTEGER : boardStateRank(a.name);
    const br = b.key === UNKNOWN_STATE ? Number.MAX_SAFE_INTEGER : boardStateRank(b.name);
    if (ar !== br) return ar - br;
    return a.name.localeCompare(b.name);
  });
  return out;
}
