import { useEffect, useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  fetchIssueHistory,
  fetchRunDetail,
  fetchRunMessages,
  fetchRunTranscript,
  type IssueHistoryResponse,
  type RunDetail,
  type RunMessage,
  type RunTranscriptResponse,
} from "@/lib/api";

// runDetailPollInterval encodes the run-detail polling rule: poll every 2s WHILE the run is
// running, then freeze. It is keyed on `outcome === "running"` (NOT `live`) so a run that has
// dropped out of the live snapshot but is still running in the store keeps polling until its
// terminal outcome lands. Exported for unit testing.
export function runDetailPollInterval(data: RunDetail | undefined): number | false {
  return data?.outcome === "running" ? 2000 : false;
}

// useRunDetail fetches one run's unified live-or-finished detail (GET /api/v1/runs/{id}). The
// payload is the live-snapshot-first unification (the daemon merges the live snapshot + store);
// the client just polls while it is running and goes static once terminal. Keyed by run id so a
// run renders identically across the live→finished transition with no re-key.
export function useRunDetail(runId: number, enabled = true) {
  return useQuery<RunDetail>({
    queryKey: ["run-detail", runId],
    queryFn: () => fetchRunDetail(runId),
    enabled: enabled && runId > 0,
    refetchInterval: (query) => runDetailPollInterval(query.state.data),
    refetchOnWindowFocus: false,
  });
}

// useTranscript fetches a run's humanized transcript. While the run is in flight it streams
// (polls @1.5s, never stale); once finished it freezes (no interval, infinite staleTime). On the
// running→finished edge it fires exactly one extra refetch to capture the final lines.
export function useTranscript(runId: number, inFlight: boolean, enabled = true) {
  const query = useQuery<RunTranscriptResponse>({
    queryKey: ["run-transcript", runId],
    queryFn: () => fetchRunTranscript(runId),
    enabled: enabled && runId > 0,
    refetchInterval: inFlight ? 1500 : false,
    staleTime: inFlight ? 0 : Infinity,
    refetchOnWindowFocus: false,
  });

  const wasInFlight = useRef(inFlight);
  const refetchRef = useRef(query.refetch);
  refetchRef.current = query.refetch;
  useEffect(() => {
    if (wasInFlight.current && !inFlight && runId > 0) {
      void refetchRef.current();
    }
    wasInFlight.current = inFlight;
  }, [inFlight, runId]);

  return query;
}

// useRunMessages fetches a run's operator messages (GET /api/v1/runs/{id}/messages). It piggybacks
// the in-flight cadence (polls @2s while running so a sent→delivered chip flip shows promptly) and
// freezes once terminal, firing one final refetch on the running→finished edge to capture any
// last-moment delivery/expiry — mirrors useTranscript (INF-250).
export function useRunMessages(runId: number, inFlight: boolean, enabled = true) {
  const query = useQuery<RunMessage[]>({
    queryKey: ["run-messages", runId],
    queryFn: () => fetchRunMessages(runId),
    enabled: enabled && runId > 0,
    refetchInterval: inFlight ? 2000 : false,
    staleTime: inFlight ? 0 : Infinity,
    refetchOnWindowFocus: false,
  });

  const wasInFlight = useRef(inFlight);
  const refetchRef = useRef(query.refetch);
  refetchRef.current = query.refetch;
  useEffect(() => {
    if (wasInFlight.current && !inFlight && runId > 0) {
      void refetchRef.current();
    }
    wasInFlight.current = inFlight;
  }, [inFlight, runId]);

  return query;
}

// useIssueHistory fetches a run's per-attempt history (GET /api/v1/issues/{id}/history) for the
// Run history panel. Disabled until an identifier is known.
export function useIssueHistory(identifier: string, enabled = true) {
  return useQuery<IssueHistoryResponse>({
    queryKey: ["issue-history", identifier],
    queryFn: () => fetchIssueHistory(identifier),
    enabled: enabled && identifier !== "",
    staleTime: 10_000,
    refetchOnWindowFocus: false,
  });
}
