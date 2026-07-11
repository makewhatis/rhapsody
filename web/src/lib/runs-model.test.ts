import { describe, expect, it } from "vitest";
import type { LinearProject, RunSummary, StateResponse } from "@/lib/api";
import {
  deriveStatTiles,
  failureSubLabel,
  isMcpTool,
  JOB_FILTERS,
  jobStatus,
  matchFilter,
  mergeJobs,
  outcomeToStatus,
  projectColorMap,
  resolveAgent,
  resolveProject,
  searchJobs,
  transcriptEntryType,
} from "@/lib/runs-model";

// A fixed "now": 2026-06-07T12:00:00 local. Tests build timestamps relative to this so the
// today/not-today split is deterministic regardless of the machine's clock.
const NOW = new Date(2026, 5, 7, 12, 0, 0).getTime();
const todayAt = (h: number, m = 0) => new Date(2026, 5, 7, h, m, 0).toISOString();
const yesterdayAt = (h: number) => new Date(2026, 5, 6, h, 0, 0).toISOString();

function state(over: Partial<StateResponse> = {}): StateResponse {
  return {
    status: "ok",
    poll_interval_ms: 2000,
    running: [],
    retrying: [],
    codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
    rate_limits: [],
    blocked: [],
    ...over,
  };
}

function runningSession(over: Partial<StateResponse["running"][number]> = {}) {
  return {
    issue_id: "id",
    issue_identifier: "INF-1",
    title: "t",
    state: "In Progress",
    project: "",
    repo: "",
    run_id: 1,
    turn_count: 1,
    last_codex_event: "",
    started_at: todayAt(11),
    last_event_at: todayAt(11, 30),
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    ...over,
  };
}

