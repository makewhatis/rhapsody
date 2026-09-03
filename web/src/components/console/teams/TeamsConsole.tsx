import * as React from "react";
import {
  Button,
  Card,
  Chip,
  Grid,
  GridSide,
  Markdown,
  Mate,
  NowMates,
  NowStats,
  NowStrip,
  Select,
  Stat,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { RoomFeed, RoomLegend } from "@/components/console/teams/RoomFeed";
import { SearchIcon } from "@/components/console/teams/icons";
import { useTeamsOverview, useTeamsRecall, useTeamsRoom, usePostToRoom } from "@/hooks/useTeams";
import { formatDateTime } from "@/lib/format";
import { errText } from "@/lib/teams-model";
import {
  DEFAULT_ROOM_WINDOW,
  INITIAL_DAYS,
  MAX_ROOM_WINDOW,
  daySections,
  filterEvents,
  localDay,
  nextRoomLimit,
  roomEvents,
  roomStats,
  type RoomFilter,
} from "@/lib/room-model";
import type { TeamsRosterRow } from "@/lib/api";
import "@/theme/teams-console.css";

// The Teams console — STUDIO-681 §5, the third slice of the dashboard redesign.
//
// Reachable only when the daemon reports `teams_enabled` (§2.2, shipped by the shell in
// sub-ticket 2), so every query below is safe to fire: none of them can reach a daemon that would
// answer `teams_disabled`. The view reads exactly two routes, `GET /api/v1/teams` and
// `GET/POST /api/v1/teams/room`, plus `GET /api/v1/teams/recall` for the memory card's two-fact
// preview — no endpoint here is new, which §11 makes a hard rule.

const FILTERS: readonly { value: RoomFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "conversation", label: "Conversation" },
  { value: "handoff", label: "Hand-offs" },
  { value: "assign", label: "Assignments" },
  { value: "quorum", label: "Quorum" },
];

export interface TeamsConsoleProps {
  /** Route to another view — the roster and memory cards' links (§5, box 3.10). */
  onNavigate: (route: "manage" | "memory" | "reviews") => void;
  /** Poll cadence for the roster and the room, matched to the daemon's own interval when known. */
  pollMs?: number;
  /** The clock the day dividers read "Today" from. Injected so the feed is testable. */
  now?: Date;
}

