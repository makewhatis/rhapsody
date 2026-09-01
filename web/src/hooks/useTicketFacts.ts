import { useQueries } from "@tanstack/react-query";
import { fetchTeamsRecall, type TeamsFact } from "@/lib/api";
import { TEAMS_RECALL_QUERY_KEY } from "@/hooks/useTeams";

/**
 * The memory facts a ticket's runs produced (STUDIO-681 §4, "Memory from this ticket").
 *
 * A memory bank is per IDENTITY, and `GET /api/v1/teams/recall` reads one identity at a time —
 * there is no by-ticket read, and inventing one is out of scope (§11). So this browses each
 * roster member's bank once and keeps the records stamped with this ticket. The fan-out is
 * bounded by the roster, which is a handful of teammates, and every request is a query
 * react-query already de-duplicates against the Memory page's own reads.
 *
 * Callers pass an EMPTY roster when Teams is off, which fires no request at all.
 */
export function useTicketFacts(
  roster: readonly string[],
  ticket: string,
): { data: TeamsFact[]; isPending: boolean } {
  const results = useQueries({
    queries: roster.map((identity) => ({
      queryKey: [...TEAMS_RECALL_QUERY_KEY, identity, ""],
      queryFn: () => fetchTeamsRecall(identity, ""),
      refetchOnWindowFocus: false,
      enabled: ticket !== "",
    })),
  });

  const facts = results
    .flatMap((r) => r.data?.facts ?? [])
    .filter((f) => f.ticket === ticket);
  return { data: facts, isPending: results.some((r) => r.isPending) };
}
