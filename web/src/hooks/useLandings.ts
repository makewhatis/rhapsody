import { useQuery } from "@tanstack/react-query";
import { fetchLandings } from "@/lib/api";

export function useLandings() {
  return useQuery({ queryKey: ["landings"], queryFn: fetchLandings, refetchInterval: 5000, refetchOnWindowFocus: false });
}
