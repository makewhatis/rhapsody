import { describe, expect, it } from "vitest";
import type { BlockedEntry } from "@/lib/api";
import type { ConsoleJobRow } from "@/lib/console-jobs";
import {
  type BoardLane,
  boardLaneTally,
  buildConsoleBoard,
  boardLaneOf,
  mergeIssueRows,
  parsePullRequest,
  runningRuns,
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
    // Each chip carries the RUN it represents (STUDIO-955), so clicking it can open that review
    // rather than the ticket's own (finished) implementation run.
    expect(card.reviewers.map((r) => r.issue)).toEqual([
      "pr:makewhatis/booch#539@jimmy",
      "pr:makewhatis/booch#540@alice",
    ]);
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

// STUDIO-952 — one card can hold runs on several providers on purpose, so each reviewer chip must
// carry its OWN row's provider rather than the card's. The [STUDIO-949] fixture is the real case:
// jerry implemented on fireworks-ai and sol reviewed on openai.
describe("a review's own provider on the chip (STUDIO-952)", () => {
  const studio949 = () =>
    buildConsoleBoard([
      row({
        issue: "STUDIO-949",
        trackerState: "In Review",
        assignee: "jerry",
        provider: "fireworks-ai",
      }),
      review("pr:makewhatis/rhapsody#186@sol", "STUDIO-949", { provider: "openai" }),
    ]);

  it("keeps the review's openai off the card's fireworks-ai", () => {
    const card = cards(studio949())[0];
    expect(card.provider).toBe("fireworks-ai");
    expect(card.reviewers.map((r) => r.provider)).toEqual(["openai"]);
  });

  it("gives a chip whose run recorded no provider an empty string, not the card's", () => {
    const card = cards(
      buildConsoleBoard([
        row({ issue: "LEGACY-1", assignee: "jerry", provider: "fireworks-ai" }),
        review("pr:makewhatis/rhapsody#1@sol", "LEGACY-1", { provider: "" }),
      ]),
    )[0];
    expect(card.reviewers[0].provider).toBe("");
  });
});

// STUDIO-955 — the Running lane's UNIT. A lane's count is a count of RUNS, so its contents must
// be runs: a review run belongs to a ticket parked in another lane, so it has no card anywhere and
// used to leave the lane simultaneously `5` and empty. `runningRuns` is the compact row per live
// review run that closes the gap.
describe("the Running lane's run rows (STUDIO-955)", () => {
  it("gives every live review run its own row, with the reviewer, ticket, provider and elapsed", () => {
    const runs = runningRuns([
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      review("pr:makewhatis/rhapsody#186@alice", "STUDIO-949", {
        status: "reviewing",
        statusLabel: "reviewing",
        runOutcome: "running",
        live: true,
        provider: "anthropic",
        elapsed: "4m",
      }),
    ]);

    expect(runs).toHaveLength(1);
    expect(runs[0]).toMatchObject({
      issue: "pr:makewhatis/rhapsody#186@alice",
      reviewer: "alice",
      ticket: "STUDIO-949",
      provider: "anthropic",
      projectSlug: "rhapsody",
      elapsed: "4m",
    });
  });

  it("leaves a finished review and a running ticket out of the rows", () => {
    const runs = runningRuns([
      row({ issue: "STUDIO-949", status: "run", statusLabel: "running", live: true }),
      review("pr:makewhatis/rhapsody#186@alice", "STUDIO-949", { status: "done", runOutcome: "completed" }),
    ]);
    // A running TICKET is already a card in the Running lane; a finished review shows as a chip on
    // its ticket. Neither is a run the lane is missing.
    expect(runs).toEqual([]);
  });
});

describe("boardLaneOf", () => {
  it("treats a live run as running even when the status word is stale", () => {
    expect(boardLaneOf({ status: "queued", live: true, trackerState: "Todo" })).toBe("running");
    expect(boardLaneOf({ status: "queued", live: false, trackerState: "Todo" })).toBe("queued");
  });
});

// STUDIO-931 — the board must not bucket a 50-row recency page. These are the two model pieces that
// make the fix: the tally that a lane reports, and the merge that lets the non-terminal fetch reach
// the board without widening the table.
describe("the board's whole-store lane counts (STUDIO-931)", () => {
  const counts = { running: 2, review: 3, queued: 1, blocked: 4 };

  it("reads each non-terminal lane's total from the tally, blocked folded into Queued", () => {
    expect(boardLaneTally("running", counts)).toBe(2);
    expect(boardLaneTally("review", counts)).toBe(3);
    // A blocked card waits in Queued, so the lane's total is both buckets together.
    expect(boardLaneTally("queued", counts)).toBe(5);
  });

  it("leaves Done uncounted, because its terminal set stays paged", () => {
    expect(boardLaneTally("done", counts)).toBeUndefined();
  });
});

describe("mergeIssueRows (STUDIO-931)", () => {
  const at = (id: number, issue: string) => ({ id, issue_identifier: issue });

  it("adds a fetched non-terminal row the page did not hold", () => {
    const merged = mergeIssueRows([at(1, "DONE-1")], [at(9, "STUDIO-877")]);
    expect(merged.map((r) => r.issue_identifier)).toEqual(["DONE-1", "STUDIO-877"]);
  });

  it("keeps the page's row when both hold the same issue — it is the fresher read", () => {
    const merged = mergeIssueRows([at(1, "STUDIO-877")], [at(9, "STUDIO-877")]);
    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe(1);
  });

  it("keeps unattributed runs individual rather than collapsing them", () => {
    const merged = mergeIssueRows([at(1, "")], [at(2, "")]);
    expect(merged).toHaveLength(2);
  });

  // The acceptance fixture at the model layer: the page is 50/50 completed — the shape of the
  // operator's own daemon — so a stuck non-terminal ticket the active fetch hands over is one row
  // the page could never have carried. `JobsView.test.tsx` drives the same fixture through the view.
  it("carries a non-terminal issue well outside the most recent 50 rows", () => {
    const page = Array.from({ length: 50 }, (_, i) => at(100 + i, `DONE-${i}`));
    const stuck = at(9, "STUDIO-877");
    const merged = mergeIssueRows(page, [stuck]);
    expect(merged).toHaveLength(51);
    expect(merged[merged.length - 1].issue_identifier).toBe("STUDIO-877");
    expect(boardLaneOf({ status: "queued", live: false, trackerState: "Backlog" })).toBe("queued");
  });
});
