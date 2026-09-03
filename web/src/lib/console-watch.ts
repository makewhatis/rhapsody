import type { ReviewJob, RunMessage, RunSummary, TeamsRoomMessage } from "@/lib/api";

// console-watch — the model behind the watch-tabs rail under the inspector (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §3C "a thin watch-tabs rail (Diff·dep / Review /
// Room / Memory / Messages)"; slice 4 of its §9 plan).
//
// The rail's job is to put, one click from the trace, the four things about a run that are NOT in
// its transcript: what the room said about the ticket, what the run's teammates retained from it,
// the operator's own line into the agent, and where the work ended up. Three of those are served
// by endpoints the daemon already has. The other two — a run-branch diff (§5, deferred to slice 7)
// and a structured review VERDICT (nothing serves one) — are dependency-named and deep-linked,
// never simulated: §6's rule is "never a dead button", and §5's is "never fake".
//
// Pure functions in a `.ts` module beside `console-trace-view`, for the same reason: every rule the
// rail leans on — which tab is real, which watch rows belong to this run, what a delivery chip says
// — is asserted directly rather than through a render.

/** The rail's tabs, in the order §3C lists them. */
export type WatchTabId = "diff" | "review" | "room" | "memory" | "messages";

export interface WatchTab {
  id: WatchTabId;
  label: string;
  /**
   * Whether the tab's own surface is waiting on something the daemon does not serve. It marks the
   * TAB, not merely the panel, so the operator can see what is missing without opening it.
   */
  dependency: boolean;
}

/**
 * The rail. `Diff` is the one tab whose whole surface is a dependency — §5 defers the run-branch
 * diff endpoint to slice 7. `Review` is not marked, because its reviewer and status ARE served
 * (`GET /api/v1/reviews`); only the findings themselves live on the pull request, which the panel
 * says in its own words rather than by disowning the tab.
 */
export const WATCH_TABS: readonly WatchTab[] = [
  { id: "diff", label: "Diff", dependency: true },
  { id: "review", label: "Review", dependency: false },
  { id: "room", label: "Room", dependency: false },
  { id: "memory", label: "Memory", dependency: false },
  { id: "messages", label: "Messages", dependency: false },
];

/** The tab the rail opens on — the room, which is the one that speaks about every run. */
export const DEFAULT_WATCH_TAB: WatchTabId = "room";

/**
 * The room posts that belong to a ticket, newest first.
 *
 * A post belongs when it REFERENCES the key — in `refs`, which is what proves it, or in the body,
 * which is how a teammate writes it in prose.
 *
 * The prose half is anchored on both sides, because a ticket key is a PREFIX of its own siblings:
 * a bare `body.includes("STUDIO-74")` puts every post about STUDIO-740 through STUDIO-749 under a
 * panel that says it is showing the posts referencing THIS ticket. [`reviewsForRun`] guards the
 * same class on the origin tag, and this is the room's version of it. `-` is part of the boundary
 * so `STUDIO-745` does not match inside `STUDIO-745-2`; the punctuation a teammate actually writes
 * around a key (backticks, brackets, a full stop, a slash in a URL) still matches.
 */
export function roomPostsFor(
  messages: readonly TeamsRoomMessage[],
  issue: string,
): TeamsRoomMessage[] {
  if (issue === "") return [];
  const mention = new RegExp(`(?<![A-Za-z0-9-])${escapeRegExp(issue)}(?![A-Za-z0-9-])`);
  return messages
    .filter((m) => (m.refs ?? []).includes(issue) || mention.test(m.body))
    .slice()
    .reverse();
}

/**
 * The window the Room tab asks the daemon for: its own hard ceiling.
 *
 * `GET /api/v1/teams/room?limit=` can only NARROW — `effective_limit` clamps every caller to
 * `MAX_ROOM_WINDOW` (`crates/config/src/room.rs`) and falls back to `DEFAULT_ROOM_WINDOW = 20`
 * when the caller names none. Asking for the ceiling is therefore the widest read available to any
 * client, and it is still a WINDOW — which is what [`roomEmptyNote`] exists to say out loud.
 */
export const ROOM_WATCH_WINDOW = 50;

/**
 * What the Room tab may state when no post it read mentions this ticket.
 *
 * The room is read newest-first and bounded, THEN filtered to the ticket, so an empty panel has
 * two quite different causes and only one of them is "the room is silent about this ticket". When
 * the read came back full, everything older than the window went unfetched, and a bare "no room
 * posts reference this ticket" is a claim about posts the console never saw — the same defect as
 * reporting a pending or failed read as an empty one, arriving on the read that SUCCEEDS.
 *
 * `read` is how many posts came back, which separates the two cases without a total the endpoint
 * does not serve: short of the window means the whole room fitted inside it and the plain absence
 * is true; at the window — or, defensively, past it — the sentence names what was read instead.
 */
export function roomEmptyNote(read: number): string {
  return read >= ROOM_WATCH_WINDOW
    ? `No post in the room's most recent ${ROOM_WATCH_WINDOW} mentions this ticket.`
    : "No post in the room mentions this ticket.";
}

