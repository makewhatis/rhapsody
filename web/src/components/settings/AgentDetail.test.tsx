// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import type { LinearProject } from "@/lib/api";
import type { UiAgent, UiGlobal } from "@/lib/settings-model";
import { AgentDetail } from "@/components/settings/AgentDetail";

const global: UiGlobal = {
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
};

const linearProjects: LinearProject[] = [
  { id: "1", name: "Docs", slug: "symphony-docs-22aa44bb66cc", team: "DOCS", color: "#f5b544" },
];

function agent(over: Partial<UiAgent> = {}): UiAgent {
  return {
    id: "docs",
    name: "Docs Bot",
    color: "#f5b544",
    projectSlug: "symphony-docs-22aa44bb66cc",
    projectName: "Docs",
    repo: "git@github.com:makewhatis/symphony-docs.git",
    repoShort: "makewhatis/symphony-docs",
    milestone: "",
    labels: [],
    enabled: false,
    status: "paused",
    running: 0,
    activeStates: ["Todo"],
    terminalStates: ["Done"],
    reviewStates: ["In Review"],
    reviewPromote: "Shipped", // ∉ activeStates on purpose (agt_docs)
    cap: 1,
    prompt: "",
    promptFile: "",
    overrides: {},
    workspaceModeRecommended: false,
    ...over,
  };
}

function renderDetail(over: Partial<UiAgent> = {}, mode: "quiet" | "chip" = "quiet") {
  const onChange = vi.fn();
  const onRemove = vi.fn();
  const onBack = vi.fn();
  render(
    <AgentDetail
      agent={agent(over)}
      global={global}
      linearProjects={linearProjects}
      mode={mode}
      onChange={onChange}
      onBack={onBack}
      onRemove={onRemove}
    />,
  );
  return { onChange, onRemove, onBack };
}

afterEach(cleanup);

