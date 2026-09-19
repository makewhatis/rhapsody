import { keepPreviousData, useQueries } from "@tanstack/react-query";
import { fetchIssueRuns, type IssueRun } from "@/lib/api";
import { BOARD_ACTIVE_LIMIT, BOARD_ACTIVE_OUTCOMES } from "@/lib/console-board";
import { HISTORY_ISSUES_QUERY_KEY, TRACKER_POLL_MS } from "@/hooks/useHistory";

/**
 * The board's complete non-terminal feed (STUDIO-931).
 *
 * The board regroups the issue listing into lanes, and the listing is ONE RECENCY PAGE — so
 * non-terminal work, which is by definition the oldest and quietest, is the first thing to fall off
 * it. This hook fetches the outcomes that can only be non-terminal, each by filter and each far
 * wider than a page, so the board's Queued and Running lanes are complete rather than a sample of
 * whatever the newest 50 rows happened to contain.
 *
 * WHY ONE QUERY PER OUTCOME. `/api/v1/history/issues` filters `outcome` as an exact match (see the
 * store's `run_filter_where`), so there is no single request that means "everything still active".
 * `completed` is deliberately NOT among them — it covers Done as well as In Review, and lifecycle
 * is not a filter — see `BOARD_ACTIVE_OUTCOMES`.
 *
 * WHY IT RIDES THE SLOW CADENCE. These are widened requests: the codebase already measured a
 * full-width listing at 0.63-0.88s warm and 3.63s cold against 1.6ms at the default width
 * (`useJobsFeed`), which is why a widened window is not polled on the live 2s cadence. The same
 * trade applies here, so the active read rides `TRACKER_POLL_MS` — and `useJobsFeed`'s live-snapshot
 * pull-forward still invalidates this whole query family the moment the live set changes, because
 * its keys share the `HISTORY_ISSUES_QUERY_KEY` prefix.
 *
 * `enabled` is `false` while the table, not the board, is on screen: a list-only visit must not pay
 * for five extra requests it never renders, and react-query keeps the last answer cached so
 * toggling back is instant.
 */
export function useBoardActive(enabled: boolean): IssueRun[] {
  return useQueries({
    queries: BOARD_ACTIVE_OUTCOMES.map((outcome) => ({
      queryKey: [...HISTORY_ISSUES_QUERY_KEY, { outcome, limit: BOARD_ACTIVE_LIMIT }],
      queryFn: () => fetchIssueRuns({ outcome, limit: BOARD_ACTIVE_LIMIT }),
      enabled,
      refetchInterval: enabled ? TRACKER_POLL_MS : false,
      refetchOnWindowFocus: false,
      placeholderData: keepPreviousData,
    })),
    combine: (results) => {
      const rows: IssueRun[] = [];
      for (const r of results) rows.push(...(r.data?.issues ?? []));
      return rows;
    },
  });
}
