import { describe, expect, it } from "vitest";
import type { RunSummary } from "@/lib/api";
import {
  checksSummary,
  clockTime,
  mergeNote,
  runOutcomeLabel,
  runOutcomePill,
  runsNewestFirst,
  type PullRequestView,
} from "./console-job-detail";

// STUDIO-742 removed the §4 summary strip, run `.rmeta` line, run one-liner and flat transcript
// timeline along with the view that showed them; their tests went with them. What the "Trace"
// zones still consume from this module is covered below.

function run(over: Partial<RunSummary> & Pick<RunSummary, "id">): RunSummary {
  return {
    issue_id: "i",
    issue_identifier: "STUDIO-654",
    title: "Attach a photo in chat",
    attempt: 1,
    session_uuid: "s",
    branch: "symphony/STUDIO-654",
    project_slug: "tally",
    repo: "",
    started_at: "2026-09-01T19:11:00Z",
    ended_at: "2026-09-01T19:15:00Z",
    outcome: "completed",
    turns: 1,
    input_tokens: 10,
    output_tokens: 20,
    total_tokens: 38_000,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  } as RunSummary;
}

describe("runsNewestFirst", () => {
  // §10 box 2.10 — runs list newest-first.
  it("orders by start time, newest first", () => {
    const ordered = runsNewestFirst([
      run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
      run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 545, started_at: "2026-09-01T16:54:00Z" }),
    ]);
    expect(ordered.map((r) => r.id)).toEqual([547, 545, 522]);
  });

  it("breaks a tie by run id and does not mutate its input", () => {
    const input = [run({ id: 1 }), run({ id: 2 })];
    expect(runsNewestFirst(input).map((r) => r.id)).toEqual([2, 1]);
    expect(input.map((r) => r.id)).toEqual([1, 2]);
  });
});

describe("clockTime", () => {
  it("renders HH:MM and nothing at all for an absent instant", () => {
    expect(clockTime("2026-09-01T19:11:00Z")).toMatch(/^\d{2}:\d{2}$/);
    expect(clockTime("")).toBe("");
    expect(clockTime("not-a-time")).toBe("");
  });
});

describe("runOutcomePill", () => {
  it("keeps a run's own outcome distinct from the ticket's status", () => {
    expect(runOutcomePill("running")).toBe("run");
    expect(runOutcomePill("completed")).toBe("done");
    expect(runOutcomePill("failed")).toBe("blocked");
    // A stopped or interrupted run is emphatically NOT "done".
    expect(runOutcomePill("stopped")).toBe("queued");
    expect(runOutcomePill("interrupted")).toBe("queued");
    expect(runOutcomePill("")).toBe("queued");
  });
});

// STUDIO-763: the header pill shipped carrying the daemon's raw column value ("completed"), which
// the prototype's own `.hd` never shows — its PILL map reads "done" / "running" / "failed".
describe("runOutcomeLabel", () => {
  it("speaks the prototype's vocabulary rather than the daemon's column value", () => {
    expect(runOutcomeLabel("completed")).toBe("done");
    expect(runOutcomeLabel("running")).toBe("running");
    expect(runOutcomeLabel("failed")).toBe("failed");
  });

  // The prototype has no state for these, and inventing one would be a claim about the run that
  // the daemon never made. A word the console does not know is passed through verbatim.
  it("passes through an outcome the prototype has no word for, rather than inventing one", () => {
    expect(runOutcomeLabel("stopped")).toBe("stopped");
    expect(runOutcomeLabel("waiting")).toBe("waiting");
    expect(runOutcomeLabel("interrupted")).toBe("interrupted");
  });

  // An empty outcome is a run the store has no answer for — which is not the same as "done".
  it("names an absent outcome as unknown", () => {
    expect(runOutcomeLabel("")).toBe("unknown");
  });
});

describe("mergeNote / checksSummary", () => {
  const pr = (over: Partial<PullRequestView> = {}): PullRequestView => ({
    number: "#230",
    url: "https://github.com/x/y/pull/230",
    draft: false,
    behind: 0,
    checks: [
      { name: "check", state: "pass", detail: "passed" },
      { name: "pr-title", state: "pass", detail: "passed" },
    ],
    ...over,
  });

  it("tallies checks by state", () => {
    expect(
      checksSummary([
        { name: "a", state: "pass", detail: "" },
        { name: "b", state: "fail", detail: "" },
        { name: "c", state: "pending", detail: "" },
        { name: "d", state: "pass", detail: "" },
      ]),
    ).toEqual({ passed: 2, failed: 1, pending: 1 });
  });

  // §10 box 2.11 — the mergeable/blocked note.
  it("blocks on a failing check before anything else", () => {
    const note = mergeNote(
      pr({ draft: true, checks: [{ name: "test", state: "fail", detail: "failing" }, { name: "lint", state: "pending", detail: "" }] }),
    );
    expect(note).toEqual({ blocked: true, text: "Blocked — 1 failing check." });
  });

  it("reports pending checks, then draft state, then mergeable", () => {
    expect(mergeNote(pr({ checks: [{ name: "test", state: "pending", detail: "" }] }))).toEqual({
      blocked: true,
      text: "Checks running — 1 still pending.",
    });
    expect(mergeNote(pr({ draft: true, behind: 0 }))).toEqual({
      blocked: true,
      text: "Draft · 0 behind · all checks passed. Mark ready to merge.",
    });
    expect(mergeNote(pr({ behind: 2 }))).toEqual({
      blocked: false,
      text: "2 behind · mergeable. All checks passed.",
    });
  });

  it("pluralises the failing-check count", () => {
    const note = mergeNote(
      pr({ checks: [{ name: "a", state: "fail", detail: "" }, { name: "b", state: "fail", detail: "" }] }),
    );
    expect(note.text).toBe("Blocked — 2 failing checks.");
  });
});
