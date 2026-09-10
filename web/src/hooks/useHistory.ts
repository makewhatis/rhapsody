import { keepPreviousData, useQuery } from "@tanstack/react-query";
import {
  fetchDaySummary,
  fetchHistory,
  fetchIssueCounts,
  fetchIssueRuns,
  localDayStartISO,
  type DaySummary,
  type HistoryFilter,
  type HistoryResponse,
  type IssueCountsResponse,
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
// route to keep that number fresh. It refetches for free while `JobsView` is mounted on the DEFAULT
// window — same `{}` key. Once the operator widens the worklist's page (STUDIO-792) `JobsView` moves
// to a key of its own, and the badge's refresh comes instead from `useJobsFeed`'s live-snapshot
// pull-forward, which invalidates this whole family by prefix rather than one filter. Its real
// defect — counting page rows rather than open work — is a follow-up of its own either way.
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

// TRACKER_POLL_MS is the cadence for a read whose freshness is the TRACKER's rather than the
// daemon's, and which is too expensive to put on the live one.
//
// The number is not a taste. The daemon resolves a ticket's lifecycle through a 60s TTL memo shared
// by the whole process (`LIFECYCLE_TTL`, crates/orchestrator/src/lifecycle.rs), so a ticket moved in
// Linear with no run involved cannot become visible to ANY console faster than that window. Asking
// more often than the answer can change buys nothing, and matching the two makes the bound statable:
// such a change reaches the console within one TTL plus one interval — under two minutes worst case,
// about one typically.
//
// What it costs a daemon with several consoles open is worth being exact about, because the obvious
// guess is wrong. The TRACKER cost is bounded by that memo and NOT by this interval or by the number
// of consoles: N consoles polling at any rate share one refresh per TTL window. What scales with
// polls is the daemon's own SQL, measured at ~1ms for the whole-store issue query over the
// operator's own database (600 runs, 425 issues) — which is why the Now strip's tally is on
// LIVE_POLL_MS instead, and why only the WIDENED issue listing, a 284KB response, is on this one.
export const TRACKER_POLL_MS = 60_000;

// The issue-listing TALLY's query key (STUDIO-828), kept beside the listing's own prefix because
// the two answer the same question at different widths and are invalidated together.
export const HISTORY_ISSUE_COUNTS_QUERY_KEY = ["history-issue-counts"] as const;

// useIssueCounts fetches the daemon-computed per-status tally over EVERY issue in the store
// (GET /api/v1/history/issues/counts) — the Now strip's numbers.
//
// The strip used to fold them out of the rows the table happened to hold, so they grew as the
// operator paged and were capped by the window (STUDIO-828 defect A). This is the same correction
// TRA-320 made to the header's day totals and for the same reason: a total is never a page.
//
// It takes no filter, so it is one cache entry per console however wide the worklist's window is —
// widening the page changes what the TABLE shows and must not change what the strip counts, which
// is the property the whole endpoint exists to hold.
//
// `useJobsFeed` runs it on LIVE_POLL_MS, with the rest of the surface. It can afford that where the
// widened listing cannot: the response is O(1) in the store's size, the query behind it measures
// ~1ms on the operator's own database, and its tracker cost is pinned to the daemon's 60s lifecycle
// memo however often it is asked. Anything slower would let the strip and the rows beside it
// disagree for the length of the gap, which is the failure this ticket is about.
export function useIssueCounts(opts?: { enabled?: boolean; refetchInterval?: number | false }) {
  return useQuery<IssueCountsResponse>({
    queryKey: HISTORY_ISSUE_COUNTS_QUERY_KEY,
    queryFn: fetchIssueCounts,
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
