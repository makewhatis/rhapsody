// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { readFileSync } from "node:fs";
import path from "node:path";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { LogEntry, RunDetail, RunMessage, RunSummary, StateResponse } from "@/lib/api";
import { MEMORY_EMPTY_NOTE, ROOM_WATCH_WINDOW } from "@/lib/console-watch";

// STUDIO-742 — the "Trace" run detail's three zones (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §3), replacing STUDIO-683's summary strip and
// flat runs list — plus the states of slice 3 (STUDIO-744) and the watch-tabs rail of slice 4
// (STUDIO-745), which is where §4's side cards moved: Diff / Review / Room / Memory / Messages,
// under the inspector, with an "Ask about this run" dock beneath the split.

const h = vi.hoisted(() => ({
  fetchIssueHistory: vi.fn(),
  fetchRunDetail: vi.fn(),
  fetchRunTranscript: vi.fn(),
  sendRunMessage: vi.fn(),
  fetchRunMessages: vi.fn(),
  fetchState: vi.fn(),
  fetchReviews: vi.fn(),
  postTeamsRoom: vi.fn(),
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
  fetchLinearIdentity: vi.fn(),
  stopRun: vi.fn(),
  resumeRun: vi.fn(),
  fetchVersion: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchIssueHistory: h.fetchIssueHistory,
    fetchRunDetail: h.fetchRunDetail,
    fetchRunTranscript: h.fetchRunTranscript,
    sendRunMessage: h.sendRunMessage,
    fetchRunMessages: h.fetchRunMessages,
    fetchState: h.fetchState,
    fetchReviews: h.fetchReviews,
    postTeamsRoom: h.postTeamsRoom,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchTeamsRoom: h.fetchTeamsRoom,
    fetchTeamsRecall: h.fetchTeamsRecall,
    fetchLinearIdentity: h.fetchLinearIdentity,
    stopRun: h.stopRun,
    resumeRun: h.resumeRun,
    fetchVersion: h.fetchVersion,
  };
});

const { JobDetailView } = await import("./JobDetailView");

const EMPTY_STATE: StateResponse = {
  status: "ok",
  poll_interval_ms: 2000,
  running: [],
  retrying: [],
  codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
  rate_limits: [],
  blocked: [],
};

/**
 * A run row shaped like the ones the daemon actually writes.
 *
 * `branch` is EMPTY and `attempt` is 0 deliberately, not for brevity: `persist_start_run` is the
 * only writer of `runs.branch` and leaves it at its default, and `attempt` only increments on the
 * retry path — 441 of 441 recorded rows carry no branch and 432 of them are attempt 0. A fixture
 * with both fields populated is what let a dead "View PR" and an undifferentiated attempt selector
 * pass a green suite. A test about a row that DOES carry them overrides them explicitly.
 */
function run(over: Partial<RunSummary> & Pick<RunSummary, "id">): RunSummary {
  return {
    issue_id: "i",
    issue_identifier: "STUDIO-654",
    title: "Attach a photo in chat",
    attempt: 0,
    session_uuid: "s",
    branch: "",
    project_slug: "tally",
    repo: "git@github.com:makewhatis/rhapsody.git",
    started_at: "2026-09-01T19:11:00Z",
    ended_at: "2026-09-01T19:15:00Z",
    outcome: "completed",
    turns: 1,
    input_tokens: 10,
    output_tokens: 20,
    total_tokens: 38_000,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  } as RunSummary;
}

function entry(over: Partial<LogEntry> & Pick<LogEntry, "seq">): LogEntry {
  return { kind: "text", tool: "", text: "", ...over };
}

/**
 * A completed run's transcript: it orients, edits, verifies (failing), coordinates, and closes on
 * a sectioned hand-off summary — one phase of every kind the spine can show.
 */
const COMPLETED: LogEntry[] = [
  entry({ seq: 1, kind: "event", text: "session started" }),
  entry({ seq: 2, kind: "thinking", text: "I should read the **api** module first." }),
  entry({ seq: 3, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 4, kind: "tool_result", text: "export interface RunSummary {" }),
  entry({ seq: 5, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 6, kind: "tool_result", text: "The file has been updated." }),
  entry({ seq: 7, kind: "tool_use", tool: "Bash", text: "command=npm test" }),
  entry({ seq: 8, kind: "tool_result", text: "Error: 1 test failed in api.test.ts" }),
  entry({ seq: 9, kind: "tool_use", tool: "mcp__symphony__teams_post", text: "body=handed off" }),
  entry({ seq: 10, kind: "tool_result", text: "posted" }),
  entry({
    seq: 11,
    kind: "text",
    text: [
      "Photo attachment shipped.",
      "",
      "## What changed",
      "",
      "Added **thumbnails** to the composer.",
      "",
      "## Verification",
      "",
      "```sh",
      "cargo test --workspace",
      "```",
      "",
      "## Follow-ups",
      "",
      "- HEIC is still unsupported",
    ].join("\n"),
  }),
  entry({ seq: 12, kind: "event", text: "turn completed" }),
];

/**
 * The `GET /api/v1/runs/{id}` payload for a run row — the 2s poll the live header reads its
 * telemetry from (STUDIO-744). Derived from the row so the default poll agrees with the history
 * it decorates; a test about the poll DISAGREEING with the row overrides it.
 */
function detailOf(row: RunSummary, over: Partial<RunDetail> = {}): RunDetail {
  return {
    run_id: row.id,
    issue_id: row.issue_id,
    issue_identifier: row.issue_identifier,
    title: row.title,
    project: row.project_slug,
    repo: row.repo,
    attempt: row.attempt,
    outcome: row.outcome,
    live: row.outcome === "running",
    issue_state: "",
    last_codex_event: "",
    turn_count: row.turns,
    input_tokens: row.input_tokens,
    output_tokens: row.output_tokens,
    total_tokens: row.total_tokens,
    usage_estimated: row.usage_estimated,
    started_at: row.started_at,
    ended_at: row.ended_at,
    last_event_at: "",
    error: row.error,
    recent_events: [],
    generated_at: "",
    ...over,
  };
}

/** One roster row, as `/api/v1/teams` serves it. */
function teammate(name: string) {
  return { name, profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: ["STUDIO-654"] };
}

/** The client the last mount rendered under — how a test simulates a poll tick landing. */
let client: QueryClient;

function mountDetail(runs: RunSummary[], onNavigate = vi.fn()) {
  h.fetchIssueHistory.mockResolvedValue({ issue_identifier: "STUDIO-654", runs });
  // A test that cares what the poll says configures it BEFORE mounting; this is only the default.
  if (h.fetchRunDetail.getMockImplementation() === undefined) {
    h.fetchRunDetail.mockImplementation(async (id: number) => {
      const row = runs.find((r) => r.id === id);
      if (row === undefined) throw new Error(`no run with id: ${id}`);
      return detailOf(row);
    });
  }
  h.fetchState.mockResolvedValue(EMPTY_STATE);
  if (h.fetchTeamsOverview.getMockImplementation() === undefined) {
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [teammate("alice")],
    });
  }
  h.fetchTeamsRoom.mockResolvedValue({ messages: [], skipped: [] });
  h.fetchTeamsRecall.mockResolvedValue({ identity: "alice", facts: [], skipped: [] });
  // Configured BEFORE mounting by a test that cares what they say; these are only the defaults.
  if (h.fetchRunMessages.getMockImplementation() === undefined) {
    h.fetchRunMessages.mockResolvedValue([]);
  }
  if (h.fetchReviews.getMockImplementation() === undefined) {
    h.fetchReviews.mockResolvedValue({ enabled: true, reviews: [] });
  }
  // A test about a Teams-OFF daemon configures this before mounting; this is only the default.
  if (h.fetchVersion.getMockImplementation() === undefined) {
    h.fetchVersion.mockResolvedValue({
      version: "v0.4.0",
      commit: "abc",
      built_at: "",
      teams_enabled: true,
    });
  }
  h.fetchLinearIdentity.mockResolvedValue({
    connected: true,
    name: "d",
    display_name: "d",
    email: "d@example.com",
    token: "",
    workspace_url_key: "studio49",
  });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client = qc;
  render(
    <QueryClientProvider client={qc}>
      <JobDetailView issue="STUDIO-654" onNavigate={onNavigate} />
    </QueryClientProvider>,
  );
  return onNavigate;
}

/** Waits out the transcript fetch: the zones mount before it resolves. */
async function settleTrace() {
  await waitFor(() => expect(document.querySelector(".trsplit, .trraw")).toBeTruthy());
  await waitFor(() =>
    expect(document.querySelector(".trspine, .trraw")?.textContent).not.toContain(
      "Loading transcript",
    ),
  );
}

/** The spine's visible phase rows, by title. */
function spineTitles(): string[] {
  return [...document.querySelectorAll(".trstep .stt")].map((el) => el.textContent ?? "");
}

/** An action in the header cluster, by its accessible name. */
function action(name: string | RegExp): HTMLElement {
  return within(document.querySelector(".trhd .acts") as HTMLElement).getByRole(
    /view pr|open ticket/i.test(String(name)) ? "link" : "button",
    { name },
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  // `clearAllMocks` clears CALLS, not implementations, and `mountDetail`'s run-detail default is
  // installed only when there is none — so without this a test that configured the poll would
  // hand its answer to every test after it.
  h.fetchRunDetail.mockReset();
  // Same reason: `clearAllMocks` clears calls, not implementations, so a Teams-off version answer,
  // a review watch set or a message timeline would otherwise be handed to every test after it.
  h.fetchVersion.mockReset();
  h.fetchRunMessages.mockReset();
  h.fetchReviews.mockReset();
  h.fetchTeamsOverview.mockReset();
  // The page geometry is defined on the live document, which outlives a render.
  const el = scroller() as unknown as Record<string, unknown>;
  delete el.scrollHeight;
  delete el.clientHeight;
  scroller().scrollTop = 0;
});

// ---------------------------------------------------------------------------------------------
// Acceptance 1 — a completed run renders header + Result card + The Split, from the slice-1 model.
// ---------------------------------------------------------------------------------------------
describe("zone A — the sticky header (§3A)", () => {
  it("carries the key, the title, the outcome and the assignee", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelector(".trhd")).toBeTruthy());
    const hd = document.querySelector(".trhd") as HTMLElement;
    expect(hd.querySelector(".k")?.textContent).toBe("STUDIO-654");
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Attach a photo in chat");
    expect(hd.querySelector(".pill")?.textContent).toContain("completed");
    await waitFor(() => expect(hd.querySelector(".who2")?.textContent).toContain("alice"));
  });

  // Acceptance — "Vitals derive from RunSummary".
  it("derives every vital from the run row: duration, turns, tokens, branch", async () => {
    mountDetail([
      run({
        id: 547,
        turns: 3,
        branch: "symphony/STUDIO-654",
        started_at: "2026-09-01T19:11:00Z",
        ended_at: "2026-09-01T19:15:30Z",
      }),
    ]);
    await waitFor(() => expect(document.querySelector(".trvitals")).toBeTruthy());
    const vitals = document.querySelector(".trvitals")?.textContent ?? "";
    expect(vitals).toContain("4m 30s");
    expect(vitals).toContain("3 turns");
    expect(vitals).toContain("38.0k");
    expect(vitals).toContain("symphony/STUDIO-654");
  });

  // Every real row leaves `branch` empty, so reading it alone made this vital a permanent dash.
  it("names the ticket's own branch on a row the daemon left branchless — the real shape", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() =>
      expect(document.querySelector(".trvitals")?.textContent).toContain("symphony/STUDIO-654"),
    );
  });

  it("marks an estimated token total rather than presenting it as authoritative", async () => {
    mountDetail([run({ id: 547, usage_estimated: true })]);
    await waitFor(() => expect(document.querySelector(".trvitals")?.textContent).toContain("~38.0k"));
  });

  it("navigates back to Jobs from the breadcrumb and the back control", async () => {
    const onNavigate = mountDetail([run({ id: 1 })]);
    fireEvent.click(await screen.findByText("Jobs"));
    expect(onNavigate).toHaveBeenCalledExactlyOnceWith("jobs");
  });

  // The daemon increments `attempt` only on the retry path, so a re-summoned ticket records every
  // one of its runs as attempt 0 — 432 of 441 real rows. Labelling by attempt gave a five-run
  // ticket five identical buttons; the run id is the daemon's own handle and is always distinct.
  it("tells the attempts apart by run id, on rows that all record the same attempt", async () => {
    mountDetail([
      run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
      run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 545, started_at: "2026-09-01T16:54:00Z" }),
    ]);
    await waitFor(() => expect(document.querySelectorAll(".trattempts button")).toHaveLength(3));
    expect([...document.querySelectorAll(".trattempts button")].map((b) => b.textContent)).toEqual([
      "run 547",
      "run 545",
      "run 522",
    ]);
    // The attempt and the start time are real data too — they ride along in the tooltip.
    expect(document.querySelector(".trattempts button span")?.getAttribute("title")).toContain(
      "attempt 0 · started ",
    );
    // Newest first AND newest selected — its transcript is the one fetched.
    expect(document.querySelector('.trattempts button[aria-pressed="true"]')?.textContent).toBe("run 547");
    await waitFor(() => expect(h.fetchRunTranscript).toHaveBeenCalledExactlyOnceWith(547));
  });

  it("renders one attempt's trace at a time, fetching only that attempt's transcript", async () => {
    h.fetchRunTranscript.mockImplementation(async (id: number) => ({
      run_id: id,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "tool_use", tool: "Bash", text: `command=echo run ${id}` })],
    }));
    mountDetail([
      run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
    ]);
    await waitFor(() => expect(screen.getByText(/echo run 547/)).toBeTruthy());
    expect(screen.queryByText(/echo run 522/)).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "run 522" }));
    await waitFor(() => expect(screen.getByText(/echo run 522/)).toBeTruthy());
    expect(h.fetchRunTranscript).toHaveBeenCalledWith(522);
    expect(screen.queryByText(/echo run 547/)).toBeNull();
  });

  // The stylesheet's offset is a fallback; the real one is measured, because the header's own
  // height changes when its cluster wraps. jsdom ships no `ResizeObserver`, so the hook must also
  // survive its absence — which is the state every other test in this file exercises.
  it("publishes the header's measured height for the spine to stick below", async () => {
    const observers: (() => void)[] = [];
    class FakeResizeObserver {
      constructor(private readonly cb: () => void) {}
      observe() {
        observers.push(this.cb);
      }
      disconnect() {}
      unobserve() {}
    }
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);
    try {
      mountDetail([run({ id: 547 })]);
      await waitFor(() => expect(document.querySelector(".trhd")).toBeTruthy());
      const header = document.querySelector(".trhd") as HTMLElement;
      Object.defineProperty(header, "offsetHeight", { value: 96, configurable: true });
      observers.forEach((fire) => fire());
      await waitFor(() =>
        expect((document.querySelector(".trrun") as HTMLElement).style.getPropertyValue("--trhd-h")).toBe("96px"),
      );
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("survives a ticket with no recorded runs", async () => {
    mountDetail([]);
    await waitFor(() => expect(screen.getByText("This ticket has no recorded runs.")).toBeTruthy());
    expect(document.querySelector(".trsplit")).toBeNull();
  });
});

