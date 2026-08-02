// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { DaySummary, StateResponse } from "@/lib/api";
import { RunsStatTiles } from "@/components/runs/StatTiles";

afterEach(cleanup);

// One live session started today with a non-zero token total (drives the Playing count).
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

// The daemon's day summary — the header cells' only source for today's figures (TRA-320). The
// rhythm series drives the sparkline; it arrives already ordered and capped by the daemon.
const summary: DaySummary = {
  since: "2026-06-07T00:00:00Z",
  runs: 3,
  completed: 2,
  input_tokens: 1000,
  output_tokens: 2000,
  total_tokens: 5000,
  seconds: 900,
  rhythm: [1000, 4000, 5000],
};

const idleSummary: DaySummary = { ...summary, runs: 0, completed: 0, rhythm: [] };

describe("RunsStatTiles (instrument strip)", () => {
  it("renders the four instrument cells derived from state + the daemon day summary", () => {
    render(<RunsStatTiles state={state} summary={summary} rows={[]} maxConcurrent={4} />);
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
      <RunsStatTiles state={state} summary={summary} rows={[]} maxConcurrent={4} />,
    );
    expect(container.querySelector('[data-pulse="true"]')).toBeTruthy();
    rerender(<RunsStatTiles state={state} summary={summary} rows={[]} maxConcurrent={4} live={false} />);
    expect(container.querySelector('[data-pulse="true"]')).toBeNull();
  });

  it("draws the token rhythm sparkline when runs happened today, and omits it when idle", () => {
    const { container } = render(
      <RunsStatTiles state={state} summary={summary} rows={[]} maxConcurrent={4} />,
    );
    expect(container.querySelector('[data-rhythm="true"]')).toBeTruthy();
    cleanup();
    const { container: idle } = render(
      <RunsStatTiles state={undefined} summary={idleSummary} rows={[]} maxConcurrent={4} />,
    );
    expect(idle.querySelector('[data-rhythm="true"]')).toBeNull();
  });

  it("omits the seat annotation when the capacity is unknown (config still loading)", () => {
    render(<RunsStatTiles state={state} summary={summary} rows={[]} maxConcurrent={0} />);
    expect(screen.queryByText(/seats/)).toBeNull();
  });

  it("does not crash when state and summary are undefined (loading)", () => {
    render(<RunsStatTiles state={undefined} summary={undefined} rows={[]} maxConcurrent={0} />);
    expect(screen.getByText("Playing")).toBeTruthy();
  });
});
