import { describe, expect, it } from "vitest";
import type { IssueRun, TeamsOverview } from "@/lib/api";
import type { JobRow } from "@/lib/runs-model";
import {
  CONSOLE_JOB_FILTERS,
  buildConsoleJobs,
  consoleJobCounts,
  consoleJobProjects,
  consoleJobStatus,
  durableAssignees,
  filterConsoleJobs,
  lastActivityByIssue,
  lifecycleByIssue,
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

function issueRow(over: Partial<IssueRun> & Pick<IssueRun, "issue_identifier">): IssueRun {
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
  } as IssueRun;
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

  // STUDIO-702 — the ticket's real state outranks the run outcome. Without it every completed run
  // read as "in review" forever, however long ago the ticket merged.
  it("prefers the ticket's lifecycle over the run outcome", () => {
    expect(consoleJobStatus("completed", "done")).toBe("done");
    expect(consoleJobStatus("completed", "canceled")).toBe("done");
    expect(consoleJobStatus("completed", "in_review")).toBe("review");
    // Reopened: the run finished but the ticket is open work again, so nothing awaits a reviewer.
    expect(consoleJobStatus("completed", "open")).toBe("queued");
    expect(consoleJobStatus("stopped", "done")).toBe("done");
  });

  // A live run outranks everything: a mid-run handoff parks the ticket in a review state while the
  // agent is still working, and the worklist must keep saying "running".
  it("keeps a live run running whatever the ticket says", () => {
    expect(consoleJobStatus("running", "in_review")).toBe("run");
    expect(consoleJobStatus("running", "done")).toBe("run");
  });

  // Failure is about the RUN, and a human still has to act on it.
  it("keeps a failed or held run blocked while its ticket is open", () => {
    expect(consoleJobStatus("failed", "open")).toBe("blocked");
    expect(consoleJobStatus("waiting", "open")).toBe("blocked");
  });

  // No answer, or one this build does not know, falls back to exactly the old mapping.
  it("falls back to the run outcome when the daemon has no answer", () => {
    expect(consoleJobStatus("completed", undefined)).toBe("review");
    expect(consoleJobStatus("completed", "")).toBe("review");
    expect(consoleJobStatus("completed", "some_future_state")).toBe("review");
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

describe("lifecycleByIssue", () => {
  it("keys each ticket's lifecycle and raw state by identifier, skipping rows with no answer", () => {
    const got = lifecycleByIssue([
      issueRow({ issue_identifier: "A", lifecycle: "done", tracker_state: "Done" }),
      issueRow({ issue_identifier: "B" }),
    ]);
    expect(got.get("A")).toEqual({ lifecycle: "done", trackerState: "Done" });
    expect(got.has("B")).toBe(false);
  });

  // The listing is one row per issue, but a duplicate must not let an older answer win.
  it("keeps the first answer for a ticket", () => {
    const got = lifecycleByIssue([
      issueRow({ issue_identifier: "A", lifecycle: "done", tracker_state: "Done" }),
      issueRow({ issue_identifier: "A", lifecycle: "open", tracker_state: "Todo" }),
    ]);
    expect(got.get("A")?.lifecycle).toBe("done");
  });
});

describe("buildConsoleJobs", () => {
  // STUDIO-702 — the acceptance case: a merged ticket reads "done", the "in review" count holds
  // only work actually awaiting a reviewer, and the Done tab has something to show.
  it("colours each row from the ticket's lifecycle, not from run history", () => {
    const rows = buildConsoleJobs(
      [
        job({ issue: "MERGED", status: "completed" }),
        job({ issue: "REVIEW", status: "completed" }),
        job({ issue: "REOPENED", status: "completed" }),
        job({ issue: "UNKNOWN", status: "completed" }),
      ],
      [
        issueRow({ issue_identifier: "MERGED", lifecycle: "done", tracker_state: "Done" }),
        issueRow({ issue_identifier: "REVIEW", lifecycle: "in_review", tracker_state: "In Review" }),
        issueRow({ issue_identifier: "REOPENED", lifecycle: "open", tracker_state: "Todo" }),
        issueRow({ issue_identifier: "UNKNOWN" }),
      ],
      undefined,
      NOW,
    );
    const status = (issue: string) => rows.find((r) => r.issue === issue)?.status;
    expect(status("MERGED")).toBe("done");
    expect(status("REVIEW")).toBe("review");
    expect(status("REOPENED")).toBe("queued");
    // No answer => the old behaviour, unchanged.
    expect(status("UNKNOWN")).toBe("review");
    // Only REVIEW and the unresolved UNKNOWN count as awaiting a reviewer; MERGED no longer does.
    expect(consoleJobCounts(rows)).toEqual({ running: 0, review: 2, queued: 1, blocked: 0 });
  });

  // The Done tab was permanently empty because `done` was unreachable — §3's filter Seg.
  it("populates the Done filter", () => {
    const rows = buildConsoleJobs(
      [job({ issue: "A", status: "completed" }), job({ issue: "B", status: "completed" })],
      [
        issueRow({ issue_identifier: "A", lifecycle: "done", tracker_state: "Done" }),
        issueRow({ issue_identifier: "B", lifecycle: "in_review", tracker_state: "In Review" }),
      ],
      undefined,
      NOW,
    );
    expect(filterConsoleJobs(rows, "done", "").map((r) => r.issue)).toEqual(["A"]);
    expect(filterConsoleJobs(rows, "review", "").map((r) => r.issue)).toEqual(["B"]);
  });

  // The raw workflow-state name is the auditable ground truth behind the normalized bucket.
  it("carries the tracker's own state name onto the row", () => {
    const rows = buildConsoleJobs(
      [job({ issue: "A", status: "completed" }), job({ issue: "B", status: "completed" })],
      [issueRow({ issue_identifier: "A", lifecycle: "canceled", tracker_state: "Won't Do" })],
      undefined,
      NOW,
    );
    expect(rows.find((r) => r.issue === "A")?.trackerState).toBe("Won't Do");
    expect(rows.find((r) => r.issue === "B")?.trackerState).toBe("");
  });

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

  // THE BUG (STUDIO-735): the ASSIGNED column showed a teammate only while the job was running,
  // because the live roster is the only place the console looked. A done or in-review job now keeps
  // the teammate the daemon recorded on its history row.
  it("keeps the teammate on a job that has left running", () => {
    const rows = buildConsoleJobs(
      [
        job({ issue: "DONE", status: "completed" }),
        job({ issue: "REVIEW", status: "completed" }),
      ],
      [
        issueRow({ issue_identifier: "DONE", lifecycle: "done", assignee: "alice" }),
        issueRow({ issue_identifier: "REVIEW", lifecycle: "in_review", assignee: "jimmy" }),
      ],
      // Nobody is live: the roster that used to be the only source knows neither ticket.
      undefined,
      NOW,
    );
    expect(rows.find((r) => r.issue === "DONE")?.assignee).toBe("alice");
    expect(rows.find((r) => r.issue === "REVIEW")?.assignee).toBe("jimmy");
  });

  // A run dispatched moments ago may not have a decorated history row yet, so the live roster stays
  // the fallback — and the durable record outranks it when both answer.
  it("falls back to the live roster only for a row with no durable assignee", () => {
    const overview: TeamsOverview = {
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [
        { name: "jimmy", profile: "p", labels: [], bank: "b", max_concurrent: 1, live_runs: 2, tickets: ["FRESH", "DONE"] },
      ],
    };
    const rows = buildConsoleJobs(
      [job({ issue: "FRESH", status: "running" }), job({ issue: "DONE", status: "completed" })],
      [issueRow({ issue_identifier: "DONE", lifecycle: "done", assignee: "alice" })],
      overview,
      NOW,
    );
    expect(rows.find((r) => r.issue === "FRESH")?.assignee).toBe("jimmy");
    expect(rows.find((r) => r.issue === "DONE")?.assignee).toBe("alice");
  });

  // A ticket nobody was routed for — solo, or a Teams-off daemon — stays "—" rather than borrowing
  // a name from anywhere.
  it("leaves a solo or Teams-off job unassigned", () => {
    const rows = buildConsoleJobs(
      [job({ issue: "SOLO", status: "completed" })],
      [issueRow({ issue_identifier: "SOLO", lifecycle: "done" })],
      undefined,
      NOW,
    );
    expect(rows[0].assignee).toBe("");
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

describe("durableAssignees", () => {
  it("reads the daemon's own assignee off each row and skips the rows without one", () => {
    const by = durableAssignees([
      issueRow({ issue_identifier: "A", assignee: "alice" }),
      issueRow({ issue_identifier: "B" }),
      issueRow({ issue_identifier: "" , assignee: "ghost" }),
    ]);
    expect(by.get("A")).toBe("alice");
    expect(by.has("B")).toBe(false);
    expect(by.has("")).toBe(false);
  });

  it("lets the first answer win when a ticket somehow has two rows", () => {
    const by = durableAssignees([
      issueRow({ issue_identifier: "A", assignee: "alice" }),
      issueRow({ issue_identifier: "A", assignee: "jimmy" }),
    ]);
    expect(by.get("A")).toBe("alice");
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
