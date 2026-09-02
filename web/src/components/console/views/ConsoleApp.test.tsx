// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { DaemonVersion } from "@/lib/api";
import type { StatusDTO } from "@/lib/bindings";

// STUDIO-681 §10, sub-ticket 2 — the app shell's acceptance boxes 2.1-2.5 and 2.12.
//
// Every one of them turns on ONE field of GET /api/v1/version: `teams_enabled`. These tests
// drive the real ConsoleApp against a mocked API rather than the gate helper directly, because
// what §2.2 promises is about the RENDERED rail — "absent from the DOM, not merely hidden" is
// not a claim a pure function can make.

const h = vi.hoisted(() => {
  // The two tray/lifecycle event bindings are subscriptions, not calls: the real ones hand the
  // Tauri listener back an unsubscribe. These stand in with the same contract so a test can emit
  // the event the desktop host would, and so an unmount really does detach.
  const navCbs = new Set<(view: string) => void>();
  const downCbs = new Set<() => void>();
  return {
    fetchVersion: vi.fn(),
    getStatus: vi.fn(),
    hasOverlayTitlebar: vi.fn(() => false),
    credentialStatus: vi.fn(),
    listLinearProjects: vi.fn(),
    probeTools: vi.fn(),
    writeInitialConfig: vi.fn(),
    onNavigate: vi.fn((cb: (view: string) => void) => {
      navCbs.add(cb);
      return () => void navCbs.delete(cb);
    }),
    onShuttingDown: vi.fn((cb: () => void) => {
      downCbs.add(cb);
      return () => void downCbs.delete(cb);
    }),
    emitNavigate: (view: string) => navCbs.forEach((cb) => cb(view)),
    emitShuttingDown: () => downCbs.forEach((cb) => cb()),
    navCount: () => navCbs.size,
  };
});

// The supervisor bridge. `getStatus` is the first-run gate's input (STUDIO-692); the four below
// are the SHIPPED wizard's own data path, stood in for so the first-run flow can be driven to its
// partial-write failure. Everything else stays the real binding.
vi.mock("@/lib/bindings", async (orig) => {
  const actual = await orig<typeof import("@/lib/bindings")>();
  return {
    ...actual,
    getStatus: h.getStatus,
    hasOverlayTitlebar: h.hasOverlayTitlebar,
    credentialStatus: h.credentialStatus,
    listLinearProjects: h.listLinearProjects,
    probeTools: h.probeTools,
    writeInitialConfig: h.writeInitialConfig,
    onNavigate: h.onNavigate,
    onShuttingDown: h.onShuttingDown,
  };
});

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
    // The Workflow editor's data path. These tests assert ROUTING, not editing (the editor
    // itself is covered in WorkflowView.test.tsx), so a config-less payload is enough — the
    // view then renders its "couldn't read WORKFLOW.md" state instead of reaching the network.
    fetchTypedConfig: vi.fn(async () => ({ config: {}, prompt_body: "" })),
    fetchLinearIdentity: vi.fn(async () => null),
    fetchProjectStatuses: vi.fn(async () => []),
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

function status(configured: boolean): StatusDTO {
  return {
    state: configured ? "running" : "stopped",
    pid: configured ? 42 : 0,
    restarts: 0,
    last_err: "",
    url: "http://127.0.0.1:8080",
    healthy: configured,
    agent_count: 0,
    configured,
  };
}

beforeEach(() => {
  window.history.replaceState(null, "", "/");
  // A plain browser has no supervisor bridge, so `getStatus` resolves null and the shell reads
  // "loading" — never "not-configured". That is the default every other test here runs under.
  h.getStatus.mockResolvedValue(null);
  // A plain browser by default; the STUDIO-701 block below is the only one that flips it.
  h.hasOverlayTitlebar.mockReturnValue(false);
  h.credentialStatus.mockResolvedValue({ has_token: true });
  h.listLinearProjects.mockResolvedValue([
    { id: "1", name: "Rhapsody", slug: "872639248532", team: "FND", color: "#10b981" },
  ]);
  h.probeTools.mockResolvedValue([]);
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

// STUDIO-690 — the Settings "Workflow" row is the entrance to the WORKFLOW.md editor (§8).
describe("Settings' Workflow row (§8, STUDIO-690)", () => {
  it("opens the WORKFLOW.md editor, which highlights Settings", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    fireEvent.click(await screen.findByRole("button", { name: /Edit/ }));
    await waitFor(() => expect(window.location.hash).toBe("#workflow"));
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Workflow"));
    expect(activeNavs()).toEqual(["settings"]);
  });

  it("stays reachable with teams off — WORKFLOW.md is the solo daemon's config too", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    mount("#workflow");
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Workflow"));
    expect(window.location.hash).toBe("#workflow");
  });
});

