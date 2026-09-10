import { describe, expect, it } from "vitest";
import type { IssueCountsResponse, IssueRun, TeamsOverview } from "@/lib/api";
import type { JobRow } from "@/lib/runs-model";
import {
  CONSOLE_JOB_FILTERS,
  JOBS_PAGE_SIZE,
  buildConsoleJobs,
  consoleJobsPageNote,
  consoleJobCounts,
  consoleJobProjects,
  consoleJobStatus,
  consoleStoreCounts,
  durableAssignees,
  filterConsoleJobs,
  lastActivityByIssue,
  lifecycleByIssue,
  mateStates,
  needsOperator,
  relativeSince,
  reviewRunIssues,
  reviewTicketIssues,
  statusNote,
  ticketAssignees,
} from "./console-jobs";
import { runOutcomeLabel } from "./console-job-detail";

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

// STUDIO-780 — "in review" was doing double duty: an agent whose whole job is to review a
// teammate's pull request, and a ticket whose work is finished and is awaiting one. `reviewing` is
// the first of those, and the signal is the daemon's `review_ticket` marker, never the title.
describe("consoleJobStatus on a review ticket", () => {
  it("says reviewing when an agent is actively reviewing", () => {
    expect(consoleJobStatus("running", undefined, true)).toBe("reviewing");
    // A mid-run handoff parks the ticket while the agent is still working; the run still wins.
    expect(consoleJobStatus("running", "in_review", true)).toBe("reviewing");
  });

  // The whole point of the split: the two claims must not resolve to the same word.
  it("still says in review for an implementation ticket awaiting one", () => {
    expect(consoleJobStatus("running", undefined, false)).toBe("run");
    expect(consoleJobStatus("completed", "in_review", false)).toBe("review");
  });

  // The marker changes the word for LIVE work only. A review ticket whose run is over is parked
  // awaiting a person exactly as any other ticket in that state is, and terminal is terminal.
  it("does not change any state but the live one", () => {
    expect(consoleJobStatus("completed", "in_review", true)).toBe("review");
    expect(consoleJobStatus("completed", "done", true)).toBe("done");
    expect(consoleJobStatus("stopped", "open", true)).toBe("queued");
    expect(consoleJobStatus("failed", "open", true)).toBe("blocked");
  });

  // A daemon that does not serve the field behaves exactly as it did before it existed.
  it("defaults to the pre-existing mapping when nothing is known", () => {
    expect(consoleJobStatus("running")).toBe("run");
  });
});

// STUDIO-826 — a TICKETLESS review job (`review.mode: ticketless`) is a run against a pull request
// with no tracker ticket behind it, so `/api/v1/history/issues` serves it with NO `lifecycle` and no
// `review_ticket`: there is no ticket to resolve a state for and none to carry the marker label.
// Every row below therefore leaves both fields off, because that absence IS the bug — a fixture that
// supplies a lifecycle passes against the broken code.
//
// Both halves of the status were wrong. Live, the row said "running" because the only route to
// `reviewing` was the ticket label. Finished, it said "in review" — `completed → review`, "the work
// now awaits somebody's review" — which is the inverse of what a finished review means, and it
// billed the operator's "Needs you" for a job that needed nobody.
describe("a ticketless review job", () => {
  it("says reviewing while the review run is live", () => {
    expect(consoleJobStatus("running", undefined, false, true)).toBe("reviewing");
  });

  // The half a label-only reading of this ticket would miss: the review IS the work, so its
  // completion is terminal. Nothing is handed to a reviewer, because the reviewer just left.
  it("says done when the review run has finished", () => {
    expect(consoleJobStatus("completed", undefined, false, true)).toBe("done");
  });

  // A failed review genuinely does need a person — it is the one review outcome that does.
  it("still says blocked when the review run failed", () => {
    expect(consoleJobStatus("failed", undefined, false, true)).toBe("blocked");
    expect(consoleJobStatus("waiting", undefined, false, true)).toBe("blocked");
  });

  it("leaves a stopped review run queued for its next dispatch", () => {
    expect(consoleJobStatus("stopped", undefined, false, true)).toBe("queued");
  });

  // The guard on the fix: an ORDINARY ticket the daemon could not resolve a lifecycle for still
  // infers "in review" from its completed outcome, exactly as it did before. That inference is
  // wrong only for a review run, and the flag is the whole difference.
  it("changes nothing about a row that is not a review run", () => {
    expect(consoleJobStatus("completed", undefined, false, false)).toBe("review");
    expect(consoleJobStatus("running", undefined, false, false)).toBe("run");
  });

  // STUDIO-780's rows keep their behaviour untouched: a review TICKET parked in its tracker's
  // review state is awaiting a person's read, and terminal is still terminal.
  it("leaves a ticket-based review row alone", () => {
    expect(consoleJobStatus("completed", "in_review", true, false)).toBe("review");
    expect(consoleJobStatus("running", "in_review", true, false)).toBe("reviewing");
    expect(consoleJobStatus("completed", "done", true, false)).toBe("done");
  });
});