export function TeamsConsole({ onNavigate, pollMs, now }: TeamsConsoleProps) {
  // How wide a window the room read asks for, and how many days of it are revealed. "Load older"
  // moves both: the daemon's room route takes a `limit` and nothing else (see room-model's pager).
  const [limit, setLimit] = React.useState(DEFAULT_ROOM_WINDOW);
  const [visibleDays, setVisibleDays] = React.useState(INITIAL_DAYS);
  const [filter, setFilter] = React.useState<RoomFilter>("all");
  const [who, setWho] = React.useState("all");
  const [search, setSearch] = React.useState("");

  const overview = useTeamsOverview(true, pollMs);
  const room = useTeamsRoom(true, limit, pollMs);
  const roster = React.useMemo(() => overview.data?.roster ?? [], [overview.data]);
  const names = React.useMemo(() => roster.map((r) => r.name), [roster]);

  const events = React.useMemo(
    () => roomEvents(room.data?.messages ?? [], names),
    [room.data, names],
  );
  // The pills summarise the whole loaded window, not the days currently revealed: they are the
  // room's state, and a number that shrank when the feed happened to be scrolled up would be lying.
  const stats = React.useMemo(() => roomStats(events), [events]);
  const today = localDay(now ?? new Date());
  const sections = React.useMemo(
    () => daySections(filterEvents(events, { filter, who, search }), today),
    [events, filter, who, search, today],
  );

  const shown = sections.slice(0, visibleDays);
  // A read that came back FULL is the only evidence that the room holds more than was served, so a
  // short window retires the pager instead of offering two more clicks that can return nothing.
  const windowFull = (room.data?.messages.length ?? 0) >= limit;
  // Older history may be one reveal away, or one wider read away — offer the pager for either.
  const hasOlder = sections.length > visibleDays || (windowFull && limit < MAX_ROOM_WINDOW);
  const loadOlder = () => {
    setVisibleDays((d) => d + 1);
    setLimit((l) => nextRoomLimit(l));
  };

  const idle = roster.filter((r) => r.live_runs === 0).length;

  return (
    // `.rh-console` is normally inherited from AppShell, which carries the theme scope; it is
    // repeated here so the view is also correct rendered on its own (a test, a gallery route).
    <section className="rh-console">
      <div className="head">
        <h1>Teams</h1>
        <TeamSwitcher count={roster.length} />
        <div className="spacer" />
        {/* The way into the ticketless review watch set and its operator controls (STUDIO-722).
            A link from here rather than a rail item of its own, because Reviews is a Teams child
            like Manage team — see `NAV_PARENT` in lib/console-routing.ts. */}
        <button type="button" className="link" onClick={() => onNavigate("reviews")}>
          Reviews →
        </button>
        <span className="build">
          {idle} idle · {stats.inReview} in review
        </span>
      </div>

      <NowStrip>
        <NowMates>
          {roster.length === 0 ? (
            <Mate name={overview.isLoading ? "loading…" : "no teammates"} />
          ) : (
            roster.map((r) => (
              <Mate key={r.name} name={r.name} running={r.live_runs > 0} task={mateTask(r)} />
            ))
          )}
        </NowMates>
        <NowStats>
          <Stat value={stats.inReview} label="in review" tone="acc" />
          <Stat value={stats.handoffs} label="hand-offs" />
          <Stat value={stats.assigned} label="assigned" />
          <Stat value={stats.quorumFailed} label="quorum ✕" tone="bad" />
        </NowStats>
      </NowStrip>

      <Grid>
        <Card title="The room" sub="newest first · async notes, never live instructions">
          <div className="roomtop">
            <div className="filters">
              {FILTERS.map((f) => (
                <Chip
                  key={f.value}
                  pressed={filter === f.value}
                  count={f.value === "quorum" && stats.quorumFailed > 0 ? stats.quorumFailed : undefined}
                  onClick={() => setFilter(f.value)}
                >
                  {f.label}
                </Chip>
              ))}
              {/*
                A Select, not a chip per teammate (§5, box 3.4): the roster scales to N, and a chip
                row that grew with it would push the filters onto three lines on a real team.
              */}
              <Select
                aria-label="Filter the room by teammate"
                value={who}
                onChange={(e) => setWho(e.target.value)}
                options={[{ value: "all", label: "All teammates" }, ...names.map((n) => ({ value: n }))]}
              />
            </div>
            <label className="search">
              <SearchIcon width={14} height={14} />
              <input
                type="text"
                aria-label="Search the room"
                placeholder="Search the room — text, ticket, PR…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
              />
            </label>
            <Composer />
          </div>
          <RoomFeed
            sections={shown}
            loading={room.isLoading}
            error={room.isError ? errText(room.error) : undefined}
            hasOlder={hasOlder && sections.length > 0}
            fetchingOlder={room.isPlaceholderData}
            onLoadOlder={loadOlder}
          />
          <RoomLegend />
        </Card>

        <GridSide>
          <Card
            title="Roster"
            sub={rosterSub(roster.length, overview.data?.manager_mode)}
            right={
              <button type="button" className="link" onClick={() => onNavigate("manage")}>
                Manage team →
              </button>
            }
          >
            {overview.isError ? (
              <div className="quiet">Could not read the roster: {errText(overview.error)}</div>
            ) : (
              <RosterTable rows={roster} />
            )}
          </Card>

          <MemoryPreview
            identity={overview.data?.default_identity || names[0] || ""}
            onOpenMemory={() => onNavigate("memory")}
          />
        </GridSide>
      </Grid>
    </section>
  );
}

/**
 * §5 asks for the team switcher's PLACEMENT; wiring it is STUDIO-668's work, and §11 puts the rest
 * of multi-team out of scope here. It is disabled rather than absent so the slot is real, and it
 * says why — a control that looked live and did nothing would be worse than none.
 */
function TeamSwitcher({ count }: { count: number }) {
  return (
    <button
      type="button"
      className="teamsw"
      disabled
      title="This daemon runs one team. Switching between teams arrives with multi-team support (STUDIO-668)."
    >
      <span className="dot" aria-hidden="true" />
      Team · {count}
      <span className="car" aria-hidden="true">
        ▾
      </span>
    </button>
  );
}

/** What a teammate is doing right now: the tickets its live runs hold, or "idle". */
function mateTask(row: TeamsRosterRow): string {
  if (row.live_runs === 0) return "idle";
  const tickets = row.tickets ?? [];
  return tickets.length > 0 ? tickets.join(", ") : `${row.live_runs} live`;
}

function rosterSub(count: number, mode: string | undefined): string {
  const team = `${count} teammate${count === 1 ? "" : "s"}`;
  return mode === undefined ? team : `${team} · ${mode === "off" ? "unrouted" : `${mode}-routed`}`;
}