// STUDIO-687 §10 box 6.3 — every rail item must reach its real view.
//
// The audit found the Teams rail item landing on a "not built yet" placeholder: the §5 room
// (TeamsConsole, sub-ticket 3) shipped fully built and tested, but nothing ever imported it, so
// the route this shell had reserved for it still rendered sub-ticket 2's stub. Both halves were
// green in isolation, which is exactly the silent 80%-shipped this box exists to catch. The
// assertion is on the room's own furniture rather than the heading, because "Teams" is also the
// stub's title — a heading test would have passed against the bug.
describe("every rail destination is the real view (§10 box 6.3)", () => {
  it("routes Teams to the room itself, not to a placeholder", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#teams");
    expect(await screen.findByRole("heading", { level: 2, name: "The room" })).toBeTruthy();
    expect(await screen.findByPlaceholderText(/Search the room/)).toBeTruthy();
    expect(screen.queryByText(/is not built yet/)).toBeNull();
  });

  it("routes the room's Manage team → card on to manage, under the Teams nav", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#teams");
    fireEvent.click(await screen.findByText("Manage team →"));
    await waitFor(() => expect(window.location.hash).toBe("#manage"));
    expect(activeNavs()).toEqual(["teams"]);
  });

  it("routes the room's Open memory → card on to the Memory page", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#teams");
    fireEvent.click(await screen.findByText("Open memory →"));
    await waitFor(() => expect(window.location.hash).toBe("#memory"));
    expect(activeNavs()).toEqual(["memory"]);
  });
});

// STUDIO-691 — the §8.1 Settings-parity rows: Tools, Logs and Updates.
//
// The STUDIO-687 audit found all three unreachable from the console (gaps G4, G5, G3) while the
// shipped Podium Settings nav has them, which blocks the §2.2.1 flip. These tests assert the same
// thing box 6.3 asserts of the rail — that the row reaches the REAL surface, not a stub — and they
// key off each shipped tab's own furniture rather than the heading, because a heading test would
// pass against a re-implementation that lost the capability.
describe("Settings' Tools/Logs/Updates rows (§8.1, STUDIO-691)", () => {
  it("opens the tool doctor, which highlights Settings", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    fireEvent.click(await screen.findByRole("button", { name: "Open Tools" }));
    await waitFor(() => expect(window.location.hash).toBe("#tools"));
    expect(await screen.findByRole("button", { name: /Re-run preflight/ })).toBeTruthy();
    expect(screen.getByText("Required CLIs")).toBeTruthy();
    expect(activeNavs()).toEqual(["settings"]);
  });

  it("opens the live log stream, which highlights Settings", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    fireEvent.click(await screen.findByRole("button", { name: "Open Logs" }));
    await waitFor(() => expect(window.location.hash).toBe("#logs"));
    expect(await screen.findByRole("tablist", { name: "Log level filter" })).toBeTruthy();
    expect(activeNavs()).toEqual(["settings"]);
  });

  it("opens the desktop updater, which highlights Settings", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount("#settings");
    fireEvent.click(await screen.findByRole("button", { name: "Open Updates" }));
    await waitFor(() => expect(window.location.hash).toBe("#updates"));
    expect(await screen.findByRole("button", { name: /Check for updates/i })).toBeTruthy();
    expect(activeNavs()).toEqual(["settings"]);
  });

  // Every one of the three is a solo-daemon surface — the tool doctor probes local CLIs, the log
  // tail is the daemon's own process log, and the updater updates the app. Gating them on teams
  // would strand a solo operator on Jobs, and Updates is the desktop app's whole update path.
  it.each([
    ["#tools", "Tools"],
    ["#logs", "Logs"],
    ["#updates", "Updates"],
  ])("keeps %s reachable with teams off", async (hash, heading) => {
    h.fetchVersion.mockResolvedValue(version(false));
    mount(hash);
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(heading));
    expect(window.location.hash).toBe(hash);
    expect(activeNavs()).toEqual(["settings"]);
  });

  it.each(["#tools", "#logs", "#updates"])("returns to Settings from %s's breadcrumb", async (hash) => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount(hash);
    fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
    await waitFor(() => expect(window.location.hash).toBe("#settings"));
  });
});

