// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { DaemonVersion, StateResponse, TeamsOverview } from "@/lib/api";

// The acceptance criterion this file exists for: **Teams off ⇒ the dashboard is byte-for-byte
// today's** — no chip, no panel, and NO fetch against /api/v1/teams* of any kind. The gate is one
// field on GET /api/v1/version, the request the shell already makes at mount, so learning that
// Teams is off costs no request of its own and touches no Teams route.

const h = vi.hoisted(() => ({
  fetchVersion: vi.fn(),
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
  fetchTeamsConfig: vi.fn(),
}));

vi.mock("@/lib/bindings", () => ({
  hasBridge: () => false,
  getStatus: vi.fn(async () => null),
  appVersion: vi.fn(async () => null),
  startDaemon: vi.fn(async () => {}),
  stopDaemon: vi.fn(async () => {}),
  restartDaemon: vi.fn(async () => {}),
  openExternal: vi.fn(),
  probeTools: vi.fn(async () => []),
  setToolOverride: vi.fn(async () => {}),
  installTool: vi.fn(async () => {}),
  pickDirectory: vi.fn(async () => ""),
  setLinearToken: vi.fn(async () => {}),
  clearLinearToken: vi.fn(async () => {}),
  credentialStatus: vi.fn(async () => ({ has_token: true })),
  writeInitialConfig: vi.fn(async () => {}),
  listLinearProjects: vi.fn(async () => []),
  onNavigate: () => () => {},
  onShuttingDown: () => () => {},
  checkForUpdate: vi.fn(async () => null),
  downloadUpdate: vi.fn(async () => {}),
  installUpdate: vi.fn(async () => null),
  activeRunCount: vi.fn(async () => 0),
  onUpdateAvailable: () => () => {},
  onUpdateDownloadProgress: () => () => {},
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchVersion: h.fetchVersion,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchTeamsRoom: h.fetchTeamsRoom,
    fetchTeamsRecall: h.fetchTeamsRecall,
    fetchTeamsConfig: h.fetchTeamsConfig,
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
        }) as unknown as StateResponse,
    ),
    fetchHistory: vi.fn(async () => ({ runs: [], next_offset: null })),
    fetchIssueRuns: vi.fn(async () => ({ issues: [], next_offset: null })),
    fetchDaySummary: vi.fn(async () => ({ runs: 0, completed: 0, failed: 0, total_tokens: 0 })),
    fetchTypedConfig: vi.fn(async () => ({ config: {}, prompt_body: "", global: null, projects: [] })),
    fetchLinearIdentity: vi.fn(async () => ({ connected: false, name: "", display_name: "", email: "", token: "", workspace_url_key: "" })),
    fetchLinearProjects: vi.fn(async () => []),
    fetchProjectStatuses: vi.fn(async () => []),
  };
});

import { AppShell } from "@/components/shell/AppShell";
import { VERSION_RETRY_MS } from "@/hooks/useTeams";

const version = (teams: boolean | undefined): DaemonVersion => ({
  version: "v0.3.4",
  commit: "abc",
  built_at: "2026-08-30T00:00:00Z",
  ...(teams === undefined ? {} : { teams_enabled: teams }),
});

const overview: TeamsOverview = {
  enabled: true,
  manager_mode: "labels",
  default_identity: "",
  backend: "local",
  roster: [
    { name: "alice", profile: "swe", labels: ["rust"], bank: "agent-alice", max_concurrent: 0, live_runs: 2, tickets: ["MT-1", "MT-9"] },
    { name: "bob", profile: "reviewer", labels: [], bank: "agent-bob", max_concurrent: 0, live_runs: 0, tickets: [] },
  ],
};

