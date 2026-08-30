import * as React from "react";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useDaySummary, useIssueRuns } from "@/hooks/useHistory";
import { useLinearProjects, useTypedConfigQuery } from "@/hooks/useConfig";
import { useNow } from "@/hooks/useNow";
import { mergeJobs } from "@/lib/runs-model";
import { RunsStatTiles } from "./StatTiles";
import { JobsList } from "./JobsList";
import { RunDetail } from "./RunDetail";

// RunsView — the Podium Jobs view (mock 1a): the full-bleed instrument strip over the dense jobs
// table, drilling into a run detail. Mounted full-width by the app shell (the strip + footer are
// edge-to-edge bands). Reuses the existing run/history/config APIs — no new endpoints.
//
// The HTTP queries poll the daemon over /api regardless of host: in a plain browser the daemon
// serves this UI from its own origin; under the Wails app the AssetServer reverse-proxies /api to
// the sidecar (desktop apiProxyMiddleware), so the relative fetches reach the daemon either way.
// Run-detail selection is normally RunsView's own state. The shell may lift it (STUDIO-652) so the
// Teams panel can open a teammate's live run: pass both props to control it, or neither to keep the
// self-contained behaviour every existing caller relies on.
export interface RunsViewProps {
  openRunId?: number | null;
  onOpenRun?: (runID: number | null) => void;
}

export function RunsView({ openRunId: controlledRunId, onOpenRun }: RunsViewProps = {}) {
  const { data: state } = useStateQuery();
  const pollMs = state?.poll_interval_ms;
  // The Jobs list reads the ISSUE-level listing and the header reads the daemon's day totals — two
  // queries with two jobs, because one issue-grouped list and one set of totals cannot both be
  // derived correctly from a single run-paged fetch (TRA-320).
  const issues = useIssueRuns({}, { refetchInterval: pollMs ?? 2000 });
  const projects = useLinearProjects().data ?? [];
  // Seat capacity ("of N seats") + the "/max" turn cell come from the resolved daemon config; both
  // fall back to omitted (no seat annotation / bare turn count) while the config is still loading.
  const config = useTypedConfigQuery().data;
  // `global` is absent when the on-disk config fails to parse — fall back to omitted seat/turn hints.
  const maxConcurrent = config?.global?.agent.max_concurrent_agents ?? 0;
  const maxTurns = config?.global?.agent.max_turns;
  const nowMs = useNow(1000);
  const summary = useDaySummary(nowMs, { refetchInterval: pollMs ?? 2000 });
  const [ownRunId, setOwnRunId] = React.useState<number | null>(null);
  const openRunId = controlledRunId !== undefined ? controlledRunId : ownRunId;
  const setOpenRunId: (id: number | null) => void = onOpenRun ?? setOwnRunId;

  if (openRunId != null) {
    // Run detail renders full-bleed too (mock 1d): the header, the edge-to-edge meta strip, and the
    // transcript card are bands like the Jobs list — not the old centred container. (P10-D4)
    return (
      // Keyed by run id so switching attempts (from the run-history panel) mounts a fresh detail with
      // clean follow/confirm state. The id is unchanged across a run's own live→finished transition,
      // so that transition still renders in place with no re-key (matching useRunDetail's keying).
      <RunDetail
        key={openRunId}
        runId={openRunId}
        projects={projects}
        maxTurns={maxTurns}
        enabled
        onBack={() => setOpenRunId(null)}
        onSelectRun={setOpenRunId}
      />
    );
  }

  const issueRuns = issues.data?.issues ?? [];
  const rows = mergeJobs(state, issueRuns, projects, nowMs);

  return (
    <div style={{ minWidth: 0 }}>
      <RunsStatTiles
        state={state}
        summary={summary.data}
        rows={issueRuns}
        maxConcurrent={maxConcurrent}
        live
      />
      <JobsList rows={rows} pollMs={pollMs} maxTurns={maxTurns} polling onSelect={setOpenRunId} />
    </div>
  );
}
