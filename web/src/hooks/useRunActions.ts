import { useMutation, useQueryClient } from "@tanstack/react-query";
import { resumeRun, sendRunMessage, stopRun, type RunActionResult } from "@/lib/api";
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
