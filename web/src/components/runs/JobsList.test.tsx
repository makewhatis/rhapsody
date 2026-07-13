// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { JobRow } from "@/lib/runs-model";
import { JobsList } from "@/components/runs/JobsList";

afterEach(cleanup);

function job(over: Partial<JobRow> = {}): JobRow {
  return {
    key: over.key ?? `k-${over.issue ?? "INF-1"}`,
    runId: 1,
    issue: "INF-1",
    title: "A job",
    agent: "Infrastructure",
    agentColor: "#34d399",
    status: "completed",
    project: "symphony-infra-tasks-9c29e9ade060",
    projectShort: "symphony-infra-tasks",
    turn: 3,
    tokens: "12.0k",
    duration: "5m 0s",
    durationAccent: false,
    live: false,
    startedAtMs: 1000,
    ...over,
  };
}

const ROWS: JobRow[] = [
  job({ key: "r1", runId: 10, issue: "INF-231", title: "Sign the dmg", status: "running", live: true, durationAccent: true, duration: "12m 4s" }),
  job({ key: "r2", runId: 11, issue: "HARV-77", title: "diff viewer", status: "stopped", agent: "Harvest" }),
  job({ key: "r3", runId: 12, issue: "CORE-112", title: "SCIM endpoint", status: "completed", agent: "Core Platform" }),
  job({ key: "r4", runId: 13, issue: "EXCH-42", title: "metered webhook", status: "failed", agent: "Exchange", subLabel: "turn timeout" }),
];

