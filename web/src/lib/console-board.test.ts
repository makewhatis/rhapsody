import { describe, expect, it } from "vitest";
import type { BlockedEntry } from "@/lib/api";
import type { ConsoleJobRow } from "@/lib/console-jobs";
import {
  buildConsoleBoard,
  boardColumnSubtitle,
  boardStateRank,
  parsePullRequest,
  UNKNOWN_STATE,
} from "@/lib/console-board";

// STUDIO-925 — the board is a client-side regroup of rows the console already holds: a CARD is a
// ticket (`review_run` falsy, keyed by `issue_identifier`), a COLUMN is its `tracker_state`, and a
// review row (`review_run` true, `review_of` naming its ticket) is folded onto the card as a chip.

let nextKey = 0;

/** One ConsoleJobRow, with only the fields a test cares about spelled out. */
function row(over: Partial<ConsoleJobRow> & Pick<ConsoleJobRow, "issue">): ConsoleJobRow {
  nextKey += 1;
  return {
    key: `k-${nextKey}`,
    runId: nextKey,
    live: false,
    title: `${over.issue} title`,
    project: "rhapsody",
    projectSlug: "rhapsody",
    status: "review",
    statusLabel: "in review",
    trackerState: "In Review",
    assignee: "",
    reviewRun: false,
    pr: "",
    prUrl: "",
    provider: "",
    reviewOf: "",
    updated: "1m ago",
    updatedAtMs: Date.parse("2026-09-18T10:00:00Z") + nextKey,
    needsYou: false,
    lifecycleResolved: true,
    ...over,
  };
}

/** A review row reviewing `of` at `pr`, with the null tracker_state the daemon sends for one. */
function review(pr: string, of: string, over: Partial<ConsoleJobRow> = {}): ConsoleJobRow {
  return row({
    issue: pr,
    reviewRun: true,
    reviewOf: of,
    trackerState: "",
    status: "done",
    statusLabel: "done",
    assignee: pr.split("@")[1] ?? "",
    ...over,
  });
}

describe("parsePullRequest", () => {
  it("reads owner, repo, number and reviewer out of a review key", () => {
    expect(parsePullRequest("pr:makewhatis/booch#540@jimmy")).toEqual({
      owner: "makewhatis",
      repo: "booch",
      number: 540,
      reviewer: "jimmy",
      url: "https://github.com/makewhatis/booch/pull/540",
    });
  });

  it("tolerates a key with no reviewer", () => {
    const pr = parsePullRequest("pr:makewhatis/booch#540");
    expect(pr?.number).toBe(540);
    expect(pr?.reviewer).toBe("");
  });

  it("refuses anything that is not a review key", () => {
    expect(parsePullRequest("STUDIO-925")).toBeUndefined();
    expect(parsePullRequest("pr:booch#540@jimmy")).toBeUndefined();
    expect(parsePullRequest("pr:makewhatis/booch#x@jimmy")).toBeUndefined();
  });
});

