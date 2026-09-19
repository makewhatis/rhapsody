// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { ConsoleJobRow, ConsoleJobCounts } from "@/lib/console-jobs";
import { BoardView, type BoardViewProps } from "./BoardView";

// The PR link is an ExternalLink, whose click seam calls `openExternal`; with no Tauri bridge that
// falls back to `window.open`, which jsdom reports as "Not implemented" noise. The seam is not what
// this file is testing (the link's href and the propagation stop are), so stub it.
vi.mock("@/lib/bindings", async (orig) => ({
  ...(await orig<typeof import("@/lib/bindings")>()),
  openExternal: vi.fn(),
}));

// STUDIO-925 — the board's two acceptance boxes at the render layer: a ticket with two review rows
// produces exactly ONE card carrying two reviewer chips, and a review row (null tracker_state) never
// becomes its own card. The regroup itself is pinned in `lib/console-board.test.ts`; this drives the
// view the operator actually sees.

let nextKey = 0;

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
    updatedAtMs: 1_700_000_000_000,
    needsYou: false,
    lifecycleResolved: true,
    ...over,
  };
}

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

const COUNTS: ConsoleJobCounts = { running: 3, review: 2, queued: 1, blocked: 0, needsYou: 2 };

function mount(
  rows: ConsoleJobRow[],
  onOpenJob = vi.fn(),
  counts = COUNTS,
  maxConcurrent = 4,
  over: Partial<BoardViewProps> = {},
) {
  render(
    <BoardView
      rows={rows}
      blocked={[]}
      filter="all"
      project=""
      counts={counts}
      maxConcurrent={maxConcurrent}
      refreshedAtMs={Date.now() - 30_000}
      nowMs={Date.now()}
      roster={["alice", "jimmy"]}
      onOpenJob={onOpenJob}
      pageNote=""
      hasMore={false}
      onLoadMore={vi.fn()}
      loadingMore={false}
      laneWidth="default"
      {...over}
    />,
  );
  return onOpenJob;
}

afterEach(cleanup);

