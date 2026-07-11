// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { StatusDTO } from "@/lib/bindings";
import { Titlebar } from "@/components/shell/Titlebar";
import { bridgeHealth } from "@/components/shell/health";
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
  toggleMaximise: vi.fn(),
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
  toggleMaximiseWindow: h.toggleMaximise,
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
    repo: "git@github.com:makewhatis/symphony.git",
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
      email: "david@makewhat.is",
      token: "lin_api_••••3f2a",
    })),
    fetchLinearProjects: vi.fn(async () => []),
    fetchProjectStatuses: vi.fn(async () => []),
  };
});

// Imported after the mocks so they take effect.
import { AppShell } from "@/components/shell/AppShell";

afterEach(cleanup);

const running: StatusDTO = {
  state: "running",
  pid: 1,
  restarts: 0,
  last_err: "",
  url: "",
  healthy: true,
  agent_count: 0,
  configured: true,
};

// Minimal props for the consolidated icon-only titlebar.
const tbProps = {
  status: running,
  health: "healthy" as const,
  onStart: () => {},
  onStop: () => {},
  onRestart: () => {},
  onToggleSettings: () => {},
};

describe("Titlebar", () => {
  it("shows health + activity and gates the icon lifecycle buttons", () => {
    const onRestart = vi.fn();
    render(<Titlebar {...tbProps} pollMs={2000} onRestart={onRestart} />);
    expect(screen.getByText("Healthy")).toBeTruthy();
    expect(screen.getByText("idle")).toBeTruthy(); // running + 0 agents → "idle"
    expect(screen.getByText("poll 2s")).toBeTruthy();
    // running ⇒ Start dim/disabled, Stop + Restart actionable (queried by their aria-label)
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Restart" }));
    expect(onRestart).toHaveBeenCalledOnce();
  });

  it("toggles Settings via the gear and reflects the active state", () => {
    const onToggleSettings = vi.fn();
    render(<Titlebar {...tbProps} settingsActive onToggleSettings={onToggleSettings} />);
    const gear = screen.getByRole("button", { name: "Settings" });
    expect(gear.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(gear);
    expect(onToggleSettings).toHaveBeenCalledOnce();
  });

  it("shows a hover tooltip naming the control", () => {
    render(<Titlebar {...tbProps} />);
    // The tooltip bubble is aria-hidden (the button is already named via aria-label), so it lives
    // outside the a11y tree — query it directly by its role attribute.
    expect(document.querySelector('[role="tooltip"]')).toBeNull();
    fireEvent.mouseEnter(screen.getByRole("button", { name: "Settings" }));
    expect(document.querySelector('[role="tooltip"]')?.textContent).toBe("Settings");
  });

  it("zooms the window on a title-bar double-click, but ignores double-clicks on a control", () => {
    h.toggleMaximise.mockClear();
    render(<Titlebar {...tbProps} />);
    // double-click the bar (via the wordmark) → toggle maximise
    fireEvent.doubleClick(screen.getByText("Symphony"));
    expect(h.toggleMaximise).toHaveBeenCalledOnce();
    // double-click a lifecycle control → no zoom (guarded so the bar action doesn't hijack buttons)
    h.toggleMaximise.mockClear();
    fireEvent.doubleClick(screen.getByRole("button", { name: "Restart" }));
    expect(h.toggleMaximise).not.toHaveBeenCalled();
  });

  it("renders distinct pills for the split stopped lifecycle states", () => {
    // not-configured (first run): a neutral "Not configured" pill, not "Offline".
    const { rerender } = render(
      <Titlebar
        {...tbProps}
        status={{ ...running, state: "stopped", healthy: false, configured: false }}
        health="not-configured"
      />,
    );
    expect(screen.getByText("Not configured")).toBeTruthy();
    expect(screen.queryByText("Offline")).toBeNull();

    // stopped + last_err (crashed daemon): an error-tone "Stopped — error" pill carrying --red.
    rerender(
      <Titlebar
        {...tbProps}
        status={{ ...running, state: "stopped", healthy: false, last_err: "boom" }}
        health="error"
      />,
    );
    const errPill = screen.getByText("Stopped — error");
    expect(errPill).toBeTruthy();
    expect((errPill.closest("span") as HTMLElement).style.color).toBe("var(--red)");

    // plain stopped: still the neutral "Offline" pill.
    rerender(
      <Titlebar {...tbProps} status={{ ...running, state: "stopped", healthy: false }} health="offline" />,
    );
    expect(screen.getByText("Offline")).toBeTruthy();
    expect(screen.queryByText("Stopped — error")).toBeNull();
  });

  it("renders decorative traffic lights in the browser but not under native chrome", () => {
    const { container, rerender } = render(<Titlebar {...tbProps} status={null} />);
    const dots = (root: HTMLElement) =>
      Array.from(root.querySelectorAll("span")).filter((s) => /rgb\(255, 95, 87\)/.test(s.style.background));
    expect(dots(container).length).toBe(1);
    rerender(<Titlebar {...tbProps} status={null} nativeChrome />);
    expect(dots(container).length).toBe(0);
  });
});

describe("bridgeHealth", () => {
  const base: StatusDTO = {
    state: "running",
    pid: 1,
    restarts: 0,
    last_err: "",
    url: "",
    healthy: true,
    agent_count: 0,
    configured: true,
  };
  it("maps the Wails supervisor status onto a health state", () => {
    expect(bridgeHealth(null)).toBe("connecting");
    expect(bridgeHealth({ ...base, healthy: true })).toBe("healthy");
    // Supervisor still coming up: "starting", or "running" before the first healthy probe — both
    // read "Connecting…", never "Offline" (a failed-launch signal) or "Degraded".
    expect(bridgeHealth({ ...base, healthy: false, state: "starting" })).toBe("connecting");
    expect(bridgeHealth({ ...base, healthy: false, state: "running" })).toBe("connecting");
    // A clean stop is "offline"…
    expect(bridgeHealth({ ...base, healthy: false, state: "stopped" })).toBe("offline");
    // …but the stopped phase is split honestly, mirroring viewForStatus: a first run with no
    // WORKFLOW.md is "not-configured" (not the same Offline a deliberately-stopped daemon shows),
    // and a stop carrying a last_err is "error" (the daemon crashed / failed to launch).
    expect(bridgeHealth({ ...base, healthy: false, state: "stopped", configured: false })).toBe(
      "not-configured",
    );
    expect(bridgeHealth({ ...base, healthy: false, state: "stopped", last_err: "boom" })).toBe("error");
    // not-configured wins over last_err: a never-configured daemon hasn't run, so "Not configured"
    // is the truer story than a stale error.
    expect(
      bridgeHealth({ ...base, healthy: false, state: "stopped", configured: false, last_err: "boom" }),
    ).toBe("not-configured");
  });
});

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

  it("wires the supervisor status + daemon health into the top bar", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Healthy")).toBeTruthy());
    expect(screen.getByText("idle")).toBeTruthy();
    expect(screen.getByText("poll 2s")).toBeTruthy();
  });

  it("toggles Settings from the titlebar gear (Runs is the main area)", async () => {
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

  it("returns to Runs via the Settings 'Back to Runs' link", async () => {
    renderShell();
    await waitFor(() => expect(screen.getByText("Jobs")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { name: "General" })).toBeTruthy();
    // the explicit, discoverable way out (vs. re-clicking the gear)
    fireEvent.click(screen.getByRole("button", { name: "Back to Runs" }));
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

      // Drive the wizard's create step to the failure via the manual fallback (a bare slugId
      // normalizes to itself, so writeInitialConfig is reached — and rejects per the mock).
      fireEvent.click(await screen.findByRole("button", { name: "Enter it manually" }));
      const slug = await screen.findByLabelText("Project slug");
      fireEvent.change(slug, { target: { value: "872639248532" } });
      fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

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

      // The wizard is gone (dashboard mounted), but the lifted banner survives the unmount.
      await waitFor(() => expect(screen.queryByLabelText("Project slug")).toBeNull());
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
