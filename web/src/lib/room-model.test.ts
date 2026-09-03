import { parseMarkdown } from "@/lib/markdown";
import { describe, expect, it } from "vitest";
import type { TeamsRoomMessage } from "@/lib/api";
import {
  BODY_TRUNCATE_AT,
  DEFAULT_ROOM_WINDOW,
  MAX_ROOM_WINDOW,
  MIN_ASSIGN_RUN,
  classify,
  dayLabel,
  daySections,
  eventTeammates,
  filterEvents,
  groupLabel,
  nextRoomLimit,
  parseAssignment,
  roomEvents,
  roomStats,
  truncateBody,
} from "@/lib/room-model";

// The pure half of the Teams console's room (STUDIO-681 §5, boxes 3.2–3.8). Everything here is
// asserted against the daemon's REAL post bodies — the `format!` strings in
// crates/orchestrator/src/{triage,quorum}.rs — because the room log carries no kind field and a
// reworded body is exactly the regression these tests exist to catch.

const ROSTER = ["alice", "jimmy"];

/** The text of every fenced block in a rendered half, in order. */
function codeOf(source: string): string[] {
  return parseMarkdown(source).flatMap((b) => (b.type === "code" ? [b.text] : []));
}

// Timestamps sit mid-UTC-day on purpose: the feed groups and prints in LOCAL time (the house style
// of lib/format), so a fixture near midnight would land on a different calendar day depending on
// the runner's timezone.
function msg(over: Partial<TeamsRoomMessage> & { id: string }): TeamsRoomMessage {
  return { from: "@manager", to: "*", at: "2026-08-31T12:00:00Z", body: "", refs: [], ...over };
}

const handoff = msg({
  id: "2026-08-31:4",
  from: "alice",
  at: "2026-08-31T15:11:00Z",
  body: "STUDIO-678 up for review — the TOCTOU is fixed.",
  refs: ["STUDIO-678", "https://github.com/x/y/pull/70"],
});

const operatorPost = msg({
  id: "2026-08-31:3",
  from: "operator",
  at: "2026-08-31T14:37:00Z",
  body: "Someone want to review the export PR? STUDIO-654",
  refs: ["STUDIO-654"],
});

// The exact body triage.rs writes for a deterministic assignment.
function assignment(seq: number, hhmm: string, ticket: string, who: string): TeamsRoomMessage {
  return msg({
    id: `2026-08-31:${seq}`,
    at: `2026-08-31T${hhmm}:00Z`,
    body: `Assigned ${ticket} to ${who} (deterministic). Reason: least-loaded (7 open).`,
    refs: [ticket],
  });
}

const quorumFailure = msg({
  id: "2026-08-31:9",
  at: "2026-08-31T16:12:00Z",
  body:
    "REVIEW QUORUM FAILED for STUDIO-678: no review ticket could be created for " +
    "https://github.com/x/y/pull/70 (asked: jimmy). STUDIO-678 is unreviewed and the ticket is " +
    "NOT marked, so a later handoff may try again.",
  refs: ["STUDIO-678"],
});

const reconcile = msg({
  id: "2026-08-31:2",
  at: "2026-08-31T13:26:00Z",
  body: "Cleaned up 11 stray identity label(s) on review-state tickets that no run ever wore: X-1.",
});

