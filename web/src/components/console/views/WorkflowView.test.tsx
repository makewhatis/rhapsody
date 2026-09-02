// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { GlobalConfigDTO, ProjectConfigDTO, TypedConfigResponse } from "@/lib/api";
import { fetchTypedConfig } from "@/lib/api";

// The console's WORKFLOW.md editor (STUDIO-690) — the last Settings-parity gate before the
// §2.2.1 flip. The acceptance is PARITY: the editor the Settings "Workflow" row opens must lose
// no capability the shipped Podium Settings editor has, and it must save through the same
// `POST /api/v1/config` discipline. So these tests drive the real view against a mocked config
// endpoint and assert on the editing capabilities and the POST payload, not on layout.

function makeGlobal(): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "e", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: { backend: "claude", max_concurrent_agents: 8, max_turns: 20, max_retry_backoff_ms: 300000 },
    claude: {
      command: "claude",
      model: "claude-sonnet-4-6",
      effort: "high",
      permission_mode: "acceptEdits",
      billing_guard: true,
      ultracode: false,
      turn_timeout_ms: 120000,
      read_timeout_ms: 0,
      stall_timeout_ms: 0,
      mcp_config: "",
      extra_args: [],
    },
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
    capabilities: [],
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
    fetchTypedConfig: vi.fn(
      async (): Promise<TypedConfigResponse> => ({
        config: {},
        prompt_body: "",
        global: makeGlobal(),
        projects: [structuredClone(infra)],
      }),
    ),
    fetchLinearIdentity: vi.fn(async () => ({
      connected: true,
      name: "David",
      display_name: "David",
      email: "d@x.io",
      token: "lin_***",
    })),
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
  pickDirectory: vi.fn(async () => "/picked/path"),
  appVersion: vi.fn(async () => null),
}));

const { WorkflowView } = await import("./WorkflowView");

// Comfortably past the autosave debounce (600ms) — used to assert a BLOCKED draft never POSTs
// even after the window has elapsed.
const AUTOSAVE_WAIT_MS = 800;

const onNavigate = vi.fn();

function mount() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WorkflowView onNavigate={onNavigate} />
    </QueryClientProvider>,
  );
}

/** Mount and wait for the fetched WORKFLOW.md to have hydrated the form. */
async function ready() {
  mount();
  await screen.findByText("Linear connection");
}

