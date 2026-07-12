import { describe, expect, it } from "vitest";
import type {
  GlobalConfigDTO,
  LinearProject,
  ProjectConfigDTO,
  ProjectStatus,
} from "@/lib/api";
import {
  applyUiAgent,
  applyUiGlobal,
  backoffToMs,
  clampProjectCaps,
  duplicateSlugs,
  effectiveModel,
  globalPromoteValid,
  msToBackoff,
  newProjectConfig,
  projectSelectOptions,
  REPO_PROMPT_PATH,
  reviewPromoteValid,
  toUiAgents,
  toUiGlobal,
} from "@/lib/settings-model";

function makeGlobal(over: Partial<GlobalConfigDTO> = {}): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "https://api.linear.app/graphql", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: {
      backend: "claude",
      max_concurrent_agents: 8,
      max_turns: 20,
      max_retry_backoff_ms: 300000,
    },
    claude: {
      command: "claude",
      model: "claude-sonnet-4-6",
      effort: "high",
      permission_mode: "bypassPermissions",
      billing_guard: true,
      ultracode: false,
      turn_timeout_ms: 3600000,
      read_timeout_ms: 5000,
      stall_timeout_ms: 300000,
      mcp_config: "",
      extra_args: ["--foo"],
    },
    workspace: { root: "/ws" },
    storage: { path: "/abs/symphony.db", retention_days: 30 },
    otel: { enabled: false, endpoint: "", protocol: "grpc", service_name: "symphony", insecure: false },
    mcp: { enabled: true, allow_send_message: true, allow_stop: false, allow_resume: false },
    server: { port: 8799 },
    logging: { dir: "~/.symphony/logs" },
    repo: "git@github.com:example/demo-repo.git",
    active_states: ["Todo", "In Progress"],
    terminal_states: ["Done", "Cancelled"],
    canceled_states: ["Cancelled"],
    review_states: [],
    review_promote_state: "In Progress",
    summon_token: "@symphony",
    github_summons: false,
    milestone: "",
    labels: [],
    prompt: "global prompt",
    prompt_file: "",
    git_flow: "graphite",
    workspace_mode: "worktree",
    dependency_mode: "graphite",
    claim_mode: "assignee",
    ...over,
  };
}

