import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { fetchHistory, type HistoryFilter, type HistoryResponse } from "@/lib/api";

// useHistory fetches the run history (GET /api/v1/history) for the merged jobs list. It keeps the
// previous page's data while refetching to avoid flicker. The Runs view drives it on the same
// cadence as the live state poll (via `refetchInterval`) so finished runs surface in the list.
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
