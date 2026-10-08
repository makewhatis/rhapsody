import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { fetchNotifications, readNotification } from "@/lib/api";

export const NOTICES_QUERY_KEY = ["notifications"] as const;
// Read state belongs to the daemon. Both app/browser poll the same endpoint and
// mutations invalidate this one cache, not a client-local dismiss list.
export function useNoticeFeed() {
  return useQuery({ queryKey: NOTICES_QUERY_KEY, queryFn: fetchNotifications,
    refetchInterval: 2000, refetchOnWindowFocus: true });
}
export function useReadNotice() {
  const client = useQueryClient();
  return useMutation({ mutationFn: (id: number) => readNotification(id),
    onSuccess: async () => { await client.invalidateQueries({ queryKey: NOTICES_QUERY_KEY }); } });
}
