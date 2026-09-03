import { describe, expect, it } from "vitest";
import type { LogEntry } from "@/lib/api";
import { buildTrace } from "@/lib/trace-model";
import { phaseGlyph } from "@/lib/console-trace-view";
import {
  LIVE_GLYPH,
  SPARK_KINDS,
  sparkSummary,
  sparkWeight,
  traceSpark,
} from "@/lib/console-trace-spark";

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

/**
 * A run whose REAL chronology contradicts the strip's order: it runs the suite first, then reads,
 * then edits — so first appearance is verified > oriented > implemented. Over the 453 recorded runs
 * measured for STUDIO-743 the strip's fixed order disagrees with first appearance in 403 of them
 * (89%), so this is the common case, not a curiosity. The strip is a CHECKLIST of kinds, not a
 * timeline, and these tests pin that choice: reordering the slots by first appearance reds them.
 */
const OUT_OF_ORDER: LogEntry[] = [
  entry({ seq: 1, kind: "tool_use", tool: "Bash", text: "command=cargo test --workspace" }),
  entry({ seq: 2, kind: "tool_result", text: "test result: ok" }),
  entry({ seq: 3, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 4, kind: "tool_result", text: "export interface RunSummary" }),
  entry({ seq: 5, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 6, kind: "tool_result", text: "applied" }),
];

/** The order the run's kinds actually first appeared in — the chronology a timeline would draw. */
function firstAppearance(entries: LogEntry[]): string[] {
  const seen: string[] = [];
  for (const phase of buildTrace(entries).phases) {
    if (!seen.includes(phase.kind)) seen.push(phase.kind);
  }
  return seen;
}

describe("traceSpark", () => {
  it("reserves a slot for every phase kind, in the spine's own order", () => {
    const steps = traceSpark(buildTrace(FULL).phases, false);
    // Every kind, present or not — that is what makes column 3 mean "Verified" on EVERY row.
    expect(steps.map((s) => s.kind)).toEqual([...SPARK_KINDS]);
    expect(steps.filter((s) => s.present).map((s) => s.kind)).toEqual([
      "oriented",
      "implemented",
      "verified",
      "coordinated",
      "handoff",
    ]);
    // The vocabulary is the spine's, not a second table that could drift from it.
    expect(steps.map((s) => s.glyph)).toEqual(SPARK_KINDS.map(phaseGlyph));
  });

  it("is a checklist of kinds, NOT the run's chronology", () => {
    // The fixture's real order is verified > oriented > implemented. The strip prints the model's
    // declaration order regardless, because a left-to-right run of glyphs that MOVED per row would
    // claim a timeline the cell is too small to draw honestly (a run has a median of 27 phases).
    expect(firstAppearance(OUT_OF_ORDER)).toEqual(["verified", "oriented", "implemented"]);
    const steps = traceSpark(buildTrace(OUT_OF_ORDER).phases, false);
    expect(steps.filter((s) => s.present).map((s) => s.kind)).toEqual([
      "oriented",
      "implemented",
      "verified",
    ]);
  });

  it("holds each kind in the SAME column whatever the run did", () => {
    // The payoff of the fixed order, and the reason an absent kind keeps its slot rather than
    // collapsing: a row is comparable with the row above it. Measured over the 453 recorded runs,
    // collapsing the gaps put `handoff` in 4 different columns and `other` in 5.
    const full = traceSpark(buildTrace(FULL).phases, false);
    const sparse = traceSpark(buildTrace(OUT_OF_ORDER).phases, false);
    const lonely = traceSpark(buildTrace(FULL.slice(0, 2)).phases, false);
    for (const strip of [full, sparse, lonely]) {
      expect(strip.map((s) => s.kind)).toEqual([...SPARK_KINDS]);
    }
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

  it("marks a kind the run never reached as an empty slot, and never counts it", () => {
    const steps = traceSpark(buildTrace(FULL.slice(0, 2)).phases, false);
    expect(steps.filter((s) => s.present).map((s) => s.kind)).toEqual(["oriented"]);
    for (const step of steps.filter((s) => !s.present)) {
      expect(step.count).toBe(0);
      expect(step.weight).toBe("none");
      // The empty slot still SAYS what it stands for — that is what makes the column readable.
      expect(step.label).not.toBe("");
    }
  });

  it("carries how heavily the run leaned on each kind, so the cell says more than hover", () => {
    // The visible cell is otherwise 41% one value (the modal strip covers 186 of the 453 recorded
    // runs), with all the discriminating detail hidden in the tooltip. Tiering the count by the
    // corpus quartiles (p25=1, median=4, p75=9) takes that from 26 distinct cells to 195, and the
    // modal one from 41% of rows to 3.5%.
    expect(sparkWeight(0)).toBe("none");
    expect(sparkWeight(1)).toBe("light");
    expect(sparkWeight(4)).toBe("mid");
    expect(sparkWeight(5)).toBe("heavy");
    // Interleaved, because the model groups CONSECUTIVE same-kind work into one phase — five reads
    // in a row are one `oriented`, which is exactly the alternation real runs show.
    const many = [...FULL];
    for (let i = 0; i < 5; i += 1) {
      many.push(entry({ seq: 20 + i * 2, kind: "tool_use", tool: "Read", text: "file_path=/a" }));
      many.push(entry({ seq: 21 + i * 2, kind: "tool_use", tool: "Edit", text: "file_path=/a" }));
    }
    const steps = traceSpark(buildTrace(many).phases, false);
    const oriented = steps.find((s) => s.kind === "oriented");
    expect(oriented?.count).toBe(6);
    expect(oriented?.weight).toBe("heavy");
    expect(steps.find((s) => s.kind === "handoff")?.weight).toBe("light");
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

  it("is empty — never six empty slots — when there is no transcript", () => {
    // A run whose transcript nobody has read must not render as a run that DID nothing.
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
      "Oriented ×1 · Implemented ×1 · Verified ×1 · Coordinated ×1 · Handed off ×1 — none: Worked",
    );
  });

  it("names the kinds the run never reached, which the empty slots only imply", () => {
    expect(sparkSummary(traceSpark(buildTrace(FULL.slice(0, 2)).phases, false))).toBe(
      "Oriented ×1 — none: Implemented, Verified, Coordinated, Handed off, Worked",
    );
  });

  it("says so plainly when there is nothing to show", () => {
    expect(sparkSummary([])).toBe("No trace");
  });

  it("does not count the playhead", () => {
    expect(sparkSummary(traceSpark([], true))).toBe("Running now");
  });
});
