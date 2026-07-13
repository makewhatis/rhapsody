// @vitest-environment jsdom
import * as React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { GlobalConfigDTO, ProjectConfigDTO, TypedConfigResponse } from "@/lib/api";
import { fetchTypedConfig } from "@/lib/api";
import type { ToolResult } from "@/lib/bindings";

function makeGlobal(): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "e", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: { backend: "claude", max_concurrent_agents: 8, max_turns: 20, max_retry_backoff_ms: 300000 },
    claude: { command: "claude", model: "claude-sonnet-4-6", effort: "high", permission_mode: "acceptEdits", billing_guard: true, ultracode: false, turn_timeout_ms: 120000, read_timeout_ms: 0, stall_timeout_ms: 0, mcp_config: "", extra_args: [] },
    workspace: { root: "/ws" },
    storage: { path: "/db", retention_days: 30 },
    otel: { enabled: false, endpoint: "", protocol: "grpc", service_name: "s", insecure: false },
    mcp: { enabled: true, allow_send_message: true, allow_stop: false, allow_resume: false },
    server: { port: 4317 },
    logging: { dir: "/logs" },
    repo: "git@github.com:example/demo-repo.git",
    active_states: ["Todo", "In Progress"],
    terminal_states: ["Done"],
    canceled_states: ["Cancelled"],
    review_states: [],
    review_promote_state: "In Progress",
    summon_token: "@symphony",
    github_summons: false,
    milestone: "",
    labels: [],
    prompt: "p",
    prompt_file: "",
    git_flow: "",
    workspace_mode: "",
    dependency_mode: "disabled",
    claim_mode: "assignee",
  };
}

const infra: ProjectConfigDTO = {
  name: "Infra Bot",
  slugs: ["infra-9c29"],
  repo: "git@github.com:example/demo-repo.git",
  enabled: true,
  overrides: {},
};

const h = vi.hoisted(() => ({
  saveTypedConfig: vi.fn(),
  setLinearToken: vi.fn(async () => {}),
  clearLinearToken: vi.fn(async () => {}),
  probeTools: vi.fn(async (): Promise<ToolResult[]> => []),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchTypedConfig: vi.fn(async (): Promise<TypedConfigResponse> => ({
      config: {},
      prompt_body: "",
      global: makeGlobal(),
      projects: [structuredClone(infra)],
    })),
    fetchLinearIdentity: vi.fn(async () => ({ connected: true, name: "David", display_name: "David", email: "d@x.io", token: "lin_***" })),
    fetchLinearProjects: vi.fn(async () => [
      { id: "1", name: "Infrastructure", slug: "infra-9c29", team: "INF", color: "#34d399" },
      { id: "2", name: "Core Platform", slug: "core-5f1a", team: "CORE", color: "#38bdf8" },
    ]),
    fetchProjectStatuses: vi.fn(async () => []),
    saveTypedConfig: h.saveTypedConfig,
  };
});

vi.mock("@/lib/bindings", () => ({
  setLinearToken: h.setLinearToken,
  clearLinearToken: h.clearLinearToken,
  pickDirectory: vi.fn(async () => ""),
  appVersion: vi.fn(async () => null), // plain-browser: no build stamp (RailVersion falls back to "dev")
  probeTools: h.probeTools, // the shell mounts the preflight/doctor probe for the rail warning dot (D6)
}));

import { Settings } from "@/components/settings/Settings";
import { ToastProvider } from "@/components/shell/Toast";
import type { SettingsTabId } from "@/components/shell/placeholders";

// Comfortably past the Settings autosave debounce (600ms) — used to assert a BLOCKED draft never
// POSTs even after the window elapses.
const AUTOSAVE_WAIT_MS = 800;

// Stateful harness so clicking a rail tab actually switches the active tab (the real shell owns
// the tab state) while keeping the Settings instance — and its pending-token state — mounted.
function Harness({ initial, onBack }: { initial: SettingsTabId; onBack?: () => void }) {
  const [tab, setTab] = React.useState<SettingsTabId>(initial);
  return <Settings tab={tab} onTab={setTab} onBack={onBack ?? (() => {})} />;
}

function renderSettings(tab: SettingsTabId) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ToastProvider>
        <Harness initial={tab} />
      </ToastProvider>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

beforeEach(() => {
  // Echo the posted config so the post-save re-sync (onSuccess setQueryData) has a valid payload.
  h.saveTypedConfig.mockImplementation(async (global, projects) => ({ config: {}, prompt_body: "", global, projects }));
  // Default: a clean preflight (no CLIs / no warnings) so the rail dot stays dark unless a test opts in.
  h.probeTools.mockResolvedValue([]);
});

