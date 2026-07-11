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
    agent({ id: "infra", name: "Infra Bot", overrides: { model: "claude-opus-4-8" }, enabled: true }),
    agent({ id: "core", name: "Core Bot", projectSlug: "core-5f1a", projectName: "Core Platform", overrides: {}, enabled: false, status: "paused", running: 0 }),
  ];

  it("renders the header counts and an Add-agent button that fires openSheet", () => {
    const openSheet = vi.fn();
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={openSheet} />);
    expect(screen.getByText("2 configured · 1 enabled")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Add agent/ }));
    expect(openSheet).toHaveBeenCalledOnce();
  });

  it("shows the effective model with claude- stripped and an override dot only when overridden", () => {
    render(<AgentList agents={agents} global={global} listStyle="rows" onSelect={() => {}} onToggle={() => {}} openSheet={() => {}} />);
    // Both resolve to opus-4-8 (Infra overrides it, Core inherits the same global default), with
    // the "claude-" prefix stripped.
    expect(screen.getAllByText("opus-4-8").length).toBe(2);
    // …but only the overriding agent shows the emerald override dot.
    expect(screen.getAllByTitle("Overridden").length).toBe(1);
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
    fireEvent.click(screen.getByText("Core Bot"));
    expect(onSelect).toHaveBeenCalledWith(1);
  });

  it("renders both rows even when two agents transiently share a slug (duplicate id)", () => {
    // Mid-edit, toUiAgent can derive the same id from a shared slug. Keying by index keeps the two
    // rows distinct (React doesn't collide their keys) so selection/state attach to the right row.
    const dupes = [
      agent({ id: "dup", name: "First Bot", enabled: true }),
      agent({ id: "dup", name: "Second Bot", projectName: "Core Platform", enabled: false, status: "paused", running: 0 }),
    ];
    const onSelect = vi.fn();
    render(<AgentList agents={dupes} global={global} listStyle="rows" onSelect={onSelect} onToggle={() => {}} openSheet={() => {}} />);
    expect(screen.getByText("First Bot")).toBeTruthy();
    expect(screen.getByText("Second Bot")).toBeTruthy();
    // The second (duplicate-keyed) row still selects its own index, not the first.
    fireEvent.click(screen.getByText("Second Bot"));
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
