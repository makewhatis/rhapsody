import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchTeamsConfig,
  fetchTeamsOverview,
  fetchTeamsRecall,
  fetchTeamsRoom,
  fetchVersion,
  postTeamsInvalidate,
  postTeamsRoom,
  saveTeamsConfig,
  type DaemonVersion,
  type TeamsConfig,
  type TeamsConfigView,
  type TeamsOverview,
  type TeamsRecallResponse,
  type TeamsRoomPost,
  type TeamsRoomResponse,
} from "@/lib/api";

export const VERSION_QUERY_KEY = ["version"] as const;
export const TEAMS_QUERY_KEY = ["teams", "overview"] as const;
export const TEAMS_ROOM_QUERY_KEY = ["teams", "room"] as const;
export const TEAMS_RECALL_QUERY_KEY = ["teams", "recall"] as const;
export const TEAMS_CONFIG_QUERY_KEY = ["teams", "config"] as const;

// useVersionQuery reads GET /api/v1/version ONCE (no polling: a daemon's build identity and its
// Teams toggle are both boot-scoped). It is deliberately a shared query rather than a per-component
// effect so the footer's build stamp and the Teams gate below cost exactly one request between
// them.
export function useVersionQuery() {
  return useQuery<DaemonVersion>({
    queryKey: VERSION_QUERY_KEY,
    queryFn: fetchVersion,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
    retry: false,
  });
}

// useTeamsEnabled is THE gate. Every Teams surface in the app hangs off it, and while it is false
// nothing fetches `/api/v1/teams*` — an app on a Teams-off daemon is byte-for-byte the app before
// this ticket, and it learns that from the one version request it already makes at mount. A daemon
// too old to serve the field, or one that cannot be reached at all, reads as off.
export function useTeamsEnabled(): boolean {
  return useVersionQuery().data?.teams_enabled === true;
}

// useTeamsOverview polls the roster while the panel is open, so a teammate picking up a ticket
// shows up without a reload. `enabled` gates it on the Teams toggle: this must never fire on a
// daemon that would answer `teams_disabled`.
export function useTeamsOverview(enabled: boolean, pollMs?: number) {
  return useQuery<TeamsOverview>({
    queryKey: TEAMS_QUERY_KEY,
    queryFn: fetchTeamsOverview,
    refetchInterval: pollMs ?? 5000,
    refetchOnWindowFocus: false,
    enabled,
  });
}

// useTeamsRoom tails the room. Reading advances no identity's cursor (the daemon's guarantee), so
// polling here can never eat a teammate's catch-up.
export function useTeamsRoom(enabled: boolean, limit?: number, pollMs?: number) {
  return useQuery<TeamsRoomResponse>({
    queryKey: [...TEAMS_ROOM_QUERY_KEY, limit ?? 0],
    queryFn: () => fetchTeamsRoom(limit),
    refetchInterval: pollMs ?? 5000,
    refetchOnWindowFocus: false,
    enabled,
  });
}

// usePostToRoom is the operator's own voice in the room (STUDIO-661). On success it invalidates
// the room query so the new post appears in the tail immediately, with no reload — the same
// round-trip shape `useInvalidateFact` uses, for the same reason: the write and what the operator
// sees next must not need a refresh to agree.
export function usePostToRoom() {
  const qc = useQueryClient();
  return useMutation<TeamsRoomPost, Error, { body: string; refs: string[] }>({
    mutationFn: (v) => postTeamsRoom(v.body, v.refs),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: TEAMS_ROOM_QUERY_KEY });
    },
  });
}

// useTeamsRecall lists what one identity remembers. An empty `query` is a browse — "everything,
// bounded" — which is the listing the invalidate button acts on. Not polled: memory changes when a
// run retains or an operator invalidates, and both of those already refresh it.
export function useTeamsRecall(identity: string, query: string, enabled: boolean) {
  return useQuery<TeamsRecallResponse>({
    queryKey: [...TEAMS_RECALL_QUERY_KEY, identity, query],
    queryFn: () => fetchTeamsRecall(identity, query),
    refetchOnWindowFocus: false,
    enabled: enabled && identity !== "",
  });
}

// useInvalidateFact marks one record non-valid with its reason. On success it invalidates the
// recall cache so the fact leaves the listing immediately — the round-trip design §5.2.3 asks the
// button to close, with no reload.
export function useInvalidateFact() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (v: { identity: string; factID: string; reason: string }) =>
      postTeamsInvalidate(v.identity, v.factID, v.reason),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: TEAMS_RECALL_QUERY_KEY });
    },
  });
}

// useTeamsConfigQuery reads teams.yaml for the Settings enable flow. Unlike every other hook here
// it is NOT gated on Teams being enabled: off is the only state anyone would open it from. It is
// fetched on demand — opening the Settings → Teams tab — so the dashboard itself still makes no
// Teams request while the feature is off.
export function useTeamsConfigQuery(enabled = true) {
  return useQuery<TeamsConfigView>({
    queryKey: TEAMS_CONFIG_QUERY_KEY,
    queryFn: fetchTeamsConfig,
    refetchOnWindowFocus: false,
    retry: false,
    enabled,
  });
}

// useSaveTeamsConfig writes teams.yaml. The daemon validates first and writes nothing on a
// rejection, so a failure here leaves the on-disk file exactly as it was and the thrown message is
// the daemon's own complaint, verbatim.
export function useSaveTeamsConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (config: TeamsConfig) => saveTeamsConfig(config),
    onSuccess: (saved) => {
      qc.setQueryData(TEAMS_CONFIG_QUERY_KEY, saved);
      // The daemon is boot-loaded, so nothing live changes yet — but the gate and the panel should
      // re-read as soon as it restarts, and dropping the cached copies now is the cheapest way to
      // guarantee they do rather than serving a stale "Teams is off".
      void qc.invalidateQueries({ queryKey: VERSION_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: TEAMS_QUERY_KEY });
    },
  });
}
