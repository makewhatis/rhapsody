import { describe, expect, it } from "vitest";
import { configWithForm, formFromConfig, type ConfigMap } from "./config";

// A representative on-disk front-matter map (what GET /api/v1/config returns). Includes an
// `otel` block the Settings form does not edit, to prove unknown keys are preserved.
function sampleConfig(): ConfigMap {
  return {
    tracker: {
      kind: "linear",
      api_key: "$LINEAR_API_KEY",
      project_slug: "symphony",
      active_states: ["Todo", "In Progress"],
      terminal_states: ["Done", "Cancelled"],
      review_promote_state: "In Review",
      milestone: "David's Tasks",
    },
    agent: { backend: "claude", max_concurrent_agents: 2, max_turns: 40 },
    claude: { model: "claude-opus-4-8", permission_mode: "bypassPermissions", billing_guard: true },
    workspace: { root: "~/.symphony/workspaces" },
    server: { port: 8799 },
    otel: { enabled: false, endpoint: "http://localhost:4317" },
  };
}

describe("formFromConfig", () => {
  it("reads nested fields, joining list fields with commas", () => {
    const f = formFromConfig(sampleConfig());
    expect(f.projectSlug).toBe("symphony");
    expect(f.activeStates).toBe("Todo, In Progress");
    expect(f.terminalStates).toBe("Done, Cancelled");
    expect(f.reviewPromoteState).toBe("In Review");
    expect(f.milestone).toBe("David's Tasks");
    expect(f.maxConcurrentAgents).toBe("2");
    expect(f.maxTurns).toBe("40");
    expect(f.model).toBe("claude-opus-4-8");
    expect(f.permissionMode).toBe("bypassPermissions");
    expect(f.billingGuard).toBe(true);
    expect(f.workspaceRoot).toBe("~/.symphony/workspaces");
    expect(f.serverPort).toBe("8799");
  });

  it("uses empty/zero defaults for absent fields", () => {
    const f = formFromConfig({});
    expect(f.projectSlug).toBe("");
    expect(f.activeStates).toBe("");
    expect(f.maxConcurrentAgents).toBe("");
    expect(f.billingGuard).toBe(true); // billing guard defaults on (nil => enabled)
  });
});

describe("configWithForm", () => {
  it("writes edited fields back and PRESERVES unknown keys (otel) and the api_key indirection", () => {
    const cfg = sampleConfig();
    const f = formFromConfig(cfg);
    f.projectSlug = "changed-slug";
    f.activeStates = "Todo, In Progress, In Review";
    f.maxConcurrentAgents = "3";
    f.billingGuard = false;

    const out = configWithForm(cfg, f);
    const tracker = out.tracker as Record<string, unknown>;
    expect(tracker.project_slug).toBe("changed-slug");
    expect(tracker.active_states).toEqual(["Todo", "In Progress", "In Review"]);
    expect(tracker.api_key).toBe("$LINEAR_API_KEY"); // never rewritten by the form
    expect((out.agent as Record<string, unknown>).max_concurrent_agents).toBe(3);
    expect((out.claude as Record<string, unknown>).billing_guard).toBe(false);
    // Unknown block survives untouched.
    expect(out.otel).toEqual({ enabled: false, endpoint: "http://localhost:4317" });
  });

  it("round-trips: applying the unedited form yields an equivalent config for covered fields", () => {
    const cfg = sampleConfig();
    const out = configWithForm(cfg, formFromConfig(cfg));
    expect(out).toEqual(cfg);
  });

  it("is idempotent for a minimal config: does NOT inject a claude block that was absent", () => {
    const cfg: ConfigMap = { tracker: { kind: "linear", project_slug: "x" }, agent: { backend: "claude" } };
    const out = configWithForm(cfg, formFromConfig(cfg));
    expect("claude" in out).toBe(false); // billing_guard default (true) must not materialize a block
    expect(out).toEqual(cfg);
  });

  it("writes billing_guard only when the user sets the non-default false", () => {
    const cfg: ConfigMap = { tracker: { kind: "linear", project_slug: "x" }, agent: { backend: "claude" } };
    const out = configWithForm(cfg, { ...formFromConfig(cfg), billingGuard: false });
    expect((out.claude as Record<string, unknown>).billing_guard).toBe(false);
  });

  it("does not mutate the input config", () => {
    const cfg = sampleConfig();
    const f = formFromConfig(cfg);
    f.projectSlug = "other";
    configWithForm(cfg, f);
    expect((cfg.tracker as Record<string, unknown>).project_slug).toBe("symphony");
  });

  it("omits empty number/list fields rather than writing zero/empty", () => {
    const out = configWithForm({}, { ...formFromConfig({}), projectSlug: "x" });
    const tracker = out.tracker as Record<string, unknown>;
    expect(tracker.project_slug).toBe("x");
    expect("max_concurrent_agents" in (out.agent as Record<string, unknown> ?? {})).toBe(false);
    expect("active_states" in tracker).toBe(false);
  });
});