describe("zone A — the header's actions are real or dependency-named, never fake", () => {
  it("links Open ticket at the connected workspace's own deep link", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(action(/open ticket/i)).toBeTruthy());
    expect(action(/open ticket/i).getAttribute("href")).toBe(
      "https://linear.app/studio49/issue/STUDIO-654",
    );
  });

  // Acceptance — "View PR / Merge render as dependency-named (not dead), never fake."
  // The row here is the real shape — `branch: ""` — so this is the ONLY path that ever fires in
  // production. Searching the row's own branch was dead code: no row has ever carried one.
  it("resolves View PR through a head-branch search, never a fabricated PR number", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(action(/view pr/i)).toBeTruthy());
    const href = action(/view pr/i).getAttribute("href") ?? "";
    expect(href).toBe(
      "https://github.com/makewhatis/rhapsody/pulls?q=is%3Apr%20head%3Asymphony%2FSTUDIO-654",
    );
    expect(href).not.toMatch(/\/pull\/\d/);
  });

  it("names its dependency, rather than linking, when the remote is not a GitHub one", async () => {
    mountDetail([run({ id: 547, repo: "git@gitlab.example:acme/app.git" })]);
    await waitFor(() => expect(document.querySelector(".trhd .acts")).toBeTruthy());
    const acts = document.querySelector(".trhd .acts") as HTMLElement;
    expect(within(acts).queryByRole("link", { name: /view pr/i })).toBeNull();
    const dep = within(acts).getByRole("button", { name: /view pr/i });
    expect(dep.querySelector(".dep")?.textContent).toBe("dep");
    // The tooltip must name what is ACTUALLY missing. Blaming the remote on a run whose remote is
    // plainly github.com sends the operator to check the wrong thing.
    expect(dep.getAttribute("title")).toMatch(/no.*pull-request endpoint/i);
    expect(dep.getAttribute("title")).toMatch(/not on github\.com/i);
  });

  it("names Merge's missing endpoint instead of offering a button that cannot merge", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(action(/^merge/i)).toBeTruthy());
    const merge = action(/^merge/i);
    expect(merge.querySelector(".dep")?.textContent).toBe("dep");
    expect(merge.getAttribute("title")).toMatch(/run-branch diff/i);
    // Inert, but NOT the `disabled` attribute: a disabled button fires no mouse events, so the
    // tooltip that names the dependency would never open — the control would be dead, not named.
    expect(merge.getAttribute("aria-disabled")).toBe("true");
    expect((merge as HTMLButtonElement).disabled).toBe(false);
    expect(merge.getAttribute("href")).toBeNull();
  });

  it("offers Stop only while the run is live, and Resume only once it has stopped", async () => {
    mountDetail([run({ id: 547, outcome: "stopped", ended_at: "2026-09-01T19:15:00Z" })]);
    await waitFor(() => expect(action(/resume/i)).toBeTruthy());
    expect((action(/resume/i) as HTMLButtonElement).disabled).toBe(false);
    expect(within(document.querySelector(".trhd .acts") as HTMLElement).queryByRole("button", { name: /^stop$/i })).toBeNull();
  });

  it("surfaces a failed Stop instead of swallowing it — the console has no toast", async () => {
    h.stopRun.mockRejectedValue(new Error("agent already exited"));
    mountDetail([run({ id: 547, outcome: "running", ended_at: "" })]);
    await waitFor(() => expect(action(/^stop$/i)).toBeTruthy());
    fireEvent.click(action(/^stop$/i));
    await waitFor(() =>
      expect(document.querySelector(".trhd .acterr")?.textContent).toBe("agent already exited"),
    );
  });

  it("surfaces a Stop that killed the agent but could not move the ticket", async () => {
    h.stopRun.mockResolvedValue({ identifier: "STUDIO-654", move_error: "Backlog state missing" });
    mountDetail([run({ id: 547, outcome: "running", ended_at: "" })]);
    await waitFor(() => expect(action(/^stop$/i)).toBeTruthy());
    fireEvent.click(action(/^stop$/i));
    await waitFor(() =>
      expect(document.querySelector(".trhd .acterr")?.textContent).toBe("Backlog state missing"),
    );
  });

  it("stops a running run through the daemon's own endpoint", async () => {
    h.stopRun.mockResolvedValue({ identifier: "STUDIO-654", moved_to: "Backlog" });
    mountDetail([run({ id: 547, outcome: "running", ended_at: "" })]);
    await waitFor(() => expect(action(/^stop$/i)).toBeTruthy());
    fireEvent.click(action(/^stop$/i));
    await waitFor(() => expect(h.stopRun).toHaveBeenCalledExactlyOnceWith(547));
  });
});

describe("zone B — the Result card (§3B)", () => {
  it("leads with the verb-phrase headline and the eyebrow for how the run ended", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    expect(document.querySelector(".trrc h2")?.textContent).toBe("Photo attachment shipped.");
    // The run called no handoff tool, so the card says "done" and does not claim a hand-off.
    expect(document.querySelector(".trrc .eyebrow")?.textContent).toBe("done");
  });

  it("says a run handed off only when it actually called the handoff tool", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [
        ...COMPLETED,
        entry({ seq: 13, kind: "tool_use", tool: "mcp__symphony__symphony_handoff", text: "{}" }),
      ],
    });
    mountDetail([run({ id: 547 })]);
    await waitFor(() =>
      expect(document.querySelector(".trrc .eyebrow")?.textContent).toBe("done · handed off"),
    );
  });

  // Acceptance — "the handoff body rendered as sanitized markdown … in labeled sub-blocks".
  it("renders the hand-off body as markdown, in the model's labelled sub-blocks", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelectorAll(".trrc .sect")).toHaveLength(3));
    const rc = document.querySelector(".trrc") as HTMLElement;

    // The model's label, and beside it the author's own heading, kept verbatim.
    expect([...rc.querySelectorAll(".sect .lab")].map((el) => el.textContent)).toEqual([
      "What changed",
      "How verified",
      "Follow-ups",
    ]);
    expect([...rc.querySelectorAll(".sect .head")].map((el) => el.textContent)).toEqual([
      "What changed",
      "Verification",
      "Follow-ups",
    ]);

    // Markdown, not its syntax: bold renders, the fence renders as a scrollable code box.
    expect(rc.querySelector(".sect strong")?.textContent).toBe("thumbnails");
    expect(rc.querySelector("pre.mdpre")?.textContent).toBe("cargo test --workspace");
    expect(rc.querySelector(".sect li")?.textContent).toBe("HEIC is still unsupported");
    expect(rc.textContent).not.toContain("**");
  });

  it("renders an injection attempt in the hand-off body as text", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [
        entry({
          seq: 1,
          kind: "text",
          text: '<script>window.__pwned = 1;</script><img src=x onerror="window.__pwned = 1">',
        }),
      ],
    });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    const rc = document.querySelector(".trrc") as HTMLElement;
    expect(rc.querySelector("script")).toBeNull();
    expect(rc.querySelector("img")).toBeNull();
    expect((window as unknown as Record<string, unknown>).__pwned).toBeUndefined();
    expect(rc.textContent).toContain("<script>");
  });

  it("carries a receipt of the same vitals, plus the run's tool count", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelector(".trreceipt")).toBeTruthy());
    const receipt = document.querySelector(".trreceipt")?.textContent ?? "";
    expect(receipt).toContain("4m 0s");
    expect(receipt).toContain("38.0k");
    expect(receipt).toContain("tools");
    expect(receipt).toContain("4"); // Read, Edit, Bash, teams_post
  });

  // §3B — "Failed -> red banner + error; Stopped -> amber reason + Resume."
  // The fixture is the one the old suite lacked: a run that HANDED OFF and only then died. Its
  // prose headline is intact, so a card that surfaces the error only when there is no prose drops
  // it exactly here — on the runs an operator opens this view to understand.
  it("shows a failed run's error even when it wrote a full hand-off first", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([
      run({ id: 547, outcome: "failed", error: "agent exited 1: turn timeout after 900s" }),
    ]);
    await settleTrace();
    const rc = document.querySelector(".trrc") as HTMLElement;
    // The hand-off is still the headline — the run did write one.
    expect(rc.querySelector("h2")?.textContent).toBe("Photo attachment shipped.");
    const banner = rc.querySelector(".trbanner") as HTMLElement;
    expect(banner.className).toContain("fail");
    expect(banner.querySelector("b")?.textContent).toBe("Error");
    expect(banner.textContent).toContain("agent exited 1: turn timeout after 900s");
  });

  it("shows a stopped run's reason as an amber banner, beside the Resume that acts on it", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547, outcome: "stopped", error: "operator stopped the run" })]);
    await settleTrace();
    const banner = document.querySelector(".trrc .trbanner") as HTMLElement;
    expect(banner.className).toContain("stop");
    expect(banner.querySelector("b")?.textContent).toBe("Reason");
    expect(banner.textContent).toContain("operator stopped the run");
    expect(action(/resume/i)).toBeTruthy();
  });

  it("shows a failed run's error when it wrote no prose at all, without repeating it", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "tool_use", tool: "Bash", text: "command=npm test" })],
    });
    mountDetail([run({ id: 547, outcome: "failed", error: "agent exited 1" })]);
    await settleTrace();
    const rc = document.querySelector(".trrc") as HTMLElement;
    expect(rc.querySelector(".trbanner")?.textContent).toContain("agent exited 1");
    // Once, not twice: the headline names the ending and leaves the string to the banner.
    expect(rc.querySelector("h2")?.textContent).toBe("The run failed before handing off.");
  });

  it("shows no banner at all for a run that recorded no error", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    expect(document.querySelector(".trrc .trbanner")).toBeNull();
  });

  // §2 puts the answer in this zone — "most runs are understood here in ~15s". Asserting the
  // wrong answer for the first frame and swapping it is worse than admitting it is not known.
  it("states nothing about the outcome until the transcript it reads has arrived", async () => {
    let release = (): void => {};
    h.fetchRunTranscript.mockReturnValue(
      new Promise((resolve) => {
        release = () => resolve({ run_id: 547, generated_at: "", entries: COMPLETED });
      }),
    );
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelector(".trrc")).toBeTruthy());
    const rc = document.querySelector(".trrc") as HTMLElement;
    expect(rc.querySelector("h2")).toBeNull();
    expect(rc.textContent).not.toContain("Completed without a written hand-off");
    // A live region announces its CONTENT, so the placeholder carries text — an empty box with an
    // aria-label announces nothing on most screen readers.
    expect(rc.querySelector(".trskel")?.getAttribute("role")).toBe("status");
    expect(rc.querySelector(".trskel")?.textContent).toBe("Loading transcript…");
    // Nor a tool count of 0, which would read as "this run called no tools".
    expect(rc.querySelector(".trreceipt")?.textContent).not.toContain("tools0");
    // The eyebrow and the vitals come off the RUN ROW, so they are known already.
    expect(rc.querySelector(".eyebrow")?.textContent).toBe("done");

    release();
    await waitFor(() =>
      expect(document.querySelector(".trrc h2")?.textContent).toBe("Photo attachment shipped."),
    );
    expect(document.querySelector(".trrc .trskel")).toBeNull();
  });

  it("synthesizes a headline rather than showing an empty card when a run wrote no prose", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "tool_use", tool: "Bash", text: "command=npm test" })],
    });
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelector(".trrc h2")?.textContent).toBeTruthy());
    expect(document.querySelector(".trrc .sect")).toBeNull();
  });
});

