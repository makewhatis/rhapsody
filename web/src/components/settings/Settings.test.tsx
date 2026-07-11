// @vitest-environment jsdom
import * as React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { GlobalConfigDTO, ProjectConfigDTO, TypedConfigResponse } from "@/lib/api";
import { fetchTypedConfig } from "@/lib/api";

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
}));

import { Settings } from "@/components/settings/Settings";
import { ToastProvider } from "@/components/shell/Toast";
import type { SettingsTabId } from "@/components/shell/placeholders";

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
});

describe("Settings (round-trip controller)", () => {
  it("reads config on mount, marks dirty on edit, and POSTs on Save with the reload toast", async () => {
    renderSettings("general");
    await screen.findByText("Linear connection");
    // pristine: Save disabled
    expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
    // edit max-concurrent (first Stepper) 8 -> 9
    fireEvent.click(screen.getAllByLabelText("Increment")[0]);
    expect(screen.getByText("Unsaved changes")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1));
    expect(h.saveTypedConfig.mock.calls[0][0].agent.max_concurrent_agents).toBe(9);
    expect(await screen.findByText("Settings saved")).toBeTruthy();
    expect(await screen.findByText("Daemon reloaded configuration ✓")).toBeTruthy();
  });

  it("writes a pasted token to the keychain on Save and never into the config payload", async () => {
    renderSettings("general");
    await screen.findByText("Linear connection");
    fireEvent.change(screen.getByPlaceholderText("Paste lin_api_…"), { target: { value: "lin_api_secret123" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(h.setLinearToken).toHaveBeenCalledWith("lin_api_secret123"));
    expect(h.saveTypedConfig).toHaveBeenCalledTimes(1);
    // the config payload carries no raw token anywhere
    expect(JSON.stringify(h.saveTypedConfig.mock.calls[0])).not.toContain("lin_api_secret123");
  });

  it("toggles enable as a draft edit that the Save bar persists via config", async () => {
    renderSettings("projects");
    await screen.findByText("Infra Bot");
    fireEvent.click(screen.getByRole("switch", { name: "Enable Infra Bot" }));
    // a casual toggle does NOT auto-persist — it marks the form dirty
    expect(h.saveTypedConfig).not.toHaveBeenCalled();
    expect(screen.getByText("Unsaved changes")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1));
    expect(h.saveTypedConfig.mock.calls[0][1][0].enabled).toBe(false);
  });

  it("creates an agent from the sheet (POST + 'Agent created' toast)", async () => {
    renderSettings("projects");
    await screen.findByText("Infra Bot");
    fireEvent.click(screen.getByRole("button", { name: /Add agent/ }));
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), { target: { value: "Core" } });
    fireEvent.click(screen.getByText("Core Platform"));
    fireEvent.change(screen.getByPlaceholderText("git@github.com:org/repo.git"), {
      target: { value: "git@github.com:makewhatis/core.git" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Create agent/ }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1));
    const postedProjects = h.saveTypedConfig.mock.calls[0][1];
    expect(postedProjects).toHaveLength(2);
    expect(postedProjects[1].slugs).toEqual(["core-5f1a"]);
    expect(await screen.findByText("Agent created")).toBeTruthy();
  });

  it("blocks Save when an agent's review-promote state isn't one of its active states", async () => {
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
    await screen.findByText("Infra Bot");
    // make an edit so dirty=true; Save must STILL be blocked by the validation error
    fireEvent.click(screen.getByRole("switch", { name: "Enable Infra Bot" }));
    expect(screen.getByText(/Review-promote state must be one of/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
    expect(h.saveTypedConfig).not.toHaveBeenCalled();
  });

  it("removes an agent (back to list, then Save persists config without that project)", async () => {
    renderSettings("projects");
    await screen.findByText("Infra Bot");
    fireEvent.click(screen.getByText("Infra Bot")); // open detail
    fireEvent.click(await screen.findByRole("button", { name: "Remove agent" }));
    // returns to the (now empty) list as a pending edit; Save persists the removal
    await screen.findByText("No agents yet");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1));
    expect(h.saveTypedConfig.mock.calls[0][1]).toHaveLength(0);
  });
});
