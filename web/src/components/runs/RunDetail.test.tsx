// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { IssueHistoryResponse, LinearProject, LogEntry, RunDetail as RunDetailDTO, RunSummary } from "@/lib/api";

const h = vi.hoisted(() => ({
  runDetail: vi.fn<(id: number) => Promise<RunDetailDTO>>(),
  transcript: vi.fn(),
  issueHistory: vi.fn<(id: string) => Promise<IssueHistoryResponse>>(),
  stopRun: vi.fn(),
  resumeRun: vi.fn(),
  runMessages: vi.fn(),
  sendRunMessage: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchRunDetail: (id: number) => h.runDetail(id),
    fetchRunTranscript: (id: number) => h.transcript(id),
    fetchIssueHistory: (id: string) => h.issueHistory(id),
    stopRun: (id: number) => h.stopRun(id),
    resumeRun: (id: number) => h.resumeRun(id),
    fetchRunMessages: (id: number) => h.runMessages(id),
    sendRunMessage: (id: number, text: string) => h.sendRunMessage(id, text),
  };
});

// Imported after vi.mock so the mocked module is in effect.
import { RunDetail } from "@/components/runs/RunDetail";
import { ToastProvider } from "@/components/shell/Toast";

const PROJECTS: LinearProject[] = [
  { id: "p1", name: "Infrastructure", slug: "symphony-infra-tasks-9c29e9ade060", team: "INF", color: "#34d399" },
];

function detail(over: Partial<RunDetailDTO> = {}): RunDetailDTO {
  return {
    run_id: 1,
    issue_id: "id-1",
    issue_identifier: "INF-231",
    title: "Sign & notarize the dmg",
    project: "symphony-infra-tasks-9c29e9ade060",
    repo: "git@github.com:example/demo-repo.git",
    attempt: 0,
    outcome: "completed",
    live: false,
    issue_state: "",
    last_codex_event: "",
    turn_count: 14,
    input_tokens: 428_000,
    output_tokens: 612_000,
    total_tokens: 1_040_000,
    usage_estimated: false,
    started_at: "2026-06-06T14:58:00Z",
    ended_at: "2026-06-06T15:10:00Z",
    last_event_at: "",
    error: "",
    recent_events: [],
    generated_at: "",
    ...over,
  };
}

function transcript(entries: LogEntry[]) {
  return { run_id: 1, entries, generated_at: "" };
}

// summary builds a per-attempt history row (for the Branch meta cell + the run-history panel).
function summary(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 1,
    issue_id: "x",
    issue_identifier: "INF-231",
    title: "t",
    attempt: 0,
    session_uuid: "",
    branch: "inf/231",
    project_slug: "symphony-infra-tasks-9c29e9ade060",
    repo: "",
    started_at: "2026-06-06T14:58:00Z",
    ended_at: "2026-06-06T15:10:00Z",
    outcome: "completed",
    turns: 14,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 1_040_000,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

beforeEach(() => {
  h.runDetail.mockResolvedValue(detail());
  h.transcript.mockResolvedValue(transcript([]));
  h.issueHistory.mockResolvedValue({ issue_identifier: "INF-231", runs: [] });
  h.stopRun.mockResolvedValue({ identifier: "INF-231", moved_to: "Backlog" });
  h.resumeRun.mockResolvedValue({ identifier: "INF-231", moved_to: "Todo" });
  h.runMessages.mockResolvedValue([]);
  h.sendRunMessage.mockResolvedValue({ id: 1, identifier: "INF-231", status: "sent" });
});

afterEach(() => {
  cleanup();
  h.runDetail.mockReset();
  h.transcript.mockReset();
  h.issueHistory.mockReset();
  h.stopRun.mockReset();
  h.resumeRun.mockReset();
  h.runMessages.mockReset();
  h.sendRunMessage.mockReset();
});

function renderDetail(runId = 1, onBack = () => {}, onSelectRun = () => {}, maxTurns: number | undefined = 20) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <RunDetail runId={runId} projects={PROJECTS} maxTurns={maxTurns} onBack={onBack} onSelectRun={onSelectRun} />
      </ToastProvider>
    </QueryClientProvider>,
  );
}

