import { describe, expect, it } from "vitest";
import { agentText, statusLabel, viewForStatus, type View } from "./status";
import type { StatusDTO } from "./bindings";

// Ported 1:1 from $REF/desktop/frontend/src/status.test.ts (pure shell view-logic; the parity
// contract for viewForStatus/statusLabel/agentText). The DTO shape mirrors $REF's StatusDTO.
function dto(over: Partial<StatusDTO>): StatusDTO {
  return {
    state: "stopped",
    pid: 0,
    restarts: 0,
    last_err: "",
    url: "http://127.0.0.1:8799",
    healthy: false,
    agent_count: 0,
    configured: true,
    ...over,
  };
}

describe("viewForStatus", () => {
  const cases: Array<[string, StatusDTO | null, View]> = [
    ["null → loading", null, "loading"],
    ["unconfigured → not-configured", dto({ configured: false }), "not-configured"],
    ["running+healthy → dashboard", dto({ state: "running", healthy: true }), "dashboard"],
    ["running but not healthy → starting", dto({ state: "running", healthy: false }), "starting"],
    ["starting → starting", dto({ state: "starting" }), "starting"],
    ["stopped clean → stopped", dto({ state: "stopped" }), "stopped"],
    ["stopped with error → error", dto({ state: "stopped", last_err: "boom" }), "error"],
  ];
  for (const [name, input, want] of cases) {
    it(name, () => expect(viewForStatus(input)).toBe(want));
  }
});

describe("statusLabel", () => {
  it("loading when no snapshot", () => expect(statusLabel(null)).toBe("Loading…"));
  it("not configured", () => expect(statusLabel(dto({ configured: false }))).toBe("Not configured"));
  it("running idle", () =>
    expect(statusLabel(dto({ state: "running", healthy: true, agent_count: 0 }))).toBe("Running — idle"));
  it("running with one agent (singular)", () =>
    expect(statusLabel(dto({ state: "running", healthy: true, agent_count: 1 }))).toBe("Running — 1 agent"));
  it("running with multiple agents (plural)", () =>
    expect(statusLabel(dto({ state: "running", healthy: true, agent_count: 4 }))).toBe("Running — 4 agents"));
  it("running but not yet healthy reads as starting", () =>
    expect(statusLabel(dto({ state: "running", healthy: false }))).toBe("Starting…"));
  it("stopped with error", () =>
    expect(statusLabel(dto({ state: "stopped", last_err: "x" }))).toBe("Stopped (error)"));
});

describe("agentText", () => {
  it("singular", () => expect(agentText(1)).toBe("1 agent"));
  it("plural", () => expect(agentText(3)).toBe("3 agents"));
});
