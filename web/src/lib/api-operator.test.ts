import fs from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  OPERATOR_HEADER,
  mergeRun,
  postRefresh,
  postReviewClear,
  postReviewDismiss,
  postReviewRerun,
  postTeamsInvalidate,
  postTeamsReinstate,
  postTeamsRoom,
  resumeRun,
  saveConfig,
  saveLeadHarnesses,
  saveTeamsConfig,
  saveTypedConfig,
  sendRunMessage,
  setDrain,
  stopRun,
  type ReviewJob,
} from "@/lib/api";

// The daemon's operator-write guard (STUDIO-982) refuses a mutation without exactly one
// `X-Rhapsody-Operator: 1`, with a cookie, or with a form-shaped content type. These pin the exact
// request every dashboard write sends.

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it("lead Save sends an operator-guarded PUT and preserves 409 entry reasons", async () => {
  const active = { harnesses: [{ harness: "claude", model: "old", effort: "high", state: "passed", tested_at: null }], last_used: null };
  const tested = [{ ...active.harnesses[0], model: "new", state: "failed: unsafe tool" }];
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ error: { message: "old list is still active" }, active, tested }), { status: 409 }));
  vi.stubGlobal("fetch", fetch);
  const entries = [{ harness: "claude", model: "new", effort: "high" }];
  expect(await saveLeadHarnesses(entries)).toEqual({ ok: false, active, tested, reason: "old list is still active" });
  expect(fetch).toHaveBeenCalledWith("/api/v1/lead/harnesses", {
    method: "PUT", credentials: "omit", headers: { "Content-Type": "application/json", Accept: "application/json", [OPERATOR_HEADER]: "1" }, body: JSON.stringify({ harnesses: entries }),
  });
});

const job: ReviewJob = {
  owner: "o",
  repo: "r",
  number: 3,
  reviewer: "alice",
  author: "bob",
} as ReviewJob;

// Every dashboard mutation, with the path and JSON body it must send.
const mutations: Array<[string, () => Promise<unknown>, string, unknown]> = [
  ["setDrain", () => setDrain(true, "operator"), "/api/v1/drain", { active: true, reason: "operator" }],
  ["postRefresh", () => postRefresh(), "/api/v1/refresh", {}],
  ["stopRun", () => stopRun(7), "/api/v1/runs/7/stop", {}],
  ["resumeRun", () => resumeRun(7), "/api/v1/runs/7/resume", {}],
  ["mergeRun", () => mergeRun(7, "abc"), "/api/v1/runs/7/merge", { confirm: "abc" }],
  ["sendRunMessage", () => sendRunMessage(7, "hi"), "/api/v1/runs/7/message", { text: "hi" }],
  ["saveConfig", () => saveConfig({ config: {}, prompt_body: "" }), "/api/v1/config", { config: {}, prompt_body: "" }],
  ["saveTypedConfig", () => saveTypedConfig({} as never, []), "/api/v1/config", { global: {}, projects: [] }],
  [
    "postTeamsInvalidate",
    () => postTeamsInvalidate("alice", "f1", "wrong"),
    "/api/v1/teams/invalidate",
    { identity: "alice", fact_id: "f1", reason: "wrong" },
  ],
  [
    "postTeamsReinstate",
    () => postTeamsReinstate("alice", "f1"),
    "/api/v1/teams/reinstate",
    { identity: "alice", fact_id: "f1" },
  ],
  ["postTeamsRoom", () => postTeamsRoom("hello"), "/api/v1/teams/room", { body: "hello", refs: [] }],
  ["postReviewRerun", () => postReviewRerun(job), "/api/v1/reviews/rerun", { owner: "o", repo: "r", number: 3 }],
  ["postReviewDismiss", () => postReviewDismiss(job), "/api/v1/reviews/dismiss", { owner: "o", repo: "r", number: 3 }],
  ["postReviewClear", () => postReviewClear(job), "/api/v1/reviews/clear", { owner: "o", repo: "r", number: 3 }],
  ["saveTeamsConfig", () => saveTeamsConfig({} as never), "/api/v1/teams/config", { config: {} }],
];

describe("operator-write guard contract (STUDIO-982)", () => {
  it.each(mutations)("%s sends exactly one operator header, JSON, and no cookies", async (_name, call, url, body) => {
    const fetchMock = vi.fn(
      async (_url: string, _init?: RequestInit) =>
        new Response(JSON.stringify({ active: true, identifier: "x" }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await call().catch(() => undefined);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [gotURL, init] = fetchMock.mock.calls[0];
    expect(gotURL).toBe(url);
    expect(init).toEqual({
      method: "POST",
      credentials: "omit",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        [OPERATOR_HEADER]: "1",
      },
      body: JSON.stringify(body),
    });
    expect(OPERATOR_HEADER).toBe("X-Rhapsody-Operator");
  });

  it("surfaces the daemon's refusal — including the guard's own reason — instead of swallowing it", async () => {
    const refusal = { error: { code: "operator_write_forbidden", message: "refused by the guard" } };
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify(refusal), { status: 403 })),
    );
    await expect(postTeamsRoom("hello")).rejects.toThrow("refused by the guard");
    await expect(stopRun(7)).rejects.toThrow("refused by the guard");
    // STUDIO-1044: setDrain and postRefresh only knew the status before; a refused write must show
    // the guard's own sentence, not a bare number.
    await expect(setDrain(true, "operator")).rejects.toThrow("refused by the guard");
    await expect(postRefresh()).rejects.toThrow("refused by the guard");
  });

  it("falls back to the status when a refusal carries no JSON envelope", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("not json", { status: 403 })),
    );
    await expect(postRefresh()).rejects.toThrow("refresh failed: 403");
    await expect(setDrain(false)).rejects.toThrow("drain cancel failed: 403");
  });

  // The inventory: no dashboard source sends a mutating method except through operatorWrite, so a
  // new write cannot skip the header by calling fetch directly.
  it("operatorWrite is the only place the dashboard sends a mutating method", () => {
    const root = path.resolve(__dirname, "..");
    const hits: string[] = [];
    const walk = (dir: string) => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
          walk(full);
        } else if (/\.(ts|tsx)$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name)) {
          const src = fs.readFileSync(full, "utf8");
          for (const m of src.matchAll(/method:\s*["'`](POST|PUT|PATCH|DELETE)["'`]/g)) {
            hits.push(`${path.relative(root, full)}:${m[1]}`);
          }
        }
      }
    };
    walk(root);
    expect(hits).toEqual([]); // No literal fetch method bypasses the shared writer.
    const api = fs.readFileSync(path.join(root, "lib/api.ts"), "utf8");
    const helper = api.slice(api.indexOf("function operatorWrite"));
    const body = helper.slice(0, helper.indexOf("\n}\n"));
    expect(body).toContain("return fetch(url, {");
    expect(body).toContain("method,");
    expect(body).toContain('credentials: "omit"');
    expect(body).toContain('[OPERATOR_HEADER]: "1"');
    expect(api).toContain('return operatorWrite("POST", url, body)');
    expect(api).toContain('operatorWrite("PUT", "/api/v1/lead/harnesses", { harnesses })');
  });
});
