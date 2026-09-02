// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import type { LogLine, LogStreamStatus } from "@/hooks/useLogStream";
import type { Updater } from "@/hooks/useUpdater";
import type { ToolResult, UpdateInfo } from "@/lib/bindings";

// STUDIO-691 — the console's Tools, Logs and Updates surfaces (STUDIO-681 §8.1).
//
// The acceptance is PARITY: after the §2.2.1 flip the console must lose no capability the shipped
// Podium Settings has, and the STUDIO-687 audit found these three (G4, G5, G3) unreachable. So these
// tests do not check layout — they drive each console view against the SAME mocked data path the
// shipped tab's own test uses and assert the shipped tab's capabilities are all there, plus the
// source contracts that would let the parity rot silently.

const h = vi.hoisted(() => ({
  probeTools: vi.fn(async (): Promise<ToolResult[]> => []),
  setToolOverride: vi.fn(async () => {}),
  pickFile: vi.fn(async () => "/new/path/bin/gh"),
  logs: { lines: [] as LogLine[], status: "open" as LogStreamStatus, clear: vi.fn() },
}));

vi.mock("@/lib/bindings", async (orig) => ({
  ...(await orig<typeof import("@/lib/bindings")>()),
  probeTools: h.probeTools,
  setToolOverride: h.setToolOverride,
  pickFile: h.pickFile,
}));

vi.mock("@/lib/api", async (orig) => ({
  ...(await orig<typeof import("@/lib/api")>()),
  fetchLinearIdentity: vi.fn(async () => ({
    connected: true,
    name: "David Johansen",
    display_name: "David",
    email: "d@x.io",
    token: "lin_api_••••3f2a",
  })),
}));

vi.mock("@/hooks/useLogStream", () => ({
  useLogStream: () => h.logs,
}));

const { LogsView, ToolsView, UpdatesView } = await import("./SettingsTabView");

const onNavigate = vi.fn();

function mount(node: ReactNode) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{node}</QueryClientProvider>);
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  h.probeTools.mockResolvedValue([]);
  h.logs.lines = [];
  h.logs.status = "open";
});

// The same three-CLI fixture ToolsTab.test.tsx drives: one healthy, one missing from PATH, one
// found-but-unhealthy.
const TOOLS: ToolResult[] = [
  { name: "claude", path: "/opt/homebrew/bin/claude", found: true, healthy: true, version: "2.1.4", detail: "Logged in · subscription (Max)" },
  { name: "gh", path: "", found: false, healthy: false, version: "", detail: "Not found on PATH" },
  { name: "git", path: "/usr/bin/git", found: true, healthy: false, version: "2.45.2", detail: "Update available (2.49.0)" },
];

describe("Tools — the tool doctor in console chrome (audit G4)", () => {
  it("renders every probed CLI with its version and its not-found warning", async () => {
    h.probeTools.mockResolvedValue(TOOLS);
    mount(<ToolsView onNavigate={onNavigate} />);
    expect(await screen.findByText("claude")).toBeTruthy();
    expect(screen.getByText("gh")).toBeTruthy();
    expect(screen.getByText("git")).toBeTruthy();
    expect(screen.getByText("v2.1.4")).toBeTruthy();
    // The missing binary names the consequence, not a bare error — the shipped copy, unchanged.
    expect(screen.getByText(/Not found on PATH — PR checks and summons will fail/)).toBeTruthy();
    // …and an unhealthy-but-found one still reads its own detail.
    expect(screen.getByText("Update available (2.49.0)")).toBeTruthy();
  });

  it("keeps the path override — the tab's one write — wired to the same binding", async () => {
    h.probeTools.mockResolvedValue(TOOLS);
    mount(<ToolsView onNavigate={onNavigate} />);
    const field = await screen.findByLabelText("gh path override");
    fireEvent.change(field, { target: { value: " /usr/local/bin/gh " } });
    fireEvent.blur(field);
    // Trimmed before it is persisted, exactly as the shipped tab does it.
    await waitFor(() => expect(h.setToolOverride).toHaveBeenCalledWith("gh", "/usr/local/bin/gh"));
  });

  it("keeps the file picker and the Re-run preflight action", async () => {
    h.probeTools.mockResolvedValue(TOOLS);
    mount(<ToolsView onNavigate={onNavigate} />);
    fireEvent.click(await screen.findByRole("button", { name: "Set gh path" }));
    await waitFor(() => expect(h.pickFile).toHaveBeenCalled());

    const before = h.probeTools.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: /Re-run preflight/ }));
    await waitFor(() => expect(h.probeTools.mock.calls.length).toBeGreaterThan(before));
  });

  it("mirrors the Linear connection the General tab configured", async () => {
    h.probeTools.mockResolvedValue(TOOLS);
    mount(<ToolsView onNavigate={onNavigate} />);
    expect(await screen.findByText("Linear connection")).toBeTruthy();
    await waitFor(() => expect(screen.getByText("David Johansen")).toBeTruthy());
  });
});