describe("zone C — The Split (§3C)", () => {
  async function mountSplit(entries: LogEntry[] = COMPLETED) {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
  }

  it("builds the spine from the slice-1 phases, with glyph, title, subtitle and side effects", async () => {
    await mountSplit();
    expect(spineTitles()).toEqual(["Oriented", "Implemented", "Verified", "Coordinated"]);
    const steps = [...document.querySelectorAll(".trstep")];
    expect(steps[0].querySelector(".g")?.textContent).toBe("◎");
    expect(steps[0].querySelector(".ssub")?.textContent).toBe("read 1 file");
    expect(steps[1].querySelector(".fx")?.textContent).toContain("edited 1 file");
    expect(steps[3].querySelector(".fx")?.textContent).toContain("posted to room");
    // The failing `npm test` marks its phase, and only that phase.
    expect(steps.filter((s) => s.classList.contains("err"))).toHaveLength(1);
    expect(steps[2].classList.contains("err")).toBe(true);
  });

  // Acceptance — "selecting a phase shows its DID call-cards … and its muted/collapsed SAID prose".
  it("opens the first phase by default and switches the inspector when a step is clicked", async () => {
    await mountSplit();
    expect(document.querySelector('.trstep[aria-pressed="true"] .stt')?.textContent).toBe("Oriented");
    expect(document.querySelector(".trinsp h4")?.textContent).toBe("Oriented — what alice did");
    expect(document.querySelector(".trinsp .trcard .tool")?.textContent).toBe("Read");

    fireEvent.click(screen.getByRole("button", { name: /Implemented/ }));
    await waitFor(() =>
      expect(document.querySelector(".trinsp h4")?.textContent).toBe("Implemented — what alice did"),
    );
    expect(document.querySelector(".trinsp .trcard .tool")?.textContent).toBe("Edit");
    expect(document.querySelector('.trstep[aria-pressed="true"] .stt')?.textContent).toBe("Implemented");
  });

  it("folds each call to a one-liner and expands it to the tool's own result", async () => {
    await mountSplit();
    const card = document.querySelector(".trinsp .trcard") as HTMLElement;
    expect(card.querySelector(".tgt")?.textContent).toBe("/repo/src/lib/api.ts");
    // Collapsed: the result is not in the document at all, not merely hidden by CSS.
    expect(card.querySelector("pre")).toBeNull();
    expect(card.querySelector("[aria-expanded]")?.getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(card.querySelector("[aria-expanded]") as HTMLElement);
    await waitFor(() => expect(card.querySelector("pre")?.textContent).toBe("export interface RunSummary {"));
    expect(card.querySelector("[aria-expanded]")?.getAttribute("aria-expanded")).toBe("true");
  });

  it("auto-expands a failing call and tints it, so a failure is never one click away", async () => {
    await mountSplit();
    fireEvent.click(screen.getByRole("button", { name: /Verified/ }));
    await waitFor(() => expect(document.querySelector(".trinsp .trcard.open")).toBeTruthy());
    const card = document.querySelector(".trinsp .trcard") as HTMLElement;
    expect(card.querySelector(".res")?.classList.contains("bad")).toBe(true);
    expect(card.querySelector("pre")?.textContent).toContain("Error: 1 test failed");
  });

  it("renders SAID as muted markdown, collapsed to its lead, with thinking behind `reasoning`", async () => {
    await mountSplit();
    const said = document.querySelector(".trinsp .trsaid") as HTMLElement;
    expect(said.querySelector(".lab")?.textContent).toBe("what alice said");
    // The prose is thinking, so it sits behind the reasoning disclosure rather than in the open.
    const reasoning = within(said).getByRole("button", { name: /reasoning/i });
    expect(reasoning.getAttribute("aria-expanded")).toBe("false");
    expect(said.querySelector(".think .md")).toBeNull();
    fireEvent.click(reasoning);
    await waitFor(() => expect(said.querySelector(".think strong")?.textContent).toBe("api"));
  });

  it("collapses a long text block to its lead paragraph, expandable in place", async () => {
    await mountSplit([
      entry({ seq: 1, kind: "text", text: "Lead paragraph.\n\nSecond paragraph.\n\nThird." }),
      entry({ seq: 2, kind: "tool_use", tool: "Read", text: "file_path=/a.ts" }),
    ]);
    const said = document.querySelector(".trinsp .trsaid") as HTMLElement;
    expect(said.querySelector(".prose")?.textContent).toBe("Lead paragraph.");
    fireEvent.click(within(said).getByRole("button", { name: /show more/i }));
    await waitFor(() =>
      expect(said.querySelector(".prose")?.textContent).toBe("Lead paragraph.Second paragraph.Third."),
    );
  });

  it("offers no Show more when the whole prose IS the lead, trailing whitespace and all", async () => {
    await mountSplit([
      entry({ seq: 1, kind: "text", text: "One paragraph, and nothing after it.\n\n  \n" }),
      entry({ seq: 2, kind: "tool_use", tool: "Read", text: "file_path=/a.ts" }),
    ]);
    const said = document.querySelector(".trinsp .trsaid") as HTMLElement;
    expect(said.querySelector(".prose")?.textContent).toBe("One paragraph, and nothing after it.");
    expect(within(said).queryByRole("button", { name: /show more/i })).toBeNull();
  });

  it("says so when the selected phase made no tool calls at all", async () => {
    await mountSplit([entry({ seq: 1, kind: "text", text: "Only prose here." })]);
    expect(document.querySelector(".trinsp .empty")?.textContent).toBe("No tool calls in this step.");
  });

  it("says so when a run recorded no transcript", async () => {
    await mountSplit([]);
    expect(screen.getByText("No transcript recorded for this run.")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------------------------
// Acceptance 3 — the filter narrows the spine (All / Edits / Bash / Errors / text).
// ---------------------------------------------------------------------------------------------
describe("zone C — the spine's filter (§3C)", () => {
  async function mountSplit() {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
  }

  it("narrows to edits, to bash, to errors, and back to all", async () => {
    await mountSplit();
    expect(spineTitles()).toHaveLength(4);

    fireEvent.click(screen.getByRole("button", { name: "Edits" }));
    await waitFor(() => expect(spineTitles()).toEqual(["Implemented"]));

    fireEvent.click(screen.getByRole("button", { name: "Bash" }));
    await waitFor(() => expect(spineTitles()).toEqual(["Verified"]));

    fireEvent.click(screen.getByRole("button", { name: "Errors" }));
    await waitFor(() => expect(spineTitles()).toEqual(["Verified"]));

    fireEvent.click(screen.getByRole("button", { name: "All" }));
    await waitFor(() => expect(spineTitles()).toHaveLength(4));
  });

  it("greps the phase's text, including output only the inspector would show", async () => {
    await mountSplit();
    const grep = screen.getByRole("searchbox", { name: /filter steps/i });
    fireEvent.change(grep, { target: { value: "api.test.ts" } });
    await waitFor(() => expect(spineTitles()).toEqual(["Verified"]));

    fireEvent.change(grep, { target: { value: "nothing matches this" } });
    await waitFor(() => expect(spineTitles()).toEqual([]));
    expect(document.querySelector(".trspine .empty")?.textContent).toBe("No step matches.");
  });

  it("moves the inspector onto a visible step when the filter hides the selected one", async () => {
    await mountSplit();
    expect(document.querySelector(".trinsp h4")?.textContent).toContain("Oriented");
    fireEvent.click(screen.getByRole("button", { name: "Edits" }));
    await waitFor(() =>
      expect(document.querySelector(".trinsp h4")?.textContent).toContain("Implemented"),
    );
  });
});

// ---------------------------------------------------------------------------------------------
// Acceptance 4 — the "Raw transcript" escape hatch (§4, mandatory).
// ---------------------------------------------------------------------------------------------
describe("the raw-transcript escape hatch (§4)", () => {
  it("drops to the flat oldest→newest LogEntry list, and back", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();

    fireEvent.click(screen.getByRole("button", { name: "Raw transcript" }));
    await waitFor(() => expect(document.querySelector(".trraw")).toBeTruthy());

    // Every entry, in the served order, tagged with its kind — the folding heuristics are gone.
    const lines = [...document.querySelectorAll(".trraw .rawline")];
    expect(lines).toHaveLength(COMPLETED.length);
    expect(lines[0].querySelector(".rk")?.textContent).toBe("event");
    expect(lines[0].textContent).toContain("session started");
    expect(lines[2].querySelector(".rk")?.textContent).toBe("tool_use");
    expect(lines[2].textContent).toContain("Read");
    expect(lines[2].textContent).toContain("file_path=/repo/src/lib/api.ts");
    // The trace's two zones are gone while the hatch is open.
    expect(document.querySelector(".trrc")).toBeNull();
    expect(document.querySelector(".trsplit")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Trace" }));
    await waitFor(() => expect(document.querySelector(".trsplit")).toBeTruthy());
    expect(document.querySelector(".trraw")).toBeNull();
  });

  it("shows the raw prose verbatim — the hatch is the one place markdown is NOT interpreted", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "text", text: "Ran **make lint**." })],
    });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    fireEvent.click(screen.getByRole("button", { name: "Raw transcript" }));
    await waitFor(() => expect(document.querySelector(".trraw")).toBeTruthy());
    expect(document.querySelector(".trraw .rawline")?.textContent).toContain("Ran **make lint**.");
    expect(document.querySelector(".trraw strong")).toBeNull();
  });
});

// ---------------------------------------------------------------------------------------------
// Slice 4 (STUDIO-745) — the watch-tabs rail under the inspector (§3C), and the ask dock (§6).
// ---------------------------------------------------------------------------------------------

/** The rail's tab buttons, by label, in the order they render. */
function tabLabels(): string[] {
  return [...document.querySelectorAll(".trwatch .tab")].map((el) => el.textContent ?? "");
}

/** Selects one of the rail's tabs and waits for its panel to be the one showing. */
async function openTab(label: string) {
  fireEvent.click(screen.getByRole("tab", { name: new RegExp(`^${label}`, "i") }));
  await waitFor(() =>
    expect(
      screen.getByRole("tab", { name: new RegExp(`^${label}`, "i") }).getAttribute("aria-selected"),
    ).toBe("true"),
  );
}

/** What the one mounted panel currently reads. */
function panel(): HTMLElement {
  return document.querySelector(`#${"trwatch-panel"}`) as HTMLElement;
}

function reviewJob(over: Record<string, unknown> = {}) {
  return {
    owner: "makewhatis",
    repo: "rhapsody",
    number: 105,
    reviewer: "jimmy",
    author: "alice",
    introduced_by: "handoff:STUDIO-654",
    requested_sha: "abc1234def",
    last_reviewed_sha: "",
    status: "in_flight",
    open: true,
    ...over,
  };
}

function runMessage(over: Partial<RunMessage> = {}): RunMessage {
  return { id: 1, run_id: 547, body: "btw the branch moved", created_at_ms: 0, status: "sent", ...over };
}

describe("the watch-tabs rail (§3C)", () => {
  it("puts the five tabs under the inspector, with Diff the only one marked a dependency", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();

    // Inside the split's right column, under the inspector — not a row of its own below the trace.
    const right = document.querySelector(".trsplit .trright") as HTMLElement;
    expect(right.querySelector(".trinsp")).toBeTruthy();
    expect(right.querySelector(".trwatch")).toBeTruthy();

    expect(tabLabels()).toEqual(["Diffdep", "Review", "Room", "Memory", "Messages"]);
    expect(screen.getByRole("tab", { name: /^room/i }).getAttribute("aria-selected")).toBe("true");
    // Every tab drives the one panel, and the panel names the tab that filled it.
    for (const tab of screen.getAllByRole("tab")) {
      expect(tab.getAttribute("aria-controls")).toBe("trwatch-panel");
    }
    expect(panel().getAttribute("aria-labelledby")).toBe("trtab-room");
  });

  // `role="tablist"` is a promise about the keyboard, not only about the screen reader: an
  // operator who reaches the rail expects the arrows to move between tabs, not to do nothing.
  it("moves between the tabs on the arrow keys, and to the ends on Home/End", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();

    const list = document.querySelector('.trwatch [role="tablist"]') as HTMLElement;
    const selected = () =>
      screen.getAllByRole("tab").find((t) => t.getAttribute("aria-selected") === "true")
        ?.textContent;

    expect(selected()).toBe("Room");
    fireEvent.keyDown(list, { key: "ArrowRight" });
    await waitFor(() => expect(selected()).toBe("Memory"));
    fireEvent.keyDown(list, { key: "ArrowLeft" });
    await waitFor(() => expect(selected()).toBe("Room"));
    fireEvent.keyDown(list, { key: "End" });
    await waitFor(() => expect(selected()).toBe("Messages"));
    // And it wraps, rather than dead-ending on the last tab.
    fireEvent.keyDown(list, { key: "ArrowRight" });
    await waitFor(() => expect(selected()).toBe("Diffdep"));
    fireEvent.keyDown(list, { key: "Home" });
    await waitFor(() => expect(selected()).toBe("Diffdep"));
  });

  // The whole reason only ONE panel is mounted: four surfaces polling for nobody to read.
  it("fetches a tab's data only once that tab is opened", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await waitFor(() => expect(h.fetchTeamsRoom).toHaveBeenCalled());
    expect(h.fetchReviews).not.toHaveBeenCalled();
    expect(h.fetchRunMessages).not.toHaveBeenCalled();

    await openTab("Review");
    await waitFor(() => expect(h.fetchReviews).toHaveBeenCalled());
    expect(h.fetchRunMessages).not.toHaveBeenCalled();
  });
});