describe("RunDetail header + meta", () => {
  it("renders the run key heading, title, header agent · attempt, and the six-cell meta strip", async () => {
    renderDetail();
    expect(await screen.findByRole("heading", { name: "INF-231" })).toBeTruthy();
    expect(screen.getByText("Sign & notarize the dmg")).toBeTruthy();
    // the header resolves the agent from the project slug (never the raw config slug id), 1-indexed
    // "attempt 1" for a clean attempt-0 run.
    expect(screen.getByText(/Infrastructure · attempt 1/)).toBeTruthy();
    expect(screen.queryByText("symphony-infra-tasks-9c29e9ade060")).toBeNull();
    // the six meta labels — State/Project/Attempt moved to the header, Branch is new.
    for (const label of ["Repo", "Turn", "Tokens", "Started", "Duration", "Branch"]) {
      expect(screen.getByText(label)).toBeTruthy();
    }
    expect(screen.getByText("example/demo-repo")).toBeTruthy(); // Repo meta cell
    expect(screen.getByText("14/20")).toBeTruthy(); // Turn cell with the config-driven /max
  });

  it("shows the header attempt 1-indexed for a retried run", async () => {
    h.runDetail.mockResolvedValue(detail({ attempt: 2 }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    // attempt 2 is the 0-indexed counter, displayed 1-indexed as the third dispatch.
    expect(screen.getByText(/Infrastructure · attempt 3/)).toBeTruthy();
    expect(screen.queryByText(/attempt 2/)).toBeNull();
  });

  it("shows the branch (from the run's history row) in the Branch meta cell", async () => {
    h.issueHistory.mockResolvedValue({
      issue_identifier: "INF-231",
      runs: [summary({ id: 1, branch: "inf/231-sign" })],
    });
    renderDetail();
    expect(await screen.findByText("inf/231-sign")).toBeTruthy();
  });

  it("shows only Open ticket for a finished (completed) run — no Stop or Resume", async () => {
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.getByRole("button", { name: /Open ticket/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Stop/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("inline-confirms Stop for a running run and calls stopRun on the second click", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    // first click arms the inline confirm on the button itself…
    fireEvent.click(screen.getByRole("button", { name: "Stop run" }));
    const confirm = await screen.findByRole("button", { name: "Stop INF-231?" });
    // …second click fires the stop.
    fireEvent.click(confirm);
    await waitFor(() => expect(h.stopRun).toHaveBeenCalledWith(1));
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("toasts the move-failure when the agent was killed but the ticket couldn't be moved", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.stopRun.mockResolvedValue({ identifier: "INF-231", move_error: "no backlog state for team" });
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    fireEvent.click(screen.getByRole("button", { name: "Stop run" }));
    fireEvent.click(await screen.findByRole("button", { name: "Stop INF-231?" }));
    await waitFor(() => expect(h.stopRun).toHaveBeenCalledWith(1));
    expect(await screen.findByText(/couldn't move the ticket: no backlog state for team/i)).toBeTruthy();
  });

  it("shows a primary Resume for a stopped run and calls resumeRun", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "stopped", ended_at: "2026-06-08T00:00:00Z", live: false }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    await waitFor(() => expect(h.resumeRun).toHaveBeenCalledWith(1));
    expect(screen.queryByRole("button", { name: /Stop/ })).toBeNull();
  });

  it("renders the amber STOPPED banner (NOT a red FAILED box) for a user-stopped run", async () => {
    h.runDetail.mockResolvedValue(
      detail({ outcome: "stopped", error: "stopped by user", ended_at: "2026-06-08T00:00:00Z", live: false }),
    );
    renderDetail();
    expect(await screen.findByText("STOPPED")).toBeTruthy();
    expect(screen.getByText(/Resume dispatches a fresh agent/)).toBeTruthy();
    expect(screen.queryByText("FAILED")).toBeNull();
  });

  it("renders an 'est.' badge with an Estimated tooltip when usage_estimated is true", async () => {
    h.runDetail.mockResolvedValue(detail({ usage_estimated: true }));
    renderDetail();
    const badge = await screen.findByText("est.");
    expect(badge.getAttribute("title")).toContain("Estimated");
  });

  it("does not render the 'est.' badge for an authoritative finished run", async () => {
    renderDetail();
    // total_tokens 1_040_000 -> "1.0M" in the Tokens meta cell
    await waitFor(() => expect(screen.getByText("1.0M")).toBeTruthy());
    expect(screen.queryByText("est.")).toBeNull();
  });

  it("renders the 'est.' badge for a live run even when usage_estimated is false", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, usage_estimated: false, ended_at: "" }));
    renderDetail();
    expect(await screen.findByText("est.")).toBeTruthy();
  });

  it("calls onBack when the Jobs button is clicked", async () => {
    const onBack = vi.fn();
    renderDetail(1, onBack);
    fireEvent.click(await screen.findByRole("button", { name: /Jobs/ }));
    expect(onBack).toHaveBeenCalledOnce();
  });

  it("renders the FAILED banner when a failed run carries an error", async () => {
    h.runDetail.mockResolvedValue(
      detail({ outcome: "failed", error: "clone --bare: git@github.com: Permission denied (publickey)." }),
    );
    renderDetail();
    expect(await screen.findByText("FAILED")).toBeTruthy();
    expect(screen.getByText(/Permission denied \(publickey\)/)).toBeTruthy();
  });

  it("omits the failure banner when the run has no error", async () => {
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("FAILED")).toBeNull();
  });

  it("keeps the detail queries idle when disabled (e.g. under the Wails host)", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={client}>
        <ToastProvider>
          <RunDetail runId={1} projects={PROJECTS} enabled={false} onBack={() => {}} onSelectRun={() => {}} />
        </ToastProvider>
      </QueryClientProvider>,
    );
    // a still-back-navigable shell renders, but no daemon requests fire
    expect(await screen.findByRole("button", { name: /Jobs/ })).toBeTruthy();
    expect(h.runDetail).not.toHaveBeenCalled();
    expect(h.transcript).not.toHaveBeenCalled();
    expect(h.issueHistory).not.toHaveBeenCalled();
  });
});

