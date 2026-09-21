// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import type { ConsoleJobRow, ConsoleJobCounts } from "@/lib/console-jobs";
import { DEFAULT_BOARD_CARD_FIELDS } from "@/hooks/useBoardCardFields";
import { BoardView, type BoardViewProps } from "./BoardView";

// The board's own stylesheet. The narrow-lane guard below reads it because jsdom lays nothing out:
// the wrap and the nowrap ticket are the whole mechanism that keeps the row from clipping, so they
// are asserted at their source rather than trusted to a rendered width no test can measure.
const boardCss = readFileSync(path.resolve(__dirname, "../../../theme/console-views.css"), "utf8");

/** The declaration block for one selector, from its opening brace to the closing one. */
function cssRule(selector: string): string {
  const at = boardCss.indexOf(selector);
  expect(at, `no rule for ${selector}`).toBeGreaterThan(-1);
  return boardCss.slice(at, boardCss.indexOf("}", at));
}

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
      heldForHuman={[]}
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
      fields={DEFAULT_BOARD_CARD_FIELDS}
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

  it("reads a held-for-human ticket as deliberately held, not idle (STUDIO-949)", () => {
    mount(
      [row({ issue: "STUDIO-939", trackerState: "Todo", status: "queued", statusLabel: "queued" })],
      vi.fn(),
      COUNTS,
      4,
      {
        heldForHuman: [
          { issue_identifier: "STUDIO-939", title: "store work", project: "rhapsody" },
        ],
      },
    );
    const chip = document.querySelector(".bcard .hchip");
    expect(chip?.textContent).toBe("held for a human");
    // It waits in Queued — a lane is run status, and a held ticket has no run.
    expect(document.querySelector('[data-lane="queued"] .bkey')?.textContent).toBe("STUDIO-939");
  });

  it("draws a card for a held ticket that has NEVER RAN (STUDIO-949)", () => {
    // The production shape: a fresh `rhapsody:human` ticket has no history row and no live row, so
    // `rows` is empty and the hold is the only evidence the ticket exists. Without synthesizing from
    // the hold set the board shows nothing for it — the silent stall this feature exists to end.
    mount([], vi.fn(), COUNTS, 4, {
      heldForHuman: [{ issue_identifier: "STUDIO-939", title: "store work", project: "rhapsody" }],
    });
    const card = document.querySelector('.bcard[aria-label="STUDIO-939 store work"]');
    expect(card).not.toBeNull();
    expect(card?.querySelector(".hchip")?.textContent).toBe("held for a human");
    expect(document.querySelector('[data-lane="queued"] .bkey')?.textContent).toBe("STUDIO-939");
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

  // STUDIO-932 — the board is NOT status-filtered. The lanes ARE that axis; only the project Select
  // narrows what is drawn. A status filter no longer has a prop to reach this view with.
  it("narrows the cards by the project Select only", () => {
    const rows = [
      row({ issue: "R-1", status: "run", statusLabel: "running" }),
      row({ issue: "B-1", status: "review", projectSlug: "booch" }),
    ];
    mount(rows);
    expect(document.querySelectorAll(".bcard")).toHaveLength(2);
    cleanup();

    mount(rows, vi.fn(), COUNTS, 4, { project: "booch" });
    expect(document.querySelectorAll(".bcard")).toHaveLength(1);
    expect(document.querySelector(".btop .bkey")?.textContent).toBe("B-1");
  });

  // The five card-field chips (STUDIO-932). Key, title and status pill are the card's identity and
  // must survive every combination; only the named element goes.
  it("hides and shows each optional card element", () => {
    mount([
      row({ issue: "R-1", assignee: "alice", trackerState: "In Progress" }),
      review("pr:makewhatis/booch#540@jimmy", "R-1"),
    ]);
    expect(document.querySelector(".bcard .who2")).not.toBeNull();
    expect(document.querySelector(".bcard .bproj")).not.toBeNull();
    expect(document.querySelectorAll(".bcard .rchip")).toHaveLength(1);
    expect(document.querySelector(".bcard .bpr")).not.toBeNull();
    cleanup();

    mount([row({ issue: "R-1", assignee: "alice" })], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, assignee: false },
    });
    expect(document.querySelector(".bcard .who2")).toBeNull();
    // The card's identity is not a toggle.
    expect(document.querySelector(".bcard .bkey")?.textContent).toBe("R-1");
    expect(document.querySelector(".bcard .pill")).not.toBeNull();
    cleanup();

    mount([row({ issue: "R-1" })], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, project: false },
    });
    expect(document.querySelector(".bcard .bproj")).toBeNull();
    cleanup();

    // Harness off hides the author's chip AND every review chip's, so the control means one thing
    // across the card. Without the review row here the review provider still leaked through.
    mount(
      [
        row({ issue: "R-1", provider: "fireworks" }),
        review("pr:makewhatis/booch#540@jimmy", "R-1", { provider: "openai" }),
      ],
      vi.fn(),
      COUNTS,
      4,
      { fields: { ...DEFAULT_BOARD_CARD_FIELDS, harness: false } },
    );
    expect(document.querySelector(".bcard .provbadge")).toBeNull();
    // The review chip itself survives — only its provider is gated.
    expect(document.querySelector(".bcard .rchip")?.textContent).toContain("jimmy");
    // …and the provider is gone from the tooltip too, not just the badge.
    expect(document.querySelector(".bcard .rchip")?.getAttribute("title")).toBe("jimmy on #540 · review done");
    cleanup();

    mount([row({ issue: "R-1" }), review("pr:makewhatis/booch#540@jimmy", "R-1")], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, reviews: false },
    });
    expect(document.querySelectorAll(".bcard .rchip")).toHaveLength(0);
    cleanup();

    mount([row({ issue: "R-1" }), review("pr:makewhatis/booch#540@jimmy", "R-1")], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, pullRequest: false },
    });
    expect(document.querySelector(".bcard .bpr")).toBeNull();
  });

  it("drops the meta row entirely, rather than leaving a gap, when all three meta fields are off", () => {
    mount([row({ issue: "R-1", assignee: "alice", provider: "fireworks" })], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, assignee: false, project: false, harness: false },
    });
    expect(document.querySelector(".bcard .bmeta")).toBeNull();
  });

  it("renders all four lanes with zero cards, each saying what its emptiness means", () => {
    mount([], vi.fn(), { ...COUNTS, running: 0, queued: 0, review: 0, blocked: 0 });
    expect(document.querySelectorAll(".bcol")).toHaveLength(4);
    expect(document.querySelectorAll(".bcard")).toHaveLength(0);
    expect(screen.getByText("Nothing is waiting for an agent.")).toBeTruthy();
    expect(screen.getByText("No agent is running.")).toBeTruthy();
  });

  it("shows a live review holding a seat as a run row in Running, not as an idle pool", () => {
    const rows = [
      row({ issue: "R-1", status: "review", trackerState: "In Review" }),
      review("R-1#5@jimmy", "R-1", { status: "reviewing", statusLabel: "reviewing", live: true }),
    ];
    mount(rows, vi.fn(), { ...COUNTS, running: 1 });
    const lane = document.querySelector('[data-lane="running"]');
    expect(lane?.querySelector(".bcount")?.textContent).toBe("1 / 4");
    expect(lane?.querySelectorAll(".bslot")).toHaveLength(3);
    expect(screen.queryByText("No agent is running.")).toBeNull();
    // The seat is held by a REVIEW run, so it shows as a run row (STUDIO-955) rather than only being
    // asserted in words while the lane renders nothing.
    expect(lane?.querySelectorAll(".brun")).toHaveLength(1);
  });

  it("does not claim an empty lane is empty when the listing is a truncated page", () => {
    mount([row({ issue: "D-1", status: "done", trackerState: "Done" })], vi.fn(), { ...COUNTS, queued: 5, running: 0 }, 4, {
      hasMore: true,
    });
    // The lane is NOT empty — the whole-store tally says five are waiting and the page really is cut
    // — so it reports the number it cannot render rather than the "not loaded yet" hedge (STUDIO-931).
    const queued = document.querySelector('[data-lane="queued"] .bempty');
    expect(queued?.textContent).toMatch(/5 in this lane/);
    expect(screen.queryByText("Nothing is waiting for an agent.")).toBeNull();
    // The whole-store running tally is 0, so that lane may still say so.
    expect(screen.getByText("No agent is running.")).toBeTruthy();
  });

  // STUDIO-965 — the whole-store tally is only allowed to stand BESIDE a row the page cannot hold
  // when the page is genuinely cut. A complete page has no such gap to explain, so the header is the
  // fold's card count; a store number it cannot reconcile is exactly the contradiction this ticket
  // removes. THE `1 in this lane` COPY IS THE DEFECT: it sent an operator looking through history for
  // a card that was folded onto a Done ticket all along.
  it("shows a lane's whole-store total only while the page is genuinely truncated", () => {
    const rows = [row({ issue: "D-1", status: "done", trackerState: "Done" })];
    const tally = { ...COUNTS, running: 0, review: 0, queued: 0, blocked: 1 };
    mount(rows, vi.fn(), tally, 4, { hasMore: true });
    const queued = document.querySelector('[data-lane="queued"]')!;
    expect(queued.querySelector(".bcount")?.textContent).toBe("1");
    expect(queued.querySelector(".bempty")?.textContent).toMatch(/1 in this lane/);
    expect(screen.queryByText("Nothing is waiting for an agent.")).toBeNull();
    cleanup();

    // The same store tally on a COMPLETE page: the board holds every row there is, so the lane
    // reports what it draws and makes no pagination claim.
    mount(rows, vi.fn(), tally);
    const complete = document.querySelector('[data-lane="queued"]')!;
    expect(complete.querySelector(".bcount")?.textContent).toBe("0");
    expect(complete.querySelector(".bempty")?.textContent).toBe("Nothing is waiting for an agent.");
    expect(document.body.textContent).not.toMatch(/in this lane/);
  });

  // A lane holding SOME of its cards on a genuinely truncated page says so rather than implying the
  // page is the lane.
  it("notes the cards a counted lane has not rendered while truncated", () => {
    mount([row({ issue: "R-1", trackerState: "In Review", status: "review" })], vi.fn(), {
      ...COUNTS,
      review: 3,
    }, 4, { hasMore: true });
    const review = document.querySelector('[data-lane="review"]')!;
    expect(review.querySelector(".bcount")?.textContent).toBe("3");
    expect(review.querySelectorAll(".bcard")).toHaveLength(1);
    expect(review.textContent).toMatch(/2 more in this lane/);
  });

  it("says the project filter emptied a lane, not the pipeline", () => {
    mount([row({ issue: "R-1", status: "run", statusLabel: "running", trackerState: "Todo" })], vi.fn(), COUNTS, 4, {
      project: "other",
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

// STUDIO-965 — the lane header and the lane body must answer the same question. The observed screen:
// three failed/interrupted reviews of two Done tickets were tallied as their own rows (`Queued 4` —
// one actual card), and a live-running ticket whose tracker state read In Review was tallied In
// Review while its card sat in Running. The counts endpoint now folds a review run onto the ticket
// it reviews and buckets a live ticket by its run, so the store tally equals the fold. This drives
// the operator's screen through the view and asserts no lane claims a card it cannot draw.
describe("the lane header matches the lane body (STUDIO-965)", () => {
  it("draws no phantom on the reported screen", () => {
    const rows = [
      row({ issue: "STUDIO-949", trackerState: "Done", status: "done", statusLabel: "done" }),
      review("pr:makewhatis/rhapsody#186@sol", "STUDIO-949", {
        status: "blocked",
        statusLabel: "blocked",
        runOutcome: "failed",
      }),
      review("pr:makewhatis/rhapsody#192@sol", "STUDIO-949", {
        status: "blocked",
        statusLabel: "blocked",
        runOutcome: "failed",
      }),
      row({ issue: "STUDIO-956", trackerState: "Done", status: "done", statusLabel: "done" }),
      review("pr:makewhatis/rhapsody#194@sol", "STUDIO-956", {
        status: "queued",
        statusLabel: "queued",
        runOutcome: "interrupted",
      }),
      row({ issue: "STUDIO-958", trackerState: "Todo", status: "queued", statusLabel: "queued" }),
      // A live run whose tracker state reads In Review: `boardLaneOf` puts its card in Running.
      row({
        issue: "STUDIO-963",
        trackerState: "In Review",
        status: "run",
        statusLabel: "running",
        live: true,
      }),
    ];
    // The PRE-FIX store tally, exactly as the operator's daemon served it: the three review rows
    // billed as their own queued/blocked jobs, the live ticket counted In Review. A complete page has
    // no such gap to explain, so every header is the fold's card count and the pagination copy never
    // appears — THE MUTATION: make that copy unconditional and the `/in this lane/` assertion reds.
    mount(rows, vi.fn(), { running: 1, review: 1, queued: 3, blocked: 2, needsYou: 2 }, 6);

    const lane = (id: string) => document.querySelector(`[data-lane="${id}"]`)!;
    const count = (id: string) => lane(id).querySelector(".bcount")?.textContent;

    // Queued 4 -> 1 card becomes Queued 1 -> 1 card: the two failed reviews are chips on a Done
    // card, not Queued jobs.
    expect(count("queued")).toBe("1");
    expect(lane("queued").querySelectorAll(".bcard")).toHaveLength(1);
    // The live ticket counts in Running, where its card is drawn, not In Review.
    expect(count("running")).toBe("1 / 6");
    expect(lane("running").querySelectorAll(".bcard")).toHaveLength(1);
    expect(lane("running").textContent).toContain("STUDIO-963");
    // In Review 1 -> 0 cards: no lane claims a card it cannot draw.
    expect(count("review")).toBe("0");
    expect(lane("review").querySelectorAll(".bcard")).toHaveLength(0);
    // The two Done cards carry all three reviews as chips, so nothing counts nowhere.
    expect(lane("done").querySelectorAll(".bcard")).toHaveLength(2);
    expect(document.querySelectorAll(".bcard .rchip")).toHaveLength(3);
    expect(document.body.textContent).not.toMatch(/in this lane|not among the jobs loaded/);
  });
});

// STUDIO-952 — the card's harness chip is the AUTHOR's, and each review chip carries its own row's
// provider. The STUDIO-949 fixture is the observed misread: the maintainer read one unlabelled chip
// as "sol running on fireworks-ai" when sol had in fact reviewed on openai.
describe("the card's harness chips (STUDIO-952)", () => {
  it("shows jerry's fireworks-ai on the meta row and sol's openai on the review chip (STUDIO-949)", () => {
    mount([
      row({ issue: "STUDIO-949", assignee: "jerry", provider: "fireworks-ai" }),
      review("pr:makewhatis/rhapsody#186@sol", "STUDIO-949", { provider: "openai" }),
    ]);

    // The card's own chip is the implementation run's — jerry's.
    expect(document.querySelector(".bcard .bmeta .provbadge")?.textContent).toBe("fireworks-ai");
    // The review chip is the review run's — sol's, a DIFFERENT provider, rendered distinctly.
    expect(document.querySelector(".bcard .rchip .provbadge")?.textContent).toBe("openai");
    // The chip's tooltip names that same provider, so the fact is reachable without the badge too.
    expect(document.querySelector(".bcard .rchip")?.getAttribute("title")).toBe(
      "sol on #186 · openai · review done",
    );
  });

  it("names the author in the meta-row tooltip even when the assignee chip is toggled off", () => {
    mount([row({ issue: "STUDIO-949", assignee: "jerry", provider: "fireworks-ai" })], vi.fn(), COUNTS, 4, {
      fields: { ...DEFAULT_BOARD_CARD_FIELDS, assignee: false },
    });
    expect(document.querySelector(".bcard .provbadge")?.getAttribute("title")).toBe(
      "jerry ran on fireworks-ai",
    );
  });

  it("attaches the meta-row harness chip to the author, not the card", () => {
    mount([row({ issue: "STUDIO-949", assignee: "jerry", provider: "fireworks-ai" })]);
    // Nested in `.who2`, the author's own element, so it cannot be read as the card's.
    const who = document.querySelector(".bcard .who2");
    expect(who?.textContent).toContain("jerry");
    expect(who?.querySelector(".provbadge")?.textContent).toBe("fireworks-ai");
    expect(who?.querySelector(".provbadge")?.getAttribute("title")).toBe("jerry ran on fireworks-ai");
  });

  it("renders no harness chip, and no placeholder, when a run recorded no provider", () => {
    mount([row({ issue: "LEGACY-1", assignee: "jerry", provider: "" })]);
    expect(document.querySelector(".bcard .provbadge")).toBeNull();
    // The author slot itself survives — only the absent provider is skipped.
    expect(document.querySelector(".bcard .who2")?.textContent).toContain("jerry");
  });

  it("keeps a providerless review chip showing who and how it ended", () => {
    mount([
      row({ issue: "LEGACY-1", assignee: "jerry", provider: "fireworks-ai" }),
      review("pr:makewhatis/rhapsody#1@sol", "LEGACY-1", { provider: "" }),
    ]);
    const chip = document.querySelector(".bcard .rchip");
    expect(chip?.textContent).toContain("sol");
    expect(chip?.querySelector(".o")?.textContent).toBe("done");
    expect(chip?.querySelector(".provbadge")).toBeNull();
  });
});

// STUDIO-955 — two defects from one tension: a card is a ticket, a lane's count is runs.
//
// 1. A reviewer chip is one RUN. The observed case: a chip reading `running` opened the ticket's
//    own implementation run, which was `done`. The chip bubbles to the card's onClick because it
//    had no handler and no stopPropagation (the PR link at `:327` is the precedent that does).
// 2. The Running lane counted runs but rendered ticket cards, so five live reviews left it reading
//    `5 / 6` beside zero cards and a caption asserting an agent had the ticket.
describe("the reviewer chip and the Running lane (STUDIO-955)", () => {
  const liveReview = (pr: string, of: string, over: Partial<ConsoleJobRow> = {}) =>
    review(pr, of, {
      status: "reviewing",
      statusLabel: "reviewing",
      runOutcome: "running",
      live: true,
      ...over,
    });

  it("a chip reading running opens that review's run, not the ticket's done one", () => {
    const onOpen = mount([
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      liveReview("pr:makewhatis/rhapsody#186@alice", "STUDIO-949"),
    ]);

    const chip = within(document.querySelector(".bcard")!).getByRole("link", { name: /alice/ });
    expect(chip.getAttribute("aria-label")).toBe("Open alice's review of STUDIO-949");

    fireEvent.click(chip);
    // Exactly once, with the REVIEW's key: without the chip's stopPropagation the click also
    // bubbles to the card and opens STUDIO-949, the author's finished implementation run.
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("pr:makewhatis/rhapsody#186@alice");
  });

  it("activates the reviewer chip from the keyboard without also opening the card", () => {
    const onOpen = mount([
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      liveReview("pr:makewhatis/rhapsody#186@alice", "STUDIO-949"),
    ]);

    const chip = within(document.querySelector(".bcard")!).getByRole("link", { name: /alice/ });
    chip.focus();
    fireEvent.keyDown(chip, { key: "Enter" });
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("pr:makewhatis/rhapsody#186@alice");
    onOpen.mockClear();
    fireEvent.keyDown(chip, { key: " " });
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("pr:makewhatis/rhapsody#186@alice");
  });

  it("counts what it shows: five running reviews render five rows, not an empty lane", () => {
    const rows = [
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      ...["alice", "jimmy", "sol", "jerry", "kim"].map((who, i) =>
        liveReview(`pr:makewhatis/rhapsody#${186 + i}@${who}`, "STUDIO-949"),
      ),
    ];
    mount(rows, vi.fn(), { ...COUNTS, running: 5, review: 1 }, 6);

    const running = document.querySelector('[data-lane="running"]')!;
    expect(running.querySelector(".bcount")?.textContent).toBe("5 / 6");
    expect(running.querySelectorAll(".brun")).toHaveLength(5);
    expect(running.querySelector(".bempty")).toBeNull();
    // The free-slot rack is untouched: five seats of six are taken.
    expect(running.querySelectorAll(".bslot")).toHaveLength(1);
  });

  it("names each running run's reviewer, ticket and clock", () => {
    mount([
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      liveReview("pr:makewhatis/rhapsody#186@alice", "STUDIO-949", {
        provider: "anthropic",
        elapsed: "4m",
      }),
    ]);

    const run = document.querySelector('[data-lane="running"] .brun')!;
    expect(run.textContent).toContain("alice");
    expect(run.textContent).toContain("STUDIO-949");
    expect(run.textContent).toContain("anthropic");
    expect(run.textContent).toContain("4m");
  });

  it("opens a running run's own trace when its row is clicked or keyed", () => {
    const onOpen = mount([
      row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
      liveReview("pr:makewhatis/rhapsody#186@alice", "STUDIO-949"),
    ]);
    const run = document.querySelector('[data-lane="running"] .brun')!;
    fireEvent.click(run);
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("pr:makewhatis/rhapsody#186@alice");
  });

  it("narrows the running run rows with the project Select, exactly as it narrows cards", () => {
    mount(
      [
        row({ issue: "R-1", status: "review", trackerState: "In Review" }),
        liveReview("pr:makewhatis/rhapsody#1@alice", "R-1", { projectSlug: "rhapsody" }),
        liveReview("pr:makewhatis/booch#2@jimmy", "R-1", { projectSlug: "booch" }),
      ],
      vi.fn(),
      { ...COUNTS, running: 2 },
      4,
      { project: "booch" },
    );
    const running = document.querySelector('[data-lane="running"]')!;
    expect(running.querySelectorAll(".brun")).toHaveLength(1);
    expect(running.querySelector(".brun")?.textContent).toContain("jimmy");
  });

  // The lane's count must describe what it SHOWS in every branch, not just once the tally and the
  // typed config have both landed. `maxConcurrent` is 0 for the whole of every first paint — the
  // config query has not resolved — so the `maxConcurrent === 0` header branch is the window the
  // operator actually sees on load, and it must count the run rows the lane renders beneath it.
  it("counts the run rows it shows while the agent cap is still unknown", () => {
    mount(
      [
        row({ issue: "STUDIO-949", status: "review", trackerState: "In Review" }),
        liveReview("pr:makewhatis/rhapsody#186@alice", "STUDIO-949"),
        liveReview("pr:makewhatis/rhapsody#187@jimmy", "STUDIO-949"),
      ],
      vi.fn(),
      COUNTS,
      0,
      { counts: undefined },
    );

    const running = document.querySelector('[data-lane="running"]')!;
    expect(running.querySelectorAll(".brun")).toHaveLength(2);
    expect(running.querySelector(".bcount")?.textContent).toBe("2");
  });

  // The pre-tally occupancy fallback must be whole-pool, exactly as the comment above it says the
  // card half already is. Under a project filter the run rows ARE narrowed — that is what the lane
  // shows — but the seats they hold are not, so a filter must not make a full pool look idle.
  it("keeps occupancy whole-pool while the tally is unknown, even under a project filter", () => {
    mount(
      [
        row({ issue: "R-1", status: "review", trackerState: "In Review" }),
        liveReview("pr:makewhatis/rhapsody#1@alice", "R-1", { projectSlug: "rhapsody" }),
        liveReview("pr:makewhatis/booch#2@jimmy", "R-1", { projectSlug: "booch" }),
      ],
      vi.fn(),
      COUNTS,
      6,
      { project: "booch", counts: undefined },
    );

    const running = document.querySelector('[data-lane="running"]')!;
    // One rule is shown (the filter narrows the rows), but both hold a seat.
    expect(running.querySelectorAll(".brun")).toHaveLength(1);
    expect(running.querySelector(".bcount")?.textContent).toBe("2 / 6");
    expect(running.querySelectorAll(".bslot")).toHaveLength(4);
  });
});

// STUDIO-968 — the row said two different things to two readers. A screen reader was told
// "alice's review of STUDIO-957" while the visible row rendered a bare "alice STUDIO-957" with no
// verb: the accessible name was MORE informative than the visible label, which is the wrong way
// round. These pin the visible side, that both sides say the same phrase, and that a run whose
// origin never resolved names a pull request rather than leaving a verb hanging off nothing.
describe("the running review row's relationship (STUDIO-968)", () => {
  const liveReview = (pr: string, of: string, over: Partial<ConsoleJobRow> = {}) =>
    review(pr, of, {
      status: "reviewing",
      statusLabel: "reviewing",
      runOutcome: "running",
      live: true,
      ...over,
    });

  /** Mount one ticket plus one live review run of it, and hand back the compact row. */
  function mountRun(of: string, over: Partial<ConsoleJobRow> = {}) {
    mount([
      row({ issue: "STUDIO-957", status: "review", trackerState: "In Review" }),
      liveReview("pr:makewhatis/rhapsody#186@alice", of, {
        provider: "anthropic",
        elapsed: "4m",
        ...over,
      }),
    ]);
    return document.querySelector('[data-lane="running"] .brun')!;
  }

  it("names the relationship in the visible row, not only in its accessible name", () => {
    const run = mountRun("STUDIO-957");
    // Reverting to the bare `{reviewer}{ticket}` render reds this line: the visible row would carry
    // no verb a sighted reader could see. Asserting the aria-label alone would stay green — which is
    // exactly how this shipped, so the visible text is what is asserted here.
    expect(run.textContent).toContain("alice is reviewing STUDIO-957");
    // The badge and clock the maintainer relies on survive the added prose.
    expect(run.textContent).toContain("anthropic");
    expect(run.textContent).toContain("4m");
  });

  it("keeps the visible text and the accessible name saying the same thing", () => {
    const run = mountRun("STUDIO-957");
    const phrase = "alice is reviewing STUDIO-957";
    // One phrase is asserted against BOTH: diverging either the visible text or the accessible name
    // alone reds this line, so the two cannot drift apart again with the sign flipped.
    expect(run.textContent).toContain(phrase);
    expect(run.getAttribute("aria-label")).toContain(phrase);
  });

  it("renders a ticketless review run as a review of a pull request, with no dangling verb", () => {
    const run = mountRun("");
    // "alice is reviewing" with nothing after it is the name-and-nothing shape the ticket calls out.
    // A ticketless review is still of a pull request, so the verb gets an object rather than hanging.
    expect(run.textContent).toContain("alice is reviewing a pull request");
    expect(run.getAttribute("aria-label")).toContain("alice is reviewing a pull request");
  });

  // Compact is the 224px track. The row already carried up to four facts, and a long provider
  // ("fireworks-ai") pressed that width on its own; the verb would turn a near-miss into a clip of
  // the ticket id or the badge. jsdom lays nothing out, so the mechanism is asserted at its source:
  // the row wraps rather than truncating, and the ticket id never breaks mid-key.
  it("wraps at the narrowest lane instead of clipping the ticket", () => {
    expect(cssRule(".rh-console .brun")).toMatch(/flex-wrap:\s*wrap/);
    expect(cssRule(".rh-console .brun .rtk")).toMatch(/white-space:\s*nowrap/);
  });
});
