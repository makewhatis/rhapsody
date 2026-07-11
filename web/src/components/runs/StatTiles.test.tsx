// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import type { StateResponse } from "@/lib/api";
import { RunsStatTiles, StatTile } from "@/components/runs/StatTiles";

afterEach(cleanup);

describe("StatTile", () => {
  it("renders the label, value and sub", () => {
    render(<StatTile label="Running" value="3" sub="2 agents active" />);
    expect(screen.getByText("Running")).toBeTruthy();
    expect(screen.getByText("3")).toBeTruthy();
    expect(screen.getByText("2 agents active")).toBeTruthy();
  });

  it("shows a pulsing accent dot when accent + pulse are set", () => {
    const { container } = render(
      <StatTile label="Running" value="3" sub="x" accent="var(--em-bright)" pulse />,
    );
    expect(container.querySelector('[data-pulse="true"]')).toBeTruthy();
  });

  it("renders no dot for a plain tile", () => {
    const { container } = render(<StatTile label="In review" value="0" sub="awaiting handoff" />);
    expect(container.querySelector('[data-pulse="true"]')).toBeNull();
    // the value inherits the default text colour (not an emerald accent)
    const value = screen.getByText("0");
    expect(value.style.color).toBe("var(--tx)");
  });
});

describe("RunsStatTiles", () => {
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
        total_tokens: 0,
      },
    ],
    retrying: [],
    codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
    rate_limits: [],
    blocked: [],
  };

  it("renders the four labelled tiles derived from state + history", () => {
    render(<RunsStatTiles state={state} history={[]} nowMs={Date.parse("2026-06-07T12:00:00Z")} />);
    expect(screen.getByText("Running")).toBeTruthy();
    expect(screen.getByText("Completed")).toBeTruthy();
    expect(screen.getByText("Tokens today")).toBeTruthy();
    expect(screen.getByText("Runtime today")).toBeTruthy();
    // one running session -> running tile value "1"
    expect(screen.getByText("1")).toBeTruthy();
    expect(screen.getByText("1 agent active")).toBeTruthy();
  });

  it("does not crash when state is undefined (loading)", () => {
    render(<RunsStatTiles state={undefined} history={[]} nowMs={0} />);
    expect(screen.getByText("Running")).toBeTruthy();
  });

  it("stops the Running tile dot pulsing when not live (e.g. under the Wails host)", () => {
    const { container } = render(
      <RunsStatTiles state={state} history={[]} nowMs={Date.parse("2026-06-07T12:00:00Z")} live={false} />,
    );
    expect(container.querySelector('[data-pulse="true"]')).toBeNull();
  });
});
