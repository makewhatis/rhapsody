import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { HISTORY_ISSUES_QUERY_KEY } from "@/hooks/useHistory";
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

// useRefresh POSTs /api/v1/refresh then invalidates the state query AND the issue listing.
//
// The listing half is not decoration (STUDIO-792). Its only caller is the Jobs worklist, whose ↻ is
// the operator asking for the current truth about the surface in front of them — and that surface is
// rows as much as it is the strip. It used to be moot: the listing polled every 2s, so refreshing
// only the snapshot still left the rows no more than a tick behind. Once a WIDENED window comes off
// that timer (see `useJobsFeed`), invalidating the snapshot alone would leave the button visibly
// inert for the rows unless the refresh happened to change the live set — a control that reports
// less than it appears to, which is the failure this ticket exists to remove.
//
// By prefix, so the widened page and the rail badge's `{}` entry are both covered.
export function useRefresh() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: postRefresh,
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: HISTORY_ISSUES_QUERY_KEY });
    },
  });
}
