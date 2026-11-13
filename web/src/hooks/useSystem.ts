import { useQuery } from "@tanstack/react-query";
import { fetchSystem } from "@/lib/api";
import { LIVE_POLL_MS } from "./useStateQuery";

export function useSystem() {
  return useQuery({ queryKey: ["system"], queryFn: fetchSystem, refetchInterval: LIVE_POLL_MS, refetchOnWindowFocus: false });
}