describe("RunDetail operator messages (INF-250)", () => {
  it("hides the composer for a finished run with no messages", async () => {
    renderDetail(); // outcome completed, no messages
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByPlaceholderText(/Send a message to the running agent/)).toBeNull();
    expect(screen.queryByText("Operator messages")).toBeNull();
  });

  it("shows the composer while running and sends a trimmed message, clearing the input", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    renderDetail();
    const input = (await screen.findByPlaceholderText(
      /Send a message to the running agent/,
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "  reply DONE and stop  " } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(h.sendRunMessage).toHaveBeenCalledWith(1, "reply DONE and stop"));
    await waitFor(() => expect(input.value).toBe(""));
  });

  it("renders the message timeline with status chips (sent / delivered · turn N / expired)", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.runMessages.mockResolvedValue([
      { id: 1, run_id: 1, body: "delivered one", created_at_ms: 1, status: "delivered", delivered_turn: 2 },
      { id: 2, run_id: 1, body: "pending one", created_at_ms: 2, status: "sent" },
      { id: 3, run_id: 1, body: "missed one", created_at_ms: 3, status: "expired" },
    ]);
    renderDetail();
    expect(await screen.findByText("delivered one")).toBeTruthy();
    expect(screen.getByText("delivered · turn 2")).toBeTruthy();
    expect(screen.getByText("pending one")).toBeTruthy();
    expect(screen.getByText("sent")).toBeTruthy();
    expect(screen.getByText("missed one")).toBeTruthy();
    expect(screen.getByText("expired")).toBeTruthy();
  });

  it("surfaces a send error inline (e.g. backlog_full) without clearing the input", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.sendRunMessage.mockRejectedValue(new Error("too many pending operator messages for this run"));
    renderDetail();
    const input = (await screen.findByPlaceholderText(
      /Send a message to the running agent/,
    )) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "one more" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(await screen.findByText(/too many pending operator messages/)).toBeTruthy();
    expect(input.value).toBe("one more"); // kept so the operator can retry
  });

  it("shows the message history on a finished run but no composer", async () => {
    h.runMessages.mockResolvedValue([
      { id: 1, run_id: 1, body: "delivered earlier", created_at_ms: 1, status: "delivered", delivered_turn: 1 },
    ]);
    renderDetail(); // outcome completed
    expect(await screen.findByText("delivered earlier")).toBeTruthy();
    expect(screen.queryByPlaceholderText(/Send a message to the running agent/)).toBeNull();
  });
});

