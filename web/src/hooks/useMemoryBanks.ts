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
 * Every roster member's bank — plus the SHARED team bank when one is configured (STUDIO-1040) — for
 * the Memory page (STUDIO-681 §6).
 *
 * `GET /api/v1/teams/recall` reads ONE bank at a time and an empty query browses it — there is no
 * all-banks read, and inventing one is out of scope (§11). So this fans out over the roster, and
 * when `teams.team_bank` is set it adds one more read with `scope=team`, which names no identity
 * (`GET /api/v1/teams` carries the shared bank id). The team bank is reported with `scope: "team"`
 * so the page can send a correction to the shared bank rather than to the author's own.
 *
 * The browse asks for `state=all` (STUDIO-689), which is what makes the Invalidated filter and the
 * invalidated stat mean the BANK rather than this session: recall serves valid records only by
 * default, so a correction made before this page was opened would otherwise be invisible.
 *
 * An empty roster with no team bank fires no request at all: a solo daemon has no bank to browse.
 */
export function useMemoryBanks(
  roster: readonly string[],
  teamBank = "",
): {
  banks: MemoryBank[];
  isPending: boolean;
  /** The first bank that could not be read; the rest of the page still renders. */
  error: unknown;
} {
  const hasTeam = teamBank !== "";
  const results = useQueries({
    queries: [
      ...roster.map((identity) => ({
        queryKey: [...TEAMS_RECALL_QUERY_KEY, identity, "", BROWSE_STATE],
        queryFn: () => fetchTeamsRecall(identity, "", BROWSE_STATE),
        refetchOnWindowFocus: false,
      })),
      ...(hasTeam
        ? [
            {
              queryKey: [...TEAMS_RECALL_QUERY_KEY, teamBank, "", BROWSE_STATE, "team"],
              queryFn: () => fetchTeamsRecall("", "", BROWSE_STATE, "team"),
              refetchOnWindowFocus: false,
            },
          ]
        : []),
    ],
  });

  // A bank that failed contributes an EMPTY bank rather than dropping out of the roster: the
  // "banks" stat counts what the page is looking at, and a bank silently missing from that count
  // would read as a teammate who remembers nothing.
  const banks: MemoryBank[] = roster.map((identity, i) => ({
    identity,
    facts: results[i]?.data?.facts ?? [],
    skipped: results[i]?.data?.skipped ?? [],
  }));
  if (hasTeam) {
    const team = results[roster.length];
    banks.push({
      identity: teamBank,
      scope: "team",
      facts: team?.data?.facts ?? [],
      skipped: team?.data?.skipped ?? [],
    });
  }

  return {
    banks,
    isPending: results.some((r) => r.isPending),
    error: results.find((r) => r.isError)?.error,
  };
}
