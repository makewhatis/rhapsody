import { useQuery } from "@tanstack/react-query";
import { fetchSystem } from "@/lib/api";

// The scratch root's usage is a filesystem walk, so this polls on a slow cadence rather than the
// live-snapshot 2s one.
export const SYSTEM_POLL_MS = 30_000;

export function useSystem() {
  return useQuery({ queryKey: ["system"], queryFn: fetchSystem, refetchInterval: SYSTEM_POLL_MS, refetchOnWindowFocus: false });
}