// STUDIO-692 — first run (§8.1, STUDIO-687 audit G2).
//
// The Podium shell swaps its whole chrome for the Onboarding wizard when the supervisor reports
// `configured: false`; a console that skipped this would flip live and hand a fresh install a
// config-less shell with nothing behind any of its rows. So the gate belongs to the shell, above
// the router: it pre-empts every route, including a deep link.
describe("first run routes to onboarding (§8.1, audit G2)", () => {
  it("shows the onboarding wizard when the daemon reports not-configured", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(false));
    mount();
    expect(await screen.findByRole("progressbar", { name: "Onboarding progress" })).toBeTruthy();
    // The rail is gone with it — every destination on it needs the config that does not exist.
    expect(document.querySelector("nav[aria-label='Primary']")).toBeNull();
  });

  it("renders the console normally when the daemon is configured", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));
    expect(screen.queryByRole("progressbar", { name: "Onboarding progress" })).toBeNull();
  });

  it("renders the console normally with no supervisor bridge at all (a plain browser)", async () => {
    h.fetchVersion.mockResolvedValue(version(false));
    h.getStatus.mockResolvedValue(null);
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "settings"]));
    expect(screen.queryByRole("progressbar", { name: "Onboarding progress" })).toBeNull();
  });

  it("pre-empts a deep link — a fresh install cannot use #settings either", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(false));
    mount("#settings");
    expect(await screen.findByRole("progressbar", { name: "Onboarding progress" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1, name: "Settings" })).toBeNull();
  });

  it("swaps to the console once the wizard has seeded a config", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(false));
    mount();
    expect(await screen.findByRole("progressbar", { name: "Onboarding progress" })).toBeTruthy();
    // The wizard's success path calls back into the shell, which re-reads status; the poll would
    // reach the same place a beat later.
    h.getStatus.mockResolvedValue(status(true));
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]), {
      timeout: 4000,
    });
    expect(screen.queryByRole("progressbar", { name: "Onboarding progress" })).toBeNull();
  });
});

// The first-run failure the console must not lose: `writeInitialConfig` can write WORKFLOW.md and
// THEN fail to start the daemon, so the very next status poll reports `configured: true` and the
// shell swaps the wizard — and its inline alert — away. The message is held above that swap, in
// the shell, or the operator is left in a console whose daemon is down with nothing said.
describe("a partial first-run write survives the swap into the console", () => {
  it("keeps the wizard's failure on screen after the config lands and the shell swaps", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(false));
    h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
    mount();

    fireEvent.click(await screen.findByRole("radio", { name: "Rhapsody" }));
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    await screen.findByText(/STEP 3 OF 3/);
    // The config lands; the daemon does not start. The poll will now see it as configured.
    h.getStatus.mockResolvedValue(status(true));
    fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

    // The shell swaps to the console...
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]), {
      timeout: 4000,
    });
    // ...and the failure came with it.
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("config saved, but the daemon could not start");

    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });
});

// ---- The flip (§2.2.1, box 6.4) -----------------------------------------------------------------

// Every slice 1–5 landed DARK: each shipped a source contract asserting `App.tsx` still rendered the
// Podium `<AppShell />`. STUDIO-687's audit found all six gates clean, so this — the ONE flip §2.2.1
// reserved for the final slice — inverts that contract. Those three per-slice guards are replaced by
// this single authoritative one; there is only one root to pin.
describe("the flip — App.tsx renders the console (§2.2.1, box 6.4)", () => {
  const src = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

  it("mounts <ConsoleApp /> as the app root, not the Podium shell", () => {
    const app = src("../../../App.tsx");
    expect(app).toContain("<ConsoleApp />");
    expect(app).not.toMatch(/<AppShell\s*\/>/);
  });

  // The verification-only primitive gallery is code-split precisely so it never ships in the main
  // bundle. The flip changes which shell renders, not that guarantee.
  it("keeps the #/demo gallery lazy so it stays out of the shipped bundle", () => {
    const app = src("../../../App.tsx");
    expect(app).toContain("React.lazy(");
    expect(app).toContain("useIsDemoRoute");
  });
});