describe("the board regroup (STUDIO-925)", () => {
  it("folds a ticket's review rows onto its one card as reviewer chips", () => {
    const board = buildConsoleBoard([
      row({ issue: "STUDIO-924", status: "run", statusLabel: "running" }),
      review("pr:makewhatis/booch#539@jimmy", "STUDIO-924", {
        status: "done",
        statusLabel: "done",
      }),
      review("pr:makewhatis/booch#540@alice", "STUDIO-924", {
        status: "done",
        statusLabel: "done",
      }),
    ]);

    expect(board).toHaveLength(1);
    expect(board[0].cards).toHaveLength(1);
    const card = board[0].cards[0];
    expect(card.issue).toBe("STUDIO-924");
    expect(card.reviewers.map((r) => r.reviewer)).toEqual(["jimmy", "alice"]);
    expect(card.reviewers.map((r) => r.outcome)).toEqual(["done", "done"]);
    // ...and the card carries the PR its reviews are on.
    expect(card.pr?.number).toBe(539);
    expect(card.pr?.url).toBe("https://github.com/makewhatis/booch/pull/539");
  });

  it("never makes a review row its own card, even when its tracker_state is null", () => {
    const board = buildConsoleBoard([
      row({
        issue: "STUDIO-924",
        status: "run",
        statusLabel: "running",
        trackerState: "In Progress",
      }),
      review("pr:makewhatis/booch#540@jimmy", "STUDIO-924"),
    ]);

    // One card, in the ticket's own column — the review's absent tracker_state invents no column.
    expect(board.map((c) => c.name)).toEqual(["In Progress"]);
    expect(board.flatMap((c) => c.cards).map((c) => c.issue)).toEqual(["STUDIO-924"]);
  });

  it("drops a review row whose origin never resolved to a ticket", () => {
    const board = buildConsoleBoard([review("pr:makewhatis/booch#540@jimmy", "")]);
    expect(board).toEqual([]);
  });

  it("keeps a ticket the daemon could not resolve in an explicit unknown column", () => {
    const board = buildConsoleBoard([
      row({ issue: "LEGACY", trackerState: "", status: "queued", statusLabel: "queued" }),
    ]);
    expect(board).toHaveLength(1);
    expect(board[0].key).toBe(UNKNOWN_STATE);
    expect(board[0].name).toBe("State unknown");
  });

  it("orders columns by workflow state, unknown last", () => {
    const board = buildConsoleBoard([
      row({ issue: "D", trackerState: "Done", status: "done", statusLabel: "done" }),
      row({ issue: "T", trackerState: "Todo", status: "queued", statusLabel: "queued" }),
      row({ issue: "R", trackerState: "In Review" }),
      row({ issue: "P", trackerState: "In Progress", status: "run", statusLabel: "running" }),
      row({ issue: "U", trackerState: "" }),
    ]);
    expect(board.map((c) => c.name)).toEqual([
      "Todo",
      "In Progress",
      "In Review",
      "Done",
      "State unknown",
    ]);
  });

  it("describes a column by its most active card, not its first", () => {
    const board = buildConsoleBoard([
      row({ issue: "A", trackerState: "In Progress", status: "review" }),
      row({ issue: "B", trackerState: "In Progress", status: "run", statusLabel: "running" }),
    ]);
    expect(board[0].subtitle).toBe("an agent is working this in a worktree");
  });

  it("attaches a blocker chip from the live snapshot's held set", () => {
    const blocked: BlockedEntry[] = [
      {
        issue_identifier: "STUDIO-924",
        title: "STUDIO-924 title",
        project: "rhapsody",
        blocker_identifier: "STUDIO-900",
        blocker_state: "In Progress",
        mode: "dag",
      },
    ];
    const board = buildConsoleBoard(
      [row({ issue: "STUDIO-924", trackerState: "Todo", status: "blocked", statusLabel: "blocked" })],
      blocked,
    );
    expect(board[0].cards[0].dependencies).toEqual(["STUDIO-900 · In Progress"]);
  });

  it("carries assignee and provider onto the card", () => {
    const board = buildConsoleBoard([
      row({
        issue: "STUDIO-925",
        trackerState: "In Progress",
        status: "run",
        statusLabel: "running",
        assignee: "jerry",
        provider: "fireworks-ai",
      }),
    ]);
    expect(board[0].cards[0].assignee).toBe("jerry");
    expect(board[0].cards[0].provider).toBe("fireworks-ai");
  });
});

describe("board column helpers", () => {
  it("ranks known states ahead of unknown ones", () => {
    expect(boardStateRank("Todo")).toBeLessThan(boardStateRank("Done"));
    expect(boardStateRank("Done")).toBeLessThan(boardStateRank("Something Else"));
  });

  it("describes each status in one line", () => {
    expect(boardColumnSubtitle(["done", "run"])).toBe("an agent is working this in a worktree");
    expect(boardColumnSubtitle(["done"])).toBe("merged or closed");
    expect(boardColumnSubtitle([])).toBe("");
  });
});