describe("JobsList", () => {
  it("renders the title, the seven column headers, and a row's cells", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    expect(screen.getByText("Jobs")).toBeTruthy();
    for (const h of ["Issue", "Agent", "Status", "Project", "Turn", "Tokens", "Duration"]) {
      expect(screen.getByText(h)).toBeTruthy();
    }
    expect(screen.getByText("INF-231")).toBeTruthy();
    expect(screen.getByText("Sign the dmg")).toBeTruthy();
    expect(screen.getAllByText("symphony-infra-tasks").length).toBeGreaterThan(0);
  });

  it("shows live counts on the segmented filter chips (Playing count in rust)", () => {
    const { container } = render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    // All 4 · Playing 1 · Completed 1 · Stopped 1 · Failed 1 · Waiting 0
    expect(screen.getByRole("button", { name: "All 4" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Playing 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Completed 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Stopped 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Failed 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Waiting 0" })).toBeTruthy();
    // the Playing chip's count renders in the rust live colour
    const playingCount = container.querySelector('[data-filter-count="running"]') as HTMLElement | null;
    expect(playingCount?.style.color).toBe("var(--rust-text)");
  });

  it("shows the Podium footer summary + the sage live cadence", () => {
    const { container } = render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    expect(screen.getByText("4 jobs — 1 playing")).toBeTruthy();
    expect(screen.getByText("live · every 2s")).toBeTruthy();
    // the footer's live dot is sage and pulses
    expect(container.querySelector('[data-live-indicator] [data-pulse="true"]')).toBeTruthy();
  });

  it("falls back to a bare 'live' footer when no poll interval is known", () => {
    render(<JobsList rows={ROWS} onSelect={() => {}} />);
    expect(screen.getByText("live")).toBeTruthy();
    expect(screen.queryByText(/every/)).toBeNull();
  });

  it("hides the live indicator when polling is disabled (e.g. under the Wails host)", () => {
    const { container } = render(<JobsList rows={ROWS} pollMs={2000} polling={false} onSelect={() => {}} />);
    expect(screen.queryByText(/every/)).toBeNull();
    expect(screen.queryByText("live")).toBeNull();
    expect(screen.getByText("live updates paused")).toBeTruthy();
    // no pulsing live dot while paused
    expect(container.querySelector('[data-live-indicator] [data-pulse="true"]')).toBeNull();
  });

  it("filters to playing rows via the segmented control", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Playing 1" }));
    expect(screen.getByText("INF-231")).toBeTruthy();
    expect(screen.queryByText("CORE-112")).toBeNull();
  });

  it("filters stopped jobs via the Stopped filter", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Stopped 1" }));
    expect(screen.getByText("HARV-77")).toBeTruthy();
    expect(screen.queryByText("INF-231")).toBeNull();
  });

  it("renders the failure sub-label inline on a failed row", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    expect(screen.getByText("turn timeout")).toBeTruthy();
  });

  it("searches over issue + title + agent", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    fireEvent.change(screen.getByPlaceholderText("Search jobs…"), { target: { value: "scim" } });
    expect(screen.getByText("CORE-112")).toBeTruthy();
    expect(screen.queryByText("INF-231")).toBeNull();
  });

  it("focuses the search field on ⌘K", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    const input = screen.getByPlaceholderText("Search jobs…");
    expect(document.activeElement).not.toBe(input);
    fireEvent.keyDown(document, { key: "k", metaKey: true });
    expect(document.activeElement).toBe(input);
  });

  it("shows an empty state when nothing matches", () => {
    render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    fireEvent.change(screen.getByPlaceholderText("Search jobs…"), { target: { value: "zzz-nomatch" } });
    expect(screen.getByText(/No jobs match/)).toBeTruthy();
  });

  it("marks running rows live (the 2px rust left-rule + tint)", () => {
    const { container } = render(<JobsList rows={ROWS} pollMs={2000} onSelect={() => {}} />);
    const liveRows = container.querySelectorAll('[data-live="true"]');
    expect(liveRows.length).toBe(1);
    // the playing row carries the rust left border + the playing-row tint (colour longhands round-
    // trip var() reliably through jsdom's cssstyle; the border/background shorthands do not)
    const row = liveRows[0] as HTMLElement;
    expect(row.style.borderLeftColor).toBe("var(--rust)");
    expect(row.style.backgroundColor).toBe("var(--tint-playing-row)");
  });

  it("renders turn/tokens/duration em-dashes for a never-run held row", () => {
    const waiting = job({
      key: "w1",
      runId: 0,
      issue: "DEP-1",
      title: "dependent",
      status: "waiting",
      subLabel: "waiting on INF-1 · In Review",
      turn: 0,
      tokens: "0",
      duration: "",
    });
    render(<JobsList rows={[waiting]} pollMs={2000} onSelect={() => {}} />);
    // three em-dashes: turn, tokens, duration
    expect(screen.getAllByText("—").length).toBe(3);
  });

  it("renders a held dependent as a waiting row: chip + subLabel, not clickable, Waiting filter (INF-320)", () => {
    const onSelect = vi.fn();
    const waiting = job({
      key: "w1",
      runId: 0,
      issue: "DEP-1",
      title: "dependent",
      status: "waiting",
      subLabel: "waiting on INF-1 · In Review",
    });
    render(<JobsList rows={[waiting, ...ROWS]} pollMs={2000} onSelect={onSelect} />);
    // The "waiting" status chip (lowercase label) and the "waiting on …" sub-label both render.
    expect(screen.getByText("waiting")).toBeTruthy();
    expect(screen.getByText("waiting on INF-1 · In Review")).toBeTruthy();
    // A never-run held ticket (runId 0) is inert — clicking it does NOT open a run.
    fireEvent.click(screen.getByText("DEP-1"));
    expect(onSelect).not.toHaveBeenCalled();
    // The "Waiting" filter button is present and narrows to the held rows only.
    fireEvent.click(screen.getByRole("button", { name: "Waiting 1" }));
    expect(screen.getByText("DEP-1")).toBeTruthy();
    expect(screen.queryByText("INF-231")).toBeNull();
  });

  it("opens a run on row click, but not for an unpersisted (run_id 0) row", () => {
    const onSelect = vi.fn();
    render(
      <JobsList
        rows={[job({ key: "p", runId: 0, issue: "NOID", title: "no persistence", status: "failed" }), ...ROWS]}
        pollMs={2000}
        onSelect={onSelect}
      />,
    );
    fireEvent.click(screen.getByText("CORE-112"));
    expect(onSelect).toHaveBeenCalledWith(12);
    onSelect.mockClear();
    fireEvent.click(screen.getByText("NOID"));
    expect(onSelect).not.toHaveBeenCalled();
  });
});
