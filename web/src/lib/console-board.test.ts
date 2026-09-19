import { describe, expect, it } from "vitest";
import type { BlockedEntry } from "@/lib/api";
import type { ConsoleJobRow } from "@/lib/console-jobs";
import {
  type BoardLane,
  buildConsoleBoard,
  boardLaneOf,
  parsePullRequest,
} from "@/lib/console-board";

// STUDIO-925 — the board is a client-side regroup of rows the console already holds: a CARD is a
// ticket (`review_run` falsy, keyed by `issue_identifier`), a LANE is its run status (STUDIO-930), and a
// review row (`review_run` true, `review_of` naming its ticket) is folded onto the card as a chip.

let nextKey = 0;

const cards = (board: BoardLane[]) => board.flatMap((l) => l.cards);
const laneIssues = (board: BoardLane[], id: BoardLane["id"]) =>
  board.find((l) => l.id === id)?.cards.map((c) => c.issue);

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
    runOutcome: "completed",
    costs: [],
    elapsed: "",
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

    expect(cards(board)).toHaveLength(1);
    const card = cards(board)[0];
    expect(card.issue).toBe("STUDIO-924");
    expect(card.reviewers.map((r) => r.reviewer)).toEqual(["jimmy", "alice"]);
    expect(card.reviewers.map((r) => r.outcome)).toEqual(["done", "done"]);
    // ...and the card carries the PR its reviews are on.
    expect(card.pr?.number).toBe(539);
    expect(card.pr?.url).toBe("https://github.com/makewhatis/booch/pull/539");
  });

  it("gives a failed review the run's own word, not the ticket status' 'blocked'", () => {
    // A failed review run maps to the `blocked` status (consoleJobStatus), so its statusLabel is
    // "blocked" while its run outcome is "failed". The chip must say "failed" — that is the fact the
    // board exists to show — while `status` still colours it red.
    const board = buildConsoleBoard([
      row({ issue: "STUDIO-924", status: "run", statusLabel: "running" }),
      review("pr:makewhatis/booch#540@alice", "STUDIO-924", {
        status: "blocked",
        statusLabel: "blocked",
        runOutcome: "failed",
      }),
    ]);

    const chip = cards(board)[0].reviewers[0];
    expect(chip.outcome).toBe("failed");
    expect(chip.status).toBe("blocked");
  });

  it("never makes a review row its own card, even when its tracker_state is null", () => {
    const board = buildConsoleBoard([
      row({ issue: "STUDIO-924", status: "run", statusLabel: "running", trackerState: "Todo" }),
      review("pr:makewhatis/booch#540@jimmy", "STUDIO-924"),
    ]);
    expect(cards(board).map((c) => c.issue)).toEqual(["STUDIO-924"]);
  });

  it("drops a review row whose origin never resolved to a ticket", () => {
    expect(cards(buildConsoleBoard([review("pr:makewhatis/booch#540@jimmy", "")]))).toEqual([]);
  });

  it("always yields the four lanes in order, even with no cards at all", () => {
    const withOne = buildConsoleBoard([row({ issue: "D", trackerState: "Done", status: "done" })]);
    for (const board of [buildConsoleBoard([]), withOne]) {
      expect(board.map((l) => l.id)).toEqual(["queued", "running", "review", "done"]);
      expect(board.map((l) => l.name)).toEqual(["Queued", "Running", "In Review", "Done"]);
    }
    const empty = buildConsoleBoard([]);
    expect(empty.every((l) => l.cards.length === 0)).toBe(true);
    // Every lane says what its emptiness means, and every caption is fixed and non-empty.
    expect(empty.every((l) => l.empty !== "" && l.caption !== "")).toBe(true);
  });

  it("puts a Todo ticket with a live run in Running, not a Todo lane", () => {
    const board = buildConsoleBoard([
      row({ issue: "STUDIO-930", trackerState: "Todo", status: "run", statusLabel: "running", live: true }),
    ]);
    expect(laneIssues(board, "running")).toEqual(["STUDIO-930"]);
    expect(laneIssues(board, "queued")).toEqual([]);
  });

  it("sorts each ticket into the lane its run status names, and lets the tracker decide Done", () => {
    const board = buildConsoleBoard([
      row({ issue: "Q", trackerState: "Todo", status: "queued", statusLabel: "queued" }),
      row({ issue: "B", trackerState: "Todo", status: "blocked", statusLabel: "blocked" }),
      row({ issue: "RV", trackerState: "Todo", status: "reviewing", statusLabel: "reviewing", live: true }),
      row({ issue: "R", trackerState: "In Review", status: "review" }),
      row({ issue: "D", trackerState: "Done", status: "done", statusLabel: "done" }),
      // A terminal tracker state wins over whatever the last run's status says.
      row({ issue: "C", trackerState: "Canceled", status: "blocked", statusLabel: "blocked" }),
    ]);
    expect(laneIssues(board, "queued")).toEqual(["Q", "B"]);
    expect(laneIssues(board, "running")).toEqual(["RV"]);
    expect(laneIssues(board, "review")).toEqual(["R"]);
    expect(laneIssues(board, "done")).toEqual(["D", "C"]);
  });

  it("keeps a ticket with no resolved tracker state on the board", () => {
    const board = buildConsoleBoard([
      row({ issue: "LEGACY", trackerState: "", status: "queued", statusLabel: "queued" }),
    ]);
    expect(laneIssues(board, "queued")).toEqual(["LEGACY"]);
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
    expect(cards(board)[0].dependencies).toEqual(["STUDIO-900 · In Progress"]);
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
    expect(cards(board)[0].assignee).toBe("jerry");
    expect(cards(board)[0].provider).toBe("fireworks-ai");
  });
});

describe("boardLaneOf", () => {
  it("treats a live run as running even when the status word is stale", () => {
    expect(boardLaneOf({ status: "queued", live: true, trackerState: "Todo" })).toBe("running");
    expect(boardLaneOf({ status: "queued", live: false, trackerState: "Todo" })).toBe("queued");
  });
});
