import { describe, expect, it } from "vitest";
import type { RunSummary, TeamsOverview } from "@/lib/api";
import type { JobRow } from "@/lib/runs-model";
import {
  CONSOLE_JOB_FILTERS,
  buildConsoleJobs,
  consoleJobCounts,
  consoleJobProjects,
  consoleJobStatus,
  filterConsoleJobs,
  lastActivityByIssue,
  mateStates,
  relativeSince,
  ticketAssignees,
} from "./console-jobs";

const NOW = Date.parse("2026-09-01T12:00:00Z");

function job(over: Partial<JobRow> & Pick<JobRow, "issue" | "status">): JobRow {
  return {
    key: `k-${over.issue}`,
    runId: 1,
    title: `${over.issue} title`,
    agent: "",
    agentColor: "",
    project: "rhapsody",
    projectShort: "Rhapsody",
    turn: 1,
    tokens: "1k",
    duration: "1m",
    durationAccent: false,
    live: over.status === "running",
    startedAtMs: NOW - 60_000,
    ...over,
  } as JobRow;
}

function issueRow(over: Partial<RunSummary> & Pick<RunSummary, "issue_identifier">): RunSummary {
  return {
    id: 1,
    issue_id: "i",
    title: "t",
    attempt: 1,
    session_uuid: "s",
    branch: "symphony/X",
    project_slug: "rhapsody",
    repo: "",
    started_at: "2026-09-01T11:00:00Z",
    ended_at: "",
    outcome: "completed",
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

describe("consoleJobStatus", () => {
  it("renames each daemon job status into the console's vocabulary", () => {
    expect(consoleJobStatus("running")).toBe("run");
    // A clean run hands its ticket to the review state — that is the pipeline's own rule.
    expect(consoleJobStatus("completed")).toBe("review");
    expect(consoleJobStatus("failed")).toBe("blocked");
    expect(consoleJobStatus("waiting")).toBe("blocked");
    expect(consoleJobStatus("stopped")).toBe("queued");
  });
});

describe("relativeSince", () => {
  it("renders each magnitude", () => {
    expect(relativeSince(NOW - 30_000, NOW)).toBe("just now");
    expect(relativeSince(NOW - 6 * 60_000, NOW)).toBe("6m ago");
    expect(relativeSince(NOW - 5 * 3600_000, NOW)).toBe("5h ago");
    expect(relativeSince(NOW - 3 * 86_400_000, NOW)).toBe("3d ago");
  });

  it("reads an unknown or skewed instant as no information, never a negative age", () => {
    expect(relativeSince(0, NOW)).toBe("—");
    expect(relativeSince(NOW + 60_000, NOW)).toBe("—");
  });
});

describe("ticketAssignees / mateStates", () => {
  const overview: TeamsOverview = {
    enabled: true,
    manager_mode: "labels",
    default_identity: "",
    backend: "local",
    roster: [
      { name: "alice", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 1, tickets: ["STUDIO-1"] },
      { name: "jimmy", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: [] },
    ],
  };

  it("maps a live ticket to its teammate", () => {
    expect(ticketAssignees(overview).get("STUDIO-1")).toBe("alice");
    expect(ticketAssignees(overview).has("STUDIO-2")).toBe(false);
    expect(ticketAssignees(undefined).size).toBe(0);
  });

  it("reports each teammate's live state for the Now strip", () => {
    expect(mateStates(overview)).toEqual([
      { name: "alice", task: "STUDIO-1", running: true },
      { name: "jimmy", task: "idle", running: false },
    ]);
  });
});

describe("lastActivityByIssue", () => {
  it("prefers a run's end over its start and keeps the newest per ticket", () => {
    const got = lastActivityByIssue([
      issueRow({ issue_identifier: "A", started_at: "2026-09-01T09:00:00Z", ended_at: "2026-09-01T10:00:00Z" }),
      issueRow({ issue_identifier: "A", started_at: "2026-09-01T11:00:00Z", ended_at: "" }),
    ]);
    expect(got.get("A")).toBe(Date.parse("2026-09-01T11:00:00Z"));
  });
});

describe("buildConsoleJobs", () => {
  // §10 box 2.6 — the Now-strip counts come from the issues data, not a hardcoded strip.
  it("counts running / in review / queued / blocked", () => {
    const rows = buildConsoleJobs(
      [
        job({ issue: "A", status: "running" }),
        job({ issue: "B", status: "completed" }),
        job({ issue: "C", status: "completed" }),
        job({ issue: "D", status: "stopped" }),
        job({ issue: "E", status: "failed" }),
        job({ issue: "F", status: "waiting" }),
      ],
      [],
      undefined,
      NOW,
    );
    expect(consoleJobCounts(rows)).toEqual({ running: 1, review: 2, queued: 1, blocked: 2 });
  });

  it("pins running tickets first, then orders by newest activity", () => {
    const rows = buildConsoleJobs(
      [
        job({ issue: "OLD", status: "completed", startedAtMs: NOW - 9 * 3600_000 }),
        job({ issue: "NEW", status: "completed", startedAtMs: NOW - 60_000 }),
        job({ issue: "LIVE", status: "running", startedAtMs: NOW - 100 * 3600_000 }),
      ],
      [],
      undefined,
      NOW,
    );
    expect(rows.map((r) => r.issue)).toEqual(["LIVE", "NEW", "OLD"]);
  });

  it("attributes a live ticket to its teammate and leaves the rest unassigned", () => {
    const overview: TeamsOverview = {
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [
        { name: "alice", profile: "p", labels: [], bank: "b", max_concurrent: 1, live_runs: 1, tickets: ["A"] },
      ],
    };
    const rows = buildConsoleJobs(
      [job({ issue: "A", status: "running" }), job({ issue: "B", status: "completed" })],
      [],
      overview,
      NOW,
    );
    expect(rows.find((r) => r.issue === "A")?.assignee).toBe("alice");
    expect(rows.find((r) => r.issue === "B")?.assignee).toBe("");
  });

  it("takes Updated from the newest run's end, not the merge's start time", () => {
    const rows = buildConsoleJobs(
      [job({ issue: "A", status: "completed", startedAtMs: NOW - 5 * 3600_000 })],
      [issueRow({ issue_identifier: "A", ended_at: "2026-09-01T11:54:00Z" })],
      undefined,
      NOW,
    );
    expect(rows[0].updated).toBe("6m ago");
  });
});

describe("filterConsoleJobs", () => {
  const rows = buildConsoleJobs(
    [
      job({ issue: "A", status: "running", project: "rhapsody", projectShort: "Rhapsody" }),
      job({ issue: "B", status: "completed", project: "rhapsody", projectShort: "Rhapsody" }),
      job({ issue: "C", status: "completed", project: "booch", projectShort: "Booch 1.0 Launch" }),
      job({ issue: "D", status: "stopped", project: "booch", projectShort: "Booch 1.0 Launch" }),
    ],
    [],
    undefined,
    NOW,
  );

  // §10 box 2.7 — the status Seg filters the table.
  it("filters by status", () => {
    expect(filterConsoleJobs(rows, "all", "").map((r) => r.issue).sort()).toEqual(["A", "B", "C", "D"]);
    expect(filterConsoleJobs(rows, "review", "").map((r) => r.issue).sort()).toEqual(["B", "C"]);
    expect(filterConsoleJobs(rows, "run", "").map((r) => r.issue)).toEqual(["A"]);
    expect(filterConsoleJobs(rows, "queued", "").map((r) => r.issue)).toEqual(["D"]);
  });

  // §10 box 2.7 — the project Select filters by project.
  it("filters by project, and composes with the status filter", () => {
    expect(filterConsoleJobs(rows, "all", "booch").map((r) => r.issue).sort()).toEqual(["C", "D"]);
    expect(filterConsoleJobs(rows, "review", "booch").map((r) => r.issue)).toEqual(["C"]);
  });

  it("offers every project present in the rows, by display name", () => {
    expect(consoleJobProjects(rows)).toEqual([
      { value: "booch", label: "Booch 1.0 Launch" },
      { value: "rhapsody", label: "Rhapsody" },
    ]);
  });

  it("keeps the Seg's options and the filter ids in step", () => {
    for (const f of CONSOLE_JOB_FILTERS) {
      expect(() => filterConsoleJobs(rows, f.id, "")).not.toThrow();
    }
  });
});
