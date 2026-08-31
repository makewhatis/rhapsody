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

// How long to wait before asking /api/v1/version again while it has never answered. See
// `useVersionQuery` for why it asks twice at all.
export const VERSION_RETRY_MS = 2000;

// useVersionQuery reads GET /api/v1/version until it gets ONE answer, and then never again — a
// daemon's build identity and its Teams toggle are both boot-scoped, so the first settled answer is
// final for the session (a mid-session toggle flip still needs the app restart it has always
// needed). It is deliberately a shared query rather than a per-component effect so the footer's
// build stamp and the Teams gate below cost one request between them.
//
// "Until it gets one answer" rather than "once at mount" is STUDIO-665: the packaged app's
// supervisor starts the webview and the daemon together, so the shell routinely fires this request
// seconds before the daemon has bound its port. A single unretried failure used to latch the Teams
// gate off — invisibly, for the whole session, on a perfectly healthy daemon — while every other
// surface polled and recovered from the same race. Note what is being retried: the *version* route,
// which is not a Teams route. A daemon that answers `teams_enabled: false` settles on that first
// response, so a Teams-off app still makes exactly one version request and zero `/api/v1/teams*`
// requests, ever.
export function useVersionQuery() {
  return useQuery<DaemonVersion>({
    queryKey: VERSION_QUERY_KEY,
    queryFn: fetchVersion,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
    retry: false,
    // Terminates on the first success, whatever it says. `data` is set for ANY answer — including
    // one from a daemon too old to carry `teams_enabled` — so an old daemon reads as off and is not
    // polled forever for a field it will never grow.
    refetchInterval: (q) => (q.state.data === undefined ? VERSION_RETRY_MS : false),
  });
}

// useTeamsEnabled is THE gate. Every Teams surface in the app hangs off it, and while it is false
// nothing fetches `/api/v1/teams*` — an app on a Teams-off daemon is byte-for-byte the app before
// STUDIO-652, and it learns that from the one version request it already makes. A daemon too old to
// serve the field reads as off, and so does one that cannot be reached — the latter only until it
// can be, since `useVersionQuery` keeps asking until it has an answer.
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
