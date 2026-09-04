import { describe, expect, it } from "vitest";
import type { LogEntry, RunDetail, RunSummary } from "@/lib/api";
import { buildResult, buildTrace } from "@/lib/trace-model";
import {
  TRACE_FILTERS,
  attemptOptions,
  cardLead,
  failingStep,
  filterPhases,
  githubRepo,
  leadParagraph,
  liveRunRow,
  playheadPhase,
  phaseGlyph,
  prSearchUrl,
  relayBatons,
  resultBanner,
  resultEyebrow,
  runBranch,
  runTeammate,
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
    expect(labels(attemptOptions([c], new Map([[547, ""]]), "alice"))).toEqual(["attempt 1 · —"]);
  });

  it("falls back to the run id while no record and no roster can name the attempt", () => {
    expect(labels(attemptOptions([c, a], NONE, ""))).toEqual(["run 547", "run 522"]);
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
      { id: 547, ordinal: 3, label: "attempt 3 · alice", startedAt: "2026-09-03T10:00:00Z" },
      { id: 545, ordinal: 2, label: "run 545", startedAt: "2026-09-03T09:00:00Z" },
      { id: 522, ordinal: 1, label: "run 522", startedAt: "2026-09-03T08:00:00Z" },
    ]);
  });

  it("survives a ticket with no runs at all", () => {
    expect(attemptOptions([], NONE, "")).toEqual([]);
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