function summary(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: 1,
    issue_id: "id",
    issue_identifier: "INF-1",
    title: "t",
    attempt: 0,
    session_uuid: "",
    branch: "",
    project_slug: "",
    repo: "",
    started_at: todayAt(10),
    ended_at: todayAt(10, 30),
    outcome: "completed",
    turns: 5,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

describe("outcomeToStatus", () => {
  it("maps each taxonomy-v2 segment outcome 1:1 onto a StatusChip key", () => {
    expect(outcomeToStatus("running")).toBe("running");
    expect(outcomeToStatus("continued")).toBe("continued"); // detail-only chip; NOT "running"
    expect(outcomeToStatus("completed")).toBe("completed");
    expect(outcomeToStatus("stopped")).toBe("stopped");
    expect(outcomeToStatus("failed")).toBe("failed");
    expect(outcomeToStatus("interrupted")).toBe("interrupted"); // detail-only chip
  });

  it("maps empty/unknown onto the neutral idle key", () => {
    expect(outcomeToStatus("")).toBe("idle");
    expect(outcomeToStatus("something-else")).toBe("idle");
  });
});

describe("deriveStatTiles", () => {
  it("always returns the four tiles in order: running, completed, tokens, runtime", () => {
    const tiles = deriveStatTiles(undefined, [], NOW);
    expect(tiles.map((t) => t.key)).toEqual(["running", "completed", "tokens", "runtime"]);
    // With no data the counters are zeroed, not crashing on undefined state.
    expect(tiles[0].value).toBe("0");
    expect(tiles[1].value).toBe("0");
    expect(tiles[2].value).toBe("0");
    expect(tiles[3].value).toBe("0s");
  });

  it("counts running sessions and active agents (distinct projects)", () => {
    const s = state({
      running: [
        runningSession({ project: "alpha" }),
        runningSession({ project: "beta" }),
        runningSession({ project: "alpha" }),
      ],
    });
    const running = deriveStatTiles(s, [], NOW)[0];
    expect(running.value).toBe("3");
    expect(running.sub).toBe("2 agents active");
    expect(running.accent).toBe("var(--em-bright)");
    expect(running.pulse).toBe(true);
  });

  it("singularizes the active-agents hint for a single agent", () => {
    const s = state({ running: [runningSession({ project: "alpha" })] });
    expect(deriveStatTiles(s, [], NOW)[0].sub).toBe("1 agent active");
  });

  it("counts store-running rows that are absent from the live snapshot in the Running tile", () => {
    const s = state({ running: [runningSession({ run_id: 1, project: "alpha" })] });
    const history = [
      // still running in the store but dropped from the snapshot — must still count
      summary({ id: 2, outcome: "running", project_slug: "beta" }),
      // the live run's own history twin (id 1) must NOT be double-counted
      summary({ id: 1, outcome: "running", project_slug: "alpha" }),
      summary({ id: 3, outcome: "completed" }),
    ];
    const running = deriveStatTiles(s, history, NOW)[0];
    expect(running.value).toBe("2"); // 1 live + 1 store-running (id 2); id 1 deduped
    expect(running.sub).toBe("2 agents active"); // alpha + beta
  });

  it("counts completed runs in the Completed tile", () => {
    const history = [
      summary({ outcome: "completed" }),
      summary({ outcome: "completed" }),
      summary({ outcome: "stopped" }),
      summary({ outcome: "failed" }),
    ];
    const completed = deriveStatTiles(state(), history, NOW)[1];
    expect(completed.label).toBe("Completed");
    expect(completed.value).toBe("2");
    expect(completed.sub).toBe("agent hand-off verified");
    expect(completed.accent).toBeUndefined();
  });

  it("sums today's tokens from today's running + finished runs, ignoring all-time codex_totals", () => {
    const s = state({
      // codex_totals is an all-time cumulative figure (persisted across daemon restarts); the
      // today tiles must NOT use it, or they would double-count + conflate prior days.
      codex_totals: { input_tokens: 999_999, output_tokens: 999_999, total_tokens: 999_999, seconds_running: 999_999 },
      running: [
        runningSession({ run_id: 5, started_at: todayAt(11), input_tokens: 400_000, output_tokens: 600_000, total_tokens: 1_000_000 }),
      ],
    });
    const history = [
      summary({ id: 7, outcome: "completed", started_at: todayAt(9), input_tokens: 1000, output_tokens: 2000, total_tokens: 3000 }),
      // the live run also has a history row (id 5) — counted once (the live row wins)
      summary({ id: 5, outcome: "running", started_at: todayAt(11), input_tokens: 123, output_tokens: 123, total_tokens: 123 }),
      // yesterday's run is excluded from "today"
      summary({ id: 8, outcome: "completed", started_at: yesterdayAt(9), input_tokens: 5000, output_tokens: 5000, total_tokens: 5000 }),
    ];
    const tokens = deriveStatTiles(s, history, NOW)[2];
    expect(tokens.value).toBe("1.0M"); // 1_000_000 (live) + 3_000 = 1_003_000 -> 1.0M
    // Three-part reconciling sub: in · out · cached. Here cache = total − in − out =
    // 1_003_000 − 401_000 − 602_000 = 0, so this also covers the zero-cache form "0 cached". (INF-282)
    expect(tokens.sub).toBe("401.0k in · 602.0k out · 0 cached");
  });

  it("subtext reconciles to the headline when cache tokens dominate (in · out · cached sum to total)", () => {
    // Repro of the bug: a huge cache-inclusive headline over tiny in/out. The sub must surface the
    // cached portion so in + out + cached == the headline total. (INF-282)
    const s = state({
      running: [
        runningSession({
          run_id: 5,
          started_at: todayAt(11),
          input_tokens: 44_000,
          output_tokens: 205_500,
          // total is cache-inclusive: cache = 38_200_000 − 44_000 − 205_500 = 37_950_500
          total_tokens: 38_200_000,
        }),
      ],
    });
    const tokens = deriveStatTiles(s, [], NOW)[2];
    expect(tokens.value).toBe("38.2M");
    expect(tokens.sub).toBe("44.0k in · 205.5k out · 38.0M cached"); // 37_950_500 -> 38.0M
    // The raw figures reconcile exactly to the headline total.
    expect(44_000 + 205_500 + 37_950_500).toBe(38_200_000);
  });

  it("sums today's runtime from running elapsed + finished durations and counts today's runs", () => {
    const s = state({
      // ignored: all-time cumulative
      codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 999_999 },
      running: [runningSession({ run_id: 5, started_at: new Date(NOW - 600_000).toISOString() })], // elapsed 600s
    });
    const history = [
      // 30 min finished run today -> 1800s
      summary({ id: 7, outcome: "completed", started_at: todayAt(10), ended_at: todayAt(10, 30) }),
      // the live run's history twin (id 5) — de-duplicated
      summary({ id: 5, outcome: "running", started_at: new Date(NOW - 600_000).toISOString() }),
      // yesterday -> excluded
      summary({ id: 8, outcome: "completed", started_at: yesterdayAt(8), ended_at: yesterdayAt(9) }),
    ];
    const runtime = deriveStatTiles(s, history, NOW)[3];
    expect(runtime.value).toBe("40m 0s"); // 600 (live elapsed) + 1800 (finished) = 2400s
    expect(runtime.sub).toBe("across 2 runs"); // live run 5 + finished run 7 (dup 5 + yesterday 8 excluded)
  });
});

