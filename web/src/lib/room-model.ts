import type { TeamsRoomMessage } from "@/lib/api";
import { fenceSpans, isClosingFence } from "@/lib/markdown";

// room-model — the pure logic behind the Teams console's room (STUDIO-681 §5, built by
// STUDIO-684): what kind of event a post is, which teammates it concerns, how a day of them
// reads, and which runs of them collapse.
//
// Separate from the components, which is this codebase's discipline for anything worth asserting
// on directly (see teams-model, runs-model, settings-model). It matters more than usual here: the
// room log carries NO `kind` field — a message is `{id, from, to, at, body, refs}` and nothing
// else — so every typed rendering in §5 rests on classifying the daemon's own post bodies. Those
// bodies are written in `crates/orchestrator/src/triage.rs` and `quorum.rs`; the rules below name
// the exact call site each pattern comes from, because a body reworded there silently demotes an
// event to the muted default here.

/** The five kinds §5 renders with their own rail color, icon and label. */
export type RoomKind = "operator" | "handoff" | "assign" | "reconcile" | "quorum";

/** The filter chips across the top of the room. */
export type RoomFilter = "all" | "conversation" | "handoff" | "assign" | "quorum";

/** The reserved name the daemon stamps on a human post (`teams_room_post`, design §0.11.4). */
export const OPERATOR_IDENTITY = "operator";

/**
 * The manager's reserved `from` (`MANAGER_IDENTITY`, crates/orchestrator/src/triage.rs). It is not
 * label-safe, so no roster entry can ever collide with it.
 */
export const MANAGER_IDENTITY = "@manager";

export interface RoomEvent {
  message: TeamsRoomMessage;
  kind: RoomKind;
  /** The small uppercase badge beside the author — "hand-off", "quorum ✕", "assign". */
  kindLabel: string;
  /** A deterministic triage assignment: the low-value run §5 collapses into a group. */
  deterministic: boolean;
  /** A quorum post reporting that nothing was requested — what the "quorum ✕" stat counts. */
  failed: boolean;
  /** The roster names this event is ABOUT (author, addressee, or named in the body). */
  teammates: string[];
  /** Local calendar day, `YYYY-MM-DD` — what the day dividers partition on. */
  day: string;
  /** Local `HH:MM`, the house style (`lib/format`), so the feed reads in the operator's own clock. */
  time: string;
}

/** A run of deterministic assignments, rendered as one expandable group (§5, box 3.5). */
export interface RoomGroup {
  type: "group";
  events: RoomEvent[];
  /** "9 tickets assigned deterministically, 11:44–13:59". */
  label: string;
}

export type FeedItem = { type: "event"; event: RoomEvent } | RoomGroup;

export interface DaySection {
  day: string;
  /** "Today · Sep 1", "Mon · Aug 31". */
  label: string;
  items: FeedItem[];
}

/**
 * How many consecutive deterministic assignments make a "run" worth collapsing. Two in a row is a
 * pair an operator can read; three is the point at which the routine drowns the conversation, which
 * is the whole reason §5 collapses them.
 */
export const MIN_ASSIGN_RUN = 3;

/** Bodies longer than this truncate with an expand (§5, box 3.7). */
export const BODY_TRUNCATE_AT = 220;

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** A ticket key as Linear spells it — what a ref has to look like to be counted as a ticket. */
const TICKET_KEY = /^[A-Z][A-Z0-9]*-\d+$/;

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** The `seq` half of a `<day>:<seq>` message id; -1 when the id is not that shape. */
function seqOf(id: string): number {
  const seq = Number.parseInt(id.slice(id.lastIndexOf(":") + 1), 10);
  return Number.isNaN(seq) ? -1 : seq;
}

/**
 * The local calendar day a post belongs to. Falls back to the id's day partition — a room id is
 * `<YYYY-MM-DD>:<seq>` (`LocalRoom::file_stem`) — so a post whose `at` never parsed still lands
 * under a divider instead of vanishing into one labelled from `NaN`.
 */
