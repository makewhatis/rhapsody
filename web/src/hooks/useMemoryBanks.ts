import { useQueries } from "@tanstack/react-query";
import { fetchTeamsRecall } from "@/lib/api";
import { TEAMS_RECALL_QUERY_KEY } from "@/hooks/useTeams";
import type { MemoryBank } from "@/lib/memory-model";

/**
 * The record states this browse asks for. `all` rather than the default `valid`, so the page shows
 * the bank as it is on disk — corrections included — and can offer to undo one it did not make.
 *
 * It is part of the query key as well as the request: a valid-only read and an all-states read of
 * the same bank are different answers and must not share a cache entry.
 */
const BROWSE_STATE = "all";

/**
 * Every roster member's bank, for the Memory page (STUDIO-681 §6).
 *
 * `GET /api/v1/teams/recall` reads ONE identity at a time and an empty query browses that bank —
 * there is no all-banks read, and inventing one is out of scope (§11). So this fans out over the
 * roster, exactly as `useTicketFacts` does for the Job-detail card, under the same query-key
 * prefix, so one `invalidateQueries` refreshes both surfaces.
 *
 * The browse asks for `state=all` (STUDIO-689), which is what makes the Invalidated filter and the
 * invalidated stat mean the BANK rather than this session: recall serves valid records only by
 * default, so a correction made before this page was opened would otherwise be invisible. That is
 * also why this no longer shares a cache ENTRY with `useTicketFacts`, which stays valid-only: two
 * different answers about the same bank must not be one entry, and the Job-detail card would
 * otherwise start rendering corrected facts.
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
      queryKey: [...TEAMS_RECALL_QUERY_KEY, identity, "", BROWSE_STATE],
      queryFn: () => fetchTeamsRecall(identity, "", BROWSE_STATE),
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
