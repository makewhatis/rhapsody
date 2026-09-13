// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { StateResponse } from "@/lib/api";

// STUDIO-880 — the console half of "a paused dispatch must be legible". A draining daemon looks
// exactly like a wedged one from the outside (tickets sit, nothing dispatches), so the banner's
// ABSENCE on an ordinary daemon and its PRESENCE on a draining one are both requirements.

const h = vi.hoisted(() => ({ fetchState: vi.fn(), setDrain: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchState: h.fetchState, setDrain: h.setDrain };
});

const { DrainBanner } = await import("./DrainBanner");

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

function renderBanner() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <DrainBanner />
    </QueryClientProvider>,
  );
}

describe("DrainBanner", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  // The daemon omits the `drain` key entirely while dispatching normally, so an ordinary console has
  // no banner to suppress — nothing to hide, and nothing that can be left showing by mistake.
  it("renders nothing when the daemon is not draining", async () => {
    h.fetchState.mockResolvedValue(state());
    const { container } = renderBanner();
    await waitFor(() => expect(h.fetchState).toHaveBeenCalled());
    expect(container.innerHTML).toBe("");
  });

  // An armed drain says so, and says that nothing was interrupted — which is the fact an operator
  // watching a run mid-turn most needs, and the one the mechanism is easiest to mistrust about.
  it("announces an armed drain and that in-flight runs are not interrupted", async () => {
    h.fetchState.mockResolvedValue(
      state({
        drain: { active: true, reason: "update", requested_at: "2026-09-12T10:00:00Z" },
        running: [{ issue_identifier: "STU-1" }] as unknown as StateResponse["running"],
      }),
    );
    renderBanner();
    const note = await screen.findByRole("status");
    expect(note.textContent).toMatch(/no new work is being started/i);
    expect(note.textContent).toMatch(/1 run is finishing the current turn/i);
    expect(note.textContent).toMatch(/nothing has been interrupted/i);
    // The reason is carried through, so "the app is upgrading" is not mistaken for an operator pause.
    expect(note.textContent).toMatch(/for an update/i);
  });

  // Reaching zero is the moment a restart becomes free, so the banner says that rather than leaving
  // the operator to infer it from an empty runs list.
  it("says a restart is now safe once nothing is in flight", async () => {
    h.fetchState.mockResolvedValue(
      state({ drain: { active: true, reason: "operator", requested_at: "" } }),
    );
    renderBanner();
    const note = await screen.findByRole("status");
    expect(note.textContent).toMatch(/nothing is in flight/i);
    expect(note.textContent).not.toMatch(/for an update/i);
  });

  // STUDIO-880 — the way OUT. A drain outlives whatever asked for one: an expired desktop budget
  // leaves it armed on purpose, so without this the daemon takes no work at all and the only
  // remedies are a restart (throwing away the turn the drain was protecting) or a hand-rolled curl.
  it("cancels the drain over the API and re-reads the daemon's answer", async () => {
    h.setDrain.mockResolvedValue({ active: false, reason: "operator", requested_at: "" });
    h.fetchState.mockResolvedValue(
      state({ drain: { active: true, reason: "operator", requested_at: "" } }),
    );
    renderBanner();
    const button = await screen.findByRole("button", { name: /cancel drain/i });
    fireEvent.click(button);
    // `false` — the banner ENDS a drain and can never arm one.
    await waitFor(() => expect(h.setDrain).toHaveBeenCalledWith(false));

    // Once the daemon stops reporting a drain the banner goes, so the control cannot be left
    // showing against a daemon that is dispatching again.
    h.fetchState.mockResolvedValue(state());
    const { container } = renderBanner();
    await waitFor(() => expect(container.innerHTML).toBe(""));
  });

  // A refused cancel must not read as a successful one: the operator is still in the state the
  // banner exists to make visible, and has to be told the button did not work.
  it("says so when the daemon refuses the cancel, and leaves the banner up", async () => {
    h.setDrain.mockRejectedValue(new Error("drain cancel failed: 503"));
    h.fetchState.mockResolvedValue(
      state({ drain: { active: true, reason: "operator", requested_at: "" } }),
    );
    renderBanner();
    fireEvent.click(await screen.findByRole("button", { name: /cancel drain/i }));
    const note = await screen.findByRole("status");
    await waitFor(() => expect(note.textContent).toMatch(/refused the cancel/i));
    expect(note.textContent).toMatch(/no new work is being started/i);
  });

  // `active: false` and an absent key mean the same thing — a daemon that cancelled a drain must not
  // leave the banner up.
  it("renders nothing for a cancelled drain", async () => {
    h.fetchState.mockResolvedValue(
      state({ drain: { active: false, reason: "operator", requested_at: "" } }),
    );
    const { container } = renderBanner();
    await waitFor(() => expect(h.fetchState).toHaveBeenCalled());
    expect(container.innerHTML).toBe("");
  });
});