export function eventDay(m: TeamsRoomMessage): string {
  const ms = Date.parse(m.at);
  if (!Number.isNaN(ms)) {
    const d = new Date(ms);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  }
  const stem = m.id.slice(0, m.id.indexOf(":") === -1 ? m.id.length : m.id.indexOf(":"));
  return /^\d{4}-\d{2}-\d{2}$/.test(stem) ? stem : "";
}

/** Local `HH:MM`, or "" for a timestamp that will not parse. */
export function eventTime(m: TeamsRoomMessage): string {
  const ms = Date.parse(m.at);
  if (Number.isNaN(ms)) return "";
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** "Today · Sep 1" for `today`, else "Mon · Aug 31". `today` is a `YYYY-MM-DD` local day. */
export function dayLabel(day: string, today: string): string {
  const parts = day.split("-").map((p) => Number.parseInt(p, 10));
  if (parts.length !== 3 || parts.some((n) => Number.isNaN(n))) return day || "Undated";
  const [y, m, d] = parts;
  const date = new Date(y, m - 1, d);
  const stamp = `${MONTHS[date.getMonth()]} ${date.getDate()}`;
  return day === today ? `Today · ${stamp}` : `${WEEKDAYS[date.getDay()]} · ${stamp}`;
}

/** Today as a `YYYY-MM-DD` local day, for `dayLabel`. */
export function localDay(now: Date): string {
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

interface Classified {
  kind: RoomKind;
  kindLabel: string;
  deterministic: boolean;
  failed: boolean;
}

/**
 * What kind of event a post is, from its author and its body.
 *
 * The author decides the voice: `operator` is the human (teal, §1.1), `@manager` is the daemon's
 * own triage/quorum machinery (muted or red), and anyone else is a teammate — every teammate post
 * is the "hand-off" voice §1.1 assigns the accent to, whatever it says.
 *
 * Only the manager's bodies are pattern-matched, each against the exact `format!` that writes it.
 * An unrecognised manager post is deliberately NOT guessed at: it renders muted and labelled
 * "manager", which is wrong about nothing, rather than being filed under an assignment it may not
 * be.
 */
export function classify(m: TeamsRoomMessage): Classified {
  const plain = { deterministic: false, failed: false };
  if (m.from === OPERATOR_IDENTITY) return { kind: "operator", kindLabel: "you", ...plain };
  if (m.from !== MANAGER_IDENTITY) return { kind: "handoff", kindLabel: "hand-off", ...plain };

  const body = m.body ?? "";
  // quorum.rs: the two refusals shout, and the fan-out that succeeded opens with "Requested".
  if (body.startsWith("REVIEW QUORUM FAILED") || body.startsWith("NO REVIEW QUORUM")) {
    return { kind: "quorum", kindLabel: "quorum ✕", deterministic: false, failed: true };
  }
  if (body.startsWith("Requested review of")) {
    return { kind: "quorum", kindLabel: "quorum", ...plain };
  }
  // triage.rs: "Assigned <KEY> to <identity>[ (deterministic)]. Reason: …".
  if (body.startsWith("Assigned ")) {
    return {
      kind: "assign",
      kindLabel: "assign",
      deterministic: body.includes("(deterministic)"),
      failed: false,
    };
  }
  // triage.rs: a model turn that named somebody off the roster. An assignment decision, but never
  // a collapsible one — a rejected trust boundary is the opposite of routine.
  if (body.startsWith("REJECTED a triage decision")) {
    return { kind: "assign", kindLabel: "rejected", ...plain };
  }
  // triage.rs: the stray-label sweep.
  if (body.startsWith("Cleaned up ")) return { kind: "reconcile", kindLabel: "reconcile", ...plain };
  return { kind: "reconcile", kindLabel: "manager", ...plain };
}

/**
 * The roster names an event is about: its author, its addressee, and any teammate named in the
 * body — which is how a manager post ("Assigned STUDIO-674 to jimmy") scopes to the teammate it
 * concerns rather than to the manager.
 *
 * An event that names nobody on the roster returns empty, and the teammate filter leaves those
 * visible: a reconcile sweep or an operator note belongs in every teammate's view, exactly as the
 * prototype's `data-who="all"` events do.
 */
export function eventTeammates(m: TeamsRoomMessage, roster: readonly string[]): string[] {
  const out: string[] = [];
  for (const name of roster) {
    if (name === "") continue;
    const named =
      m.from === name ||
      m.to === name ||
      new RegExp(`(^|[^A-Za-z0-9-])${escapeName(name)}([^A-Za-z0-9-]|$)`).test(m.body ?? "");
    if (named) out.push(name);
  }
  return out;
}

/** Roster names are `^[a-z][a-z0-9-]*$` (`is_label_safe`), but escape anyway rather than trust it. */
function escapeName(name: string): string {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Classified events, NEWEST FIRST (§5: "feed: newest-first"). The daemon serves oldest first. */
export function roomEvents(
  messages: readonly TeamsRoomMessage[],
  roster: readonly string[],
): RoomEvent[] {
  const events = messages.map((message) => ({
    message,
    ...classify(message),
    teammates: eventTeammates(message, roster),
    day: eventDay(message),
    time: eventTime(message),
  }));
  return events.sort((a, b) => {
    const at = Date.parse(a.message.at);
    const bt = Date.parse(b.message.at);
    // An unparseable timestamp sorts oldest rather than jumping to the top of the feed.
    const an = Number.isNaN(at) ? -Infinity : at;
    const bn = Number.isNaN(bt) ? -Infinity : bt;
    if (an !== bn) return bn - an;
    return seqOf(b.message.id) - seqOf(a.message.id);
  });
}

/** Which kinds each filter chip admits (§5, box 3.3). */
export const FILTER_KINDS: Record<RoomFilter, readonly RoomKind[] | null> = {
  all: null,
  conversation: ["operator", "handoff"],
  handoff: ["handoff"],
  assign: ["assign", "reconcile"],
  quorum: ["quorum"],
};

export interface RoomQuery {
  filter: RoomFilter;
  /** A roster name, or "all". */
  who: string;
  /** Free text — matched against the body, the author and the refs. */
  search: string;
}

export function matchesFilter(event: RoomEvent, filter: RoomFilter): boolean {
  const kinds = FILTER_KINDS[filter];
  return kinds === null || kinds.includes(event.kind);
}

export function matchesTeammate(event: RoomEvent, who: string): boolean {
  return who === "all" || event.teammates.length === 0 || event.teammates.includes(who);
}

export function matchesSearch(event: RoomEvent, search: string): boolean {
  const needle = search.trim().toLowerCase();
  if (needle === "") return true;
  const m = event.message;
  return (
    (m.body ?? "").toLowerCase().includes(needle) ||
    m.from.toLowerCase().includes(needle) ||
    (m.refs ?? []).some((r) => r.toLowerCase().includes(needle))
  );
}

export function filterEvents(events: readonly RoomEvent[], q: RoomQuery): RoomEvent[] {
  return events.filter(
    (e) => matchesFilter(e, q.filter) && matchesTeammate(e, q.who) && matchesSearch(e, q.search),
  );
}

export interface RoomStats {
  /** Distinct tickets a teammate handed off in the loaded window. */
  inReview: number;
  handoffs: number;
  assigned: number;
  quorumFailed: number;
}

/**
 * The four stat pills (§5, box 3.1), all four derived from the room window this view already
 * loaded — the console's data sources are `/api/v1/teams` and `/api/v1/teams/room` and nothing
 * else, so "in review" is counted as the DISTINCT tickets a hand-off referenced rather than read
 * off an issue list. Six hand-offs covering five tickets is five in review, which is exactly the
 * relationship the prototype's numbers show.
 */
export function roomStats(events: readonly RoomEvent[]): RoomStats {
  const tickets = new Set<string>();
  let handoffs = 0;
  let assigned = 0;
  let quorumFailed = 0;
  for (const e of events) {
    if (e.kind === "handoff") {
      handoffs += 1;
      for (const ref of e.message.refs ?? []) if (TICKET_KEY.test(ref)) tickets.add(ref);
    }
    if (e.kind === "assign") assigned += 1;
    if (e.kind === "quorum" && e.failed) quorumFailed += 1;
  }
  return { inReview: tickets.size, handoffs, assigned, quorumFailed };
}

/**
 * The feed as it renders: days newest-first, and inside each day a run of `minRun` or more
 * consecutive deterministic assignments folded into one group.
 *
 * Runs are collapsed WITHIN a day, never across one: a group that straddled a divider would have
 * to appear under one of the two days and would lie about the other.
 */
export function daySections(
  events: readonly RoomEvent[],
  today: string,
  minRun: number = MIN_ASSIGN_RUN,
): DaySection[] {
  const sections: DaySection[] = [];
  for (const event of events) {
    const last = sections[sections.length - 1];
    if (last && last.day === event.day) last.items.push({ type: "event", event });
    else sections.push({ day: event.day, label: dayLabel(event.day, today), items: [{ type: "event", event }] });
  }
  return sections.map((s) => ({ ...s, items: collapseRuns(s.items, minRun) }));
}

function collapseRuns(items: readonly FeedItem[], minRun: number): FeedItem[] {
  const out: FeedItem[] = [];
  let run: RoomEvent[] = [];
  const flush = () => {
    if (run.length >= minRun) out.push({ type: "group", events: run, label: groupLabel(run) });
    else for (const event of run) out.push({ type: "event", event });
    run = [];
  };
  for (const item of items) {
    if (item.type === "event" && item.event.deterministic) {
      run.push(item.event);
      continue;
    }
    flush();
    out.push(item);
  }
  flush();
  return out;
}

/** "9 tickets assigned deterministically, 11:44–13:59" — the run's span, oldest time first. */
export function groupLabel(events: readonly RoomEvent[]): string {
  const times = events.map((e) => e.time).filter((t) => t !== "");
  const span = times.length > 0 ? `, ${times[times.length - 1]}–${times[0]}` : "";
  const n = events.length;
  return `${n} ticket${n === 1 ? "" : "s"} assigned deterministically${span}`;
}

/**
 * The three facts a triage assignment carries, pulled back out of the post triage.rs writes:
 * `Assigned <KEY> to <identity>[ (deterministic)]. Reason: <why>`. Returns null for anything that
 * is not that sentence, so a reworded body degrades to the raw text rather than to a wrong parse.
 */
export function parseAssignment(body: string): { ticket: string; identity: string; reason: string } | null {
  const m = /^Assigned (\S+) to (\S+?)(?: \(deterministic\))?\.? Reason: ([\s\S]*)$/.exec(body ?? "");
  if (!m) return null;
  return { ticket: m[1], identity: m[2], reason: m[3].trim().replace(/\.$/, "") };
}

/** A body split for §5's truncate-with-expand. `rest` is "" when the whole body already fits. */
export function truncateBody(body: string, at: number = BODY_TRUNCATE_AT): { head: string; rest: string } {
  const text = body ?? "";
  if (text.length <= at) return { head: text, rest: "" };
  // Break on the last space before the limit so the visible half never ends mid-word.
  const space = text.lastIndexOf(" ", at);
  return fenceSafeSplit(text, space > at / 2 ? space : at);
}

/** The plain split: everything before the cut is shown, everything after it expands. */
function plainSplit(text: string, cut: number): { head: string; rest: string } {
  return { head: text.slice(0, cut), rest: text.slice(cut).trim() };
}

/**
 * The split, made safe for a cut that lands inside a fenced code block (STUDIO-739).
 *
 * Both halves are rendered as markdown INDEPENDENTLY, so a raw cut inside a fence leaves the tail
 * starting on the closing fence — which opens a new unterminated block, turning every remaining
 * word of the post into monospace code. A post that leads with its verification output produces
 * exactly that.
 *
 * The repair CLOSES the block on the head and REOPENS it on the tail rather than moving the cut,
 * because moving it to a block boundary is unbounded in both directions: forwards it drags a whole
 * 5KB block into the collapsed feed (and, for an unterminated one, leaves no tail to expand at
 * all), backwards it throws the preview away. Keeping the cut where it is holds the head to the
 * budget plus the one fence line this adds, and keeps code rendering as code on both sides. Only a
 * cut inside a fence LINE has to move — half a fence is not a fence — and both of those moves are
 * bounded by that line.
 */
function fenceSafeSplit(text: string, cut: number): { head: string; rest: string } {
  const fence = fenceSpans(text).find((f) => cut > f.start && cut < f.end);
  if (!fence) return plainSplit(text, cut);
  // Inside the OPENING fence line: no part of the block can be kept, so cut just before it. When
  // the block OPENS the post that leaves no head at all — but no cut can do better there, because
  // a head ending mid-fence-line is a truncated unterminated fence, which renders as an EMPTY code
  // box and leaves the tail starting mid-info-string: the content lines then lazily continue as a
  // paragraph and the CLOSING fence reopens as a block that swallows the trailing prose. Blank
  // either way, so protect the tail.
  if (cut < fence.body) return plainSplit(text, fence.start);
  // Inside the CLOSING one: every content line is already in the head, so take the fence too.
  if (cut >= fence.close) return plainSplit(text, fence.end);
  // A cut inside a line can also MANUFACTURE a closing fence: split `cat ```' at its space and
  // the tail opens on " ```", which closes the reopened block at once and spills the rest of the
  // code out as prose. Backing up until the tail no longer reads as one fixes that, and backing
  // up a CHARACTER at a time bounds the cost by the marker run rather than by the line: a single
  // long code line whose tail is a marker run would otherwise cost the whole preview budget.
  const eol = text.indexOf("\n", cut) === -1 ? text.length : text.indexOf("\n", cut);
  const lineStart = text.lastIndexOf("\n", cut) + 1;
  let at = cut;
  while (at > lineStart && isClosingFence(text.slice(at, eol), fence.marker)) at -= 1;
  const head = text.slice(0, at);
  return {
    head: `${head}${head.endsWith("\n") ? "" : "\n"}${fence.marker}`,
    rest: `${fence.marker}${fence.info}\n${text.slice(at)}`.trimEnd(),
  };
}

// --- the day pager ---
//
// The daemon's room read takes ONE parameter, `limit`, and answers with the newest N posts
// (`GET /api/v1/teams/room`, `RoomParams`); there is no `before` and no per-day cursor, and the
// window is hard-capped server-side. So "Load older" widens the window it asks for and reveals one
// more calendar day of what comes back. Nothing here invents a route — a true backward pager needs
// a `before=<day>` parameter the daemon does not have, which §11 makes a dependency to flag rather
// than a surface to invent.

/** `DEFAULT_ROOM_WINDOW` (crates/config/src/room.rs) — the window a `limit`-less read serves. */
export const DEFAULT_ROOM_WINDOW = 20;

/**
 * `MAX_ROOM_WINDOW` (crates/config/src/room.rs) — the server-side ceiling. Restated here so the
 * pager knows when asking again cannot buy anything, and stops offering.
 */
export const MAX_ROOM_WINDOW = 50;

/** Days shown before the first "Load older" — the prototype opens on today plus the day before. */
export const INITIAL_DAYS = 2;

/** The window the next "Load older" asks for, clamped to what the daemon will serve. */
export function nextRoomLimit(limit: number): number {
  const now = Number.isFinite(limit) && limit > 0 ? limit : DEFAULT_ROOM_WINDOW;
  return Math.min(now + DEFAULT_ROOM_WINDOW, MAX_ROOM_WINDOW);
}
