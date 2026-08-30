// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { TeamsConfig, TeamsConfigView } from "@/lib/api";

const h = vi.hoisted(() => ({
  fetchTeamsConfig: vi.fn(),
  saveTeamsConfig: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchTeamsConfig: h.fetchTeamsConfig, saveTeamsConfig: h.saveTeamsConfig };
});

import { TeamsTab } from "@/components/settings/TeamsTab";

const config: TeamsConfig = {
  enabled: true,
  manager: { mode: "labels", default_identity: "", model: "claude-opus-5", max_tokens: 4000, timeout_ms: 5000 },
  memory: { backend: "local", path: "", endpoint: "", bank_prefix: "agent-", recall_top_k: 8 },
  roster: [{ name: "alice", profile: "swe", labels: ["rust"], bank: "", max_concurrent: 0 }],
  prompt_budget_bytes: 16000,
};

function view(over: Partial<TeamsConfigView> = {}): TeamsConfigView {
  return {
    path: "/home/d/.rhapsody/teams.yaml",
    present: false,
    error: "",
    config: { ...config, enabled: false, roster: [] },
    restart_required: true,
    ...over,
  };
}

function renderTab(v: TeamsConfigView = view()) {
  h.fetchTeamsConfig.mockResolvedValue(v);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <TeamsTab />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("TeamsTab — the off state", () => {
  // The never-seed rule at the UI layer: an absent teams.yaml means Teams is off, and opening this
  // tab must not create one. The only path to a file is the explicit action below.
  it("says Teams is off and offers a deliberate create action", async () => {
    renderTab();
    expect(await screen.findByText("Teams is off")).toBeTruthy();
    expect(screen.getByText(/Nothing creates one until you do/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Create teams.yaml…" })).toBeTruthy();
    expect(h.saveTeamsConfig).not.toHaveBeenCalled();
  });

  it("names the exact path the daemon reads", async () => {
    renderTab();
    expect(await screen.findByText("/home/d/.rhapsody/teams.yaml")).toBeTruthy();
  });

  // A rejected teams.yaml reads as "Teams is off" everywhere else in the app, which is
  // indistinguishable from never having written one. This is the one place that difference shows.
  it("reports a present-but-rejected file with the daemon's reason", async () => {
    renderTab(view({ present: true, error: 'teams_invalid: roster name "Alice" is not label-safe' }));
    expect(await screen.findByText("Teams is off — teams.yaml was rejected")).toBeTruthy();
    expect(screen.getByText(/is not label-safe/)).toBeTruthy();
  });

  it("summarises an existing, loading file", async () => {
    renderTab(view({ present: true, config }));
    expect(await screen.findByText("Teams is on")).toBeTruthy();
    expect(screen.getByText(/1 teammate\(s\) · assignment: labels · memory: local/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Edit teams.yaml…" })).toBeTruthy();
  });
});

describe("TeamsTab — the editor", () => {
  async function openEditor(v: TeamsConfigView = view()) {
    renderTab(v);
    fireEvent.click(await screen.findByRole("button", { name: v.present ? "Edit teams.yaml…" : "Create teams.yaml…" }));
  }

  it("shows exactly what Save will write, and that a restart is needed", async () => {
    await openEditor();
    fireEvent.change(screen.getByLabelText("Teammate 1 name"), { target: { value: "alice" } });
    fireEvent.change(screen.getByLabelText("Teammate 1 labels"), { target: { value: "rust, config" } });
    expect(screen.getByText(/- name: alice/)).toBeTruthy();
    expect(screen.getByText(/labels: \[rust, config\]/)).toBeTruthy();
    expect(screen.getByText(/Restart the daemon for this to take effect/)).toBeTruthy();
  });

  // The same three rules `Teams::validate` enforces, so the obvious mistake is caught while typing
  // rather than after a round-trip the daemon would refuse.
  it("blocks Save on a name the daemon would reject", async () => {
    await openEditor();
    fireEvent.change(screen.getByLabelText("Teammate 1 name"), { target: { value: "Alice" } });
    expect(screen.getByText(/is not label-safe/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Save teams.yaml" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("writes the file only when Save is pressed", async () => {
    await openEditor();
    fireEvent.change(screen.getByLabelText("Teammate 1 name"), { target: { value: "alice" } });
    fireEvent.change(screen.getByLabelText("Teammate 1 labels"), { target: { value: "rust" } });
    expect(h.saveTeamsConfig).not.toHaveBeenCalled();

    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.enabled).toBe(true);
    expect(sent.roster).toEqual([{ name: "alice", profile: "swe", labels: ["rust"] }]);
  });

  // A minimal editor must not silently drop what it does not show.
  it("preserves keys the editor does not expose when editing an existing file", async () => {
    await openEditor(view({ present: true, config }));
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.manager.model).toBe("claude-opus-5");
    expect(sent.memory.bank_prefix).toBe("agent-");
    expect(sent.prompt_budget_bytes).toBe(16000);
  });

  it("surfaces the daemon's rejection verbatim and stays in the editor", async () => {
    await openEditor();
    fireEvent.change(screen.getByLabelText("Teammate 1 name"), { target: { value: "alice" } });
    h.saveTeamsConfig.mockRejectedValue(new Error('teams_invalid: duplicate roster name "alice"'));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    expect(await screen.findByText(/duplicate roster name "alice"/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Save teams.yaml" })).toBeTruthy();
  });

  it("adds and removes roster rows", async () => {
    await openEditor();
    fireEvent.click(screen.getByRole("button", { name: "Add teammate" }));
    expect(screen.getByLabelText("Teammate 2 name")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove teammate 2" }));
    expect(screen.queryByLabelText("Teammate 2 name")).toBeNull();
  });
});
