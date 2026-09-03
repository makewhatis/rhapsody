// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { IssueRun, StateResponse } from "@/lib/api";
import { phaseGlyph } from "@/lib/console-trace-view";
import { SPARK_KINDS } from "@/lib/console-trace-spark";

// STUDIO-681 §10, sub-ticket 2 — the Jobs worklist's acceptance boxes 2.6, 2.7 and 2.8,
// driven through the real view against the endpoints §9 has: /api/v1/state for the live
// snapshot and /api/v1/history/issues for one row per ticket.

const h = vi.hoisted(() => ({
  fetchState: vi.fn(),
  fetchIssueRuns: vi.fn(),
  fetchTeamsOverview: vi.fn(),
  fetchRunTranscript: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchState: h.fetchState,
    fetchIssueRuns: h.fetchIssueRuns,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchRunTranscript: h.fetchRunTranscript,
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

function run(over: Partial<IssueRun> & Pick<IssueRun, "issue_identifier" | "outcome">): IssueRun {
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
  } as IssueRun;
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

// STUDIO-702 — the daemon now reports each ticket's real lifecycle on the issue listing, and the
// worklist colours itself from that rather than from a run outcome that never expires.
describe("the ticket lifecycle (STUDIO-702)", () => {
  async function mountLifecycleJobs() {
    h.fetchState.mockResolvedValue(EMPTY_STATE);
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ issue_identifier: "MERGED", outcome: "completed", lifecycle: "done", tracker_state: "Done" }),
        run({ issue_identifier: "DROPPED", outcome: "completed", lifecycle: "canceled", tracker_state: "Won't Do" }),
        run({ issue_identifier: "REVIEW", outcome: "completed", lifecycle: "in_review", tracker_state: "In Review" }),
        run({ issue_identifier: "LEGACY", outcome: "completed" }),
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
    await waitFor(() => expect(rowKeys()).toHaveLength(4));
  }

  // The bug: two of these four tickets are terminal and used to be counted as awaiting review, for
  // as long as the store kept their runs.
  it("counts only work actually awaiting a reviewer", async () => {
    await mountLifecycleJobs();
    // REVIEW, plus LEGACY, which the daemon could not resolve and which falls back as before.
    expect(stat("in review")).toBe("2");
  });

  // The Done tab was permanently empty: `done` was unreachable from run outcomes alone.
  it("populates the Done filter with the terminal tickets", async () => {
    await mountLifecycleJobs();
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
    await waitFor(() => expect(rowKeys().sort()).toEqual(["DROPPED", "MERGED"]));

    fireEvent.click(screen.getByRole("button", { name: "In review" }));
    await waitFor(() => expect(rowKeys().sort()).toEqual(["LEGACY", "REVIEW"]));
  });

  it("hovers the tracker's own state name behind the normalized Pill", async () => {
    await mountLifecycleJobs();
    const cellFor = (key: string) =>
      [...document.querySelectorAll(".jtbl tbody tr")]
        .find((tr) => tr.textContent?.includes(key))
        ?.querySelectorAll("td")[2];
    expect(cellFor("DROPPED")?.getAttribute("title")).toBe("Won't Do");
    expect(cellFor("LEGACY")?.getAttribute("title")).toBeNull();
  });
});

// STUDIO-735 — the ASSIGNED column used to name a teammate only while the job was running, because
// the live roster was the only place it looked. The daemon now reports a durable assignee per
// history row, and the column keeps it for the whole life of the ticket.
describe("the durable assignee (STUDIO-735)", () => {
  const assignedCell = (key: string) =>
    [...document.querySelectorAll(".jtbl tbody tr")]
      .find((tr) => tr.textContent?.includes(key))
      ?.querySelectorAll("td")[1]?.textContent;

  it("keeps the teammate on a done or in-review job, and stays '—' for an unrouted one", async () => {
    h.fetchState.mockResolvedValue(EMPTY_STATE);
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ issue_identifier: "MERGED", outcome: "completed", lifecycle: "done", assignee: "alice" }),
        run({ issue_identifier: "REVIEW", outcome: "completed", lifecycle: "in_review", assignee: "jimmy" }),
        run({ issue_identifier: "SOLO", outcome: "completed", lifecycle: "done" }),
      ],
      next_offset: null,
    });
    // Nobody is live: every one of these rows would have rendered "—" before this ticket.
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    });
    mount();

    await waitFor(() => expect(rowKeys()).toHaveLength(3));
    expect(assignedCell("MERGED")).toBe("alice");
    expect(assignedCell("REVIEW")).toBe("jimmy");
    expect(assignedCell("SOLO")).toBe("—");
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
    expect(cells).toHaveLength(6);
    expect(cells[1].textContent).toBe("—"); // Assigned: no identity on a finished run
    expect(cells[2].textContent).toContain("in review"); // Status
    expect(cells[3].textContent).toBe("···"); // Trace: unread until pointed at (STUDIO-743)
    expect(cells[4].textContent).toBe("—"); // PR: no endpoint serves one
  });
});

