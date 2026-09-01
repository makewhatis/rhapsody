// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type * as React from "react";
import type {
  TeamsFact,
  TeamsOverview,
  TeamsRecallResponse,
  TeamsRoomMessage,
  TeamsRoomResponse,
} from "@/lib/api";
import { DEFAULT_ROOM_WINDOW } from "@/lib/room-model";

// The Teams console (STUDIO-681 §5) — one test per acceptance box in §10, sub-ticket 3.
//
// The room fixture is dated relative to a FROZEN clock rather than to the wall clock, because two
// of the boxes are about calendar days: "Today" has to be today for the divider assertion, and the
// day pager has to have a third day to reveal. Times sit mid-UTC-day so the local calendar day the
// feed groups on is the same one whatever timezone the runner is in.

const h = vi.hoisted(() => ({
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
  postTeamsRoom: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchTeamsRoom: h.fetchTeamsRoom,
    fetchTeamsRecall: h.fetchTeamsRecall,
    postTeamsRoom: h.postTeamsRoom,
  };
});

import { TeamsConsole } from "@/components/console/teams/TeamsConsole";

const NOW = new Date("2026-09-01T18:00:00Z");
/** `YYYY-MM-DD` for a day offset from the frozen clock, in the runner's local zone. */
function day(offset: number): string {
  const d = new Date(NOW);
  d.setDate(d.getDate() + offset);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}
/**
 * An RFC3339 stamp `minutes` before midday UTC, `offset` days back. Everything stays inside one
 * hour either side of 12:00 UTC on purpose: the feed groups and prints in LOCAL time (the house
 * style of `lib/format`), so a fixture near a midnight would land on a different calendar day
 * depending on the runner's timezone and take the day-divider assertions with it.
 */
function at(offset: number, minutes: number): string {
  const d = new Date(NOW);
  d.setDate(d.getDate() + offset);
  d.setUTCHours(11, 59 - minutes, 0, 0);
  return d.toISOString();
}

const overview: TeamsOverview = {
  enabled: true,
  manager_mode: "labels",
  default_identity: "alice",
  backend: "local",
  roster: [
    { name: "alice", profile: "swe", labels: ["rust"], bank: "agent-alice", max_concurrent: 0, live_runs: 1, tickets: ["STUDIO-684"] },
    { name: "jimmy", profile: "swe", labels: [], bank: "agent-jimmy", max_concurrent: 0, live_runs: 0, tickets: [] },
  ],
};

const LONG_BODY =
  "STUDIO-678 review round 2 addressed — the operator found a real TOCTOU I missed. " +
  "EarsCycle.issues is one immutable fetch per cycle, so every guard that reads iss.labels to " +
  "decide whether to write is deciding on state that predates the writes made earlier in the same " +
  "cycle, which silently defeated three separate bounds at once and needed PassWrites to carry the " +
  "pass's own writes forward.";

function m(over: Partial<TeamsRoomMessage> & { id: string }): TeamsRoomMessage {
  return { from: "@manager", to: "*", at: at(0, 6), body: "", refs: [], ...over };
}