// Acceptance — "Room tab shows this ticket's room posts; Memory tab shows this ticket's facts."
describe("Room · this ticket / Memory from this ticket (§3C)", () => {
  it("shows the room posts that reference this ticket and hides the ones that do not", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRoom.mockResolvedValue({
      messages: [
        { id: "f:1", from: "operator", to: "*", at: "2026-09-01T16:37:00Z", body: "Who can review this?", refs: ["STUDIO-654"] },
        { id: "f:2", from: "alice", to: "*", at: "2026-09-01T19:11:00Z", body: "Unrelated post", refs: [] },
        { id: "f:3", from: "alice", to: "*", at: "2026-09-01T19:20:00Z", body: "STUDIO-654 is **up for review**.", refs: [] },
      ],
      skipped: [],
    });

    await waitFor(() => expect(screen.getByText("Who can review this?")).toBeTruthy());
    expect(screen.queryByText("Unrelated post")).toBeNull();
    // STUDIO-739's renderer, the same one the room page uses — the prose is agent markdown.
    expect(panel().querySelector(".mcard strong")?.textContent).toBe("up for review");
    // Newest first, matching the room itself.
    expect([...panel().querySelectorAll(".mcard")][0].textContent).toContain("up for review");
  });

  it("shows only the facts this ticket's runs retained, on the Memory tab", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRecall.mockResolvedValue({
      identity: "alice",
      facts: [
        { id: "1", identity: "alice", document_id: "", ticket: "STUDIO-654", commit_sha: "", pr: "", run_id: "547", at: "", state: "valid", reason: "", content: "Run `make fixtures` first." },
        { id: "2", identity: "alice", document_id: "", ticket: "OTHER-1", commit_sha: "", pr: "", run_id: "1", at: "", state: "valid", reason: "", content: "Not this ticket." },
      ],
      skipped: [],
    });
    await settleTrace();
    await openTab("Memory");

    await waitFor(() => expect(screen.getByText(/make fixtures/)).toBeTruthy());
    expect(screen.queryByText("Not this ticket.")).toBeNull();
    expect(panel().querySelector(".mcard code")?.textContent).toBe("make fixtures");
    expect(panel().textContent).toContain("run 547");
  });

  // Nothing under `/api/v1/teams*` is fetched with Teams off — so the tab says which feature would
  // fill it rather than showing an empty list that reads as "nobody said anything".
  it("names the dependency, and fetches nothing, on a Teams-off daemon", async () => {
    h.fetchVersion.mockResolvedValue({ version: "v0.4.0", commit: "a", built_at: "", teams_enabled: false });
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    await settleTrace();

    // The WHOLE sentence, not its prefix: a `so there is no {noun phrase}` join renders as
    // "…so there is no the room posts about this ticket." and a prefix assertion never sees it.
    await waitFor(() =>
      expect(panel().textContent).toBe(
        "Teams is off on this daemon, so there is no room for anyone to post in.",
      ),
    );
    await openTab("Memory");
    expect(panel().textContent).toBe(
      "Teams is off on this daemon, so this ticket's runs retained no memory to show.",
    );
    expect(h.fetchTeamsRoom).not.toHaveBeenCalled();
    expect(h.fetchTeamsRecall).not.toHaveBeenCalled();
    // And there is no room to ask into either, so the dock is not offered.
    expect(document.querySelector(".askdock")).toBeNull();
  });
});

// A read that SUCCEEDED and came back empty is still not licence to state an absence: both these
// panels read a bounded window, so what they may say is bounded too. Room can tell the two cases
// apart by the size of its own read; recall carries no bound, so Memory names the window always.
describe("a bounded read never states an unbounded absence", () => {
  /** `n` room posts, none of them about the run's ticket. */
  function otherPosts(n: number) {
    return Array.from({ length: n }, (_, i) => ({
      id: `f:${i}`,
      from: "alice",
      to: "*",
      at: "2026-09-01T19:11:00Z",
      body: `OTHER-${i} moved on`,
      refs: [],
    }));
  }

  it("asks the daemon for the widest room window it will serve", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    await settleTrace();
    // Not the default 20 the daemon falls back to when the console names no limit.
    await waitFor(() => expect(h.fetchTeamsRoom).toHaveBeenCalledWith(ROOM_WATCH_WINDOW));
  });

  it("says what it read when the room came back full and none of it names this ticket", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRoom.mockResolvedValue({ messages: otherPosts(ROOM_WATCH_WINDOW), skipped: [] });
    await settleTrace();

    await waitFor(() =>
      expect(panel().textContent).toContain(
        `No post in the room's most recent ${ROOM_WATCH_WINDOW} mentions this ticket.`,
      ),
    );
    // Never the bare absence: everything older than the window went unread.
    expect(panel().textContent).not.toContain("No post in the room mentions this ticket.");
  });

  // A short read is not a whole-room read either — the daemon's 32-file day cap can truncate it
  // without the count ever reaching the window — so this panel names no number AND claims nothing.
  it("claims neither the window nor the whole room when the read fell short", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRoom.mockResolvedValue({ messages: otherPosts(3), skipped: [] });
    await settleTrace();

    await waitFor(() =>
      expect(panel().textContent).toContain(
        "No post in the room's recent history mentions this ticket.",
      ),
    );
    // Not the window sentence either — this read never reached the window.
    expect(panel().textContent).not.toContain("most recent");
    expect(panel().textContent).not.toContain("No post in the room mentions this ticket.");
  });

  // The escape the sentence owes the operator: the window is the console's, not the room's.
  it("offers the room itself when the window had nothing about this ticket", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    const onNavigate = mountDetail([run({ id: 1 })]);
    await settleTrace();

    fireEvent.click(await within(panel()).findByRole("link", { name: /open the room/i }));
    expect(onNavigate).toHaveBeenCalledWith("teams");
  });

  it("names the recall window rather than claiming this ticket retained nothing", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    await settleTrace();
    await openTab("Memory");

    await waitFor(() => expect(panel().textContent).toContain(MEMORY_EMPTY_NOTE));
  });
});

// Acceptance — "Messages composer posts to /runs/{id}/message and flips sent→delivered."
describe("Messages — the composer and its timeline (§3C)", () => {
  it("lists what was sent and flips its chip to delivered when the daemon says so", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.fetchRunMessages.mockResolvedValue([runMessage()]);
    mountDetail([run(LIVE)]);
    await settleTrace();
    await openTab("Messages");

    await waitFor(() => expect(panel().querySelector(".trmsgs .msg")).toBeTruthy());
    expect(panel().querySelector(".trmsgs .msg .body")?.textContent).toBe("btw the branch moved");
    expect(panel().querySelector(".trmsgs .msg .chip")?.textContent).toBe("sent");
    expect(panel().querySelector(".trmsgs .msg .chip.delivered")).toBeNull();

    // The 2s in-flight poll is what notices the agent picking it up.
    h.fetchRunMessages.mockResolvedValue([runMessage({ status: "delivered", delivered_turn: 3 })]);
    await poll(["run-messages", 547]);
    await waitFor(() =>
      expect(panel().querySelector(".trmsgs .msg .chip.delivered")?.textContent).toBe(
        "delivered · turn 3",
      ),
    );
  });

  it("posts a composed message to the daemon's own endpoint and clears the box", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.sendRunMessage.mockResolvedValue({ id: 3, identifier: "STUDIO-654", status: "sent" });
    mountDetail([run(LIVE)]);
    await settleTrace();
    await openTab("Messages");

    const box = (await screen.findByLabelText(/message the running agent/i)) as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "btw the branch moved" } });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
    await waitFor(() =>
      expect(h.sendRunMessage).toHaveBeenCalledExactlyOnceWith(547, "btw the branch moved"),
    );
    await waitFor(() => expect(box.value).toBe(""));
  });

  // The list is HISTORY, so it is shown for a finished run too — what was delivered, and what
  // expired because the run ended first. Only the send is impossible.
  it("still shows a finished run's timeline, including a message that expired undelivered", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.fetchRunMessages.mockResolvedValue([runMessage({ status: "expired" })]);
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Messages");

    await waitFor(() =>
      expect(panel().querySelector(".trmsgs .msg .chip.expired")?.textContent).toContain(
        "the run ended first",
      ),
    );
    expect((screen.getByRole("button", { name: /^send$/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  // The header's action is the way IN to the tab now — one composer, in the rail, not two.
  it("is where the header's Message action lands, with the cursor already in the box", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(screen.queryByLabelText(/message the running agent/i)).toBeNull();

    fireEvent.click(action(/^message/i));
    const box = await screen.findByLabelText(/message the running agent/i);
    expect(screen.getByRole("tab", { name: /^messages/i }).getAttribute("aria-selected")).toBe("true");
    await waitFor(() => expect(document.activeElement).toBe(box));
    // Exactly one composer in the document — the rail's.
    expect(screen.getAllByLabelText(/message the running agent/i)).toHaveLength(1);
  });

  // The focus request is CONSUMED. A monotonic counter re-fired on every later mount of the panel,
  // so merely clicking the Messages tab ejected a keyboard user out of the tablist and into the
  // textarea, and scrolled the page under a mouse user.
  it("does not re-steal focus when the operator later just clicks the Messages tab", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run(LIVE)]);
    await settleTrace();

    fireEvent.click(action(/^message/i));
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByLabelText(/message the running agent/i)),
    );

    await openTab("Room");
    const tab = screen.getByRole("tab", { name: /^messages/i });
    tab.focus();
    fireEvent.click(tab);
    await waitFor(() => expect(screen.queryByLabelText(/message the running agent/i)).toBeTruthy());
    // The tab the operator activated keeps the focus; the composer does not take it back.
    expect(document.activeElement).toBe(tab);

    // But asking AGAIN through the header still lands the cursor in the box.
    fireEvent.click(action(/^message/i));
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByLabelText(/message the running agent/i)),
    );
  });

  // Reading the room mid-compose must not throw away what the operator typed, which is why the
  // draft is held above the panel that unmounts.
  it("keeps a half-written message across a trip to another tab", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run(LIVE)]);
    await settleTrace();
    await openTab("Messages");
    fireEvent.change(await screen.findByLabelText(/message the running agent/i), {
      target: { value: "btw the branch moved" },
    });

    await openTab("Room");
    expect(screen.queryByLabelText(/message the running agent/i)).toBeNull();
    await openTab("Messages");
    const box = (await screen.findByLabelText(/message the running agent/i)) as HTMLTextAreaElement;
    expect(box.value).toBe("btw the branch moved");
  });

  // An instruction written for one attempt is not an instruction for another, and retargeting it
  // silently would send the operator's words to a run they never chose.
  it("drops the draft when the operator switches attempt", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ ...LIVE }), run({ id: 522, outcome: "completed" })]);
    await settleTrace();
    await openTab("Messages");
    fireEvent.change(await screen.findByLabelText(/message the running agent/i), {
      target: { value: "btw the branch moved" },
    });

    fireEvent.click(screen.getByRole("button", { name: /run 522/i }));
    await waitFor(() =>
      expect(
        (screen.getByLabelText(/message the running agent/i) as HTMLTextAreaElement).value,
      ).toBe(""),
    );
  });
});

// A settled react-query ERROR is not `isPending`, so branching on that alone states the empty copy
// as fact about a read that never landed. The Messages one is the damaging case: an operator told
// "no message has been sent" answers it by sending the same message a second time.
describe("a failed read is never reported as an empty one", () => {
  const cases = [
    { tab: "Messages", none: /No message has been sent/i, fail: () => h.fetchRunMessages.mockRejectedValue(new Error("boom")) },
    { tab: "Review", none: /No review has been requested/i, fail: () => h.fetchReviews.mockRejectedValue(new Error("boom")) },
  ];

  for (const c of cases) {
    it(`says the ${c.tab} read failed rather than that there is nothing`, async () => {
      h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
      c.fail();
      mountDetail([run({ id: 547 })]);
      await settleTrace();
      await openTab(c.tab);
      await waitFor(() => expect(panel().textContent).toContain("the request failed"));
      expect(panel().textContent).not.toMatch(c.none);
    });
  }

  it("says the room read failed rather than that nobody mentioned this ticket", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    h.fetchTeamsRoom.mockRejectedValue(new Error("boom"));
    await settleTrace();
    await waitFor(() => expect(panel().textContent).toContain("the request failed"));
    expect(panel().textContent).not.toMatch(/mentions this ticket/i);
  });

  // ONE bank failing is enough. The roster is deliberately TWO teammates with only bob's bank
  // rejecting: on a one-member roster `some` and `every` are indistinguishable, so the obvious
  // fixture would pass just as happily against the `every` the hook's own comment warns against.
  it("says the memory read failed when any ONE teammate's bank cannot be read", async () => {
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [teammate("alice"), teammate("bob")],
    });
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    // AFTER the mount: `mountDetail` installs its own recall default unconditionally, and the
    // Memory panel is not mounted until its tab is opened below, so nothing has read it yet.
    h.fetchTeamsRecall.mockImplementation(async (identity: string) => {
      if (identity === "bob") throw new Error("boom");
      return { identity, facts: [], skipped: [] };
    });
    await settleTrace();
    await openTab("Memory");
    await waitFor(() => expect(panel().textContent).toContain("the request failed"));
    expect(panel().textContent).not.toMatch(/is stamped with this ticket/i);
  });

  // The recall's own PREREQUISITE. With no roster there is nobody to recall from, so the fan-out
  // fires nothing and settles as a successful empty read — while the console has in fact learned
  // nothing at all about whose banks to look in.
  it("does not claim an empty memory while the roster itself is unread", async () => {
    h.fetchTeamsOverview.mockRejectedValue(new Error("boom"));
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Memory");
    await waitFor(() => expect(panel().textContent).toContain("the request failed"));
    expect(panel().textContent).not.toMatch(/is stamped with this ticket/i);
    // And it really did have no bank to ask — the claim would have been about nothing.
    expect(h.fetchTeamsRecall).not.toHaveBeenCalled();
  });
});

