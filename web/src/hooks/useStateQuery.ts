import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { fetchState, postRefresh, type StateResponse } from "@/lib/api";

export const STATE_QUERY_KEY = ["state"] as const;

// useStateQuery polls /api/v1/state every 2000 ms (design §10.2). It runs in both hosts — a plain
// browser (the daemon's own origin) and the Wails app (which reverse-proxies /api to the sidecar).
// Pass `{ enabled: false }` to suspend the poll on a view that doesn't need the live snapshot.
export function useStateQuery(opts?: { enabled?: boolean }) {
  return useQuery<StateResponse>({
    queryKey: STATE_QUERY_KEY,
    queryFn: fetchState,
    refetchInterval: 2000,
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
