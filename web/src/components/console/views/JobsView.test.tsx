// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { RunSummary, StateResponse } from "@/lib/api";

// STUDIO-681 §10, sub-ticket 2 — the Jobs worklist's acceptance boxes 2.6, 2.7 and 2.8,
// driven through the real view against the endpoints §9 has: /api/v1/state for the live
// snapshot and /api/v1/history/issues for one row per ticket.

const h = vi.hoisted(() => ({
  fetchState: vi.fn(),
  fetchIssueRuns: vi.fn(),
  fetchTeamsOverview: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchState: h.fetchState,
    fetchIssueRuns: h.fetchIssueRuns,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchVersion: vi.fn(async () => ({
      version: "v0.4.0",
      commit: "abc",
      built_at: "",
      teams_enabled: true,
    })),
    fetchLinearProjects: vi.fn(async () => []),
    postRefresh: vi.fn(async () => {}),
  };
});

const { JobsView } = await import("./JobsView");

function run(over: Partial<RunSummary> & Pick<RunSummary, "issue_identifier" | "outcome">): RunSummary {
  return {
    id: 1,
    issue_id: `id-${over.issue_identifier}`,
    title: `${over.issue_identifier} title`,
    attempt: 1,
    session_uuid: "s",
    branch: `symphony/${over.issue_identifier}`,
    project_slug: "rhapsody",
    repo: "",
    started_at: "2026-09-01T10:00:00Z",
    ended_at: "2026-09-01T10:30:00Z",
    turns: 1,
    input_tokens: 1,
    output_tokens: 1,
    total_tokens: 2,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  } as RunSummary;
}

const EMPTY_STATE: StateResponse = {
  status: "ok",
  poll_interval_ms: 2000,
  running: [],
  retrying: [],
  codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
  rate_limits: [],
  blocked: [],
};

function mount(onOpenJob = vi.fn()) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <JobsView onOpenJob={onOpenJob} />
    </QueryClientProvider>,
  );
  return onOpenJob;
}

/** A Now-strip stat by its label. */
function stat(label: string): string {
  const cell = [...document.querySelectorAll(".stat")].find(
    (el) => el.querySelector(".l")?.textContent === label,
  );
  return cell?.querySelector(".n")?.textContent ?? "";
}

/** The ticket key of each visible table row, in order. */
function rowKeys(): string[] {
  return [...document.querySelectorAll(".jtbl tbody tr")].map(
    (tr) => tr.querySelector(".ti")?.textContent?.split(" · ")[0] ?? "",
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("the Now strip (§3)", () => {
  // Box 2.6
  it("counts running / in review / queued / blocked from the issues data", async () => {
    h.fetchState.mockResolvedValue({
      ...EMPTY_STATE,
      running: [
        {
          issue_id: "id-A",
          issue_identifier: "A",
          title: "A title",
          state: "In Progress",
          project: "rhapsody",
          repo: "",
          run_id: 9,
          turn_count: 1,
          last_codex_event: "",
          started_at: "2026-09-01T11:00:00Z",
          last_event_at: "2026-09-01T11:00:00Z",
          input_tokens: 0,
          output_tokens: 0,
          total_tokens: 0,
        },
      ],
      blocked: [
        {
          issue_identifier: "D",
          title: "D title",
          project: "rhapsody",
          blocker_identifier: "C",
          blocker_state: "In Review",
          mode: "dag",
        },
      ],
    });
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ issue_identifier: "B", outcome: "completed" }),
        run({ issue_identifier: "C", outcome: "completed" }),
        run({ issue_identifier: "E", outcome: "stopped" }),
        run({ issue_identifier: "F", outcome: "failed" }),
      ],
      next_offset: null,
    });
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    });
    mount();

    await waitFor(() => expect(stat("running")).toBe("1"));
    expect(stat("in review")).toBe("2"); // B, C — a clean run hands its ticket to review
    expect(stat("queued")).toBe("1"); // E
    expect(stat("blocked")).toBe("2"); // D (held) + F (failed)
  });

  it("shows each teammate's live state", async () => {
    h.fetchState.mockResolvedValue(EMPTY_STATE);
    h.fetchIssueRuns.mockResolvedValue({ issues: [], next_offset: null });
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [
        { name: "alice", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 1, tickets: ["STUDIO-1"] },
        { name: "jimmy", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: [] },
      ],
    });
    mount();

    await waitFor(() => expect(screen.getByText("alice")).toBeTruthy());
    expect(screen.getByText("STUDIO-1")).toBeTruthy();
    expect(screen.getByText("idle")).toBeTruthy();
  });
});

