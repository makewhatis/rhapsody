import { useMemo } from "react";
import {
  Chip,
  ExternalLink,
  Pill,
  TeammateAvatar,
  TicketChip,
} from "@/components/console";
import { cn } from "@/lib/utils";
import type { BoardLaneWidth } from "@/hooks/useBoardLaneWidth";
import type { BoardCardFields } from "@/hooks/useBoardCardFields";
import { teammateColor } from "@/theme/teammates";
import type { BlockedEntry } from "@/lib/api";
import {
  buildConsoleBoard,
  FILTERED_LANE_EMPTY,
  TRUNCATED_LANE_EMPTY,
  pullRequestLabel,
  type BoardCard,
  type BoardLane,
  type ReviewerChip,
} from "@/lib/console-board";
import {
  relativeSince,
  type ConsoleJobCounts,
  type ConsoleJobRow,
  type ConsoleJobStatus,
} from "@/lib/console-jobs";

// Board — the worklist as work, not as runs (STUDIO-925).
//
// The Jobs table's unit of display is the RUN, so one ticket with two finished reviews reads as
// three unrelated rows. Here the unit is the TICKET: a card per work item, its reviews folded in as
// chips, and a lane per run status. It is built entirely from what the table already holds —
// `console-board.buildConsoleBoard` does the regroup, the same `useJobsFeed` feeds it, and the
// project Select above it narrows the cards the same way it narrows rows. No new endpoint, no
// daemon change.
//
// The STATUS filter does NOT narrow the board (STUDIO-932): the four lanes already ARE the status
// axis, so a status Seg here was a control that undid the board rather than a filter over it. The
// Seg is not rendered in Board mode at all (see `JobsView`), and no status filter reaches this view.
//
// The four lanes ALWAYS render, empty ones included: the board is quietest exactly when the
// pipeline is idle or starved, and that is the state it must not hide. Running draws its unused
// `max_concurrent_agents` slots, so an idle slot beside a full Queued lane reads as starvation.
//
// READ-ONLY by design: a lane is a run status (Done is a tracker state), so dragging a card would imply a tracker write —
// the console has no write path for it, and the rate limit hit on 2026-09-17 is why it stays out.
export interface BoardViewProps {
  /** Every worklist row, review rows included — the board folds them onto their tickets. */
  rows: readonly ConsoleJobRow[];
  /** The live snapshot's held dependents — the board's only dependency edge. */
  blocked: readonly BlockedEntry[];
  /** The project Select's value ("" = all projects). */
  project: string;
  /** The daemon's whole-store tally, so the footer agrees with the Now strip above it. */
  counts: ConsoleJobCounts | undefined;
  /** `max_concurrent_agents` — the cap the footer and the Running lane measure against. */
  maxConcurrent: number;
  /** The lane track width, from the display-options popover. */
  laneWidth: BoardLaneWidth;
  /** Which card elements the display-options chips currently show (STUDIO-932). */
  fields: BoardCardFields;
  /** When the listing was last answered, for the footer's refreshed stamp. */
  refreshedAtMs: number;
  nowMs: number;
  roster: readonly string[];
  onOpenJob: (issue: string) => void;
  /** The table's own truncation sentence, carried so the board cannot silently stop either. */
  pageNote: string;
  hasMore: boolean;
  onLoadMore: () => void;
  loadingMore: boolean;
}