describe("toUiGlobal / applyUiGlobal", () => {
  it("maps daemon global -> UI model and back, preserving untouched fields", () => {
    const g = makeGlobal();
    const ui = toUiGlobal(g);
    expect(ui.model).toBe("claude-sonnet-4-6");
    expect(ui.maxConcurrent).toBe(8);
    expect(ui.maxTurns).toBe(20);
    expect(ui.billingGuard).toBe(true);
    expect(ui.ultracode).toBe(false);
    expect(ui.requestTimeoutMin).toBe(60); // 3600000 ms / 60000
    expect(ui.stallTimeoutMin).toBe(5); // 300000 ms / 60000
    expect(ui.command).toBe("claude");
    expect(ui.extraArgs).toBe("--foo");
    expect(ui.pollIntervalSec).toBe(30);
    expect(ui.workspaceRoot).toBe("/ws");
    expect(ui.historyRetentionDays).toBe(30);
    expect(ui.persistArtifacts).toBe(true);
    expect(ui.dashboardPort).toBe(8799);
    expect(ui.telemetryEnabled).toBe(false);
    expect(ui.telemetryEndpoint).toBe("");
    expect(ui.logsPath).toBe("~/.symphony/logs");
    expect(ui.backoff).toBe("exponential");
    // labels are surfaced read-only (no editor) and ride through applyUiGlobal untouched.
    expect(toUiGlobal(makeGlobal({ labels: ["jp-symphony"] })).labels).toEqual(["jp-symphony"]);
    expect(applyUiGlobal(makeGlobal({ labels: ["jp-symphony"] }), ui).labels).toEqual(["jp-symphony"]);

    const next = applyUiGlobal(g, {
      ...ui,
      model: "claude-opus-4-8",
      maxConcurrent: 5,
      requestTimeoutMin: 2,
      extraArgs: "--mcp ./mcp.json --verbose",
      pollIntervalSec: 2,
      telemetryEnabled: true,
      telemetryEndpoint: "https://otlp:4318",
      persistArtifacts: false,
      ultracode: true,
    });
    expect(next.claude.model).toBe("claude-opus-4-8");
    expect(next.claude.ultracode).toBe(true);
    expect(next.agent.max_concurrent_agents).toBe(5);
    expect(next.claude.turn_timeout_ms).toBe(120000);
    expect(next.claude.extra_args).toEqual(["--mcp", "./mcp.json", "--verbose"]);
    expect(next.polling.interval_ms).toBe(2000);
    expect(next.otel.endpoint).toBe("https://otlp:4318");
    expect(next.otel.enabled).toBe(true);
    expect(next.storage.path).toBe("off");
    // untouched secret-bearing + structural fields survive the round-trip
    expect(next.tracker.api_key_set).toBe(true);
    expect(next.summon_token).toBe("@symphony");
    expect(next.prompt).toBe("global prompt");
  });

  it("maps otel.enabled to an explicit toggle, independent of endpoint-presence (INF-299)", () => {
    // The seeded default-on config (enabled + a hub endpoint) surfaces the toggle ON.
    const seeded = makeGlobal({
      otel: { enabled: true, endpoint: "https://collector.example:4317", protocol: "grpc", service_name: "symphony", insecure: false },
    });
    const ui = toUiGlobal(seeded);
    expect(ui.telemetryEnabled).toBe(true);
    expect(ui.telemetryEndpoint).toBe("https://collector.example:4317");

    // Toggling export OFF disables it while KEEPING the endpoint (so re-enabling needs no re-typing).
    const optedOut = applyUiGlobal(seeded, { ...ui, telemetryEnabled: false });
    expect(optedOut.otel.enabled).toBe(false);
    expect(optedOut.otel.endpoint).toBe("https://collector.example:4317");
    // Transport fields ride through untouched.
    expect(optedOut.otel.protocol).toBe("grpc");
    expect(optedOut.otel.service_name).toBe("symphony");
    expect(optedOut.otel.insecure).toBe(false);

    // A non-empty endpoint no longer auto-enables: the toggle is authoritative.
    const stillOff = applyUiGlobal(seeded, { ...ui, telemetryEnabled: false, telemetryEndpoint: "https://otlp:4318" });
    expect(stillOff.otel.enabled).toBe(false);
    expect(stillOff.otel.endpoint).toBe("https://otlp:4318");
  });

  it("maps the global git_flow both ways and seeds the default when unset", () => {
    // Explicit value round-trips.
    const g = makeGlobal({ git_flow: "graphite" });
    expect(toUiGlobal(g).gitFlow).toBe("graphite");
    expect(applyUiGlobal(g, { ...toUiGlobal(g), gitFlow: "any" }).git_flow).toBe("any");

    // An empty/unset git_flow coalesces to the "any" default in the UI.
    const blank = makeGlobal({ git_flow: "" });
    expect(toUiGlobal(blank).gitFlow).toBe("any");
  });

  it("maps the global workspace_mode both ways and seeds the worktree default when unset (INF-418)", () => {
    // Explicit value round-trips.
    const g = makeGlobal({ workspace_mode: "clone" });
    expect(toUiGlobal(g).workspaceMode).toBe("clone");
    expect(applyUiGlobal(g, { ...toUiGlobal(g), workspaceMode: "worktree" }).workspace_mode).toBe("worktree");

    // An empty/unset workspace_mode coalesces to the "worktree" default in the UI.
    const blank = makeGlobal({ workspace_mode: "" });
    expect(toUiGlobal(blank).workspaceMode).toBe("worktree");
  });

  it("maps the global dependency_mode both ways and seeds the default when unset (INF-320)", () => {
    // Explicit value round-trips (mirrors git_flow; the value set is the three-valued enum).
    const g = makeGlobal({ dependency_mode: "dag" });
    expect(toUiGlobal(g).dependencyMode).toBe("dag");
    expect(applyUiGlobal(g, { ...toUiGlobal(g), dependencyMode: "graphite" }).dependency_mode).toBe("graphite");

    // An empty/unset dependency_mode coalesces to the flat "disabled" seed (NOT git_flow-derived):
    // a git_flow:"graphite" project with a blank dependency_mode is STILL "disabled" in the UI.
    const blank = makeGlobal({ dependency_mode: "", git_flow: "graphite" });
    expect(toUiGlobal(blank).dependencyMode).toBe("disabled");
  });

  it("maps the tracker github_summons flag both ways and defaults false when absent (AIE-302)", () => {
    // true round-trips through the UI model and back to the daemon shape.
    const on = makeGlobal({ github_summons: true });
    expect(toUiGlobal(on).githubSummons).toBe(true);
    expect(applyUiGlobal(on, { ...toUiGlobal(on), githubSummons: false }).github_summons).toBe(false);

    // Absent => false (opt-in); the user's edit is written explicitly, not just the loaded value.
    const off = makeGlobal();
    expect(toUiGlobal(off).githubSummons).toBe(false);
    expect(applyUiGlobal(off, { ...toUiGlobal(off), githubSummons: true }).github_summons).toBe(true);
  });

  it("maps the global prompt + prompt_file both ways", () => {
    const g = makeGlobal({ prompt: "inline body {{ issue.identifier }}", prompt_file: "prompts/global.md" });
    const ui = toUiGlobal(g);
    expect(ui.prompt).toBe("inline body {{ issue.identifier }}");
    expect(ui.promptFile).toBe("prompts/global.md");

    // Editing the path round-trips; a trimmed empty path clears the file source (back to inline).
    const withFile = applyUiGlobal(g, { ...ui, promptFile: "  /abs/prompt.md  " });
    expect(withFile.prompt_file).toBe("/abs/prompt.md");
    const cleared = applyUiGlobal(g, { ...ui, promptFile: "   ", prompt: "new inline" });
    expect(cleared.prompt_file).toBe("");
    expect(cleared.prompt).toBe("new inline");
  });

  it("restores a real storage path when persist is toggled back on (no data loss)", () => {
    const base = makeGlobal({ storage: { path: "/abs/symphony.db", retention_days: 30 } });
    const off = applyUiGlobal(base, { ...toUiGlobal(base), persistArtifacts: false });
    expect(off.storage.path).toBe("off");
    const onAgain = applyUiGlobal(off, { ...toUiGlobal(off), persistArtifacts: true }, base);
    expect(onAgain.storage.path).toBe("/abs/symphony.db");
  });

  it("maps retry-backoff strategy <-> max delay ms", () => {
    expect(backoffToMs("fixed")).toBe(30000);
    expect(backoffToMs("linear")).toBe(120000);
    expect(backoffToMs("exponential")).toBe(300000);
    expect(msToBackoff(30000)).toBe("fixed");
    expect(msToBackoff(120000)).toBe("linear");
    expect(msToBackoff(300000)).toBe("exponential");
    expect(msToBackoff(0)).toBe("fixed");
  });

  it("floors a sub-minute turn_timeout to 1 minute (round-trips to 60000, never 0)", () => {
    // A deliberate 20s cap rounds to 0 minutes under plain Math.round; an unrelated General-tab save
    // would then write turn_timeout_ms:0, which the daemon floors <=0 to 1 HOUR (runner.go) — silently
    // corrupting the cap. Floor to 1 min so the worst case is a round-UP to 60000, never 0.
    const g = makeGlobal({ claude: { ...makeGlobal().claude, turn_timeout_ms: 20000 } });
    const ui = toUiGlobal(g);
    expect(ui.requestTimeoutMin).toBe(1);
    expect(applyUiGlobal(g, ui).claude.turn_timeout_ms).toBe(60000);
  });

  it("preserves a global stall_timeout_ms:0 as disabled and ceilings sub-minute (unlike turn's floor)", () => {
    // The global stall is display-only (no General-tab control; preserved verbatim via ...g.claude) but
    // is surfaced as the per-agent inherited default — so its display must honor the daemon's
    // disabled-at-0 semantics, not a 1-min floor. 0 stays 0; a sub-minute value ceilings to 1 (a plain
    // Math.round would flip a real sub-minute stall into 0 = disabled).
    const disabled = toUiGlobal(makeGlobal({ claude: { ...makeGlobal().claude, stall_timeout_ms: 0 } }));
    expect(disabled.stallTimeoutMin).toBe(0);
    const sub = toUiGlobal(makeGlobal({ claude: { ...makeGlobal().claude, stall_timeout_ms: 20000 } }));
    expect(sub.stallTimeoutMin).toBe(1);
  });
});

