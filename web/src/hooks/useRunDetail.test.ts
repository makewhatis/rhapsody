import { describe, expect, it } from "vitest";
import type { RunDetail } from "@/lib/api";
import { runDetailPollInterval } from "@/hooks/useRunDetail";

function detail(outcome: string): RunDetail {
  return {
    run_id: 1,
    issue_id: "id",
    issue_identifier: "INF-1",
    title: "t",
    project: "",
    repo: "",
    attempt: 0,
    outcome,
    live: outcome === "running",
    issue_state: "",
    last_codex_event: "",
    turn_count: 1,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    usage_estimated: false,
    started_at: "2026-06-07T10:00:00Z",
    ended_at: outcome === "running" ? "" : "2026-06-07T10:10:00Z",
    last_event_at: "",
    error: "",
    recent_events: [],
    generated_at: "",
  };
}

describe("runDetailPollInterval", () => {
  it("polls while the run is running (outcome-driven, not live-driven)", () => {
    expect(runDetailPollInterval(detail("running"))).toBe(2000);
  });

  it("freezes once the run reaches a terminal outcome", () => {
    expect(runDetailPollInterval(detail("completed"))).toBe(false);
    expect(runDetailPollInterval(detail("failed"))).toBe(false);
    expect(runDetailPollInterval(detail("stopped"))).toBe(false);
  });

  it("does not poll before the first payload arrives", () => {
    expect(runDetailPollInterval(undefined)).toBe(false);
  });
});
