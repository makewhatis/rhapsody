// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { readFileSync } from "node:fs";
import path from "node:path";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { LogEntry, RunDetail, RunSummary, StateResponse } from "@/lib/api";
import type { PullRequestView } from "@/lib/console-job-detail";

// STUDIO-742 — the "Trace" run detail's three zones (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §3), replacing STUDIO-683's summary strip and
// flat runs list. The §4 side cards it did NOT replace (the PR dependency card, the room slice,
// the ticket's memory) keep their boxes from STUDIO-681 §10 sub-ticket 2 until the watch-tabs
// rail takes them over in slice 4.

const h = vi.hoisted(() => ({
  fetchIssueHistory: vi.fn(),
  fetchRunDetail: vi.fn(),
  fetchRunTranscript: vi.fn(),
  sendRunMessage: vi.fn(),
  fetchState: vi.fn(),
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
  fetchLinearIdentity: vi.fn(),
  stopRun: vi.fn(),
  resumeRun: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchIssueHistory: h.fetchIssueHistory,
    fetchRunDetail: h.fetchRunDetail,
    fetchRunTranscript: h.fetchRunTranscript,
    sendRunMessage: h.sendRunMessage,
    fetchState: h.fetchState,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchTeamsRoom: h.fetchTeamsRoom,
    fetchTeamsRecall: h.fetchTeamsRecall,
    fetchLinearIdentity: h.fetchLinearIdentity,
    stopRun: h.stopRun,
    resumeRun: h.resumeRun,
    fetchVersion: vi.fn(async () => ({
      version: "v0.4.0",
      commit: "abc",
      built_at: "",
      teams_enabled: true,
    })),
  };
});

const { JobDetailView, PullRequestCard } = await import("./JobDetailView");

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
  h.fetchTeamsOverview.mockResolvedValue({
    enabled: true,
    manager_mode: "labels",
    default_identity: "",
    backend: "local",
    roster: [
      { name: "alice", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: ["STUDIO-654"] },
    ],
  });
  h.fetchTeamsRoom.mockResolvedValue({ messages: [], skipped: [] });
  h.fetchTeamsRecall.mockResolvedValue({ identity: "alice", facts: [], skipped: [] });
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
// The §4 side cards STUDIO-742 did not replace — retained until slice 4's watch-tabs rail.
// ---------------------------------------------------------------------------------------------
describe("the pull-request card (§4)", () => {
  function card(pr: PullRequestView | null) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <PullRequestCard pr={pr} />
      </QueryClientProvider>,
    );
  }

  const checks: PullRequestView["checks"] = [
    { name: "lint", state: "pass", detail: "passed" },
    { name: "test", state: "fail", detail: "failing" },
    { name: "web", state: "pending", detail: "running" },
  ];

  // Box 2.11
  it("lists each CI check with its pass/fail state and the blocked note", () => {
    card({ number: "#230", url: "https://example/230", draft: false, behind: 0, checks });
    expect([...document.querySelectorAll(".chk")].map((el) => el.textContent)).toEqual([
      "lintpassed",
      "testfailing",
      "webrunning",
    ]);
    expect(document.querySelector(".chk.ok")?.textContent).toContain("lint");
    expect(document.querySelector(".chk.bad")?.textContent).toContain("test");
    expect(document.querySelector(".chk.pending")?.textContent).toContain("web");
    expect(document.querySelector(".prnote")?.textContent).toContain("Blocked — 1 failing check.");
  });

  it("reports a mergeable PR", () => {
    card({
      number: "#230",
      url: "https://example/230",
      draft: false,
      behind: 0,
      checks: [{ name: "lint", state: "pass", detail: "passed" }],
    });
    expect(document.querySelector(".prnote")?.textContent).toContain("mergeable");
    expect(screen.getByText("#230")).toBeTruthy();
  });

  // §11 — the card must name the missing data source, not fabricate a PR.
  it("names the dependency when no PR data exists, which is the shipped state", () => {
    card(null);
    expect(screen.getByText(/the daemon serves no PR endpoint yet/i)).toBeTruthy();
    expect(document.querySelector(".chk")).toBeNull();
  });
});

