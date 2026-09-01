import { useQueries } from "@tanstack/react-query";
import { fetchTeamsRecall } from "@/lib/api";
import { TEAMS_RECALL_QUERY_KEY } from "@/hooks/useTeams";
import type { MemoryBank } from "@/lib/memory-model";

/**
 * Every roster member's bank, for the Memory page (STUDIO-681 §6).
 *
 * `GET /api/v1/teams/recall` reads ONE identity at a time and an empty query browses that bank —
 * there is no all-banks read, and inventing one is out of scope (§11). So this fans out over the
 * roster, exactly as `useTicketFacts` does for the Job-detail card, and shares its query keys, so
 * the two surfaces de-duplicate against each other instead of asking twice.
 *
 * An empty roster fires no request at all: a solo daemon has no bank to browse.
 */
export function useMemoryBanks(roster: readonly string[]): {
  banks: MemoryBank[];
  isPending: boolean;
  /** The first bank that could not be read; the rest of the page still renders. */
  error: unknown;
} {
  const results = useQueries({
    queries: roster.map((identity) => ({
      queryKey: [...TEAMS_RECALL_QUERY_KEY, identity, ""],
      queryFn: () => fetchTeamsRecall(identity, ""),
      refetchOnWindowFocus: false,
    })),
  });

  // A bank that failed contributes an EMPTY bank rather than dropping out of the roster: the
  // "banks" stat counts what the page is looking at, and a bank silently missing from that count
  // would read as a teammate who remembers nothing.
  const banks = roster.map((identity, i) => ({
    identity,
    facts: results[i]?.data?.facts ?? [],
    skipped: results[i]?.data?.skipped ?? [],
  }));

  return {
    banks,
    isPending: results.some((r) => r.isPending),
    error: results.find((r) => r.isError)?.error,
  };
}
