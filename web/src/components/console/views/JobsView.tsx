import { useEffect, useMemo, useRef, useState } from "react";
import {
  Card,
  Chip,
  Mate,
  NowMates,
  NowStats,
  NowStrip,
  Pill,
  Seg,
  Select,
  Stat,
  TeammateAvatar,
  TicketChip,
} from "@/components/console";
import { cn } from "@/lib/utils";
import { teammateColor } from "@/theme/teammates";
import {
  CONSOLE_JOB_FILTERS,
  buildConsoleJobs,
  consoleJobCounts,
  consoleJobProjects,
  filterConsoleJobs,
  mateStates,
  type ConsoleJobFilterId,
  type ConsoleJobRow,
} from "@/lib/console-jobs";
import { sparkSummary, traceSpark } from "@/lib/console-trace-spark";
import { buildTrace } from "@/lib/trace-model";
import { mergeJobs } from "@/lib/runs-model";
import { useLinearProjects } from "@/hooks/useConfig";
import { useIssueRuns } from "@/hooks/useHistory";
import { useNow } from "@/hooks/useNow";
import { useTranscript } from "@/hooks/useRunDetail";
import { useRefresh, useStateQuery } from "@/hooks/useStateQuery";
import { useTeamsEnabled, useTeamsOverview } from "@/hooks/useTeams";

const ALL_PROJECTS = "";

// Jobs — the console's home worklist (STUDIO-681 §3), built by STUDIO-683. Every ticket the
// daemon is working, at a glance: the Now strip, the two filters, and the table.
//
// Its inputs are the endpoints §9 actually has — `/api/v1/state` for the live snapshot and
// `/api/v1/history/issues` for one row per ticket. `lib/console-jobs.ts` records what that
// costs against the `GET /api/v1/issues` the spec assumed.
export function JobsView({ onOpenJob }: { onOpenJob: (issue: string) => void }) {
  const nowMs = useNow(30_000);
  const state = useStateQuery();
  const issueRuns = useIssueRuns();
  const projects = useLinearProjects().data ?? [];
  const teamsEnabled = useTeamsEnabled();
  const overview = useTeamsOverview(teamsEnabled);
  const refresh = useRefresh();

  const [filter, setFilter] = useState<ConsoleJobFilterId>("all");
  const [project, setProject] = useState(ALL_PROJECTS);

  const issueRows = useMemo(() => issueRuns.data?.issues ?? [], [issueRuns.data]);
  const rows = useMemo(
    () =>
      buildConsoleJobs(
        mergeJobs(state.data, issueRows, projects, nowMs),
        issueRows,
        overview.data,
        nowMs,
      ),
    [state.data, issueRows, projects, overview.data, nowMs],
  );

  const counts = consoleJobCounts(rows);
  const mates = mateStates(overview.data);
  const roster = mates.map((m) => m.name);
  const visible = filterConsoleJobs(rows, filter, project);
  const projectOptions = [{ value: ALL_PROJECTS, label: "All projects" }, ...consoleJobProjects(rows)];

  return (
    <section>
      <div className="head">
        <h1>Jobs</h1>
        <div className="spacer" />
        <Chip onClick={() => refresh.mutate()} disabled={refresh.isPending}>
          ↻ Refresh
        </Chip>
      </div>

      <NowStrip>
        <NowMates>
          {mates.length === 0 ? (
            <Mate name="rhapsodyd" task={counts.running > 0 ? "running" : "idle"} running={counts.running > 0} />
          ) : (
            mates.map((mate) => (
              <Mate key={mate.name} name={mate.name} task={mate.task} running={mate.running} />
            ))
          )}
        </NowMates>
        <NowStats>
          <Stat value={counts.running} label="running" />
          <Stat value={counts.review} label="in review" tone="acc" />
          <Stat value={counts.queued} label="queued" />
          <Stat value={counts.blocked} label="blocked" tone="bad" />
          {/* The operator's own queue (STUDIO-743, design record §6). It cuts across the four
              above rather than partitioning with them — see `needsOperator`. */}
          <Stat value={counts.needsYou} label="needs you" tone="op" />
        </NowStats>
      </NowStrip>

      <div className="jfilters">
        <Seg
          accent
          aria-label="Filter by status"
          options={CONSOLE_JOB_FILTERS.map((f) => ({ value: f.id, label: f.label }))}
          value={filter}
          onChange={(v) => setFilter(v as ConsoleJobFilterId)}
        />
        <Select
          aria-label="Filter by project"
          options={projectOptions}
          value={project}
          onChange={(e) => setProject(e.target.value)}
        />
      </div>

      <Card>
        <table className="jtbl">
          <thead>
            <tr>
              <th>Ticket</th>
              <th>Assigned</th>
              <th>Status</th>
              <th>Trace</th>
              <th>PR</th>
              <th>Updated</th>
            </tr>
          </thead>
          <tbody>
            {visible.map((row) => (
              <JobsRow key={row.key} row={row} roster={roster} onOpen={onOpenJob} />
            ))}
          </tbody>
        </table>
        {visible.length === 0 ? <div className="empty">{emptyMessage(rows.length, issueRuns.isPending)}</div> : null}
      </Card>
    </section>
  );
}