describe("the side column's teams cards (§4)", () => {
  // STUDIO-739 — the room slice and the ticket's memory carry the same agent prose the room and
  // the memory page do, so they render it the same way.
  it("renders the markdown in a room post and in a retained fact", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRoom.mockResolvedValue({
      messages: [
        { id: "f:1", from: "alice", to: "*", at: "2026-09-01T16:37:00Z", body: "STUDIO-654 is **up for review**.", refs: ["STUDIO-654"] },
      ],
      skipped: [],
    });
    h.fetchTeamsRecall.mockResolvedValue({
      identity: "alice",
      facts: [
        { id: "1", identity: "alice", document_id: "", ticket: "STUDIO-654", commit_sha: "", pr: "", run_id: "547", at: "", state: "valid", reason: "", content: "Run `make fixtures` first." },
      ],
      skipped: [],
    });

    await waitFor(() => expect(document.querySelector(".mcard strong")).toBeTruthy());
    expect(document.querySelector(".mcard strong")?.textContent).toBe("up for review");
    await waitFor(() =>
      expect([...document.querySelectorAll(".mcard code")].map((c) => c.textContent)).toContain(
        "make fixtures",
      ),
    );
  });

  it("shows the room posts and memory facts that reference this ticket", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 1, generated_at: "", entries: [] });
    mountDetail([run({ id: 1 })]);
    h.fetchTeamsRoom.mockResolvedValue({
      messages: [
        { id: "f:1", from: "operator", to: "*", at: "2026-09-01T16:37:00Z", body: "Who can review this?", refs: ["STUDIO-654"] },
        { id: "f:2", from: "alice", to: "*", at: "2026-09-01T19:11:00Z", body: "Unrelated post", refs: [] },
      ],
      skipped: [],
    });
    h.fetchTeamsRecall.mockResolvedValue({
      identity: "alice",
      facts: [
        { id: "1", identity: "alice", document_id: "", ticket: "STUDIO-654", commit_sha: "", pr: "", run_id: "547", at: "", state: "valid", reason: "", content: "Grep DeepSeek after a config rebase." },
        { id: "2", identity: "alice", document_id: "", ticket: "OTHER-1", commit_sha: "", pr: "", run_id: "1", at: "", state: "valid", reason: "", content: "Not this ticket." },
      ],
      skipped: [],
    });

    await waitFor(() => expect(screen.getByText("Who can review this?")).toBeTruthy());
    expect(screen.queryByText("Unrelated post")).toBeNull();
    await waitFor(() =>
      expect(screen.getByText("Grep DeepSeek after a config rebase.")).toBeTruthy(),
    );
    expect(screen.queryByText("Not this ticket.")).toBeNull();
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
    expect(rule(".rh-console .trinsp")).toMatch(/min-width:\s*0/);
    expect(rule(".rh-console .trrc .body")).toMatch(/min-width:\s*0/);
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
    expect(scroller().scrollTop).toBe(0);
    // And the chip is the way back: it re-takes the playhead AND the bottom of the page.
    fireEvent.click(document.querySelector(".trlatest") as HTMLElement);
    await waitFor(() => expect(scroller().scrollTop).toBe(3000));
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
    mountDetail([run(LIVE)]);
    await waitFor(() => expect(action(/^message/i)).toBeTruthy());
    fireEvent.click(action(/^message/i));
    fireEvent.change(await screen.findByLabelText(/message the running agent/i), {
      target: { value: "   " },
    });
    fireEvent.click(screen.getByRole("button", { name: /^send$/i }));
    expect(h.sendRunMessage).not.toHaveBeenCalled();
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

  it("keeps the jump out of a stopped run's amber banner when nothing failed", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: STREAMING,
    });
    mountDetail([run({ id: 547, outcome: "stopped", error: "stopped by the operator" })]);
    await settleTrace();
    expect(document.querySelector(".trbanner.stop")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /jump to failing step/i })).toBeNull();
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