describe("3.2 — a post is typed by kind from its author and the daemon's own body", () => {
  it("stamps the operator's post as the human voice", () => {
    expect(classify(operatorPost)).toMatchObject({ kind: "operator", kindLabel: "you" });
  });

  it("stamps every teammate post as the hand-off voice", () => {
    expect(classify(handoff)).toMatchObject({ kind: "handoff", kindLabel: "hand-off" });
    expect(classify(msg({ id: "d:1", from: "jimmy", body: "a note" })).kind).toBe("handoff");
  });

  it("types triage's assignment post, and marks the deterministic ones", () => {
    const deterministic = classify(assignment(1, "11:44", "STUDIO-403", "jimmy"));
    expect(deterministic).toMatchObject({ kind: "assign", deterministic: true });
    const modelled = classify(msg({ id: "d:1", body: "Assigned MT-1 to alice. Reason: rust" }));
    expect(modelled).toMatchObject({ kind: "assign", deterministic: false });
  });

  it("types the stray-label sweep as reconcile", () => {
    expect(classify(reconcile)).toMatchObject({ kind: "reconcile", kindLabel: "reconcile" });
  });

  it("types both quorum refusals as a failure, and a fan-out that worked as plain quorum", () => {
    expect(classify(quorumFailure)).toMatchObject({ kind: "quorum", kindLabel: "quorum ✕", failed: true });
    const none = msg({
      id: "d:1",
      body: "NO REVIEW QUORUM for MT-1: alice handed off a PR but the roster holds no other teammate.",
    });
    expect(classify(none)).toMatchObject({ kind: "quorum", failed: true });
    const fanned = msg({
      id: "d:2",
      body: "Requested review of https://x/pull/1 from jimmy (MT-2), for MT-1 handed off by alice.",
    });
    expect(classify(fanned)).toMatchObject({ kind: "quorum", kindLabel: "quorum", failed: false });
  });

  it("leaves an unrecognised manager post muted and labelled for what it is, guessing nothing", () => {
    const odd = classify(msg({ id: "d:1", body: "Something the manager grew a new sentence for." }));
    expect(odd).toMatchObject({ kind: "reconcile", kindLabel: "manager", deterministic: false });
  });

  it("does not mistake a teammate quoting a manager body for a manager post", () => {
    const quoted = msg({ id: "d:1", from: "alice", body: "REVIEW QUORUM FAILED is what I saw." });
    expect(classify(quoted).kind).toBe("handoff");
  });
});

describe("3.3 — the filter chips admit exactly the kinds §5 names", () => {
  const events = roomEvents(
    [handoff, operatorPost, assignment(1, "11:44", "MT-1", "jimmy"), reconcile, quorumFailure],
    ROSTER,
  );
  const kinds = (filter: "all" | "conversation" | "handoff" | "assign" | "quorum") =>
    filterEvents(events, { filter, who: "all", search: "" }).map((e) => e.kind);

  it("Conversation is operator + hand-off only", () => {
    expect(new Set(kinds("conversation"))).toEqual(new Set(["operator", "handoff"]));
  });

  it("Assignments is assign + reconcile", () => {
    expect(new Set(kinds("assign"))).toEqual(new Set(["assign", "reconcile"]));
  });

  it("Quorum is quorum", () => {
    expect(new Set(kinds("quorum"))).toEqual(new Set(["quorum"]));
  });

  it("All is everything", () => {
    expect(kinds("all")).toHaveLength(5);
  });
});

describe("3.4 — the teammate filter scopes the feed", () => {
  it("scopes to the teammate an event is about, by author and by name in the body", () => {
    expect(eventTeammates(handoff, ROSTER)).toEqual(["alice"]);
    expect(eventTeammates(assignment(1, "11:44", "MT-1", "jimmy"), ROSTER)).toEqual(["jimmy"]);
  });

  it("does not match a name inside a longer word", () => {
    const m = msg({ id: "d:1", body: "aliceandjimmyish naming" });
    expect(eventTeammates(m, ROSTER)).toEqual([]);
  });

  it("keeps events that name nobody visible under every teammate", () => {
    const events = roomEvents([handoff, reconcile], ROSTER);
    const forJimmy = filterEvents(events, { filter: "all", who: "jimmy", search: "" });
    expect(forJimmy.map((e) => e.message.id)).toEqual([reconcile.id]);
  });

  it("hides another teammate's hand-off", () => {
    const jimmys = msg({ id: "2026-08-31:6", from: "jimmy", at: "2026-08-31T16:00:00Z", body: "mine" });
    const events = roomEvents([handoff, jimmys], ROSTER);
    const forAlice = filterEvents(events, { filter: "all", who: "alice", search: "" });
    expect(forAlice.map((e) => e.message.id)).toEqual([handoff.id]);
  });
});