function line(over: Partial<LogLine>): LogLine {
  return { seq: 1, time: "2026-06-07T10:00:00Z", level: "INFO", msg: "hello", ...over };
}

describe("Logs — the live log stream in console chrome (audit G5)", () => {
  it("renders streamed lines with their attrs and the live status", () => {
    h.logs.lines = [
      line({ seq: 1, level: "INFO", msg: "poll tick", attrs: { eligible: "3" } }),
      line({ seq: 2, level: "ERROR", msg: "dispatch failed" }),
    ];
    mount(<LogsView onNavigate={onNavigate} />);
    expect(screen.getByText("poll tick")).toBeTruthy();
    expect(screen.getByText("dispatch failed")).toBeTruthy();
    expect(screen.getByText("eligible=3")).toBeTruthy();
    expect(screen.getByText("live")).toBeTruthy();
  });

  it("keeps the level filter, and it actually filters", () => {
    h.logs.lines = [line({ seq: 1, level: "INFO", msg: "poll tick" }), line({ seq: 2, level: "ERROR", msg: "dispatch failed" })];
    mount(<LogsView onNavigate={onNavigate} />);
    const tabs = screen.getByRole("tablist", { name: "Log level filter" });
    fireEvent.click(within(tabs).getByRole("tab", { name: /error/i }));
    expect(screen.getByText("dispatch failed")).toBeTruthy();
    expect(screen.queryByText("poll tick")).toBeNull();
  });

  it("keeps Clear wired to the stream's own buffer", () => {
    h.logs.lines = [line({ seq: 1 })];
    mount(<LogsView onNavigate={onNavigate} />);
    fireEvent.click(screen.getByRole("button", { name: /clear/i }));
    expect(h.logs.clear).toHaveBeenCalledOnce();
  });

  it("reports an unreachable stream rather than an empty console", () => {
    h.logs.status = "closed";
    mount(<LogsView onNavigate={onNavigate} />);
    expect(screen.getByText("unavailable")).toBeTruthy();
  });
});

function info(over: Partial<UpdateInfo> = {}): UpdateInfo {
  return { available: true, version: "1.4.0", current_version: "1.3.0", notes: "Fixes the sync bug.", ...over };
}

function stubUpdater(over: Partial<Updater> = {}): Updater {
  return {
    phase: "idle",
    info: null,
    progress: null,
    error: null,
    activeRunsPrompt: null,
    pending: false,
    check: vi.fn(),
    download: vi.fn(),
    requestInstall: vi.fn(),
    confirmInstallNow: vi.fn(),
    deferToQuit: vi.fn(),
    dismissPrompt: vi.fn(),
    ...over,
  };
}

