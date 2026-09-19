// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import type { IssueRun } from "@/lib/api";

const h = vi.hoisted(() => ({ fetchIssueRuns: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchIssueRuns: h.fetchIssueRuns };
});

const { useBoardActive } = await import("@/hooks/useBoardActive");
const { BOARD_ACTIVE_LIMIT, BOARD_ACTIVE_OUTCOMES } = await import("@/lib/console-board");
const { TRACKER_POLL_MS } = await import("@/hooks/useHistory");

let nextId = 1;

function run(over: Partial<IssueRun> & Pick<IssueRun, "issue_identifier" | "outcome">): IssueRun {
  return {
    id: (nextId += 1),
    issue_id: `id-${over.issue_identifier}`,
    title: `${over.issue_identifier} title`,
    attempt: 1,
    session_uuid: "s",
    branch: `symphony/${over.issue_identifier}`,
    project_slug: "rhapsody",
    repo: "",
    started_at: "2026-09-01T10:00:00Z",
    ended_at: "2026-09-01T10:30:00Z",
    turns: 1,
    input_tokens: 1,
    output_tokens: 1,
    total_tokens: 2,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  } as IssueRun;
}

// STUDIO-931 — the board's non-terminal lanes fetch by LATEST run outcome, wide, instead of
// bucketing the 50-row recency page. The fetch SHAPE is the fix, so it is what the hook test pins.
describe("useBoardActive", () => {
  function mount(enabled = true) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={qc}>{children}</QueryClientProvider>
    );
    return renderHook(() => useBoardActive(enabled), { wrapper });
  }

  beforeEach(() => {
    h.fetchIssueRuns.mockImplementation(async () => ({ issues: [], next_offset: null }));
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    h.fetchIssueRuns.mockReset();
  });

  it("fetches each issue's latest-run outcome by filter, never the recency page", async () => {
    mount();
    await waitFor(() =>
      expect(h.fetchIssueRuns).toHaveBeenCalledTimes(BOARD_ACTIVE_OUTCOMES.length),
    );
    for (const outcome of BOARD_ACTIVE_OUTCOMES) {
      expect(h.fetchIssueRuns).toHaveBeenCalledWith({
        latestOutcome: outcome,
        limit: BOARD_ACTIVE_LIMIT,
      });
    }
    // Every request carries a latestOutcome AND a limit: an omitted outcome would be the recency
    // page again, an omitted limit would be the 50-row window this ticket exists to escape, and
    // `outcome` (which filters BEFORE the partition) would return stale runs of finished tickets.
    for (const [filter] of h.fetchIssueRuns.mock.calls) {
      expect(filter.latestOutcome).toBeTruthy();
      expect(filter.outcome).toBeUndefined();
      expect(filter.limit).toBe(BOARD_ACTIVE_LIMIT);
    }
  });

  it("merges the outcomes' issues into one list", async () => {
    h.fetchIssueRuns.mockImplementation(async ({ latestOutcome }: { latestOutcome: string }) => ({
      issues:
        latestOutcome === "stopped"
          ? [run({ issue_identifier: "STUDIO-877", outcome: "stopped" })]
          : latestOutcome === "running"
            ? [run({ issue_identifier: "LIVE-1", outcome: "running" })]
            : [],
      next_offset: null,
    }));
    const { result } = mount();
    await waitFor(() => expect(result.current).toHaveLength(2));
    expect(result.current.map((r) => r.issue_identifier).sort()).toEqual(["LIVE-1", "STUDIO-877"]);
  });

  it("makes no request while the table, not the board, is on screen", async () => {
    const { result } = mount(false);
    // Give any stray effect a beat to fire; nothing should have.
    await new Promise((r) => setTimeout(r, 0));
    expect(h.fetchIssueRuns).not.toHaveBeenCalled();
    expect(result.current).toEqual([]);
  });

  it("polls on the tracker's slow cadence, not the live one", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount();
    await waitFor(() =>
      expect(h.fetchIssueRuns).toHaveBeenCalledTimes(BOARD_ACTIVE_OUTCOMES.length),
    );
    const settled = h.fetchIssueRuns.mock.calls.length;
    // The live cadence is 2s; a widened read must not ride it. One TRACKER_POLL_MS must buy exactly
    // one more round of the five outcome queries and no more.
    await vi.advanceTimersByTimeAsync(TRACKER_POLL_MS + 1);
    expect(h.fetchIssueRuns.mock.calls.length).toBe(settled + BOARD_ACTIVE_OUTCOMES.length);
    vi.useRealTimers();
  });
});
