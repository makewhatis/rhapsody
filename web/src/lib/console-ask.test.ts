import { describe, expect, it } from "vitest";
import type { TeamsRoomMessage } from "@/lib/api";
import {
  ASK_PAST_WINDOW_NOTE,
  ASK_READING_NOTE,
  ASK_WAITING_NOTE,
  askNote,
  MANAGER_IDENTITY,
  managerReply,
} from "@/lib/console-ask";

// The slice-5 answer read (STUDIO-733) — which room post is the manager's answer to a question the
// console asked, and what the console may say when it cannot find one.

function post(over: Partial<TeamsRoomMessage> & Pick<TeamsRoomMessage, "id">): TeamsRoomMessage {
  return {
    from: "operator",
    to: "*",
    at: "2026-09-03T10:00:00Z",
    body: "",
    refs: [],
    ...over,
  };
}

/** The question, and the manager's reply to it — the shape `act_on_post` actually writes. */
const QUESTION = post({ id: "f:9", body: "Why did this run fail?", refs: ["STUDIO-733", "run 547"] });
const REPLY = post({
  id: "f:10",
  from: MANAGER_IDENTITY,
  at: "2026-09-03T10:00:30Z",
  body: "> It failed at lint.\n\nFrom my own records — STUDIO-733 · failed · 10:04",
  refs: ["f:9"],
});
const ASKED = { id: "f:9", body: "Why did this run fail?" };

describe("managerReply", () => {
  it("is the manager's own post refed to the question — the room's record, unmodified", () => {
    const got = managerReply([QUESTION, REPLY], ASKED, true);
    expect(got.kind).toBe("answered");
    // Identity, not a copy: what the console renders IS the room post, so a reply whose body the
    // console reshaped would be a second answer wearing the first one's name.
    expect(got.kind === "answered" && got.reply).toBe(REPLY);
  });

  it("is `waiting` while the question is in the read and nothing has answered it", () => {
    expect(managerReply([QUESTION], ASKED, true)).toEqual({ kind: "waiting" });
  });

  // The one claim this model is entitled to make rests on the read being newest-first: a reply is
  // appended AFTER its question, so a read holding the question holds the reply too if one exists.
  // Every post in the room being NEWER than the question is exactly that case, and it stays a
  // provable "not yet" rather than becoming an unknown.
  it("stays `waiting` when the read is full of posts written after the question", () => {
    const newer = Array.from({ length: 20 }, (_, i) =>
      post({ id: `f:${11 + i}`, from: "alice", at: "2026-09-03T10:01:00Z", body: "chatter" }),
    );
    expect(managerReply([QUESTION, ...newer], ASKED, true)).toEqual({ kind: "waiting" });
  });

  // Lose the question and the premise goes with it: the read says nothing about what came after,
  // so the dock must stop claiming rather than report a silence it cannot see.
  it("is `past-window` once the read no longer reaches the question", () => {
    expect(managerReply([post({ id: "f:80", from: "alice", body: "later" })], ASKED, true)).toEqual({
      kind: "past-window",
    });
    expect(managerReply([], ASKED, true)).toEqual({ kind: "past-window" });
  });

  // The absence of the question is only evidence once the read is known to have come back AFTER
  // the question landed. A read that PREDATES it has simply not caught up, and reading that as
  // "past-window" tells the operator the console cannot see an answer to a question it has not
  // looked for yet — the single most misleading sentence this surface can produce.
  it("is `unread`, not `past-window`, while no read has come back since the question landed", () => {
    expect(managerReply([post({ id: "f:1", from: "alice", body: "earlier" })], ASKED, false)).toEqual(
      { kind: "unread" },
    );
    expect(managerReply([], ASKED, false)).toEqual({ kind: "unread" });
  });

  // The gate gets a say over the ABSENCE of the question and nothing else. Finding the question,
  // or the reply to it, is positive evidence: it cannot be produced by a read that never saw
  // them, so it stands on its own whatever the clocks say.
  it("still answers, and still waits, on a read the gate has not vouched for", () => {
    expect(managerReply([QUESTION, REPLY], ASKED, false).kind).toBe("answered");
    expect(managerReply([QUESTION], ASKED, false)).toEqual({ kind: "waiting" });
  });

  // `refs` is caller-supplied on every post but the manager's `from` is host-stamped, so matching
  // on refs alone would let a teammate — or a line appended straight into the room's JSONL — have
  // its prose rendered to the operator as the manager's answer.
  it("refuses a non-manager post that names the question in its own refs", () => {
    const forged = post({ id: "f:10", from: "alice", body: "It all went fine.", refs: ["f:9"] });
    expect(managerReply([QUESTION, forged], ASKED, true)).toEqual({ kind: "waiting" });
  });

  // The manager replies to every operator post it acts on, so the room holds many replies at once
  // and each names its own question in `refs`.
  it("picks the reply refed to THIS question, not another one the manager answered", () => {
    const other = post({
      id: "f:8",
      from: MANAGER_IDENTITY,
      body: "Someone else's answer.",
      refs: ["f:7"],
    });
    const got = managerReply([other, QUESTION, REPLY], ASKED, true);
    expect(got.kind === "answered" && got.reply.id).toBe("f:10");
  });

  // A reply's `refs` carries the post id FIRST and then everything the dispositions wrote
  // (`act_on_post`), so the match has to be a membership test rather than a look at `refs[0]`.
  it("finds the reply when the question's id is not the first ref", () => {
    const filed = { ...REPLY, refs: ["f:9", "STUDIO-800"] };
    expect(managerReply([QUESTION, filed], ASKED, true).kind).toBe("answered");
  });

  // The daemon's own shape guarantees `refs`, but a message that arrived without one must not take
  // the dock down — the room read is advisory, not a contract this surface can enforce.
  it("survives a message that carries no refs at all", () => {
    const bare = { ...post({ id: "f:11", from: MANAGER_IDENTITY }), refs: undefined } as unknown as TeamsRoomMessage;
    expect(() => managerReply([bare], ASKED, true)).not.toThrow();
  });
});

