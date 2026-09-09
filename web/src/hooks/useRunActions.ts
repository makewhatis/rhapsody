import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchRunMergeability,
  mergeRun,
  resumeRun,
  sendRunMessage,
  stopRun,
  type MergeRunResult,
  type RunActionResult,
  type RunMergeability,
} from "@/lib/api";
import { STATE_QUERY_KEY } from "@/hooks/useStateQuery";

// useStopRun kills the agent for a run and moves its ticket to Backlog, then invalidates the
// live state + this run's detail so the UI reflects the stopped outcome. The detail key mirrors
// useRunDetail's ["run-detail", runId] so the open RunDetailView refetches on settle.
export function useStopRun(runID: number) {
  const qc = useQueryClient();
  return useMutation<RunActionResult, Error>({
    mutationFn: () => stopRun(runID),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: ["run-detail", runID] });
    },
  });
}

// useResumeRun moves a stopped run's ticket back to Todo so the daemon re-dispatches it, then
// invalidates the live state + this run's detail (["run-detail", runId]) so the UI updates.
export function useResumeRun(runID: number) {
  const qc = useQueryClient();
  return useMutation<RunActionResult, Error>({
    mutationFn: () => resumeRun(runID),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: ["run-detail", runID] });
    },
  });
}

// useSendRunMessage queues an operator message for a live run's agent (INF-250), then invalidates
// this run's message list (["run-messages", runId], matching useRunMessages) so the new row shows
// immediately as "sent" without waiting for the next poll tick.
export function useSendRunMessage(runID: number) {
  const qc = useQueryClient();
  return useMutation<{ id: number; identifier: string; status: string }, Error, string>({
    mutationFn: (text: string) => sendRunMessage(runID, text),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: ["run-messages", runID] });
    },
  });
}

// useMergeRun merges a run's pull request (STUDIO-767). The mutation variable is the CONFIRMATION —
// "" for the handshake's first leg, then the receipt's own head SHA — and never a pull request: the
// daemon derives the coordinate from the run row, and the console has no way to name one.
//
// It invalidates the live state and this run's detail on settle, because a merge changes what the
// ticket's next poll will say about it.
export function useMergeRun(runID: number) {
  const qc = useQueryClient();
  return useMutation<MergeRunResult, Error, string>({
    mutationFn: (confirm: string) => mergeRun(runID, confirm),
    onSettled: (data) => {
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: ["run-detail", runID] });
      // The verdict the header renders Merge from is stale once a merge has ACTUALLY been applied
      // — an armed auto-merge makes the next answer "already merged" (STUDIO-790). Only then: the
      // handshake's first leg answers `confirm_required` having merged nothing and changed
      // nothing, so re-resolving on it would spend a tracker read (and, on a review-state ticket,
      // two blocking `gh` calls) to re-derive an answer that cannot have moved — while the
      // operator is reading the modal.
      if (data?.status === "merged") {
        void qc.invalidateQueries({ queryKey: mergeabilityKey(runID) });
      }
    },
  });
}

function mergeabilityKey(runID: number) {
  return ["run-mergeability", runID] as const;
}

// useRunMergeability asks what Merge would do BEFORE it is clicked (STUDIO-790): GET
// /api/v1/runs/{id}/mergeability, which resolves the run's pull request and applies every refusal
// without merging anything. The header renders a live primary from a `mergeable` verdict and a
// reason-bearing disabled control from a refusal, so the daemon's answer is readable without an
// irreversible-looking click.
//
// It does NOT poll. Each read costs the daemon a few bounded `gh` round trips, and the verdict only
// moves on events the console already reacts to — a merge attempt invalidates it above, and
// remounting the run detail refetches it. `enabled` carries the Teams gate: with Teams off the
// daemon serves `teams_disabled` and the header is dependency-named without asking.
export function useRunMergeability(runID: number, enabled: boolean) {
  return useQuery<RunMergeability>({
    queryKey: mergeabilityKey(runID),
    queryFn: () => fetchRunMergeability(runID),
    enabled: enabled && runID > 0,
    refetchOnWindowFocus: false,
    // One failing read must not become a retry storm against `gh`; the header stays usable on a
    // failure anyway, because a question nobody could answer is not a refusal.
    retry: false,
  });
}