export function BoardView({
  rows,
  blocked,
  project,
  counts,
  maxConcurrent,
  laneWidth,
  fields,
  refreshedAtMs,
  nowMs,
  roster,
  onOpenJob,
  pageNote,
  hasMore,
  onLoadMore,
  loadingMore,
}: BoardViewProps) {
  // The regroup is over EVERY row, then the project Select narrows the cards — never the input to
  // `buildConsoleBoard`. A review row filtered away before the regroup would silently strip a
  // surviving card of its chips, which is the one thing the board exists to show. Status is NOT
  // applied: the lanes are that axis (STUDIO-932).
  const lanes = useMemo(() => buildConsoleBoard(rows, blocked), [rows, blocked]);
  const filtered = project !== "";
  const visible = useMemo(
    () =>
      lanes.map((lane) => ({
        ...lane,
        cards: lane.cards.filter((card) => project === "" || card.projectSlug === project),
      })),
    [lanes, project],
  );

  const running = counts?.running;
  const capped = maxConcurrent > 0 && (running ?? 0) >= maxConcurrent;
  const refreshed = relativeSince(refreshedAtMs, nowMs);
  // Occupancy is the daemon's whole-store tally, not the filtered lane: a project filter must not
  // make a full pool look idle. Before the tally lands, fall back to the unfiltered Running lane.
  const occupied = running ?? lanes.find((l) => l.id === "running")?.cards.length ?? 0;

  return (
    <div className="boardwrap">
      <div className="board" data-lane-width={laneWidth}>
        {visible.map((lane) => (
          <LaneView
            key={lane.id}
            lane={lane}
            filtered={filtered}
            occupied={occupied}
            truncated={hasMore}
            maxConcurrent={maxConcurrent}
            roster={roster}
            fields={fields}
            onOpen={onOpenJob}
          />
        ))}
      </div>

      <div className="bfoot">
        <span className={cn("bfrun", capped && "capped")}>
          Running <b>{running ?? "—"}</b>
          {maxConcurrent > 0 ? ` / ${maxConcurrent}` : ""}
        </span>
        <span className="bfstat">
          Blocked <b>{counts?.blocked ?? "—"}</b>
        </span>
        <span className="bfstat">
          Queued <b>{counts?.queued ?? "—"}</b>
        </span>
        <span className="bfstat">
          In Review <b>{counts?.review ?? "—"}</b>
        </span>
        <div className="bspacer" />
        {pageNote === "" ? null : <span className="bnote">{pageNote}</span>}
        {hasMore ? (
          <Chip onClick={onLoadMore} disabled={loadingMore}>
            Load more
          </Chip>
        ) : null}
        <span className="bref">{refreshed === "—" ? "not refreshed yet" : `refreshed ${refreshed}`}</span>
      </div>
    </div>
  );
}

// One lane. The Running lane also draws the pool: `occupied / max` in its header and one dashed slot
// per unused seat, so free capacity is visible as absence made concrete.
function LaneView({
  lane,
  filtered,
  occupied,
  truncated,
  maxConcurrent,
  roster,
  fields,
  onOpen,
}: {
  lane: BoardLane;
  filtered: boolean;
  occupied: number;
  /** The listing is one page of a longer one, so an empty lane says nothing about the pipeline. */
  truncated: boolean;
  maxConcurrent: number;
  roster: readonly string[];
  fields: BoardCardFields;
  onOpen: (issue: string) => void;
}) {
  const isRunning = lane.id === "running";
  const freeSlots = isRunning && maxConcurrent > 0 ? Math.max(0, maxConcurrent - occupied) : 0;
  // A held seat with no card in this lane is a live review (folded onto its ticket in In Review) or
  // an unattributed run; "No agent is running." would contradict the `n / max` beside it.
  const emptyLine = filtered
    ? FILTERED_LANE_EMPTY
    : isRunning && occupied > 0
      ? "Agents are busy on reviews and other runs, shown on their tickets in other lanes."
      : truncated && !(isRunning && occupied === 0)
        ? TRUNCATED_LANE_EMPTY
        : lane.empty;
  return (
    <section className="bcol" aria-label={lane.name} data-lane={lane.id}>
      <header className="bcolhd">
        <span className="bname">{lane.name}</span>
        <span className="bcount" title={isRunning && maxConcurrent > 0 ? "Whole pool, all projects" : undefined}>
          {isRunning && maxConcurrent > 0 ? `${occupied} / ${maxConcurrent}` : lane.cards.length}
        </span>
        <span className="bsub">{lane.caption}</span>
      </header>
      <div className="bcards">
        {lane.cards.map((card) => (
          <BoardCardView key={card.key} card={card} roster={roster} fields={fields} onOpen={onOpen} />
        ))}
        {lane.cards.length === 0 ? (
          <div className="bempty">{emptyLine}</div>
        ) : null}
        {Array.from({ length: freeSlots }, (_, i) => (
          <div className="bslot" key={`slot-${i}`}>
            idle slot
          </div>
        ))}
      </div>
    </section>
  );
}

