import { describe, expect, it } from "vitest";
import { viewForStatus, statusLabel, agentText, conductorStatus, type ConductorSignals } from "@/lib/daemon-status";
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

describe("conductorStatus", () => {
  // A reachable, running, healthy daemon with N agents — the base "playing/idle" case.
  function sig(overrides: Partial<ConductorSignals>): ConductorSignals {
    return {
      connecting: false,
      reachable: true,
      running: true,
      degraded: false,
      agentCount: 0,
      pollMs: 2000,
      ...overrides,
    };
  }

  it("reports Playing with a pulsing rust dot and a pluralized agent count", () => {
    const one = conductorStatus(sig({ agentCount: 1 }));
    expect(one.phase).toBe("playing");
    expect(one.label).toBe("Playing — 1 agent");
    expect(one.dot).toBe("var(--rust-text)");
    expect(one.pulse).toBe(true);
    expect(one.detail).toBe("daemon healthy · poll 2s");

    const three = conductorStatus(sig({ agentCount: 3 }));
    expect(three.label).toBe("Playing — 3 agents");
  });

  it("reports Idle with a neutral, non-pulsing dot when running with no agents", () => {
    const m = conductorStatus(sig({ agentCount: 0 }));
    expect(m.phase).toBe("idle");
    expect(m.label).toBe("Idle — watching for tickets");
    expect(m.dot).toBe("var(--neutral)");
    expect(m.pulse).toBe(false);
    expect(m.detail).toBe("daemon healthy · poll 2s");
  });

  it("reports Paused (amber) when the daemon is reachable but stopped", () => {
    const m = conductorStatus(sig({ running: false, agentCount: 0 }));
    expect(m.phase).toBe("paused");
    expect(m.label).toBe("Paused");
    expect(m.dot).toBe("var(--amber)");
    expect(m.pulse).toBe(false);
  });

  it("reports Unreachable (red) with a retrying suffix when the daemon can't be reached", () => {
    const m = conductorStatus(sig({ reachable: false, running: false }));
    expect(m.phase).toBe("unreachable");
    expect(m.label).toBe("Daemon unreachable");
    expect(m.dot).toBe("var(--red)");
    expect(m.detail).toBe("retrying…");
  });

  it("reports Connecting… while the first status is still resolving", () => {
    const m = conductorStatus(sig({ connecting: true }));
    expect(m.phase).toBe("connecting");
    expect(m.label).toBe("Connecting…");
    expect(m.dot).toBe("var(--neutral)");
  });

  it("tints the dot amber and annotates the suffix when the daemon is degraded", () => {
    const m = conductorStatus(sig({ degraded: true, agentCount: 2 }));
    expect(m.phase).toBe("degraded");
    expect(m.dot).toBe("var(--amber)");
    expect(m.pulse).toBe(false);
    expect(m.detail).toBe("daemon degraded · poll 2s");
    // the running label is preserved so the agent count is still visible
    expect(m.label).toBe("Playing — 2 agents");
  });

  it("omits the poll suffix when no interval is known", () => {
    expect(conductorStatus(sig({ pollMs: undefined })).detail).toBe("daemon healthy");
  });
});
