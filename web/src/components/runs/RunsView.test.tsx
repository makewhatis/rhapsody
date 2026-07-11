// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { RunSummary } from "@/lib/api";

const h = vi.hoisted(() => ({
  bridge: false,
  state: vi.fn(),
  history: vi.fn(),
  linearProjects: vi.fn(),
  runDetail: vi.fn(),
  transcript: vi.fn(),
  issueHistory: vi.fn(),
}));

vi.mock("@/lib/bindings", () => ({ hasBridge: () => h.bridge }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchState: () => h.state(),
    fetchHistory: () => h.history(),
    fetchLinearProjects: () => h.linearProjects(),
    fetchRunDetail: (id: number) => h.runDetail(id),
    fetchRunTranscript: (id: number) => h.transcript(id),
    fetchIssueHistory: (id: string) => h.issueHistory(id),
  };
});

import { RunsView } from "@/components/runs/RunsView";
import { ToastProvider } from "@/components/shell/Toast";

function summary(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 12,
    issue_id: "x",
    issue_identifier: "CORE-112",
    title: "SCIM provisioning endpoint",
    attempt: 1,
    session_uuid: "",
    branch: "core/112-scim",
    project_slug: "symphony-core-5f1a2b3c4d5e",
    repo: "git@github.com:makewhatis/symphony-core.git",
    started_at: "2026-06-05T09:18:00Z",
    ended_at: "2026-06-05T10:40:00Z",
    outcome: "completed",
    turns: 31,
    input_tokens: 50,
    output_tokens: 92,
    total_tokens: 142_100,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

beforeEach(() => {
  h.state.mockResolvedValue({
    status: "ok",
    poll_interval_ms: 2000,
    running: [],
    retrying: [],
    codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
    rate_limits: [],
    blocked: [],
  });
  h.history.mockResolvedValue({ runs: [summary()], next_offset: null });
  h.linearProjects.mockResolvedValue([]);
  h.runDetail.mockResolvedValue({
    run_id: 12,
    issue_id: "x",
    issue_identifier: "CORE-112",
    title: "SCIM provisioning endpoint",
    project: "symphony-core-5f1a2b3c4d5e",
    repo: "git@github.com:makewhatis/symphony-core.git",
    attempt: 1,
    outcome: "completed",
    live: false,
    issue_state: "",
    last_codex_event: "",
    turn_count: 31,
    input_tokens: 50,
    output_tokens: 92,
    total_tokens: 142_100,
    usage_estimated: false,
    started_at: "2026-06-05T09:18:00Z",
    ended_at: "2026-06-05T10:40:00Z",
    last_event_at: "",
    error: "",
    recent_events: [],
    generated_at: "",
  });
  h.transcript.mockResolvedValue({ run_id: 12, entries: [], generated_at: "" });
  h.issueHistory.mockResolvedValue({ issue_identifier: "CORE-112", runs: [] });
});

afterEach(() => {
  cleanup();
  h.bridge = false;
  for (const v of Object.values(h)) if (typeof v !== "boolean") v.mockReset();
});

function renderView() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <RunsView />
      </ToastProvider>
    </QueryClientProvider>,
  );
}

describe("RunsView", () => {
  it("renders the stat tiles and the unified jobs list", async () => {
    renderView();
    // the job row arrives once the history query resolves
    expect(await screen.findByText("CORE-112")).toBeTruthy();
    expect(screen.getByText("Jobs")).toBeTruthy();
    // stat tiles (labels unique to the tile row, not the filter pills)
    expect(screen.getByText("Tokens today")).toBeTruthy();
    expect(screen.getByText("Runtime today")).toBeTruthy();
  });

  it("opens the run detail on row click and returns to the list on Back", async () => {
    renderView();
    fireEvent.click(await screen.findByText("CORE-112"));
    // detail header heading (mono issue id)
    expect(await screen.findByRole("heading", { name: "CORE-112" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Jobs/ }));
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
  });

  it("polls the daemon over /api under the Wails bridge too (the AssetServer proxies /api to the sidecar)", async () => {
    h.bridge = true;
    renderView();
    // the history row arrives → the HTTP queries fired even under the app host
    expect(await screen.findByText("CORE-112")).toBeTruthy();
    expect(h.state).toHaveBeenCalled();
    expect(h.history).toHaveBeenCalled();
    // the list is live, not paused
    expect(screen.queryByText("live updates paused")).toBeNull();
  });
});
