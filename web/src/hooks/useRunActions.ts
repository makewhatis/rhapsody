import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  mergeRun,
  resumeRun,
  sendRunMessage,
  stopRun,
  type MergeRunResult,
  type RunActionResult,
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
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: ["run-detail", runID] });
    },
  });
}