// Acceptance — "Diff + Review render as dependency-named (deep-link / status), never fake."
describe("Diff and Review — dependency-named, never invented (§5)", () => {
  it("shows no diff at all, names the endpoint it waits on, and deep-links the pull request", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Diff");

    expect(panel().querySelector(".trdep")?.textContent).toContain("needs a daemon endpoint");
    // Nothing that could be read as a diff: no patch text, no +/- lines, no file list.
    expect(panel().querySelector("pre")).toBeNull();
    expect(panel().textContent).not.toMatch(/^[+-]{3}/m);
    // The deep link is the head-branch SEARCH, so it can never assert a PR that does not exist.
    const link = within(panel()).getByRole("link", { name: /pull request/i });
    expect(link.getAttribute("href")).toBe(
      "https://github.com/makewhatis/rhapsody/pulls?q=is%3Apr%20head%3Asymphony%2FSTUDIO-654",
    );
    expect(panel().textContent).toContain("symphony/STUDIO-654");
  });

  it("names the missing link too when the run's remote is not a GitHub one", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547, repo: "git@gitlab.com:o/r.git" })]);
    await settleTrace();
    await openTab("Diff");
    expect(within(panel()).queryByRole("link")).toBeNull();
    expect(panel().textContent).toContain("not on github.com");
  });

  it("reports the reviewer and the watch-set status, and names the verdict as the dependency", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.fetchReviews.mockResolvedValue({
      enabled: true,
      reviews: [
        reviewJob({ status: "reviewed", last_reviewed_sha: "e90ccc6457f" }),
        // A different ticket's pull request: this run's tab must not claim it.
        reviewJob({ number: 104, reviewer: "bob", introduced_by: "handoff:STUDIO-744" }),
      ],
    });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Review");

    await waitFor(() => expect(panel().querySelectorAll(".trrev .rev")).toHaveLength(1));
    const row = panel().querySelector(".trrev .rev") as HTMLElement;
    expect(row.textContent).toContain("jimmy");
    expect(row.querySelector(".pill")?.textContent).toContain("Reviewed");
    expect(row.querySelector(".pr")?.getAttribute("href")).toBe(
      "https://github.com/makewhatis/rhapsody/pull/105",
    );
    expect(row.textContent).toContain("e90ccc6");
    // What is NOT served: the findings. The panel says so rather than printing a verdict.
    expect(panel().querySelector(".trdep")?.textContent).toContain(
      "The verdict itself is a dependency",
    );
  });

  it("says nothing is watching this run rather than showing an empty table", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Review");
    await waitFor(() =>
      expect(panel().textContent).toContain("No review has been requested"),
    );
    expect(panel().querySelector(".trrev")).toBeNull();
  });

  // `{enabled: false}` is the daemon's own answer — Teams is off, or the review mode is not
  // `ticketless`. It is not an error, and it is not "no reviewer yet".
  it("distinguishes a daemon that does not run ticketless review at all", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.fetchReviews.mockResolvedValue({ enabled: false, reviews: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await openTab("Review");
    await waitFor(() =>
      expect(panel().textContent).toContain("Ticketless review is not enabled"),
    );
  });
});