describe("search narrows by text, author and refs", () => {
  const events = roomEvents([handoff, operatorPost], ROSTER);
  const found = (search: string) =>
    filterEvents(events, { filter: "all", who: "all", search }).map((e) => e.message.id);

  it("matches a ticket in the refs", () => {
    expect(found("STUDIO-654")).toEqual([operatorPost.id]);
  });

  it("matches body text case-insensitively", () => {
    expect(found("toctou")).toEqual([handoff.id]);
  });

  it("matches the author", () => {
    expect(found("alice")).toEqual([handoff.id]);
  });
});

describe("3.6 — the feed is newest-first and partitioned by calendar day", () => {
  const older = msg({ id: "2026-08-30:1", from: "jimmy", at: "2026-08-30T12:00:00Z", body: "yesterday" });
  const events = roomEvents([older, operatorPost, handoff], ROSTER);

  it("orders newest first", () => {
    expect(events.map((e) => e.message.id)).toEqual([handoff.id, operatorPost.id, older.id]);
  });

  it("splits into one section per calendar day, newest day first", () => {
    const sections = daySections(events, "2026-08-31");
    expect(sections.map((s) => s.day)).toEqual(["2026-08-31", "2026-08-30"]);
    expect(sections[0].items).toHaveLength(2);
    expect(sections[1].items).toHaveLength(1);
  });

  it("labels the current day 'Today' and any other day by weekday", () => {
    expect(dayLabel("2026-08-31", "2026-08-31")).toBe("Today · Aug 31");
    expect(dayLabel("2026-08-30", "2026-08-31")).toBe("Sun · Aug 30");
  });
});

describe("3.5 — a run of deterministic assignments collapses into one group", () => {
  const run = [
    assignment(1, "11:44", "STUDIO-638", "alice"),
    assignment(2, "11:45", "STUDIO-402", "jimmy"),
    assignment(3, "13:46", "STUDIO-673", "alice"),
  ];

  it("collapses a run of at least MIN_ASSIGN_RUN into a single expandable group", () => {
    const sections = daySections(roomEvents(run, ROSTER), "2026-09-01");
    expect(sections[0].items).toHaveLength(1);
    const [item] = sections[0].items;
    expect(item.type).toBe("group");
    if (item.type !== "group") throw new Error("expected a group");
    expect(item.events).toHaveLength(3);
    expect(item.label).toMatch(/^3 tickets assigned deterministically, /);
  });

  it("leaves a shorter run as individual events", () => {
    const sections = daySections(roomEvents(run.slice(0, MIN_ASSIGN_RUN - 1), ROSTER), "2026-09-01");
    expect(sections[0].items.every((i) => i.type === "event")).toBe(true);
  });

  it("breaks the run on any other event, so a hand-off is never swallowed by a group", () => {
    const sections = daySections(roomEvents([...run, handoff], ROSTER), "2026-09-01");
    expect(sections[0].items.map((i) => i.type)).toEqual(["event", "group"]);
  });

  it("never collapses a model-decided assignment — only the deterministic ones", () => {
    const modelled = run.map((m) => ({ ...m, body: m.body.replace(" (deterministic)", "") }));
    const sections = daySections(roomEvents(modelled, ROSTER), "2026-09-01");
    expect(sections[0].items.every((i) => i.type === "event")).toBe(true);
  });

  it("never groups across a day divider", () => {
    const yesterday = run.map((m, i) => ({
      ...m,
      id: `2026-08-30:${i}`,
      at: m.at.replace("2026-08-31", "2026-08-30"),
    }));
    const sections = daySections(roomEvents([...run, ...yesterday], ROSTER), "2026-09-01");
    expect(sections).toHaveLength(2);
    expect(sections.every((s) => s.items.length === 1 && s.items[0].type === "group")).toBe(true);
  });

  it("labels the group with the run's span, oldest time first", () => {
    const events = roomEvents(run, ROSTER);
    expect(groupLabel(events)).toMatch(/, \d\d:\d\d–\d\d:\d\d$/);
  });
});