beforeEach(() => {
  // Echo the posted config so the post-save re-sync (onSuccess setQueryData) has a valid payload.
  h.saveTypedConfig.mockImplementation(async (global, projects) => ({
    config: {},
    prompt_body: "",
    global,
    projects,
  }));
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("the console's WORKFLOW.md editor (STUDIO-690)", () => {
  it("views the daemon's current WORKFLOW.md", async () => {
    await ready();
    // Global defaults are on screen, loaded from GET /api/v1/config.
    expect(screen.getByDisplayValue("/ws")).toBeTruthy();
    expect(screen.getByDisplayValue("8")).toBeTruthy();
  });

  it("edits a global default and saves it through POST /api/v1/config", async () => {
    await ready();
    // Max concurrent agents (the first Stepper) 8 -> 9.
    fireEvent.click(screen.getAllByLabelText("Increment")[0]);
    // The POST is debounced, exactly as the Podium editor debounces it.
    expect(h.saveTypedConfig).not.toHaveBeenCalled();
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][0].agent.max_concurrent_agents).toBe(9);
    await screen.findByText("All changes saved");
  });

  it("edits the per-agent half of WORKFLOW.md too — no capability of the Podium editor is lost", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    await screen.findByText(/configured/);
    fireEvent.click(screen.getByRole("switch", { name: "Enable Infra Bot" }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][1][0].enabled).toBe(false);
  });

  it("adds an agent from the sheet", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Projects" }));
    await screen.findByText(/configured/);
    fireEvent.click(screen.getByRole("button", { name: /Add agent/ }));
    fireEvent.change(screen.getByPlaceholderText("Search your Linear projects…"), {
      target: { value: "Core" },
    });
    fireEvent.click(screen.getByText("Core Platform"));
    fireEvent.change(screen.getByPlaceholderText("git@github.com:org/repo.git"), {
      target: { value: "git@github.com:example/core.git" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Create agent/ }));
    await waitFor(() => expect(h.saveTypedConfig).toHaveBeenCalledTimes(1), { timeout: 2000 });
    expect(h.saveTypedConfig.mock.calls[0][1]).toHaveLength(2);
  });

  it("writes a pasted Linear token to the keychain, never into the config payload", async () => {
    await ready();
    fireEvent.change(screen.getByPlaceholderText("Paste lin_api_…"), {
      target: { value: "lin_api_secret123" },
    });
    await waitFor(() => expect(h.setLinearToken).toHaveBeenCalledWith("lin_api_secret123"), {
      timeout: 2000,
    });
    expect(JSON.stringify(h.saveTypedConfig.mock.calls[0])).not.toContain("lin_api_secret123");
  });

  it("holds the POST the daemon would reject, and says why", async () => {
    const bad = makeGlobal();
    bad.review_states = ["In Review"]; // review enabled, so the promote check applies
    bad.review_promote_state = "Shipped"; // ∉ infra's active states (Todo, In Progress)
    vi.mocked(fetchTypedConfig).mockResolvedValueOnce({
      config: {},
      prompt_body: "",
      global: bad,
      projects: [structuredClone(infra)],
    });
    await ready();
    fireEvent.click(screen.getAllByLabelText("Increment")[0]);
    expect(screen.getByText(/Review-promote state must be one of/)).toBeTruthy();
    await new Promise((r) => setTimeout(r, AUTOSAVE_WAIT_MS));
    expect(h.saveTypedConfig).not.toHaveBeenCalled();
  });

  it("reports a daemon that cannot serve the config rather than an empty form", async () => {
    vi.mocked(fetchTypedConfig).mockRejectedValueOnce(new Error("connection refused"));
    mount();
    expect(await screen.findByRole("note")).toBeTruthy();
    expect(screen.getByRole("note").textContent).toContain("WORKFLOW.md");
    expect(screen.queryByText("Linear connection")).toBeNull();
  });

  it("says what a save does, and claims neither a live apply nor a restart", async () => {
    await ready();
    // The daemon WATCHES WORKFLOW.md and hot-reloads it (crates/orchestrator/src/reload.rs), so
    // "restart to apply" — true of teams.yaml — would be false here. The note states the
    // verifiable contract instead: the file is rewritten atomically, or rejected and left alone.
    const lead = screen.getByRole("note").textContent ?? "";
    expect(lead).toContain("WORKFLOW.md");
    expect(lead).toMatch(/re-reads/i);
    expect(lead).not.toMatch(/restart/i);
  });

  it("returns to Settings from the breadcrumb", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(onNavigate).toHaveBeenCalledWith("settings");
  });
});

// Source contracts — the two rules this slice could break silently, checked against the source
// rather than the DOM (the `index.css.test.ts` / `Pill.test.tsx` precedent).
describe("source contracts", () => {
  const src = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

  // NOTE: the §2.2.1 "land-dark" guard that used to sit here — asserting App.tsx still rendered the
  // Podium <AppShell /> — was retired by STUDIO-687's box-6.4 flip. The root is now pinned once, in
  // ConsoleApp.test.tsx ("the flip — App.tsx renders the console").

  // The embedded Podium editor renders inside `.rh-console`, where `--accent` means the brand
  // amber rather than Podium's hover background. The embed scope hands the Podium meaning back;
  // losing that line would repaint an outlined button's hover bright amber.
  it("restores the Podium meaning of --accent inside the embed scope", () => {
    expect(src("../../../theme/console-workflow.css")).toMatch(
      /\.wfembed\s*{[^}]*--accent:\s*var\(--bg-hover\)/,
    );
  });
});
