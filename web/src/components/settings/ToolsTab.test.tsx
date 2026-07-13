// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ToolResult } from "@/lib/bindings";

// Reskinned Tools tab (mock 2c): a preflight header (age + Re-run), a read-only Linear mirror, and
// the required-CLI rows with the amber warning state for a binary missing from PATH. A missing OR
// unhealthy binary reads amber; the previous Install/Update supervisor action is replaced by the
// mock's universal path-override remediation.
const allTools: ToolResult[] = [
  { name: "claude", path: "/opt/homebrew/bin/claude", found: true, healthy: true, version: "2.1.4", detail: "Logged in · subscription (Max)" },
  { name: "gh", path: "", found: false, healthy: false, version: "", detail: "Not found on PATH" },
  { name: "git", path: "/usr/bin/git", found: true, healthy: false, version: "2.45.2", detail: "Update available (2.49.0)" },
];

const h = vi.hoisted(() => ({
  probeTools: vi.fn(),
  setToolOverride: vi.fn(async () => {}),
  pickFile: vi.fn(async () => "/new/path/bin/git"),
}));

vi.mock("@/lib/bindings", () => ({
  probeTools: h.probeTools,
  setToolOverride: h.setToolOverride,
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
  it("mirrors the connected Linear account read-only", async () => {
    renderTools();
    expect(await screen.findByText(/Connected as/)).toBeTruthy();
    expect(screen.getByText("David Johansen")).toBeTruthy();
    expect(screen.getByText("Authenticated")).toBeTruthy();
  });

  it("shows the amber PATH warning for a missing CLI (gh)", async () => {
    renderTools();
    expect(await screen.findByText("Not found on PATH — PR checks and summons will fail")).toBeTruthy();
  });

  it("renders a healthy CLI with its version and no warning message", async () => {
    renderTools();
    expect(await screen.findByText("v2.1.4")).toBeTruthy();
    // claude is healthy, so it offers "Override…", not the amber "Set path…"
    expect(screen.getByRole("button", { name: "Override claude path" })).toBeTruthy();
  });

  it("offers 'Set path…' on a warning row (missing OR unhealthy binary)", async () => {
    renderTools();
    // gh is missing, git is present-but-unhealthy — both warn → "Set path…"
    expect(await screen.findByRole("button", { name: "Set gh path" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Set git path" })).toBeTruthy();
  });

  it("re-runs the preflight probe on demand", async () => {
    renderTools();
    await screen.findByText("Not found on PATH — PR checks and summons will fail");
    expect(h.probeTools).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: /Re-run preflight/ }));
    await waitFor(() => expect(h.probeTools).toHaveBeenCalledTimes(2));
  });

  it("persists a path override through the file picker + setToolOverride (executable path, not a dir)", async () => {
    renderTools();
    await screen.findByText("Not found on PATH — PR checks and summons will fail");
    fireEvent.click(screen.getByRole("button", { name: "Set git path" }));
    await waitFor(() => expect(h.pickFile).toHaveBeenCalled());
    await waitFor(() => expect(h.setToolOverride).toHaveBeenCalledWith("git", "/new/path/bin/git"));
  });

  it("persists a typed path override via setToolOverride (works without a picker binding)", async () => {
    renderTools();
    await screen.findByText("Not found on PATH — PR checks and summons will fail");
    const field = screen.getByLabelText("git path override");
    // surrounding whitespace must be trimmed before it reaches the binding
    fireEvent.change(field, { target: { value: "  /opt/homebrew/bin/git  " } });
    fireEvent.blur(field);
    await waitFor(() => expect(h.setToolOverride).toHaveBeenCalledWith("git", "/opt/homebrew/bin/git"));
  });

  it("surfaces an error (instead of silently failing) when the binding rejects an override", async () => {
    h.setToolOverride.mockRejectedValueOnce(new Error("not an executable file"));
    renderTools();
    await screen.findByText("Not found on PATH — PR checks and summons will fail");
    const field = screen.getByLabelText("git path override");
    fireEvent.change(field, { target: { value: "/bad/path" } });
    fireEvent.blur(field);
    expect(await screen.findByText("not an executable file")).toBeTruthy();
  });

  it("shows a neutral empty note when the probe yields no tools (no desktop bridge)", async () => {
    h.probeTools.mockResolvedValue([]);
    renderTools();
    expect(await screen.findByText("No required CLIs detected.")).toBeTruthy();
    // no stale "Symphony desktop app" copy survives the rebrand
    expect(screen.queryByText(/Symphony/)).toBeNull();
  });
});