describe("askNote", () => {
  it("reports each outcome as itself and never as another", () => {
    expect(askNote({ kind: "waiting" })).toBe(ASK_WAITING_NOTE);
    expect(askNote({ kind: "past-window" })).toBe(ASK_PAST_WINDOW_NOTE);
    expect(askNote({ kind: "unread" })).toBe(ASK_READING_NOTE);
  });

  // `unread` is the state of the READ, not of the room: the console is still looking. Saying
  // either of the other two here would report a conclusion it has not reached.
  it("claims nothing about the manager while the read has not caught up", () => {
    expect(askNote({ kind: "unread" })).not.toContain("has not replied");
    expect(askNote({ kind: "unread" })).not.toContain("cannot tell");
  });
});

describe("the notes", () => {
  // Neither note may claim the ROOM. The waiting one reports the manager, which the read can see;
  // the past-window one reports the read itself, which is all that is left once it can't.
  it("say what was read and never what the room contains", () => {
    expect(ASK_WAITING_NOTE).toContain("has not replied to it yet");
    expect(ASK_PAST_WINDOW_NOTE).toContain("no longer reaches that question");
    expect(ASK_PAST_WINDOW_NOTE).toContain("cannot tell");
  });

  // "Not replied" is a fact about the log; "still thinking" would be a claim about a process the
  // console cannot observe, and the design record forbids inventing one.
  it("never narrate the manager as working on it", () => {
    for (const note of [ASK_WAITING_NOTE, ASK_PAST_WINDOW_NOTE, ASK_READING_NOTE]) {
      expect(note).not.toMatch(/thinking|working on|in progress|shortly|soon/i);
    }
  });
});