// Acceptance — "'Ask about this run' posts a room message refed to the run."
describe("the ask dock (§6)", () => {
  it("posts the operator's question to the room, refed to the ticket AND the run", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.postTeamsRoom.mockResolvedValue({
      id: "f:9", from: "operator", to: "*", at: "2026-09-03T10:00:00Z", refs: [], delivered: 0,
    });
    mountDetail([run({ id: 547 })]);
    await settleTrace();

    const box = screen.getByLabelText(/ask about this run/i);
    fireEvent.change(box, { target: { value: "Why did alice pick this reviewer?" } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));

    await waitFor(() =>
      expect(h.postTeamsRoom).toHaveBeenCalledExactlyOnceWith("Why did alice pick this reviewer?", [
        "STUDIO-654",
        "run 547",
      ]),
    );
    // It says where the question went, and — with no reply in the room read — says only that.
    await waitFor(() => expect(screen.getByText(/posted to the room/i)).toBeTruthy());
    expect((box as HTMLInputElement).value).toBe("");
  });

  // The dock's whole claim is that the question is about THIS attempt (that is what `refs` says),
  // so an unsent one must not ride an attempt switch and be posted refed to a run it was never
  // about — the same rule the composer's draft follows.
  it("drops an unsent question, and its receipt, when the operator switches attempt", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.postTeamsRoom.mockResolvedValue({
      id: "f:9", from: "operator", to: "*", at: "2026-09-03T10:00:00Z", refs: [], delivered: 0,
    });
    mountDetail([run({ id: 547 }), run({ id: 522 })]);
    await settleTrace();

    // A question that DID land leaves an exchange…
    fireEvent.change(screen.getByLabelText(/ask about this run/i), { target: { value: "why?" } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));
    await waitFor(() => expect(screen.getByText(/posted to the room/i)).toBeTruthy());
    // …which quotes the question that landed and goes on quoting it while the next one is being
    // written. That is why it may stay on screen where STUDIO-745's subject-less "Posted to the
    // room" chip had to be cleared on the first keystroke: this card cannot be read as a claim
    // about unsent text, and an answer that vanished mid-follow-up would have to be read in the
    // room after all.
    fireEvent.change(screen.getByLabelText(/ask about this run/i), {
      target: { value: "why did run 547 fail?" },
    });
    expect(document.querySelector(".askex .qb")?.textContent).toBe("why?");

    fireEvent.click(screen.getByRole("button", { name: /run 522/i }));
    await waitFor(() =>
      expect((screen.getByLabelText(/ask about this run/i) as HTMLInputElement).value).toBe(""),
    );
    // And nothing about run 547 was posted refed to run 522.
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));
    await flushSend();
    expect(h.postTeamsRoom).toHaveBeenCalledExactlyOnceWith("why?", ["STUDIO-654", "run 547"]);
  });

  it("refuses an empty question rather than posting one", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    fireEvent.change(screen.getByLabelText(/ask about this run/i), { target: { value: "  " } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));
    await flushSend();
    expect(h.postTeamsRoom).not.toHaveBeenCalled();
  });

  it("surfaces a refused post rather than swallowing it", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.postTeamsRoom.mockRejectedValue(new Error("teams_disabled"));
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    fireEvent.change(screen.getByLabelText(/ask about this run/i), { target: { value: "why?" } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));
    await waitFor(() =>
      expect(document.querySelector(".askdock .acterr")?.textContent).toContain("teams_disabled"),
    );
    // Nothing landed, so there is no question to quote and no reply to look up.
    expect(document.querySelector(".askex")).toBeNull();
  });

  // -------------------------------------------------------------------------------------------
  // STUDIO-733 (answering-manager slice 5) — the manager's room reply, read back inline.
  //
  // Acceptance — "the console surfaces the manager's room reply inline, refed to the question/run
  // — the SAME room post, not a re-computed answer", and "found → shown; not-yet-answered → an
  // honest pending/absence, never fabricated".
  // -------------------------------------------------------------------------------------------

  /**
   * Posts `body` through the dock and returns the id the daemon echoed for it.
   *
   * `room` is what the room read answers from then on. It is armed BEFORE the post because
   * `usePostToRoom` invalidates the room query on success, and that refetch — not the 5s poll — is
   * what puts the reply on screen inside a `waitFor` window.
   */
  async function ask(body: string, id = "f:9", room?: Record<string, unknown>[]): Promise<string> {
    h.postTeamsRoom.mockResolvedValue({
      id, from: "operator", to: "*", at: "2026-09-03T10:00:00Z", refs: [], delivered: 0,
    });
    if (room !== undefined) h.fetchTeamsRoom.mockResolvedValue({ messages: room, skipped: [] });
    fireEvent.change(screen.getByLabelText(/ask about this run/i), { target: { value: body } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));
    await waitFor(() => expect(h.postTeamsRoom).toHaveBeenCalled());
    return id;
  }

  /** One room post, shaped like the daemon's. */
  function roomPost(over: Record<string, unknown>) {
    return { from: "operator", to: "*", at: "2026-09-03T10:00:30Z", body: "", refs: [], ...over };
  }

  /** The manager's own reply: quoted prose over the host's records, the shape `compose_reply` writes. */
  const MANAGER_ANSWER = roomPost({
    id: "f:10",
    from: "@manager",
    body: "> It stopped at the **lint** step.\n\nFrom my own records — STUDIO-654 · completed · 19:15",
  });

  it("renders the manager's own room post beside the question, not a re-computed answer", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    const id = await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:9", body: "Why did this stop?", refs: ["STUDIO-654", "run 547"] }),
      { ...MANAGER_ANSWER, refs: ["f:9"] },
    ]);
    expect(id).toBe("f:9");

    const card = () => document.querySelector(".askex .mcard") as HTMLElement;
    await waitFor(() => expect(card()).toBeTruthy());
    // The question the operator sent, quoted back beside the answer to it.
    expect(document.querySelector(".askex .qb")?.textContent).toBe("Why did this stop?");
    expect(card().querySelector(".who2")?.textContent).toBe("@manager");
    // The room post's own body, through the room's own renderer. Both halves of the daemon's
    // partition survive: `QUOTE_PREFIX` still marks every line the MODEL wrote — STUDIO-739's
    // parser leaves block quotes as literal text, so the marker the daemon stamped is the marker
    // the operator sees — and the host's records still sit under the grounding lead beside it.
    // That layout is the whole reason a plant cannot pass itself off as the daemon's records, so
    // reshaping it here would cost the operator exactly the signal it exists to give them.
    expect(card().textContent).toContain("> It stopped at the");
    expect(card().querySelector("strong")?.textContent).toBe("lint");
    expect(card().textContent).toContain("From my own records — STUDIO-654 · completed · 19:15");
    // And it is a READ: nothing was posted but the question itself.
    expect(h.postTeamsRoom).toHaveBeenCalledExactlyOnceWith("Why did this stop?", [
      "STUDIO-654",
      "run 547",
    ]);
  });

  // The read is newest-first, so a read holding the question holds everything written after it —
  // which makes "not replied yet" a fact about the log rather than a guess about a process.
  it("says the manager has not replied while the question is in the read and nothing answers it", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:9", body: "Why did this stop?" }),
      roomPost({ id: "f:11", from: "alice", body: "unrelated chatter" }),
    ]);

    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "@manager has not replied to it yet",
      ),
    );
    // Never a process it cannot see, and never someone else's post dressed as the answer.
    expect(document.querySelector(".askex .pending")?.textContent).not.toMatch(/thinking|working/i);
    expect(document.querySelector(".askex .mcard")).toBeNull();
  });

  // Once the read no longer reaches the question it says nothing about what came after it, so the
  // dock has to stop claiming rather than report a silence it cannot see.
  it("stops claiming once the read no longer reaches the question", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:900", from: "alice", body: "much later" }),
    ]);

    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "no longer reaches that question",
      ),
    );
    expect(document.querySelector(".askex .pending")?.textContent).not.toContain(
      "has not replied to it yet",
    );
    // Never a dead end: the room is one click away, which is where the rest of the read is.
    expect(document.querySelector(".askex .pending .link")?.textContent).toContain("Open the room");
  });

  // `past-window` is a claim about a read that COULD have seen the question. The room query is
  // warm on essentially every run detail — "room" is the default watch tab, and `useTeamsRoom`
  // keeps the previous window on screen — so the dock joins a read whose newest data PREDATES the
  // question it was just handed. Reading that as "the window has moved past your question" is the
  // most misleading sentence this surface can produce, and it would fire on the ordinary path.
  it("never reports a question posted a moment ago as past the read's window", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    h.fetchTeamsRoom.mockResolvedValue({
      messages: [roomPost({ id: "f:1", from: "alice", body: "posted before the question" })],
      skipped: [],
    });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await waitFor(() => expect(h.fetchTeamsRoom).toHaveBeenCalled());

    // Hold the read the post triggers open, so the window between the question landing and the
    // first read that could contain it is observable rather than a single frame.
    let land: () => void = () => {};
    h.fetchTeamsRoom.mockReturnValue(
      new Promise((resolve) => {
        land = () =>
          resolve({
            messages: [
              roomPost({ id: "f:1", from: "alice", body: "posted before the question" }),
              roomPost({ id: "f:9", body: "Why did this stop?" }),
            ],
            skipped: [],
          });
      }),
    );
    await ask("Why did this stop?", "f:9");

    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain("reading it back"),
    );
    // Not a conclusion about the room, and not one about the manager either: nothing has looked.
    const pending = document.querySelector(".askex .pending")?.textContent ?? "";
    expect(pending).not.toContain("no longer reaches that question");
    expect(pending).not.toContain("has not replied to it yet");

    // And it is a state the read LEAVES — the first read that settles after the question decides.
    await act(async () => {
      land();
    });
    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "@manager has not replied to it yet",
      ),
    );
  });

  // The same false sentence, arriving by the other door: a room query whose FIRST read is still in
  // flight when the question lands. The post's invalidate cannot cancel that read — react-query
  // cancels a fetch only on a query that already holds data — so it dedupes into it, and a snapshot
  // taken BEFORE the question was appended settles after it. "Settled since this mounted" is true
  // of that read while "could have seen the question" is not, which is the distinction the gate
  // exists to draw. It is on the ordinary path: the Room tab dispatches that first read on mount,
  // so an operator who asks inside its round trip is asking exactly here.
  it("never reports a question as past the window on a first read that predates it", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    // No read may settle before the question lands, so the first one is held open. `mountDetail`
    // installs its own room default as it renders, so this has to be armed after it and before the
    // watch tabs mount — which the assertions below then hold it to.
    const reads: Array<(v: unknown) => void> = [];
    mountDetail([run({ id: 547 })]);
    h.fetchTeamsRoom.mockImplementation(() => new Promise((resolve) => reads.push(resolve)));
    await settleTrace();
    await waitFor(() => expect(reads.length).toBeGreaterThanOrEqual(1));

    const roomQuery = () =>
      client.getQueryCache().find({ queryKey: ["teams", "room", ROOM_WATCH_WINDOW] });
    // The state this case is about: a genuinely FIRST read, in flight, nothing cached under it.
    expect(roomQuery()?.state.data).toBeUndefined();
    expect(roomQuery()?.state.fetchStatus).toBe("fetching");

    await ask("Why did this stop?", "f:9");
    await waitFor(() => expect(document.querySelector(".askex")).toBeTruthy());
    // The post deduped into that read rather than replacing it — the premise of the whole case.
    const deduped = reads.length;
    expect(deduped).toBe(1);

    // It settles: a window from before the question, and this query's first data.
    await act(async () => {
      reads[0]({
        messages: [roomPost({ id: "f:1", from: "alice", body: "posted before the question" })],
        skipped: [],
      });
    });
    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toBeTruthy(),
    );
    const pending = document.querySelector(".askex .pending")?.textContent ?? "";
    expect(pending).not.toContain("no longer reaches that question");
    expect(pending).not.toContain("has not replied to it yet");
    expect(pending).toContain("reading it back");

    // And the gate still OPENS — on a read whose fetch was DISPATCHED after the question, which is
    // the next one on this key. Waiting is not the fix; claiming from a read that could not have
    // seen the question is, and a dock that never claimed again would be no more use than one that
    // claimed wrongly.
    await act(async () => {
      void client.invalidateQueries();
    });
    await waitFor(() => expect(reads.length).toBeGreaterThan(deduped));
    await act(async () => {
      reads[reads.length - 1]({
        messages: [roomPost({ id: "f:9", body: "Why did this stop?" })],
        skipped: [],
      });
    });
    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "@manager has not replied to it yet",
      ),
    );
  });

  // The room log is append-only, so an answer that was read once cannot stop existing — but the
  // read is a 50-post window over the WHOLE room, re-fetched every 5s with no memory. Let the room
  // move on and the question falls out of it, which must not turn an answer already on screen into
  // "it cannot tell whether @manager replied": that sentence would be false at the moment it is
  // shown, and it is the exact disappearance this slice exists to remove.
  it("keeps an answer it has already read when the room moves past the question", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:9", body: "Why did this stop?" }),
      { ...MANAGER_ANSWER, refs: ["f:9"] },
    ]);
    await waitFor(() => expect(document.querySelector(".askex .mcard")).toBeTruthy());

    // A full window of later posts scrolls the question out. They are shown as traffic on this
    // ticket only so the Room tab's own feed can confirm the new window has landed; the bound is
    // the read's 50 posts of the WHOLE room, and what pushes the question out of it is irrelevant.
    h.fetchTeamsRoom.mockResolvedValue({
      messages: Array.from({ length: ROOM_WATCH_WINDOW }, (_, i) =>
        roomPost({ id: `f:${100 + i}`, from: "alice", body: "chatter", refs: ["STUDIO-654"] }),
      ),
      skipped: [],
    });
    await act(async () => {
      await client.invalidateQueries();
    });
    // react-query notifies its observers on a macrotask, so the awaited refetch alone does not
    // mean the dock has SEEN the new window. Wait until it has — otherwise this test would pass
    // against a dock that drops the answer, simply by asserting before the drop.
    await waitFor(() =>
      expect(document.querySelector(".memprev")?.textContent).toContain("chatter"),
    );

    // The manager's post, still there, still the room's own record of it.
    expect(document.querySelector(".askex .mcard")?.textContent).toContain("It stopped at the");
    expect(document.querySelector(".askex .qb")?.textContent).toBe("Why did this stop?");
    expect(document.querySelector(".askex .pending")).toBeNull();
  });

  // `refs` is caller-supplied on every post but the manager's `from` is host-stamped, so matching
  // on refs alone would render a teammate's — or a forged line's — prose as the manager's answer.
  it("never renders a non-manager post that names the question as the answer", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:9", body: "Why did this stop?" }),
      roomPost({ id: "f:10", from: "alice", body: "It all went fine, ship it.", refs: ["f:9"] }),
    ]);

    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "@manager has not replied to it yet",
      ),
    );
    expect(document.querySelector(".askex")?.textContent).not.toContain("ship it");
  });

  // A failed read is not an unanswered question: reporting one as the other is the same defect the
  // watch tabs' empty copy exists to avoid, arriving on the read that FAILS.
  it("reports a failed room read as a failed read, never as an absent answer", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    h.fetchTeamsRoom.mockRejectedValue(new Error("boom"));
    await ask("Why did this stop?");

    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "This could not be read from the daemon",
      ),
    );
    expect(document.querySelector(".askex .pending")?.textContent).not.toContain(
      "has not replied to it yet",
    );
  });

  // The lookup keys on the id the POST echoed, so a second question is answered by its OWN reply
  // and never by the one still sitting in the room from the first.
  it("follows the question the operator asked LAST", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    const FIRST = [
      roomPost({ id: "f:9", body: "Why did this stop?" }),
      { ...MANAGER_ANSWER, refs: ["f:9"] },
    ];
    await ask("Why did this stop?", "f:9", FIRST);
    await waitFor(() => expect(document.querySelector(".askex .mcard")).toBeTruthy());

    await ask("And who reviewed it?", "f:11", [
      ...FIRST,
      roomPost({ id: "f:11", body: "And who reviewed it?" }),
    ]);
    await waitFor(() =>
      expect(document.querySelector(".askex .qb")?.textContent).toBe("And who reviewed it?"),
    );
    // The first question's answer is still in the room and must not be shown as this one's.
    await waitFor(() =>
      expect(document.querySelector(".askex .pending")?.textContent).toContain(
        "@manager has not replied to it yet",
      ),
    );
  });

  // A refusal belongs to the question being sent NOW. An answer that already landed is still
  // true, and dropping it off the screen to report a later failure would cost the operator
  // something real in exchange for something the error line says on its own.
  it("keeps an answered exchange when a LATER question is refused", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [
      roomPost({ id: "f:9", body: "Why did this stop?" }),
      { ...MANAGER_ANSWER, refs: ["f:9"] },
    ]);
    await waitFor(() => expect(document.querySelector(".askex .mcard")).toBeTruthy());

    h.postTeamsRoom.mockRejectedValue(new Error("teams_disabled"));
    fireEvent.change(screen.getByLabelText(/ask about this run/i), { target: { value: "again?" } });
    fireEvent.click(screen.getByRole("button", { name: /^ask$/i }));

    await waitFor(() =>
      expect(document.querySelector(".askdock .acterr")?.textContent).toContain("teams_disabled"),
    );
    // The answered exchange is still the first question's, and still its answer.
    expect(document.querySelector(".askex .qb")?.textContent).toBe("Why did this stop?");
    expect(document.querySelector(".askex .mcard")?.textContent).toContain("It stopped at the");
  });

  // The announcement that matters is the ANSWER arriving, and the pending note is REPLACED by the
  // card — so a live region scoped to the note would go silent at the one moment it should speak.
  it("announces the answer, not only the wait", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    await ask("Why did this stop?", "f:9", [roomPost({ id: "f:9", body: "Why did this stop?" })]);
    const live = () => document.querySelector(".askex [role='status']") as HTMLElement;
    await waitFor(() => expect(live()?.textContent).toContain("has not replied to it yet"));

    h.fetchTeamsRoom.mockResolvedValue({
      messages: [roomPost({ id: "f:9", body: "Why did this stop?" }), { ...MANAGER_ANSWER, refs: ["f:9"] }],
      skipped: [],
    });
    await act(async () => {
      await client.invalidateQueries();
    });
    // The SAME region now carries the answer, so it is announced rather than silently swapped in.
    await waitFor(() => expect(live()?.textContent).toContain("It stopped at the"));
    expect(live().querySelector(".mcard")).toBeTruthy();
  });

  // With nothing asked there is nothing to look up, so the dock is not a room reader.
  it("reads the room only once a question has landed", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    // Off the Room tab, so the only thing that could read the room is the dock.
    await openTab("Messages");
    h.fetchTeamsRoom.mockClear();
    await flushSend();
    expect(h.fetchTeamsRoom).not.toHaveBeenCalled();

    await ask("Why did this stop?");
    // Same window as the Room tab's own read, so the two share one query and can never show
    // different rooms.
    await waitFor(() => expect(h.fetchTeamsRoom).toHaveBeenCalledWith(ROOM_WATCH_WINDOW));
  });

  // The raw hatch is the debugger's escape from the folding, not a place to ask about it.
  it("is not offered while the raw transcript is open", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    expect(document.querySelector(".askdock")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Raw transcript" }));
    await waitFor(() => expect(document.querySelector(".trraw")).toBeTruthy());
    expect(document.querySelector(".askdock")).toBeNull();
    expect(document.querySelector(".trwatch")).toBeNull();
  });
});

// The exchange card sits directly on top of the dock and is drawn as one control with it, so the
// two borders that meet have to agree about their corners.
describe("the ask card and the dock read as one control (STUDIO-733)", () => {
  const css = readFileSync(path.resolve(__dirname, "../../../theme/console-trace.css"), "utf8");

  it("squares the corners where the card meets the dock, and only there", () => {
    // The card gives up its bottom corners and its bottom border to the dock below it…
    expect(css).toContain(
      ".rh-console .askex { border: 1px solid var(--line); border-bottom: 0; " +
        "border-radius: var(--r) var(--r) 0 0;",
    );
    // …and the dock gives up its top corners, but only while a card is there to meet them.
    expect(css).toContain(
      ".rh-console .askex + .askdock { border-radius: 0 0 var(--r) var(--r); }",
    );
  });
});