describe("the filter bar and the table (§3)", () => {
  async function mountFourJobs() {
    h.fetchState.mockResolvedValue(EMPTY_STATE);
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ issue_identifier: "R-1", outcome: "completed", project_slug: "rhapsody", started_at: "2026-09-01T10:04:00Z" }),
        run({ issue_identifier: "R-2", outcome: "stopped", project_slug: "rhapsody", started_at: "2026-09-01T10:03:00Z" }),
        run({ issue_identifier: "B-1", outcome: "completed", project_slug: "booch", started_at: "2026-09-01T10:02:00Z" }),
        run({ issue_identifier: "B-2", outcome: "failed", project_slug: "booch", started_at: "2026-09-01T10:01:00Z" }),
      ],
      next_offset: null,
    });
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    });
    const onOpen = mount();
    await waitFor(() => expect(rowKeys()).toHaveLength(4));
    return onOpen;
  }

  // Box 2.7 — the status Seg.
  it("filters the table by status", async () => {
    await mountFourJobs();
    fireEvent.click(screen.getByRole("button", { name: "In review" }));
    await waitFor(() => expect(rowKeys().sort()).toEqual(["B-1", "R-1"]));

    fireEvent.click(screen.getByRole("button", { name: "Queued" }));
    await waitFor(() => expect(rowKeys()).toEqual(["R-2"]));

    fireEvent.click(screen.getByRole("button", { name: "All" }));
    await waitFor(() => expect(rowKeys()).toHaveLength(4));
  });

  // Box 2.7 — the project Select.
  it("filters the table by project, and composes with the status filter", async () => {
    await mountFourJobs();
    const select = screen.getByLabelText("Filter by project");
    fireEvent.change(select, { target: { value: "booch" } });
    await waitFor(() => expect(rowKeys().sort()).toEqual(["B-1", "B-2"]));

    fireEvent.click(screen.getByRole("button", { name: "In review" }));
    await waitFor(() => expect(rowKeys()).toEqual(["B-1"]));
  });

  it("says so when a filter matches nothing, rather than showing an empty table", async () => {
    await mountFourJobs();
    fireEvent.click(screen.getByRole("button", { name: "Running" }));
    await waitFor(() => expect(screen.getByText("No jobs match this filter.")).toBeTruthy());
  });

  // Box 2.8
  it("routes a row click to THAT ticket's job/:key", async () => {
    const onOpen = await mountFourJobs();
    const row = [...document.querySelectorAll(".jtbl tbody tr")].find((tr) =>
      tr.textContent?.includes("B-2"),
    );
    fireEvent.click(row!);
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("B-2");
  });

  it("opens a row from the keyboard too", async () => {
    const onOpen = await mountFourJobs();
    const row = [...document.querySelectorAll(".jtbl tbody tr")].find((tr) =>
      tr.textContent?.includes("R-1"),
    );
    fireEvent.keyDown(row!, { key: "Enter" });
    expect(onOpen).toHaveBeenCalledExactlyOnceWith("R-1");
  });

  it("renders every §3 column, with a dash where the daemon serves no data", async () => {
    await mountFourJobs();
    const row = [...document.querySelectorAll(".jtbl tbody tr")].find((tr) =>
      tr.textContent?.includes("R-1"),
    )!;
    const cells = within(row as HTMLElement).getAllByRole("cell");
    expect(cells).toHaveLength(5);
    expect(cells[1].textContent).toBe("—"); // Assigned: no identity on a finished run
    expect(cells[2].textContent).toContain("in review"); // Status
    expect(cells[3].textContent).toBe("—"); // PR: no endpoint serves one
  });
});