describe("the board (STUDIO-925)", () => {
  it("renders one card per ticket with its reviews folded in as chips", () => {
    mount([
      row({ issue: "STUDIO-924", status: "run", statusLabel: "running" }),
      review("pr:makewhatis/booch#539@jimmy", "STUDIO-924"),
      review("pr:makewhatis/booch#540@alice", "STUDIO-924"),
    ]);

    const cards = document.querySelectorAll(".bcard");
    expect(cards).toHaveLength(1);
    expect(cards[0].textContent).toContain("STUDIO-924");

    const chips = [...document.querySelectorAll(".bcard .rchip")];
    expect(chips).toHaveLength(2);
    expect(chips.map((c) => c.querySelector(".o")?.textContent)).toEqual(["done", "done"]);
    expect(chips[0].textContent).toContain("jimmy");
    expect(chips[1].textContent).toContain("alice");
    // A completed review is green — the board's "two gates passed" signal.
    expect(chips[0].classList.contains("ok")).toBe(true);
  });

  it("shows a failed review as 'failed' in the bad tone, not the status' 'blocked'", () => {
    mount([
      row({ issue: "STUDIO-924", status: "run", statusLabel: "running" }),
      review("pr:makewhatis/booch#540@alice", "STUDIO-924", {
        status: "blocked",
        statusLabel: "blocked",
        runOutcome: "failed",
      }),
    ]);

    const chip = document.querySelector(".bcard .rchip");
    expect(chip?.querySelector(".o")?.textContent).toBe("failed");
    expect(chip?.classList.contains("bad")).toBe(true);
  });

  it("never renders a review row as its own card, even with a null tracker_state", () => {
    mount([
      row({
        issue: "STUDIO-924",
        status: "run",
        statusLabel: "running",
        trackerState: "In Progress",
      }),
      review("pr:makewhatis/booch#540@jimmy", "STUDIO-924"),
    ]);

    expect(document.querySelectorAll(".bcard")).toHaveLength(1);
    // The review row invents no lane: the four are fixed, and the one card is under Running.
    expect(document.querySelectorAll(".bcol")).toHaveLength(4);
    expect(document.querySelector('[data-lane="running"] .bkey')?.textContent).toBe("STUDIO-924");
    expect(document.querySelector(".btop .bkey")?.textContent).toBe("STUDIO-924");
  });

  it("links the card's PR to GitHub", () => {
    mount([
      row({ issue: "STUDIO-924" }),
      review("pr:makewhatis/booch#540@jimmy", "STUDIO-924"),
    ]);
    const link = screen.getByRole("link", {
      name: /Open makewhatis\/booch pull request 540/,
    });
    expect(link.getAttribute("href")).toBe("https://github.com/makewhatis/booch/pull/540");
    expect(link.textContent).toBe("#540");
  });

  it("does not open the ticket when the PR link is clicked", () => {
    const onOpen = mount([
      row({ issue: "STUDIO-924" }),
      review("pr:makewhatis/booch#540@jimmy", "STUDIO-924"),
    ]);
    fireEvent.click(screen.getByRole("link", { name: /Open makewhatis/ }));
    expect(onOpen).not.toHaveBeenCalled();
  });

  it("opens the ticket when its card is clicked or keyed", () => {
    const onOpen = mount([row({ issue: "STUDIO-924" })]);
    fireEvent.click(document.querySelector(".bcard")!);
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("STUDIO-924");
    fireEvent.keyDown(document.querySelector(".bcard")!, { key: "Enter" });
    expect(onOpen).toHaveBeenCalledTimes(2);
  });

  it("reads running against the cap, and the three counts beside it", () => {
    mount([row({ issue: "STUDIO-924" })]);
    expect(document.querySelector(".bfrun")?.textContent).toBe("Running 3 / 4");
    expect(document.querySelector(".bfoot")?.textContent).toContain("Blocked 0");
    expect(document.querySelector(".bfoot")?.textContent).toContain("Queued 1");
    expect(document.querySelector(".bfoot")?.textContent).toContain("In Review 2");
  });

  it("marks the footer when the cap is reached", () => {
    mount([row({ issue: "STUDIO-924" })], vi.fn(), { ...COUNTS, running: 4 }, 4);
    expect(document.querySelector(".bfrun")?.classList.contains("capped")).toBe(true);
  });

  it("narrows the cards by the status Seg and the project Select", () => {
    const rows = [
      row({ issue: "R-1", status: "run", statusLabel: "running" }),
      row({ issue: "B-1", status: "review", projectSlug: "booch" }),
    ];
    mount(rows, vi.fn(), COUNTS, 4, { filter: "run" });
    expect(document.querySelectorAll(".bcard")).toHaveLength(1);
    expect(document.querySelector(".btop .bkey")?.textContent).toBe("R-1");
    cleanup();

    mount(rows, vi.fn(), COUNTS, 4, { project: "booch" });
    expect(document.querySelectorAll(".bcard")).toHaveLength(1);
    expect(document.querySelector(".btop .bkey")?.textContent).toBe("B-1");
  });

  it("renders all four lanes with zero cards, each saying what its emptiness means", () => {
    mount([]);
    expect(document.querySelectorAll(".bcol")).toHaveLength(4);
    expect(document.querySelectorAll(".bcard")).toHaveLength(0);
    expect(screen.getByText("Nothing is waiting for an agent.")).toBeTruthy();
    expect(screen.getByText("No agent is running.")).toBeTruthy();
  });

  it("says the filter emptied a lane, not the pipeline", () => {
    mount([row({ issue: "R-1", status: "run", statusLabel: "running", trackerState: "Todo" })], vi.fn(), COUNTS, 4, {
      filter: "done",
    });
    expect(screen.queryByText("Nothing is waiting for an agent.")).toBeNull();
    expect(screen.getAllByText("No tickets here match the filter.")).toHaveLength(4);
  });

  it("captions each lane with a fixed line, and a running Todo ticket is under Running", () => {
    mount([row({ issue: "STUDIO-930", status: "run", statusLabel: "running", live: true, trackerState: "Todo" })]);
    const running = document.querySelector('[data-lane="running"]')!;
    expect(running.querySelector(".bkey")?.textContent).toBe("STUDIO-930");
    expect(running.querySelector(".bsub")?.textContent).toBe("an agent has the ticket right now");
    expect(document.querySelector('[data-lane="queued"] .bcard')).toBeNull();
  });

  it("draws the unused Running slots and the occupancy against the cap", () => {
    mount([row({ issue: "R-1", status: "run", statusLabel: "running", live: true, trackerState: "Todo" })]);
    const running = document.querySelector('[data-lane="running"]')!;
    expect(running.querySelector(".bcount")?.textContent).toBe("3 / 4");
    expect(running.querySelectorAll(".bslot")).toHaveLength(1);
    cleanup();
    mount([], vi.fn(), { ...COUNTS, running: 0 }, 4);
    expect(document.querySelectorAll('[data-lane="running"] .bslot')).toHaveLength(4);
    cleanup();
    // No known cap: no slots invented.
    mount([], vi.fn(), COUNTS, 0);
    expect(document.querySelectorAll(".bslot")).toHaveLength(0);
  });

  it("carries the lane width to the track", () => {
    mount([], vi.fn(), COUNTS, 4, { laneWidth: "wide" });
    expect(document.querySelector(".board")?.getAttribute("data-lane-width")).toBe("wide");
  });
});
