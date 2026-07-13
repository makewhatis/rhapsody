// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { StatusDTO } from "@/lib/bindings";
import { ToastProvider, useToast } from "@/components/shell/Toast";

// --- Mock the Wails bridge + daemon HTTP API for the AppShell integration test ---
const h = vi.hoisted(() => ({
  status: {
    state: "running",
    pid: 1,
    restarts: 0,
    last_err: "",
    url: "http://127.0.0.1:8799",
    healthy: true,
    agent_count: 0,
    configured: true,
  } as StatusDTO,
  startDaemon: vi.fn(async () => {}),
  stopDaemon: vi.fn(async () => {}),
  restartDaemon: vi.fn(async () => {}),
  openExternal: vi.fn(),
  navHandlers: [] as ((view: string) => void)[],
  credentialStatus: vi.fn(async () => ({ has_token: true })),
  writeInitialConfig: vi.fn(async (_slug: string) => {}),
  shutdownHandlers: [] as (() => void)[],
}));

vi.mock("@/lib/bindings", () => ({
  hasBridge: () => false,
  getStatus: vi.fn(async () => h.status),
  appVersion: vi.fn(async () => null),
  startDaemon: h.startDaemon,
  stopDaemon: h.stopDaemon,
  restartDaemon: h.restartDaemon,
  openExternal: h.openExternal,
  // The Settings tabs reach app-side capabilities through these bindings; stub them so mounting
  // the General/Projects/Tools panels under the shell doesn't hit a real (absent) Wails bridge.
  probeTools: vi.fn(async () => []),
  setToolOverride: vi.fn(async () => {}),
  installTool: vi.fn(async () => {}),
  pickDirectory: vi.fn(async () => ""),
  setLinearToken: vi.fn(async () => {}),
  clearLinearToken: vi.fn(async () => {}),
  // The first-run wizard (mounted when status.configured is false) reaches these; route them at
  // the hoisted handles so a test can drive a partial-write failure.
  credentialStatus: () => h.credentialStatus(),
  writeInitialConfig: (s: string) => h.writeInitialConfig(s),
  // The project step fetches real Linear projects (INF-277); stub it so the wizard mounts the
  // picker (and its "Enter it manually" fallback) without a Wails bridge.
  listLinearProjects: vi.fn(async () => [
    { id: "1", name: "Symphony App", slug: "872639248532", team: "FND", color: "#10b981" },
  ]),
  // Capture tray:navigate subscribers so a test can fire the event; mirror the real
  // contract by returning an unsubscribe that drops the handler.
  onNavigate: (cb: (view: string) => void) => {
    h.navHandlers.push(cb);
    return () => {
      h.navHandlers = h.navHandlers.filter((f) => f !== cb);
    };
  },
  onShuttingDown: (cb: () => void) => {
    h.shutdownHandlers.push(cb);
    return () => {
      h.shutdownHandlers = h.shutdownHandlers.filter((f) => f !== cb);
    };
  },
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  const global: import("@/lib/api").GlobalConfigDTO = {
    tracker: { kind: "linear", endpoint: "https://api.linear.app/graphql", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: { backend: "claude", max_concurrent_agents: 8, max_turns: 20, max_retry_backoff_ms: 300000 },
    claude: { command: "claude", model: "claude-sonnet-4-6", effort: "high", permission_mode: "acceptEdits", billing_guard: true, ultracode: false, turn_timeout_ms: 120000, read_timeout_ms: 0, stall_timeout_ms: 0, mcp_config: "", extra_args: [] },
    workspace: { root: "/ws" },
    storage: { path: "/db", retention_days: 30 },
    otel: { enabled: false, endpoint: "", protocol: "grpc", service_name: "symphony", insecure: false },
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
  return {
    ...actual,
    fetchState: vi.fn(
      async () =>
        ({
          status: "ok",
          poll_interval_ms: 2000,
          running: [],
          retrying: [],
          codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
          rate_limits: [],
          blocked: [],
        }) satisfies import("@/lib/api").StateResponse,
    ),
    // The mounted Runs view (INF-227) fetches history; stub it so the merged jobs list renders empty.
    fetchHistory: vi.fn(async () => ({ runs: [], next_offset: null })),
    postRefresh: vi.fn(async () => {}),
    fetchTypedConfig: vi.fn(async () => ({ config: {}, prompt_body: "", global, projects: [] })),
    fetchLinearIdentity: vi.fn(async () => ({
      connected: true,
      name: "David Johansen",
      display_name: "David",
      email: "david@example.com",
      token: "lin_api_••••3f2a",
    })),
    fetchLinearProjects: vi.fn(async () => []),
    fetchProjectStatuses: vi.fn(async () => []),
  };
});

// Imported after the mocks so they take effect.
import { AppShell } from "@/components/shell/AppShell";

afterEach(cleanup);

describe("Toast", () => {
  function Consumer() {
    const { toast } = useToast();
    return (
      <button type="button" onClick={() => toast("Settings saved", "done")}>
        fire
      </button>
    );
  }

  it("shows a toast and auto-dismisses after the duration", () => {
    vi.useFakeTimers();
    try {
      render(
        <ToastProvider duration={3400}>
          <Consumer />
        </ToastProvider>,
      );
      fireEvent.click(screen.getByText("fire"));
      expect(screen.getByText("Settings saved")).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(3500);
      });
      expect(screen.queryByText("Settings saved")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("AppShell (integration)", () => {
  beforeEach(() => {
    h.restartDaemon.mockClear();
    h.navHandlers = [];
    // The lifted-error test mutates these; reset so the configured-by-default cases are unaffected.
    h.status = { ...h.status, configured: true };
    h.credentialStatus.mockResolvedValue({ has_token: true });
    h.writeInitialConfig.mockResolvedValue(undefined);
  });

  function renderShell() {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <AppShell />
      </QueryClientProvider>,
    );
  }

  it("wires the supervisor status into the toolbar's conductor cluster", async () => {
    renderShell();
    // running + 0 agents (via the HTTP /api/v1/state path, no bridge) reads as Idle, with the mono
    // "daemon healthy · poll 2s" suffix.
    await waitFor(() => expect(screen.getByText("Idle — watching for tickets")).toBeTruthy());
    expect(screen.getByText("daemon healthy · poll 2s")).toBeTruthy();
  });

  it("toggles Settings from the toolbar gear (Runs is the main area)", async () => {
    renderShell();
    // default view is Runs (the re-skinned Runs view, INF-227)
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    // the gear opens Settings (General)
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { name: "General" })).toBeTruthy();
    // clicking it again toggles back to Runs
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByText("Jobs")).toBeTruthy();
  });

  it("jumps to the Tools settings tab via the toolbar Tools shortcut", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Tools" }));
    // the Tools tab's own heading confirms the rail switched straight to it
    await waitFor(() => expect(screen.getByRole("heading", { name: "Tools" })).toBeTruthy());
  });

  it("returns to Runs via the Settings rail '← Jobs' link", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { name: "General" })).toBeTruthy();
    // the explicit, discoverable way out at the top of the rail (vs. re-clicking the gear)
    fireEvent.click(screen.getByRole("button", { name: "Jobs" }));
    expect(screen.getByText("Jobs")).toBeTruthy();
  });

  it("fires the bound restart and toasts on success", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Restart" }));
    expect(h.restartDaemon).toHaveBeenCalledOnce();
    await waitFor(() => expect(screen.getByText("Daemon restarted")).toBeTruthy());
  });

  it("follows tray navigate events to the matching view", async () => {
    renderShell();
    // default view is Runs
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    // the tray's "Settings…" emits "settings" → Settings (General)
    act(() => h.navHandlers.forEach((fn) => fn("settings")));
    expect(screen.getByRole("heading", { name: "General" })).toBeTruthy();
    // the tray's "Open Dashboard" emits "dashboard" → Runs
    act(() => h.navHandlers.forEach((fn) => fn("dashboard")));
    expect(screen.getByText("Jobs")).toBeTruthy(); // the re-skinned Runs view (INF-227) is mounted
  });

  it("keeps the lifted onboarding error banner after the poll flips configured and unmounts the wizard", async () => {
    // Fresh install: not configured yet → the shell mounts the wizard, not the dashboard.
    h.status = { ...h.status, configured: false };
    h.credentialStatus.mockResolvedValue({ has_token: true });
    // WriteInitialConfig wrote WORKFLOW.md but the daemon-start leg failed (partial write).
    h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
    try {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      renderShell();

      // Drive the wizard to its failing final step: pick the project (step 2), continue to the
      // sound check (step 3), then "Start playing" reaches writeInitialConfig — which rejects.
      fireEvent.click(await screen.findByRole("radio", { name: "Symphony App" }));
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));
      fireEvent.click(await screen.findByRole("button", { name: "Start playing" }));

      // The lifted banner shows the partial-write message (in addition to the wizard's inline alert).
      await waitFor(() =>
        expect(screen.getAllByText(/config saved, but the daemon could not start/).length).toBeGreaterThan(0),
      );

      // Now the daemon reports configured: true (WORKFLOW.md is on disk). The next ~2s poll flips
      // notConfigured false and unmounts the wizard — exactly the regression from PR #1893.
      h.status = { ...h.status, configured: true };
      await act(async () => {
        await vi.advanceTimersByTimeAsync(2100);
      });

      // The wizard is gone (dashboard mounted — its step marker no longer renders), but the lifted
      // banner survives the unmount.
      await waitFor(() => expect(screen.queryByText(/STEP 3 OF 3/)).toBeNull());
      expect(screen.getByText(/config saved, but the daemon could not start/)).toBeTruthy();

      // The banner is dismissible.
      fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
      expect(screen.queryByText(/config saved, but the daemon could not start/)).toBeNull();
    } finally {
      vi.useRealTimers();
      h.status = { ...h.status, configured: true };
      h.writeInitialConfig.mockReset();
    }
  });

  it("shows the Shutting down… overlay when the app begins quitting", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    expect(screen.queryByText("Shutting down…")).toBeNull();
    // Go emits app:shutting-down once the daemon stop starts (off the main thread).
    act(() => h.shutdownHandlers.forEach((fn) => fn()));
    expect(screen.getByText("Shutting down…")).toBeTruthy();
  });

  it("renders a labelled tabpanel for the active view", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    // Runs view: the panel labels itself "Runs"
    let panel = document.getElementById("shell-top-panel");
    expect(panel?.getAttribute("role")).toBe("tabpanel");
    expect(panel?.getAttribute("aria-label")).toBe("Runs");
    // open Settings via the gear and assert the panel relabels + the nested settings rail wiring
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "General" })).toBeTruthy());
    panel = document.getElementById("shell-top-panel");
    expect(panel?.getAttribute("aria-label")).toBe("Settings");
    expect(
      screen.getByRole("tab", { name: "General" }).getAttribute("aria-controls"),
    ).toBe("shell-settings-panel");
  });
});