const PROJECTS: LinearProject[] = [
  { id: "p1", name: "Infrastructure", slug: "symphony-infra-tasks-9c29e9ade060", team: "INF", color: "#34d399" },
  { id: "p2", name: "Core Platform", slug: "symphony-core-5f1a2b3c4d5e", team: "CORE", color: "#38bdf8" },
];

describe("projectColorMap", () => {
  it("indexes name + colour by slug", () => {
    const m = projectColorMap(PROJECTS);
    expect(m.get("symphony-infra-tasks-9c29e9ade060")).toEqual({ name: "Infrastructure", color: "#34d399" });
    expect(m.get("missing")).toBeUndefined();
  });
});

describe("resolveAgent", () => {
  it("resolves a project's name + colour, else falls back to projShort + the emerald token", () => {
    expect(resolveAgent("symphony-infra-tasks-9c29e9ade060", "", PROJECTS)).toEqual({
      name: "Infrastructure",
      color: "#34d399",
    });
    expect(resolveAgent("unknown-aabbccdd11", "", PROJECTS)).toEqual({
      name: "unknown",
      color: "var(--em-bright)",
    });
    // single-project mode (no slug): fall back to the repo short name
    expect(resolveAgent("", "git@github.com:example/demo-repo.git", PROJECTS).name).toBe(
      "example/demo-repo",
    );
  });
});

describe("resolveProject", () => {
  it("resolves the Linear project name, else projShort, else the raw slug", () => {
    // in the fetched list → its display name (never the raw slug id)
    expect(resolveProject("symphony-infra-tasks-9c29e9ade060", PROJECTS)).toBe("Infrastructure");
    // not in the list but carries a trailing hex id → projShort strips it
    expect(resolveProject("unknown-aabbccdd11", PROJECTS)).toBe("unknown");
    // id-only slug not in the list: projShort can't strip it, so the raw slug passes through (the
    // bug surface — these unreadable ids must only appear when the project truly can't be resolved)
    expect(resolveProject("872639248532", PROJECTS)).toBe("872639248532");
    // no slug (single-project / unattributed) → an em dash
    expect(resolveProject("", PROJECTS)).toBe("—");
  });
});

