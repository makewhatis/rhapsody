import { useEffect, useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { LIVE_POLL_MS } from "@/hooks/useStateQuery";
import {
  fetchIssueHistory,
  fetchRunDetail,
  fetchRunIdentityEvents,
  fetchRunMessages,
  fetchRunProvenance,
  fetchRunTranscript,
  type EventHit,
  type IssueHistoryResponse,
  type RunDetail,
  type RunMessage,
  type RunProvenance,
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

// useRunProvenance fetches what a run actually ran on (GET /api/v1/runs/{id}/provenance,
// STUDIO-909). Provenance is written once at dispatch and never changes, so this never polls and
// never goes stale — one fetch per run id, unlike the run detail it sits beside.
export function useRunProvenance(runId: number, enabled = true) {
  return useQuery<RunProvenance>({
    queryKey: ["run-provenance", runId],
    queryFn: () => fetchRunProvenance(runId),
    enabled: enabled && runId > 0,
    staleTime: Infinity,
    refetchOnWindowFocus: false,
  });
}

// useTranscript fetches a run's humanized transcript. While the run is in flight it streams
// (polls @1.5s, never stale); once finished it freezes (no interval, infinite staleTime). On the
// running→finished edge it fires exactly one extra refetch to capture the final lines.
export function useTranscript(
  runId: number,
  inFlight: boolean,
  enabled = true,
  pollMs = 1500,
) {
  const query = useQuery<RunTranscriptResponse>({
    queryKey: ["run-transcript", runId],
    queryFn: () => fetchRunTranscript(runId),
    enabled: enabled && runId > 0,
    refetchInterval: inFlight ? pollMs : false,
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
//
// `refetchInterval` is conditional (STUDIO-1020): the strip paints a still-running review round
// violet "reviewing" from that row alone, and nothing else on the detail refreshes it — the 10s
// `staleTime` with no interval would leave the chip asserting a live review for as long as the
// operator kept the page open, long after the review's own header pill had moved on. So the query
// rides the live cadence while any review round in its payload is still going, and freezes the
// moment none is.
export function useIssueHistory(identifier: string, enabled = true) {
  return useQuery<IssueHistoryResponse>({
    queryKey: ["issue-history", identifier],
    queryFn: () => fetchIssueHistory(identifier),
    enabled: enabled && identifier !== "",
    staleTime: 10_000,
    refetchInterval: (query) => historyPollInterval(query.state.data),
    refetchOnWindowFocus: false,
  });
}

// historyPollInterval encodes the review strip's liveness rule (STUDIO-1020): poll WHILE any review
// round in the payload has not ended, and stop once none has. Keyed on the review's OWN `ended_at`
// — the very fact `reviewState` reads to paint the chip (`lib/console-trace-view`) — rather than on
// `outcome`, so the chip and its refresh can never disagree about which rounds are live. An absent
// payload (and a ticket with no reviews at all) is `false`: nothing is claimed yet, so nothing has
// to be refreshed. Exported for unit testing, in the shape `runDetailPollInterval` set.
export function historyPollInterval(data: IssueHistoryResponse | undefined): number | false {
  const live = (data?.reviews ?? []).some((review) => review.ended_at.trim() === "");
  return live ? LIVE_POLL_MS : false;
}

// useRunIdentityEvents fetches one ticket's durable routing rows (STUDIO-746) — the per-run
// record of who each attempt was dispatched as, which is what keeps a FINISHED run attributed
// once its teammate has dropped off the live roster.
//
// It rides the attempt list's own cadence (`useIssueHistory`, 10s) rather than the run poll's,
// because it answers the same question that list does: which runs exist and whose they were. A
// routing row is written once at dispatch and never rewritten, so nothing here has to keep up
// with a run in flight — only with a ticket that gains one.
export function useRunIdentityEvents(identifier: string, enabled = true) {
  return useQuery<EventHit[]>({
    queryKey: ["run-identities", identifier],
    queryFn: () => fetchRunIdentityEvents(identifier),
    enabled: enabled && identifier !== "",
    staleTime: 10_000,
    refetchOnWindowFocus: false,
  });
}
