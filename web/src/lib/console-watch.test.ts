import { describe, expect, it } from "vitest";
import type { ReviewJob, RunMessage, RunSummary, TeamsRoomMessage } from "@/lib/api";
import {
  DEFAULT_WATCH_TAB,
  WATCH_TABS,
  askRefs,
  messageChip,
  originTicket,
  reviewRunPr,
  reviewsForRun,
  roomPostsFor,
} from "@/lib/console-watch";

// The slice-4 watch-tabs model (STUDIO-745) — which tab is a dependency, which room posts and
// review rows belong to a run, what a delivery chip says, and what an "Ask about this run" post is
// refed to.

function run(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 547,
    issue_id: "i",
    issue_identifier: "STUDIO-745",
    title: "Watch tabs",
    attempt: 0,
    session_uuid: "s",
    branch: "",
    project_slug: "",
    repo: "git@github.com:makewhatis/rhapsody.git",
    started_at: "2026-09-03T10:00:00Z",
    ended_at: "2026-09-03T10:04:30Z",
    outcome: "completed",
    turns: 2,
    input_tokens: 1,
    output_tokens: 2,
    total_tokens: 3,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  } as RunSummary;
}

function post(over: Partial<TeamsRoomMessage> = {}): TeamsRoomMessage {
  return {
    id: "f:1",
    from: "alice",
    to: "*",
    at: "2026-09-03T10:00:00Z",
    body: "",
    refs: [],
    ...over,
  };
}

function job(over: Partial<ReviewJob> = {}): ReviewJob {
  return {
    owner: "makewhatis",
    repo: "rhapsody",
    number: 105,
    reviewer: "jimmy",
    author: "alice",
    introduced_by: "handoff:STUDIO-745",
    requested_sha: "abc1234def",
    last_reviewed_sha: "",
    status: "requested",
    open: true,
    ...over,
  };
}

function message(over: Partial<RunMessage> = {}): RunMessage {
  return {
    id: 1,
    run_id: 547,
    body: "btw the branch moved",
    created_at_ms: 0,
    status: "sent",
    ...over,
  };
}

describe("the rail", () => {
  // §3C names five, and the ONE that is not real is the one an endpoint is missing for. A rail
  // that marked Review as a dependency too would be disowning data the daemon does serve.
  it("names Diff as the only tab whose whole surface is waiting on an endpoint", () => {
    expect(WATCH_TABS.map((t) => t.id)).toEqual(["diff", "review", "room", "memory", "messages"]);
    expect(WATCH_TABS.filter((t) => t.dependency).map((t) => t.id)).toEqual(["diff"]);
    expect(DEFAULT_WATCH_TAB).toBe("room");
  });
});

describe("roomPostsFor", () => {
  it("keeps a post that refs the ticket and one that names it in prose, newest first", () => {
    const posts = roomPostsFor(
      [
        post({ id: "f:1", body: "Who can review this?", refs: ["STUDIO-745"] }),
        post({ id: "f:2", body: "Unrelated" }),
        post({ id: "f:3", body: "STUDIO-745 is up for review." }),
      ],
      "STUDIO-745",
    );
    expect(posts.map((p) => p.id)).toEqual(["f:3", "f:1"]);
  });

  it("keeps nothing at all for a run with no ticket, rather than every post", () => {
    expect(roomPostsFor([post({ body: "anything" })], "")).toEqual([]);
  });

  // The daemon always sends `refs`, but a hand-rolled log line the room skipped could not.
  it("survives a post with no refs array", () => {
    const bare = { ...post({ body: "STUDIO-745" }), refs: undefined } as unknown as TeamsRoomMessage;
    expect(roomPostsFor([bare], "STUDIO-745")).toHaveLength(1);
  });
});