// The two desktop behaviours the Podium shell owned that the flip would otherwise strand. They are
// not Settings surfaces (§8.1) but they ARE shipped capabilities, and the flip is what would drop
// them — so they move with the root rather than becoming a follow-up.
describe("the desktop bridge survives the flip", () => {
  it("routes the tray's Settings… item to Settings and Dashboard back to Jobs", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));

    act(() => h.emitNavigate("settings"));
    await waitFor(() => expect(activeNavs()).toEqual(["settings"]));

    act(() => h.emitNavigate("dashboard"));
    await waitFor(() => expect(activeNavs()).toEqual(["jobs"]));
  });

  // The subscription must detach with the shell — a listener left behind would fire into an
  // unmounted tree on the next tray click.
  it("detaches the tray subscription on unmount", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    const view = mount();
    await waitFor(() => expect(h.navCount()).toBe(1));
    view.unmount();
    expect(h.navCount()).toBe(0);
  });

  it("shows the shutdown overlay when the app begins quitting", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));
    expect(screen.queryByText("Shutting down…")).toBeNull();

    act(() => h.emitShuttingDown());
    expect(await screen.findByText("Shutting down…")).toBeTruthy();
  });
});

// STUDIO-701 — the desktop window chrome the §2.2.1 flip dropped on the floor.
//
// The packaged app asks for macOS `titleBarStyle: "Overlay"`, so it has no system title bar to
// move the window by and the native traffic lights float over the top-left of the web content —
// straight onto the rail's logo. The shell decides once, from the host, and hands the answer to
// whichever surface is mounted: the rail on a configured install, the setup bar on a fresh one.
describe("desktop window chrome (STUDIO-701)", () => {
  // The host predicate itself (bridge present AND macOS) is covered in bindings.test.ts; what is
  // under test here is that the shell asks it and passes the answer down.
  it("gives the rail a drag strip and the traffic-light inset in the desktop app", async () => {
    h.hasOverlayTitlebar.mockReturnValue(true);
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));
    expect(document.querySelector(".app.rh-console.overlay-titlebar")).not.toBeNull();
    const drag = document.querySelector(".rail")?.firstElementChild;
    expect(drag?.className).toBe("drag");
    expect(drag?.hasAttribute("data-tauri-drag-region")).toBe(true);
    // The lockup follows the strip, so the lights land on the strip and not on the mark.
    expect(drag?.nextElementSibling?.classList.contains("logo")).toBe(true);
  });

  it("gives the first-run setup bar the same drag region and inset", async () => {
    // A fresh install never sees the rail, and it is the FIRST window a desktop user gets: an
    // un-draggable one with the lights sitting on the wordmark.
    h.hasOverlayTitlebar.mockReturnValue(true);
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(false));
    mount();
    expect(await screen.findByRole("progressbar", { name: "Onboarding progress" })).toBeTruthy();
    expect(document.querySelector(".rh-console.setup.overlay-titlebar")).not.toBeNull();
    expect(document.querySelector("header.setuphead")?.hasAttribute("data-tauri-drag-region")).toBe(true);
  });

  it("leaves the daemon-served dashboard exactly as it was in a plain browser", async () => {
    h.fetchVersion.mockResolvedValue(version(true));
    h.getStatus.mockResolvedValue(status(true));
    mount();
    await waitFor(() => expect(railItems()).toEqual(["jobs", "teams", "memory", "settings"]));
    expect(document.querySelector(".overlay-titlebar")).toBeNull();
    expect(document.querySelector("[data-tauri-drag-region]")).toBeNull();
    expect(document.querySelector(".rail")?.firstElementChild?.classList.contains("logo")).toBe(true);
  });
});