describe("toUiAgents", () => {
  const linear: LinearProject[] = [
    { id: "1", name: "Infrastructure", slug: "infra-9c29", team: "INF", color: "#34d399" },
    { id: "2", name: "Core Platform", slug: "core-5f1a", team: "CORE", color: "#38bdf8" },
  ];
  const statuses: ProjectStatus[] = [{ slug: "infra-9c29", name: "Infra", status: "active", running: 2 }];

  const projects: ProjectConfigDTO[] = [
    {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      repo: "",
      milestone: "v1",
      enabled: true,
      max_concurrent_agents: 2, // explicit per-agent cap (below the global max of 8)
      overrides: { model: "claude-opus-4-8" },
      effective: {
        name: "Infra Bot",
        repo: "git@github.com:example/demo-repo.git",
        model: "claude-opus-4-8",
        effort: "high",
        permission: "bypassPermissions",
        ultracode: false,
        turn_timeout_ms: 3600000,
        stall_timeout_ms: 300000,
        active_states: ["Todo", "In Progress"],
        terminal_states: ["Done", "Cancelled"],
        canceled_states: ["Cancelled"],
        review_states: [],
        review_promote_state: "In Progress",
        max_concurrent_agents: 2,
        milestone: "v1",
        labels: [],
        prompt: "global prompt",
        prompt_file: "",
        git_flow: "graphite",
        workspace_mode: "worktree",
        dependency_mode: "graphite",
        claim_mode: "assignee",
        enabled: true,
      },
    },
    {
      name: "Core Bot",
      slugs: ["core-5f1a"],
      enabled: false,
      overrides: {},
    },
  ];

  it("resolves effective model, override dot, repoShort, colour, status + count", () => {
    const g = makeGlobal();
    const agents = toUiAgents(projects, g, linear, statuses);
    const infra = agents[0];
    expect(infra.id).toBe("infra-9c29");
    expect(infra.projectSlug).toBe("infra-9c29");
    expect(infra.color).toBe("#34d399");
    expect(infra.repoShort).toBe("example/demo-repo");
    expect(infra.overrides.model).toBe("claude-opus-4-8");
    expect(effectiveModel(infra, toUiGlobal(g))).toBe("claude-opus-4-8");
    // enabled + running>0 => running with a live count
    expect(infra.status).toBe("running");
    expect(infra.running).toBe(2);
    expect(infra.cap).toBe(2);

    const core = agents[1];
    // no model override => inherits the global default model
    expect(core.overrides.model).toBeUndefined();
    expect(effectiveModel(core, toUiGlobal(g))).toBe("claude-sonnet-4-6");
    // disabled => paused regardless of live status
    expect(core.status).toBe("paused");
    expect(core.color).toBe("#38bdf8");
  });

  it("seeds reviewPromote from the (live) global, never a project's stale effective snapshot", () => {
    const g = makeGlobal({ review_promote_state: "Doing" });
    const stale: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: {},
      // a stale effective snapshot carrying the OLD global promote
      effective: { ...projects[0].effective!, review_promote_state: "In Progress" },
    };
    const [agent] = toUiAgents([stale], g, linear, statuses);
    expect(agent.reviewPromote).toBe("Doing");
  });
});

