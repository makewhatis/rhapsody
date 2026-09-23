import { describe, expect, it } from "vitest";
import type { LogEntry, RunDetail, RunProvenance, RunSummary, TicketCostRow } from "@/lib/api";
import { buildResult, buildTrace, type TracePhase } from "@/lib/trace-model";
import {
  PROVENANCE_UNKNOWN,
  REVIEW_STATE_LABELS,
  TRACE_FILTERS,
  harnessFidelity,
  attemptBucket,
  attemptOptions,
  cardLead,
  currentStepLabel,
  failingStep,
  filterPhases,
  githubRepo,
  leadParagraph,
  liveRunRow,
  playheadPhase,
  phaseGlyph,
  prSearchUrl,
  provenanceFields,
  relayBatons,
  resultBanner,
  resultEyebrow,
  reviewOptions,
  reviewState,
  runBranch,
  runTeammate,
  runVitals,
  ticketCostView,
  ticketUrl,
  UNKNOWN_PROVIDER,
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

function phase(over: Partial<TracePhase> & Pick<TracePhase, "title">): TracePhase {
  return {
    id: "1",
    kind: "other",
    subtitle: "",
    turn: 0,
    did: [],
    said: [],
    effects: [],
    failed: false,
    orphanResults: [],
    ...over,
  };
}

// The Jobs worklist's live-activity signal (STUDIO-926) — never a step count, only the most
// recent phase's own title and subtitle, the same pairing the run-detail spine renders per step.
describe("currentStepLabel", () => {
  it("reads the most recent phase's title and subtitle", () => {
    expect(
      currentStepLabel([
        phase({ title: "Oriented", subtitle: "read 2 files" }),
        phase({ title: "Verified", subtitle: "cargo test --workspace" }),
      ]),
    ).toBe("Verified · cargo test --workspace");
  });

  it("drops the separator when the phase has no subtitle", () => {
    expect(currentStepLabel([phase({ title: "Handed off" })])).toBe("Handed off");
  });

  it("returns undefined for a run with no phases yet, rather than a fabricated label", () => {
    expect(currentStepLabel([])).toBeUndefined();
  });
});

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

  // The daemon writes NO branch on any run row — `persist_start_run` leaves the column at its
  // default and is its only writer — so reading the row alone made this vital a permanent dash.
  it("names the branch the daemon's own naming gives the ticket when the row carries none", () => {
    expect(runVitals(run({ branch: "" }), []).branch).toBe("symphony/STUDIO-742");
  });

  it("shows a dash only when there is no ticket to derive a branch from either", () => {
    expect(runVitals(run({ branch: "", issue_identifier: "" }), []).branch).toBe("—");
  });
});

describe("runBranch — the workspace branch, served or derived (§3A)", () => {
  it("prefers the row's own branch whenever the daemon served one", () => {
    expect(runBranch(run({ branch: "symphony/OTHER-1" }))).toBe("symphony/OTHER-1");
    expect(runBranch(run({ branch: "  symphony/OTHER-1  " }))).toBe("symphony/OTHER-1");
  });

  it("derives `symphony/<KEY>` — the frozen branch-naming contract — when it did not", () => {
    expect(runBranch(run({ branch: "", issue_identifier: "STUDIO-742" }))).toBe("symphony/STUDIO-742");
  });

  // The daemon derives the branch from `sanitize_key(identifier)`, not the raw identifier: a key
  // with a character outside `[A-Za-z0-9._-]` names a DIFFERENT branch than the ticket spells.
  it("sanitizes the key exactly as the daemon does, so the name is one it really creates", () => {
    expect(runBranch(run({ branch: "", issue_identifier: "team/issue 1" }))).toBe("symphony/team_issue_1");
    expect(runBranch(run({ branch: "", issue_identifier: "abc.def_ghi-1" }))).toBe("symphony/abc.def_ghi-1");
    expect(runBranch(run({ branch: "", issue_identifier: "." }))).toBe("symphony/_");
  });

  it("derives nothing at all rather than a bare prefix when the row names no ticket", () => {
    expect(runBranch(run({ branch: "", issue_identifier: "" }))).toBe("");
    expect(runBranch(run({ branch: "", issue_identifier: "   " }))).toBe("");
  });
});