// One card. A real activation target like a table row: the whole card opens the ticket's job, so it
// owes Enter/Space and a focus ring as well as the pointer (§10 box 2.8).
//
// The five display-option chips (STUDIO-932) gate the card's OPTIONAL elements only. The ticket key,
// title and status pill are the card's identity and always draw; the assignee, project, harness,
// review chips and pull request come and go with `fields`.
function BoardCardView({
  card,
  roster,
  fields,
  onOpen,
}: {
  card: BoardCard;
  roster: readonly string[];
  fields: BoardCardFields;
  onOpen: (issue: string) => void;
}) {
  const open = () => onOpen(card.issue);
  // Hiding every meta element drops the row rather than leaving an empty flex line with a gap.
  const showMeta =
    (fields.assignee && card.assignee !== "") ||
    (fields.project && card.project !== "") ||
    (fields.harness && card.provider !== "");
  return (
    <article
      className={cn("bcard", card.live && "live")}
      role="link"
      tabIndex={0}
      aria-label={`${card.issue} ${card.title}`}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          open();
        }
      }}
    >
      <div className="btop">
        <span className="bkey">{card.issue}</span>
        <Pill variant={card.status}>{card.statusLabel}</Pill>
      </div>
      {card.title === "" ? null : <div className="btitle">{card.title}</div>}
      {showMeta ? (
        <div className="bmeta">
          {!fields.assignee || card.assignee === "" ? null : (
            <span className="who2">
              <TeammateAvatar color={teammateColor(roster, card.assignee)} size={7} />
              {card.assignee}
            </span>
          )}
          {!fields.project || card.project === "" ? null : <span className="bproj">{card.project}</span>}
          {!fields.harness || card.provider === "" ? null : (
            <span className="provbadge" title={`ran on ${card.provider}`}>
              {card.provider}
            </span>
          )}
        </div>
      ) : null}
      {!fields.reviews || card.reviewers.length === 0 ? null : (
        <div className="bchips" aria-label="Reviews">
          {card.reviewers.map((r) => (
            <ReviewerChipView key={r.key} chip={r} />
          ))}
        </div>
      )}
      {card.dependencies.length === 0 ? null : (
        <div className="bchips" aria-label="Blockers">
          {card.dependencies.map((d) => (
            <span className="dchip" key={d} title={d}>
              waiting on {d}
            </span>
          ))}
        </div>
      )}
      {!fields.pullRequest || card.pr === undefined ? null : (
        // The anchor is wrapped rather than given its own `onClick`: `ExternalLink` deliberately
        // does not accept one (so its open-seam cannot be replaced), and the card underneath must not
        // navigate when the operator clicked the link.
        <div className="bpr" onClick={(e) => e.stopPropagation()}>
          <ExternalLink href={card.pr.url} aria-label={`Open ${card.pr.owner}/${card.pr.repo} pull request ${card.pr.number}`}>
            <TicketChip variant="pr">{pullRequestLabel(card.pr)}</TicketChip>
          </ExternalLink>
        </div>
      )}
    </article>
  );
}

// A reviewer's chip: who reviewed, and how that run ended. The tone is the chip's whole point — two
// green chips on an in-review card is the two-gate state at a glance — so it keys off the review
// RUN's status rather than reusing the status Pill, whose `done` is the tracker-blue `--info`.
function ReviewerChipView({ chip }: { chip: ReviewerChip }) {
  // Two reviewers can be on different pull requests, so the chip names its own PR in the tooltip
  // rather than assuming the card's primary one.
  const on = chip.pr === undefined ? "" : ` on ${pullRequestLabel(chip.pr)}`;
  return (
    <span
      className={cn("rchip", reviewerTone(chip.status))}
      title={`${chip.reviewer}${on} · review ${chip.outcome}`}
    >
      <span className="d" aria-hidden="true" />
      {chip.reviewer}
      <span className="o">{chip.outcome}</span>
    </span>
  );
}

function reviewerTone(status: ConsoleJobStatus): string {
  switch (status) {
    case "run":
    case "reviewing":
      return "live";
    case "done":
      return "ok";
    case "blocked":
      return "bad";
    default:
      return "queued";
  }
}
