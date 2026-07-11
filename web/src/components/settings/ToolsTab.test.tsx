// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ToolResult } from "@/lib/bindings";

const allTools: ToolResult[] = [
  { name: "claude", path: "/opt/homebrew/bin/claude", found: true, healthy: true, version: "2.1.4", detail: "Logged in · subscription (Max)" },
  { name: "gh", path: "/opt/homebrew/bin/gh", found: true, healthy: true, version: "2.62.0", detail: "Authenticated as @djohansen" },
  { name: "gt", path: "", found: false, healthy: false, version: "", detail: "Not found on PATH" },
  { name: "git", path: "/usr/bin/git", found: true, healthy: false, version: "2.45.2", detail: "Update available (2.49.0)" },
];

const h = vi.hoisted(() => ({
  probeTools: vi.fn(),
  setToolOverride: vi.fn(async () => {}),
  installTool: vi.fn(async () => {}),
  pickFile: vi.fn(async () => "/new/path/bin/git"),
}));

vi.mock("@/lib/bindings", () => ({
  probeTools: h.probeTools,
  setToolOverride: h.setToolOverride,
  installTool: h.installTool,
  pickFile: h.pickFile,
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchLinearIdentity: vi.fn(async () => ({ connected: true, name: "David Johansen", display_name: "David", email: "d@x.io", token: "lin_api_••••3f2a" })),
  };
});

import { ToolsTab } from "@/components/settings/ToolsTab";

function renderTools() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ToolsTab />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

beforeEach(() => {
  h.probeTools.mockResolvedValue(allTools);
  h.pickFile.mockResolvedValue("/new/path/bin/git");
});

describe("ToolsTab", () => {
  it("summarizes issues + detected count from the toolcheck", async () => {
    renderTools();
    expect(await screen.findByText("2 issues need attention")).toBeTruthy();
    expect(screen.getByText("2 of 4 required CLIs detected · re-checked on launch")).toBeTruthy();
  });

  it("shows an all-ready banner when every CLI is healthy", async () => {
    h.probeTools.mockResolvedValue(allTools.slice(0, 2));
    renderTools();
    expect(await screen.findByText("All systems ready")).toBeTruthy();
  });

  it("renders Install for a missing CLI and Update for an unhealthy one", async () => {
    renderTools();
    expect(await screen.findByRole("button", { name: "Install" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Update" })).toBeTruthy();
    // a healthy CLI shows the Ready status, not an action button
    expect(screen.getAllByText("Ready").length).toBeGreaterThanOrEqual(1);
  });

  it("re-runs the preflight probe on demand", async () => {
    renderTools();
    await screen.findByText("2 issues need attention");
    expect(h.probeTools).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: /Re-run preflight/ }));
    await waitFor(() => expect(h.probeTools).toHaveBeenCalledTimes(2));
  });

  it("installs a missing CLI through the supervisor binding", async () => {
    renderTools();
    fireEvent.click(await screen.findByRole("button", { name: "Install" }));
    await waitFor(() => expect(h.installTool).toHaveBeenCalledWith("gt"));
  });

  it("persists a path override through the file picker + setToolOverride (executable path, not a dir)", async () => {
    renderTools();
    await screen.findByText("2 issues need attention");
    fireEvent.click(screen.getByRole("button", { name: "Choose path for git" }));
    await waitFor(() => expect(h.pickFile).toHaveBeenCalled());
    await waitFor(() => expect(h.setToolOverride).toHaveBeenCalledWith("git", "/new/path/bin/git"));
  });

  it("persists a typed path override via setToolOverride (works without a picker binding)", async () => {
    renderTools();
    await screen.findByText("2 issues need attention");
    const field = screen.getByLabelText("git path override");
    // surrounding whitespace must be trimmed before it reaches the binding
    fireEvent.change(field, { target: { value: "  /opt/homebrew/bin/git  " } });
    fireEvent.blur(field);
    await waitFor(() => expect(h.setToolOverride).toHaveBeenCalledWith("git", "/opt/homebrew/bin/git"));
  });

  it("surfaces an error (instead of silently failing) when the binding rejects an override", async () => {
    h.setToolOverride.mockRejectedValueOnce(new Error("not an executable file"));
    renderTools();
    await screen.findByText("2 issues need attention");
    const field = screen.getByLabelText("git path override");
    fireEvent.change(field, { target: { value: "/bad/path" } });
    fireEvent.blur(field);
    expect(await screen.findByText("not an executable file")).toBeTruthy();
  });

  it("mirrors the connected Linear account read-only", async () => {
    renderTools();
    expect(await screen.findByText(/Connected as/)).toBeTruthy();
    expect(screen.getByText("David Johansen")).toBeTruthy();
    expect(screen.getByText("Authenticated")).toBeTruthy();
  });

  it("does not show 'All systems ready' when the probe yields no tools (no bridge / failure)", async () => {
    h.probeTools.mockResolvedValue([]);
    renderTools();
    expect(await screen.findByText("Tool preflight unavailable")).toBeTruthy();
    expect(screen.queryByText("All systems ready")).toBeNull();
    expect(screen.getByText(/No CLIs detected/)).toBeTruthy();
  });

  it("does not render stale CLI rows when a re-probe errors", async () => {
    h.probeTools.mockReset();
    h.probeTools.mockResolvedValueOnce(allTools).mockRejectedValue(new Error("probe failed"));
    renderTools();
    await screen.findByText("2 issues need attention");
    expect(screen.getByText("claude")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Re-run preflight/ }));
    // on error the banner flips to unavailable and the stale rows are replaced by the empty note
    expect(await screen.findByText("Tool preflight unavailable")).toBeTruthy();
    expect(screen.queryByText("claude")).toBeNull();
    expect(screen.getByText(/No CLIs detected/)).toBeTruthy();
  });
});
