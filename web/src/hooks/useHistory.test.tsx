// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

const h = vi.hoisted(() => ({ fetchHistoryCosts: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchHistoryCosts: h.fetchHistoryCosts };
});

const { useLiveHistoryCosts } = await import("@/hooks/useHistory");
const { LIVE_POLL_MS } = await import("@/hooks/useStateQuery");

// useLiveHistoryCosts is the run detail's read of the per-ticket cost ledger (STUDIO-975 round 1).
// The ledger is not terminal-only — `update_run_progress` writes `runs.total_tokens` after every
// turn — so a run-detail total fetched once froze while the per-attempt vitals beside it kept
// moving. It must ride the run's own cadence while live, then take ONE last pass as the run ends
// (where the final turn lands), then stop.
function mount(live: boolean) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={qc}>{children}</QueryClientProvider>
  );
  return renderHook(({ inFlight }: { inFlight: boolean }) => useLiveHistoryCosts(inFlight), {
    wrapper,
    initialProps: { inFlight: live },
  });
}

describe("useLiveHistoryCosts", () => {
  beforeEach(() => {
    h.fetchHistoryCosts.mockResolvedValue({ costs: [] });
  });

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    vi.clearAllMocks();
    h.fetchHistoryCosts.mockReset();
  });

  it("polls the ledger on the live cadence while the run is live", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount(true);
    await waitFor(() => expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(1));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVE_POLL_MS + 1);
    });

    expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(2);
  });

  it("stops polling once the run is terminal", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    mount(false);
    await waitFor(() => expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(1));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVE_POLL_MS * 3);
    });

    expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(1);
  });

  // The edge the interval cannot cover: polling stops the moment the run goes terminal, so the
  // final turn's tokens — which reach the ledger only as the run ends — need one explicit pass.
  it("fires exactly one more refetch on the live→terminal edge", async () => {
    const { rerender } = mount(true);
    await waitFor(() => expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(1));

    rerender({ inFlight: false });

    await waitFor(() => expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(2));
    // One pass, not a new cadence: a moment later it is still two.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(h.fetchHistoryCosts).toHaveBeenCalledTimes(2);
  });
});
