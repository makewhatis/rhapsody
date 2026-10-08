import { useQuery } from "@tanstack/react-query";
import { fetchAccounts } from "@/lib/api";
import { LIVE_POLL_MS } from "./useStateQuery";

export function useAccounts() {
  return useQuery({ queryKey: ["accounts"], queryFn: fetchAccounts, refetchInterval: LIVE_POLL_MS, refetchOnWindowFocus: false });
}
