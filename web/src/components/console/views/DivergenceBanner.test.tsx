// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReviewDivergence, StateResponse } from "@/lib/api";

// STUDIO-898 — the console half of "a stalled pull request must be legible". Both directions are
// requirements, and the ABSENCE is the one that decides whether the presence is ever read: a banner
// that is always up is the permanent unread warning this feature exists to replace.

const h = vi.hoisted(() => ({ fetchState: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchState: h.fetchState };
});

const { DivergenceBanner } = await import("./DivergenceBanner");

function state(over: Partial<StateResponse> = {}): StateResponse {
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

function divergence(over: Partial<ReviewDivergence> = {}): ReviewDivergence {
  return {
    pr: "makewhatis/rhapsody#164",
    kind: "changes_requested_no_run",
    detail: "a reviewer asked for changes and the ticket has had no run since",
    ticket: "STUDIO-893",
    reviewer: "jimmy",
    stale_secs: 6 * 3600,
    ...over,
  };
}

function renderBanner() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <DivergenceBanner />
    </QueryClientProvider>,
  );
}

describe("DivergenceBanner", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  // The daemon omits the key entirely on a healthy board, so an ordinary console has no banner to
  // suppress — and none that can be left showing by mistake.
  it("renders nothing on a healthy board", async () => {
    h.fetchState.mockResolvedValue(state());
    const { container } = renderBanner();
    await waitFor(() => expect(h.fetchState).toHaveBeenCalled());
    expect(container.innerHTML).toBe("");
  });

  // An empty array means the same thing as an absent key, and a client that treated the two
  // differently would show an empty warning on a healthy board.
  it("renders nothing for an empty list", async () => {
    h.fetchState.mockResolvedValue(state({ review_divergence: [] }));
    const { container } = renderBanner();
    await waitFor(() => expect(h.fetchState).toHaveBeenCalled());
    expect(container.innerHTML).toBe("");
  });

  // A reported divergence names the pull request, its ticket and how long — the three facts an
  // operator needs to decide whether to intervene. The wording comes from the daemon's own `detail`,
  // so the console cannot drift from it.
  it("names the pull request, its ticket and how long it has been stuck", async () => {
    h.fetchState.mockResolvedValue(state({ review_divergence: [divergence()] }));
    renderBanner();
    const banner = await screen.findByRole("status");
    expect(banner.textContent).toContain("makewhatis/rhapsody#164");
    expect(banner.textContent).toContain("STUDIO-893");
    expect(banner.textContent).toContain(
      "a reviewer asked for changes and the ticket has had no run since",
    );
    expect(banner.textContent).toContain("for 6 hours");
    // It is a report, not an action: the banner offers no control, because no control could honestly
    // resolve a cause the daemon has not diagnosed.
    expect(screen.queryByRole("button")).toBeNull();
    expect(banner.textContent).toContain("Nothing has been changed on your behalf");
  });

  // Several diverged pull requests are counted rather than pluralised wrongly, and each is listed:
  // "which one" is the operator's first question and a count alone cannot answer it.
  it("lists every diverged pull request", async () => {
    h.fetchState.mockResolvedValue(
      state({
        review_divergence: [
          divergence(),
          divergence({
            pr: "makewhatis/strava#21",
            kind: "approved_still_open",
            detail: "every required reviewer approved and the pull request is still open",
            ticket: "STUDIO-897",
            reviewer: "",
            stale_secs: 3 * 24 * 3600,
          }),
        ],
      }),
    );
    renderBanner();
    const banner = await screen.findByRole("status");
    expect(banner.textContent).toContain("2 pull requests are neither progressing nor reported");
    expect(banner.textContent).toContain("makewhatis/rhapsody#164");
    expect(banner.textContent).toContain("makewhatis/strava#21");
    expect(banner.textContent).toContain("for 3 days");
  });

  // The shortest staleness the daemon can actually report — just past its ninety-minute threshold —
  // reads as "1 hour" and not "0 hours". The coarse rendering must never round a real report down to
  // nothing, which is the one way it could make a reported divergence look like a non-event.
  it("renders the shortest reportable staleness as an hour, never zero", async () => {
    h.fetchState.mockResolvedValue(
      state({ review_divergence: [divergence({ stale_secs: 91 * 60 })] }),
    );
    renderBanner();
    const banner = await screen.findByRole("status");
    expect(banner.textContent).toContain("for 1 hour");
  });

  // STUDIO-956: `round_budget_exhausted` has no staleness threshold and is reported the moment the
  // budget is spent, so it is the one kind that reaches the minutes branch — often at zero seconds.
  // It must read "1 minute" and never the ungrammatical "1 minutes".
  it("renders a just-spent budget as a single minute, not '1 minutes'", async () => {
    h.fetchState.mockResolvedValue(
      state({
        review_divergence: [
          divergence({
            kind: "round_budget_exhausted",
            detail:
              "the review↔author round budget is spent, so no further review or author re-run will be dispatched until it is cleared",
            reviewer: "",
            stale_secs: 0,
          }),
        ],
      }),
    );
    renderBanner();
    const banner = await screen.findByRole("status");
    expect(banner.textContent).toContain("for 1 minute");
    expect(banner.textContent).not.toContain("1 minutes");
  });
});