describe("applyUiAgent", () => {
  const g = makeGlobal();
  const orig: ProjectConfigDTO = {
    name: "Infra Bot",
    slugs: ["infra-9c29"],
    repo: "git@github.com:org/old.git",
    enabled: true,
    overrides: { model: "claude-opus-4-8" },
  };

  it("writes a SPARSE override map (present keys only) and never an inherited key", () => {
    const ui = toUiAgents([{ ...orig, effective: undefined }], g, [], [])[0];
    // override effort, leave permission inherited
    ui.overrides = { model: "claude-opus-4-8", effort: "low" };
    const next = applyUiAgent(orig, ui, g);
    expect(next.overrides).toEqual({ model: "claude-opus-4-8", effort: "low" });
    expect("permission" in next.overrides).toBe(false);
  });

  it("threads a boolean ultracode override both ways (present key only when overridden)", () => {
    // Reading: an explicit ultracode override surfaces in the UI overrides map…
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { ultracode: false },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    expect(ui.overrides.ultracode).toBe(false);
    // …and writing it back keeps the sparse key.
    expect(applyUiAgent(overridden, ui, g).overrides).toEqual({ ultracode: false });

    // An inheriting agent has no ultracode key, and an Override→true seeds it on save.
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([inheriting], g, [], [])[0];
    expect("ultracode" in ui2.overrides).toBe(false);
    ui2.overrides = { ...ui2.overrides, ultracode: true };
    expect(applyUiAgent(inheriting, ui2, g).overrides).toEqual({ ultracode: true });
  });

  it("threads the timeout/billing/command overrides both ways (min<->ms, sparse, omit on inherit)", () => {
    // Reading: explicit overrides surface in the UI map; the two timeouts convert ms->min.
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: {
        turn_timeout_ms: 300000,
        stall_timeout_ms: 60000,
        billing_guard: false,
        command: "claude-custom",
      },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    expect(ui.overrides.turnTimeoutMin).toBe(5); // 300000 / 60000
    expect(ui.overrides.stallTimeoutMin).toBe(1); // 60000 / 60000
    expect(ui.overrides.billingGuard).toBe(false);
    expect(ui.overrides.command).toBe("claude-custom");
    // …and writing back converts min->ms and keeps every sparse key.
    expect(applyUiAgent(overridden, ui, g).overrides).toEqual({
      turn_timeout_ms: 300000,
      stall_timeout_ms: 60000,
      billing_guard: false,
      command: "claude-custom",
    });

    // An inheriting agent has none of the four keys; engaging only some seeds only those on save.
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([inheriting], g, [], [])[0];
    expect("turnTimeoutMin" in ui2.overrides).toBe(false);
    expect("stallTimeoutMin" in ui2.overrides).toBe(false);
    expect("billingGuard" in ui2.overrides).toBe(false);
    expect("command" in ui2.overrides).toBe(false);
    ui2.overrides = { ...ui2.overrides, turnTimeoutMin: 10, billingGuard: true };
    expect(applyUiAgent(inheriting, ui2, g).overrides).toEqual({
      turn_timeout_ms: 600000,
      billing_guard: true,
    });
  });

  it("threads the git_flow override both ways: present key only when overridden, blank inherits", () => {
    // Reading: an explicit git_flow override surfaces in the UI map…
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { git_flow: "any" },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    expect(ui.overrides.gitFlow).toBe("any");
    // …and writing it back keeps the sparse key.
    expect(applyUiAgent(overridden, ui, g).overrides).toEqual({ git_flow: "any" });

    // An inheriting agent has no gitFlow key; engaging it seeds the override on save.
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([inheriting], g, [], [])[0];
    expect("gitFlow" in ui2.overrides).toBe(false);
    ui2.overrides = { ...ui2.overrides, gitFlow: "graphite" };
    expect(applyUiAgent(inheriting, ui2, g).overrides).toEqual({ git_flow: "graphite" });

    // A blank git_flow inherits (clearing the field never POSTs git_flow:"").
    ui.overrides = { ...ui.overrides, gitFlow: "  " };
    expect("git_flow" in applyUiAgent(overridden, ui, g).overrides).toBe(false);
  });

  it("threads the workspace_mode override both ways: present key only when overridden, blank inherits (INF-418)", () => {
    // Reading: an explicit workspace_mode override surfaces in the UI map…
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { workspace_mode: "clone" },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    expect(ui.overrides.workspaceMode).toBe("clone");
    // …and writing it back keeps the sparse key.
    expect(applyUiAgent(overridden, ui, g).overrides).toEqual({ workspace_mode: "clone" });

    // An inheriting agent has no workspaceMode key; engaging it seeds the override on save.
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([inheriting], g, [], [])[0];
    expect("workspaceMode" in ui2.overrides).toBe(false);
    ui2.overrides = { ...ui2.overrides, workspaceMode: "worktree" };
    expect(applyUiAgent(inheriting, ui2, g).overrides).toEqual({ workspace_mode: "worktree" });

    // A blank workspace_mode inherits (clearing the field never POSTs workspace_mode:"").
    ui.overrides = { ...ui.overrides, workspaceMode: "  " };
    expect("workspace_mode" in applyUiAgent(overridden, ui, g).overrides).toBe(false);
  });

  it("surfaces the daemon's workspace_mode_recommended hint on the UI agent (INF-418)", () => {
    const recommended: ProjectConfigDTO = {
      name: "Stacking Bot",
      slugs: ["stacking-9c29"],
      enabled: true,
      overrides: {},
      workspace_mode_recommended: true,
    };
    const ui = toUiAgents([{ ...recommended, effective: undefined }], g, [], [])[0];
    expect(ui.workspaceModeRecommended).toBe(true);

    // Absent => false (no nag), and the hint is display-only: it is never written back on save.
    const plain: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([plain], g, [], [])[0];
    expect(ui2.workspaceModeRecommended).toBe(false);
    expect("workspace_mode_recommended" in applyUiAgent(plain, ui2, g)).toBe(false);
  });

  it("threads the dependency_mode override both ways: present key only when overridden, blank inherits (INF-320)", () => {
    // Reading: an explicit dependency_mode override surfaces in the UI map…
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { dependency_mode: "dag" },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    expect(ui.overrides.dependencyMode).toBe("dag");
    // …and writing it back keeps the sparse key.
    expect(applyUiAgent(overridden, ui, g).overrides).toEqual({ dependency_mode: "dag" });

    // An inheriting agent has no dependencyMode key; engaging it seeds the override on save.
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui2 = toUiAgents([inheriting], g, [], [])[0];
    expect("dependencyMode" in ui2.overrides).toBe(false);
    ui2.overrides = { ...ui2.overrides, dependencyMode: "graphite" };
    expect(applyUiAgent(inheriting, ui2, g).overrides).toEqual({ dependency_mode: "graphite" });

    // A blank dependency_mode inherits (clearing the field never POSTs dependency_mode:"").
    ui.overrides = { ...ui.overrides, dependencyMode: "  " };
    expect("dependency_mode" in applyUiAgent(overridden, ui, g).overrides).toBe(false);
  });

  it("floors a sub-minute per-agent turn_timeout override to 1 min; null stays inherited", () => {
    // A present sub-minute override (20s) floors to 1 min — same corruption guard as the global path:
    // rounding to 0 + write-back -> turn_timeout_ms:0, which the daemon floors <=0 to 1 HOUR.
    const sub: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { turn_timeout_ms: 20000 },
    };
    const ui = toUiAgents([sub], g, [], [])[0];
    expect(ui.overrides.turnTimeoutMin).toBe(1);
    expect(applyUiAgent(sub, ui, g).overrides).toEqual({ turn_timeout_ms: 60000 });

    // null/absent stays undefined (inherit the global) — no floor applied.
    const inherit: ProjectConfigDTO = {
      name: "A",
      slugs: ["a"],
      enabled: true,
      overrides: { turn_timeout_ms: null },
    };
    const ui3 = toUiAgents([inherit], g, [], [])[0];
    expect("turnTimeoutMin" in ui3.overrides).toBe(false);
  });

  it("preserves a stall_timeout_ms:0 override as disabled (no 1-min floor); ceilings sub-minute", () => {
    // The stall knob's zero-semantics differ from turn: the daemon treats stall_timeout_ms <= 0 as
    // "stall detection disabled" (reconcile_run.go skips `stall <= 0`) and the Stepper is min=0. A
    // 1-min floor (as turn uses) would corrupt a deliberately-disabled 0 into a 1-minute stall kill.
    const disabled: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { stall_timeout_ms: 0 },
    };
    const uiDisabled = toUiAgents([disabled], g, [], [])[0];
    expect(uiDisabled.overrides.stallTimeoutMin).toBe(0); // 0 stays disabled, NOT floored to 1
    // …and round-trips back to 0 (disabled), NOT to 60000 (the regressed "1-minute stall kill").
    expect(applyUiAgent(disabled, uiDisabled, g).overrides).toEqual({ stall_timeout_ms: 0 });

    // A nonzero sub-minute stall (20s) ceilings to the 1-min UI floor — Math.round would have flipped
    // it to 0 (= disabled), lossy the other way. The UI edits whole minutes, so 1 min is the most
    // conservative nonzero mapping that never disables a configured stall.
    const sub: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { stall_timeout_ms: 20000 },
    };
    const uiSub = toUiAgents([sub], g, [], [])[0];
    expect(uiSub.overrides.stallTimeoutMin).toBe(1);
    expect(applyUiAgent(sub, uiSub, g).overrides).toEqual({ stall_timeout_ms: 60000 });

    // null/absent stays undefined (inherit the global) — same as turn.
    const inheritStall: ProjectConfigDTO = {
      name: "A",
      slugs: ["a"],
      enabled: true,
      overrides: { stall_timeout_ms: null },
    };
    const uiInherit = toUiAgents([inheritStall], g, [], [])[0];
    expect("stallTimeoutMin" in uiInherit.overrides).toBe(false);
  });

  it("treats a blank command override as inherit (clearing the field never POSTs command:'')", () => {
    // An agent with an explicit command override whose field the user then clears…
    const overridden: ProjectConfigDTO = {
      name: "Infra Bot",
      slugs: ["infra-9c29"],
      enabled: true,
      overrides: { command: "claude-custom" },
    };
    const ui = toUiAgents([{ ...overridden, effective: undefined }], g, [], [])[0];
    ui.overrides = { ...ui.overrides, command: "" };
    // …saves with NO command key (inherit), not command:"" (which would clobber the global binary).
    expect("command" in applyUiAgent(overridden, ui, g).overrides).toBe(false);

    // Whitespace-only is blank too.
    ui.overrides = { ...ui.overrides, command: "   " };
    expect("command" in applyUiAgent(overridden, ui, g).overrides).toBe(false);
  });

  it("collapses to the selected slug and persists per-agent state lists + cap when they diverge", () => {
    const ui = toUiAgents([orig], g, [], [])[0];
    ui.projectSlug = "core-5f1a";
    ui.activeStates = ["Todo", "Doing"]; // diverges from global
    ui.cap = 3; // below global max (8)
    const next = applyUiAgent(orig, ui, g);
    expect(next.slugs).toEqual(["core-5f1a"]);
    expect(next.active_states).toEqual(["Todo", "Doing"]);
    expect(next.max_concurrent_agents).toBe(3);
  });

  it("does NOT pin inherited state lists / cap / repo / milestone when an unrelated field is edited", () => {
    // an agent inheriting everything (no per-project overrides)
    const inheriting: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui = toUiAgents([inheriting], g, [], [])[0]; // states/cap/repo/milestone come from the global
    ui.name = "Renamed"; // edit something unrelated
    const next = applyUiAgent(inheriting, ui, g);
    expect(next.name).toBe("Renamed");
    expect(next.active_states).toBeUndefined(); // == global => still inherits
    expect(next.terminal_states).toBeUndefined();
    expect(next.review_states).toBeUndefined();
    expect(next.max_concurrent_agents).toBeUndefined(); // cap == global max => inherits
    expect(next.repo).toBeUndefined(); // == global repo => still inherits
    expect(next.milestone).toBeUndefined(); // == global milestone => still inherits
  });

  it("maps a per-agent prompt_file override both ways (empty => inherit)", () => {
    const orig: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {}, prompt_file: "prompts/a.md" };
    const ui = toUiAgents([orig], g, [], [])[0];
    expect(ui.promptFile).toBe("prompts/a.md"); // the raw override surfaces

    // Writing a path persists it as the per-agent override.
    ui.promptFile = "  prompts/changed.md  ";
    expect(applyUiAgent(orig, ui, g).prompt_file).toBe("prompts/changed.md");

    // Clearing the path (whitespace) drops the override → inherit the global prompt_file.
    const cleared = toUiAgents([orig], g, [], [])[0];
    cleared.promptFile = "   ";
    expect(applyUiAgent(orig, cleared, g).prompt_file).toBeUndefined();
  });

  it("surfaces a project's OWN labels (raw), not the merged global default", () => {
    const gWithLabels = makeGlobal({ labels: ["bug", "urgent"] });
    const p: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui = toUiAgents([p], gWithLabels, [], [])[0];
    // A project with no per-project override shows an EMPTY chips field (it inherits at the daemon).
    // The chips bind to the raw override so removing the last chip can actually clear the field;
    // the inherited global value is surfaced separately (toUiGlobal.labels) for the editor's hint.
    expect(ui.labels).toEqual([]);
    expect(toUiGlobal(gWithLabels).labels).toEqual(["bug", "urgent"]);
  });

  it("removing a project's only label clears the field instead of re-inheriting the global", () => {
    // Regression: a project pinning ["symphony-do"] under a global default of ["jp-symphony"]. The
    // ✕ on the only chip used to flip the chip to the inherited global value (the chips bound to the
    // EFFECTIVE merge), so it never disappeared. With raw binding the field empties.
    const gWithLabels = makeGlobal({ labels: ["jp-symphony"] });
    const orig: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {}, labels: ["symphony-do"] };
    const ui = toUiAgents([orig], gWithLabels, [], [])[0];
    expect(ui.labels).toEqual(["symphony-do"]);
    const dto = applyUiAgent(orig, { ...ui, labels: [] }, gWithLabels);
    expect(dto.labels).toBeUndefined(); // saved as "inherit"
    const after = toUiAgents([dto], gWithLabels, [], [])[0];
    expect(after.labels).toEqual([]); // chip gone — NOT flipped to ["jp-symphony"]
  });

  it("applyUiAgent writes a per-project labels override only when it diverges from the global", () => {
    const gWithLabels = makeGlobal({ labels: ["bug"] });
    const p: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui = toUiAgents([p], gWithLabels, [], [])[0];

    // A labels list that matches the global collapses to undefined (inherit)
    ui.labels = ["bug"];
    expect(applyUiAgent(p, ui, gWithLabels).labels).toBeUndefined();

    // A labels list that diverges is written as a per-project override
    ui.labels = ["feature", "bug"];
    expect(applyUiAgent(p, ui, gWithLabels).labels).toEqual(["feature", "bug"]);
  });

  it("applyUiAgent collapses an empty labels list to undefined (inherit)", () => {
    const gWithLabels = makeGlobal({ labels: ["bug"] });
    const p: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    const ui = toUiAgents([p], gWithLabels, [], [])[0];
    ui.labels = [];
    // An empty list means "no override" → inherit (undefined), not "require no labels"
    expect(applyUiAgent(p, ui, gWithLabels).labels).toBeUndefined();
  });
});