describe("RunDetail transcript", () => {
  const entries: LogEntry[] = [
    { seq: 1, kind: "event", tool: "", text: "turn 1 · 14:10:12" },
    { seq: 2, kind: "text", tool: "", text: "I'll start by **understanding** the state." },
    { seq: 3, kind: "tool_use", tool: "mcp__claude_ai_Linear__get_issue", text: "id=INF-231" },
    { seq: 4, kind: "tool_result", tool: "", text: "{\"ok\":true}" },
  ];

  it("renders dividers, bolded text, MCP tool chips and tool output", async () => {
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    expect(await screen.findByText("turn 1 · 14:10:12")).toBeTruthy();
    // markdown bold -> <strong>
    const strong = screen.getByText("understanding");
    expect(strong.tagName.toLowerCase()).toBe("strong");
    // MCP tool chip shows the full mcp__ name
    expect(screen.getByText(/mcp__claude_ai_Linear__get_issue/)).toBeTruthy();
    expect(screen.getByText("id=INF-231")).toBeTruthy();
    expect(screen.getByText(/"ok":true/)).toBeTruthy();
  });

  it("renders inline `code` spans in prose as <code>", async () => {
    h.transcript.mockResolvedValue(transcript([{ seq: 1, kind: "text", tool: "", text: "run `make lint` before pushing" }]));
    renderDetail();
    const code = await screen.findByText("make lint");
    expect(code.tagName.toLowerCase()).toBe("code");
  });

  it("collapses a long prose entry behind 'Show more' and expands it", async () => {
    const long = Array.from({ length: 30 }, (_, i) => `line ${i} of the agent's reasoning`).join("\n");
    h.transcript.mockResolvedValue(transcript([{ seq: 1, kind: "text", tool: "", text: long }]));
    renderDetail();
    const more = await screen.findByRole("button", { name: "Show more…" });
    fireEvent.click(more);
    expect(screen.getByRole("button", { name: "Show less" })).toBeTruthy();
  });

  it("marks only mcp__ tools with the MCP chip attribute", async () => {
    h.transcript.mockResolvedValue(
      transcript([
        { seq: 1, kind: "tool_use", tool: "mcp__claude_ai_Linear__get_issue", text: "id=INF-231" },
        { seq: 2, kind: "tool_use", tool: "Bash", text: "ls -la" },
      ]),
    );
    const { container } = renderDetail();
    await screen.findByText(/mcp__claude_ai_Linear__get_issue/);
    // exactly one tool chip is flagged as MCP; the Bash chip is not
    expect(container.querySelectorAll('[data-mcp="true"]').length).toBe(1);
  });

  it("shows the streaming chip + blinking cursor while running", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.transcript.mockResolvedValue(transcript(entries));
    const { container } = renderDetail();
    await screen.findByText("Agent output");
    expect(screen.getByText("streaming")).toBeTruthy();
    expect(container.querySelector("[data-live-cursor]")).toBeTruthy();
  });

  it("shows the final footer (no cursor) for a finished run", async () => {
    h.transcript.mockResolvedValue(transcript(entries));
    const { container } = renderDetail();
    await screen.findByText("turn 1 · 14:10:12");
    expect(screen.getByText(/· final/)).toBeTruthy();
    expect(screen.queryByText("streaming")).toBeNull();
    expect(container.querySelector("[data-live-cursor]")).toBeNull();
  });

  it("does not flash 'No transcript' while the transcript query is still loading", async () => {
    h.transcript.mockReturnValue(new Promise<ReturnType<typeof transcript>>(() => {}));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("No transcript for this run.")).toBeNull();
  });

  it("shows 'No transcript' once the transcript query resolves empty for a finished run", async () => {
    h.transcript.mockResolvedValue(transcript([]));
    renderDetail();
    expect(await screen.findByText("No transcript for this run.")).toBeTruthy();
  });

  it("keeps streaming chrome out for a cached running run when polling is disabled", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    client.setQueryData(["run-detail", 1], detail({ outcome: "running", live: true, ended_at: "" }));
    client.setQueryData(["run-transcript", 1], transcript(entries));
    const { container } = render(
      <QueryClientProvider client={client}>
        <ToastProvider>
          <RunDetail runId={1} projects={PROJECTS} enabled={false} onBack={() => {}} onSelectRun={() => {}} />
        </ToastProvider>
      </QueryClientProvider>,
    );
    // the transcript renders from cache (so the panel is mounted), but the live chrome is gated off
    expect(await screen.findByText("turn 1 · 14:10:12")).toBeTruthy();
    expect(screen.getByText(/· final/)).toBeTruthy();
    expect(screen.queryByText("streaming")).toBeNull();
    expect(container.querySelector("[data-live-cursor]")).toBeNull();
  });
});