describe("an assignment's group line reads the ticket, the teammate and the reason back out", () => {
  it("parses the deterministic body triage.rs writes", () => {
    expect(parseAssignment("Assigned STUDIO-674 to jimmy (deterministic). Reason: least-loaded (7 open).")).toEqual({
      ticket: "STUDIO-674",
      identity: "jimmy",
      reason: "least-loaded (7 open)",
    });
  });

  it("parses the model-decided body, which ends without a full stop", () => {
    expect(parseAssignment("Assigned MT-1 to alice. Reason: owns the rust label")).toEqual({
      ticket: "MT-1",
      identity: "alice",
      reason: "owns the rust label",
    });
  });

  it("keeps the label-write caveat triage.rs appends, rather than truncating the sentence", () => {
    const parsed = parseAssignment(
      "Assigned MT-1 to alice. Reason: least-loaded (the label write failed; the run wears the " +
        "assignment from memory and the label reconciles on a later cycle)",
    );
    expect(parsed?.reason).toContain("the label write failed");
  });

  it("returns null for anything that is not that sentence, so a reword degrades to raw text", () => {
    expect(parseAssignment("Cleaned up 2 stray identity label(s)")).toBeNull();
    expect(parseAssignment("Assigned MT-1 to alice")).toBeNull();
  });
});