describe("resultBanner — the Result card says WHY a run ended badly (§3B)", () => {
  it("carries a failed run's error, red, whether or not it also wrote a hand-off", () => {
    const banner = resultBanner(run({ outcome: "failed", error: "agent exited 1: turn timeout" }));
    expect(banner).toEqual({ label: "Error", tone: "fail", text: "agent exited 1: turn timeout" });
  });

  it("carries a stopped run's reason, amber", () => {
    expect(resultBanner(run({ outcome: "stopped", error: "operator stopped the run" }))).toEqual({
      label: "Reason",
      tone: "stop",
      text: "operator stopped the run",
    });
    expect(resultBanner(run({ outcome: "interrupted", error: "daemon restarted" }))?.tone).toBe("stop");
  });

  it("has no banner for a run that recorded no error", () => {
    expect(resultBanner(run())).toBeNull();
    expect(resultBanner(run({ outcome: "failed", error: "   " }))).toBeNull();
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

  // The head-branch search would otherwise be dead code: no run row the daemon has ever written
  // carries a branch, so the ONLY path that ever fires in production is the derived one.
  it("searches the derived head branch on a row whose branch the daemon left empty", () => {
    expect(prSearchUrl(run({ branch: "" }))).toBe(
      "https://github.com/makewhatis/rhapsody/pulls?q=is%3Apr%20head%3Asymphony%2FSTUDIO-742",
    );
  });

  it("offers no PR link at all when there is no branch to search or no remote to search it on", () => {
    expect(prSearchUrl(run({ branch: "", issue_identifier: "" }))).toBe("");
    expect(prSearchUrl(run({ repo: "" }))).toBe("");
    expect(prSearchUrl(run({ repo: "git@gitlab.example:acme/app.git" }))).toBe("");
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

  // Measured over the 441 recorded runs: requiring whole-lead equality left 184 (41.7%) printing
  // their own H1 again directly under it, because the model GROWS the headline out of the lead.
  it("drops only the sentence the headline was grown from, keeping the rest of the lead", () => {
    expect(card("Postgres is up on 5433. Running the full suite next.", "Postgres is up on 5433.")).toBe(
      "Running the full suite next.",
    );
    expect(card("Shipped it.\n\nDetail follows.", "Shipped it.")).toBe("Detail follows.");
    expect(
      card("**Wired** the watcher. It polls every 2s.", "Wired the watcher."),
    ).toBe("It polls every 2s.");
  });

  it("drops a lead the headline reached PAST — the H1 already carries all of it", () => {
    expect(card("Done. And here is why.", "Done. And here is why. More.")).toBe("");
  });

  // A whole SENTENCE the headline continues past, not merely a string prefix of it.
  it("keeps a lead that only happens to spell the start of the headline", () => {
    expect(card("A", "Absolutely everything changed.")).toBe("A");
    expect(card("Wired", "Wired the watcher end to end.")).toBe("Wired");
  });

  it("keeps a lead whose opening sentence is not the one the headline was grown from", () => {
    expect(card("A first line. A second.", "Something else entirely.")).toBe(
      "A first line. A second.",
    );
  });

  it("has nothing to show when the prose opened on a heading", () => {
    expect(card("", "Anything")).toBe("");
  });

  // 25 of the 446 recorded runs signed off on a bare URL, which carries no sentence punctuation
  // for the walk to end on, so the whole lead — headline included — printed under the H1.
  it("ends the headline's sentence at a line break when it has no punctuation to end on", () => {
    expect(card("Done. Draft PR: https://example.com/pull/5\n\nThe suite is green.", "Done. Draft PR: https://example.com/pull/5")).toBe(
      "The suite is green.",
    );
  });

  // A CLIPPED headline is a prefix of the sentence it came from, so the whole lead trivially
  // starts with it — the answer-first card must still print everything past that sentence.
  // Measured over the 445 recorded runs, treating that prefix as "the H1 already said it" deleted
  // the entire hand-off from 15 of them, one of them a 3,355-char six-paragraph lead.
  it("keeps the rest of a lead the headline could only CLIP", () => {
    const prose = [
      "The CI success gate can never fail under `sh`, which makes it a rubber stamp: every job",
      "reports green whether or not the suite it shells out to actually ran to completion. Your two",
      "questions, both now answered.",
      "",
      "The gate is a one-liner and it swallows the exit status.",
      "",
      "The fix is one flag, and the suite proves it.",
    ].join("\n");
    const built = buildResult([entry({ seq: 1, text: prose })]);
    expect(built.headline.endsWith("\u2026")).toBe(true);
    const rest = cardLead(built);
    expect(rest).toContain("The gate is a one-liner");
    expect(rest).toContain("The fix is one flag");
    expect(rest).not.toBe("");
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

// ---------------------------------------------------------------------------------------------
// STUDIO-744 — slice 3: the live playhead, the failed run's jump-to-failing-step, and the
// attempt relay's baton (design record §3, §4, §9 slice 3).
// ---------------------------------------------------------------------------------------------

function detail(over: Partial<RunDetail> = {}): RunDetail {
  return {
    run_id: 1,
    issue_id: "i",
    issue_identifier: "STUDIO-742",
    title: "Trace",
    project: "",
    repo: "",
    attempt: 1,
    outcome: "running",
    live: true,
    issue_state: "In Progress",
    last_codex_event: "",
    turn_count: 5,
    input_tokens: 3,
    output_tokens: 4,
    total_tokens: 91_000,
    usage_estimated: false,
    started_at: "2026-09-03T10:00:00Z",
    ended_at: "",
    last_event_at: "2026-09-03T10:07:00Z",
    error: "",
    recent_events: [],
    generated_at: "",
    ...over,
  };
}

describe("liveRunRow — the 2s run-detail poll over the issue-history row", () => {
  it("takes the poll's fresher turns, tokens and outcome while the run is live", () => {
    const merged = liveRunRow(
      run({ id: 7, outcome: "running", turns: 1, ended_at: "" }),
      detail({ run_id: 7 }),
    );
    expect(merged.turns).toBe(5);
    expect(merged.total_tokens).toBe(91_000);
    expect(merged.outcome).toBe("running");
    // The identity fields stay the history row's — the poll is telemetry, not a re-identification.
    expect(merged.issue_identifier).toBe("STUDIO-742");
    expect(merged.id).toBe(7);
  });

  it("carries the terminal outcome the poll saw, which the cached history row cannot know", () => {
    const merged = liveRunRow(
      run({ id: 7, outcome: "running", ended_at: "" }),
      detail({ run_id: 7, outcome: "failed", ended_at: "2026-09-03T10:09:00Z", error: "exit 101" }),
    );
    expect(merged.outcome).toBe("failed");
    expect(merged.ended_at).toBe("2026-09-03T10:09:00Z");
    expect(merged.error).toBe("exit 101");
  });

  it("leaves the row untouched when no detail has arrived, or when it is for another run", () => {
    const row = run({ id: 7, outcome: "running", turns: 1 });
    expect(liveRunRow(row, undefined)).toBe(row);
    expect(liveRunRow(row, detail({ run_id: 8, turn_count: 99 }))).toBe(row);
  });

  it("never resurrects a finished row from a stale live snapshot", () => {
    const row = run({ id: 7, outcome: "completed", ended_at: "2026-09-03T10:04:30Z" });
    const merged = liveRunRow(row, detail({ run_id: 7, outcome: "running", ended_at: "" }));
    expect(merged.outcome).toBe("completed");
    expect(merged.ended_at).toBe("2026-09-03T10:04:30Z");
  });

  it("overlays a RUNNING row and nothing else — the only in-flight shape the store writes", () => {
    // `start_run` inserts every row with `OUTCOME_RUNNING` (`crates/store/src/sqlite.rs`),
    // so an ""-outcome row is not a run in progress and has no live telemetry to take.
    const row = run({ id: 7, outcome: "", ended_at: "" });
    expect(liveRunRow(row, detail({ run_id: 7, turn_count: 99 }))).toBe(row);
  });
});

describe("playheadPhase — where a live run's spine sits", () => {
  it("is the NEWEST phase, which is what a streaming run is writing into", () => {
    const phases = buildTrace(TRANSCRIPT).phases;
    expect(phases.length).toBeGreaterThan(1);
    expect(playheadPhase(phases)?.id).toBe(phases[phases.length - 1].id);
  });

  it("is undefined for a transcript that has not arrived", () => {
    expect(playheadPhase([])).toBeUndefined();
  });
});

describe("failingStep — where 'jump to failing step' lands", () => {
  it("names the first failed phase and the seq of its first failed call", () => {
    const phases = buildTrace(TRANSCRIPT).phases;
    const step = failingStep(phases);
    const failed = phases.find((p) => p.failed);
    expect(step).not.toBeNull();
    expect(step?.phaseId).toBe(failed?.id);
    expect(step?.cardSeq).toBe(6);
  });

  it("still names the phase when the failure is a phase-level one with no failing call", () => {
    const phases = buildTrace([
      entry({ seq: 1, kind: "tool_use", tool: "Read", text: "file_path=/a.ts" }),
      entry({ seq: 2, kind: "tool_result", text: "ok" }),
    ]).phases;
    const marked = phases.map((p, i) => (i === 0 ? { ...p, failed: true } : p));
    expect(failingStep(marked)).toEqual({ phaseId: marked[0].id, cardSeq: null });
  });

  it("is null when nothing failed", () => {
    expect(failingStep(buildTrace([entry({ seq: 1, kind: "text", text: "fine" })]).phases)).toBeNull();
    expect(failingStep([])).toBeNull();
  });
});

describe("runTeammate — who a run was, from the records the daemon actually keeps", () => {
  const ROUTED = new Map([
    [522, "alice"],
    [547, "jimmy"],
  ]);
  const NONE = new Map<number, string>();

  it("reads a ticketless review run's reviewer out of its own `pr:` key", () => {
    expect(
      runTeammate(run({ issue_identifier: "pr:makewhatis/rhapsody#12@jimmy" }), NONE, "alice"),
    ).toBe("jimmy");
  });

  // STUDIO-746 — the durable record, which is what survives the run: it outranks the live roster,
  // and it answers for an attempt the live roster has never heard of.
  it("names a run from its OWN durable dispatch identity, over the live fallback", () => {
    expect(runTeammate(run({ id: 547, issue_identifier: "STUDIO-746" }), ROUTED, "alice")).toBe(
      "jimmy",
    );
    expect(runTeammate(run({ id: 522, issue_identifier: "STUDIO-746" }), ROUTED, "")).toBe("alice");
  });

  it("falls back to the live roster only for a run with no durable record at all", () => {
    expect(runTeammate(run({ id: 999, issue_identifier: "STUDIO-746" }), ROUTED, "alice")).toBe(
      "alice",
    );
  });

  // The tri-state's whole point: a run whose ledger says it routed to NOBODY is not a run whose
  // teammate is merely unknown, so the live roster must not answer for it.
  it("names nobody for a run recorded as unrouted, rather than borrowing the live name", () => {
    const unrouted = new Map([[547, ""]]);
    expect(runTeammate(run({ id: 547, issue_identifier: "STUDIO-746" }), unrouted, "alice")).toBe(
      "",
    );
  });

  it("names nobody rather than guessing, when there is nobody to name", () => {
    expect(runTeammate(run({ issue_identifier: "STUDIO-744" }), NONE, "")).toBe("");
    expect(runTeammate(run({ issue_identifier: "pr:owner/repo#1@" }), NONE, "")).toBe("");
  });

  // The `@` is what makes the suffix a name. Without one there is no reviewer in the key, and
  // slicing from a `lastIndexOf` of -1 would render the whole coordinate as a teammate.
  it("names nobody for a `pr:` key carrying no reviewer at all", () => {
    expect(runTeammate(run({ issue_identifier: "pr:owner/repo#1" }), NONE, "alice")).toBe("");
  });
});

describe("attemptOptions — the header selector's \"attempt N · teammate\" labels (STUDIO-763)", () => {
  const a = run({ id: 522, started_at: "2026-09-03T08:00:00Z" });
  const b = run({ id: 545, started_at: "2026-09-03T09:00:00Z" });
  const c = run({ id: 547, started_at: "2026-09-03T10:00:00Z" });
  const newestFirst = [c, b, a];
  const NONE = new Map<number, string>();
  const labels = (opts: readonly { label: string }[]) => opts.map((o) => o.label);

  // The prototype's own selector reads "attempt 1 · alice / attempt 2 · jimmy", and STUDIO-746
  // wired the per-run identity that makes the second half answerable.
  it("labels each attempt with the teammate that attempt was dispatched as", () => {
    const identities = new Map([
      [522, "alice"],
      [545, "jimmy"],
      [547, "alice"],
    ]);
    expect(labels(attemptOptions(newestFirst, identities, ""))).toEqual([
      "attempt 3 · alice",
      "attempt 2 · jimmy",
      "attempt 1 · alice",
    ]);
  });

  // The ordinal is the ticket's OWN ordering of its runs, oldest first — not `runs.attempt`, which
  // the daemon increments on the retry path only, so 432 of 441 recorded rows carry a 0 and an
  // "attempt 0" label repeated three times names none of them.
  it("numbers by the ticket's run order, not by the daemon's retry counter", () => {
    const rows = [
      run({ id: 547, attempt: 0, started_at: "2026-09-03T10:00:00Z" }),
      run({ id: 545, attempt: 0, started_at: "2026-09-03T09:00:00Z" }),
      run({ id: 522, attempt: 0, started_at: "2026-09-03T08:00:00Z" }),
    ];
    const identities = new Map([
      [522, "alice"],
      [545, "alice"],
      [547, "alice"],
    ]);
    expect(labels(attemptOptions(rows, identities, ""))).toEqual([
      "attempt 3 · alice",
      "attempt 2 · alice",
      "attempt 1 · alice",
    ]);
  });

  // The two degradations the acceptance names, and they are DIFFERENT answers: a run whose ledger
  // recorded "nobody" is not a run nothing has answered for yet.
  it("says nobody for a run its own ledger recorded as unrouted", () => {
    const [only] = attemptOptions([c], new Map([[547, ""]]), "alice");
    expect(only.label).toBe("attempt 1 · —");
    // A dash IS an answer, so the option is named and the tooltip still owes the run id.
    expect(only.named).toBe(true);
  });

  it("falls back to the run id while no record and no roster can name the attempt", () => {
    const opts = attemptOptions([c, a], NONE, "");
    expect(labels(opts)).toEqual(["run 547", "run 522"]);
    // Not named, so the view does not repeat the id it is already showing.
    expect(opts.map((o) => o.named)).toEqual([false, false]);
  });

  // The live roster is the documented fallback for a run with no routing row at all, and the
  // caller withholds it until the durable search has answered — so a name from it is a real one.
  it("uses the live-roster fallback the same way the header assignee does", () => {
    expect(labels(attemptOptions([c], NONE, "alice"))).toEqual(["attempt 1 · alice"]);
  });

  // A ticketless review run carries its reviewer IN ITS KEY, so it is named without any ledger.
  it("names a ticketless review attempt from its own `pr:` key", () => {
    const review = run({ id: 560, issue_identifier: "pr:makewhatis/rhapsody#12@jimmy" });
    expect(labels(attemptOptions([review, c], NONE, ""))).toEqual([
      "attempt 2 · jimmy",
      "run 547",
    ]);
  });

  // The run id is the daemon's own unambiguous handle on an attempt, and the ordinal is not: the
  // history endpoint serves at most its newest 50 rows, so on a ticket that ran more the numbering
  // is relative to that window. The handle stays reachable in the tooltip either way.
  it("keeps the daemon's run id and start time on every option, whatever the label says", () => {
    const identities = new Map([[547, "alice"]]);
    expect(attemptOptions(newestFirst, identities, "")).toEqual([
      { id: 547, ordinal: 3, label: "attempt 3 · alice", named: true, startedAt: "2026-09-03T10:00:00Z" },
      { id: 545, ordinal: 2, label: "run 545", named: false, startedAt: "2026-09-03T09:00:00Z" },
      { id: 522, ordinal: 1, label: "run 522", named: false, startedAt: "2026-09-03T08:00:00Z" },
    ]);
  });

  it("survives a ticket with no runs at all", () => {
    expect(attemptOptions([], NONE, "")).toEqual([]);
  });
});

describe("reviewOptions — the run detail's review strip (STUDIO-976)", () => {
  const NONE = new Map<number, string>();
  const labels = (opts: readonly { label: string }[]) => opts.map((o) => o.label);

  // A review is NOT an attempt: it is never numbered, and it names its reviewer. The reviewer comes
  // from the run's own `pr:…@reviewer` key, so a review run needs no ledger lookup at all.
  it("labels each review by its reviewer and never by an ordinal", () => {
    const reviews = [
      run({ id: 601, issue_identifier: "pr:makewhatis/rhapsody#147@alice" }),
      run({ id: 602, issue_identifier: "pr:makewhatis/rhapsody#147@sol" }),
    ];
    const opts = reviewOptions(reviews, NONE, "");
    expect(labels(opts)).toEqual(["review · alice", "review · sol"]);
    expect(opts.map((o) => o.named)).toEqual([true, true]);
  });

  // The identity invariant `JobDetailView` states: the entry's reviewer resolves through the SAME
  // `runTeammate` the header assignee, the baton and the attempt labels use. A durable routing row
  // that says somebody else for this run id may not make the strip disagree with the header — the
  // review's own key wins, exactly as [`runTeammate`] specifies for a `pr:` run.
  it("resolves the reviewer through the same source as the header, not the routing ledger", () => {
    const review = run({ id: 601, issue_identifier: "pr:owner/repo#1@alice" });
    const identities = new Map([[601, "bob"]]);
    expect(labels(reviewOptions([review], identities, "carol"))).toEqual(["review · alice"]);
    // Pinned against the shared resolver itself, so a second source here would disagree visibly.
    expect(runTeammate(review, identities, "carol")).toBe("alice");
  });

  // A `pr:` key with no `@` names no reviewer; the run id is then the daemon's own handle, and the
  // tooltip still owes the start time.
  it("falls back to the bare run id when the key carries no reviewer", () => {
    const reviews = [
      run({ id: 601, issue_identifier: "pr:owner/repo#1", started_at: "2026-09-03T10:00:00Z" }),
    ];
    expect(reviewOptions(reviews, NONE, "")).toEqual([
      {
        id: 601,
        label: "review 601",
        named: false,
        startedAt: "2026-09-03T10:00:00Z",
        state: "none",
      },
    ]);
  });

  it("renders nothing for a ticket the daemon credited no reviews to", () => {
    expect(reviewOptions([], NONE, "")).toEqual([]);
  });

  // STUDIO-1020 — each round's chip carries its OWN verdict state, read from the run rather than
  // from the per-(PR, reviewer) watch set, which holds only the latest status. The four states are
  // exclusive, and `ended_at` decides "reviewing" before the verdict does.
  describe("each round's verdict state (STUDIO-1020)", () => {
    const key = (id: number) => `pr:makewhatis/rhapsody#223@${id === 1 ? "jimmy" : "alice"}`;

    it("reads reviewing while the run has not ended, whatever a stale verdict says", () => {
      const running = run({
        id: 1,
        issue_identifier: key(1),
        ended_at: "",
        verdict: "approved",
      });
      expect(reviewState(running)).toBe("reviewing");
      expect(reviewOptions([running], NONE, "")[0].state).toBe("reviewing");
    });

    it("reads the daemon's own verdict once the run has ended", () => {
      const approved = run({
        id: 1,
        issue_identifier: key(1),
        ended_at: "2026-09-03T10:04:30Z",
        verdict: "approved",
      });
      const changes = run({
        id: 2,
        issue_identifier: key(2),
        ended_at: "2026-09-03T10:04:30Z",
        verdict: "changes_requested",
      });
      expect(reviewState(approved)).toBe("approved");
      expect(reviewState(changes)).toBe("changes_requested");
      // Two rounds of one pull request keep their own answers — the whole point of per-run records.
      expect(reviewOptions([approved, changes], NONE, "").map((o) => o.state)).toEqual([
        "approved",
        "changes_requested",
      ]);
    });

    it("reads neutral for an ended round the daemon recorded no verdict for", () => {
      for (const over of [
        { outcome: "failed" },
        { outcome: "stopped" },
        { outcome: "completed" }, // a truncated/undeclared round also lands here
      ]) {
        const round = run({
          id: 1,
          issue_identifier: key(1),
          ended_at: "2026-09-03T10:04:30Z",
          ...over,
        });
        expect(reviewState(round)).toBe("none");
      }
      // A value this build does not recognise is neutral, never rounded to either verdict.
      const unknown = run({
        id: 1,
        issue_identifier: key(1),
        ended_at: "2026-09-03T10:04:30Z",
        verdict: "lgtm" as never,
      });
      expect(reviewState(unknown)).toBe("none");
    });

    it("gives each state a tooltip phrase and none to the neutral chip", () => {
      expect(REVIEW_STATE_LABELS.reviewing).toBe("reviewing");
      expect(REVIEW_STATE_LABELS.changes_requested).toBe("changes requested");
      expect(REVIEW_STATE_LABELS.approved).toBe("approved");
      expect(REVIEW_STATE_LABELS.none).toBe("");
    });
  });
});

describe("attemptBucket — the single-row breakpoint the header publishes (STUDIO-763)", () => {
  // The stylesheet grants the one-row header per count because the width it costs is not a
  // constant: the selector grows ~110px per attempt while every other member is fixed. These are
  // the four thresholds `console-trace.css` is written against — 1100 / 1280 / 1400 / 1700, where
  // 1100 is the desktop window's own default width (`desktop/src-tauri/tauri.conf.json`).
  it("names one bucket per measured breakpoint", () => {
    expect(attemptBucket(1)).toBe("1");
    expect(attemptBucket(2)).toBe("2");
    expect(attemptBucket(3)).toBe("3");
    expect(attemptBucket(4)).toBe("few");
    expect(attemptBucket(5)).toBe("few");
  });

  // Five is the widest selector a 1728px display holds whole; at six every label ellipsizes at
  // every width up to 1920, so there is no threshold to grant and this bucket never gets the row.
  it("separates the counts no display fits from the ones a wide one does", () => {
    expect(attemptBucket(6)).toBe("many");
    expect(attemptBucket(9)).toBe("many");
  });

  // A ticket the console has fetched no history for yet renders the header before its runs land,
  // and a bucket the stylesheet has no rule for would leave that header with no breakpoint at all.
  it("puts a ticket with no attempts yet in the narrowest bucket, not a fifth one", () => {
    expect(attemptBucket(0)).toBe("1");
  });
});

describe("relayBatons — the handoff baton the attempt selector switches between", () => {
  const older = run({ id: 522, started_at: "2026-09-03T08:00:00Z" });
  const newer = run({ id: 547, started_at: "2026-09-03T10:00:00Z" });
  const relay = [newer, older]; // newest-first, as `runsNewestFirst` orders it
  const NONE = new Map<number, string>();

  it("hands the baton IN to a run that follows another, naming both teammates", () => {
    const review = run({ id: 547, issue_identifier: "pr:makewhatis/rhapsody#12@jimmy" });
    const { incoming, outgoing } = relayBatons([review, older], review, NONE, "alice");
    expect(incoming).toEqual({ from: "alice", to: "jimmy", text: "alice → jimmy" });
    expect(outgoing).toBeNull();
  });

  it("hands the baton OUT of the run its successor picked up from", () => {
    const review = run({ id: 547, issue_identifier: "pr:makewhatis/rhapsody#12@jimmy" });
    const { incoming, outgoing } = relayBatons([review, older], older, NONE, "alice");
    expect(incoming).toBeNull();
    expect(outgoing).toEqual({ from: "alice", to: "jimmy", text: "alice → jimmy" });
  });

  // STUDIO-746 — the relay the design record's §6 is actually about: two attempts of ONE ticket,
  // each naming the teammate its own dispatch recorded. Before the per-run identity both sides
  // resolved to the ticket's single name and the row could only say "run 522 → run 547".
  it("names each attempt's OWN teammate across an implement→review relay", () => {
    const identities = new Map([
      [522, "alice"],
      [547, "jimmy"],
    ]);
    expect(relayBatons(relay, newer, identities, "").incoming).toEqual({
      from: "alice",
      to: "jimmy",
      text: "alice → jimmy",
    });
    expect(relayBatons(relay, older, identities, "").outgoing).toEqual({
      from: "alice",
      to: "jimmy",
      text: "alice → jimmy",
    });
  });

  it("names the runs, not a teammate handing to herself, when one identity covers both", () => {
    const { incoming } = relayBatons(relay, newer, NONE, "alice");
    expect(incoming).toEqual({ from: "alice", to: "alice", text: "alice · run 522 → run 547" });
  });

  it("still marks the relay when no teammate resolves at all", () => {
    const { incoming } = relayBatons(relay, newer, NONE, "");
    expect(incoming).toEqual({ from: "", to: "", text: "run 522 → run 547" });
  });

  it("gives a ticket's only run no baton in either direction", () => {
    expect(relayBatons([newer], newer, NONE, "alice")).toEqual({ incoming: null, outgoing: null });
  });

  it("gives a run the list does not contain no baton, rather than guessing a neighbour", () => {
    expect(relayBatons(relay, run({ id: 999 }), NONE, "alice")).toEqual({
      incoming: null,
      outgoing: null,
    });
  });
});

// The run-detail provenance line (STUDIO-909) — what the run actually ran on, each value with the
// config key it came from. The origin is the load-bearing half: the failure this exists to make
// visible was an override nothing else on the run named.
describe("provenanceFields — the header's provenance line (STUDIO-909)", () => {
  it("renders all three values with the origin of each configurable one", () => {
    const p: RunProvenance = {
      run_id: 7,
      harness: "opencode",
      harness_origin: "profile",
      model: "deepseek-v4p1-flash",
      model_origin: "review.model.opencode",
      provider: "fireworks-ai",
    };
    expect(provenanceFields(p)).toEqual([
      { label: "harness", value: "opencode", origin: "profile" },
      { label: "model", value: "deepseek-v4p1-flash", origin: "review.model.opencode" },
      // Derived from the harness + model at dispatch, so there is no config key to name.
      { label: "provider", value: "fireworks-ai", origin: "" },
    ]);
  });

  it("reads the model's origin from model_origin, never the profile or harness origin", () => {
    // The exact undiagnosable shape: the teammate's profile named one model, the operator's
    // review.model.opencode substituted another. Reading harness_origin here (or the profile's
    // origin) would report "profile" and hide the override — the bug this field exists to expose.
    const fields = provenanceFields({
      run_id: 7,
      harness: "opencode",
      harness_origin: "profile",
      model: "fireworks-ai/x",
      model_origin: "review.model.opencode",
      provider: "fireworks-ai",
    });
    expect(fields.find((f) => f.label === "model")?.origin).toBe("review.model.opencode");
  });

  it("says unknown for every value of a run that recorded none, and names no origin", () => {
    expect(provenanceFields(undefined)).toEqual([
      { label: "harness", value: PROVENANCE_UNKNOWN, origin: "" },
      { label: "model", value: PROVENANCE_UNKNOWN, origin: "" },
      { label: "provider", value: PROVENANCE_UNKNOWN, origin: "" },
    ]);
  });

  it("treats an empty recorded value as unknown rather than showing a blank", () => {
    const fields = provenanceFields({
      run_id: 7,
      harness: "claude",
      harness_origin: "agent.backend",
      model: "",
      model_origin: "claude.model",
      provider: "",
    });
    expect(fields.find((f) => f.label === "model")).toEqual({
      label: "model",
      value: PROVENANCE_UNKNOWN,
      origin: "",
    });
    expect(fields.find((f) => f.label === "harness")).toEqual({
      label: "harness",
      value: "claude",
      origin: "agent.backend",
    });
  });
});

// The run detail's whole-ticket token total (STUDIO-975) — the figure that used to require adding
// up every attempt by hand. It reads only the daemon's per-(ticket, provider) ledger, never a fold
// over the attempt list, and a ticket the ledger does not carry is an honest "—", not a 0.
describe("ticketCostView — the run detail's whole-ticket total (STUDIO-975)", () => {
  const cost = (
    ticket: string,
    provider: string,
    total_tokens: number,
    usage_estimated = false,
  ): TicketCostRow => ({ ticket, provider, total_tokens, usage_estimated });

  it("sums the ledger's provider buckets into one whole-ticket total, largest first", () => {
    const v = ticketCostView(
      [cost("STUDIO-974", "anthropic", 200_000), cost("STUDIO-974", "fireworks-ai", 607_780)],
      "STUDIO-974",
    );
    expect(v?.total).toBe("807.8k");
    expect(v?.buckets).toEqual(["607.8k fireworks-ai", "200.0k anthropic"]);
  });

  // A ticket's earlier rounds are absent from `/history/issues` (one row per key) — the ledger is
  // the only input that carries them, so this view can never drop them the way that fold would.
  it("totals every round the ledger carries, not only the newest", () => {
    const v = ticketCostView(
      [cost("STUDIO-974", "anthropic", 93_600_000), cost("STUDIO-974", "anthropic", 2_400_000)],
      "STUDIO-974",
    );
    expect(v?.total).toBe("96.0M");
  });

  // The ticket's own warning: `provider: ""` is real spend. Dropping the bucket would make the
  // parts stop summing to the whole.
  it("labels the empty-provider bucket rather than hiding it, and counts it in the total", () => {
    const v = ticketCostView([cost("STUDIO-1", "", 42), cost("STUDIO-1", "anthropic", 100)], "STUDIO-1");
    expect(v?.total).toBe("142");
    expect(v?.buckets).toEqual(["100 anthropic", `42 ${UNKNOWN_PROVIDER}`]);
  });

  // A bucket that ended without a clean `result` event is a floor; a total with one floored input
  // is itself a floor, marked with the same "~" `runVitals.tokens` uses.
  it("marks the total, and the floored bucket, with a leading tilde", () => {
    const v = ticketCostView([cost("STUDIO-2", "anthropic", 1_000_000, true)], "STUDIO-2");
    expect(v?.total).toBe("~1.0M");
    expect(v?.buckets).toEqual(["~1.0M anthropic"]);
  });

  it("returns null — never a confident zero — for a ticket the ledger does not carry", () => {
    expect(ticketCostView([cost("STUDIO-3", "anthropic", 5)], "STUDIO-4")).toBeNull();
    // A zero-token row is "spent nothing". The SERVER's ledger does return such rows (its SQL has
    // no HAVING); it is this module's `ticketCostsByIssue` that drops `total_tokens <= 0`. Either
    // way the detail renders "—" for it exactly as it does for an absent ticket.
    expect(ticketCostView([cost("STUDIO-5", "anthropic", 0)], "STUDIO-5")).toBeNull();
  });

  // STUDIO-978: the observability a harness actually offers, read from run provenance. The two
  // rules map the DESIGN's dividing line onto the view: reduced event fidelity is stated, and a
  // harness that cannot be steered hides the composer.
  describe("harnessFidelity", () => {
    it("reduces the spine only for a final-text-only harness", () => {
      expect(harnessFidelity({ run_id: 1, harness_events: "final_text_only" }).reducedEvents).toBe(
        true,
      );
      expect(harnessFidelity({ run_id: 1, harness_events: "structured" }).reducedEvents).toBe(false);
    });

    // MUTATION GUARD: treat FinalTextOnly as structured (e.g. `!== "structured"` or reading the
    // wrong field) and the first assertion reds — the operator would see a blank spine with no
    // explanation.
    it("does not reduce the spine when the shape is unknown or structured", () => {
      expect(harnessFidelity({ run_id: 1 }).reducedEvents).toBe(false);
      expect(harnessFidelity(undefined).reducedEvents).toBe(false);
    });

    // MUTATION GUARD: expose steering for Steering::None and this reds.
    it("hides the composer ONLY when the harness explicitly cannot be steered", () => {
      expect(harnessFidelity({ run_id: 1, harness_steering: "none" }).steeringAvailable).toBe(false);
      expect(harnessFidelity({ run_id: 1, harness_steering: "live" }).steeringAvailable).toBe(true);
      expect(
        harnessFidelity({ run_id: 1, harness_steering: "between_turns" }).steeringAvailable,
      ).toBe(true);
      // Absence is "unknown", not "none": a legacy run keeps its working composer.
      expect(harnessFidelity({ run_id: 1 }).steeringAvailable).toBe(true);
      expect(harnessFidelity(undefined).steeringAvailable).toBe(true);
    });
  });
});