function emptyMessage(total: number, loading: boolean): string {
  if (loading) return "Loading jobs…";
  return total === 0 ? "No jobs yet." : "No jobs match this filter.";
}

// How long the operator must rest on a row before its transcript is fetched. The guard is the
// whole reason a sweep down the table costs nothing: every row that is merely PASSED — crossed by
// the pointer, or tabbed through on the way to another — clears its timer on the way out, so only
// a row that was actually stopped on is ever fetched.
const SPARK_DWELL_MS = 120;

// One worklist row. It is a real activation target, not a div with a click handler: the whole
// row navigates, so it owes Enter/Space and a focus ring as well as the pointer (§10 box 2.8).
function JobsRow({
  row,
  roster,
  onOpen,
}: {
  row: ConsoleJobRow;
  roster: readonly string[];
  onOpen: (issue: string) => void;
}) {
  // Arming is one-way: once this row's transcript has been asked for it stays asked for, so the
  // strip does not blink away when the operator moves on.
  const [armed, setArmed] = useState(false);
  const dwell = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearDwell = () => {
    if (dwell.current !== null) {
      clearTimeout(dwell.current);
      dwell.current = null;
    }
  };
  const arm = () => {
    if (!armed && dwell.current === null) {
      dwell.current = setTimeout(() => {
        dwell.current = null;
        setArmed(true);
      }, SPARK_DWELL_MS);
    }
  };
  useEffect(() => clearDwell, []);

  return (
    <tr
      tabIndex={0}
      role="link"
      aria-label={`${row.issue} ${row.title}`}
      onClick={() => onOpen(row.issue)}
      onMouseEnter={arm}
      onMouseLeave={clearDwell}
      // Focus arms too, so the strip is reachable without a pointer — and behind the same dwell,
      // because tabbing to the tenth row passes through nine others exactly as a pointer does.
      onFocus={arm}
      onBlur={clearDwell}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen(row.issue);
        }
      }}
    >
      <td>
        <div className="ti">
          {row.issue}
          {row.title === "" ? "" : ` · ${row.title}`}
        </div>
        <div className="pj">{row.project}</div>
      </td>
      <td>
        {row.assignee === "" ? (
          "—"
        ) : (
          <span className="who2">
            <TeammateAvatar color={teammateColor(roster, row.assignee)} size={7} />
            {row.assignee}
          </span>
        )}
      </td>
      {/* The Pill shows the normalized status; the tracker's own state name is the ground truth
          behind it, so it hovers (STUDIO-702). "" when the daemon had no answer for this ticket. */}
      <td title={row.trackerState === "" ? undefined : row.trackerState}>
        <Pill variant={row.status}>
          {row.statusLabel}
          {row.subLabel === undefined ? "" : ` · ${row.subLabel}`}
        </Pill>
      </td>
      <td>
        <TraceSpark runId={row.runId} live={row.live} armed={armed} />
      </td>
      <td>{row.pr === "" ? "—" : <TicketChip variant="pr">{row.pr}</TicketChip>}</td>
      <td className="up">{row.updated}</td>
    </tr>
  );
}