// Oldest first, exactly as the daemon serves the room.
const messages: TeamsRoomMessage[] = [
  m({ id: `${day(-2)}:1`, from: "jimmy", at: at(-2, 6), body: "Two days ago, from jimmy.", refs: ["STUDIO-660"] }),
  m({ id: `${day(-1)}:1`, at: at(-1, 8), body: "Assigned STUDIO-638 to alice (deterministic). Reason: least-loaded (3 open)." }),
  m({ id: `${day(-1)}:2`, at: at(-1, 7), body: "Assigned STUDIO-403 to jimmy (deterministic). Reason: least-loaded (4 open)." }),
  m({ id: `${day(-1)}:3`, at: at(-1, 6), body: "Assigned STUDIO-402 to alice (deterministic). Reason: least-loaded (5 open)." }),
  m({ id: `${day(-1)}:4`, at: at(-1, 5), body: "Cleaned up 11 stray identity label(s) on review-state tickets that no run ever wore: X-1." }),
  m({ id: `${day(0)}:1`, from: "alice", at: at(0, 5), body: LONG_BODY, refs: ["STUDIO-678", "https://github.com/x/y/pull/70"] }),
  m({ id: `${day(0)}:2`, from: "jimmy", at: at(0, 4), body: "STUDIO-676 up for review — all 15 checks pass.", refs: ["STUDIO-676"] }),
  m({ id: `${day(0)}:3`, from: "operator", at: at(0, 3), body: "Someone want to review the export PR?", refs: ["STUDIO-654"] }),
  m({
    id: `${day(0)}:4`,
    at: at(0, 2),
    body:
      "REVIEW QUORUM FAILED for STUDIO-678: no review ticket could be created for " +
      "https://github.com/x/y/pull/70 (asked: jimmy).",
    refs: ["STUDIO-678"],
  }),
];

const room: TeamsRoomResponse = { messages, skipped: [] };

function fact(over: Partial<TeamsFact> & { id: string }): TeamsFact {
  return {
    identity: "alice",
    document_id: over.id,
    ticket: "STUDIO-654",
    commit_sha: "",
    pr: "",
    run_id: "547",
    at: at(0, 1),
    state: "valid",
    reason: "",
    content: "",
    ...over,
  };
}

// Deliberately NOT in recency order: the preview shows the two most recent, so it has to sort.
const recall: TeamsRecallResponse = {
  identity: "alice",
  facts: [
    fact({ id: "20260901T105400Z-run-540", at: at(0, 8), run_id: "540", content: "A third fact the preview must not show." }),
    fact({ id: "20260901T191100Z-run-547", at: at(0, 1), commit_sha: "44d8675", content: "Grep DeepSeek after any config.go rebase." }),
    fact({ id: "20260901T165400Z-run-545", at: at(0, 2), run_id: "545", content: "The vision Router picks the model by input shape." }),
  ],
  skipped: [],
};

interface Fixtures {
  /** What GET /api/v1/teams/room answers with; a rejection stands in for the daemon refusing. */
  room?: TeamsRoomResponse;
  roomError?: Error;
}

function renderConsole(
  props: Partial<React.ComponentProps<typeof TeamsConsole>> = {},
  fixtures: Fixtures = {},
) {
  h.fetchTeamsOverview.mockResolvedValue(overview);
  if (fixtures.roomError) h.fetchTeamsRoom.mockRejectedValue(fixtures.roomError);
  else h.fetchTeamsRoom.mockResolvedValue(fixtures.room ?? room);
  h.fetchTeamsRecall.mockResolvedValue(recall);
  h.postTeamsRoom.mockResolvedValue({ id: `${day(0)}:5`, from: "operator", to: "*", at: at(0, 0), refs: [], delivered: 0 });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onNavigate = props.onNavigate ?? vi.fn();
  const view = render(
    <QueryClientProvider client={qc}>
      <TeamsConsole {...props} onNavigate={onNavigate} now={props.now ?? NOW} />
    </QueryClientProvider>,
  );
  return { ...view, onNavigate };
}

/** Every rendered room event, in DOM order. */
function events(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".event"));
}

