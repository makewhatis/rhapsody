// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import type { HistoryFilter, RetryEntry, RunningSession, StateResponse } from "@/lib/api";

const h = vi.hoisted(() => ({ fetchState: vi.fn(), fetchIssueRuns: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchState: h.fetchState, fetchIssueRuns: h.fetchIssueRuns };
});

const { liveJobsSignature, useJobsFeed } = await import("@/hooks/useJobsFeed");
const { LIVE_POLL_MS } = await import("@/hooks/useStateQuery");

/** One live session, with only the fields the signature is allowed to care about set meaningfully. */
function running(over: Partial<RunningSession> = {}): RunningSession {
  return {
    issue_id: "id",
    issue_identifier: "TRA-1",
    title: "t",
    state: "In Progress",
    project: "",
    repo: "",
    run_id: 7,
    turn_count: 1,
    last_codex_event: "",
    started_at: "2026-09-08T10:00:00Z",
    last_event_at: "2026-09-08T10:00:00Z",
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    ...over,
  };
}

function retry(over: Partial<RetryEntry> = {}): RetryEntry {
  return { issue_identifier: "TRA-2", attempt: 1, due_at: "2026-09-08T10:01:00Z", error: "", ...over };
}

function snapshot(over: Partial<StateResponse> = {}): StateResponse {
  return {
    status: "ok",
    poll_interval_ms: 2000,
    running: [],
    retrying: [],
    codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
    rate_limits: [],
    blocked: [],
    ...over,
  };
}

describe("liveJobsSignature", () => {
  // The signature is what decides whether a live snapshot forces the issue listing to refetch, so
  // anything that ticks on EVERY poll must stay out of it. A running agent's turn count and token
  // totals move continuously; if either leaked in, every 2s state tick would invalidate the list and
  // the fix for a list that never refreshes would become a list that refetches twice per tick.
  it("ignores the per-poll churn of a run that is still the same run", () => {
    const before = liveJobsSignature(snapshot({ running: [running({ turn_count: 3, total_tokens: 10 })] }));
    const after = liveJobsSignature(snapshot({ running: [running({ turn_count: 4, total_tokens: 99 })] }));
    expect(after).toBe(before);
  });

  // The transition that made this ticket visible: a run finishes, so the header's "running" count
  // drops immediately while the row below it still reads from the last issue-listing fetch.
  it("changes when a run leaves the live set", () => {
    const live = liveJobsSignature(snapshot({ running: [running()] }));
    const gone = liveJobsSignature(snapshot({ running: [] }));
    expect(gone).not.toBe(live);
  });

  it("changes when a retry becomes a new attempt", () => {
    const first = liveJobsSignature(snapshot({ retrying: [retry({ attempt: 1 })] }));
    const second = liveJobsSignature(snapshot({ retrying: [retry({ attempt: 2 })] }));
    expect(second).not.toBe(first);
  });

  // The daemon does not promise an order, and a reordered snapshot is not a state change.
  it("does not depend on the order the daemon lists live work in", () => {
    const a = liveJobsSignature(
      snapshot({ running: [running({ run_id: 1, issue_identifier: "TRA-1" }), running({ run_id: 2, issue_identifier: "TRA-2" })] }),
    );
    const b = liveJobsSignature(
      snapshot({ running: [running({ run_id: 2, issue_identifier: "TRA-2" }), running({ run_id: 1, issue_identifier: "TRA-1" })] }),
    );
    expect(b).toBe(a);
  });

  it("reads an absent snapshot as no live work rather than throwing", () => {
    expect(liveJobsSignature(undefined)).toBe(liveJobsSignature(snapshot()));
  });
});

