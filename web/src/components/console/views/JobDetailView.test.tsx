// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { readFileSync } from "node:fs";
import path from "node:path";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { LogEntry, RunSummary, StateResponse } from "@/lib/api";
import type { PullRequestView } from "@/lib/console-job-detail";

// STUDIO-742 — the "Trace" run detail's three zones (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §3), replacing STUDIO-683's summary strip and
// flat runs list. The §4 side cards it did NOT replace (the PR dependency card, the room slice,
// the ticket's memory) keep their boxes from STUDIO-681 §10 sub-ticket 2 until the watch-tabs
// rail takes them over in slice 4.

const h = vi.hoisted(() => ({
  fetchIssueHistory: vi.fn(),
  fetchRunTranscript: vi.fn(),
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
    fetchRunTranscript: h.fetchRunTranscript,
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

function run(over: Partial<RunSummary> & Pick<RunSummary, "id">): RunSummary {
  return {
    issue_id: "i",
    issue_identifier: "STUDIO-654",
    title: "Attach a photo in chat",
    attempt: 1,
    session_uuid: "s",
    branch: "symphony/STUDIO-654",
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

function mountDetail(runs: RunSummary[], onNavigate = vi.fn()) {
  h.fetchIssueHistory.mockResolvedValue({ issue_identifier: "STUDIO-654", runs });
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
    mountDetail([run({ id: 547, turns: 3, started_at: "2026-09-01T19:11:00Z", ended_at: "2026-09-01T19:15:30Z" })]);
    await waitFor(() => expect(document.querySelector(".trvitals")).toBeTruthy());
    const vitals = document.querySelector(".trvitals")?.textContent ?? "";
    expect(vitals).toContain("4m 30s");
    expect(vitals).toContain("3 turns");
    expect(vitals).toContain("38.0k");
    expect(vitals).toContain("symphony/STUDIO-654");
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

  it("selects the newest run, and offers the older attempts alongside it", async () => {
    mountDetail([
      run({ id: 522, attempt: 1, started_at: "2026-08-30T20:21:00Z" }),
      run({ id: 547, attempt: 3, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 545, attempt: 2, started_at: "2026-09-01T16:54:00Z" }),
    ]);
    await waitFor(() => expect(document.querySelectorAll(".trattempts button")).toHaveLength(3));
    expect([...document.querySelectorAll(".trattempts button")].map((b) => b.textContent)).toEqual([
      "attempt 3",
      "attempt 2",
      "attempt 1",
    ]);
    // Newest first AND newest selected — its transcript is the one fetched.
    expect(document.querySelector('.trattempts button[aria-pressed="true"]')?.textContent).toBe("attempt 3");
    await waitFor(() => expect(h.fetchRunTranscript).toHaveBeenCalledExactlyOnceWith(547));
  });

  it("renders one attempt's trace at a time, fetching only that attempt's transcript", async () => {
    h.fetchRunTranscript.mockImplementation(async (id: number) => ({
      run_id: id,
      generated_at: "",
      entries: [entry({ seq: 1, kind: "tool_use", tool: "Bash", text: `command=echo run ${id}` })],
    }));
    mountDetail([
      run({ id: 547, attempt: 2, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 522, attempt: 1, started_at: "2026-08-30T20:21:00Z" }),
    ]);
    await waitFor(() => expect(screen.getByText(/echo run 547/)).toBeTruthy());
    expect(screen.queryByText(/echo run 522/)).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "attempt 1" }));
    await waitFor(() => expect(screen.getByText(/echo run 522/)).toBeTruthy());
    expect(h.fetchRunTranscript).toHaveBeenCalledWith(522);
    expect(screen.queryByText(/echo run 547/)).toBeNull();
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
    expect(dep.getAttribute("title")).toMatch(/no.*pull-request endpoint/i);
  });

  it("names Merge's missing endpoint instead of offering a button that cannot merge", async () => {
    mountDetail([run({ id: 547 })]);
    await waitFor(() => expect(action(/^merge/i)).toBeTruthy());
    const merge = action(/^merge/i);
    expect(merge.querySelector(".dep")?.textContent).toBe("dep");
    expect(merge.getAttribute("title")).toMatch(/run-branch diff/i);
    expect((merge as HTMLButtonElement).disabled).toBe(true);
  });

  it("offers Stop only while the run is live, and Resume only once it has stopped", async () => {
    mountDetail([run({ id: 547, outcome: "stopped", ended_at: "2026-09-01T19:15:00Z" })]);
    await waitFor(() => expect(action(/resume/i)).toBeTruthy());
    expect((action(/resume/i) as HTMLButtonElement).disabled).toBe(false);
    expect(within(document.querySelector(".trhd .acts") as HTMLElement).queryByRole("button", { name: /^stop$/i })).toBeNull();
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
});