describe("reviewRunIssues", () => {
  it("collects only the rows the daemon marked", () => {
    const got = reviewRunIssues([
      issueRow({ issue_identifier: "pr:acme/x#1@alice", review_run: true }),
      issueRow({ issue_identifier: "MT-1" }),
      issueRow({ issue_identifier: "MT-2", review_run: false }),
    ]);
    expect([...got]).toEqual(["pr:acme/x#1@alice"]);
  });

  // The daemon reads the run's own id; nothing on this side parses one. A key that merely LOOKS
  // like a review run, or a title that opens with the word, is not the signal.
  it("never reads the key or the title", () => {
    const got = reviewRunIssues([
      issueRow({
        issue_identifier: "pr:acme/x#2@jimmy",
        title: "Review acme/x#2 at abc1234",
      }),
    ]);
    expect(got.size).toBe(0);
  });

  it("ignores a row with no key", () => {
    expect(reviewRunIssues([issueRow({ issue_identifier: "", review_run: true })]).size).toBe(0);
  });
});

describe("reviewTicketIssues", () => {
  it("collects only the tickets the daemon marked", () => {
    const got = reviewTicketIssues([
      issueRow({ issue_identifier: "REVIEW", review_ticket: true }),
      issueRow({ issue_identifier: "IMPL" }),
      issueRow({ issue_identifier: "EXPLICIT", review_ticket: false }),
    ]);
    expect([...got]).toEqual(["REVIEW"]);
  });

  // The signal is the daemon's marker. A title is a convention, and a hand-written ticket that
  // happens to open with the word is not a review ticket.
  it("never reads the title", () => {
    const got = reviewTicketIssues([
      issueRow({ issue_identifier: "IMPL", title: "Review: STUDIO-1 do the thing" }),
    ]);
    expect(got.size).toBe(0);
  });

  it("ignores a row with no ticket key", () => {
    expect(reviewTicketIssues([issueRow({ issue_identifier: "", review_ticket: true })]).size).toBe(
      0,
    );
  });
});

// STUDIO-780 problem 2 — the row painted the TICKET's lifecycle, the run detail painted the RUN's
// outcome, and with no cue which subject either word belonged to the list read as stuck: "they are
// stuck in 'in review' in the dashboard, and when I click in, they are all done".
describe("statusNote", () => {
  it("states the run's own outcome beside a ticket parked in review", () => {
    expect(statusNote("review", "completed", true)).toBe("run done");
  });

  it("uses the same word the run detail's header prints", () => {
    expect(statusNote("review", "completed", true)).toBe(`run ${runOutcomeLabel("completed")}`);
    expect(statusNote("done", "failed", true)).toBe(`run ${runOutcomeLabel("failed")}`);
  });

  // The status was inferred FROM the outcome, so the two are one fact and a note would only
  // restate the pill in other words.
  it("says nothing when the daemon never resolved a lifecycle", () => {
    expect(statusNote("review", "completed", false)).toBeUndefined();
  });

  // A live run IS the row's status.
  it("says nothing about a run still going", () => {
    expect(statusNote("run", "running", true)).toBeUndefined();
    expect(statusNote("blocked", "waiting", true)).toBeUndefined();
  });

  // Same subject, same word — nothing diverged, so nothing to reconcile.
  it("says nothing when the two words agree", () => {
    expect(statusNote("done", "completed", true)).toBeUndefined();
  });

  // A merged ticket whose run failed is the other direction of the same confusion.
  it("states a failure the ticket's own state hides", () => {
    expect(statusNote("done", "failed", true)).toBe("run failed");
    expect(statusNote("review", "failed", true)).toBe("run failed");
  });

  it("says nothing about an outcome it has no word for", () => {
    expect(statusNote("review", "", true)).toBeUndefined();
  });
});

