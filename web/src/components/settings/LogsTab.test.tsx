// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, within } from "@testing-library/react";
import type { LogLine, LogStreamStatus } from "@/hooks/useLogStream";
import { LogsTab } from "@/components/settings/LogsTab";

// Mock the SSE hook so the tab test is deterministic (the hook's EventSource handling is
// covered separately). The mock is mutated per-test before render.
const state: { lines: LogLine[]; status: LogStreamStatus } = { lines: [], status: "open" };
const clear = vi.fn();
vi.mock("@/hooks/useLogStream", () => ({
  useLogStream: () => ({ lines: state.lines, status: state.status, clear }),
}));

function line(over: Partial<LogLine>): LogLine {
  return { seq: 1, time: "2026-06-07T10:00:00Z", level: "INFO", msg: "hello", ...over };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  state.lines = [];
  state.status = "open";
});

describe("LogsTab", () => {
  it("renders streamed lines with their message and attrs", () => {
    state.lines = [
      line({ seq: 1, level: "INFO", msg: "poll tick", attrs: { eligible: "3" } }),
      line({ seq: 2, level: "ERROR", msg: "dispatch failed" }),
    ];
    render(<LogsTab />);
    expect(screen.getByText("poll tick")).toBeTruthy();
    expect(screen.getByText("dispatch failed")).toBeTruthy();
    // attr key=value is rendered alongside the message
    expect(screen.getByText("eligible=3")).toBeTruthy();
  });

  it("shows the live status and the visible line count", () => {
    state.lines = [line({ seq: 1 })];
    state.status = "open";
    render(<LogsTab />);
    expect(screen.getByText("live")).toBeTruthy();
    expect(screen.getByText("1 line")).toBeTruthy();
  });

  it("filters by level (Error+ hides INFO/WARN)", () => {
    state.lines = [
      line({ seq: 1, level: "INFO", msg: "info-line" }),
      line({ seq: 2, level: "WARN", msg: "warn-line" }),
      line({ seq: 3, level: "ERROR", msg: "err-line" }),
    ];
    render(<LogsTab />);
    fireEvent.click(screen.getByRole("tab", { name: "Error" }));
    expect(screen.queryByText("info-line")).toBeNull();
    expect(screen.queryByText("warn-line")).toBeNull();
    expect(screen.getByText("err-line")).toBeTruthy();
  });

  it("tracks the live count as the filter narrows", () => {
    state.lines = [
      line({ seq: 1, level: "INFO", msg: "a" }),
      line({ seq: 2, level: "WARN", msg: "b" }),
      line({ seq: 3, level: "ERROR", msg: "c" }),
    ];
    render(<LogsTab />);
    expect(screen.getByText("3 lines")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Warn+" }));
    expect(screen.getByText("2 lines")).toBeTruthy();
  });

  it("tints WARN rows amber and labels them WRN", () => {
    state.lines = [line({ seq: 1, level: "WARN", msg: "restarting daemon" })];
    render(<LogsTab />);
    expect(screen.getByText("WRN")).toBeTruthy();
    // the row carries the amber warn-row tint token
    const row = screen.getByText("restarting daemon").closest("div");
    expect(row?.style.background).toContain("--tint-warn-row");
  });

  it("invokes clear() from the Clear button", () => {
    state.lines = [line({ seq: 1 })];
    render(<LogsTab />);
    fireEvent.click(screen.getByRole("button", { name: /Clear/ }));
    expect(clear).toHaveBeenCalledOnce();
  });

  it("pauses and resumes follow mode from the footer control", () => {
    state.lines = [line({ seq: 1, msg: "a" }), line({ seq: 2, msg: "b" })];
    render(<LogsTab />);
    // starts following → the footer offers "pause follow"
    expect(screen.getByText("following — newest at bottom")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /pause follow/ }));
    // paused → the footer swaps to "jump to latest"
    expect(screen.getByText("paused — scrolled up")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /jump to latest/ }));
    expect(screen.getByText("following — newest at bottom")).toBeTruthy();
  });

  it("surfaces an unavailable stream as an empty-state note", () => {
    state.lines = [];
    state.status = "closed";
    render(<LogsTab />);
    expect(screen.getByText(/isn't available/)).toBeTruthy();
  });

  it("shows a waiting note when connected but no lines yet", () => {
    state.lines = [];
    state.status = "open";
    render(<LogsTab />);
    expect(screen.getByText(/Waiting for the daemon to log/)).toBeTruthy();
  });

  it("shows a no-match note when the filter excludes all lines", () => {
    state.lines = [line({ seq: 1, level: "INFO", msg: "info-line" })];
    render(<LogsTab />);
    fireEvent.click(screen.getByRole("tab", { name: "Error" }));
    expect(screen.getByText(/No lines match/)).toBeTruthy();
  });

  it("shows a connecting note (not the waiting note) while connecting with no lines", () => {
    state.lines = [];
    state.status = "connecting";
    render(<LogsTab />);
    expect(screen.getByText(/Connecting to the daemon/)).toBeTruthy();
    expect(screen.queryByText(/Waiting for the daemon to log/)).toBeNull();
  });
});

// A focused check that the rows live inside the scrolling console region.
describe("LogsTab console", () => {
  it("renders rows in the mono console container", () => {
    state.lines = [line({ seq: 7, msg: "in-console" })];
    render(<LogsTab />);
    const consoleEl = screen.getByTestId("log-console");
    expect(within(consoleEl).getByText("in-console")).toBeTruthy();
  });
});
