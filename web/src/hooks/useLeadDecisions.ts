import { useQuery } from "@tanstack/react-query";
import { fetchLeadDecisions } from "@/lib/api";
import { useVersionQuery } from "@/hooks/useTeams";

export const LEAD_DECISIONS_QUERY_KEY = ["lead-decisions"] as const;

// Capability-gated full paper trail. Compact notices/read state have their own daemon API.
export function useLeadDecisions() {
  const version = useVersionQuery().data;
  const enabled = version?.teams_enabled === true && version.lead_enabled === true;
  const query = useQuery({
    queryKey: LEAD_DECISIONS_QUERY_KEY,
    queryFn: () => fetchLeadDecisions(),
    enabled,
    refetchInterval: 5000,
    refetchOnWindowFocus: false,
  });
  return { ...query, enabled };
}