describe("useJobsFeed", () => {
  function mount(filter?: HistoryFilter) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={qc}>{children}</QueryClientProvider>
    );
    return { qc, ...renderHook(() => useJobsFeed(filter), { wrapper }) };
  }

  beforeEach(() => {
    h.fetchState.mockResolvedValue(snapshot());
    h.fetchIssueRuns.mockResolvedValue({ issues: [], next_offset: null });
  });

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    vi.clearAllMocks();
    h.fetchState.mockReset();
    h.fetchIssueRuns.mockReset();
  });

  // The bug itself: `useIssueRuns()` defaults `refetchInterval` to `false`, so the Jobs table only
  // ever loaded on mount while the header strip above it polled on. Pinning the interval to the
  // header's own constant is the whole point — a number typed here independently could drift away
  // from `useStateQuery` and put the two back out of step.
  it("keeps polling the issue listing on the header's cadence", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount();
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVE_POLL_MS + 1);
    });
    expect(h.fetchIssueRuns).toHaveBeenCalledTimes(2);
    // Same cadence as the counts above the table, not merely "some" cadence.
    expect(h.fetchState.mock.calls.length).toBe(h.fetchIssueRuns.mock.calls.length);
  });

  // Equal intervals still leave the two queries phase-shifted by up to a full tick, which is long
  // enough to render "0 running" over a row that still says running. The live snapshot is the
  // fresher of the two, so a change in it pulls the listing forward instead of waiting for the next
  // tick.
  it("refetches the issue listing as soon as the live set changes, without waiting for a tick", async () => {
    const { qc, result } = mount();
    // Wait for the FIRST snapshot specifically: until one has landed there is nothing to compare
    // against, and a swap made before then would only seed the comparison — which is a different
    // code path, and one this test would otherwise pass through without exercising.
    await waitFor(() => expect(result.current.state.data).toBeDefined());
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));

    h.fetchState.mockResolvedValue(snapshot({ running: [running()] }));
    await act(async () => {
      await qc.refetchQueries({ queryKey: ["state"] });
    });

    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(2));
  });

  // The other half of the guard in `liveJobsSignature`: a snapshot that reports the same work, only
  // further along, must not cost a listing fetch.
  it("does not refetch the issue listing when only a running agent's counters moved", async () => {
    h.fetchState.mockResolvedValue(snapshot({ running: [running({ turn_count: 1 })] }));
    const { qc, result } = mount();
    await waitFor(() => expect(result.current.state.data).toBeDefined());
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));

    h.fetchState.mockResolvedValue(snapshot({ running: [running({ turn_count: 2, total_tokens: 500 })] }));
    await act(async () => {
      await qc.refetchQueries({ queryKey: ["state"] });
    });

    expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1);
  });

  // The first snapshot to arrive seeds the comparison; it is not a "change". Without this the list
  // would fetch twice on every mount of the Jobs surface for no new information.
  it("does not treat the first live snapshot as a change", async () => {
    h.fetchState.mockResolvedValue(snapshot({ running: [running()] }));
    const { result } = mount();
    await waitFor(() => expect(result.current.state.data).toBeDefined());
    expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1);
  });

  // STUDIO-792 widened the worklist's page, and the window reaches this hook as `filter`. It is the
  // interval that must not follow it there: on the operator's own daemon a full-width request
  // measured 0.63s-0.88s warm and 3.63s cold against 1.6ms at the default width, so keeping one
  // cadence would leave a request of that size firing every 2s for as long as the page stayed open.
  it("does not put a widened window on that cadence", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount({ limit: 400 });
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));
    // The window really is being sent — otherwise this box would pass on a hook that had quietly
    // stopped paging at all, which is the cheap way to make a "no poll" assertion go green.
    expect(h.fetchIssueRuns).toHaveBeenCalledWith({ limit: 400 });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVE_POLL_MS * 5);
    });

    expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1);
    // And the live snapshot is untouched by any of this: the strip keeps its own cadence, so what
    // was taken off the timer is the wide listing request and nothing else.
    expect(h.fetchState.mock.calls.length).toBeGreaterThan(1);
  });

  // The other half of that trade, and the reason dropping the interval is not dropping freshness.
  // Every transition the interval was there for changes `liveJobsSignature` first: the daemon
  // removes a run from the live map and writes its end row inside ONE control-loop event, and only
  // republishes the snapshot `/api/v1/state` serves once that handler has returned. So the snapshot
  // that reports the run gone is already backed by a store that has recorded it, and the
  // pull-forward carries the widened page just as it carries the default one — it invalidates the
  // listing family by prefix, not one filter.
  it("still refetches a widened window as soon as the live set changes", async () => {
    const { qc, result } = mount({ limit: 400 });
    await waitFor(() => expect(result.current.state.data).toBeDefined());
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));

    h.fetchState.mockResolvedValue(snapshot({ running: [running()] }));
    await act(async () => {
      await qc.refetchQueries({ queryKey: ["state"] });
    });

    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(2));
    // Refetched at the window the operator is actually holding, not collapsed back to the default.
    expect(h.fetchIssueRuns).toHaveBeenLastCalledWith({ limit: 400 });
  });

  // "No busy-loop" (the ticket's third acceptance point). react-query's `refetchIntervalInBackground`
  // defaults to false and its focus manager reads `document.visibilityState`, so a hidden window
  // makes ZERO requests on either query — the deliberate backgrounded cadence is "none", and it is
  // pinned here because it is a default we are relying on rather than one we wrote.
  it("makes no requests at all while the window is hidden", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount();
    await waitFor(() => expect(h.fetchIssueRuns).toHaveBeenCalledTimes(1));
    const settled = { state: h.fetchState.mock.calls.length, issues: h.fetchIssueRuns.mock.calls.length };

    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    act(() => {
      window.dispatchEvent(new Event("visibilitychange"));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVE_POLL_MS * 5);
    });

    expect(h.fetchIssueRuns).toHaveBeenCalledTimes(settled.issues);
    expect(h.fetchState).toHaveBeenCalledTimes(settled.state);
    visibility.mockRestore();
  });
});
