import { describe, it, expect } from "vitest";
import type { LogEntry, RunSummary } from "@/lib/api";
import {
  baseToolName,
  buildResult,
  buildTrace,
  parseToolArgs,
  toolPhaseKind,
} from "@/lib/trace-model";

let seq = 0;
function entry(kind: LogEntry["kind"], text: string, tool = ""): LogEntry {
  seq += 1;
  return { seq, kind, tool, text };
}
const use = (tool: string, text: string) => entry("tool_use", text, tool);
const res = (text: string) => entry("tool_result", text);
const say = (text: string) => entry("text", text);
const think = (text: string) => entry("thinking", text);
const ev = (text: string) => entry("event", text);

function run(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 1,
    issue_id: "i1",
    issue_identifier: "STUDIO-741",
    title: "Trace model",
    attempt: 1,
    session_uuid: "u",
    branch: "symphony/STUDIO-741",
    project_slug: "",
    repo: "",
    started_at: "2026-09-03T10:00:00Z",
    ended_at: "2026-09-03T10:46:00Z",
    outcome: "completed",
    turns: 12,
    input_tokens: 1,
    output_tokens: 1,
    total_tokens: 2,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

// A realistic run, shaped exactly the way `crates/agent/src/humanize.rs` renders one: a `tool_use`
// text is the tool's input object as SORTED `key=value` pairs, and the agent works mostly through
// `Bash` (78% of 77k real tool calls in ~/.rhapsody/logs).
function realisticTranscript(): LogEntry[] {
  seq = 0;
  return [
    ev("session started"),
    think("The watcher must be edge-triggered, not level."),
    use("Read", "file_path=crates/orchestrator/src/reviewintro.rs"),
    res("318 lines"),
    use("Bash", 'command=grep -rn "dispatch_review" crates/ description=Find dispatch sites'),
    res("4 hits"),
    say("The store already exposes `load_live_review_watch`."),
    ev("turn completed"),
    use("Write", "content=pub(crate) async fn sweep… file_path=crates/orchestrator/src/reviewwatch.rs"),
    res("+178"),
    use("Edit", "file_path=crates/orchestrator/src/reviewintro.rs new_string=… old_string=…"),
    res("(ok)"),
    use("Bash", "command=cargo test --workspace description=Run the suite"),
    res("test result: ok. 5 passed; 0 failed; 0 ignored"),
    use("mcp__symphony__teams_post", 'body=STUDIO-721 is up for review refs=["STUDIO-721"]'),
    res("(ok)"),
    use("mcp__symphony__teams_retain", "content=The edge trigger cannot be a sha comparison"),
    res("(ok)"),
    // The shape 543 of 554 real handoffs use: a lead paragraph, then labelled headings.
    say("Wired the edge-triggered watcher.\n\n### What shipped\n- New watcher task.\n\n### Verification\n- `cargo test` green.\n\n### Follow-ups\n- None."),
    use("mcp__symphony__symphony_handoff", ""),
    res("(ok)"),
    ev("turn completed"),
  ];
}

describe("baseToolName", () => {
  // The daemon serves the tool name VERBATIM, so room/memory tools arrive MCP-prefixed. A
  // classifier matching bare "teams_post" would silently never fire on a real transcript.
  it("strips the mcp server prefix, including a server name with underscores", () => {
    expect(baseToolName("mcp__symphony__teams_post")).toBe("teams_post");
    expect(baseToolName("mcp__claude_ai_Linear__save_comment")).toBe("save_comment");
    expect(baseToolName("Bash")).toBe("Bash");
    expect(baseToolName("")).toBe("");
  });
});

describe("parseToolArgs", () => {
  it("splits the humanizer's sorted key=value summary", () => {
    expect(parseToolArgs("command=cargo test --workspace description=Run the suite")).toEqual({
      command: "cargo test --workspace",
      description: "Run the suite",
    });
  });

  it("keeps an `=` inside a value that does not start a sorted-later key", () => {
    // `CGO_ENABLED` sorts BEFORE `command`, so it cannot be a real next key — the humanizer
    // emits keys in sorted order. Without that check this splits mid-command.
    expect(parseToolArgs("command=CGO_ENABLED=1 go test ./... description=Test")).toEqual({
      command: "CGO_ENABLED=1 go test ./...",
      description: "Test",
    });
  });

  it("returns nothing for prose that is not a key=value summary", () => {
    expect(parseToolArgs("just some words")).toEqual({});
    expect(parseToolArgs("")).toEqual({});
  });
});

describe("toolPhaseKind", () => {
  it("maps the named tools per the design record", () => {
    expect(toolPhaseKind("Read", "file_path=a.rs")).toBe("oriented");
    expect(toolPhaseKind("Grep", "pattern=foo")).toBe("oriented");
    expect(toolPhaseKind("Edit", "file_path=a.rs")).toBe("implemented");
    expect(toolPhaseKind("Write", "file_path=a.rs")).toBe("implemented");
    expect(toolPhaseKind("mcp__symphony__teams_post", "body=hi")).toBe("coordinated");
    expect(toolPhaseKind("mcp__symphony__teams_retain", "content=x")).toBe("coordinated");
    expect(toolPhaseKind("mcp__symphony__symphony_handoff", "")).toBe("handoff");
  });

  // Bash is 78% of real tool calls, so classifying on the tool NAME alone collapses almost the
  // whole run into one phase. The command's own verb is the real signal.
  it("classifies Bash by its command verb, not by the tool name", () => {
    expect(toolPhaseKind("Bash", "command=cargo test --workspace")).toBe("verified");
    expect(toolPhaseKind("Bash", "command=make lint")).toBe("verified");
    expect(toolPhaseKind("Bash", "command=npm run build")).toBe("verified");
    expect(toolPhaseKind("Bash", "command=go test ./...")).toBe("verified");
    expect(toolPhaseKind("Bash", 'command=grep -rn "foo" crates/')).toBe("oriented");
    expect(toolPhaseKind("Bash", "command=sed -n '1,40p' src/lib.rs")).toBe("oriented");
    expect(toolPhaseKind("Bash", "command=git log --oneline -5")).toBe("oriented");
    expect(toolPhaseKind("Bash", "command=sed -i 's/a/b/' src/lib.rs")).toBe("implemented");
    expect(toolPhaseKind("Bash", "command=git commit -m 'x'")).toBe("implemented");
    expect(toolPhaseKind("Bash", "command=gh pr comment 96 --body hi")).toBe("coordinated");
    expect(toolPhaseKind("Bash", "command=gh pr view --json number")).toBe("oriented");
  });

  // `cd` is the single most common first word (8458 real calls) because commands are
  // `cd /x && <the real verb>`. Reading only the first word classifies nearly everything wrong.
  it("looks past a leading `cd` and env assignments to the real verb", () => {
    expect(toolPhaseKind("Bash", "command=cd /w/web && npm test")).toBe("verified");
    expect(toolPhaseKind("Bash", "command=cd /w && grep -rn foo .")).toBe("oriented");
  });

  // A loop body splits into segments that begin with shell grammar, not with a command.
  it("looks past shell grammar and command wrappers", () => {
    expect(toolPhaseKind("Bash", "command=for f in *.rs; do grep -n foo $f; done")).toBe("oriented");
    expect(toolPhaseKind("Bash", "command=export RUST_LOG=debug && cargo test")).toBe("verified");
    expect(toolPhaseKind("Bash", "command=timeout 300 cargo test --workspace")).toBe("verified");
  });

  // Infra tools interleave flags that carry values, so the first positional is often the
  // NAMESPACE rather than the verb — the verb has to be looked for, not assumed.
  it("finds an infra verb past flags that carry values", () => {
    expect(toolPhaseKind("Bash", "command=kubectl --context home -n flagsmith get deploy")).toBe(
      "oriented",
    );
    expect(toolPhaseKind("Bash", "command=kubectl -n booch apply -f deploy.yaml")).toBe(
      "implemented",
    );
  });

  it("leaves a command it cannot read unclassified rather than guessing", () => {
    expect(toolPhaseKind("Bash", "command=some_unknown_bin --with args")).toBe("other");
    // The humanizer clips a value at 60 runes; a command cut mid-word cannot be classified.
    expect(toolPhaseKind("Bash", "command=…")).toBe("other");
    expect(toolPhaseKind("Skill", "command=x")).toBe("other");
  });

  // This module's contract is tolerating ARBITRARY agent output. A verb spelled like an
  // `Object.prototype` member used to reach the runner table through the prototype chain and throw
  // out of `buildTrace` entirely — a blank run-detail view, not one mis-titled phase.
  it("does not read a command named after an Object.prototype member off the prototype chain", () => {
    for (const cmd of [
      "constructor",
      "toString",
      "valueOf",
      "hasOwnProperty",
      "__proto__",
      "isPrototypeOf",
      "propertyIsEnumerable",
    ]) {
      expect(toolPhaseKind("Bash", `command=${cmd} test`), cmd).toBe("other");
      seq = 0;
      expect(() =>
        buildTrace([use("Bash", `command=${cmd} --help description=x`), res("(ok)")]),
        cmd,
      ).not.toThrow();
    }
  });
});

describe("buildTrace — phase grouping", () => {
  it("groups a realistic transcript into plain-language phases", () => {
    const trace = buildTrace(realisticTranscript());
    expect(trace.phases.map((p) => p.title)).toEqual([
      "Oriented",
      "Implemented",
      "Verified",
      "Coordinated",
      "Handed off",
    ]);
    expect(trace.grouping).toBe("turns");
  });

  it("degrades a marker-less transcript to tool-cluster grouping, never to a flat list", () => {
    seq = 0;
    const trace = buildTrace([
      use("Read", "file_path=a.rs"),
      res("10 lines"),
      use("Read", "file_path=b.rs"),
      res("20 lines"),
      use("Edit", "file_path=a.rs"),
      res("(ok)"),
      use("Bash", "command=cargo test"),
      res("test result: ok. 1 passed; 0 failed"),
    ]);
    expect(trace.grouping).toBe("clusters");
    expect(trace.phases.map((p) => p.title)).toEqual(["Oriented", "Implemented", "Verified"]);
    // Clustered, not flat: the two Reads share one phase.
    expect(trace.phases[0].did).toHaveLength(2);
  });

  it("yields a single phase for an unmarked single-kind transcript", () => {
    seq = 0;
    const trace = buildTrace([use("Read", "file_path=a.rs"), res("10 lines")]);
    expect(trace.grouping).toBe("single");
    expect(trace.phases).toHaveLength(1);
  });

  // The common real shape: `humanize.rs` emits one `session started` and one `turn completed` per
  // session, so the dividers are present and honoured but fall either side of the work. The label
  // must say the dividers were used — slice 2 shows it to the operator as an honesty statement.
  it("reports turn grouping when the dividers were honoured, even if they split nothing", () => {
    seq = 0;
    const trace = buildTrace([
      ev("session started"),
      use("Read", "file_path=a.rs"),
      res("10 lines"),
      use("Edit", "file_path=a.rs"),
      res("(ok)"),
      ev("turn completed"),
    ]);
    expect(trace.grouping).toBe("turns");
    expect(trace.phases.map((p) => p.turn)).toEqual([0, 0]);
  });

  it("returns no phases for an empty transcript", () => {
    expect(buildTrace([]).phases).toEqual([]);
  });

  it("keeps prose that sits between two same-kind clusters inside one phase", () => {
    seq = 0;
    const trace = buildTrace([
      use("Read", "file_path=a.rs"),
      res("10 lines"),
      say("Now the other file."),
      use("Read", "file_path=b.rs"),
      res("20 lines"),
    ]);
    expect(trace.phases).toHaveLength(1);
    expect(trace.phases[0].did).toHaveLength(2);
    expect(trace.phases[0].said).toHaveLength(1);
  });
});

describe("buildTrace — DID/SAID", () => {
  it("splits DID from SAID and pairs each result to its call", () => {
    const trace = buildTrace(realisticTranscript());
    const oriented = trace.phases[0];
    expect(oriented.did.map((c) => c.tool)).toEqual(["Read", "Bash"]);
    expect(oriented.did[0].result).toBe("318 lines");
    expect(oriented.did[1].result).toBe("4 hits");
    expect(oriented.said.map((s) => s.kind)).toEqual(["thinking", "text"]);
    // A card's result points back at the entry that carried it.
    expect(oriented.did[0].resultSeq).toBe(4);
  });

  it("pairs a result to the nearest preceding unpaired call, not the newest card", () => {
    seq = 0;
    const trace = buildTrace([
      use("Read", "file_path=a.rs"),
      use("Read", "file_path=b.rs"),
      res("for a"),
      res("for b"),
    ]);
    expect(trace.phases[0].did.map((c) => c.result)).toEqual(["for a", "for b"]);
  });

  // Parallel calls of DIFFERENT kinds open different phases, but their results still arrive
  // together afterwards. Pairing must span that boundary, or each result folds onto a call that
  // did not produce it — silently, since a wrong result still looks like a result.
  it("pairs results across a phase boundary when calls of different kinds are batched", () => {
    seq = 0;
    const trace = buildTrace([
      use("Read", "file_path=a.rs"),
      use("Bash", "command=cargo test"),
      res("10 lines"),
      res("test result: ok. 1 passed; 0 failed"),
    ]);
    expect(trace.phases.map((p) => p.kind)).toEqual(["oriented", "verified"]);
    expect(trace.phases[0].did[0].result).toBe("10 lines");
    expect(trace.phases[1].did[0].result).toBe("test result: ok. 1 passed; 0 failed");
  });

  // A failure that arrives after its phase has closed must still mark that phase.
  it("marks the phase failed when a batched result fails after the phase closed", () => {
    seq = 0;
    const trace = buildTrace([
      use("Bash", "command=cargo test"),
      use("Read", "file_path=a.rs"),
      res("Exit code 101"),
      res("10 lines"),
    ]);
    expect(trace.phases[0].failed).toBe(true);
    expect(trace.phases[0].effects).toContainEqual({ kind: "error", label: "error" });
    expect(trace.phases[1].failed).toBe(false);
  });

  it("keeps an orphan result rather than dropping it", () => {
    seq = 0;
    const trace = buildTrace([res("truncated transcript")]);
    expect(trace.phases[0].orphanResults).toEqual(["truncated transcript"]);
  });

  it("carries a humanized target on each card", () => {
    const trace = buildTrace(realisticTranscript());
    expect(trace.phases[0].did[0].target).toBe("crates/orchestrator/src/reviewintro.rs");
    expect(trace.phases[2].did[0].target).toBe("cargo test --workspace");
  });
});

describe("buildTrace — side effects", () => {
  it("derives edited / room / memory chips", () => {
    const trace = buildTrace(realisticTranscript());
    const implemented = trace.phases[1];
    expect(implemented.effects).toEqual([{ kind: "edited", label: "edited 2 files" }]);
    const coordinated = trace.phases[3];
    expect(coordinated.effects).toEqual([
      { kind: "room", label: "posted to room" },
      { kind: "memory", label: "retained 1 fact" },
    ]);
  });

  it("counts DISTINCT edited files, not edit calls", () => {
    seq = 0;
    const trace = buildTrace([
      use("Edit", "file_path=a.rs new_string=1"),
      res("(ok)"),
      use("Edit", "file_path=a.rs new_string=2"),
      res("(ok)"),
    ]);
    expect(trace.phases[0].effects).toEqual([{ kind: "edited", label: "edited 1 file" }]);
  });

  // A `Bash` card classified `implemented` NEVER carries a file_path, so treating it as a write
  // whose path was clipped invents files outright: over the 435 real transcripts in
  // ~/.rhapsody/logs, 1,759 of the 3,057 phases showing an `edited N files` chip drew part of that
  // count from a non-edit tool — 2,448 files that were never written. No chip beats a false one.
  it("never invents an edited file for a shell write", () => {
    for (const command of [
      "git push -u origin HEAD",
      "git checkout -- internal/api/batch_handlers_create_test.go",
      "cargo test --workspace 2>&1 | tee /tmp/wstest.log",
      "mkdir -p target/out",
    ]) {
      seq = 0;
      const phase = buildTrace([use("Bash", `command=${command}`), res("(ok)")]).phases[0];
      expect(phase.effects, command).toEqual([]);
      expect(phase.subtitle, command).not.toContain("edited");
    }
  });

  // The other half of the same rule: a real edit tool whose file_path the humanizer clipped away
  // still counts, so an unnamed write is never silently lost from the count.
  it("still counts an edit whose file_path did not survive the humanizer", () => {
    seq = 0;
    const phase = buildTrace([use("Edit", "new_string=1 old_string=0"), res("(ok)")]).phases[0];
    expect(phase.effects).toEqual([{ kind: "edited", label: "edited 1 file" }]);
    expect(phase.subtitle).toBe("edited 1 file");
  });

  it("flags a failing result as an error chip", () => {
    seq = 0;
    const trace = buildTrace([use("Bash", "command=cargo test"), res("Exit code 101")]);
    expect(trace.phases[0].failed).toBe(true);
    expect(trace.phases[0].did[0].failed).toBe(true);
    expect(trace.phases[0].effects).toContainEqual({ kind: "error", label: "error" });
  });

  // The traps: real PASSING results contain the words "failed" and "errors".
  it("does not read a passing test summary as a failure", () => {
    seq = 0;
    const ok = buildTrace([
      use("Bash", "command=cargo test"),
      res("test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"),
    ]);
    expect(ok.phases[0].failed).toBe(false);
    seq = 0;
    const lint = buildTrace([
      use("Bash", "command=npm run lint"),
      res("✖ 18 problems (0 errors, 18 warnings)"),
    ]);
    expect(lint.phases[0].failed).toBe(false);
  });

  it("recognises the real failure markers", () => {
    for (const text of [
      "Exit code 1",
      "<tool_use_error>String to replace not found in file.",
      "FAIL node src/store/buildMealStore.test.ts",
      "--- FAIL: TestEntitlementEventRepository (0.01s)",
      "error[E0308]: mismatched types",
      "thread 'main' panicked at src/lib.rs:4:1",
    ]) {
      seq = 0;
      const trace = buildTrace([use("Bash", "command=x"), res(text)]);
      expect(trace.phases[0].did[0].failed, text).toBe(true);
    }
    // Exit code 0 is a success.
    seq = 0;
    expect(buildTrace([use("Bash", "command=x"), res("Exit code 0")]).phases[0].failed).toBe(false);
  });

  it("marks a failing turn event as an error on the phase it closes", () => {
    seq = 0;
    const trace = buildTrace([
      use("Read", "file_path=a.rs"),
      res("10 lines"),
      ev("turn failed: max_turns"),
    ]);
    expect(trace.phases[0].failed).toBe(true);
  });
});

describe("buildResult", () => {
  it("extracts a headline and the sectioned body from a handoff", () => {
    const card = buildResult(realisticTranscript(), run());
    expect(card.source).toBe("handoff");
    expect(card.headline).toBe("Wired the edge-triggered watcher.");
    expect(card.sections.map((s) => s.label)).toEqual([
      "What changed",
      "How verified",
      "Follow-ups",
    ]);
    expect(card.sections[0].heading).toBe("What shipped");
    expect(card.sections[0].body).toBe("- New watcher task.");
    expect(card.sections[1].body).toBe("- `cargo test` green.");
  });

  // The real heading vocabulary is a FAMILY, measured over 784 transcripts: "what shipped" (148),
  // "what landed" (49), "what i did" (48), "verification" (207), "flagged, not fixed" (9)…
  // Matching the three literal display labels would classify almost every real handoff as Notes.
  it("classifies the heading synonyms that runs actually write", () => {
    seq = 0;
    const card = buildResult(
      [
        say(
          [
            "Shipped it.",
            "",
            "## What landed",
            "- a",
            "",
            "## Verification",
            "- b",
            "",
            "## Left undone, deliberately",
            "- c",
            "",
            "## The guard",
            "- d",
          ].join("\n"),
        ),
      ],
      run(),
    );
    expect(card.sections.map((s) => s.label)).toEqual([
      "What changed",
      "How verified",
      "Follow-ups",
      "Notes",
    ]);
  });

  it("does not split on a heading inside a fenced code block", () => {
    seq = 0;
    const card = buildResult(
      [say(["Done.", "", "## What changed", "```md", "## Verification", "```", "- real"].join("\n"))],
      run(),
    );
    expect(card.sections).toHaveLength(1);
    expect(card.sections[0].body).toBe("```md\n## Verification\n```\n- real");
  });

  it("keeps the lead paragraph that sits before the first heading", () => {
    seq = 0;
    const card = buildResult([say("Wired the watcher.\n\nMore detail here.\n\n## Verification\n- ok")], run());
    expect(card.lead).toBe("Wired the watcher.\n\nMore detail here.");
  });

  it("falls back to a sensible headline when the run wrote no prose", () => {
    seq = 0;
    const card = buildResult([ev("session started")], run({ outcome: "failed", error: "boom" }));
    expect(card.source).toBe("fallback");
    expect(card.headline).not.toBe("");
    expect(card.headline).toContain("boom");
    expect(card.sections).toEqual([]);
  });

  it("falls back with no run at all", () => {
    const card = buildResult([], undefined);
    expect(card.headline).not.toBe("");
  });

  it("strips markdown from the headline and grows a too-short opener", () => {
    seq = 0;
    // "Done." alone is not a headline — the real corpus opens this way constantly.
    const card = buildResult([say("Done. Draft PR: **https://example.com/pull/80**")], run());
    expect(card.headline).toBe("Done. Draft PR: https://example.com/pull/80");
    seq = 0;
    const bold = buildResult([say("**Wired** the `watcher` end to end.")], run());
    expect(bold.headline).toBe("Wired the watcher end to end.");
  });

  // The other 11 of 554: the prose opens with a TITLE heading and no lead paragraph.
  it("treats a leading unclassified heading as the lead, not a Notes section", () => {
    seq = 0;
    const card = buildResult(
      [say("## STUDIO-354 — complete\n\nBuilt it end to end.\n\n## Verification\n- ok")],
      run(),
    );
    expect(card.lead).toBe("Built it end to end.");
    expect(card.sections.map((s) => s.label)).toEqual(["How verified"]);
    expect(card.headline).toBe("Built it end to end.");
  });

  it("stops the headline at a bullet rather than running two thoughts together", () => {
    seq = 0;
    const card = buildResult([say("Wired the watcher end to end.\n- first bullet\n- second")], run());
    expect(card.headline).toBe("Wired the watcher end to end.");
  });

  // `markdown.ts` renders the card BODY right under this H1 and gets the flanking rules right, so
  // stripping emphasis with a plainer rule made the same identifier read two ways in one card.
  it("leaves an intraword underscore alone while still stripping real emphasis", () => {
    seq = 0;
    expect(buildResult([say("Fixed `load_live_review_watch` so the sweep sees rows.")], run()).headline).toBe(
      "Fixed load_live_review_watch so the sweep sees rows.",
    );
    seq = 0;
    expect(buildResult([say("Bounded HINDSIGHT_API_RERANKER_MAX_CANDIDATES at last.")], run()).headline).toBe(
      "Bounded HINDSIGHT_API_RERANKER_MAX_CANDIDATES at last.",
    );
    seq = 0;
    expect(buildResult([say("Wired _both_ `teams_post` and `teams_retain` end to end.")], run()).headline).toBe(
      "Wired both teams_post and teams_retain end to end.",
    );
  });

  // The corpus writes the stub as its OWN paragraph, so growing within the opening paragraph never
  // reached a second sentence: 6 of 435 real runs finished under the floor, two of them with an H1
  // reading literally "Done." over a 2,500-char body.
  it("grows a stub headline past the floor with the prose that follows it", () => {
    seq = 0;
    const card = buildResult(
      [say("Done.\n\n## What shipped\n\nThe watch set is now steered from the console.")],
      run(),
    );
    expect(card.headline).toBe("Done. The watch set is now steered from the console.");
  });

  it("skips a leading heading when looking for the headline", () => {
    seq = 0;
    const card = buildResult([say("## Handoff\n\nBuilt the thing properly.")], run());
    expect(card.headline).toBe("Built the thing properly.");
  });
});
