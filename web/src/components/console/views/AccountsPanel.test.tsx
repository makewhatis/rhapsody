// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AccountsPanel } from "./AccountsPanel";
import { LimitChips, jobLimitReports } from "./LimitChips";
import type { AccountView } from "@/lib/api";

afterEach(cleanup);
const account = (level: string, extra = {}): AccountView => ({
  account: level, windows: [{ window: "five_hour", utilization: 0.95, resets_at_s: 1600 }],
  status: "allowed", using_credits: false, last_seen_s: 1000, source: "stream",
  stale: false, detection: "stream", level, today_usd: 1.25, cost_kind: "usd", ...extra,
});

describe("AccountsPanel", () => {
  it("renders bars, countdown, a state chip per level, credits, USD or API-equivalent, and detection mode", () => {
    render(<AccountsPanel nowS={1000} accounts={[
      ...["ok", "warn", "stop-new", "handoff", "wall"].map((level) => account(level)),
      account("ok", { account: "Claude", using_credits: true, cost_kind: "api_equivalent", detection: "probe" }),
      account("wall", { account: "ChatGPT", today_usd: null, detection: "wall_only" }),
      account("warn", { account: "paid", detection: "budget" }),
    ]} />);
    expect(screen.getAllByRole("progressbar")).toHaveLength(8);
    expect(screen.getAllByText("resets in 10m")).toHaveLength(8);
    for (const level of ["ok", "warn", "stop-new", "handoff", "wall"]) expect(screen.getAllByText(level).length).toBeGreaterThan(0);
    expect(screen.getByText("credits in use")).toBeTruthy();
    expect(screen.getByText("$1.25 API-equivalent today")).toBeTruthy();
    expect(screen.getAllByText("$1.25 USD today").length).toBeGreaterThan(0);
    for (const mode of ["stream", "probe", "wall-only", "budget"]) expect(screen.getAllByText(mode).length).toBeGreaterThan(0);
    expect(screen.getByText("USD today unknown")).toBeTruthy();
  });
  it("empty and stale states", () => {
    const view = render(<AccountsPanel accounts={[]} nowS={1000} />);
    expect(screen.getByText(/No account observations yet/)).toBeTruthy();
    view.rerender(<AccountsPanel accounts={[account("handoff", { stale: true })]} nowS={1000} />);
    expect(screen.getByText("stale")).toBeTruthy();
    expect(screen.getByText(/last known handoff/)).toBeTruthy();
    expect(screen.getByRole("progressbar").getAttribute("aria-valuenow")).toBe("95");
  });
});

it("Job-page chips for parked, switched, waiting and handed off", () => {
  render(<LimitChips reports={[
    { ticket: "MT-1", account: "Claude", state: "parked", resume_at_s: 1600 },
    { ticket: "MT-1", account: "Claude", state: "switched", model: "openai/gpt" },
    { ticket: "MT-1", account: "Claude", state: "waiting" },
    { ticket: "MT-1", account: "Claude", state: "handed_off", identity: "bob" },
  ]} />);
  expect(screen.getByText(/parked: Claude limit, resumes/)).toBeTruthy();
  expect(screen.getByText("switched to openai/gpt (limit)")).toBeTruthy();
  expect(screen.getByText("waiting: Claude limit")).toBeTruthy();
  expect(screen.getByText("handed to bob")).toBeTruthy();
});

it("keeps a switched engine's history and names a verified identity handoff", () => {
  const event = { run_id: 1, issue_identifier: "MT-1", seq: 3, at: "", kind: "limit.handoff", tool: "", text: JSON.stringify({ ticket: "MT-1", account: "Claude", state: "waiting", identity: "alice" }) };
  expect(jobLimitReports([], [event], 2, "bob", 1)[0]).toMatchObject({ state: "handed_off", identity: "bob" });
  expect(jobLimitReports([], [event], 1, "bob")).toEqual([]);
  expect(jobLimitReports([], [event], 2, "")).toEqual([]);
  expect(jobLimitReports([], [{ ...event, text: "corrupt" }], 2, "bob")).toEqual([]);
  event.text = JSON.stringify({ ticket: "MT-1", account: "Claude", state: "switched", model: "openai/gpt", identity: "alice" });
  expect(jobLimitReports([], [event], 2, "alice", 1)[0].model).toBe("openai/gpt");
  expect(jobLimitReports([], [event], 3, "alice", 2)).toEqual([]);
  expect(jobLimitReports([], [event], 3, "carol", 2)).toEqual([]);
  event.text = JSON.stringify({ ticket: "MT-1", account: "Claude", state: "handed_off", identity: "bob" });
  expect(jobLimitReports([], [event], 1, "bob")[0]).toMatchObject({ state: "handed_off", identity: "bob" });
});