// The sharpest of the three: Updates is the desktop app's ENTIRE auto-update path, so every step of
// the phase machine has to survive the move into the console — a check, a download, and the install
// that must warn before it stops a running agent.
describe("Updates — the desktop auto-update path in console chrome (audit G3)", () => {
  it("fires a manual check", () => {
    const u = stubUpdater();
    mount(<UpdatesView onNavigate={onNavigate} updater={u} />);
    fireEvent.click(screen.getByRole("button", { name: /check for updates/i }));
    expect(u.check).toHaveBeenCalledOnce();
  });

  it("announces an available version with its notes, and downloads it", () => {
    const u = stubUpdater({ phase: "available", info: info() });
    mount(<UpdatesView onNavigate={onNavigate} updater={u} />);
    expect(screen.getByText(/1\.4\.0/)).toBeTruthy();
    // The release notes ride along in the shipped "What's new" disclosure.
    fireEvent.click(screen.getByRole("button", { name: /What's new/i }));
    expect(screen.getByText(/Fixes the sync bug\./)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /^download/i }));
    expect(u.download).toHaveBeenCalledOnce();
  });

  it("offers the restart once the download is ready", () => {
    const u = stubUpdater({ phase: "ready", info: info() });
    mount(<UpdatesView onNavigate={onNavigate} updater={u} />);
    fireEvent.click(screen.getByRole("button", { name: /restart/i }));
    expect(u.requestInstall).toHaveBeenCalledOnce();
  });

  // The safety interlock. It lives INSIDE UpdatesTab, so embedding the shipped component is what
  // brings it along — a re-implementation of the panel would have dropped it, and an install would
  // then stop running agents with no warning.
  it("still warns before an install stops running agents", () => {
    const u = stubUpdater({ phase: "ready", info: info(), activeRunsPrompt: 2 });
    mount(<UpdatesView onNavigate={onNavigate} updater={u} />);
    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-label")).toBe("2 agents are playing");
    // All three of the shipped choices survive: stop them now, wait for the next quit, or cancel.
    fireEvent.click(within(dialog).getByRole("button", { name: "Install on next quit" }));
    expect(u.deferToQuit).toHaveBeenCalledOnce();
    fireEvent.click(within(dialog).getByRole("button", { name: "Update now" }));
    expect(u.confirmInstallNow).toHaveBeenCalledOnce();
  });
});

describe("console chrome", () => {
  it.each([
    ["Tools", <ToolsView onNavigate={onNavigate} />],
    ["Logs", <LogsView onNavigate={onNavigate} />],
    ["Updates", <UpdatesView onNavigate={onNavigate} updater={stubUpdater()} />],
  ])("%s heads its page and returns to Settings from the breadcrumb", (title, node) => {
    mount(node);
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(title);
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(onNavigate).toHaveBeenCalledWith("settings");
  });
});

// Source contracts — the rules this slice could break silently, checked against the source rather
// than the DOM (the WorkflowView.test.tsx / index.css.test.ts precedent).
describe("source contracts", () => {
  const src = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

  // NOTE: the §2.2.1 "land-dark" guard that used to sit here — asserting App.tsx still rendered the
  // Podium <AppShell /> — was retired by STUDIO-687's box-6.4 flip. The root is now pinned once, in
  // ConsoleApp.test.tsx ("the flip — App.tsx renders the console").

  // Parity is a property of the code: the views EMBED the shipped tabs. A future edit that swapped
  // an import for a hand-rolled panel would pass every behavioural test above only until it drifted
  // — this is what makes the reuse itself the contract.
  it("embeds the shipped Podium tabs rather than re-implementing them", () => {
    const view = src("./SettingsTabView.tsx");
    for (const tab of ["ToolsTab", "LogsTab", "UpdatesTab"]) {
      expect(view).toContain(`import { ${tab} } from "@/components/settings/${tab}"`);
      expect(view).toContain(`<${tab}`);
    }
  });

  // No invented endpoints (§11): the views reach the daemon only through the hooks the shipped tabs
  // already use, so nothing here may name an API path of its own.
  it("adds no endpoint of its own", () => {
    // Comments stripped first — the file's doc comment NAMES the endpoints the shipped hooks
    // already own, which is the point; what must not appear is a path in the code.
    const code = src("./SettingsTabView.tsx")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/^\s*\/\/.*$/gm, "");
    expect(code).not.toMatch(/\/api\//);
    expect(code).not.toMatch(/\bfetch\s*\(/);
  });

  // The embedded Podium tabs render inside `.rh-console`, where `--accent` means the brand amber
  // rather than Podium's hover background. The embed scope hands the Podium meaning back; losing
  // that line would repaint the tabs' button hovers bright amber.
  it("restores the Podium meaning of --accent inside the embed scope", () => {
    expect(src("../../../theme/console-settings-tabs.css")).toMatch(
      /\.tabembed\s*{[^}]*--accent:\s*var\(--bg-hover\)/,
    );
  });
});