describe("RunDetail follow mode", () => {
  const entries: LogEntry[] = [
    { seq: 1, kind: "event", tool: "", text: "turn 1 · 14:10:12" },
    { seq: 2, kind: "text", tool: "", text: "streaming line" },
  ];

  it("pauses following on an upward scroll and resumes via jump-to-latest", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.transcript.mockResolvedValue(transcript(entries));
    const { container } = renderDetail();
    await screen.findByText("streaming line");
    // starts following: the header shows "following ↓", no jump-to-latest affordance yet
    expect(screen.getByText("following ↓")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /jump to latest/ })).toBeNull();

    const scroller = container.querySelector("[data-transcript-scroll]") as HTMLElement;
    // jsdom doesn't lay out, so supply the scroll geometry the follow decision reads.
    Object.defineProperty(scroller, "scrollHeight", { value: 1000, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 300, configurable: true });
    scroller.scrollTop = 0; // scrolled to the top → far from the bottom
    fireEvent.scroll(scroller);

    expect(await screen.findByText("paused")).toBeTruthy();
    const jump = screen.getByRole("button", { name: /jump to latest/ });
    fireEvent.click(jump);

    // resumes: back to "following ↓", the jump affordance is gone
    expect(await screen.findByText("following ↓")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /jump to latest/ })).toBeNull();
  });

  it("does not show follow-mode chrome for a finished run", async () => {
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    await screen.findByText("streaming line");
    expect(screen.queryByText("following ↓")).toBeNull();
    expect(screen.queryByText("paused")).toBeNull();
    expect(screen.queryByRole("button", { name: /jump to latest/ })).toBeNull();
  });
});

describe("RunDetail run history", () => {
  it("flags the current attempt in the run history and opens another attempt on click", async () => {
    const onSelectRun = vi.fn();
    h.issueHistory.mockResolvedValue({
      issue_identifier: "INF-231",
      runs: [
        summary({ id: 1, attempt: 1, outcome: "completed", total_tokens: 1_040_000 }),
        summary({ id: 2, attempt: 0, outcome: "failed", started_at: "2026-06-05T10:00:00Z", ended_at: "2026-06-05T10:05:00Z", total_tokens: 1000, error: "boom" }),
      ],
    });
    renderDetail(1, () => {}, onSelectRun);
    expect(await screen.findByText("· current")).toBeTruthy();
    // the current attempt (id 1, 0-indexed attempt 1) shows its label 1-indexed as "attempt 2";
    // the attempt-0 row (id 2) shows none. The raw "attempt 1" must never render.
    expect(screen.getByText("attempt 2")).toBeTruthy();
    expect(screen.queryByText("attempt 1")).toBeNull();
    expect(screen.queryByText("attempt 0")).toBeNull();
    // clicking the OTHER attempt (id 2) opens it — target its unique token label.
    fireEvent.click(screen.getByText("1.0k tok"));
    expect(onSelectRun).toHaveBeenCalledWith(2);
  });

  it("hides the attempt badge for a clean attempt-0 run but shows it for a retried one", async () => {
    h.issueHistory.mockResolvedValue({
      issue_identifier: "INF-231",
      runs: [
        summary({ id: 1, attempt: 0, outcome: "completed", total_tokens: 1_040_000 }),
        summary({ id: 2, attempt: 2, outcome: "failed", started_at: "2026-06-05T10:00:00Z", ended_at: "2026-06-05T10:05:00Z", total_tokens: 1000, error: "boom" }),
      ],
    });
    renderDetail();
    expect(await screen.findByText("attempt 3")).toBeTruthy();
    expect(screen.queryByText("attempt 0")).toBeNull();
  });

  it("does not flash 'No prior runs' while issue history is still loading", async () => {
    h.issueHistory.mockReturnValue(new Promise<IssueHistoryResponse>(() => {}));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("No prior runs.")).toBeNull();
  });

  it("shows 'No prior runs' once issue history resolves empty", async () => {
    h.issueHistory.mockResolvedValue({ issue_identifier: "INF-231", runs: [] });
    renderDetail();
    expect(await screen.findByText("No prior runs.")).toBeTruthy();
  });
});
