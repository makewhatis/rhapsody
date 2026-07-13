// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { StateResponse } from "@/lib/api";
import { RunsStatTiles } from "@/components/runs/StatTiles";

afterEach(cleanup);

const NOW = Date.parse("2026-06-07T12:00:00Z");

// One live session started today with a non-zero token total (drives the Playing count + the
// Tokens-today rhythm sparkline).
const state: StateResponse = {
  status: "ok",
  poll_interval_ms: 2000,
  running: [
    {
      issue_id: "a",
      issue_identifier: "INF-1",
      title: "t",
      state: "In Progress",
      project: "alpha",
      repo: "",
      run_id: 1,
      turn_count: 2,
      last_codex_event: "",
      started_at: "2026-06-07T11:00:00Z",
      last_event_at: "",
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: 5000,
    },
  ],
  retrying: [],
  codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
  rate_limits: [],
  blocked: [],
};

describe("RunsStatTiles (instrument strip)", () => {
  it("renders the four instrument cells derived from state + history", () => {
    render(<RunsStatTiles state={state} history={[]} nowMs={NOW} maxConcurrent={4} />);
    expect(screen.getByText("Playing")).toBeTruthy();
    expect(screen.getByText("Completed")).toBeTruthy();
    expect(screen.getByText("Tokens today")).toBeTruthy();
    expect(screen.getByText("Runtime today")).toBeTruthy();
    // one running session → Playing value "1" + the seat annotation ("of N seats")
    expect(screen.getByText("1")).toBeTruthy();
    expect(screen.getByText("of 4 seats")).toBeTruthy();
  });

  it("pulses the Playing dot while live and stops it when not live", () => {
    const { container, rerender } = render(
      <RunsStatTiles state={state} history={[]} nowMs={NOW} maxConcurrent={4} />,
    );
    expect(container.querySelector('[data-pulse="true"]')).toBeTruthy();
    rerender(<RunsStatTiles state={state} history={[]} nowMs={NOW} maxConcurrent={4} live={false} />);
    expect(container.querySelector('[data-pulse="true"]')).toBeNull();
  });

  it("draws the token rhythm sparkline when runs happened today, and omits it when idle", () => {
    const { container } = render(
      <RunsStatTiles state={state} history={[]} nowMs={NOW} maxConcurrent={4} />,
    );
    expect(container.querySelector('[data-rhythm="true"]')).toBeTruthy();
    cleanup();
    const { container: idle } = render(
      <RunsStatTiles state={undefined} history={[]} nowMs={NOW} maxConcurrent={4} />,
    );
    expect(idle.querySelector('[data-rhythm="true"]')).toBeNull();
  });

  it("omits the seat annotation when the capacity is unknown (config still loading)", () => {
    render(<RunsStatTiles state={state} history={[]} nowMs={NOW} maxConcurrent={0} />);
    expect(screen.queryByText(/seats/)).toBeNull();
  });

  it("does not crash when state is undefined (loading)", () => {
    render(<RunsStatTiles state={undefined} history={[]} nowMs={0} maxConcurrent={0} />);
    expect(screen.getByText("Playing")).toBeTruthy();
  });
});
