import { useQuery } from "@tanstack/react-query";
import { fetchRunDiff, type RunDiffResult } from "@/lib/api";

// useRunDiff reads the diff a run produced on its branch (STUDIO-749): GET /api/v1/runs/{id}/diff.
//
// It does NOT poll, and it is fetched only while the Diff tab is the selected one — the rail mounts
// only the panel being read, which is what keeps its cost honest. Each read costs the daemon four
// bounded `gh` round trips, every one of them blocking on the request's own task, so a background
// poll of this would be four GitHub calls per operator per tick for a surface nobody is looking at.
//
// What it gives up by not polling is a diff that grows under the operator's eyes as the agent
// pushes. That is the right trade here and it is not silent: remounting the run detail refetches,
// and the panel carries the head SHA the diff is OF, so a stale one is identifiable rather than
// merely old.
export function useRunDiff(runID: number) {
  return useQuery<RunDiffResult>({
    queryKey: ["run-diff", runID],
    queryFn: () => fetchRunDiff(runID),
    // No `enabled` flag beyond a real run id: MOUNTING is the gate, and it is a truer one than a
    // boolean, because the rail renders only the selected panel. A caller passing `true` here
    // would read as though there were a second condition when there is not.
    enabled: runID > 0,
    refetchOnWindowFocus: false,
    // One failing read must not become a retry storm against `gh`. The panel stays readable on a
    // failure anyway: it says the daemon could not be asked, which is a different statement from
    // the daemon saying there is no diff.
    retry: false,
  });
}