function renderShell() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AppShell />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  h.fetchTeamsOverview.mockResolvedValue(overview);
  h.fetchTeamsRoom.mockResolvedValue({ messages: [], skipped: [] });
  h.fetchTeamsRecall.mockResolvedValue({ identity: "alice", facts: [], skipped: [] });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Teams off", () => {
  it("shows no chip and never touches a Teams route", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    renderShell();
    await waitFor(() => expect(h.fetchVersion).toHaveBeenCalled());
    expect(screen.queryByText(/^Teams: /)).toBeNull();
    expect(h.fetchTeamsOverview).not.toHaveBeenCalled();
    expect(h.fetchTeamsRoom).not.toHaveBeenCalled();
    expect(h.fetchTeamsRecall).not.toHaveBeenCalled();
    // Nor the config route: that one is reachable only by opening Settings → Teams.
    expect(h.fetchTeamsConfig).not.toHaveBeenCalled();
  });

  // A daemon older than STUDIO-652 omits the field entirely; that reads as off, not as a crash.
  it("treats a daemon too old to report the field as off", async () => {
    h.fetchVersion.mockResolvedValue(version(undefined));
    renderShell();
    await waitFor(() => expect(h.fetchVersion).toHaveBeenCalled());
    expect(screen.queryByText(/^Teams: /)).toBeNull();
    expect(h.fetchTeamsOverview).not.toHaveBeenCalled();
  });

  // An unreachable daemon must not white-screen the shell or start guessing. Since STUDIO-665 the
  // gate keeps asking the *version* route while it has never had an answer, so "off" here means off
  // *transiently* — and while that lasts it must stay off and touch no Teams route at all.
  it("treats an unreachable version endpoint as off while it keeps retrying", async () => {
    h.fetchVersion.mockRejectedValue(new Error("connection refused"));
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderShell();
      await waitFor(() => expect(h.fetchVersion).toHaveBeenCalled());
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3 * VERSION_RETRY_MS + 100);
      });
      // Still asking — that is the whole point of the fix.
      expect(h.fetchVersion.mock.calls.length).toBeGreaterThan(1);
      // ...and still off, with no Teams route touched during the retry window.
      expect(screen.queryByText(/^Teams: /)).toBeNull();
      expect(h.fetchTeamsOverview).not.toHaveBeenCalled();
      expect(h.fetchTeamsRoom).not.toHaveBeenCalled();
      expect(h.fetchTeamsRecall).not.toHaveBeenCalled();
      expect(h.fetchTeamsConfig).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  // Retrying is scoped to "has never answered". A real `teams_enabled: false` is an answer, so the
  // Teams-off app still makes exactly ONE version request for the whole session — the pin the
  // STUDIO-652 comment used to make about the mount-time fetch, now made about settling.
  it("stops asking once a definitive Teams-off answer has arrived", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderShell();
      await waitFor(() => expect(h.fetchVersion).toHaveBeenCalledTimes(1));
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5 * VERSION_RETRY_MS);
      });
      expect(h.fetchVersion).toHaveBeenCalledTimes(1);
      expect(screen.queryByText(/^Teams: /)).toBeNull();
      expect(h.fetchTeamsOverview).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  // A daemon too old to carry the field still ANSWERED, so the retries stop there too: an old
  // daemon is not polled forever just because it cannot say the word.
  it("stops asking a daemon too old to report the field", async () => {
    h.fetchVersion.mockResolvedValue(version(undefined));
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderShell();
      await waitFor(() => expect(h.fetchVersion).toHaveBeenCalledTimes(1));
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5 * VERSION_RETRY_MS);
      });
      expect(h.fetchVersion).toHaveBeenCalledTimes(1);
      expect(screen.queryByText(/^Teams: /)).toBeNull();
      expect(h.fetchTeamsOverview).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("Teams on", () => {
  it("shows the status chip with the teammate and live-run counts", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    renderShell();
    expect(await screen.findByRole("button", { name: "Teams: 2 teammates, 2 live" })).toBeTruthy();
  });

  it("opens the Teams panel from the chip", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    renderShell();
    fireEvent.click(await screen.findByRole("button", { name: "Teams: 2 teammates, 2 live" }));
    expect(await screen.findByRole("tabpanel", { name: "Teams" })).toBeTruthy();
    expect(await screen.findByRole("button", { name: "Show alice's memory" })).toBeTruthy();
  });
});

// The STUDIO-665 repro: in the packaged app the supervisor starts the webview and the daemon
// together, so the shell can mount and fire its version request seconds before the daemon has bound
// its port. Before this ticket that single failure latched the gate off for the whole session.
describe("Teams gate recovery from the daemon boot race", () => {
  it("shows the chip once the daemon comes up, with no reload", async () => {
    h.fetchVersion
      .mockRejectedValueOnce(new Error("connection refused"))
      .mockRejectedValueOnce(new Error("connection refused"))
      .mockResolvedValue(version(true));
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderShell();

      // Mount-time fetch fails: the gate reads as off and asks for nothing Teams-shaped.
      await waitFor(() => expect(h.fetchVersion).toHaveBeenCalled());
      expect(screen.queryByText(/^Teams: /)).toBeNull();
      expect(h.fetchTeamsOverview).not.toHaveBeenCalled();

      // Two refusals in, the daemon binds its port and the third attempt lands. The chip appears
      // on its own — nobody reloaded the app.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(2 * VERSION_RETRY_MS + 100);
      });
      expect(await screen.findByRole("button", { name: "Teams: 2 teammates, 2 live" })).toBeTruthy();
      // It really did have to ask again to get there: the mock only resolves on the third call, so
      // the mount-time request alone could never have produced that chip.
      expect(h.fetchVersion.mock.calls.length).toBeGreaterThanOrEqual(3);

      // And having succeeded, the version route goes quiet for the rest of the session.
      const settled = h.fetchVersion.mock.calls.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5 * VERSION_RETRY_MS);
      });
      expect(h.fetchVersion).toHaveBeenCalledTimes(settled);
    } finally {
      vi.useRealTimers();
    }
  });
});