describe("toUiAgent state inheritance", () => {
  it("inherits global state lists when a project's lists are empty (matching the daemon)", () => {
    const p: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {}, active_states: [], review_states: [] };
    const ui = toUiAgents([p], makeGlobal(), [], [])[0];
    expect(ui.activeStates).toEqual(["Todo", "In Progress"]); // global active
    expect(ui.reviewStates).toEqual([]); // global review (empty)
  });
});

describe("clampProjectCaps", () => {
  it("bounds per-agent caps above the global max, leaving null (inherit) and in-bounds caps alone", () => {
    const projects: ProjectConfigDTO[] = [
      { name: "A", slugs: ["a"], enabled: true, overrides: {}, max_concurrent_agents: 8 },
      { name: "B", slugs: ["b"], enabled: true, overrides: {}, max_concurrent_agents: 2 },
      { name: "C", slugs: ["c"], enabled: true, overrides: {}, max_concurrent_agents: null },
    ];
    const clamped = clampProjectCaps(projects, 3);
    expect(clamped[0].max_concurrent_agents).toBe(3); // 8 -> 3 (clamped to global max)
    expect(clamped[1].max_concurrent_agents).toBe(2); // already within bounds
    expect(clamped[2].max_concurrent_agents).toBeNull(); // null = inherit, untouched
  });
});

