import { describe, expect, it } from "vitest";
import type { LogEntry, RunSummary } from "@/lib/api";
import {
  buildJobSummary,
  checksSummary,
  clockTime,
  mergeNote,
  runDescription,
  runMeta,
  runOutcomePill,
  runsNewestFirst,
  transcriptTimeline,
  type PullRequestView,
} from "./console-job-detail";

const NOW = Date.parse("2026-09-01T20:00:00Z");

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

describe("buildJobSummary", () => {
  // §10 box 2.9 — the summary strip's fields come from /issues/<key>/history.
  it("derives every strip field from the run rows", () => {
    const summary = buildJobSummary(
      [
        run({ id: 522, started_at: "2026-08-30T20:21:00Z", ended_at: "2026-08-30T20:45:00Z" }),
        run({ id: 547, branch: "symphony/STUDIO-654", ended_at: "2026-09-01T19:15:00Z" }),
      ],
      { assignee: "alice", nowMs: NOW },
    );
    expect(summary).toMatchObject({
      status: "review",
      statusLabel: "in review",
      assignee: "alice",
      branch: "symphony/STUDIO-654",
      runs: 2,
      title: "Attach a photo in chat",
      project: "tally",
    });
    expect(summary.updated).toBe("45m ago");
  });

  it("reads the live snapshot as running and a failed newest run as blocked", () => {
    expect(buildJobSummary([run({ id: 1, outcome: "running", ended_at: "" })], { live: true, nowMs: NOW }).status).toBe("run");
    expect(buildJobSummary([run({ id: 1, outcome: "failed" })], { nowMs: NOW }).status).toBe("blocked");
  });

  it("reads a live ticket as running even before its run row is persisted", () => {
    // A just-dispatched ticket has no history yet. Losing the live signal here would show it
    // as "queued" — the daemon idle on work it is actively running.
    expect(buildJobSummary([], { live: true, nowMs: NOW }).status).toBe("run");
    // …and a live ticket whose newest stored row is an older completed run is still running.
    expect(buildJobSummary([run({ id: 1, outcome: "completed" })], { live: true, nowMs: NOW }).status).toBe("run");
  });

  // STUDIO-706 — the detail header reads the TICKET's lifecycle, exactly as the worklist row
  // does (STUDIO-702). Without it a merged ticket's header said "in review" forever while its
  // own worklist row said "done": the same ticket, two answers, on two screens.
  it("prefers the ticket's lifecycle over the run outcome", () => {
    const merged = buildJobSummary([run({ id: 1 })], { lifecycle: "done", nowMs: NOW });
    expect(merged.status).toBe("done");
    expect(merged.statusLabel).toBe("done");

    const inReview = buildJobSummary([run({ id: 1 })], { lifecycle: "in_review", nowMs: NOW });
    expect(inReview.status).toBe("review");
    expect(inReview.statusLabel).toBe("in review");

    // Deliberately a STOPPED run: `completed` already falls back to "review", so a merged-only
    // check would pass with the lifecycle ignored entirely. Here the outcome alone says "queued"
    // and only the ticket's state can produce "review".
    expect(
      buildJobSummary([run({ id: 1, outcome: "stopped" })], { lifecycle: "in_review", nowMs: NOW })
        .status,
    ).toBe("review");
  });

  // The worklist's carve-outs are not re-derived here — they come from the one shared
  // `consoleJobStatus`, so the two screens cannot drift. These pin that they still hold.
  it("keeps the worklist's carve-outs: live outranks the ticket, an open ticket keeps blocked", () => {
    // A mid-run handoff parks the ticket in a review state while the agent is still working.
    expect(
      buildJobSummary([run({ id: 1, outcome: "running", ended_at: "" })], {
        live: true,
        lifecycle: "in_review",
        nowMs: NOW,
      }).status,
    ).toBe("run");
    expect(buildJobSummary([], { live: true, lifecycle: "done", nowMs: NOW }).status).toBe("run");
    // `failed` describes the RUN, and a human still has to act on it.
    expect(
      buildJobSummary([run({ id: 1, outcome: "failed" })], { lifecycle: "open", nowMs: NOW }).status,
    ).toBe("blocked");
    // What `open` does override is `completed -> review`.
    expect(buildJobSummary([run({ id: 1 })], { lifecycle: "open", nowMs: NOW }).status).toBe("queued");
  });

  it("falls back to the run outcome when the daemon resolved no lifecycle", () => {
    expect(buildJobSummary([run({ id: 1 })], { nowMs: NOW }).status).toBe("review");
    expect(buildJobSummary([run({ id: 1 })], { lifecycle: "", nowMs: NOW }).status).toBe("review");
    expect(
      buildJobSummary([run({ id: 1 })], { lifecycle: "some_future_state", nowMs: NOW }).status,
    ).toBe("review");
  });

  it("survives a ticket with no runs at all", () => {
    const summary = buildJobSummary([], { nowMs: NOW });
    expect(summary.runs).toBe(0);
    expect(summary.branch).toBe("—");
    expect(summary.updated).toBe("—");
  });

  it("shows no pull request when no caller supplies one", () => {
    // The daemon serves none (see the module's DEPENDENCY note) — the field must stay empty
    // rather than fabricate a number.
    expect(buildJobSummary([run({ id: 1 })], { nowMs: NOW }).pullRequest).toBe("");
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

describe("runMeta / runDescription", () => {
  it("renders the run's window, duration, turns and tokens", () => {
    const meta = runMeta(run({ id: 547, turns: 1 }), "alice");
    expect(meta.identity).toBe("alice");
    expect(meta.window).toMatch(/^\d{2}:\d{2} → \d{2}:\d{2}$/);
    expect(meta.duration).toBe("4m 0s");
    expect(meta.turns).toBe("1 turn");
    expect(meta.tokens).toBe("38.0k tokens");
  });

  it("leaves a running run's window open-ended and marks an estimated total", () => {
    const meta = runMeta(run({ id: 1, ended_at: "", turns: 3, usage_estimated: true }));
    expect(meta.window).toMatch(/^\d{2}:\d{2} →$/);
    expect(meta.turns).toBe("3 turns");
    expect(meta.tokens).toBe("~38.0k tokens");
  });

  it("describes a run by its error when it failed, else by its outcome", () => {
    expect(runDescription(run({ id: 1, outcome: "failed", error: "turn_timeout" }))).toBe("turn_timeout");
    expect(runDescription(run({ id: 1 }))).toBe("completed");
  });
});

describe("transcriptTimeline", () => {
  const entries: LogEntry[] = [
    { seq: 1, kind: "event", tool: "", text: "session started" },
    { seq: 2, kind: "thinking", tool: "", text: "recalled 2 facts from bank" },
    { seq: 3, kind: "tool_use", tool: "Bash", text: "git rebase origin/master" },
    { seq: 4, kind: "tool_result", tool: "", text: "6 conflicts, resolved" },
    { seq: 5, kind: "tool_use", tool: "teams_post", text: "handed off" },
    { seq: 6, kind: "tool_use", tool: "teams_retain", text: "2 facts" },
    { seq: 7, kind: "event", tool: "", text: "turn completed" },
  ];

  // §10 box 2.10 — expanding a run shows its transcript timeline.
  it("types each line and folds a tool result onto the call that produced it", () => {
    const timeline = transcriptTimeline(entries);
    expect(timeline.map((t) => t.kind)).toEqual(["turn", "note", "tool", "post", "retain", "done"]);
    expect(timeline[2]).toMatchObject({ tool: "Bash", result: "6 conflicts, resolved" });
    expect(timeline[3].tool).toBe("teams_post");
  });

  it("keeps an orphaned result as its own line rather than dropping it", () => {
    const timeline = transcriptTimeline([{ seq: 1, kind: "tool_result", tool: "", text: "output" }]);
    expect(timeline).toEqual([{ seq: 1, kind: "note", text: "output", tool: "", result: "" }]);
  });

  it("does not fold a second result onto an already-folded call", () => {
    const timeline = transcriptTimeline([
      { seq: 1, kind: "tool_use", tool: "Bash", text: "ls" },
      { seq: 2, kind: "tool_result", tool: "", text: "first" },
      { seq: 3, kind: "tool_result", tool: "", text: "second" },
    ]);
    // The first result folds (adding no line); the second has nowhere to go and becomes a note.
    expect(timeline).toHaveLength(2);
    expect(timeline[0]).toMatchObject({ kind: "tool", tool: "Bash", result: "first" });
    expect(timeline[1]).toMatchObject({ kind: "note", text: "second" });
  });

  it("renders an unrecognised event line rather than hiding it", () => {
    const timeline = transcriptTimeline([{ seq: 1, kind: "event", tool: "", text: "something new" }]);
    expect(timeline).toEqual([{ seq: 1, kind: "turn", text: "something new", tool: "", result: "" }]);
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
