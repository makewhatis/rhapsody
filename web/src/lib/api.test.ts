import { afterEach, describe, expect, it, vi } from "vitest";
import {
  fetchDaySummary,
  fetchIssueRuns,
  fetchRunIdentityEvents,
  fetchRunMessages,
  fetchState,
  fetchVersion,
  localDayStartISO,
  resumeRun,
  sendRunMessage,
  stopRun,
} from "@/lib/api";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("run-action wrappers", () => {
  it("stopRun POSTs to /runs/{id}/stop and returns the action result", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ identifier: "INF-9", moved_to: "Backlog" }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const r = await stopRun(7);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/runs/7/stop",
      expect.objectContaining({ method: "POST" }),
    );
    expect(r.moved_to).toBe("Backlog");
  });

  it("resumeRun POSTs to /runs/{id}/resume", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ identifier: "INF-9", moved_to: "Todo" }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const r = await resumeRun(7);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/runs/7/resume",
      expect.objectContaining({ method: "POST" }),
    );
    expect(r.moved_to).toBe("Todo");
  });

  it("stopRun RESOLVES (partial success) when a 200 body carries move_error", async () => {
    // A killed-but-couldn't-move stop is a partial success: the daemon returns 200 with
    // move_error in the body. The wrapper must resolve (not throw) so the UI can surface it.
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ identifier: "INF-9", move_error: "no backlog state for team" }), {
          status: 200,
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const r = await stopRun(7);
    expect(r.move_error).toBe("no backlog state for team");
    expect(r.identifier).toBe("INF-9");
  });

  it("surfaces the daemon error envelope message on failure", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ error: { code: "not_running", message: "run is not currently running" } }), {
          status: 409,
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(stopRun(7)).rejects.toThrow("run is not currently running");
  });
});

describe("operator messages (INF-250)", () => {
  it("sendRunMessage POSTs {text} to /runs/{id}/message and returns the row", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ id: 11, identifier: "INF-250", status: "sent" }), { status: 202 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const r = await sendRunMessage(7, "watch the branch");
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/runs/7/message",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ text: "watch the branch" }) }),
    );
    expect(r.id).toBe(11);
    expect(r.status).toBe("sent");
  });

  it("sendRunMessage surfaces the daemon error envelope (409 backlog_full)", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({ error: { code: "backlog_full", message: "too many pending operator messages for this run" } }),
          { status: 409 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(sendRunMessage(7, "hi")).rejects.toThrow("too many pending operator messages");
  });

  it("fetchRunMessages GETs /runs/{id}/messages and tolerates a null body", async () => {
    const fetchMock = vi.fn(async () => new Response("null", { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const msgs = await fetchRunMessages(7);
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/runs/7/messages", expect.anything());
    expect(msgs).toEqual([]);
  });
});

describe("fetchState defensive coalescing (INF-320)", () => {
  it("coalesces a missing blocked[] to [] so the Runs model can map over it unconditionally", async () => {
    // A daemon that predates the held-dependents snapshot field (INF-318 ships only the log line) omits
    // `blocked` — fetchState must default it to [] alongside running/retrying/rate_limits so the waiting
    // indicator path never white-screens on undefined.
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            status: "ok",
            poll_interval_ms: 2000,
            running: [],
            retrying: [],
            codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
            // rate_limits + blocked omitted on purpose
          }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const s = await fetchState();
    expect(s.blocked).toEqual([]);
    expect(s.rate_limits).toEqual([]);
  });

  it("passes through a populated blocked[] verbatim", async () => {
    const blocked = [
      {
        issue_identifier: "INF-2",
        title: "successor",
        project: "symphony-app",
        blocker_identifier: "INF-1",
        blocker_state: "In Review",
        mode: "graphite",
      },
    ];
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            status: "ok",
            poll_interval_ms: 2000,
            running: [],
            retrying: [],
            codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
            rate_limits: [],
            blocked,
          }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const s = await fetchState();
    expect(s.blocked).toEqual(blocked);
  });
});

