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

// The prefix every issue-listing query shares, whatever its filter. Exported so a caller that has
// just learned the listing is out of date — `useJobsFeed`, off the live snapshot — can invalidate
// the whole family without reconstructing each filter's key.
export const HISTORY_ISSUES_QUERY_KEY = ["history-issues"] as const;

// useIssueRuns fetches the ISSUE-level listing (GET /api/v1/history/issues) that backs the Jobs
// list: one row per issue, paged by issue. This is what keeps a ticket in a retry loop from
// crowding every other issue off the page — grouping a run-paged fetch client-side cannot, at any
// page size. (TRA-320)
//
// `refetchInterval` still defaults to `false`, because a caller that renders the listing on its own
// (a search, a one-shot page) genuinely wants one fetch. A caller that renders it as ROWS beside the
// live snapshot does not have that freedom: the two feed one merged array, so a half frozen at mount
// makes the surface report a run's state from whenever the page was opened. `useJobsFeed` is where
// that pairing is made, rather than left to each call site (STUDIO-791).
//
// `ConsoleApp`'s Jobs nav badge unions the same two sources and is deliberately NOT on that pairing:
// it counts every issue in the listing page rather than open work, so its number is wrong by an
// amount no cadence can fix, and polling it would put this endpoint on a 2s timer on every console
// route to keep that number fresh. It refetches for free while `JobsView` is mounted — same query
// key — and its real defect is a follow-up of its own.
export function useIssueRuns(
  filter: HistoryFilter = {},
  opts?: { enabled?: boolean; refetchInterval?: number | false },
) {
  return useQuery<IssueRunsResponse>({
    queryKey: [...HISTORY_ISSUES_QUERY_KEY, filter],
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
