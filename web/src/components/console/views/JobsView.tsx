import { useMemo, useState } from "react";
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
import { mergeJobs } from "@/lib/runs-model";
import { useLinearProjects } from "@/hooks/useConfig";
import { useIssueRuns } from "@/hooks/useHistory";
import { useNow } from "@/hooks/useNow";
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
  return (
    <tr
      tabIndex={0}
      role="link"
      aria-label={`${row.issue} ${row.title}`}
      onClick={() => onOpen(row.issue)}
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
      <td>{row.pr === "" ? "—" : <TicketChip variant="pr">{row.pr}</TicketChip>}</td>
      <td className="up">{row.updated}</td>
    </tr>
  );
}