async function ready() {
  await screen.findByText(/Someone want to review the export PR/);
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("3.1 — the now strip shows teammate states and the four stat pills", () => {
  it("renders every teammate's live state", async () => {
    renderConsole();
    await ready();
    const strip = document.querySelector(".now");
    expect(strip).toBeTruthy();
    const mates = within(strip as HTMLElement);
    expect(mates.getByText("alice")).toBeTruthy();
    // A teammate with a live run says what it is working; an idle one says idle.
    expect(mates.getByText("STUDIO-684")).toBeTruthy();
    expect(mates.getByText("idle")).toBeTruthy();
  });

  it("renders exactly the four stat pills §5 names, counted from the room window", async () => {
    renderConsole();
    await ready();
    const stats = document.querySelectorAll(".now .stat");
    expect(Array.from(stats).map((s) => s.querySelector(".l")?.textContent)).toEqual([
      "in review",
      "hand-offs",
      "assigned",
      "quorum ✕",
    ]);
    const n = (i: number) => stats[i].querySelector(".n")?.textContent;
    // Three hand-offs over three distinct tickets; three assignments; one quorum failure.
    expect([n(0), n(1), n(2), n(3)]).toEqual(["3", "3", "3", "1"]);
  });
});

describe("3.2 — events render typed by kind with the right rail, icon and label", () => {
  it("stamps each event with its kind and shows the kind label", async () => {
    renderConsole();
    await ready();
    const kinds = events().map((e) => e.dataset.kind);
    expect(new Set(kinds)).toEqual(new Set(["operator", "handoff", "quorum", "reconcile"]));
    const operator = events().find((e) => e.dataset.kind === "operator") as HTMLElement;
    expect(within(operator).getByText("you")).toBeTruthy();
    expect(within(operator).getByText("operator")).toBeTruthy();
    const quorum = events().find((e) => e.dataset.kind === "quorum") as HTMLElement;
    expect(within(quorum).getByText("quorum ✕")).toBeTruthy();
  });

  it("gives every event an icon", async () => {
    renderConsole();
    await ready();
    for (const event of events()) expect(event.querySelector("svg.ic")).toBeTruthy();
  });

  // Design §0.11.5: room content is untrusted and must render as quoted, attributed data — never
  // as text that could read as the app talking.
  it("renders each body quoted and attributed to its author", async () => {
    renderConsole();
    await ready();
    const body = screen.getByText(/Someone want to review the export PR/);
    expect(body.closest("blockquote")).toBeTruthy();
    expect(body.closest(".event")?.querySelector(".from")?.textContent).toBe("operator");
  });

  it("paints a rail per kind from the tokens, in the stylesheet", () => {
    const css = readFileSync(
      path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../theme/teams-console.css"),
      "utf8",
    );
    expect(css).toMatch(/\[data-kind="operator"\][^{]*\{[^}]*border-left-color:\s*var\(--operator\)/);
    expect(css).toMatch(/\[data-kind="handoff"\][^{]*\{[^}]*border-left-color:\s*var\(--handoff\)/);
    expect(css).toMatch(/\[data-kind="quorum"\][^{]*\{[^}]*border-left-color:\s*var\(--bad\)/);
  });
});

describe("3.3 — the filter chips show exactly the kinds §5 assigns them", () => {
  async function chip(name: string) {
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`^${name}`) }));
    await waitFor(() => expect(screen.getByRole("button", { name: new RegExp(`^${name}`) }).getAttribute("aria-pressed")).toBe("true"));
  }

  it("Conversation shows only operator and hand-off events", async () => {
    renderConsole();
    await ready();
    await chip("Conversation");
    expect(new Set(events().map((e) => e.dataset.kind))).toEqual(new Set(["operator", "handoff"]));
    expect(document.querySelector(".group")).toBeNull();
  });

  it("Assignments shows assign and reconcile", async () => {
    renderConsole();
    await ready();
    await chip("Assignments");
    const kinds = new Set(events().map((e) => e.dataset.kind));
    expect(kinds.has("reconcile")).toBe(true);
    expect(kinds.has("handoff")).toBe(false);
    expect(kinds.has("operator")).toBe(false);
  });

  it("Quorum shows quorum only", async () => {
    renderConsole();
    await ready();
    await chip("Quorum");
    expect(new Set(events().map((e) => e.dataset.kind))).toEqual(new Set(["quorum"]));
  });

  it("All shows everything again", async () => {
    renderConsole();
    await ready();
    await chip("Quorum");
    await chip("All");
    expect(new Set(events().map((e) => e.dataset.kind)).size).toBeGreaterThan(1);
  });
});

