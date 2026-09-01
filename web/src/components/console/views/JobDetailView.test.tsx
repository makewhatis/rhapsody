// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { RunSummary, StateResponse } from "@/lib/api";
import type { PullRequestView } from "@/lib/console-job-detail";

// STUDIO-681 §10, sub-ticket 2 — the Job-detail page's acceptance boxes 2.9, 2.10 and 2.11.

const h = vi.hoisted(() => ({
  fetchIssueHistory: vi.fn(),
  fetchRunTranscript: vi.fn(),
  fetchState: vi.fn(),
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
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
    repo: "",
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

function mountDetail(runs: RunSummary[], onNavigate = vi.fn()) {
  h.fetchIssueHistory.mockResolvedValue({ issue_identifier: "STUDIO-654", runs });
  h.fetchState.mockResolvedValue(EMPTY_STATE);
  h.fetchTeamsOverview.mockResolvedValue({
    enabled: true,
    manager_mode: "labels",
    default_identity: "",
    backend: "local",
    roster: [
      { name: "alice", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: [] },
    ],
  });
  h.fetchTeamsRoom.mockResolvedValue({ messages: [], skipped: [] });
  h.fetchTeamsRecall.mockResolvedValue({ identity: "alice", facts: [], skipped: [] });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <JobDetailView issue="STUDIO-654" onNavigate={onNavigate} />
    </QueryClientProvider>,
  );
  return onNavigate;
}

/** A summary-strip cell's value, by its label. */
function kv(label: string): string {
  const cell = [...document.querySelectorAll(".kv")].find(
    (el) => el.querySelector(".l")?.textContent === label,
  );
  return cell?.querySelector(".v")?.textContent ?? "";
}

/** The run ids listed, in render order. */
function runIds(): string[] {
  return [...document.querySelectorAll(".run .rid")].map((el) => el.textContent ?? "");
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("the summary strip (§4)", () => {
  // Box 2.9
  it("renders every field from /issues/<key>/history", async () => {
    mountDetail([
      run({ id: 522, started_at: "2026-08-30T20:21:00Z", ended_at: "2026-08-30T20:45:00Z" }),
      run({ id: 547 }),
    ]);
    await waitFor(() => expect(kv("Runs")).toBe("2"));
    expect(kv("Status")).toContain("in review");
    expect(kv("Branch")).toBe("symphony/STUDIO-654");
    expect(kv("Updated")).not.toBe("");
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Attach a photo in chat");
  });

  it("shows a dash for the fields no endpoint serves", async () => {
    mountDetail([run({ id: 1 })]);
    await waitFor(() => expect(kv("Runs")).toBe("1"));
    expect(kv("Pull request")).toBe("—"); // no PR endpoint (§9/§11)
    expect(kv("Assignee")).toBe("—"); // no identity on a stored run row
  });

  it("navigates back to Jobs from the breadcrumb", async () => {
    const onNavigate = mountDetail([run({ id: 1 })]);
    fireEvent.click(await screen.findByText("Jobs"));
    expect(onNavigate).toHaveBeenCalledExactlyOnceWith("jobs");
  });

  it("survives a ticket with no recorded runs", async () => {
    mountDetail([]);
    await waitFor(() => expect(screen.getByText("This ticket has no recorded runs.")).toBeTruthy());
    expect(kv("Runs")).toBe("0");
  });
});

describe("the runs list (§4)", () => {
  // Box 2.10 — newest first.
  it("lists runs newest-first", async () => {
    mountDetail([
      run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
      run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 545, started_at: "2026-09-01T16:54:00Z" }),
    ]);
    await waitFor(() => expect(runIds()).toEqual(["run 547", "run 545", "run 522"]));
  });

  // Box 2.10 — expanding shows meta + the transcript timeline.
  it("shows a run's meta and transcript timeline when it is expanded", async () => {
    h.fetchRunTranscript.mockResolvedValue({
      run_id: 547,
      generated_at: "",
      entries: [
        { seq: 1, kind: "event", tool: "", text: "session started" },
        { seq: 2, kind: "tool_use", tool: "Bash", text: "git rebase origin/master" },
        { seq: 3, kind: "tool_result", tool: "", text: "6 conflicts, resolved" },
        { seq: 4, kind: "tool_use", tool: "teams_post", text: "handed off to the room" },
        { seq: 5, kind: "event", tool: "", text: "turn completed" },
      ],
    });
    mountDetail([run({ id: 547, turns: 1 })]);

    // The newest run opens by default, so its transcript is the one fetched.
    await waitFor(() => expect(screen.getByText(/git rebase origin\/master/)).toBeTruthy());
    expect(document.querySelector(".rmeta")?.textContent).toContain("1 turn");
    expect(document.querySelector(".rmeta")?.textContent).toContain("38.0k tokens");
    expect(screen.getByText(/6 conflicts, resolved/)).toBeTruthy();
    expect(screen.getByText(/handed off to the room/)).toBeTruthy();
    expect(document.querySelector(".tline.done")?.textContent).toContain("turn completed");
  });

  it("fetches only the transcript of a run that is actually open", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 0, generated_at: "", entries: [] });
    mountDetail([
      run({ id: 547, started_at: "2026-09-01T19:11:00Z" }),
      run({ id: 545, started_at: "2026-09-01T16:54:00Z" }),
      run({ id: 522, started_at: "2026-08-30T20:21:00Z" }),
    ]);
    await waitFor(() => expect(runIds()).toHaveLength(3));
    await waitFor(() => expect(h.fetchRunTranscript).toHaveBeenCalled());
    // Three runs, one open: exactly one transcript request, for the newest run.
    expect(h.fetchRunTranscript).toHaveBeenCalledExactlyOnceWith(547);
  });

  it("says so when a run recorded no transcript", async () => {
    h.fetchRunTranscript.mockResolvedValue({ run_id: 547, generated_at: "", entries: [] });
    mountDetail([run({ id: 547 })]);
    await waitFor(() =>
      expect(screen.getByText("No transcript recorded for this run.")).toBeTruthy(),
    );
  });
});

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