function RosterTable({ rows }: { rows: readonly TeamsRosterRow[] }) {
  return (
    <table className="rtbl" aria-label="Roster">
      <thead>
        <tr>
          <th>Teammate</th>
          <th>Profile</th>
          <th>Now</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={row.name}>
            <td>
              <span className={row.live_runs > 0 ? "nm run" : "nm"}>
                <span className="st" aria-hidden="true" />
                {row.name}
              </span>
              <div className="m">{row.bank}</div>
            </td>
            <td>{row.profile}</td>
            <td className="now-col">{mateTask(row)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** How many of an identity's most recent facts the side card previews before deferring to §6. */
const MEMORY_PREVIEW = 2;

function MemoryPreview({ identity, onOpenMemory }: { identity: string; onOpenMemory: () => void }) {
  const recall = useTeamsRecall(identity, "", identity !== "");
  const facts = React.useMemo(() => {
    const all = [...(recall.data?.facts ?? [])];
    // Recall is bounded by `recall_top_k` and ordered by the bank, not by us: sort before slicing
    // so "2 recent" is actually the two most recent. A record the daemon could not stamp cleanly
    // sorts oldest rather than into an arbitrary slot, the same rule the room's feed uses.
    const when = (iso: string) => {
      const ms = Date.parse(iso);
      return Number.isNaN(ms) ? -Infinity : ms;
    };
    all.sort((a, b) => when(b.at) - when(a.at));
    return all.slice(0, MEMORY_PREVIEW);
  }, [recall.data]);

  return (
    <Card
      title="Memory"
      sub={identity === "" ? "no bank" : `${identity} · ${facts.length} recent`}
      right={
        <button type="button" className="link" onClick={onOpenMemory}>
          Open memory →
        </button>
      }
    >
      <div className="memprev">
        {recall.isError ? (
          <div className="quiet">Could not read the bank: {errText(recall.error)}</div>
        ) : facts.length === 0 ? (
          <div className="quiet">{recall.isLoading ? "Loading…" : "Nothing retained yet."}</div>
        ) : (
          facts.map((fact) => (
            <div className="mcard" key={fact.id}>
              <div className="top">
                {fact.ticket === "" ? null : <TicketChip>{fact.ticket}</TicketChip>}
                {fact.run_id === "" ? null : <TicketChip variant="sha">run {fact.run_id}</TicketChip>}
                <Timestamp>{formatDateTime(fact.at)}</Timestamp>
              </div>
              {/* Untrusted content, same as a room post (design §0.11.5) — quoted, never
                  asserted, and rendered as markdown-shaped data (STUDIO-739). */}
              <blockquote>
                <Markdown source={fact.content} />
              </blockquote>
            </div>
          ))
        )}
        <button type="button" className="link" onClick={onOpenMemory}>
          See all memory →
        </button>
      </div>
    </Card>
  );
}

/**
 * The operator's own voice in the room (§5, box 3.9). The post carries no author field: the daemon
 * stamps the reserved name `operator` on it (design §0.11.4), and there is nothing here that could
 * argue with that.
 */
function Composer() {
  const [body, setBody] = React.useState("");
  const [refs, setRefs] = React.useState("");
  const post = usePostToRoom();

  const submit = () => {
    const text = body.trim();
    if (text === "" || post.isPending) return;
    post.mutate(
      { body: text, refs: splitRefs(refs) },
      {
        onSuccess: () => {
          setBody("");
          setRefs("");
        },
      },
    );
  };

  return (
    <div className="composer">
      <textarea
        aria-label="Post to the team room"
        placeholder="Post to the team room…  e.g.  Someone review the export PR — STUDIO-498"
        value={body}
        onChange={(e) => setBody(e.target.value)}
      />
      <div className="row">
        <input
          type="text"
          aria-label="Refs"
          placeholder="Refs (optional): STUDIO-498, a PR url, a SHA"
          value={refs}
          onChange={(e) => setRefs(e.target.value)}
        />
        <Button onClick={submit} disabled={body.trim() === "" || post.isPending}>
          {post.isPending ? "Posting…" : "Post as operator"}
        </Button>
      </div>
      {post.isError ? <div className="quiet">Could not post: {errText(post.error)}</div> : null}
    </div>
  );
}

/** Refs are typed as a comma- or space-separated line; blanks are dropped rather than posted. */
export function splitRefs(text: string): string[] {
  return text
    .split(/[,\s]+/)
    .map((s) => s.trim())
    .filter((s) => s !== "");
}