describe("3.4 — the teammate filter is a Select and scopes the feed", () => {
  it("is a select, not a chip per teammate", async () => {
    renderConsole();
    await ready();
    const select = screen.getByLabelText("Filter the room by teammate");
    expect(select.tagName).toBe("SELECT");
    expect(Array.from((select as HTMLSelectElement).options).map((o) => o.value)).toEqual([
      "all",
      "alice",
      "jimmy",
    ]);
  });

  it("scopes the feed to one teammate", async () => {
    renderConsole();
    await ready();
    fireEvent.change(screen.getByLabelText("Filter the room by teammate"), { target: { value: "alice" } });
    await waitFor(() => expect(screen.queryByText(/STUDIO-676 up for review/)).toBeNull());
    expect(screen.getByText(/EarsCycle.issues is one immutable fetch/)).toBeTruthy();
  });
});

describe("3.5 — a run of deterministic assignments collapses into one group", () => {
  it("renders the run as one expandable group, not as individual events", async () => {
    renderConsole();
    await ready();
    const group = document.querySelector("details.group") as HTMLDetailsElement;
    expect(group).toBeTruthy();
    expect(within(group).getByText(/3 tickets assigned deterministically/)).toBeTruthy();
    // Collapsed: no assignment renders as its own event row.
    expect(events().some((e) => e.dataset.kind === "assign")).toBe(false);
    // Expandable: the individual assignments are inside, one line each.
    expect(within(group).getAllByText(/STUDIO-(638|403|402)/)).toHaveLength(3);
  });
});

describe("3.6 — day dividers partition the feed, newest first", () => {
  it("labels today and orders the days newest first", async () => {
    renderConsole();
    await ready();
    const days = Array.from(document.querySelectorAll(".day .t")).map((d) => d.textContent);
    expect(days[0]).toMatch(/^Today · /);
    expect(days).toHaveLength(2);
  });

  it("orders events newest first within a day", async () => {
    renderConsole();
    await ready();
    const froms = events().map((e) => e.querySelector(".from")?.textContent);
    expect(froms.slice(0, 4)).toEqual(["@manager", "operator", "jimmy", "alice"]);
  });
});

describe("3.7 — a long body truncates with a working expand", () => {
  it("shows a head, keeps the tail behind a closed expand, and opens it on click", async () => {
    renderConsole();
    await ready();
    const expand = screen.getByText("show full note");
    const details = expand.closest("details") as HTMLDetailsElement;
    const quote = details.closest("blockquote") as HTMLElement;

    // The visible half is the text node before the <details>, and it stops well short of the tail.
    const head = quote.firstChild?.textContent ?? "";
    expect(head).toContain("review round 2 addressed");
    expect(head).not.toContain("PassWrites");
    expect(head.length).toBeLessThan(LONG_BODY.length);
    expect(details.open).toBe(false);

    fireEvent.click(expand);
    expect(details.open).toBe(true);
    expect(within(details).getByText(/PassWrites/)).toBeTruthy();
    // Nothing is lost in the split: head + tail is the body the daemon served.
    expect(`${head} ${within(details).getByText(/PassWrites/).textContent}`).toBe(LONG_BODY);
  });
});