describe("mergeJobs", () => {
  it("merges live running sessions with history, dedups by run id (live wins)", () => {
    const s = state({
      running: [
        runningSession({ run_id: 10, issue_identifier: "INF-1", project: "symphony-infra-tasks-9c29e9ade060", turn_count: 14, total_tokens: 84_200 }),
      ],
    });
    const history = [
      // same run as the live one (id 10) — should be dropped in favour of the live row
      summary({ id: 10, issue_identifier: "INF-1", outcome: "running" }),
      summary({ id: 9, issue_identifier: "CORE-2", outcome: "completed", project_slug: "symphony-core-5f1a2b3c4d5e" }),
    ];
    const rows = mergeJobs(s, history, PROJECTS, NOW);
    expect(rows).toHaveLength(2);
    const inf = rows.find((r) => r.issue === "INF-1")!;
    expect(inf.live).toBe(true);
    expect(inf.runId).toBe(10);
    expect(inf.turn).toBe(14);
    expect(inf.tokens).toBe("84.2k");
    expect(inf.agent).toBe("Infrastructure"); // resolved from the Linear project list
    expect(inf.agentColor).toBe("#34d399");
    expect(inf.projectShort).toBe("Infrastructure"); // resolved from the Linear project list (never the raw slug)
    expect(inf.durationAccent).toBe(true);
  });

  it("sorts running rows first, then most-recent started_at", () => {
    const s = state({
      running: [runningSession({ run_id: 1, issue_identifier: "RUN-1", started_at: todayAt(9) })],
    });
    const history = [
      summary({ id: 2, issue_identifier: "OLD", outcome: "completed", started_at: todayAt(8) }),
      summary({ id: 3, issue_identifier: "NEW", outcome: "completed", started_at: todayAt(11) }),
    ];
    const rows = mergeJobs(s, history, PROJECTS, NOW);
    expect(rows.map((r) => r.issue)).toEqual(["RUN-1", "NEW", "OLD"]);
  });

  it("keeps an unpersisted live session (run_id 0) as a non-clickable row keyed by issue", () => {
    const s = state({ running: [runningSession({ run_id: 0, issue_identifier: "NOID" })] });
    const rows = mergeJobs(s, [], PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].runId).toBe(0);
    expect(rows[0].key).toBe("live-NOID");
    expect(rows[0].live).toBe(true);
  });

  it("uses runDuration for finished rows and projShort fallback when project isn't in the list", () => {
    const history = [
      summary({ id: 5, issue_identifier: "X-1", outcome: "completed", project_slug: "unknown-aabbccdd11", started_at: todayAt(10), ended_at: todayAt(10, 30) }),
    ];
    const row = mergeJobs(state(), history, PROJECTS, NOW)[0];
    expect(row.live).toBe(false);
    expect(row.duration).toBe("30m 0s");
    expect(row.durationAccent).toBe(false);
    expect(row.agent).toBe("unknown"); // projShort of a slug not in the Linear list
    expect(row.agentColor).toBe("var(--em-bright)"); // fallback colour token
    expect(row.status).toBe("completed");
  });

  it("groups multiple segments of one issue into a SINGLE job row by the newest segment", () => {
    const history = [
      summary({ id: 30, issue_identifier: "JOB-1", outcome: "completed", started_at: todayAt(11) }),
      summary({ id: 29, issue_identifier: "JOB-1", outcome: "continued", started_at: todayAt(10) }),
      summary({ id: 28, issue_identifier: "JOB-1", outcome: "continued", started_at: todayAt(9) }),
    ];
    const rows = mergeJobs(state(), history, PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].issue).toBe("JOB-1");
    expect(rows[0].status).toBe("completed"); // newest segment decides; continued never pins running
    expect(rows[0].runId).toBe(30); // newest segment's detail
  });

  it("a job whose claim is gone never reads running, no matter how many continued segments", () => {
    const history = [
      summary({ id: 41, issue_identifier: "JOB-2", outcome: "continued", started_at: todayAt(11) }),
      summary({ id: 40, issue_identifier: "JOB-2", outcome: "continued", started_at: todayAt(10) }),
    ];
    const rows = mergeJobs(state(), history, PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].status).toBe("stopped"); // newest is `continued` with no live/queued → stopped
  });

  it("synthesizes a queued row from state.retrying so a between-segments job reads running", () => {
    const s = state({
      retrying: [{ issue_identifier: "JOB-3", attempt: 2, due_at: todayAt(11, 30), error: "" }],
    });
    const history = [
      summary({ id: 50, issue_identifier: "JOB-3", outcome: "continued", started_at: todayAt(11) }),
    ];
    const rows = mergeJobs(s, history, PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].status).toBe("running"); // a pending retry/continuation keeps the job running
  });

  it("shows the failure reason as a sub-label on a failed job", () => {
    const history = [
      summary({ id: 60, issue_identifier: "FAIL-1", outcome: "failed", error: "turn_timeout: turn exceeded 30m0s" }),
    ];
    const rows = mergeJobs(state(), history, PROJECTS, NOW);
    expect(rows[0].status).toBe("failed");
    expect(rows[0].subLabel).toBe("turn timeout");
  });

  it("turns a held dependent (state.blocked) into a non-clickable waiting row (INF-320)", () => {
    const s = state({
      blocked: [
        {
          issue_identifier: "DEP-1",
          title: "dependent",
          project: "symphony-infra-tasks-9c29e9ade060",
          blocker_identifier: "INF-1",
          blocker_state: "In Review",
          mode: "graphite",
        },
      ],
    });
    const rows = mergeJobs(s, [], PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    const w = rows[0];
    expect(w.status).toBe("waiting");
    expect(w.runId).toBe(0); // never ran → not clickable
    expect(w.issue).toBe("DEP-1");
    expect(w.title).toBe("dependent");
    expect(w.agent).toBe("Infrastructure"); // resolved from the Linear project list (via entry.project)
    expect(w.projectShort).toBe("Infrastructure");
    expect(w.subLabel).toBe("waiting on INF-1 · In Review");
  });

  it("collapses a blocked issue that is ALSO live to running (live wins; no longer waiting)", () => {
    const s = state({
      running: [runningSession({ run_id: 5, issue_identifier: "DEP-1", project: "symphony-infra-tasks-9c29e9ade060" })],
      blocked: [
        { issue_identifier: "DEP-1", title: "dependent", project: "symphony-infra-tasks-9c29e9ade060", blocker_identifier: "INF-1", blocker_state: "In Review" },
      ],
    });
    const rows = mergeJobs(s, [], PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].status).toBe("running"); // live precedence
    expect(rows[0].runId).toBe(5);
  });

  it("keeps the real run status + clickability when a blocked issue ALSO has finished history (INF-320)", () => {
    // Defensive: the daemon never holds a ticket that has already run, but if a blocked[] entry and a
    // finished history row collide on one issue, the real (openable) run must win over the waiting hold.
    const s = state({
      blocked: [{ issue_identifier: "DEP-1", title: "dependent", project: "", blocker_identifier: "INF-1", blocker_state: "In Review" }],
    });
    const history = [summary({ id: 77, issue_identifier: "DEP-1", outcome: "completed" })];
    const rows = mergeJobs(s, history, PROJECTS, NOW);
    expect(rows).toHaveLength(1);
    expect(rows[0].status).toBe("completed"); // not "waiting"
    expect(rows[0].runId).toBe(77); // clickable — opens the real run
    expect(rows[0].subLabel).toBeUndefined(); // no "waiting on …" label
  });

  it("renders NO waiting rows for an empty blocked[] (the disabled-project case)", () => {
    const rows = mergeJobs(
      state({ blocked: [] }),
      [summary({ id: 2, issue_identifier: "DONE", outcome: "completed" })],
      PROJECTS,
      NOW,
    );
    expect(rows.some((r) => r.status === "waiting")).toBe(false);
    expect(rows).toHaveLength(1);
  });
});