describe("reviewRunPr / originTicket", () => {
  it("reads the pull request out of a review run's own key, dropping the reviewer", () => {
    expect(reviewRunPr("pr:makewhatis/rhapsody#105@jimmy")).toBe("makewhatis/rhapsody#105");
  });

  // `is_review_key` only checks the prefix, so a key with no reviewer is a shape this must answer
  // for — the whole rest of it is the coordinate, not a slice from a -1 index.
  it("still reads a pr: key that carries no reviewer", () => {
    expect(reviewRunPr("pr:makewhatis/rhapsody#105")).toBe("makewhatis/rhapsody#105");
  });

  it("is empty for an ordinary ticket key", () => {
    expect(reviewRunPr("STUDIO-745")).toBe("");
  });

  it("parses the ticket out of the origin tag on the separator, not a literal prefix", () => {
    expect(originTicket("handoff:STUDIO-720")).toBe("STUDIO-720");
    // `console:` is declared beside `handoff:` in reviewintro.rs and nothing writes it yet.
    expect(originTicket("console:STUDIO-720")).toBe("STUDIO-720");
    expect(originTicket("handoff")).toBe("");
  });
});

describe("reviewsForRun", () => {
  it("matches an author's run through the origin tag its own hand-off wrote", () => {
    const rows = reviewsForRun(
      [
        job({ reviewer: "jimmy" }),
        job({ reviewer: "bob", introduced_by: "handoff:STUDIO-744", number: 104 }),
      ],
      run(),
    );
    expect(rows.map((r) => r.reviewer)).toEqual(["jimmy"]);
  });

  // A review run is keyed by the PULL REQUEST, so the origin tag — which names the AUTHOR's
  // ticket, not this run's key — would match nothing at all.
  it("matches a review run through the pull request its key carries, every reviewer of it", () => {
    const rows = reviewsForRun(
      [
        job({ reviewer: "jimmy" }),
        job({ reviewer: "bob" }),
        job({ number: 104, reviewer: "jimmy" }),
      ],
      run({ issue_identifier: "pr:makewhatis/rhapsody#105@jimmy" }),
    );
    expect(rows.map((r) => `${r.reviewer}#${r.number}`)).toEqual(["jimmy#105", "bob#105"]);
  });

  it("matches nothing for a run with no identifier, rather than every row", () => {
    expect(reviewsForRun([job()], run({ issue_identifier: "" }))).toEqual([]);
  });

  // The origin is a whole-ticket match: `handoff:STUDIO-7450` is a different ticket, and a
  // `startsWith` would fold it into this one.
  it("does not fold a longer ticket key into this one", () => {
    expect(reviewsForRun([job({ introduced_by: "handoff:STUDIO-7450" })], run())).toEqual([]);
  });
});

describe("messageChip", () => {
  it("moves sent → delivered, naming the turn the daemon recorded", () => {
    expect(messageChip(message())).toEqual({ tone: "sent", label: "sent" });
    expect(messageChip(message({ status: "delivered", delivered_turn: 3 }))).toEqual({
      tone: "delivered",
      label: "delivered · turn 3",
    });
  });

  // Turn 0 is a real turn. A truthiness check would print the bare "delivered" for it.
  it("names turn 0, which a truthy check would drop", () => {
    expect(messageChip(message({ status: "delivered", delivered_turn: 0 })).label).toBe(
      "delivered · turn 0",
    );
  });

  it("falls back to a bare delivered when the daemon recorded no turn", () => {
    expect(messageChip(message({ status: "delivered" })).label).toBe("delivered");
  });

  it("says in words what an expired message means", () => {
    const chip = messageChip(message({ status: "expired" }));
    expect(chip.tone).toBe("expired");
    expect(chip.label).toContain("the run ended first");
  });

  it("shows a status this build has never heard of verbatim", () => {
    const chip = messageChip(message({ status: "shredded" as RunMessage["status"] }));
    expect(chip).toEqual({ tone: "unknown", label: "shredded" });
  });
});

describe("askRefs", () => {
  it("refs the ticket AND the run, so the question is about this attempt", () => {
    expect(askRefs(run())).toEqual(["STUDIO-745", "run 547"]);
  });

  it("still refs the run when the row carries no identifier", () => {
    expect(askRefs(run({ issue_identifier: "" }))).toEqual(["run 547"]);
  });
});