describe("3.8 — 'Load older' asks the daemon for the previous day's page", () => {
  it("opens on the newest days, then widens the window and reveals the day before", async () => {
    renderConsole();
    await ready();
    expect(h.fetchTeamsRoom).toHaveBeenCalledWith(DEFAULT_ROOM_WINDOW);
    expect(document.querySelectorAll(".day")).toHaveLength(2);

    fireEvent.click(screen.getByRole("button", { name: /Load older/ }));

    await waitFor(() => expect(document.querySelectorAll(".day")).toHaveLength(3));
    expect(h.fetchTeamsRoom).toHaveBeenCalledWith(2 * DEFAULT_ROOM_WINDOW);
    expect(screen.getByText("Two days ago, from jimmy.")).toBeTruthy();
  });

  // A wider window is a different query key, so without the room query keeping its previous page
  // the feed would blank to "Loading the room…" every time the operator asked for one more day.
  it("keeps the feed on screen while the wider read is in flight, and spins the pager", async () => {
    renderConsole();
    await ready();
    // The widened read never settles, so the in-flight state is what the assertions see.
    h.fetchTeamsRoom.mockReturnValue(new Promise(() => {}));

    fireEvent.click(screen.getByRole("button", { name: /Load older/ }));

    await waitFor(() => expect(document.querySelector(".older .sp")).toBeTruthy());
    expect(screen.getByText(/Someone want to review the export PR/)).toBeTruthy();
    expect(screen.queryByText(/Loading the room/)).toBeNull();
  });
});

describe("3.9 — the composer posts as the operator, with refs", () => {
  it("sends the body and the refs it was given, and clears on success", async () => {
    renderConsole();
    await ready();
    const body = screen.getByLabelText("Post to the team room");
    const refs = screen.getByLabelText("Refs");
    fireEvent.change(body, { target: { value: "Someone review the export PR" } });
    fireEvent.change(refs, { target: { value: "STUDIO-498, https://github.com/x/y/pull/9" } });
    fireEvent.click(screen.getByRole("button", { name: "Post as operator" }));

    await waitFor(() =>
      expect(h.postTeamsRoom).toHaveBeenCalledWith("Someone review the export PR", [
        "STUDIO-498",
        "https://github.com/x/y/pull/9",
      ]),
    );
    await waitFor(() => expect((body as HTMLTextAreaElement).value).toBe(""));
  });

  it("will not post an empty body", async () => {
    renderConsole();
    await ready();
    const post = screen.getByRole("button", { name: "Post as operator" });
    expect((post as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(post);
    expect(h.postTeamsRoom).not.toHaveBeenCalled();
  });
});

describe("3.10 — the side cards route to manage and memory", () => {
  it("routes the roster card's link to manage", async () => {
    const { onNavigate } = renderConsole();
    await ready();
    fireEvent.click(screen.getByRole("button", { name: /Manage team/ }));
    expect(onNavigate).toHaveBeenCalledWith("manage");
  });

  it("routes the memory card's link to memory", async () => {
    const { onNavigate } = renderConsole();
    await ready();
    fireEvent.click(screen.getByRole("button", { name: /Open memory/ }));
    expect(onNavigate).toHaveBeenCalledWith("memory");
  });

  it("previews the two most recent facts and no more", async () => {
    renderConsole();
    await ready();
    expect(await screen.findByText(/Grep DeepSeek after any config.go rebase/)).toBeTruthy();
    expect(screen.getByText(/The vision Router picks the model/)).toBeTruthy();
    expect(screen.queryByText(/A third fact the preview must not show/)).toBeNull();
  });

  it("lists the roster with each teammate's profile and bank", async () => {
    renderConsole();
    await ready();
    const roster = screen.getByRole("table", { name: "Roster" });
    expect(within(roster).getByText("agent-alice")).toBeTruthy();
    expect(within(roster).getAllByText("swe")).toHaveLength(2);
  });
});

describe("the console degrades rather than breaking", () => {
  it("says the room is empty rather than rendering a bare feed", async () => {
    renderConsole({}, { room: { messages: [], skipped: [] } });
    expect(await screen.findByText(/Nothing has been posted yet/)).toBeTruthy();
  });

  it("surfaces the daemon's own complaint when the room cannot be read", async () => {
    renderConsole({}, { roomError: new Error("teams_disabled") });
    expect(await screen.findByText(/teams_disabled/)).toBeTruthy();
  });
});