// The fourth condition, which lives in the builder: a failed row's `subLabel` IS the error, and
// "blocked · run failed · <error>" spends a third of the pill restating what follows it.
describe("statusNote on a row that already explains itself", () => {
  it("leaves a failed row's error to speak for the run", () => {
    const rows = buildConsoleJobs(
      [job({ issue: "BROKE", status: "failed", subLabel: "boom" })],
      [issueRow({ issue_identifier: "BROKE", lifecycle: "done" })],
      undefined,
      NOW,
    );
    expect(rows[0]?.subLabel).toBe("boom");
    expect(rows[0]?.statusNote).toBeUndefined();
  });

  // Without the guard the same row would carry both — this is what is being suppressed.
  it("would otherwise have had one", () => {
    expect(statusNote("done", "failed", true)).toBe("run failed");
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
      { name: "alice", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 1, tickets: ["STUDIO-1"], queued: 0 },
      { name: "jimmy", profile: "swe", labels: [], bank: "b", max_concurrent: 1, live_runs: 0, tickets: [], queued: 0 },
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
    // Both are on the operator's list (STUDIO-743): the tracker answered for this payload, so the
    // count is knowable, and a row parked in review awaits a person however it got there.
    expect(consoleJobCounts(rows)).toEqual({
      running: 0,
      review: 2,
      queued: 1,
      blocked: 0,
      needsYou: 2,
    });
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
    // The four pills count rows the daemon definitely served, so they answer here. needsYou does
    // not (STUDIO-743): no issue rows were passed at all, which is the shape a cold lifecycle cache
    // serves, and B and C only read "in review" because a `completed` outcome was inferred into it.
    // A number off that would be a guess dressed as a count, so the strip says "—" instead.
    expect(consoleJobCounts(rows)).toEqual({
      running: 1,
      review: 2,
      queued: 1,
      blocked: 2,
      needsYou: null,
    });
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
        { name: "jimmy", profile: "p", labels: [], bank: "b", max_concurrent: 1, live_runs: 2, tickets: ["FRESH", "DONE"], queued: 0 },
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
        { name: "alice", profile: "p", labels: [], bank: "b", max_concurrent: 1, live_runs: 1, tickets: ["A"], queued: 0 },
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

  // STUDIO-780 — `reviewing` gets no Seg button of its own: it is a kind of RUNNING, so "Running"
  // covers it and "In review" (which means "parked, waiting on a person") does not. A live row that
  // answered to no button would vanish from every filter but "All".
  describe("with a review ticket being reviewed", () => {
    const withReviewing = buildConsoleJobs(
      [
        job({ issue: "A", status: "running" }),
        job({ issue: "R", status: "running" }),
        job({ issue: "B", status: "completed" }),
      ],
      [issueRow({ issue_identifier: "R", review_ticket: true })],
      undefined,
      NOW,
    );

    it("files reviewing under Running, not under In review", () => {
      expect(filterConsoleJobs(withReviewing, "run", "").map((r) => r.issue).sort()).toEqual([
        "A",
        "R",
      ]);
      expect(filterConsoleJobs(withReviewing, "review", "").map((r) => r.issue)).toEqual(["B"]);
    });

    it("leaves no row unreachable from the Seg", () => {
      const reachable = new Set(
        CONSOLE_JOB_FILTERS.filter((f) => f.id !== "all").flatMap((f) =>
          filterConsoleJobs(withReviewing, f.id, "").map((r) => r.issue),
        ),
      );
      expect([...reachable].sort()).toEqual(["A", "B", "R"]);
    });

    it("counts reviewing as running in the Now strip", () => {
      expect(consoleJobCounts(withReviewing).running).toBe(2);
    });

    // The pin exists to surface live work; a reviewing row is live work.
    it("pins reviewing to the top beside running", () => {
      expect(withReviewing.slice(0, 2).map((r) => r.status).sort()).toEqual([
        "reviewing",
        "run",
      ]);
    });

    it("labels the reviewing row, and leaves an ordinary live row alone", () => {
      const row = withReviewing.find((r) => r.issue === "R");
      expect(row?.status).toBe("reviewing");
      expect(row?.statusLabel).toBe("reviewing");
      // "A" is running too, and is NOT a review ticket: the narrowing must not reach it.
      const ordinary = withReviewing.find((r) => r.issue === "A");
      expect(ordinary?.status).toBe("run");
      expect(ordinary?.statusLabel).toBe("running");
    });

    // An agent has it, so it is not the operator's move.
    it("does not bill a reviewing row to Needs you", () => {
      expect(needsOperator("reviewing", "running")).toBe(false);
    });
  });

  // STUDIO-780 problem 2, end to end through the builder.
  describe("with a parked ticket whose run has finished", () => {
    const parked = buildConsoleJobs(
      [job({ issue: "PARKED", status: "completed" }), job({ issue: "GUESSED", status: "completed" })],
      [issueRow({ issue_identifier: "PARKED", lifecycle: "in_review" })],
      undefined,
      NOW,
    );
    const row = (issue: string) => parked.find((r) => r.issue === issue);

    it("says the run is done beside the ticket being in review", () => {
      expect(row("PARKED")?.statusLabel).toBe("in review");
      expect(row("PARKED")?.statusNote).toBe("run done");
    });

    it("adds nothing to a row whose status was only inferred from that same outcome", () => {
      expect(row("GUESSED")?.statusLabel).toBe("in review");
      expect(row("GUESSED")?.statusNote).toBeUndefined();
    });
  });
});

// STUDIO-743 (design record §6) — the Now strip's fifth stat. "Needs you" is the operator's own
// queue: the tickets whose next move is a HUMAN's rather than an agent's.
describe("needsOperator", () => {
  it("counts a ticket parked in review — a human's merge or verdict is the next move", () => {
    expect(needsOperator("review", "completed")).toBe(true);
  });

  it("counts a failed run — a person has to decide what happens next", () => {
    expect(needsOperator("blocked", "failed")).toBe(true);
  });

  // A held dependent is `blocked` too, but it is waiting on its PREDECESSOR, not on the operator —
  // and that predecessor is itself a row in this worklist, counted there. Counting the dependent
  // as well would bill the same human decision twice.
  it("does not count a ticket held on an uncleared dependency", () => {
    expect(needsOperator("blocked", "waiting")).toBe(false);
  });

  it("counts nothing the daemon is still driving or has finished with", () => {
    expect(needsOperator("run", "running")).toBe(false);
    expect(needsOperator("queued", "stopped")).toBe(false);
    expect(needsOperator("done", "completed")).toBe(false);
  });
});

describe("the Now strip's Needs you count", () => {
  // A healthy tracker: the daemon answered a lifecycle for the tickets it knows, which is what
  // makes the count knowable at all.
  function rows() {
    return buildConsoleJobs(
      [
        job({ issue: "LIVE", status: "running" }),
        job({ issue: "REVIEW", status: "completed" }),
        job({ issue: "MERGED", status: "completed" }),
        job({ issue: "FAILED", status: "failed" }),
        job({ issue: "HELD", status: "waiting" }),
      ],
      [
        issueRow({ issue_identifier: "REVIEW", lifecycle: "in_review" }),
        issueRow({ issue_identifier: "MERGED", lifecycle: "done" }),
        issueRow({ issue_identifier: "LIVE", lifecycle: "open" }),
      ],
      undefined,
      NOW,
    );
  }

  it("marks each row that is waiting on the operator", () => {
    const needs = rows()
      .filter((r) => r.needsYou)
      .map((r) => r.issue)
      .sort();
    expect(needs).toEqual(["FAILED", "REVIEW"]);
  });

  it("counts them alongside the four existing pills", () => {
    expect(consoleJobCounts(rows())).toEqual({
      running: 1,
      review: 1,
      queued: 0,
      blocked: 2,
      needsYou: 2,
    });
  });

  // It is a count of a DIFFERENT set from the in-review tally: a failed run needs a human without
  // reading "in review", and a held dependent is blocked without needing one. On a healthy tracker
  // the review rows are all genuinely parked for a person, so the two numbers do sit close together
  // — the failed runs are the difference — and that convergence is the honest answer rather than a
  // defect. It is also why the strip paints only this one of them now (David, 2026-09-03): two
  // pills reporting one question read as a duplicate. What this pins is the membership, in both
  // directions, so neither number can quietly become the other.
  it("is a different set from the in-review tally in both directions", () => {
    const byIssue = new Map(rows().map((r) => [r.issue, r]));
    expect(byIssue.get("FAILED")).toMatchObject({ status: "blocked", needsYou: true });
    expect(byIssue.get("HELD")).toMatchObject({ status: "blocked", needsYou: false });
    expect(byIssue.get("REVIEW")).toMatchObject({ status: "review", needsYou: true });
    expect(byIssue.get("MERGED")).toMatchObject({ status: "done", needsYou: false });
  });

  // THE FAILURE DIRECTION, AND THE REASON THE COUNT IS NULLABLE. `issue_lifecycles` answers off a
  // TTL cache and the tracker AT REQUEST TIME, so a cold cache or a failed Linear round-trip
  // returns rows stripped of every `lifecycle` — this exact payload. `consoleJobStatus` then maps
  // each `completed` outcome to "in review" by inference, so the review tally INFLATES at the same
  // moment the daemon has the least idea what is true. A count of 0 there would be a claim that
  // nothing awaits the operator, which is the one thing the console cannot know; `null` renders
  // "—" instead. This is deliberately a property of the PAYLOAD, not of any single row.
  it("reads unknown, never zero, when the payload resolved no lifecycle at all", () => {
    const outage = buildConsoleJobs(
      [
        job({ issue: "REVIEW", status: "completed" }),
        job({ issue: "MERGED", status: "completed" }),
        job({ issue: "LIVE", status: "running" }),
      ],
      // Exactly what the endpoint serves on a cold cache: the runs are all still there, and not one
      // of them carries a tracker answer.
      [
        issueRow({ issue_identifier: "REVIEW", lifecycle: undefined }),
        issueRow({ issue_identifier: "MERGED", lifecycle: undefined }),
      ],
      undefined,
      NOW,
    );
    // The inference has inflated the review tally — both finished runs read "in review" — which is
    // precisely why the operator's own count must not answer off it.
    expect(consoleJobCounts(outage)).toEqual({
      running: 1,
      review: 2,
      queued: 0,
      blocked: 0,
      needsYou: null,
    });
  });

  // The gate is "did the tracker answer for this payload", not "did it answer for this row" — a
  // ticket the tracker does not know about does not make the whole count unknowable.
  it("still counts when the tracker answered for only some of the page", () => {
    const partial = buildConsoleJobs(
      [job({ issue: "REVIEW", status: "completed" }), job({ issue: "UNKNOWN", status: "failed" })],
      [issueRow({ issue_identifier: "REVIEW", lifecycle: "in_review" })],
      undefined,
      NOW,
    );
    expect(consoleJobCounts(partial).needsYou).toBe(2);
  });

  // An empty worklist is not an outage: there is genuinely nothing waiting on anybody, and "—"
  // there would be a shrug where a fact is available.
  it("reads zero, not unknown, for an empty worklist", () => {
    expect(consoleJobCounts([])).toEqual({
      running: 0,
      review: 0,
      queued: 0,
      blocked: 0,
      needsYou: 0,
    });
  });
});

// STUDIO-826, end to end through the builder and the strip — David's own worklist, with both
// teammates idle and 0 running:
//
//   pr:makewhatis/rhapsody#135@jimmy · Review makewhatis/rhapsody#135 at 34a573c   [ in review ]
//   pr:makewhatis/rhapsody#136@alice · Review makewhatis/rhapsody#136 at 7b8b9e1   [ in review ]
//
// Both runs had FINISHED, and both pushed "Needs you" up while needing nobody. The payload below is
// the shape `/api/v1/history/issues` actually served for them, measured on the live daemon: the
// `pr:` rows carry `review_run` and NOTHING else — no `lifecycle`, no `review_ticket` — because
// there is no ticket behind them. The ordinary ticket beside them is what a real page always has,
// and it is also what keeps the count knowable (see the `needsYou` nullability above).
describe("a page of ticketless review jobs", () => {
  const rows = buildConsoleJobs(
    [
      job({ issue: "pr:makewhatis/rhapsody#135@jimmy", status: "completed" }),
      job({ issue: "pr:makewhatis/rhapsody#136@alice", status: "completed" }),
      job({ issue: "pr:makewhatis/rhapsody#137@alice", status: "running" }),
      job({ issue: "pr:makewhatis/rhapsody#138@jimmy", status: "failed" }),
      job({ issue: "STUDIO-712", status: "completed" }),
    ],
    [
      issueRow({ issue_identifier: "pr:makewhatis/rhapsody#135@jimmy", review_run: true }),
      issueRow({ issue_identifier: "pr:makewhatis/rhapsody#136@alice", review_run: true }),
      issueRow({ issue_identifier: "pr:makewhatis/rhapsody#137@alice", review_run: true }),
      issueRow({ issue_identifier: "pr:makewhatis/rhapsody#138@jimmy", review_run: true }),
      issueRow({ issue_identifier: "STUDIO-712", lifecycle: "in_review" }),
    ],
    undefined,
    NOW,
  );
  const row = (issue: string) => rows.find((r) => r.issue === issue);

  it("paints the pill each review job's own run earned", () => {
    expect(row("pr:makewhatis/rhapsody#137@alice")?.statusLabel).toBe("reviewing");
    expect(row("pr:makewhatis/rhapsody#135@jimmy")?.statusLabel).toBe("done");
    expect(row("pr:makewhatis/rhapsody#136@alice")?.statusLabel).toBe("done");
    expect(row("pr:makewhatis/rhapsody#138@jimmy")?.statusLabel).toBe("blocked");
    // The ordinary ticket on the same page is untouched: its work really does await a reviewer.
    expect(row("STUDIO-712")?.statusLabel).toBe("in review");
  });

  // The half a pill-only test would miss, and the half David actually complained about: the strip
  // said four tickets needed him while nothing did. Before the fix these numbers read
  // `review: 3, needsYou: 4` — the three finished-or-live review runs inferred into "in review",
  // plus the failed one.
  it("bills Needs you for the failed review only", () => {
    expect(consoleJobCounts(rows)).toEqual({
      running: 1,
      review: 1,
      queued: 0,
      blocked: 1,
      needsYou: 2,
    });
  });

  it("keeps a finished review out of both the In review stat and Needs you", () => {
    expect(row("pr:makewhatis/rhapsody#135@jimmy")).toMatchObject({
      status: "done",
      needsYou: false,
    });
    expect(row("pr:makewhatis/rhapsody#136@alice")).toMatchObject({
      status: "done",
      needsYou: false,
    });
  });

  // A failed review genuinely does need a person, and says so.
  it("keeps a failed review billed to Needs you", () => {
    expect(row("pr:makewhatis/rhapsody#138@jimmy")).toMatchObject({
      status: "blocked",
      needsYou: true,
    });
  });

  // `reviewing` gets no Seg button of its own (STUDIO-780), and a ticketless one must not change
  // that: the live review job files under "Running", never under "In review", and no row lands in
  // two buckets. (The blocked row answers to no button but "All" — that is the Seg's pre-existing
  // shape, unchanged here: `CONSOLE_JOB_FILTERS` has never offered a "blocked" one.)
  it("keeps the Seg's buckets disjoint, with reviewing under Running", () => {
    expect(filterConsoleJobs(rows, "run", "").map((r) => r.issue)).toEqual([
      "pr:makewhatis/rhapsody#137@alice",
    ]);
    expect(filterConsoleJobs(rows, "review", "").map((r) => r.issue)).toEqual(["STUDIO-712"]);
    const buckets = CONSOLE_JOB_FILTERS.filter((f) => f.id !== "all").flatMap((f) =>
      filterConsoleJobs(rows, f.id, "").map((r) => r.issue),
    );
    expect(buckets.length).toBe(new Set(buckets).size);
    expect([...buckets].sort()).toEqual([
      "STUDIO-712",
      "pr:makewhatis/rhapsody#135@jimmy",
      "pr:makewhatis/rhapsody#136@alice",
      "pr:makewhatis/rhapsody#137@alice",
    ]);
  });

  // No tracker answered for these rows and none ever will, so the row's status is the RUN's — one
  // fact, not two. "done · run done" would be the pill restating itself.
  it("adds no run note to a row that has no ticket behind it", () => {
    expect(row("pr:makewhatis/rhapsody#135@jimmy")?.statusNote).toBeUndefined();
    expect(row("pr:makewhatis/rhapsody#138@jimmy")?.statusNote).toBeUndefined();
  });
});

// The sparkline needs the run it should preview, and whether that run is still going.
describe("the row's run identity", () => {
  it("carries the durable run id and the live flag through to the row", () => {
    const rows = buildConsoleJobs(
      [
        job({ issue: "LIVE", status: "running", runId: 42 }),
        job({ issue: "OFF", status: "completed", runId: 0 }),
      ],
      [],
      undefined,
      NOW,
    );
    const row = (issue: string) => rows.find((r) => r.issue === issue);
    expect(row("LIVE")).toMatchObject({ runId: 42, live: true });
    // Persistence off: no run to read a transcript from, and the row says so rather than guessing.
    expect(row("OFF")).toMatchObject({ runId: 0, live: false });
  });
});

// STUDIO-792. The worklist is served one page at a time and used to end at 50 rows with nothing
// on the page saying so. These are the sentences that make the cut visible — and, when there is
// no cut, say that too, so a list that ends reads as finished rather than merely truncated.
describe("consoleJobsPageNote", () => {
  it("says older jobs are unloaded while the daemon offers another page", () => {
    expect(consoleJobsPageNote({ loaded: 50, visible: 50, hasMore: true, filtered: false })).toBe(
      "Showing the 50 most recent jobs. Older jobs are not loaded yet.",
    );
  });

  // A filter applied to a truncated list is itself truncated, and that is the more dangerous half:
  // "Done · 3 rows" over an unloaded tail reads as a complete answer to a question it never asked.
  it("warns that a filter has not seen the unloaded tail", () => {
    expect(consoleJobsPageNote({ loaded: 50, visible: 3, hasMore: true, filtered: true })).toBe(
      "Showing 3 of the 50 most recent jobs. Older jobs are not loaded yet, so this filter has not been applied to them.",
    );
  });

  it("says the list is complete once the daemon offers no further page", () => {
    expect(consoleJobsPageNote({ loaded: 386, visible: 386, hasMore: false, filtered: false })).toBe(
      "Showing all 386 jobs.",
    );
    expect(consoleJobsPageNote({ loaded: 386, visible: 12, hasMore: false, filtered: true })).toBe(
      "Showing 12 of all 386 jobs.",
    );
  });

  // The empty state has its own message in the table; a second one under it would be noise.
  it("says nothing at all when there are no jobs", () => {
    expect(consoleJobsPageNote({ loaded: 0, visible: 0, hasMore: false, filtered: false })).toBe("");
    expect(consoleJobsPageNote({ loaded: 0, visible: 0, hasMore: true, filtered: false })).toBe("");
  });

  it("counts one job in the singular", () => {
    expect(consoleJobsPageNote({ loaded: 1, visible: 1, hasMore: false, filtered: false })).toBe(
      "Showing all 1 job.",
    );
    expect(consoleJobsPageNote({ loaded: 1, visible: 1, hasMore: true, filtered: false })).toBe(
      "Showing the 1 most recent job. Older jobs are not loaded yet.",
    );
  });

  // The page size the "Load more" step advances by has to be the store's own, or the first click
  // would re-ask for rows already held (smaller) or skip the daemon's page boundary (larger).
  it("steps by the store's own default page size", () => {
    expect(JOBS_PAGE_SIZE).toBe(50);
  });
});

// STUDIO-828 — the Now strip's five numbers now come from a figure the daemon computes over every
// issue in the store, instead of from a fold over the rows this client happened to fetch.
describe("consoleStoreCounts", () => {
  function counts(buckets: IssueCountsResponse["buckets"]): IssueCountsResponse {
    return { issues: buckets.reduce((n, b) => n + b.count, 0), buckets };
  }

  // Before the first response there is no answer, and the strip renders "—". A zero would be a
  // claim that the store is empty, which is the sort of number this ticket exists to stop the
  // console inventing.
  it("has no answer until the daemon has given one", () => {
    expect(consoleStoreCounts(undefined)).toBeUndefined();
  });

  it("applies the row's own rule to each bucket, weighted by its count", () => {
    expect(
      consoleStoreCounts(
        counts([
          { outcome: "running", count: 2 },
          { outcome: "completed", lifecycle: "in_review", count: 7 },
          { outcome: "completed", lifecycle: "done", count: 300 },
          { outcome: "completed", lifecycle: "open", count: 1 },
          { outcome: "failed", lifecycle: "open", count: 3 },
        ]),
      ),
    ).toEqual({ running: 2, review: 7, queued: 1, blocked: 3, needsYou: 10 });
  });

  // A FINISHED review RUN is done rather than awaiting one — the "Needs you" inflation STUDIO-826
  // fixed on the rows, reaching the tally by the same field and the same rule. A live one reads
  // `reviewing`, which the strip counts as running, which is also why the daemon does not resolve
  // the sibling `review_ticket` marker for this payload.
  it("reads a review run's own outcome exactly as a row does", () => {
    expect(
      consoleStoreCounts(
        counts([
          { outcome: "running", count: 1 },
          { outcome: "completed", review_run: true, count: 4 },
          // One ordinary answered ticket, so the needs-you gate below is open and its zero is a
          // real zero rather than the "—" a wholly unanswered payload gets.
          { outcome: "completed", lifecycle: "done", count: 1 },
        ]),
      ),
    ).toEqual({ running: 1, review: 0, queued: 0, blocked: 0, needsYou: 0 });
  });

  // The outage gate, moved from the page to the payload and unchanged in meaning: when the daemon
  // resolved NO lifecycle at all, every `completed` is inferred into "in review" and a count over
  // that would be a number the console invented. Some bucket answering is enough — a store where
  // most tickets are unknown is a healthy tracker that does not know every ticket.
  it("refuses a needs-you number when the tracker answered nothing, and gives one when it did", () => {
    expect(
      consoleStoreCounts(counts([{ outcome: "completed", count: 2 }]))?.needsYou,
    ).toBeNull();
    expect(
      consoleStoreCounts(
        counts([
          { outcome: "completed", count: 2 },
          { outcome: "completed", lifecycle: "done", count: 1 },
        ]),
      )?.needsYou,
    ).toBe(2);
    // An empty store is knowable: nothing is waiting because there is nothing.
    expect(consoleStoreCounts(counts([]))).toEqual({
      running: 0,
      review: 0,
      queued: 0,
      blocked: 0,
      needsYou: 0,
    });
  });

  // The acceptance criterion the strip and the table share: a count is derived by the SAME rule the
  // row's pill is. Pinned by folding one store both ways — through the row pipeline the table uses,
  // and through the buckets the daemon serves for the same tickets — and asserting they agree.
  //
  // This is what makes the daemon's choice to group INPUTS rather than statuses checkable. Were the
  // daemon to send its own five numbers instead, nothing on this side could tell whether they had
  // been derived by the same rule; here the two answers are computed from the same facts by the
  // same function and the fixture would have to be wrong for them to agree by accident.
  it("agrees with the table's own tally over the same store", () => {
    const rows: IssueRun[] = [
      issueRow({ issue_identifier: "A", outcome: "running" }),
      issueRow({ issue_identifier: "B", outcome: "completed", lifecycle: "in_review" }),
      issueRow({ issue_identifier: "C", outcome: "completed", lifecycle: "done" }),
      issueRow({ issue_identifier: "D", outcome: "completed", lifecycle: "open" }),
      issueRow({ issue_identifier: "E", outcome: "failed", lifecycle: "open" }),
      issueRow({ issue_identifier: "F", outcome: "completed" }),
      issueRow({ issue_identifier: "G", outcome: "completed", review_run: true }),
      issueRow({ issue_identifier: "H", outcome: "running", review_ticket: true }),
    ];
    const table = consoleJobCounts(
      buildConsoleJobs(
        rows.map((r) => job({ issue: r.issue_identifier, status: r.outcome as JobRow["status"] })),
        rows,
        undefined,
        NOW,
      ),
    );
    // What `handle_issue_counts` serves for exactly those tickets: one bucket per distinct
    // combination of the same per-row facts. H's `review_ticket` is absent here, and the two
    // answers still agree — which is the evidence for skipping that lookup: the table paints H
    // "reviewing" and the strip counts it running, and the marker changes only the word.
    const strip = consoleStoreCounts(
      counts([
        { outcome: "running", count: 2 },
        { outcome: "completed", lifecycle: "in_review", count: 1 },
        { outcome: "completed", lifecycle: "done", count: 1 },
        { outcome: "completed", lifecycle: "open", count: 1 },
        { outcome: "failed", lifecycle: "open", count: 1 },
        { outcome: "completed", count: 1 },
        { outcome: "completed", review_run: true, count: 1 },
      ]),
    );
    expect(strip).toEqual(table);
    expect(strip).toEqual({ running: 2, review: 2, queued: 1, blocked: 1, needsYou: 3 });
  });

  // The held dependents the daemon's tally cannot see (`state.blocked`) are added by the client, so
  // the strip does not silently drop a row the table draws. Inert on a Rhapsody daemon — the Rust
  // snapshot carries no such set — which is exactly why it is pinned rather than assumed.
  it("adds the live snapshot's held dependents, which the daemon's tally cannot carry", () => {
    const payload = counts([{ outcome: "completed", lifecycle: "done", count: 1 }]);
    expect(consoleStoreCounts(payload)?.blocked).toBe(0);
    const held = { issue_identifier: "HELD", title: "", project: "", blocker_identifier: "X", blocker_state: "In Review" };
    const withHeld = consoleStoreCounts(payload, [held, { ...held, issue_identifier: "HELD2" }]);
    expect(withHeld?.blocked).toBe(2);
    // A held ticket waits on its PREDECESSOR, not on the operator — the same rule `needsOperator`
    // applies to the row, reached through the same call.
    expect(withHeld?.needsYou).toBe(0);
  });
});
