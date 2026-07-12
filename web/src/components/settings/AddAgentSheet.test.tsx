// @vitest-environment jsdom
import type * as React from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, within } from "@testing-library/react";
import type { LinearProject } from "@/lib/api";
import type { UiGlobal } from "@/lib/settings-model";
import { AddAgentSheet } from "@/components/settings/AddAgentSheet";

const projects: LinearProject[] = [
  { id: "1", name: "Infrastructure", slug: "symphony-infra-9c29", team: "INF", color: "#34d399" },
  { id: "2", name: "Core Platform", slug: "example-core-5f1a", team: "CORE", color: "#38bdf8" },
  { id: "3", name: "Harvest", slug: "acme-harvest-7b2c", team: "HARV", color: "#a78bfa" },
];

const global: UiGlobal = {
  model: "claude-opus-4-8",
  effort: "high",
  permission: "acceptEdits",
  ultracode: false,
  maxConcurrent: 3,
  maxTurns: 60,
  backoff: "exponential",
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
};

function renderSheet(over: Partial<React.ComponentProps<typeof AddAgentSheet>> = {}) {
  const onClose = vi.fn();
  const onCreate = vi.fn();
  render(
    <AddAgentSheet open onClose={onClose} onCreate={onCreate} projects={projects} usedSlugs={[]} global={global} {...over} />,
  );
  return { onClose, onCreate };
}

afterEach(cleanup);

describe("AddAgentSheet", () => {
  it("renders nothing when closed", () => {
    const { container } = render(
      <AddAgentSheet open={false} onClose={() => {}} onCreate={() => {}} projects={projects} usedSlugs={[]} global={global} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("excludes already-configured projects from the picker (unique-slug rule)", () => {
    renderSheet({ usedSlugs: ["example-core-5f1a"] });
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), { target: { value: "" } });
    expect(screen.queryByText("Core Platform")).toBeNull(); // already used → not offered
    expect(screen.getByText("Infrastructure")).toBeTruthy();
  });

  it("filters the picker by name, slug or team", () => {
    renderSheet();
    const search = screen.getByPlaceholderText("Search your Linear projects…");
    fireEvent.change(search, { target: { value: "core" } });
    expect(screen.getByText("Core Platform")).toBeTruthy();
    expect(screen.queryByText("Infrastructure")).toBeNull();
    // team match
    fireEvent.change(search, { target: { value: "HARV" } });
    expect(screen.getByText("Harvest")).toBeTruthy();
    expect(screen.queryByText("Core Platform")).toBeNull();
  });

  it("collapses to a summary once a project is picked, and inherits-from-global preview shows defaults", () => {
    renderSheet();
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), { target: { value: "Infra" } });
    fireEvent.click(screen.getByText("Infrastructure"));
    // summary button (collapsed) shows team + name; search input is gone
    expect(screen.queryByPlaceholderText("Search your Linear projects…")).toBeNull();
    expect(screen.getByText("claude-opus-4-8")).toBeTruthy();
    // the inherits preview advertises a fresh agent's per-agent cap as 1 (NEW_AGENT_CAP), not the
    // global max (3) — matching the cap a created agent actually gets.
    const capRow = screen.getByText("Per-agent cap").closest("div") as HTMLElement;
    expect(within(capRow).getByText("1")).toBeTruthy();
  });

  it("gates Create on project set AND repo length > 4", () => {
    const { onCreate } = renderSheet();
    const create = () => screen.getByRole("button", { name: /Create agent/ }) as HTMLButtonElement;
    expect(create().disabled).toBe(true);
    // pick a project
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), { target: { value: "Infra" } });
    fireEvent.click(screen.getByText("Infrastructure"));
    expect(create().disabled).toBe(true); // repo still empty
    // too-short repo
    fireEvent.change(screen.getByPlaceholderText("git@github.com:org/repo.git"), { target: { value: "abcd" } });
    expect(create().disabled).toBe(true);
    // valid repo
    fireEvent.change(screen.getByPlaceholderText("git@github.com:org/repo.git"), { target: { value: "git@github.com:o/r.git" } });
    expect(create().disabled).toBe(false);
    fireEvent.click(create());
    expect(onCreate).toHaveBeenCalledTimes(1);
    expect(onCreate.mock.calls[0][0].slug).toBe("symphony-infra-9c29");
    expect(onCreate.mock.calls[0][1]).toBe("git@github.com:o/r.git");
  });

  it("closes on overlay click and Escape", () => {
    const { onClose } = renderSheet();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
    onClose.mockClear();
    fireEvent.click(screen.getByTestId("sheet-overlay"));
    expect(onClose).toHaveBeenCalled();
  });
});
