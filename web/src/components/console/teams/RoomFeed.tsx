import { TicketChip, Timestamp } from "@/components/console";
import { KindIcon } from "@/components/console/teams/icons";
import { parseAssignment, truncateBody, type DaySection, type RoomEvent, type RoomGroup } from "@/lib/room-model";

// The room's feed (STUDIO-681 §5): day dividers, typed events, the collapsed assignment group and
// the day pager. Presentational — every decision about what belongs where was already made in
// `lib/room-model`, so this file only says how it looks.

export interface RoomFeedProps {
  /** Newest day first, already filtered and already day-paged by the view. */
  sections: readonly DaySection[];
  /** The room has never answered yet. */
  loading: boolean;
  /** The daemon's own complaint, verbatim, when the read failed. */
  error?: string;
  /** A wider window or an unrevealed day is still available. */
  hasOlder: boolean;
  /** A widened read is in flight — the pager's spinner. */
  fetchingOlder: boolean;
  onLoadOlder: () => void;
}

export function RoomFeed({ sections, loading, error, hasOlder, fetchingOlder, onLoadOlder }: RoomFeedProps) {
  if (error !== undefined) {
    return <div className="quiet">Could not read the room: {error}</div>;
  }
  if (sections.length === 0) {
    return (
      <div className="quiet">
        {loading
          ? "Loading the room…"
          : "Nothing has been posted yet — teammates post with `teams_post`, and you can post above."}
      </div>
    );
  }
  return (
    <div className="feed">
      {sections.map((section) => (
        <div key={section.day || "undated"}>
          <div className="day">
            <span className="t">{section.label}</span>
            <span className="ln" />
          </div>
          {section.items.map((item) =>
            item.type === "group" ? (
              <AssignmentGroup key={item.events[0].message.id} group={item} />
            ) : (
              <Event key={item.event.message.id} event={item.event} />
            ),
          )}
        </div>
      ))}
      {hasOlder ? (
        <div className="older">
          <button type="button" onClick={onLoadOlder}>
            {fetchingOlder ? <span className="sp" /> : null}
            Load older
          </button>
        </div>
      ) : null}
    </div>
  );
}

function Event({ event }: { event: RoomEvent }) {
  const { message, kind, kindLabel, time } = event;
  const { head, rest } = truncateBody(message.body ?? "");
  return (
    <article className="event" data-kind={kind}>
      <KindIcon kind={kind} />
      <div className="meta">
        <span className="from">{message.from}</span>
        <span className="kind">{kindLabel}</span>
        <Timestamp>{time}</Timestamp>
      </div>
      {/*
        A room post is untrusted content that also reaches every teammate's prompt (design §0.11.5),
        so it renders as QUOTED, attributed data — never as text that could read as the app talking.
      */}
      <blockquote className="body">
        {head}
        {rest === "" ? null : (
          <details className="more">
            <summary>show full note</summary>
            <div className="full">{rest}</div>
          </details>
        )}
      </blockquote>
      {(message.refs ?? []).length > 0 ? (
        <div className="chips">
          {message.refs.map((ref) => (
            <RefChip key={ref} refValue={ref} />
          ))}
        </div>
      ) : null}
    </article>
  );
}

function AssignmentGroup({ group }: { group: RoomGroup }) {
  return (
    <details className="group">
      <summary>
        <span className="car" aria-hidden="true">
          ▸
        </span>
        <b>{group.label}</b>
      </summary>
      <div className="inner">
        {group.events.map((event) => {
          const parsed = parseAssignment(event.message.body ?? "");
          const ticket = parsed?.ticket ?? event.message.refs?.[0] ?? "";
          const identity = parsed?.identity ?? event.teammates[0] ?? "";
          return (
            <div className="gline" key={event.message.id}>
              <Timestamp>{event.time}</Timestamp>
              <span className="who">→ {identity}</span>
              <span>{parsed?.reason ?? event.message.body}</span>
              {ticket === "" ? null : <TicketChip>{ticket}</TicketChip>}
            </div>
          );
        })}
      </div>
    </details>
  );
}

/** A `pull/<n>` url in the refs is a PR, a hex string is a commit, anything else is a ticket key. */
export function RefChip({ refValue }: { refValue: string }) {
  const pr = /\/pull\/(\d+)/.exec(refValue);
  if (pr) return <TicketChip variant="pr">PR #{pr[1]}</TicketChip>;
  if (/^[0-9a-f]{7,40}$/i.test(refValue)) return <TicketChip variant="sha">{refValue.slice(0, 7)}</TicketChip>;
  return <TicketChip>{refValue}</TicketChip>;
}

/** The color key under the feed, naming each rail (§5). */
export function RoomLegend() {
  return (
    <div className="legend">
      <span>
        <i style={{ background: "var(--operator)" }} />
        operator (you)
      </span>
      <span>
        <i style={{ background: "var(--handoff)" }} />
        teammate hand-off
      </span>
      <span>
        <i style={{ background: "var(--ink-4)" }} />
        manager · assign / reconcile
      </span>
      <span>
        <i style={{ background: "var(--bad)" }} />
        quorum failed
      </span>
    </div>
  );
}
