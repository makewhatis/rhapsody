import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchRunMessages, fetchState, resumeRun, sendRunMessage, stopRun } from "@/lib/api";

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
