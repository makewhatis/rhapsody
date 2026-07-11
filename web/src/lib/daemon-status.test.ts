import { describe, expect, it } from "vitest";
import { viewForStatus, statusLabel, agentText } from "@/lib/daemon-status";
import type { StatusDTO } from "@/lib/bindings";

function s(overrides: Partial<StatusDTO>): StatusDTO {
  return {
    state: "stopped",
    pid: 0,
    restarts: 0,
    last_err: "",
    url: "",
    healthy: false,
    agent_count: 0,
    configured: true,
    ...overrides,
  };
}

describe("viewForStatus", () => {
  it("maps every lifecycle phase", () => {
    expect(viewForStatus(null)).toBe("loading");
    expect(viewForStatus(s({ configured: false }))).toBe("not-configured");
    expect(viewForStatus(s({ state: "running", healthy: true }))).toBe("running");
    expect(viewForStatus(s({ state: "running", healthy: false }))).toBe("starting");
    expect(viewForStatus(s({ state: "starting" }))).toBe("starting");
    expect(viewForStatus(s({ state: "stopped", last_err: "" }))).toBe("stopped");
    expect(viewForStatus(s({ state: "stopped", last_err: "boom" }))).toBe("error");
  });
});

describe("statusLabel", () => {
  it("renders the titlebar status text", () => {
    expect(statusLabel(null)).toBe("Loading…");
    expect(statusLabel(s({ configured: false }))).toBe("Not configured");
    expect(statusLabel(s({ state: "running", healthy: true, agent_count: 0 }))).toBe("Running — idle");
    expect(statusLabel(s({ state: "running", healthy: true, agent_count: 1 }))).toBe("Running — 1 agent");
    expect(statusLabel(s({ state: "running", healthy: true, agent_count: 3 }))).toBe("Running — 3 agents");
    expect(statusLabel(s({ state: "running", healthy: false }))).toBe("Starting…");
    expect(statusLabel(s({ state: "stopped" }))).toBe("Stopped");
    expect(statusLabel(s({ state: "stopped", last_err: "boom" }))).toBe("Stopped (error)");
  });
});

describe("agentText", () => {
  it("pluralizes", () => {
    expect(agentText(1)).toBe("1 agent");
    expect(agentText(2)).toBe("2 agents");
  });
});