describe("newProjectConfig (INF-279 repo-prompt default)", () => {
  it("seeds a new agent with the canonical repo prompt_file", () => {
    const project: LinearProject = { slug: "new-proj", name: "New Proj" } as LinearProject;
    const cfg = newProjectConfig(project, "  git@github.com:org/repo.git  ");
    expect(cfg.prompt_file).toBe(REPO_PROMPT_PATH);
    expect(cfg.slugs).toEqual(["new-proj"]);
    expect(cfg.repo).toBe("git@github.com:org/repo.git");
    expect(cfg.enabled).toBe(true);
  });
});

describe("duplicateSlugs", () => {
  it("detects a slug configured on more than one agent", () => {
    const mk = (slug: string): ProjectConfigDTO => ({ name: slug, slugs: [slug], enabled: true, overrides: {} });
    expect(duplicateSlugs([mk("a"), mk("b")])).toBe(false);
    expect(duplicateSlugs([mk("a"), mk("a")])).toBe(true);
    expect(duplicateSlugs([])).toBe(false);
  });
});

describe("reviewPromoteValid", () => {
  const review = ["In Review"]; // review enabled
  it("validates promote ∈ active only when review is enabled (covers agt_docs)", () => {
    expect(reviewPromoteValid({ reviewStates: review, activeStates: ["Todo", "In Progress"], reviewPromote: "In Progress" })).toBe(true);
    // agt_docs ships reviewPromote "Shipped" with activeStates ["Todo"] + review ON on purpose
    expect(reviewPromoteValid({ reviewStates: review, activeStates: ["Todo"], reviewPromote: "Shipped" })).toBe(false);
    // empty promote is vacuously valid
    expect(reviewPromoteValid({ reviewStates: review, activeStates: ["Todo"], reviewPromote: "" })).toBe(true);
  });
  it("does NOT block when review is off, even if promote ∉ active (daemon skips the check)", () => {
    expect(reviewPromoteValid({ reviewStates: [], activeStates: ["Todo"], reviewPromote: "Shipped" })).toBe(true);
  });
  it("compares states case-insensitively (matching the daemon's NormalizeState)", () => {
    expect(reviewPromoteValid({ reviewStates: review, activeStates: ["In Progress"], reviewPromote: "in progress" })).toBe(true);
  });
});

