// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { DaemonVersion } from "@/lib/api";

// STUDIO-681 §10, sub-ticket 2 — the app shell's acceptance boxes 2.1-2.5 and 2.12.
//
// Every one of them turns on ONE field of GET /api/v1/version: `teams_enabled`. These tests
// drive the real ConsoleApp against a mocked API rather than the gate helper directly, because
// what §2.2 promises is about the RENDERED rail — "absent from the DOM, not merely hidden" is
// not a claim a pure function can make.

const h = vi.hoisted(() => ({ fetchVersion: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchVersion: h.fetchVersion,
    fetchState: vi.fn(async () => ({
      status: "ok" as const,
      poll_interval_ms: 2000,
      running: [],
      retrying: [],
      codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
      rate_limits: [],
      blocked: [],
    })),
    fetchIssueRuns: vi.fn(async () => ({ issues: [], next_offset: null })),
    fetchLinearProjects: vi.fn(async () => []),
    fetchTeamsOverview: vi.fn(async () => ({
      enabled: true,
      manager_mode: "labels",
      default_identity: "",
      backend: "local",
      roster: [],
    })),
    fetchTeamsConfig: vi.fn(async () => ({
      path: "~/.rhapsody/teams.yaml",
      present: false,
      error: "",
      restart_required: true,
      config: {
        enabled: false,
        manager: { mode: "labels", default_identity: "", model: "", max_tokens: 0, timeout_ms: 60000 },
        memory: { backend: "local", path: "", endpoint: "", api_key: "", bank_prefix: "", recall_top_k: 5 },
        quorum: { enabled: false, reviewers: 1 },
        roster: [],
        prompt_budget_bytes: 0,
      },
    })),
    fetchIssueHistory: vi.fn(async () => ({ issue_identifier: "", runs: [] })),
    fetchTeamsRoom: vi.fn(async () => ({ messages: [], skipped: [] })),
    fetchTeamsRecall: vi.fn(async () => ({ identity: "", facts: [], skipped: [] })),
  };
});

const { ConsoleApp } = await import("./ConsoleApp");

function version(teams_enabled: boolean): DaemonVersion {
  return { version: "v0.4.0", commit: "abc", built_at: "2026-09-01T00:00:00Z", teams_enabled };
}

function mount(hash = "") {
  window.location.hash = hash;
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ConsoleApp />
    </QueryClientProvider>,
  );
}

/** The rail's nav rows, in order, by their route id. */
function railItems(): string[] {
  return [...document.querySelectorAll("nav[aria-label='Primary'] a")].map(
    (a) => a.getAttribute("data-nav") ?? "",
  );
}

/** The route ids of every highlighted nav row — exactly one, once the rail has settled. */
function activeNavs(): string[] {
  return [...document.querySelectorAll("nav[aria-label='Primary'] a.active")].map(
    (a) => a.getAttribute("data-nav") ?? "",
  );
}

beforeEach(() => {
  window.history.replaceState(null, "", "/");
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("the rail is capability-gated on /api/v1/version (§2.2)", () => {
  // Box 2.1
  it("renders Jobs, Teams, Memory and Settings when teams is on", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));
  });

  // Box 2.2 — the load-bearing one: ABSENT, not greyed.
  it("renders ONLY Jobs and Settings when teams is off, with Teams and Memory absent from the DOM", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "settings"]));
    expect(document.querySelector("[data-nav='teams']")).toBeNull();
    expect(document.querySelector("[data-nav='memory']")).toBeNull();
    expect(screen.queryByText("Teams")).toBeNull();
    expect(screen.queryByText("Memory")).toBeNull();
  });

  it("reads a daemon too old to carry the field as teams off", async () => {
    h.fetchVersion.mockResolvedValue({ version: "v0.3.0", commit: "old", built_at: "" });
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "settings"]));
  });
});

describe("routing (§2.3)", () => {
  // Box 2.3
  it("lands on Jobs by default and never auto-lands on Teams", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("");
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Jobs"));
    // Normalised once the capability settles — the gate deliberately does not touch the URL
    // before /api/v1/version has answered.
    await waitFor(() => expect(window.location.hash).toBe("#jobs"));
    expect(activeNavs()).toEqual(["jobs"]);
  });

  it("lands on Jobs from an unknown deep link too", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#nonsense");
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Jobs"));
  });

  // Box 2.4
  it.each(["#teams", "#memory", "#manage"])(
    "redirects %s to Jobs when teams is off",
    async (hash) => {
      h.fetchVersion.mockResolvedValue(version(false));
      mount(hash);
      await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Jobs"));
      // The address bar is corrected too — a stale bookmark must not keep lying.
      await waitFor(() => expect(window.location.hash).toBe("#jobs"));
    },
  );

  it("keeps a job deep link reachable with teams off", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    mount("#job/STUDIO-654");
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toContain("STUDIO-654"));
  });

  // Box 2.12 — asserted as "exactly one row is highlighted, and it is the parent", so a
  // second highlighted row would fail rather than hide behind a passing containment check.
  it("highlights the parent nav item for job/:key and for manage", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#job/STUDIO-654");
    await waitFor(() => expect(railItems()).toContain("teams")); // the rail has settled
    expect(activeNavs()).toEqual(["jobs"]);

    cleanup();
    mount("#manage");
    await waitFor(() => expect(railItems()).toContain("teams"));
    expect(activeNavs()).toEqual(["teams"]);
  });

  it("navigates from the rail", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount();
    await waitFor(() => expect(railItems()).toContain("settings"));
    fireEvent.click(screen.getByText("Settings"));
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Settings"));
  });
});

describe("Settings' Teams row (§8)", () => {
  // Box 2.5
  it("shows the Enable-teams card when teams is off", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    mount("#settings");
    await waitFor(() =>
      expect(screen.getByText(/the daemon runs solo, one agent per issue/i)).toBeTruthy(),
    );
    // It is the ONLY discovery path while the rail hides Teams, and it must not claim a live
    // toggle: teams.enabled is boot-loaded.
    expect(screen.getByText(/changes apply on restart/i)).toBeTruthy();
    expect(screen.queryByText(/Manage team/)).toBeNull();
  });

  it("shows Manage team → when teams is on", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    await waitFor(() => expect(screen.getByText(/Manage team/)).toBeTruthy());
    expect(screen.queryByText(/the daemon runs solo/i)).toBeNull();
  });

  it("routes Manage team → to the manage route, which highlights Teams", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    fireEvent.click(await screen.findByText(/Manage team/));
    await waitFor(() => expect(window.location.hash).toBe("#manage"));
    expect(activeNavs()).toEqual(["teams"]);
  });
});