// STUDIO-743 (design record §6) — the additive Jobs-home touch: the operator's own count on the
// Now strip, and a per-row preview of each run's shape in the run detail's phase glyphs.
describe("the Needs you count (§6)", () => {
  it("counts the tickets whose next move is the operator's, not the daemon's", async () => {
    h.fetchState.mockResolvedValue({
      ...EMPTY_STATE,
      blocked: [
        {
          issue_identifier: "HELD",
          title: "HELD title",
          project: "rhapsody",
          blocker_identifier: "REVIEW",
          blocker_state: "In Review",
          mode: "dag",
        },
      ],
    });
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ issue_identifier: "REVIEW", outcome: "completed", lifecycle: "in_review" }),
        // A finished run the tracker has no answer for. It reads "in review" off the outcome
        // alone, which is an inference that never expires — so it is NOT the operator's to act on.
        run({ issue_identifier: "STALE", outcome: "completed" }),
        run({ issue_identifier: "MERGED", outcome: "completed", lifecycle: "done" }),
        run({ issue_identifier: "FAILED", outcome: "failed" }),
        run({ issue_identifier: "IDLE", outcome: "stopped" }),
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

    // REVIEW is parked for a reviewer and FAILED needs a decision. MERGED is finished, IDLE is
    // the daemon's to redispatch, and HELD waits on REVIEW rather than on the operator.
    await waitFor(() => expect(stat("needs you")).toBe("2"));
    // The stat is not a re-tint of its neighbours, and this pins it against BOTH of the pills it
    // overlaps: two rows read "in review" and only one of them wants a human, two read "blocked"
    // (HELD + FAILED) and only one of them does.
    expect(stat("in review")).toBe("2");
    expect(stat("blocked")).toBe("2");
  });
});