// ---------------------------------------------------------------------------------------------
// Acceptance 6 — wide content (code) scrolls in its own box; the page never scrolls sideways.
// ---------------------------------------------------------------------------------------------
describe("wide content is contained (STUDIO-681's layout rule)", () => {
  const css = readFileSync(path.resolve(__dirname, "../../../theme/console-trace.css"), "utf8");

  /** The declarations of one selector's block. */
  function rule(selector: string): string {
    const at = css.indexOf(`${selector} {`);
    expect(at, `${selector} is not declared`).toBeGreaterThan(-1);
    return css.slice(at, css.indexOf("}", at));
  }

  it("scrolls a tool result inside the call-card rather than widening the page", () => {
    const out = rule(".rh-console .trcard .out pre");
    expect(out).toMatch(/overflow-x:\s*auto/);
    expect(out).toMatch(/white-space:\s*pre-wrap/);
    // A path or a URL with no spaces has nowhere to wrap without this.
    expect(out).toMatch(/overflow-wrap:\s*anywhere/);
  });

  it("contains a raw transcript line the same way", () => {
    const line = rule(".rh-console .trraw .rawline");
    expect(line).toMatch(/white-space:\s*pre-wrap/);
    expect(line).toMatch(/overflow-wrap:\s*anywhere/);
  });

  it("gives the inspector a zero minimum, so a wide child cannot stretch the grid column", () => {
    // A CSS grid track is `min-width: auto` by default, which means "as wide as the content" —
    // the one way a fenced code block inside the inspector can push the whole page sideways.
    expect(rule(".rh-console .trsplit")).toMatch(/grid-template-columns:\s*264px minmax\(0, 1fr\)/);
    // `.trright` is the grid CHILD now — the inspector and the watch rail share that column — so
    // it is the one that has to carry the zero minimum; `.trinsp` alone would not save the page.
    expect(rule(".rh-console .trright")).toMatch(/min-width:\s*0/);
    expect(rule(".rh-console .trinsp")).toMatch(/min-width:\s*0/);
    expect(rule(".rh-console .trwatch .tabbody")).toMatch(/min-width:\s*0/);
    expect(rule(".rh-console .trrc .body")).toMatch(/min-width:\s*0/);
  });

  it("wraps an operator message body rather than widening the rail", () => {
    const body = rule(".rh-console .trmsgs .msg .body");
    expect(body).toMatch(/white-space:\s*pre-wrap/);
    expect(body).toMatch(/overflow-wrap:\s*anywhere/);
  });

  it("keeps the spine's one-line summaries from being widened by a long command", () => {
    expect(rule(".rh-console .trstep .ssub")).toMatch(/text-overflow:\s*ellipsis/);
    expect(rule(".rh-console .trcard .tgt")).toMatch(/text-overflow:\s*ellipsis/);
  });

  it("hardcodes no color the token set already names", () => {
    expect(css).not.toMatch(/#[0-9a-f]{3,8}\b/i);
  });

  // The header wraps: `flex-wrap: wrap` over an attempt selector, a vitals strip and six actions
  // is taller than one row on a narrow window, and a spine pinned to a literal offset then slides
  // underneath it. The offset is a custom property the view measures and publishes.
  it("sticks the spine below the header's MEASURED height, not a hardcoded one", () => {
    expect(rule(".rh-console .trhd")).toMatch(/flex-wrap:\s*wrap/);
    expect(rule(".rh-console .trspine")).toMatch(/top:\s*var\(--trhd-h,\s*\d+px\)/);
  });

  it("wraps and scrolls a failure banner, which can carry one very long line", () => {
    const banner = rule(".rh-console .trrc .trbanner");
    expect(banner).toMatch(/overflow-x:\s*auto/);
    expect(rule(".rh-console .trrc .trbanner span")).toMatch(/overflow-wrap:\s*anywhere/);
  });
});

// ---------------------------------------------------------------------------------------------
// STUDIO-744 — slice 3 of the Trace plan: the states the completed-run hero does not cover. A
// live run (the playhead), a failed one (the jump to the failing step), and a ticket whose work
// relayed across more than one run (the attempt selector's baton).
// ---------------------------------------------------------------------------------------------

/** A transcript still being written: it has oriented and edited, and has not verified yet. */
const STREAMING: LogEntry[] = [
  entry({ seq: 1, kind: "event", text: "session started" }),
  entry({ seq: 2, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 3, kind: "tool_result", text: "export interface RunSummary {" }),
  entry({ seq: 4, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" }),
  entry({ seq: 5, kind: "tool_result", text: "The file has been updated." }),
];

/** The same transcript one poll later — a phase the spine has not seen before. */
const STREAMED_ON: LogEntry[] = [
  ...STREAMING,
  entry({ seq: 6, kind: "tool_use", tool: "Bash", text: "command=cargo test --workspace" }),
  entry({ seq: 7, kind: "tool_result", text: "test result: ok. 0 failed" }),
];

/** The spine step the inspector is showing. */
function selectedStep(): string {
  return document.querySelector('.trstep[aria-pressed="true"] .stt')?.textContent ?? "";
}

/** The step the playhead's `now` badge marks; "" when the spine marks none. */
function nowStep(): string {
  return document.querySelector(".trstep.now .stt")?.textContent ?? "";
}

/** The spine's grep field. */
function grepField(): HTMLInputElement {
  return screen.getByRole("searchbox", { name: /filter steps/i }) as HTMLInputElement;
}

/** Re-runs a polled query's fetcher, which is what a poll tick does. */
async function poll(key: unknown[]) {
  await act(async () => {
    await client.invalidateQueries({ queryKey: key });
  });
}

/** The page's own scroller, which is what the run detail scrolls. */
function scroller(): HTMLElement {
  return (document.scrollingElement ?? document.documentElement) as HTMLElement;
}

/**
 * Gives the page a real geometry. jsdom lays nothing out, so every dimension is 0 and "at the
 * bottom" is trivially true — the follow rules cannot be exercised without saying how tall the
 * page is. `height` is a getter so a test can GROW the page the way a poll does.
 */
function sizePage(height: () => number, viewport = 800) {
  const el = scroller();
  Object.defineProperty(el, "scrollHeight", { get: height, configurable: true });
  Object.defineProperty(el, "clientHeight", { value: viewport, configurable: true });
}

/** Puts the window where a scrolled-up operator has left it, and tells the view about it. */
function scrollUp(distance: number) {
  sizePage(() => 2000);
  scroller().scrollTop = 2000 - 800 - distance;
  fireEvent.scroll(window);
}

const LIVE = { id: 547, outcome: "running", ended_at: "" } as const;

describe("the live run — the spine is a playhead (§3A/§3C)", () => {
  it("opens on the NEWEST phase, the one a streaming run is writing into", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(spineTitles()).toEqual(["Oriented", "Implemented"]);
    // A finished run opens on its FIRST step — you read a trace forwards. A live one does not.
    expect(selectedStep()).toBe("Implemented");
    expect(document.querySelector(".trstep.now")).toBeTruthy();
  });

  it("advances the playhead when the transcript poll brings a newer phase", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(selectedStep()).toBe("Implemented");

    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    await waitFor(() => expect(selectedStep()).toBe("Verified"));
    expect(document.querySelector(".trinsp .trcard .tgt")?.textContent).toBe(
      "cargo test --workspace",
    );
  });

  it("offers no jump-to-latest on a live run whose transcript has not arrived", async () => {
    // A run that has just started has no phase to track — and so nothing to have fallen behind.
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(spineTitles()).toEqual([]);
    expect(document.querySelector(".trlatest")).toBeNull();
  });

  it("marks the playhead over the RUN, not the filtered spine — the `now` badge cannot lie", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(spineTitles()).toEqual(["Oriented", "Implemented", "Verified"]);
    expect(nowStep()).toBe("Verified");

    // A grep that hides the phase the run is writing into marks NOTHING. The selection falls back
    // to the newest step still visible — that is a choice of what to READ — but `now` is a claim
    // about where the run IS, and it must not name a step the run has already left.
    fireEvent.change(grepField(), { target: { value: "api.ts" } });
    await waitFor(() => expect(spineTitles()).toEqual(["Oriented", "Implemented"]));
    expect(nowStep()).toBe("");
    expect(selectedStep()).toBe("Implemented");

    // Nor is the page still FOLLOWING a head it is not showing: the chip is offered, and it
    // clears the grep on its way back rather than scrolling to a spine the newest step is off.
    const latest = document.querySelector(".trlatest") as HTMLElement;
    expect(latest).toBeTruthy();
    fireEvent.click(latest);
    await waitFor(() => expect(nowStep()).toBe("Verified"));
    expect(grepField().value).toBe("");
    expect(selectedStep()).toBe("Verified");
  });

  it("holds still once the operator picks a step, and offers the way back to the playhead", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    expect(document.querySelector(".trlatest")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /Oriented/ }));
    await waitFor(() => expect(selectedStep()).toBe("Oriented"));
    // Reading an older step must not be yanked away by the next poll tick.
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    expect(selectedStep()).toBe("Oriented");
    const latest = document.querySelector(".trlatest") as HTMLElement;
    expect(latest).toBeTruthy();

    fireEvent.click(latest);
    await waitFor(() => expect(selectedStep()).toBe("Verified"));
    expect(document.querySelector(".trlatest")).toBeNull();
  });

  it("offers the same jump when the operator has scrolled up out of follow", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    scrollUp(600);
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeTruthy());
    // Back within the follow threshold, and the chip has nothing to offer again.
    scrollUp(0);
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeNull());
  });

  it("keeps a grep that is not hiding the playhead when the chip takes the page back", async () => {
    // The chip is offered for two independent reasons, and only one of them is the filter's
    // fault. Here the grep is showing the head perfectly well and the operator has simply
    // scrolled up: taking the page back to the bottom is all that was asked for, and wiping what
    // they typed on the way would be a loss they did not ask for at all.
    sizePage(() => 2000);
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    mountDetail([run(LIVE)]);
    await settleTrace();
    fireEvent.change(grepField(), { target: { value: "cargo" } });
    await waitFor(() => expect(spineTitles()).toEqual(["Verified"]));
    expect(nowStep()).toBe("Verified"); // the head is on the spine, so the page is still following

    scroller().scrollTop = 0;
    fireEvent.scroll(window);
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeTruthy());
    fireEvent.click(document.querySelector(".trlatest") as HTMLElement);

    await waitFor(() => expect(scroller().scrollTop).toBe(2000));
    expect(grepField().value).toBe("cargo");
    expect(spineTitles()).toEqual(["Verified"]);
  });

  it("keeps a live run pinned to the bottom as the stream appends to it", async () => {
    let height = 1000;
    sizePage(() => height);
    scroller().scrollTop = 200; // pinned to the bottom of a 1000px page in an 800px viewport
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();

    height = 1600; // the poll appended a step, and the page grew under the operator
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    // The view asks for the very bottom; a real engine clamps that to scrollHeight − clientHeight,
    // and jsdom, which lays nothing out, records the request verbatim.
    await waitFor(() => expect(scroller().scrollTop).toBe(1600));
    expect(document.querySelector(".trlatest")).toBeNull();
  });

  it("follows a live run opened at the top of a tall page", async () => {
    // The other side of the same rule, and the one an operator meets FIRST: nobody has scrolled
    // yet, so the position at mount belongs to the view they came from, not to this run. Reading
    // it as a choice is what used to open a live trace pinned to nothing and never follow again.
    let height = 2000;
    sizePage(() => height);
    scroller().scrollTop = 0; // not the bottom of a 2000px page — and not the operator's doing
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();

    height = 3000;
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    await waitFor(() => expect(spineTitles()).toContain("Verified"));
    expect(scroller().scrollTop).toBe(3000);
  });

  it("never drags a scrolled-up operator back down, however much the stream grows", async () => {
    let height = 2000;
    sizePage(() => height);
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    scroller().scrollTop = 0;
    fireEvent.scroll(window);
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeTruthy());

    height = 3000;
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    // Observed at the moment the GROWTH RENDER has actually happened — the appended step is on
    // the spine, so the follow effect has already run against the taller page. Asserting straight
    // after the poll instead proves only that the refetch had not landed yet, and passes whether
    // or not the guard is there.
    await waitFor(() => expect(spineTitles()).toContain("Verified"));
    expect(scroller().scrollTop).toBe(0);
    // And the chip is the way back: it re-takes the playhead AND the bottom of the page.
    fireEvent.click(document.querySelector(".trlatest") as HTMLElement);
    await waitFor(() => expect(scroller().scrollTop).toBe(3000));
  });

  it("follows the stream again once the chip has taken the page back", async () => {
    // The chip does not only move the page, it re-takes the pin: the NEXT growth has to follow.
    // A browser would confirm the chip's own scroll with a scroll event, but a rule that only
    // works because the engine volunteers one is a rule that does not work.
    let height = 2000;
    sizePage(() => height);
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();
    scroller().scrollTop = 0;
    fireEvent.scroll(window);
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeTruthy());

    fireEvent.click(document.querySelector(".trlatest") as HTMLElement);
    await waitFor(() => expect(scroller().scrollTop).toBe(2000));
    // Nothing left to be behind, so nothing left to offer.
    await waitFor(() => expect(document.querySelector(".trlatest")).toBeNull());

    height = 3000;
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    await waitFor(() => expect(spineTitles()).toContain("Verified"));
    expect(scroller().scrollTop).toBe(3000);
  });

  it("never drags a scrolled-up operator down when a filtered-in phase flips follow back on", async () => {
    // The other half of the same promise, and the half a detached scroll listener used to break:
    // while the page is not following, the operator's position is still theirs. The page is tall
    // only while the head is VISIBLE (the `now` badge is on the spine) — exactly when follow can
    // be on — so the growth and the follow-flip land in ONE commit, the way a real poll does.
    sizePage(() => (document.querySelector(".trstep.now") === null ? 2000 : 3000));
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run(LIVE)]);
    await settleTrace();

    // A grep for a step the run has not written yet hides every phase, the head included: the
    // page stops following, and the chip is offered.
    fireEvent.change(grepField(), { target: { value: "cargo" } });
    await waitFor(() => expect(spineTitles()).toEqual([]));
    expect(nowStep()).toBe("");
    expect(document.querySelector(".trlatest")).toBeTruthy();

    // The operator scrolls up to read. Nothing about follow being off makes this position less
    // real, and the follow rule has to observe it.
    scroller().scrollTop = 0;
    fireEvent.scroll(window);

    // The poll brings the phase the grep was looking for. It is the newest one, so it is BOTH
    // visible and the head: follow flips back on and the page grows in the same commit.
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMED_ON });
    await poll(["run-transcript", 547]);
    await waitFor(() => expect(spineTitles()).toEqual(["Verified"]));
    expect(nowStep()).toBe("Verified"); // follow really did flip back on

    expect(scroller().scrollTop).toBe(0);
    // Still where they left it, and the chip still says how to get back.
    expect(document.querySelector(".trlatest")).toBeTruthy();
  });

  it("never offers a playhead or a jump on a run that has finished", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: STREAMING });
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    expect(selectedStep()).toBe("Oriented");
    expect(document.querySelector(".trstep.now")).toBeNull();
    scrollUp(600);
    expect(document.querySelector(".trlatest")).toBeNull();
  });

  it("pulses the header while the run is streaming, and stops when it is not", async () => {
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(document.querySelector(".trhd .trpulse")).toBeTruthy());
    cleanup();
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(document.querySelector(".trhd")).toBeTruthy());
    expect(document.querySelector(".trhd .trpulse")).toBeNull();
  });

  it("reads its live turns and tokens from the 2s run-detail poll, not the cached history row", async () => {
    mountDetail([run({ ...LIVE, turns: 1, total_tokens: 1_000 })]);
    await waitFor(() => expect(document.querySelector(".trvitals")?.textContent).toContain("1 turn"));

    h.fetchRunDetail.mockImplementation(async (id: number) =>
      detailOf(run({ ...LIVE, id }), { turn_count: 9, total_tokens: 91_000 }),
    );
    await poll(["run-detail", 547]);
    await waitFor(() =>
      expect(document.querySelector(".trvitals")?.textContent).toContain("9 turns"),
    );
    expect(document.querySelector(".trvitals")?.textContent).toContain("91.0k");
  });

  it("ends the run on the poll's terminal outcome, which the cached history row cannot see", async () => {
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^stop$/i)).toBeTruthy());

    h.fetchRunDetail.mockImplementation(async (id: number) =>
      detailOf(run({ ...LIVE, id }), {
        outcome: "failed",
        ended_at: "2026-09-01T19:15:00Z",
        error: "cargo test exited 101",
      }),
    );
    await poll(["run-detail", 547]);
    await waitFor(() =>
      expect(document.querySelector(".trhd .pill")?.textContent).toContain("failed"),
    );
    expect(document.querySelector(".trbanner.fail")?.textContent).toContain("cargo test exited 101");
    expect(
      within(document.querySelector(".trhd .acts") as HTMLElement).queryByRole("button", {
        name: /^stop$/i,
      }),
    ).toBeNull();
  });
});

