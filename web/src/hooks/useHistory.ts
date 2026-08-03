import { keepPreviousData, useQuery } from "@tanstack/react-query";
import {
  fetchDaySummary,
  fetchHistory,
  fetchIssueRuns,
  localDayStartISO,
  type DaySummary,
  type HistoryFilter,
  type HistoryResponse,
  type IssueRunsResponse,
} from "@/lib/api";

// useHistory fetches the RUN-level history (GET /api/v1/history). It keeps the previous page's data
// while refetching to avoid flicker. Note this is a run-paged fetch: it is the right input for a
// per-run view (a single issue's attempts, a run search), and the WRONG input for an issue-grouped
// list or for a total — see useIssueRuns and useDaySummary. (TRA-320)
export function useHistory(
  filter: HistoryFilter = {},
  opts?: { enabled?: boolean; refetchInterval?: number | false },
) {
  return useQuery<HistoryResponse>({
    queryKey: ["history", filter],
    queryFn: () => fetchHistory(filter),
    enabled: opts?.enabled ?? true,
    refetchInterval: opts?.refetchInterval ?? false,
    refetchOnWindowFocus: false,
    placeholderData: keepPreviousData,
  });
}

// useIssueRuns fetches the ISSUE-level listing (GET /api/v1/history/issues) that backs the Jobs
// list: one row per issue, paged by issue. This is what keeps a ticket in a retry loop from
// crowding every other issue off the page — grouping a run-paged fetch client-side cannot, at any
// page size. (TRA-320)
export function useIssueRuns(
  filter: HistoryFilter = {},
  opts?: { enabled?: boolean; refetchInterval?: number | false },
) {
  return useQuery<IssueRunsResponse>({
    queryKey: ["history-issues", filter],
    queryFn: () => fetchIssueRuns(filter),
    enabled: opts?.enabled ?? true,
    refetchInterval: opts?.refetchInterval ?? false,
    refetchOnWindowFocus: false,
    placeholderData: keepPreviousData,
  });
}

// useDaySummary fetches the daemon-computed totals for the local day containing `nowMs`
// (GET /api/v1/history/summary) — the header's runs/tokens/runtime cells. The query key carries the
// day boundary, not the raw `nowMs`, so a 1s ticking clock does not refetch every tick but crossing
// local midnight does re-key onto the new day. (TRA-320)
export function useDaySummary(
  nowMs: number,
  opts?: { enabled?: boolean; refetchInterval?: number | false },
) {
  const since = localDayStartISO(nowMs);
  return useQuery<DaySummary>({
    queryKey: ["history-summary", since],
    queryFn: () => fetchDaySummary(nowMs),
    enabled: opts?.enabled ?? true,
    refetchInterval: opts?.refetchInterval ?? false,
    refetchOnWindowFocus: false,
    placeholderData: keepPreviousData,
  });
}