describe("the row trace-sparkline (§6)", () => {
  const TRANSCRIPT = {
    run_id: 7,
    entries: [
      { seq: 1, kind: "tool_use", tool: "Read", text: "file_path=/repo/src/lib/api.ts" },
      { seq: 2, kind: "tool_result", tool: "", text: "export interface RunSummary" },
      { seq: 3, kind: "tool_use", tool: "Edit", text: "file_path=/repo/src/lib/api.ts" },
      { seq: 4, kind: "tool_result", tool: "", text: "applied" },
    ],
    generated_at: "2026-09-03T12:00:00Z",
  };

  async function mountJobs() {
    h.fetchState.mockResolvedValue(EMPTY_STATE);
    h.fetchIssueRuns.mockResolvedValue({
      issues: [
        run({ id: 7, issue_identifier: "A-1", outcome: "completed" }),
        run({ id: 8, issue_identifier: "B-2", outcome: "completed" }),
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
    h.fetchRunTranscript.mockResolvedValue(TRANSCRIPT);
    mount();
    await waitFor(() => expect(rowKeys()).toEqual(["A-1", "B-2"]));
  }

  function row(issue: string): HTMLElement {
    return [...document.querySelectorAll(".jtbl tbody tr")].find((tr) =>
      tr.textContent?.includes(issue),
    ) as HTMLElement;
  }

  function glyphs(issue: string): string[] {
    return [...row(issue).querySelectorAll(".spark .gly")].map((el) => el.textContent ?? "");
  }

  /** The glyphs of the kinds the run actually reached — the slots that are not empty. */
  function lit(issue: string): string[] {
    return [...row(issue).querySelectorAll(".spark .gly:not(.off)")].map((el) => el.textContent ?? "");
  }

  // The acceptance criterion with teeth: a worklist of N rows must not cost N transcripts.
  it("fetches no transcript for a table nobody has pointed at", async () => {
    await mountJobs();
    expect(h.fetchRunTranscript).not.toHaveBeenCalled();
  });

  it("draws the run's shape in the spine's own glyphs once a row is pointed at", async () => {
    await mountJobs();
    fireEvent.mouseEnter(row("A-1"));

    await waitFor(() =>
      expect(lit("A-1")).toEqual([phaseGlyph("oriented"), phaseGlyph("implemented")]),
    );
    // Every kind keeps its column, whether the run reached it or not: that is what makes the cell
    // a checklist a reader can compare with the row above rather than an invented chronology.
    expect(glyphs("A-1")).toEqual(SPARK_KINDS.map(phaseGlyph));
    expect(row("A-1").querySelectorAll(".spark .gly.off").length).toBe(4);
    // Only the row that was pointed at — its neighbour is still unread.
    expect(h.fetchRunTranscript).toHaveBeenCalledExactlyOnceWith(7);
    expect(glyphs("B-2")).toEqual([]);
  });

  it("names the strip so it is not a row of mystery symbols", async () => {
    await mountJobs();
    fireEvent.mouseEnter(row("A-1"));
    await waitFor(() =>
      // The exact counts AND what the run never reached — the empty slots only imply the latter,
      // and "it never tested" is worth saying out loud rather than leaving to be inferred.
      expect(row("A-1").querySelector(".spark")?.getAttribute("aria-label")).toBe(
        "Oriented ×1 · Implemented ×1 — none: Verified, Coordinated, Handed off, Worked",
      ),
    );
  });

  // The strip's label is asserted through the ROLE and the computed accessible NAME, not by reading
  // the attribute back off the element. The row around it is a `<tr role="link" aria-label=…>`
  // (§10 box 2.8), and a labelled ancestor is exactly the shape that can swallow a descendant's
  // name — `link` is not one of the roles ARIA marks children-presentational, so the strip stays
  // its own named node, and this query fails if that ever stops being true.
  it("announces the strip as a named image of its own, inside the labelled row", async () => {
    await mountJobs();
    fireEvent.mouseEnter(row("A-1"));
    await waitFor(() =>
      expect(
        within(row("A-1")).getByRole("img", {
          name: "Oriented ×1 · Implemented ×1 — none: Verified, Coordinated, Handed off, Worked",
        }),
      ).toBeTruthy(),
    );
    // And the row keeps its own name — the two names coexist rather than one replacing the other.
    expect(row("A-1").getAttribute("aria-label")).toBe("A-1 A-1 title");
  });

  it("does not fetch for a row the pointer merely crosses", async () => {
    await mountJobs();
    fireEvent.mouseEnter(row("A-1"));
    fireEvent.mouseLeave(row("A-1"));
    fireEvent.mouseEnter(row("B-2"));
    fireEvent.mouseLeave(row("B-2"));
    await new Promise((resolve) => setTimeout(resolve, 400));
    expect(h.fetchRunTranscript).not.toHaveBeenCalled();
  });

  it("does not fetch for a row that is tabbed through on the way to another", async () => {
    await mountJobs();
    fireEvent.focus(row("A-1"));
    fireEvent.blur(row("A-1"));
    await new Promise((resolve) => setTimeout(resolve, 400));
    expect(h.fetchRunTranscript).not.toHaveBeenCalled();
  });

  // A pointer is not the only way through the table (§10 box 2.8 made the row focusable).
  it("draws the strip for a row reached from the keyboard", async () => {
    await mountJobs();
    fireEvent.focus(row("B-2"));
    await waitFor(() =>
      expect(lit("B-2")).toEqual([phaseGlyph("oriented"), phaseGlyph("implemented")]),
    );
    expect(h.fetchRunTranscript).toHaveBeenCalledExactlyOnceWith(8);
  });

  it("shows a dash, never an invented shape, when the transcript cannot be read", async () => {
    await mountJobs();
    h.fetchRunTranscript.mockRejectedValue(new Error("nope"));
    fireEvent.focus(row("A-1"));
    await waitFor(() =>
      expect(row("A-1").querySelector(".spark")?.textContent).toBe("—"),
    );
  });

  // A held dependent is a synthetic row with no run behind it (`mergeJobs` gives it runId 0), so
  // there is no transcript to read and the cell must say so rather than sit on a spinner.
  it("says so, and asks for nothing, on a row that has never run", async () => {
    h.fetchState.mockResolvedValue({
      ...EMPTY_STATE,
      blocked: [
        {
          issue_identifier: "HELD",
          title: "HELD title",
          project: "rhapsody",
          blocker_identifier: "C",
          blocker_state: "In Review",
          mode: "dag",
        },
      ],
    });
    h.fetchIssueRuns.mockResolvedValue({ issues: [], next_offset: null });
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    });
    mount();
    await waitFor(() => expect(rowKeys()).toEqual(["HELD"]));

    fireEvent.focus(row("HELD"));
    await new Promise((resolve) => setTimeout(resolve, 400));
    expect(row("HELD").querySelector(".spark")?.textContent).toBe("—");
    expect(h.fetchRunTranscript).not.toHaveBeenCalled();
  });

  it("marks a live run with the playhead", async () => {
    h.fetchState.mockResolvedValue({
      ...EMPTY_STATE,
      running: [
        {
          issue_id: "id-A-1",
          issue_identifier: "A-1",
          title: "A-1 title",
          state: "In Progress",
          project: "rhapsody",
          repo: "",
          run_id: 7,
          turn_count: 1,
          last_codex_event: "",
          started_at: "2026-09-01T11:00:00Z",
          last_event_at: "2026-09-01T11:00:00Z",
          input_tokens: 0,
          output_tokens: 0,
          total_tokens: 0,
        },
      ],
    });
    h.fetchIssueRuns.mockResolvedValue({ issues: [], next_offset: null });
    h.fetchTeamsOverview.mockResolvedValue({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    });
    h.fetchRunTranscript.mockResolvedValue(TRANSCRIPT);
    mount();
    await waitFor(() => expect(rowKeys()).toEqual(["A-1"]));

    fireEvent.focus(row("A-1"));
    await waitFor(() => expect(row("A-1").querySelector(".spark .gly.now")).not.toBeNull());
    expect(row("A-1").querySelector(".spark")?.getAttribute("aria-label")).toContain("Running now");
  });
});

// The strip's two states are only distinguishable by CSS — a glyph with no rule is an unstyled
// character, and the playhead with no rule is indistinguishable from the phases beside it. The
// class names are therefore checked against the stylesheet as well as the DOM, the way the
// console's Pill variants are (STUDIO-681 §10 box 1.4).
describe("the §6 additions are painted, not just classed", () => {
  const viewsCss = readFileSync(path.resolve(__dirname, "../../../theme/console-views.css"), "utf8");
  const consoleCss = readFileSync(path.resolve(__dirname, "../../../theme/console.css"), "utf8");

  it("styles the sparkline's glyph and its playhead", () => {
    expect(viewsCss).toMatch(/\.spark \.gly \{/);
    expect(viewsCss).toMatch(/\.spark \.gly\.now \{[^}]*color: var\(--operator\)/);
  });

  // A reserved slot with no rule of its own is indistinguishable from a kind the run DID reach,
  // which would turn the checklist into a lie; a weight tier with no rule is invisible.
  it("holds the empty slot and the weight tiers apart from a lit glyph", () => {
    expect(viewsCss).toMatch(/\.spark \.gly\.off \{[^}]*border-style: dashed/);
    for (const tier of ["light", "mid", "heavy"]) {
      expect(viewsCss).toMatch(new RegExp(`\\.spark \\.gly\\.wt-${tier} \\{[^}]*color:`));
    }
  });

  it("gives Needs you the operator's own colour", () => {
    expect(consoleCss).toMatch(/\.stat\.op \.n \{[^}]*color: var\(--operator\)/);
  });
});