describe("3.7 — a long body truncates with the rest kept for the expand", () => {
  it("leaves a short body whole", () => {
    expect(truncateBody("short")).toEqual({ head: "short", rest: "" });
  });

  it("splits a long body on a word boundary and keeps every character", () => {
    const body = "word ".repeat(120).trim();
    const { head, rest } = truncateBody(body);
    expect(head.length).toBeLessThanOrEqual(220);
    expect(head.endsWith(" ")).toBe(false);
    expect(`${head} ${rest}`).toBe(body);
  });

  it("still splits a body with no spaces to break on", () => {
    const body = "x".repeat(400);
    const { head, rest } = truncateBody(body);
    expect(head).toHaveLength(220);
    expect(head + rest).toBe(body);
  });

  // STUDIO-739 — both halves are rendered as markdown independently now. A cut inside a fence
  // leaves the TAIL starting on the closing fence, which opens a new unterminated block and turns
  // every remaining word of the post into monospace code. The repair closes the block on the head
  // and reopens it on the tail, which is the only move that keeps the head inside its budget: a
  // fenced block is arbitrarily long, so cutting to either of its boundaries is not bounded by
  // anything. The budget below is `BODY_TRUNCATE_AT` plus the one fence line the repair adds.
  it("closes a fenced block on the head and reopens it on the tail", () => {
    const body = `Verification:\n\n\`\`\`sh\n${"cargo test --workspace\n".repeat(12)}\`\`\`\n\nAll green, wired into the transcript.`;
    const { head, rest } = truncateBody(body);
    expect(head.length).toBeLessThanOrEqual(BODY_TRUNCATE_AT + "```sh\n".length);
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["paragraph", "code"]);
    // The prose after the block is still prose, and the tail's code is still code.
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code", "paragraph"]);
    // The preview is the block's opening lines — neither an empty box nor the whole block.
    const [headCode, ...tailCode] = [...codeOf(head), ...codeOf(rest)];
    expect(headCode).not.toBe("");
    expect(headCode + tailCode.join("")).toBe("cargo test --workspace\n".repeat(12).slice(0, -1));
  });

  it("bounds the head when the body opens with a long fenced block", () => {
    const body = `\`\`\`\n${"x".repeat(5000)}\n\`\`\`\nprose after the block.`;
    const { head, rest } = truncateBody(body);
    expect(head.length).toBeLessThanOrEqual(BODY_TRUNCATE_AT + 4);
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["code"]);
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code", "paragraph"]);
  });

  it("keeps the expand affordance when the block is never closed", () => {
    const body = `\`\`\`\n${"y".repeat(600)}`;
    const { head, rest } = truncateBody(body);
    expect(head.length).toBeLessThanOrEqual(BODY_TRUNCATE_AT + 4);
    // An unterminated block used to swallow the whole post into the head, leaving no `<details>`.
    expect(rest).not.toBe("");
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["code"]);
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code"]);
  });

  it("spends the budget on the block instead of stopping short of it", () => {
    const body = `hi\n\`\`\`\n${"z".repeat(400)}\n\`\`\`\ntail`;
    const { head } = truncateBody(body);
    // Cutting back to the block's start left a three-character preview ("hi\n") here.
    expect(head.length).toBeGreaterThanOrEqual(BODY_TRUNCATE_AT);
  });

  it("cuts before the fence when the cut lands inside the opening one", () => {
    const body = `${"a".repeat(218)}\n\`\`\`rust\nfn main() {}\n\`\`\`\ntail`;
    const { head, rest } = truncateBody(body);
    // Half an opening fence is not a fence, so the whole block moves to the tail.
    expect(head).toBe(`${"a".repeat(218)}\n`);
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code", "paragraph"]);
  });

  it("takes a closing fence into the head rather than splitting it in two", () => {
    const body = `\`\`\`\n${"b".repeat(213)}\n\`\`\``;
    const { head, rest } = truncateBody(body);
    // Every content line already fits, so the three characters left are the fence itself.
    expect(head).toBe(body);
    expect(rest).toBe("");
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["code"]);
  });

  it("does not let the cut manufacture a closing fence on the tail", () => {
    // The cut falls on the space inside `cat ```' — splitting there would open the tail on
    // " ```", which IS a closing fence, so the reopened block would close empty and spill the
    // rest of the code out as prose. Backing up to the line's start puts the line back together.
    const body = `\`\`\`\n${"filler line\n".repeat(17)}cat \`\`\`\nmorecode\n\`\`\`\ntail`;
    const { head, rest } = truncateBody(body);
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["code"]);
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code", "paragraph"]);
    // Only the trailing marker run moves to the tail, not the whole line: backing up a line at a
    // time can spend the entire preview budget on one long code line.
    expect(codeOf(rest)[0]).toBe("t ```\nmorecode");
    expect(head.length).toBeGreaterThan(BODY_TRUNCATE_AT - 8);
  });

  it("adds only the fence it needs and drops nothing else", () => {
    const body = `lead in\n\n\`\`\`rust\n${"line of output\n".repeat(20)}\`\`\`\ntail`;
    const { head, rest } = truncateBody(body);
    expect(head.endsWith("\n```")).toBe(true);
    expect(rest.startsWith("```rust\n")).toBe(true);
    // Strip the two fences this split synthesized and the halves rejoin into the original body —
    // an assertion the split can fail, unlike one written in terms of `head.length`.
    expect(head.slice(0, -"\n```".length) + rest.slice("```rust\n".length)).toBe(body);
  });

  it("spends the budget on the code line rather than giving the whole line up", () => {
    // The tail of this line IS a closing fence, so the cut has to back up; backing up to the
    // line's START would leave a seven-character empty box as the preview of a 238-char body.
    const body = `\`\`\`\n${"a".repeat(210)} \`\`\`\nmore\n\`\`\``;
    const { head, rest } = truncateBody(body);
    expect(head.length).toBeGreaterThanOrEqual(BODY_TRUNCATE_AT - 8);
    expect(codeOf(head)[0]).toBe("a".repeat(209));
    expect(parseMarkdown(rest).map((b) => b.type)).toEqual(["code"]);
  });

  it("keeps a preview when the post opens with a long fence line", () => {
    // Nothing precedes the block, so cutting to its start would leave an EMPTY preview and a
    // blank post in the collapsed feed. The plain cut keeps the head inside the budget.
    const body = `~~~ ${"opt ".repeat(60)}\ncode\n~~~`;
    const { head, rest } = truncateBody(body);
    expect(head).not.toBe("");
    expect(head.length).toBeLessThanOrEqual(BODY_TRUNCATE_AT);
    expect(rest).not.toBe("");
  });

  it("previews a post that opens with an inline code span", () => {
    // The parser read this post's opening inline code span as a fence, so the whole body became
    // one unterminated code block starting at offset 0 — and the split cut to that start, leaving
    // the feed with an empty preview of a post that was entirely readable prose before.
    const body = `\`\`\`make lint\`\`\` was clean and the workspace built. ${"detail ".repeat(40)}`;
    const { head, rest } = truncateBody(body);
    expect(parseMarkdown(head).map((b) => b.type)).toEqual(["paragraph"]);
    expect(head.startsWith("```make lint``` was clean and the workspace built.")).toBe(true);
    expect(rest).not.toBe("");
  });
});

