// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, within } from "@testing-library/react";
import type { UiAgent, UiGlobal } from "@/lib/settings-model";
import { AgentList, EmptyState } from "@/components/settings/AgentList";

const global = {
  model: "claude-opus-4-8",
  effort: "high",
  permission: "acceptEdits",
  ultracode: false,
  maxConcurrent: 3,
  maxTurns: 60,
  backoff: "exponential",
  billingGuard: true,
  command: "claude",
  requestTimeoutMin: 60,
  stallTimeoutMin: 5,
  extraArgs: "",
  workspaceRoot: "/ws",
  historyRetentionDays: 30,
  persistArtifacts: true,
  dashboardPort: 4317,
  pollIntervalSec: 2,
  telemetryEnabled: false,
  telemetryEndpoint: "",
  logsPath: "/logs",
  prompt: "",
  promptFile: "",
  gitFlow: "any",
  workspaceMode: "worktree",
  labels: [],
  capabilities: [],
  dependencyMode: "disabled",
  claimMode: "assignee",
  githubSummons: false,
  mcpEnabled: true,
  mcpAllowSendMessage: true,
  mcpAllowStop: false,
  mcpAllowResume: false,
} satisfies UiGlobal;

function agent(over: Partial<UiAgent> = {}): UiAgent {
  return {
    id: "infra",
    name: "Infra Bot",
    color: "#34d399",
    projectSlug: "infra-9c29",
    projectName: "Infrastructure",
    repo: "git@github.com:example/demo-repo.git",
    repoShort: "example/demo-repo",
    milestone: "v1",
    labels: [],
    capabilities: [],
    enabled: true,
    status: "running",
    running: 2,
    activeStates: ["Todo", "In Progress"],
    terminalStates: ["Done"],
    reviewStates: ["In Review"],
    reviewPromote: "In Progress",
    cap: 2,
    prompt: "",
    promptFile: "",
    overrides: {},
    workspaceModeRecommended: false,
    ...over,
  };
}

afterEach(cleanup);

describe("AgentList (rows)", () => {
  const agents = [
    agent({ id: "infra", name: "Infra Bot", repo: "git@github.com:makewhatis/rhapsody.git", repoShort: "makewhatis/rhapsody", overrides: { model: "claude-opus-4-8" }, enabled: true }),
    agent({ id: "core", name: "Core Bot", repo: "git@github.com:makewhatis/core.git", repoShort: "makewhatis/core", projectSlug: "core-5f1a", projectName: "Core Platform", overrides: {}, enabled: false, status: "paused", running: 0 }),
  ];

  it("renders the header counts + the rust seats-playing fragment and an Add-agent button that fires openSheet", () => {
    const openSheet = vi.fn();
    // maxConcurrent 3; Infra enabled & playing 2 runs, Core paused: 2 configured · 1 enabled · 2 of 3 playing.
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={openSheet} />);
    expect(screen.getByText(/2 configured/)).toBeTruthy();
    expect(screen.getByText(/1 enabled/)).toBeTruthy();
    expect(screen.getByText("2 of 3 seats playing")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Add agent/ }));
    expect(openSheet).toHaveBeenCalledOnce();
  });

  it("leads each row with the repo (mono) and the Linear project", () => {
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={() => {}} />);
    expect(screen.getByText("makewhatis/rhapsody")).toBeTruthy();
    expect(screen.getByText("makewhatis/core")).toBeTruthy();
    expect(screen.getByText("Core Platform")).toBeTruthy();
  });

  it("shows the effective model with claude- stripped and an override dot only when overridden", () => {
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={() => {}} />);
    // Both resolve to opus-4-8 (Infra overrides it, Core inherits the same global default), with
    // the "claude-" prefix stripped.
    expect(screen.getAllByText("opus-4-8").length).toBe(2);
    // …but only the overriding agent shows the override dot.
    expect(screen.getAllByTitle("Overridden").length).toBe(1);
  });

  it("shows the dashed open-seats affordance (maxConcurrent − enabled) and fires openSheet", () => {
    const openSheet = vi.fn();
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={openSheet} />);
    // 3 seats − 1 enabled = 2 open.
    const affordance = screen.getByRole("button", { name: /2 seats open/ });
    fireEvent.click(affordance);
    expect(openSheet).toHaveBeenCalledOnce();
  });

  it("hides the open-seats affordance when every seat is claimed (open = 0)", () => {
    // 3 enabled agents against a 3-seat cap -> 0 open -> no affordance.
    const full = [agent({ enabled: true }), agent({ enabled: true }), agent({ enabled: true })];
    render(<AgentList agents={full} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={() => {}} />);
    expect(screen.queryByText(/seats open/)).toBeNull();
  });

  it("selects a row on click but the enable toggle stops propagation and persists instead", () => {
    const onSelect = vi.fn();
    const onToggle = vi.fn();
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={onSelect} onToggle={onToggle} openSheet={() => {}} />);
    const toggles = screen.getAllByRole("switch");
    fireEvent.click(toggles[0]); // Infra (index 0), currently enabled -> toggled off
    expect(onToggle).toHaveBeenCalledWith(0, false);
    expect(onSelect).not.toHaveBeenCalled();
    // clicking the row body navigates (Core is index 1)
    fireEvent.click(screen.getByText("Core Platform"));
    expect(onSelect).toHaveBeenCalledWith(1);
  });

  it("renders both rows even when two agents transiently share a slug (duplicate id)", () => {
    // Mid-edit, toUiAgent can derive the same id from a shared slug. Keying by index keeps the two
    // rows distinct (React doesn't collide their keys) so selection/state attach to the right row.
    const dupes = [
      agent({ id: "dup", repoShort: "org/first", enabled: true }),
      agent({ id: "dup", repoShort: "org/second", projectName: "Core Platform", enabled: false, status: "paused", running: 0 }),
    ];
    const onSelect = vi.fn();
    render(<AgentList agents={dupes} global={global} listStyle="rows" onSelect={onSelect} onToggle={() => {}} openSheet={() => {}} />);
    expect(screen.getByText("org/first")).toBeTruthy();
    expect(screen.getByText("org/second")).toBeTruthy();
    // The second (duplicate-keyed) row still selects its own index, not the first.
    fireEvent.click(screen.getByText("org/second"));
    expect(onSelect).toHaveBeenCalledWith(1);
  });

  it("renders a card variant when listStyle=cards", () => {
    render(<AgentList agents={agents} global={global} listStyle="cards" onSelect={() => {}} onToggle={() => {}} openSheet={() => {}} />);
    expect(screen.getByText("Infra Bot")).toBeTruthy();
    // cards show a Project meta row
    expect(within(screen.getByText("Infrastructure").closest("div") as HTMLElement).getByText("Infrastructure")).toBeTruthy();
  });
});

describe("EmptyState", () => {
  it("renders the empty copy and fires openSheet from the primary CTA", () => {
    const openSheet = vi.fn();
    render(<EmptyState openSheet={openSheet} />);
    expect(screen.getByText("No agents yet")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Add your first agent/ }));
    expect(openSheet).toHaveBeenCalledOnce();
  });
});