describe("globalPromoteValid", () => {
  it("enforces the global scope only when global review states are non-empty", () => {
    // review off globally => always valid regardless of promote
    expect(globalPromoteValid(makeGlobal({ review_states: [], review_promote_state: "Shipped" }))).toBe(true);
    // review on + promote ∈ global active
    expect(
      globalPromoteValid(makeGlobal({ review_states: ["In Review"], active_states: ["Todo", "In Progress"], review_promote_state: "In Progress" })),
    ).toBe(true);
    // review on + promote ∉ global active => invalid (daemon would reject)
    expect(
      globalPromoteValid(makeGlobal({ review_states: ["In Review"], active_states: ["Todo"], review_promote_state: "Shipped" })),
    ).toBe(false);
  });

  // Regression (blank-screen bug): the daemon returns review_states: null when review states are
  // unset — not []. globalPromoteValid must treat null as "review off", never read null.length.
  it("treats a null global review_states as 'review off' without throwing", () => {
    expect(globalPromoteValid(makeGlobal({ review_states: null }))).toBe(true);
  });
});

describe("projectSelectOptions (INF-277 unmatched slug)", () => {
  const projects: LinearProject[] = [
    { id: "1", name: "Symphony App", slug: "872639248532", team: "FND", color: "#fff" },
  ];

  it("maps known projects to options and reports matched", () => {
    const { options, unmatched } = projectSelectOptions(projects, "872639248532");
    expect(unmatched).toBe(false);
    expect(options).toEqual([{ value: "872639248532", label: "Symphony App", note: "872639248532" }]);
  });

  it("appends a synthetic raw-slug option with a 'not found in Linear' note when unmatched", () => {
    const { options, unmatched } = projectSelectOptions(projects, "symphony-app-872639248532");
    expect(unmatched).toBe(true);
    expect(options.at(-1)).toEqual({
      value: "symphony-app-872639248532",
      label: "symphony-app-872639248532",
      note: "not found in Linear",
    });
  });

  it("treats an empty/blank saved slug as unset (not unmatched)", () => {
    expect(projectSelectOptions(projects, "").unmatched).toBe(false);
    expect(projectSelectOptions(projects, "  ").unmatched).toBe(false);
    expect(projectSelectOptions(projects, "  ").options).toHaveLength(1);
  });
});

describe("null list fields from the daemon (blank-screen regression)", () => {
  it("toUiAgents tolerates a null global review_states and resolves it to []", () => {
    const g = makeGlobal({ review_states: null });
    const p: ProjectConfigDTO = { name: "A", slugs: ["a"], enabled: true, overrides: {} };
    expect(() => toUiAgents([p], g, [], [])).not.toThrow();
    expect(toUiAgents([p], g, [], [])[0].reviewStates).toEqual([]);
  });
});
