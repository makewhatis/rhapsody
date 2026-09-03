import { describe, expect, it } from "vitest";
import type { LogEntry } from "@/lib/api";
import { buildTrace } from "@/lib/trace-model";
import { phaseGlyph } from "@/lib/console-trace-view";
import { LIVE_GLYPH, SPARK_KINDS, sparkSummary, traceSpark } from "@/lib/console-trace-spark";

// The Jobs worklist's trace sparkline (STUDIO-743, design record §6) — the glance-view half of
// the vocabulary the run-detail spine speaks. Built over the SAME slice-1 phases the spine
// renders, so the two can never drift apart.

function entry(over: Partial<LogEntry> & Pick<LogEntry, "seq">): LogEntry {
  return { kind: "text", tool: "", text: "", ...over };
}

/** A run that reads, edits, tests, posts to the room and hands off — the common real shape. */
const FULL: LogEntry[] = [
  entry({ seq: 1, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 2, kind: "tool_result", text: "export interface RunSummary" }),
  entry({ seq: 3, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 4, kind: "tool_result", text: "applied" }),
  entry({ seq: 5, kind: "tool_use", tool: "Bash", text: "command=cargo test --workspace" }),
  entry({ seq: 6, kind: "tool_result", text: "test result: ok" }),
  entry({ seq: 7, kind: "tool_use", tool: "mcp__symphony__teams_post", text: "message=done" }),
  entry({ seq: 8, kind: "tool_result", text: "posted" }),
  entry({ seq: 9, kind: "tool_use", tool: "mcp__symphony__symphony_handoff", text: "{}" }),
  entry({ seq: 10, kind: "tool_result", text: "moved to In Review" }),
];

describe("traceSpark", () => {
  it("shows one glyph per phase kind the run touched, in the spine's own order", () => {
    const steps = traceSpark(buildTrace(FULL).phases, false);
    expect(steps.map((s) => s.kind)).toEqual([
      "oriented",
      "implemented",
      "verified",
      "coordinated",
      "handoff",
    ]);
    // The vocabulary is the spine's, not a second table that could drift from it.
    expect(steps.map((s) => s.glyph)).toEqual([
      phaseGlyph("oriented"),
      phaseGlyph("implemented"),
      phaseGlyph("verified"),
      phaseGlyph("coordinated"),
      phaseGlyph("handoff"),
    ]);
  });

  it("names each glyph with the phase's own title and how many of them there were", () => {
    const twice = [
      ...FULL,
      entry({ seq: 11, kind: "tool_use", tool: "Read", text: "file_path=/repo/README.md" }),
      entry({ seq: 12, kind: "tool_result", text: "# Rhapsody" }),
    ];
    const steps = traceSpark(buildTrace(twice).phases, false);
    const oriented = steps.find((s) => s.kind === "oriented");
    expect(oriented?.label).toBe("Oriented");
    expect(oriented?.count).toBe(2);
  });

  it("omits a kind the run never reached rather than showing an empty slot", () => {
    const readOnly = FULL.slice(0, 2);
    expect(traceSpark(buildTrace(readOnly).phases, false).map((s) => s.kind)).toEqual(["oriented"]);
  });

  it("appends the playhead, last, while the run is still in flight", () => {
    const steps = traceSpark(buildTrace(FULL).phases, true);
    expect(steps[steps.length - 1]).toMatchObject({ kind: "live", label: "Running now" });
    // The playhead is not a phase, so it never claims a count.
    expect(steps[steps.length - 1].count).toBe(0);
  });

  it("is a lone playhead for a live run that has not logged a tool call yet", () => {
    expect(traceSpark([], true).map((s) => s.kind)).toEqual(["live"]);
  });

  it("is empty — never a fabricated shape — when there is no transcript", () => {
    expect(traceSpark([], false)).toEqual([]);
  });

  it("keeps the playhead apart from every phase glyph, not just apart by colour", () => {
    expect(SPARK_KINDS.map(phaseGlyph)).not.toContain(LIVE_GLYPH);
  });

  it("covers every phase kind the model can produce", () => {
    // A kind missing from SPARK_KINDS would be silently dropped from every sparkline, so the
    // list is asserted against the model's own vocabulary rather than trusted.
    const kinds = new Set(
      buildTrace([
        entry({ seq: 1, kind: "tool_use", tool: "Read", text: "file_path=/a" }),
        entry({ seq: 2, kind: "tool_use", tool: "Edit", text: "file_path=/a" }),
        entry({ seq: 3, kind: "tool_use", tool: "Bash", text: "command=cargo test" }),
        entry({ seq: 4, kind: "tool_use", tool: "mcp__symphony__teams_post", text: "m=1" }),
        entry({ seq: 5, kind: "tool_use", tool: "mcp__symphony__symphony_handoff", text: "{}" }),
        entry({ seq: 6, kind: "tool_use", tool: "SomeUnknownTool", text: "x=1" }),
      ]).phases.map((p) => p.kind),
    );
    for (const kind of kinds) expect(SPARK_KINDS).toContain(kind);
  });
});

describe("sparkSummary", () => {
  it("reads as a sentence a screen reader can announce", () => {
    const steps = traceSpark(buildTrace(FULL).phases, false);
    expect(sparkSummary(steps)).toBe(
      "Oriented ×1 · Implemented ×1 · Verified ×1 · Coordinated ×1 · Handed off ×1",
    );
  });

  it("says so plainly when there is nothing to show", () => {
    expect(sparkSummary([])).toBe("No trace");
  });

  it("does not count the playhead", () => {
    expect(sparkSummary(traceSpark([], true))).toBe("Running now");
  });
});
