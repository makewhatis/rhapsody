import { describe, expect, it } from "vitest";
import type { LogEntry, RunSummary } from "@/lib/api";
import { buildTrace } from "@/lib/trace-model";
import {
  TRACE_FILTERS,
  cardLead,
  filterPhases,
  githubRepo,
  leadParagraph,
  phaseGlyph,
  prSearchUrl,
  resultEyebrow,
  runVitals,
  ticketUrl,
} from "@/lib/console-trace-view";

// The slice-2 view model (STUDIO-742) — the derivations the three-zone run detail needs that are
// not the slice-1 trace model itself: the header's vitals, the spine's filter, and the honest
// links behind the header's actions.

function entry(over: Partial<LogEntry> & Pick<LogEntry, "seq">): LogEntry {
  return { kind: "text", tool: "", text: "", ...over };
}

function run(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 1,
    issue_id: "i",
    issue_identifier: "STUDIO-742",
    title: "Trace",
    attempt: 1,
    session_uuid: "s",
    branch: "symphony/STUDIO-742",
    project_slug: "",
    repo: "git@github.com:makewhatis/rhapsody.git",
    started_at: "2026-09-03T10:00:00Z",
    ended_at: "2026-09-03T10:04:30Z",
    outcome: "completed",
    turns: 3,
    input_tokens: 1,
    output_tokens: 2,
    total_tokens: 38_000,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

const TRANSCRIPT: LogEntry[] = [
  entry({ seq: 1, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 2, kind: "tool_result", text: "export interface RunSummary" }),
  entry({ seq: 3, kind: "thinking", text: "The header needs the branch." }),
  entry({ seq: 4, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 5, kind: "tool_result", text: "applied" }),
  entry({ seq: 6, kind: "tool_use", tool: "Bash", text: "command=npm test" }),
  entry({ seq: 7, kind: "tool_result", text: "Error: 1 failed" }),
];

describe("runVitals — the header's mono strip derives from RunSummary (§3A)", () => {
  it("reads duration from ended−started, and turns/tokens/branch verbatim", () => {
    const v = runVitals(run(), buildTrace(TRANSCRIPT).phases);
    expect(v.duration).toBe("4m 30s");
    expect(v.turns).toBe("3 turns");
    expect(v.tokens).toBe("38.0k");
    expect(v.branch).toBe("symphony/STUDIO-742");
  });

  it("counts the trace's tool calls for the Result card's receipt", () => {
    expect(runVitals(run(), buildTrace(TRANSCRIPT).phases).tools).toBe(3);
  });

  it("marks an estimated token total rather than presenting it as authoritative", () => {
    expect(runVitals(run({ usage_estimated: true }), []).tokens).toBe("~38.0k");
  });

  it("shows a dash, never a fabricated 0s, while the run has not ended", () => {
    expect(runVitals(run({ ended_at: "", outcome: "running" }), []).duration).toBe("—");
  });

  it("shows a dash for a run row that carries no branch", () => {
    expect(runVitals(run({ branch: "" }), []).branch).toBe("—");
  });
});

describe("filterPhases — the spine's filter narrows to matching phases (§3C)", () => {
  const phases = buildTrace(TRANSCRIPT).phases;

  it("offers exactly the four named filters, All first", () => {
    expect(TRACE_FILTERS).toEqual(["all", "edits", "bash", "errors"]);
  });

  it("All keeps every phase", () => {
    expect(filterPhases(phases, "all", "")).toHaveLength(phases.length);
  });

  it("Edits keeps only phases that actually changed a file", () => {
    const kept = filterPhases(phases, "edits", "");
    expect(kept).not.toHaveLength(0);
    kept.forEach((p) => expect(p.effects.some((e) => e.kind === "edited")).toBe(true));
  });

  it("Bash keeps only phases that ran a shell command", () => {
    const kept = filterPhases(phases, "bash", "");
    expect(kept).not.toHaveLength(0);
    kept.forEach((p) => expect(p.did.some((c) => c.tool === "Bash")).toBe(true));
  });

  it("Errors keeps only failing phases", () => {
    const kept = filterPhases(phases, "errors", "");
    expect(kept).not.toHaveLength(0);
    kept.forEach((p) => expect(p.failed).toBe(true));
  });

  it("greps the phase's own text — its title, its calls and its prose", () => {
    expect(filterPhases(phases, "all", "api.ts").length).toBeGreaterThan(0);
    expect(filterPhases(phases, "all", "the branch")).toHaveLength(1);
    expect(filterPhases(phases, "all", "no such string anywhere")).toHaveLength(0);
  });

  it("greps case-insensitively and ignores surrounding whitespace", () => {
    expect(filterPhases(phases, "all", "  NPM TEST ")).toEqual(filterPhases(phases, "all", "npm test"));
    expect(filterPhases(phases, "all", "npm test")).toHaveLength(1);
  });

  it("applies the chip and the grep together, not either-or", () => {
    expect(filterPhases(phases, "edits", "npm test")).toHaveLength(0);
  });
});

describe("resultEyebrow — the Result card says what kind of ending this was (§3B)", () => {
  it("distinguishes a run that handed off from one that merely stopped talking", () => {
    expect(resultEyebrow(run(), "handoff")).toEqual({ text: "done · handed off", tone: "done" });
    expect(resultEyebrow(run(), "text")).toEqual({ text: "done", tone: "done" });
  });

  it("tones a failed run red and a stopped run amber", () => {
    expect(resultEyebrow(run({ outcome: "failed" }), "text")).toEqual({ text: "failed", tone: "fail" });
    expect(resultEyebrow(run({ outcome: "stopped" }), "text")).toEqual({ text: "stopped", tone: "stop" });
  });

  it("names an outcome it does not know rather than claiming the run is done", () => {
    expect(resultEyebrow(run({ outcome: "interrupted" }), "text").text).toBe("interrupted");
    expect(resultEyebrow(run({ outcome: "" }), "fallback").text).toBe("unknown");
  });
});

describe("the header's links are real or absent — never a fabricated PR (§5 dependency rule)", () => {
  it("reads owner/name off both remote spellings", () => {
    expect(githubRepo("git@github.com:makewhatis/rhapsody.git")).toBe("makewhatis/rhapsody");
    expect(githubRepo("https://github.com/makewhatis/rhapsody.git")).toBe("makewhatis/rhapsody");
  });

  it("returns nothing for a host it cannot vouch for, so no link is offered", () => {
    expect(githubRepo("git@gitlab.com:makewhatis/rhapsody.git")).toBe("");
    expect(githubRepo("")).toBe("");
    expect(githubRepo("github.com.evil.example/a/b")).toBe("");
  });

  it("links View PR to a head-branch SEARCH, since no endpoint serves a PR number", () => {
    expect(prSearchUrl(run())).toBe(
      "https://github.com/makewhatis/rhapsody/pulls?q=is%3Apr%20head%3Asymphony%2FSTUDIO-742",
    );
  });

  it("offers no PR link at all when the branch or the remote is missing", () => {
    expect(prSearchUrl(run({ branch: "" }))).toBe("");
    expect(prSearchUrl(run({ repo: "" }))).toBe("");
  });

  it("builds the ticket deep link from the connected workspace, or not at all", () => {
    expect(ticketUrl("studio49", "STUDIO-742")).toBe("https://linear.app/studio49/issue/STUDIO-742");
    expect(ticketUrl("", "STUDIO-742")).toBe("");
    expect(ticketUrl("studio49", "")).toBe("");
  });
});

describe("phaseGlyph — one glyph per phase kind, shared with the Jobs sparkline (§6)", () => {
  it("gives every phase kind a distinct glyph", () => {
    const kinds = ["oriented", "implemented", "verified", "coordinated", "handoff", "other"] as const;
    const glyphs = kinds.map(phaseGlyph);
    expect(new Set(glyphs).size).toBe(kinds.length);
    glyphs.forEach((g) => expect(g).not.toBe(""));
  });
});

describe("cardLead — the Result card shows a lead only when the H1 does not already say it", () => {
  const card = (lead: string, headline: string) =>
    cardLead({ headline, lead, sections: [], source: "text" });

  it("drops a lead the headline was drawn from, rather than printing it twice", () => {
    expect(card("Photo attachment shipped.", "Photo attachment shipped.")).toBe("");
    expect(card("Photo attachment **shipped**.", "Photo attachment shipped.")).toBe("");
  });

  it("keeps a lead that carries more than its opening sentence", () => {
    expect(card("Done. And here is why.", "Done. And here is why. More.")).toBe(
      "Done. And here is why.",
    );
    expect(card("Shipped it.\n\nDetail follows.", "Shipped it.")).toBe(
      "Shipped it.\n\nDetail follows.",
    );
  });

  it("has nothing to show when the prose opened on a heading", () => {
    expect(card("", "Anything")).toBe("");
  });
});

describe("leadParagraph — SAID collapses to its lead (§3C)", () => {
  it("cuts at the first blank line", () => {
    expect(leadParagraph("Lead paragraph.\n\nSecond.\n\nThird.")).toBe("Lead paragraph.");
  });

  it("keeps a multi-line paragraph whole", () => {
    expect(leadParagraph("One line\nand its continuation.\n\nNext.")).toBe(
      "One line\nand its continuation.",
    );
  });

  it("never cuts inside a fenced block, whose blank lines are content", () => {
    const source = "```sh\ncargo test\n\ncargo build\n```\n\nAfter.";
    expect(leadParagraph(source)).toBe("```sh\ncargo test\n\ncargo build\n```");
  });

  it("returns the whole prose when it is a single paragraph", () => {
    expect(leadParagraph("  Just the one.  ")).toBe("Just the one.");
    expect(leadParagraph("")).toBe("");
  });
});