describe("AgentDetail", () => {
  it("fires the review-promote validation error when promote ∉ active states (agt_docs)", () => {
    renderDetail();
    expect(screen.getByText(/“Shipped” must be one of the active states \(Todo\)\./)).toBeTruthy();
  });

  it("clears the validation once promote is an active state", () => {
    renderDetail({ activeStates: ["Todo", "In Progress"], reviewPromote: "In Progress" });
    // (the parenthesised "(states…)" distinguishes the field error from the section description)
    expect(screen.queryByText(/must be one of the active states \(/)).toBeNull();
  });

  it("Override seeds the global value (sparse map gains the key)", () => {
    const { onChange } = renderDetail();
    // quiet mode: each Claude-override row shows an "Override" button while inherited.
    const overrideButtons = screen.getAllByRole("button", { name: "Override" });
    fireEvent.click(overrideButtons[0]); // Model
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.model).toBe("claude-opus-4-8");
  });

  it("Reset deletes the key (back to inherit)", () => {
    const { onChange } = renderDetail({ overrides: { model: "claude-sonnet-4-6" } });
    fireEvent.click(screen.getByRole("button", { name: /Reset to global default/ }));
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect("model" in next.overrides).toBe(false);
  });

  it("shows the Custom prompt badge when a custom prompt is present, Inherited otherwise", () => {
    const { onChange: _a } = renderDetail({ prompt: "do the thing" });
    expect(screen.getByText("Custom")).toBeTruthy();
    cleanup();
    renderDetail({ prompt: "" });
    expect(screen.getByText("Inherited")).toBeTruthy();
  });

  it("removes the agent through the danger zone", () => {
    const { onRemove } = renderDetail();
    fireEvent.click(screen.getByRole("button", { name: "Remove agent" }));
    expect(onRemove).toHaveBeenCalledOnce();
  });

  it("flips enable as a controlled draft edit (onChange with the new flag)", () => {
    const { onChange } = renderDetail(); // fixture starts disabled
    fireEvent.click(screen.getByRole("switch", { name: "Enable agent" }));
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.enabled).toBe(true);
  });

  it("syncs an inline name edit up to the parent", () => {
    const { onChange } = renderDetail();
    fireEvent.change(screen.getByLabelText("Agent name"), { target: { value: "Docs Agent" } });
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.name).toBe("Docs Agent");
  });

  it("renders the chip-mode inherit/override pills", () => {
    renderDetail({}, "chip");
    // twelve inherited rows (model/effort/permission/git-flow/ultracode/turn-timeout/stall-timeout/
    // billing-guard/command/dependency-mode/workspace-mode/claim-mode) -> twelve "Inherited" pills
    expect(screen.getAllByRole("button", { name: "Inherited" }).length).toBe(12);
  });

  it("seeds the git_flow override from the global on Override and clears it on Reset", () => {
    // git_flow is the 4th override row (model, effort, permission, git-flow); the global default is "any".
    const { onChange } = renderDetail();
    const overrideButtons = screen.getAllByRole("button", { name: "Override" });
    fireEvent.click(overrideButtons[3]);
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.gitFlow).toBe(global.gitFlow);

    cleanup();
    const { onChange: onChange2 } = renderDetail({ overrides: { gitFlow: "graphite" } });
    fireEvent.click(screen.getByRole("button", { name: /Reset to global default/ }));
    const reset = onChange2.mock.calls.at(-1)?.[0] as UiAgent;
    expect("gitFlow" in reset.overrides).toBe(false);
  });

  it("recommends clone for a stacking project: banner only when recommended + unset, one-click accept seeds the override (INF-418)", () => {
    // Not recommended → no banner.
    renderDetail({ workspaceModeRecommended: false });
    expect(screen.queryByText("Recommended: Clone")).toBeNull();
    cleanup();

    // Recommended + the override is set already → no nag (respect the explicit choice).
    renderDetail({ workspaceModeRecommended: true, overrides: { workspaceMode: "worktree" } });
    expect(screen.queryByText("Recommended: Clone")).toBeNull();
    cleanup();

    // Recommended + unset override → banner with rationale; "Use clone" seeds the clone override
    // (dirties the form; nothing persists until the parent saves).
    const { onChange } = renderDetail({ workspaceModeRecommended: true });
    expect(screen.getByText("Recommended: Clone")).toBeTruthy();
    expect(screen.getByText(/remove the cross-ticket checkout lock/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Use clone" }));
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.workspaceMode).toBe("clone");
  });

  it("seeds the ultracode override from the global on Override and clears it on Reset", () => {
    // The global default is ultracode=false; overriding seeds that value into the sparse map.
    const { onChange } = renderDetail();
    const overrideButtons = screen.getAllByRole("button", { name: "Override" });
    fireEvent.click(overrideButtons[4]); // Ultracode is the 5th row (model, effort, permission, git-flow, ultracode)
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.ultracode).toBe(false);

    cleanup();
    const { onChange: onChange2 } = renderDetail({ overrides: { ultracode: true } });
    fireEvent.click(screen.getByRole("button", { name: /Reset to global default/ }));
    const reset = onChange2.mock.calls.at(-1)?.[0] as UiAgent;
    expect("ultracode" in reset.overrides).toBe(false);
  });

  it("seeds the turn-timeout override (minutes) from the global on Override", () => {
    const { onChange } = renderDetail();
    // Override rows in order: model, effort, permission, git-flow, ultracode, THEN turn timeout (index 5).
    const overrideButtons = screen.getAllByRole("button", { name: "Override" });
    fireEvent.click(overrideButtons[5]);
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.turnTimeoutMin).toBe(global.requestTimeoutMin);
  });

  it("the inherited prompt template declares the HANDOFF: in-review marker", () => {
    // The Settings placeholder must mirror the seeded default prompt, which now requires the agent
    // to end with `HANDOFF: in-review` — otherwise the inherited template misrepresents what fresh
    // installs seed and runs under-claim `completed` (INF-272 run-outcome taxonomy v2).
    renderDetail({ prompt: "" });
    // The "Prompt source" section is collapsed while the prompt is inherited — open it to reach the
    // inline editor whose placeholder is the inherited template.
    fireEvent.click(screen.getByText("Prompt source"));
    expect(screen.getByPlaceholderText(/HANDOFF: in-review/)).toBeTruthy();
  });

  it("shows the raw slug + a 'not found in Linear' hint when the saved slug matches no project (INF-277)", () => {
    // A pre-INF-277 free-text value that was never a real slugId: the picker matches nothing, so the
    // trigger must show the raw slug (not the empty placeholder) and a hint must flag it.
    renderDetail({ projectSlug: "symphony-app-872639248532" });
    // The Select trigger renders the saved value as its label (synthetic option), so the raw slug
    // is visible without opening the dropdown.
    expect(screen.getByText("symphony-app-872639248532")).toBeTruthy();
    expect(screen.getByText(/Not found in Linear/)).toBeTruthy();
  });

  it("shows no 'not found' hint when the saved slug matches a known project", () => {
    renderDetail(); // fixture slug is a known project
    expect(screen.queryByText(/Not found in Linear/)).toBeNull();
  });

  it("seeds the dependency_mode override from the global on Override and clears it on Reset (INF-320)", () => {
    // Override rows in DOM order: model, effort, permission, git-flow, ultracode, turn, stall,
    // billing, command, dependency-mode (index 9), workspace-mode (index 10), claim-mode (index 11).
    // The global default is "disabled"; dependency-mode stays at index 9 (workspace-mode and then
    // claim-mode are appended after it, INF-418/INF-477).
    const { onChange } = renderDetail();
    const overrideButtons = screen.getAllByRole("button", { name: "Override" });
    expect(overrideButtons).toHaveLength(12);
    fireEvent.click(overrideButtons[9]);
    const next = onChange.mock.calls.at(-1)?.[0] as UiAgent;
    expect(next.overrides.dependencyMode).toBe(global.dependencyMode);

    cleanup();
    const { onChange: onChange2 } = renderDetail({ overrides: { dependencyMode: "dag" } });
    fireEvent.click(screen.getByRole("button", { name: /Reset to global default/ }));
    const reset = onChange2.mock.calls.at(-1)?.[0] as UiAgent;
    expect("dependencyMode" in reset.overrides).toBe(false);
  });

  it("offers all three dependency-mode options once the row is overridden", () => {
    // enabled:true so the header status text isn't also "Disabled" (the Select trigger shows "Disabled").
    renderDetail({ enabled: true, overrides: { dependencyMode: "disabled" } });
    // Overridden → the Select is rendered; open it and assert all three option labels are present.
    fireEvent.click(screen.getByText("Disabled"));
    expect(screen.getByText("Graphite")).toBeTruthy();
    expect(screen.getByText("DAG")).toBeTruthy();
  });

  it("documents the dependency-mode hint (all three options, thresholds, and that disabled is the default)", () => {
    renderDetail();
    // The hint is the shared DEPENDENCY_MODE_HINT, always rendered in the row's left column. Anchor on
    // a phrase unique to it, then assert the full text covers the thresholds + trade-off + default
    // (substring matches like "parallel"/"In Review" also appear elsewhere in the editor).
    const hint = screen.getByText(/How dependent tickets are sequenced/);
    const text = hint.textContent ?? "";
    expect(text).toContain("Disabled (default)");
    expect(text).toContain("In Review");
    expect(text).toContain("merged");
    expect(text).toContain("parallel");
  });

  it("renders the billing-guard safety note", () => {
    renderDetail();
    expect(
      screen.getByText(/Forces the agent to bill your logged-in Claude subscription/),
    ).toBeTruthy();
  });
});
