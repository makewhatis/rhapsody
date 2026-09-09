import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { fetchState, postRefresh, type StateResponse } from "@/lib/api";

export const STATE_QUERY_KEY = ["state"] as const;

// LIVE_POLL_MS is the cadence of the live snapshot (design §10.2) and, since STUDIO-791, the cadence
// every query rendered ALONGSIDE that snapshot must share. The Jobs surface merges this snapshot
// with `/api/v1/history/issues` into the one array behind both its header counts and its rows; while
// the second polled at `false` that array had a live half and a half frozen at mount, so the surface
// could report a run's state long after it had changed. See `useJobsFeed` for the whole account.
// Import this rather than typing 2000 again — a second literal is exactly how the two drift apart.
//
// It is a client constant and NOT the daemon's `poll_interval_ms`, which `/api/v1/state` does
// publish and which `RunsView`/`AppShell` already read (their `?? 2000` is a pre-first-snapshot
// fallback, not a hardcoded cadence). Reading it here would be an improvement, but only if this
// query moves with it: a pair held to ONE freshness is the whole point, so pointing the listing at
// the daemon's field while the snapshot beside it stayed on the constant would re-open the very gap
// STUDIO-791 closed. Moving both is a change of its own — see the PR's follow-up note.
export const LIVE_POLL_MS = 2000;

// useStateQuery polls /api/v1/state on the live cadence. It runs in both hosts — a plain browser (the
// daemon's own origin) and the Wails app (which reverse-proxies /api to the sidecar).
// Pass `{ enabled: false }` to suspend the poll on a view that doesn't need the live snapshot.
export function useStateQuery(opts?: { enabled?: boolean }) {
  return useQuery<StateResponse>({
    queryKey: STATE_QUERY_KEY,
    queryFn: fetchState,
    refetchInterval: LIVE_POLL_MS,
    refetchOnWindowFocus: false,
    enabled: opts?.enabled ?? true,
  });
}

// useRefresh POSTs /api/v1/refresh then invalidates the state query.
export function useRefresh() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: postRefresh,
    onSettled: () => qc.invalidateQueries({ queryKey: STATE_QUERY_KEY }),
  });
}