describe("Settings (autosave controller)", () => {
  it("reads config on mount, shows Saving… on edit, and autosaves after the debounce", async () => {
    renderSettings("general");
    await screen.findByText("Linear connection");
    // pristine: the settled indicator, no Save button anywhere
    expect(screen.getByText("All changes saved")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    // edit max-concurrent (first Stepper) 8 -> 9: the indicator flips to Saving… immediately…
    fireEvent.click(screen.getAllByLabelText("Increment")[0]);
    expect(screen.getByText("Saving…")).toBeTruthy();
    expect(h.saveTypedConfig).not.toHaveBeenCalled(); // …but the POST is debounced
    // …then the debounce fires exactly one POST and the indicator settles.
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][0].agent.max_concurrent_agents).toBe(9);
    await screen.findByText("All changes saved");
  });

  it("writes a pasted token to the keychain on autosave and never into the config payload", async () => {
    renderSettings("general");
    await screen.findByText("Linear connection");
    fireEvent.change(screen.getByPlaceholderText("Paste lin_api_…"), { target: { value: "lin_api_secret123" } });
    await waitFor(() => expect(h.setLinearToken).toHaveBeenCalledWith("lin_api_secret123"), { timeout: 2000 });
    expect(h.saveTypedConfig).toHaveBeenCalledTimes(1);
    // the config payload carries no raw token anywhere
    expect(JSON.stringify(h.saveTypedConfig.mock.calls[0])).not.toContain("lin_api_secret123");
  });

  it("autosaves an enable/pause toggle as a draft edit", async () => {
    renderSettings("projects");
    await screen.findByText(/configured/);
    fireEvent.click(screen.getByRole("switch", { name: "Enable Infra Bot" }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][1][0].enabled).toBe(false);
  });

  it("appends an agent from the sheet and autosaves it", async () => {
    renderSettings("projects");
    await screen.findByText(/configured/);
    fireEvent.click(screen.getByRole("button", { name: /Add agent/ }));
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), { target: { value: "Core" } });
    fireEvent.click(screen.getByText("Core Platform"));
    fireEvent.change(screen.getByPlaceholderText("git@github.com:org/repo.git"), {
      target: { value: "git@github.com:example/core.git" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Create agent/ }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    const postedProjects = h.saveTypedConfig.mock.calls[0][1];
    expect(postedProjects).toHaveLength(2);
    expect(postedProjects[1].slugs).toEqual(["core-5f1a"]);
  });

  it("blocks autosave (and surfaces the error) when a review-promote state isn't one of its active states", async () => {
    const bad = makeGlobal();
    bad.review_states = ["In Review"]; // review enabled, so the promote check applies
    bad.review_promote_state = "Shipped"; // ∉ infra's active states (Todo, In Progress)
    vi.mocked(fetchTypedConfig).mockResolvedValueOnce({
      config: {},
      prompt_body: "",
      global: bad,
      projects: [structuredClone(infra)],
    });
    renderSettings("projects");
    await screen.findByText(/configured/);
    // make an edit so dirty=true; the block message shows and autosave must NOT fire
    fireEvent.click(screen.getByRole("switch", { name: "Enable Infra Bot" }));
    expect(screen.getByText(/Review-promote state must be one of/)).toBeTruthy();
    // wait past the debounce window — a blocked draft is never POSTed
    await new Promise((r) => setTimeout(r, AUTOSAVE_WAIT_MS));
    expect(h.saveTypedConfig).not.toHaveBeenCalled();
  });

  it("removes an agent and autosaves the config without that project", async () => {
    renderSettings("projects");
    await screen.findByText(/configured/);
    fireEvent.click(screen.getByText("example/demo-repo")); // open detail via the repo cell
    fireEvent.click(await screen.findByRole("button", { name: "Remove agent" }));
    await screen.findByText("No agents yet"); // back to the (now empty) list as a pending edit
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][1]).toHaveLength(0);
  });
});

describe("Settings rail (Tools warning dot — D6)", () => {
  it("lights the rail's Tools amber dot when the preflight/doctor reports a warning", async () => {
    // gh missing from PATH → a doctor warning, even though we're viewing the General tab.
    h.probeTools.mockResolvedValueOnce([
      { name: "gh", path: "", found: false, healthy: false, version: "", detail: "Not found on PATH" },
    ]);
    renderSettings("general");
    await screen.findByText("Linear connection");
    expect(await screen.findByRole("img", { name: "Tools — warnings" })).toBeTruthy();
  });

  it("leaves the rail's Tools dot dark when every required CLI is healthy", async () => {
    h.probeTools.mockResolvedValue([
      { name: "claude", path: "/c", found: true, healthy: true, version: "1", detail: "ok" },
      { name: "git", path: "/g", found: true, healthy: true, version: "2", detail: "ok" },
    ]);
    renderSettings("general");
    await screen.findByText("Linear connection");
    // give the probe a tick to resolve, then assert the warning dot never appears
    await new Promise((r) => setTimeout(r, 50));
    expect(screen.queryByRole("img", { name: /warnings/ })).toBeNull();
  });
});