describe("jobStatus", () => {
  const seg = (outcome: string, opts: { live?: boolean; queued?: boolean } = {}) => ({
    outcome,
    live: opts.live ?? false,
    queued: opts.queued ?? false,
  });
  it("derives running ONLY from live/queued rows, else the newest segment decides", () => {
    expect(jobStatus([seg("completed"), seg("continued"), seg("continued")])).toBe("completed");
    expect(jobStatus([seg("failed"), seg("continued")])).toBe("failed");
    expect(jobStatus([seg("continued", { queued: true })])).toBe("running"); // queued retry
    expect(jobStatus([seg("running", { live: true })])).toBe("running");
    expect(jobStatus([seg("interrupted")])).toBe("stopped"); // claim gone
    expect(jobStatus([seg("stopped"), seg("completed")])).toBe("stopped");
    // a historical continued segment must NOT pin a finished job to running
    expect(jobStatus([seg("continued")])).toBe("stopped");
  });

  it("returns 'waiting' for a held group, but live/queued/history still take precedence (INF-320)", () => {
    expect(jobStatus([{ outcome: "waiting", live: false, queued: false, waiting: true }])).toBe("waiting");
    // A waiting ticket that is ALSO live/queued is, by definition, no longer waiting.
    expect(jobStatus([{ outcome: "waiting", live: true, queued: false, waiting: true }])).toBe("running");
    expect(jobStatus([{ outcome: "waiting", live: false, queued: true, waiting: true }])).toBe("running");
    // A held entry that ALSO has a real finished segment keeps the real status (the run stays openable).
    expect(
      jobStatus([
        { outcome: "completed", live: false, queued: false, waiting: false },
        { outcome: "waiting", live: false, queued: false, waiting: true },
      ]),
    ).toBe("completed");
  });
});

describe("failureSubLabel", () => {
  it("maps reason prefixes onto compact sub-labels", () => {
    expect(failureSubLabel("turn_timeout: turn exceeded 30m0s")).toBe("turn timeout");
    expect(failureSubLabel("stalled")).toBe("stalled");
    expect(failureSubLabel("git clone failed")).toBe("git clone failed"); // short reason passes through
    expect(failureSubLabel("x".repeat(50))).toBe("x".repeat(40) + "…"); // long reason trims to ~40
    expect(failureSubLabel("")).toBe("");
  });
});