// The row's trace-sparkline (STUDIO-743; design record §6) — the run's shape in the same phase
// glyphs the run-detail spine draws.
//
// WHY IT IS LAZY, AND THE DEPENDENCY THAT WOULD MAKE IT EAGER. The shape can only be computed from
// the run's transcript, and NOTHING in the worklist's own payload carries it: `IssueRun`
// (`GET /api/v1/history/issues`) is a `RunSummary` plus a lifecycle, a tracker state and an
// assignee — an outcome, a turn count and token totals, no phases. Drawing the whole table
// eagerly therefore means one `GET /api/v1/runs/{id}/transcript` per row, and those are not small:
// over the 400 most recent recorded runs the median transcript is 30KB and the largest 212KB, so a
// 50-row page would pull ~1.5MB and re-parse 50 session logs on the daemon to fill a table cell.
// The ticket's acceptance rules that out, so the fetch waits for the operator to point at ONE row
// (see `SPARK_DWELL_MS`) and is cached under the same query key the run detail uses, which makes
// opening the row afterwards free.
//
// The dependency this leaves open, flagged rather than brute-forced (ticket + design record §5):
// a cheap per-run PHASE SUMMARY on the issue listing — the daemon already reads each transcript to
// serve it — would let every row draw its strip on load with no extra request at all.
function TraceSpark({ runId, live, armed }: { runId: number; live: boolean; armed: boolean }) {
  // `inFlight: false` even for a live run: a worklist strip is a preview, not a stream, and a
  // per-row 1.5s poll of a 30KB transcript is exactly the cost this component exists to avoid. So
  // a live row's strip is a SNAPSHOT taken when it was armed and does not grow as the run does —
  // the playhead says the run is still going, and the run detail is where it is watched.
  const transcript = useTranscript(runId, false, armed && runId > 0);
  const steps = useMemo(
    () => traceSpark(buildTrace(transcript.data?.entries ?? []).phases, live),
    [transcript.data, live],
  );

  // Every state is a labelled `role="img"`, the strip included: the cell's content is glyphs and
  // ellipses either way, so the label is the only thing a screen reader can usefully announce.
  // Persistence off: there is no run row and so no transcript to read one from.
  if (runId <= 0) return <SparkNote label="No stored run" text="—" />;
  if (!armed) return <SparkNote label="Trace — rest here to preview this run" text="···" />;
  if (transcript.isPending) return <SparkNote label="Reading the transcript…" text="···" busy />;
  // A transcript the daemon could not serve, and a run that logged nothing, are both "no shape to
  // show" — never an invented one.
  if (steps.length === 0) {
    return <SparkNote label={transcript.isError ? "Transcript unavailable" : "No trace"} text="—" />;
  }

  const summary = sparkSummary(steps);
  return (
    <span className="spark" role="img" aria-label={summary} title={summary}>
      {steps.map((step) => (
        <span key={step.kind} className={cn("gly", step.kind === "live" && "now")} aria-hidden="true">
          {step.glyph}
        </span>
      ))}
    </span>
  );
}

/** The strip's one-line states — unread, loading, and the two kinds of "nothing to show". */
function SparkNote({ label, text, busy }: { label: string; text: string; busy?: boolean }) {
  return (
    <span className="spark idle" role="img" aria-label={label} title={label} aria-busy={busy}>
      {text}
    </span>
  );
}
