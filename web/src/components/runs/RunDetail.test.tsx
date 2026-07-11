// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { IssueHistoryResponse, LinearProject, LogEntry, RunDetail as RunDetailDTO } from "@/lib/api";

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

function renderDetail(runId = 1, onBack = () => {}, onSelectRun = () => {}) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <RunDetail runId={runId} projects={PROJECTS} onBack={onBack} onSelectRun={onSelectRun} />
      </ToastProvider>
    </QueryClientProvider>,
  );
}

describe("RunDetail header + meta", () => {
  it("renders the issue id, title, resolved agent and meta grid", async () => {
    renderDetail();
    expect(await screen.findByRole("heading", { name: "INF-231" })).toBeTruthy();
    expect(screen.getByText("Sign & notarize the dmg")).toBeTruthy();
    // Both the header agent and the Project meta cell resolve to the Linear project name (never the
    // raw config slug id) — they derive from the same project slug, so the name appears twice.
    expect(screen.getAllByText("Infrastructure")).toHaveLength(2);
    expect(screen.queryByText("symphony-infra-tasks-9c29e9ade060")).toBeNull(); // raw slug never shown when resolvable
    expect(screen.getByText("example/demo-repo")).toBeTruthy(); // repo meta cell
  });

  it("omits the Attempt meta row for a clean attempt-0 run", async () => {
    h.runDetail.mockResolvedValue(detail({ attempt: 0 }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("Attempt")).toBeNull();
  });

  it("shows the Attempt meta row 1-indexed for a retried run", async () => {
    h.runDetail.mockResolvedValue(detail({ attempt: 2 }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    // scope the value lookup to the Attempt meta cell so a future fixture that happens to render
    // "3" elsewhere (e.g. a turn/token count) can't make this assertion ambiguous. attempt 2 is
    // the 0-indexed counter, displayed 1-indexed as "3" (the third dispatch).
    const attemptCell = screen.getByText("Attempt").closest("div");
    expect(attemptCell).toBeTruthy();
    expect(within(attemptCell as HTMLElement).getByText("3")).toBeTruthy();
    // the raw 0-indexed value must not leak into the cell.
    expect(within(attemptCell as HTMLElement).queryByText("2")).toBeNull();
  });

  it("shows only Open ticket for a finished (completed) run — no Stop or Resume", async () => {
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.getByRole("button", { name: /Open ticket/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Restart/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("shows Stop (with confirm) for a running run and calls stopRun on confirm", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    fireEvent.click(screen.getByRole("button", { name: "Stop" })); // opens confirm
    fireEvent.click(screen.getByRole("button", { name: "Stop agent" })); // confirm action
    await waitFor(() => expect(h.stopRun).toHaveBeenCalledWith(1));
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("toasts the move-failure when the agent was killed but the ticket couldn't be moved", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.stopRun.mockResolvedValue({ identifier: "INF-231", move_error: "no backlog state for team" });
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    fireEvent.click(screen.getByRole("button", { name: "Stop" })); // opens confirm
    fireEvent.click(screen.getByRole("button", { name: "Stop agent" })); // confirm action
    await waitFor(() => expect(h.stopRun).toHaveBeenCalledWith(1));
    // The success toast surfaces the killed-but-not-moved distinction in its desc line.
    expect(await screen.findByText(/couldn't move the ticket: no backlog state for team/i)).toBeTruthy();
  });

  it("shows Resume for a stopped run and calls resumeRun", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "stopped", ended_at: "2026-06-08T00:00:00Z", live: false }));
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    await waitFor(() => expect(h.resumeRun).toHaveBeenCalledWith(1));
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
  });

  it("renders the amber Stopped panel (NOT a red Failure box) for a user-stopped run", async () => {
    h.runDetail.mockResolvedValue(
      detail({ outcome: "stopped", error: "stopped by user", ended_at: "2026-06-08T00:00:00Z", live: false }),
    );
    renderDetail();
    expect(await screen.findByText("Stopped")).toBeTruthy();
    // the action-oriented Resume guidance, and NO red "Failure" box anywhere.
    expect(screen.getByText(/Resume dispatches a fresh agent/)).toBeTruthy();
    expect(screen.queryByText("Failure")).toBeNull();
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
    // live token totals are always in-flight estimates
    expect(await screen.findByText("est.")).toBeTruthy();
  });

  it("calls onBack when the Jobs button is clicked", async () => {
    const onBack = vi.fn();
    renderDetail(1, onBack);
    fireEvent.click(await screen.findByRole("button", { name: /Jobs/ }));
    expect(onBack).toHaveBeenCalledOnce();
  });

  it("renders the failure reason panel when a failed run carries an error", async () => {
    h.runDetail.mockResolvedValue(
      detail({ outcome: "failed", error: "clone --bare: git@github.com: Permission denied (publickey)." }),
    );
    renderDetail();
    expect(await screen.findByText("Failure")).toBeTruthy();
    expect(screen.getByText(/Permission denied \(publickey\)/)).toBeTruthy();
  });

  it("omits the failure panel when the run has no error", async () => {
    renderDetail();
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("Failure")).toBeNull();
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
    { seq: 1, kind: "event", tool: "", text: "SESSION STARTED" },
    { seq: 2, kind: "text", tool: "", text: "I'll start by **understanding** the state." },
    { seq: 3, kind: "tool_use", tool: "mcp__claude_ai_Linear__get_issue", text: "id=INF-231" },
    { seq: 4, kind: "tool_result", tool: "", text: "{\"ok\":true}" },
  ];

  it("renders dividers, bolded text, MCP tool chips and tool output", async () => {
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    expect(await screen.findByText("SESSION STARTED")).toBeTruthy();
    // markdown bold -> <strong>
    const strong = screen.getByText("understanding");
    expect(strong.tagName.toLowerCase()).toBe("strong");
    // MCP tool chip shows the full mcp__ name
    expect(screen.getByText(/mcp__claude_ai_Linear__get_issue/)).toBeTruthy();
    expect(screen.getByText("id=INF-231")).toBeTruthy();
    expect(screen.getByText(/"ok":true/)).toBeTruthy();
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

  it("shows the live cursor + streaming note while running, not when finished", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    expect(await screen.findByText("running…")).toBeTruthy();
    expect(screen.getByText("streaming")).toBeTruthy();
  });

  it("shows the final note (no cursor) for a finished run", async () => {
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    await screen.findByText("SESSION STARTED");
    expect(screen.getByText("final")).toBeTruthy();
    expect(screen.queryByText("running…")).toBeNull();
  });

  it("does not flash 'No transcript' while the transcript query is still loading", async () => {
    // run detail resolves (finished, 0 entries so far) but the transcript fetch never settles —
    // the empty message must wait on the transcript query's loading state, not just entries.length.
    h.transcript.mockReturnValue(new Promise<ReturnType<typeof transcript>>(() => {}));
    renderDetail();
    // the loaded shell is up (meta grid present) but the empty message is gated on loading
    await screen.findByRole("heading", { name: "INF-231" });
    expect(screen.queryByText("No transcript for this run.")).toBeNull();
  });

  it("shows 'No transcript' once the transcript query resolves empty for a finished run", async () => {
    h.transcript.mockResolvedValue(transcript([]));
    renderDetail();
    expect(await screen.findByText("No transcript for this run.")).toBeTruthy();
  });

  it("keeps streaming chrome out for a cached running run when polling is disabled", async () => {
    // Prime the cache with a running detail, then render disabled: useRunDetail serves the cached
    // payload (data present, outcome === "running") but polling is paused. streaming = inFlight &&
    // enabled must be false, so no live chrome leaks even though the state badge still reads running.
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    client.setQueryData(["run-detail", 1], detail({ outcome: "running", live: true, ended_at: "" }));
    client.setQueryData(["run-transcript", 1], transcript(entries));
    render(
      <QueryClientProvider client={client}>
        <ToastProvider>
          <RunDetail runId={1} projects={PROJECTS} enabled={false} onBack={() => {}} onSelectRun={() => {}} />
        </ToastProvider>
      </QueryClientProvider>,
    );
    // the transcript renders from cache (so the panel is mounted), but the live chrome is gated off
    expect(await screen.findByText("SESSION STARTED")).toBeTruthy();
    expect(screen.getByText("final")).toBeTruthy();
    expect(screen.queryByText("streaming")).toBeNull();
    expect(screen.queryByText("running…")).toBeNull();
  });

  it("shows streaming chrome for a running run when polling is enabled", async () => {
    h.runDetail.mockResolvedValue(detail({ outcome: "running", live: true, ended_at: "" }));
    h.transcript.mockResolvedValue(transcript(entries));
    renderDetail();
    expect(await screen.findByText("running…")).toBeTruthy();
    expect(screen.getByText("streaming")).toBeTruthy();
  });
});

describe("RunDetail run history", () => {

  it("flags the current attempt in the run history and opens another attempt on click", async () => {
    const onSelectRun = vi.fn();
    h.issueHistory.mockResolvedValue({
      issue_identifier: "INF-231",
      runs: [
        { id: 1, issue_id: "x", issue_identifier: "INF-231", title: "t", attempt: 1, session_uuid: "", branch: "inf/231", project_slug: "symphony-infra-tasks-9c29e9ade060", repo: "", started_at: "2026-06-06T14:58:00Z", ended_at: "2026-06-06T15:10:00Z", outcome: "completed", turns: 14, input_tokens: 0, output_tokens: 0, total_tokens: 1_040_000, usage_estimated: false, error: "", transcript_path: "" },
        { id: 2, issue_id: "x", issue_identifier: "INF-231", title: "t", attempt: 0, session_uuid: "", branch: "inf/231", project_slug: "symphony-infra-tasks-9c29e9ade060", repo: "", started_at: "2026-06-05T10:00:00Z", ended_at: "2026-06-05T10:05:00Z", outcome: "failed", turns: 3, input_tokens: 0, output_tokens: 0, total_tokens: 1000, usage_estimated: false, error: "boom", transcript_path: "" },
      ],
    });
    renderDetail(1, () => {}, onSelectRun);
    expect(await screen.findByText("· current")).toBeTruthy();
    // the current attempt (id 1, 0-indexed attempt 1) shows its label 1-indexed as "attempt 2";
    // the attempt-0 row (id 2) shows none. The raw "attempt 1" must never render.
    expect(screen.getByText("attempt 2")).toBeTruthy();
    expect(screen.queryByText("attempt 1")).toBeNull();
    expect(screen.queryByText("attempt 0")).toBeNull();
    // clicking the OTHER attempt (id 2) opens it — target its unique token label since the
    // attempt-0 row no longer renders an "attempt" badge.
    fireEvent.click(screen.getByText("1.0k tok"));
    expect(onSelectRun).toHaveBeenCalledWith(2);
  });

  it("hides the attempt badge for a clean attempt-0 run but shows it for a retried one", async () => {
    h.issueHistory.mockResolvedValue({
      issue_identifier: "INF-231",
      runs: [
        { id: 1, issue_id: "x", issue_identifier: "INF-231", title: "t", attempt: 0, session_uuid: "", branch: "inf/231", project_slug: "symphony-infra-tasks-9c29e9ade060", repo: "", started_at: "2026-06-06T14:58:00Z", ended_at: "2026-06-06T15:10:00Z", outcome: "completed", turns: 14, input_tokens: 0, output_tokens: 0, total_tokens: 1_040_000, usage_estimated: false, error: "", transcript_path: "" },
        { id: 2, issue_id: "x", issue_identifier: "INF-231", title: "t", attempt: 2, session_uuid: "", branch: "inf/231", project_slug: "symphony-infra-tasks-9c29e9ade060", repo: "", started_at: "2026-06-05T10:00:00Z", ended_at: "2026-06-05T10:05:00Z", outcome: "failed", turns: 3, input_tokens: 0, output_tokens: 0, total_tokens: 1000, usage_estimated: false, error: "boom", transcript_path: "" },
      ],
    });
    renderDetail();
    // wait for the run-history rows to resolve (the 0-indexed attempt-2 row is labelled 1-indexed
    // as "attempt 3")…
    expect(await screen.findByText("attempt 3")).toBeTruthy();
    // …while the clean attempt-0 row shows no bare "attempt 0".
    expect(screen.queryByText("attempt 0")).toBeNull();
  });

  it("does not flash 'No prior runs' while issue history is still loading", async () => {
    // detail resolves but the history fetch never settles — the empty message must wait on the
    // history query's loading state, not just attempts.length.
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