describe("matchFilter", () => {
  const rows = mergeJobs(
    state({ running: [runningSession({ run_id: 1, issue_identifier: "RUN-1" })] }),
    [
      summary({ id: 2, issue_identifier: "STOP", outcome: "stopped" }),
      summary({ id: 3, issue_identifier: "DONE", outcome: "completed" }),
      summary({ id: 4, issue_identifier: "FAIL", outcome: "failed" }),
    ],
    PROJECTS,
    NOW,
  );
  const ids = (f: Parameters<typeof matchFilter>[1]) => rows.filter((r) => matchFilter(r, f)).map((r) => r.issue).sort();

  it("filters by each of the four job states", () => {
    expect(ids("all")).toEqual(["DONE", "FAIL", "RUN-1", "STOP"]);
    expect(ids("running")).toEqual(["RUN-1"]);
    expect(ids("completed")).toEqual(["DONE"]);
    expect(ids("stopped")).toEqual(["STOP"]);
    expect(ids("failed")).toEqual(["FAIL"]);
  });

  it("exposes a Waiting filter that selects only waiting rows (INF-320)", () => {
    expect(JOB_FILTERS.some((f) => f.id === "waiting" && f.label === "Waiting")).toBe(true);
    const waitingRows = mergeJobs(
      state({ blocked: [{ issue_identifier: "DEP-1", title: "d", project: "", blocker_identifier: "INF-1", blocker_state: "In Review" }] }),
      [summary({ id: 2, issue_identifier: "DONE", outcome: "completed" })],
      PROJECTS,
      NOW,
    );
    expect(waitingRows.filter((r) => matchFilter(r, "waiting")).map((r) => r.issue)).toEqual(["DEP-1"]);
    // the completed row is NOT a waiting row
    expect(waitingRows.filter((r) => matchFilter(r, "completed")).map((r) => r.issue)).toEqual(["DONE"]);
  });
});

describe("searchJobs", () => {
  const rows = mergeJobs(
    state(),
    [
      summary({ id: 1, issue_identifier: "INF-231", title: "Sign & notarize the dmg", project_slug: "symphony-infra-tasks-9c29e9ade060" }),
      summary({ id: 2, issue_identifier: "CORE-118", title: "rate limit headers", project_slug: "symphony-core-5f1a2b3c4d5e" }),
    ],
    PROJECTS,
    NOW,
  );

  it("matches issue, title, or agent case-insensitively", () => {
    expect(searchJobs(rows, "inf-231").map((r) => r.issue)).toEqual(["INF-231"]);
    expect(searchJobs(rows, "RATE").map((r) => r.issue)).toEqual(["CORE-118"]);
    expect(searchJobs(rows, "core platform").map((r) => r.issue)).toEqual(["CORE-118"]); // agent name
    expect(searchJobs(rows, "")).toHaveLength(2);
  });

  it("matches the predecessor identifier via the waiting sub-label (INF-320)", () => {
    const waitingRows = mergeJobs(
      state({ blocked: [{ issue_identifier: "DEP-9", title: "dep", project: "", blocker_identifier: "BLK-7", blocker_state: "In Review" }] }),
      [],
      PROJECTS,
      NOW,
    );
    // The blocker BLK-7 lives only in the sub-label — search must reach it.
    expect(searchJobs(waitingRows, "blk-7").map((r) => r.issue)).toEqual(["DEP-9"]);
  });
});

describe("transcriptEntryType", () => {
  it("maps LogEntry kinds onto transcript visual types", () => {
    expect(transcriptEntryType("event")).toBe("divider");
    expect(transcriptEntryType("tool_use")).toBe("tool");
    expect(transcriptEntryType("tool_result")).toBe("out");
    expect(transcriptEntryType("text")).toBe("text");
    expect(transcriptEntryType("thinking")).toBe("text");
  });
});

describe("isMcpTool", () => {
  it("detects mcp__ tool names", () => {
    expect(isMcpTool("mcp__claude_ai_Linear__get_issue")).toBe(true);
    expect(isMcpTool("Bash")).toBe(false);
    expect(isMcpTool("")).toBe(false);
  });
});