/**
 * What the Memory tab may state when no fact it read is stamped with this ticket.
 *
 * Unconditional, unlike [`roomEmptyNote`], because recall gives the console nothing to condition
 * on. An empty-query browse is bounded by `recall_top_k` — "browse widens what MATCHES, never how
 * much comes back" (`crates/orchestrator/src/teamsmemory.rs`), unset meaning `FALLBACK_TOP_K = 8`
 * (`crates/config/src/memory.rs`) — and neither the request nor `TeamsRecallResponse` carries
 * that bound, so a bank returning N facts is indistinguishable from a bank truncated AT N. Since
 * no read here is one the tab may call complete, it names the window every time rather than
 * sometimes overclaiming.
 */
export const MEMORY_EMPTY_NOTE =
  "No fact in the newest records the daemon returns from each teammate's bank is stamped with " +
  "this ticket.";

/** Every regex metacharacter neutralised — a tracker key is not guaranteed to carry none. */
function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** The `pr:` prefix a ticketless review run's key carries (`crates/orchestrator/src/review.rs`). */
const REVIEW_KEY_PREFIX = "pr:";

/**
 * The pull request a review run is OF — `owner/repo#number` — or "" for any other run.
 *
 * A ticketless review run's key is `pr:owner/repo#<n>@<reviewer>`
 * (`crates/orchestrator/src/review.rs`), so the coordinate is carried by the run's own identifier.
 * The reviewer suffix is dropped: the tab lists every reviewer of the pull request, not only the
 * one this attempt wore, because a two-reviewer round is one round.
 */
export function reviewRunPr(identifier: string): string {
  const key = identifier.trim();
  if (!key.startsWith(REVIEW_KEY_PREFIX)) return "";
  const coord = key.slice(REVIEW_KEY_PREFIX.length);
  // `lastIndexOf`, because a repository name cannot contain `@` but a reviewer identity is the
  // last segment either way; -1 means the key carries no reviewer, and the whole rest is the
  // coordinate.
  const at = coord.lastIndexOf("@");
  return (at < 0 ? coord : coord.slice(0, at)).trim();
}

/**
 * The ticket a watch row's `introduced_by` names, or "" when it names none.
 *
 * The origin is written as `<origin>:<identifier>` — `handoff:STUDIO-720`
 * (`crates/orchestrator/src/reviewintro.rs`, `REVIEW_ORIGIN_HANDOFF`). It is parsed on the
 * SEPARATOR rather than matched against the literal prefix so that `console:` — the sibling
 * constant declared beside it, which nothing writes yet — reads the same way the day it does.
 */
export function originTicket(introducedBy: string): string {
  const at = introducedBy.indexOf(":");
  return at < 0 ? "" : introducedBy.slice(at + 1).trim();
}

/** `owner/repo#number` — the form the daemon, the room and the design record all use. */
function prCoord(job: ReviewJob): string {
  return `${job.owner}/${job.repo}#${job.number}`;
}

/**
 * The watch-set rows that belong to a run, in the daemon's own order.
 *
 * Two ways in, because a run reaches the review watch set from either side of it. A run that IS a
 * review is keyed by the pull request, so its rows are that pull request's. Every other run is an
 * AUTHOR's, and the only link the daemon serves back to it is the origin tag its own hand-off
 * wrote (`handoff:<identifier>`) — there is no run id and no branch on a watch row.
 *
 * That origin match is per-TICKET rather than per-run, which is the honest limit here: two attempts
 * of one ticket that each handed off resolve to the same rows. Naming that is better than the
 * alternative of showing nothing, and the rail says which pull request each row is on.
 */
export function reviewsForRun(jobs: readonly ReviewJob[], run: RunSummary): ReviewJob[] {
  const pr = reviewRunPr(run.issue_identifier);
  if (pr !== "") return jobs.filter((job) => prCoord(job) === pr);
  const issue = run.issue_identifier.trim();
  if (issue === "") return [];
  return jobs.filter((job) => originTicket(job.introduced_by) === issue);
}

/** What a message's delivery status reads as beside it (§3C's sent→delivered chip). */
export interface MessageChip {
  /** The chip's own class, which is also its tone. */
  tone: "sent" | "delivered" | "expired" | "unknown";
  label: string;
}

/**
 * The chip one operator message carries.
 *
 * `delivered` names the TURN it landed on when the daemon recorded one, because "delivered" alone
 * does not tell an operator whether the agent had already passed the step they were writing about.
 * `expired` says what happened in words — a status an operator has never seen before reads as a
 * silent failure otherwise.
 *
 * A status this build has not heard of is shown verbatim rather than rounded to one it knows: the
 * three spellings are `crates/store/src/types.rs`'s `RUN_MESSAGE_*`, and a fourth added there and
 * not here should be visible, not disguised.
 */
export function messageChip(message: RunMessage): MessageChip {
  switch (message.status) {
    case "sent":
      return { tone: "sent", label: "sent" };
    case "delivered":
      return {
        tone: "delivered",
        label:
          message.delivered_turn === undefined
            ? "delivered"
            : `delivered · turn ${message.delivered_turn}`,
      };
    case "expired":
      return { tone: "expired", label: "expired — the run ended first" };
    default:
      return { tone: "unknown", label: message.status || "unknown" };
  }
}

/**
 * The refs an "Ask about this run" post carries (§6, "posts to the room refed to the run today").
 *
 * Both coordinates, because they answer different questions: the ticket is what a teammate reading
 * the room greps for, and the run is what makes the question about THIS attempt rather than the
 * ticket in general. A run whose row carries no identifier still refs the run.
 */
export function askRefs(run: RunSummary): string[] {
  const issue = run.issue_identifier.trim();
  const refs = [`run ${run.id}`];
  return issue === "" ? refs : [issue, ...refs];
}