// TRA-320 — the issue-level listing and the daemon-computed day summary. Both exist so the
// dashboard stops deriving an issue-grouped list and a set of totals from one run-paged fetch.
describe("issue listing + day summary (TRA-320)", () => {
  it("fetchIssueRuns GETs /history/issues and passes the shared history filters through", async () => {
    const fetchMock = vi.fn(
      async () => new Response(JSON.stringify({ issues: [], next_offset: 50 }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const r = await fetchIssueRuns({ project: "core", outcome: "failed", limit: 50 });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/history/issues?outcome=failed&project=core&limit=50",
      expect.anything(),
    );
    expect(r.next_offset).toBe(50);
  });

  it("fetchIssueRuns tolerates a null/omitted issues array so the table can map safely", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({}), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    const r = await fetchIssueRuns({});
    expect(r.issues).toEqual([]);
    expect(r.next_offset).toBeNull();
  });

  it("localDayStartISO renders the LOCAL midnight as a whole-second RFC3339 UTC instant", () => {
    // Local, not UTC: the header cells have always counted a local day, and moving the sum into
    // the daemon must not shift that boundary. The local calendar fields confirm the boundary.
    const now = new Date(2026, 7, 2, 14, 33, 12).getTime();
    const iso = localDayStartISO(now);
    expect(iso).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/); // seconds precision, no millis
    const parsed = new Date(iso);
    expect(parsed.getFullYear()).toBe(2026);
    expect(parsed.getMonth()).toBe(7);
    expect(parsed.getDate()).toBe(2);
    expect(parsed.getHours()).toBe(0);
    expect(parsed.getMinutes()).toBe(0);
    expect(parsed.getSeconds()).toBe(0);
  });

  it("fetchDaySummary sends its own local midnight as `since`", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            since: "2026-08-02T07:00:00Z",
            runs: 105,
            completed: 61,
            input_tokens: 1_200_000,
            output_tokens: 800_000,
            total_tokens: 53_900_000,
            seconds: 13_422,
            rhythm: [1, 2, 3],
          }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const now = new Date(2026, 7, 2, 14, 33, 12).getTime();
    const s = await fetchDaySummary(now);
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/v1/history/summary?since=${encodeURIComponent(localDayStartISO(now))}`,
      expect.anything(),
    );
    expect(s.total_tokens).toBe(53_900_000);
    expect(s.runs).toBe(105);
  });

  it("fetchDaySummary tolerates an omitted rhythm array", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            since: "2026-08-02T07:00:00Z",
            runs: 0,
            completed: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            seconds: 0,
          }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    expect((await fetchDaySummary(Date.now())).rhythm).toEqual([]);
  });
});

describe("fetchVersion", () => {
  it("GETs /api/v1/version and returns the daemon's build identity", async () => {
    const fetchMock = vi.fn(
      async () =>
        new Response(
          JSON.stringify({
            version: "v0.3.1-8-g581e281",
            commit: "581e28193d420970a04d545e65087ebf9bbc45e4",
            built_at: "2026-08-13T16:10:35Z",
          }),
          { status: 200 },
        ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const v = await fetchVersion();
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/version", expect.anything());
    expect(v.commit).toBe("581e28193d420970a04d545e65087ebf9bbc45e4");
    expect(v.version).toBe("v0.3.1-8-g581e281");
    expect(v.built_at).toBe("2026-08-13T16:10:35Z");
  });
});

// STUDIO-746 — the run detail's durable per-run attribution. The routing rows are the daemon's own
// record of who a run was dispatched as; one bounded search covers every attempt of a ticket
// because a dispatch writes exactly one of them.
describe("run identity events (STUDIO-746)", () => {
  it("asks the event search for BOTH routing kinds, scoped to the ticket", async () => {
    const fetchMock = vi.fn(async (url: string) =>
      new Response(
        JSON.stringify({
          hits: [
            {
              run_id: url.includes("unrouted") ? 522 : 547,
              issue_identifier: "STUDIO-746",
              seq: 1,
              at: "",
              kind: url.includes("unrouted") ? "teams.unrouted" : "teams.route",
              tool: "",
              text: url.includes("unrouted") ? "reason=solo" : "identity=alice reason=label",
            },
          ],
        }),
        { status: 200 },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const hits = await fetchRunIdentityEvents("STUDIO-746");
    const asked = fetchMock.mock.calls.map((c) => c[0]);
    expect(asked).toContain("/api/v1/events?issue=STUDIO-746&kind=teams.route&limit=100");
    expect(asked).toContain("/api/v1/events?issue=STUDIO-746&kind=teams.unrouted&limit=100");
    expect(hits.map((h) => h.run_id).sort()).toEqual([522, 547]);
  });

  it("percent-encodes a ticketless review run's own `pr:` key", async () => {
    const fetchMock = vi.fn(
      async (_url: string) => new Response(JSON.stringify({ hits: [] }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await fetchRunIdentityEvents("pr:makewhatis/rhapsody#12@jimmy");
    for (const [url] of fetchMock.mock.calls) {
      expect(url).toContain("issue=pr%3Amakewhatis%2Frhapsody%2312%40jimmy");
    }
  });

  it("tolerates a null/omitted hits array so the model can fold safely", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({}), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    expect(await fetchRunIdentityEvents("STUDIO-746")).toEqual([]);
  });
});