/**
 * Waits long enough for a send that WAS started to reach the mocked endpoint.
 *
 * `send.mutate` hands the body to react-query, which calls the mutation fn on a later tick — so a
 * synchronous `not.toHaveBeenCalled()` after a click proves only that the tick has not come round
 * yet, and passes with the refusal deleted. `refuses to send an empty message at all` carries the
 * positive control that this wait is long enough to be worth anything.
 */
async function flushSend() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 50));
  });
}

describe("the live run — the Message composer (§3A)", () => {
  it("sends an operator message to the running agent through the daemon's own endpoint", async () => {
    h.sendRunMessage.mockResolvedValue({ id: 3, identifier: "STUDIO-654", status: "sent" });
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    const button = action(/^message/i);
    expect(button.querySelector(".dep")).toBeNull();
    expect(button.getAttribute("aria-disabled")).toBeNull();

    fireEvent.click(button);
    const box = await screen.findByLabelText(/message the running agent/i);
    fireEvent.change(box, { target: { value: "btw the branch moved" } });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
    await waitFor(() => expect(h.sendRunMessage).toHaveBeenCalledExactlyOnceWith(547, "btw the branch moved"));
    await waitFor(() => expect((box as HTMLTextAreaElement).value).toBe(""));
  });

  it("surfaces a refused message rather than swallowing it", async () => {
    h.sendRunMessage.mockRejectedValue(new Error("too many pending operator messages for this run"));
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    fireEvent.click(action(/^message/i));
    fireEvent.change(await screen.findByLabelText(/message the running agent/i), {
      target: { value: "hi" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
    await waitFor(() =>
      expect(document.querySelector(".trmsg .acterr")?.textContent).toContain(
        "too many pending operator messages",
      ),
    );
  });

  it("refuses to send an empty message at all", async () => {
    h.sendRunMessage.mockResolvedValue({ id: 3, identifier: "STUDIO-654", status: "sent" });
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    fireEvent.click(action(/^message/i));
    const box = await screen.findByLabelText(/message the running agent/i);
    fireEvent.change(box, { target: { value: "   " } });
    // Both ways in. The button is the one a mouse takes; the textarea's Enter is the one that
    // reaches `submit` even when the button is disabled, and so the one that pins the refusal.
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
    fireEvent.keyDown(box, { key: "Enter" });
    await flushSend();
    expect(h.sendRunMessage).not.toHaveBeenCalled();

    // The positive control for that flush, on the very same wait: whitespace is what was refused,
    // not a send that simply had not reached the endpoint yet. (And Enter sends, trimmed.)
    fireEvent.change(box, { target: { value: "  say something  " } });
    fireEvent.keyDown(box, { key: "Enter" });
    await flushSend();
    expect(h.sendRunMessage).toHaveBeenCalledExactlyOnceWith(547, "say something");
  });

  it("keeps a half-written message on screen when the run ends underneath it", async () => {
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    // The header's action names the composer only while there is one to name — an `aria-controls`
    // pointing at an id the document does not carry is a dangling reference, not a hint.
    expect(action(/^message/i).getAttribute("aria-controls")).toBeNull();
    fireEvent.click(action(/^message/i));
    const box = (await screen.findByLabelText(/message the running agent/i)) as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "btw the branch moved" } });
    expect(action(/^message/i).getAttribute("aria-controls")).toBe("trmsg");

    // The run ends mid-compose, which the 2s poll is what notices.
    h.fetchRunDetail.mockImplementation(async (id: number) =>
      detailOf(run({ ...LIVE, id }), { outcome: "completed", ended_at: "2026-09-01T19:15:00Z" }),
    );
    await poll(["run-detail", 547]);
    await waitFor(() =>
      expect(document.querySelector(".trmsg .acterr")?.textContent).toContain(
        "there is no agent left to deliver this to",
      ),
    );
    // Discarding what the operator typed is not this view's call to make; refusing to send it is.
    expect(box.value).toBe("btw the branch moved");
    const send = screen.getByRole("button", { name: /^send$/i }) as HTMLButtonElement;
    expect(send.disabled).toBe(true);
    fireEvent.click(send);
    // And `submit` refuses it too, which is what the textarea's Enter — never disabled — asks for.
    fireEvent.keyDown(box, { key: "Enter" });
    await flushSend();
    expect(h.sendRunMessage).not.toHaveBeenCalled();
    expect(box.value).toBe("btw the branch moved");
  });

  // A finished run has no agent to reach, so the endpoint is not a dependency — it is inapplicable.
  it("names the dependency instead on a run that has already ended", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    expect(action(/^message/i).getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(action(/^message/i));
    expect(screen.queryByLabelText(/message the running agent/i)).toBeNull();
  });
});

describe("the failed run — jump to the failing step (§3B)", () => {
  it("selects the failing phase and expands the tool result that failed", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547, outcome: "failed", error: "cargo test exited 101" })]);
    await settleTrace();
    expect(document.querySelector(".trbanner.fail")).toBeTruthy();
    expect(selectedStep()).toBe("Oriented");

    fireEvent.click(screen.getByRole("button", { name: /jump to failing step/i }));
    await waitFor(() => expect(selectedStep()).toBe("Verified"));
    const top = document.querySelector(".trcard.err .top") as HTMLElement;
    expect(top.getAttribute("aria-expanded")).toBe("true");
    expect(document.querySelector(".trcard.err .out")?.textContent).toContain("1 test failed");

    // And it re-opens one the operator folded away — the jump is an instruction, not a default.
    fireEvent.click(top);
    expect(top.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: /jump to failing step/i }));
    await waitFor(() =>
      expect(document.querySelector(".trcard.err .top")?.getAttribute("aria-expanded")).toBe("true"),
    );
  });

  it("offers no jump when the failure left no failing step in the transcript", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "text", text: "Nothing ran." })],
    });
    mountDetail([run({ id: 547, outcome: "failed", error: "the worker crashed before dispatch" })]);
    await settleTrace();
    expect(document.querySelector(".trbanner.fail")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /jump to failing step/i })).toBeNull();
  });

  it("clears a filter that hides the failing phase — the jump is an instruction, not a wish", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547, outcome: "failed", error: "cargo test exited 101" })]);
    await settleTrace();

    // An operator who has been poking the chips on a failed run and then reads the banner: Edits
    // hides the phase that actually failed, and `selected` discards a pick the filter hides.
    fireEvent.click(screen.getByRole("button", { name: "Edits" }));
    await waitFor(() => expect(spineTitles()).toEqual(["Implemented"]));

    fireEvent.click(screen.getByRole("button", { name: /jump to failing step/i }));
    await waitFor(() => expect(selectedStep()).toBe("Verified"));
    // The failing call is on screen AND open — a jump that selects nothing the operator can see
    // is the same no-op as one that selects nothing at all.
    expect(document.querySelector(".trcard.err .top")?.getAttribute("aria-expanded")).toBe("true");
    expect(spineTitles()).toEqual(["Oriented", "Implemented", "Verified", "Coordinated"]);

    // The grep is the other half of the filter, and hides a phase just as completely.
    fireEvent.change(grepField(), { target: { value: "export interface" } });
    await waitFor(() => expect(spineTitles()).toEqual(["Oriented"]));
    fireEvent.click(screen.getByRole("button", { name: /jump to failing step/i }));
    await waitFor(() => expect(selectedStep()).toBe("Verified"));
    expect(grepField().value).toBe("");
    expect(document.querySelector(".trcard.err .top")?.getAttribute("aria-expanded")).toBe("true");
  });

  it("keeps the jump out of a stopped run's amber banner, even when a step did fail", async () => {
    // The COMPLETED transcript's `npm test` failed — an operator stopping a run while a test is
    // red is not an exotic input, so the gate has to be the BANNER's tone and not merely whether
    // the trace holds a failing step. §3B gives the jump to the failed banner alone ("Stopped ->
    // amber reason + Resume"), and `.trbanner .jump` is tinted `--bad`, which an amber banner is
    // not.
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([run({ id: 547, outcome: "stopped", error: "stopped by the operator" })]);
    await settleTrace();
    expect(document.querySelector(".trbanner.stop")).toBeTruthy();
    // The failing step really is in this trace: the spine marks it red.
    expect(document.querySelector(".trstep.err")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /jump to failing step/i })).toBeNull();
    // What a stop offers instead.
    expect(action(/^Resume$/)).toBeTruthy();
  });
});

describe("the attempt relay — the handoff baton (§3C/§6)", () => {
  const RELAY = [
    run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
    run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
  ];

  it("marks the baton into the attempt being read, and out of the one before it", async () => {
    mountDetail(RELAY);
    await settleTrace();
    await waitFor(() =>
      expect(document.querySelector(".trbaton.in")?.textContent).toContain("run 522 → run 547"),
    );
    expect(document.querySelector(".trbaton.out")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "run 522" }));
    await waitFor(() => expect(document.querySelector(".trbaton.out")).toBeTruthy());
    expect(document.querySelector(".trbaton.out")?.textContent).toContain("run 522 → run 547");
    expect(document.querySelector(".trbaton.in")).toBeNull();
  });

  // The console can name ONE teammate per ticket, not one per run, so today's relay reads as the
  // run handoff it is rather than inventing a second name. See `runTeammate`.
  it("carries the teammate it can name beside the relay", async () => {
    mountDetail(RELAY);
    await waitFor(() =>
      expect(document.querySelector(".trbaton.in")?.textContent).toContain("alice · run 522"),
    );
  });

  // NOT tested through the view: a baton naming two DIFFERENT teammates. Nothing the daemon
  // serves can produce that payload — `/issues/{id}/history` matches `issue_identifier` exactly
  // (`crates/store/src/sqlite.rs`), so every run in one selector shares a key, and one key
  // resolves to one name. `relayBatons`' own unit tests pin the two-name branch, and slice 5's
  // per-run identity is what will reach it. A view test over a history the store never writes
  // would be green and prove nothing.

  it("gives a ticket with a single run no baton in either direction", async () => {
    mountDetail([run({ id: 547 })]);
    await settleTrace();
    expect(document.querySelector(".trbaton")).toBeNull();
  });

  // The one run the console CAN attribute to a teammate per-run: a ticketless review carries its
  // reviewer in its own key, and the header and the inspector must not disagree about who it was.
  it("attributes a ticketless review run to the reviewer named in its own key", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: COMPLETED });
    mountDetail([
      run({
        id: 547,
        issue_id: "pr:makewhatis/rhapsody#12@jimmy",
        issue_identifier: "pr:makewhatis/rhapsody#12@jimmy",
        title: "Review makewhatis/rhapsody#12 at 9f1c0aa",
      }),
    ]);
    await settleTrace();
    expect(document.querySelector(".trhd .who2")?.textContent).toContain("jimmy");
    expect(document.querySelector(".trinsp h4")?.textContent).toBe("Oriented — what jimmy did");
  });
});