describe("3.1 — the four stat pills are derived from the loaded room window", () => {
  it("counts hand-offs, assignments, quorum failures, and distinct tickets in review", () => {
    const second = msg({
      id: "2026-08-31:7",
      from: "jimmy",
      at: "2026-08-31T17:00:00Z",
      body: "STUDIO-678 needs another look",
      refs: ["STUDIO-678"],
    });
    const events = roomEvents(
      [handoff, second, operatorPost, assignment(1, "11:44", "MT-1", "jimmy"), reconcile, quorumFailure],
      ROSTER,
    );
    // Two hand-offs, both about STUDIO-678 ⇒ one ticket in review.
    expect(roomStats(events)).toEqual({ inReview: 1, handoffs: 2, assigned: 1, quorumFailed: 1 });
  });

  it("counts only ticket-shaped refs, never a PR url", () => {
    expect(roomStats(roomEvents([handoff], ROSTER)).inReview).toBe(1);
  });
});

describe("3.8 — the pager widens the window it asks the daemon for", () => {
  it("steps by the default window and stops at the daemon's ceiling", () => {
    expect(nextRoomLimit(DEFAULT_ROOM_WINDOW)).toBe(2 * DEFAULT_ROOM_WINDOW);
    expect(nextRoomLimit(MAX_ROOM_WINDOW)).toBe(MAX_ROOM_WINDOW);
    expect(nextRoomLimit(MAX_ROOM_WINDOW - 1)).toBe(MAX_ROOM_WINDOW);
  });

  it("treats a nonsense limit as the default window rather than growing from it", () => {
    expect(nextRoomLimit(0)).toBe(2 * DEFAULT_ROOM_WINDOW);
    expect(nextRoomLimit(Number.NaN)).toBe(2 * DEFAULT_ROOM_WINDOW);
  });
});

describe("a message the daemon could not stamp cleanly still renders", () => {
  it("falls back to the id's day partition when `at` will not parse", () => {
    const broken = msg({ id: "2026-08-29:1", at: "not-a-time" });
    const [event] = roomEvents([broken], ROSTER);
    expect(event.day).toBe("2026-08-29");
    expect(event.time).toBe("");
  });

  it("sorts an unparseable timestamp oldest rather than to the top of the feed", () => {
    const broken = msg({ id: "2026-08-29:1", at: "not-a-time" });
    expect(roomEvents([broken, handoff], ROSTER).map((e) => e.message.id)).toEqual([
      handoff.id,
      broken.id,
    ]);
  });
});
