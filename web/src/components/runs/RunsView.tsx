import * as React from "react";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useHistory } from "@/hooks/useHistory";
import { useLinearProjects } from "@/hooks/useConfig";
import { useNow } from "@/hooks/useNow";
import { mergeJobs } from "@/lib/runs-model";
import { RunsStatTiles } from "./StatTiles";
import { JobsList } from "./JobsList";
import { RunDetail } from "./RunDetail";

// RunsView — the re-skinned Runs dashboard: 4 stat tiles + the unified Live+History jobs list,
// drilling into a run detail. Mounted into the app shell's "runs" tab. Reuses the existing
// run/history/metrics APIs — no new endpoints.
//
// The HTTP queries poll the daemon over /api regardless of host: in a plain browser the daemon
// serves this UI from its own origin; under the Wails app the AssetServer reverse-proxies /api to
// the sidecar (desktop apiProxyMiddleware), so the relative fetches reach the daemon either way.
export function RunsView() {
  const { data: state } = useStateQuery();
  const pollMs = state?.poll_interval_ms;
  const history = useHistory({}, { refetchInterval: pollMs ?? 2000 });
  const projects = useLinearProjects().data ?? [];
  const nowMs = useNow(1000);
  const [openRunId, setOpenRunId] = React.useState<number | null>(null);

  if (openRunId != null) {
    return (
      <RunDetail
        runId={openRunId}
        projects={projects}
        enabled
        onBack={() => setOpenRunId(null)}
        onSelectRun={setOpenRunId}
      />
    );
  }

  const historyRuns = history.data?.runs ?? [];
  const rows = mergeJobs(state, historyRuns, projects, nowMs);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 22, minWidth: 0 }}>
      <RunsStatTiles state={state} history={historyRuns} nowMs={nowMs} live />
      <JobsList rows={rows} pollMs={pollMs} polling onSelect={setOpenRunId} />
    </div>
  );
}
