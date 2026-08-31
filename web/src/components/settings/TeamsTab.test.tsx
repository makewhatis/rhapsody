// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
  memory: { backend: "local", path: "", endpoint: "", api_key: "", bank_prefix: "agent-", recall_top_k: 8 },
  quorum: { enabled: false, reviewers: 2 },
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

// The YAML preview is a <pre>: testing-library's text matcher collapses whitespace, which would
// erase the very line structure this preview exists to show. Assert on its raw textContent instead.
function preview(): string {
  return document.querySelector("pre")?.textContent ?? "";
}

// A `Field inline` renders a two-column grid: [label + hint] | [control]. Scoping to that grid is
// how a test names ONE Stepper on a page that has several — the spinners are generically labelled
// "Increment"/"Decrement" by the shared primitive.
function field(label: string): HTMLElement {
  // label → its flex row → the label/hint column → the two-column grid that also holds the control.
  const el = screen.getByText(label).closest("div")?.parentElement?.parentElement;
  if (!el) throw new Error(`no field for ${label}`);
  return el;
}

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
    expect(sent.roster).toEqual([{ name: "alice", profile: "swe", labels: ["rust"], bank: "", max_concurrent: 0 }]);
  });

  // An editor must not silently drop what it does not show. STUDIO-667 models every field the
  // schema declares, so the key at risk is now the one a NEWER daemon serves that this build has
  // never heard of — an editor that sent only the keys it knows would delete it on the first save.
  it("preserves keys the editor does not model when editing an existing file", async () => {
    const future = { ...config, future_knob: { deep: [1, 2, 3] } } as unknown as TeamsConfig;
    await openEditor(view({ present: true, config: future }));
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: future }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect((sent as unknown as Record<string, unknown>).future_knob).toEqual({ deep: [1, 2, 3] });
    // …and a no-op edit is a no-op: every modelled field round-trips byte-for-byte.
    expect(sent.manager.model).toBe("claude-opus-5");
    expect(sent.memory.bank_prefix).toBe("agent-");
    expect(sent.prompt_budget_bytes).toBe(16000);
    expect(sent.quorum).toEqual({ enabled: false, reviewers: 2 });
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

// STUDIO-667 — "we never want to make someone configure yaml": every field in the Teams schema is
// reachable from this tab. The tests below are the coverage argument, one group per schema block.
describe("TeamsTab — the quorum", () => {
  async function openEditor(v: TeamsConfigView) {
    renderTab(v);
    fireEvent.click(await screen.findByRole("button", { name: v.present ? "Edit teams.yaml…" : "Create teams.yaml…" }));
  }

  const twoUp: TeamsConfig = {
    ...config,
    roster: [
      { name: "alice", profile: "swe", labels: [], bank: "", max_concurrent: 0 },
      { name: "bob", profile: "reviewer", labels: [], bank: "", max_concurrent: 0 },
    ],
  };

  // The whole trigger for this ticket: David had to hand-edit teams.yaml to turn the quorum on.
  it("enables, sizes and disables the quorum without touching the file", async () => {
    await openEditor(view({ present: true, config: twoUp }));
    const toggle = screen.getByRole("switch", { name: "Fan out reviews on handoff" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    // Sizing is not even offered while the quorum is off — there is nothing to size.
    expect(screen.queryByText("Reviewers per handoff")).toBeNull();

    fireEvent.click(toggle);
    expect(screen.getByText("Reviewers per handoff")).toBeTruthy();
    expect(preview()).toContain("quorum:\n  enabled: true\n  reviewers: 2");
    expect(h.saveTeamsConfig).not.toHaveBeenCalled();

    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: twoUp }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).quorum).toEqual({ enabled: true, reviewers: 2 });
  });

  // "Copy must say what it costs" — and must name the roster-size degradation, because the daemon
  // clamps silently: two teammates buy one reviewer, not two.
  it("states the cost in runs and the roster clamp", async () => {
    await openEditor(view({ present: true, config: twoUp }));
    fireEvent.click(screen.getByRole("switch", { name: "Fan out reviews on handoff" }));
    expect(screen.getByText(/fans out 1 review run/)).toBeTruthy();
    expect(screen.getByText(/2 teammates means 1/)).toBeTruthy();
  });

  it("floors the reviewer count at one — a quorum of zero is `enabled: false`, not a quorum", async () => {
    await openEditor(view({ present: true, config: twoUp }));
    fireEvent.click(screen.getByRole("switch", { name: "Fan out reviews on handoff" }));
    const dec = within(field("Reviewers per handoff")).getByRole("button", { name: "Decrement" });
    fireEvent.click(dec);
    fireEvent.click(dec);
    fireEvent.click(dec);
    expect(preview()).toContain("reviewers: 1");
  });
});

describe("TeamsTab — memory, the manager and the roster overrides", () => {
  async function openEditor(v: TeamsConfigView) {
    renderTab(v);
    fireEvent.click(await screen.findByRole("button", { name: v.present ? "Edit teams.yaml…" : "Create teams.yaml…" }));
  }

  // The cloud-bank switch is a UI action, not a yaml session (STUDIO-660 shipped the backend; the
  // v1 editor's backend list never grew the third option).
  it("switches memory onto the hindsight cloud bank and takes an endpoint", async () => {
    await openEditor(view({ present: true, config }));
    fireEvent.click(screen.getByRole("button", { name: "local" }));
    fireEvent.click(screen.getByRole("option", { name: "hindsight" }));
    fireEvent.change(screen.getByLabelText("Endpoint"), { target: { value: "https://h.example" } });
    fireEvent.change(screen.getByLabelText("API key"), { target: { value: "$HINDSIGHT_API_KEY" } });

    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.memory.backend).toBe("hindsight");
    expect(sent.memory.endpoint).toBe("https://h.example");
    expect(sent.memory.api_key).toBe("$HINDSIGHT_API_KEY");
  });

  // The acceptance criterion, at the surface that has to honour it.
  it("never renders a stored literal api_key back in cleartext", async () => {
    const stored: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "sk-live-abc123" },
    };
    await openEditor(view({ present: true, config: stored }));
    expect(document.body.textContent).not.toContain("sk-live-abc123");
    expect((document.body.innerHTML.match(/sk-live-abc123/g) ?? []).length).toBe(0);
    expect(screen.getByText(/A key is stored in teams.yaml and is not shown/)).toBeTruthy();
    // …and an unrelated save does not wipe the key the operator never saw.
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).memory.api_key).toBe("sk-live-abc123");
  });

  // Replace is destructive and one click away: it clears the carry-forward flag, so saving from
  // that state writes `api_key: ""` and de-authenticates the backend. The way back must outlive the
  // click that created the need for it, and the copy must say what a blank save does.
  it("offers a way back out of Replace, and says what saving blank would do", async () => {
    const stored: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "sk-live-abc123" },
    };
    await openEditor(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Replace" }));
    expect(screen.getByText(/Save with this blank and the stored key is removed/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Keep existing" }));
    // Back to the masked state — and the literal still never appeared on screen.
    expect(screen.getByText(/A key is stored in teams.yaml and is not shown/)).toBeTruthy();
    expect(document.body.textContent).not.toContain("sk-live-abc123");

    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).memory.api_key).toBe("sk-live-abc123");
  });

  // …but the undo is offered ONLY when there is something to keep. A fresh hindsight config with no
  // key must not grow a "Keep existing" button that would restore nothing.
  it("does not offer a way back when no key is stored", async () => {
    const fresh: TeamsConfig = { ...config, memory: { ...config.memory, backend: "hindsight", api_key: "" } };
    await openEditor(view({ present: true, config: fresh }));
    expect(screen.queryByRole("button", { name: "Keep existing" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Replace" })).toBeNull();
    expect(screen.getByLabelText("API key")).toBeTruthy();
  });

  // A `$NAME` is a POINTER, not a secret: it is shown and edited like any other field, with no
  // masking and no replace dance, because the credential never sits in teams.yaml at all.
  it("edits a $VAR indirection directly rather than masking it", async () => {
    const env: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "$HINDSIGHT_API_KEY" },
    };
    await openEditor(view({ present: true, config: env }));
    expect((screen.getByLabelText("API key") as HTMLInputElement).value).toBe("$HINDSIGHT_API_KEY");
    expect(screen.queryByRole("button", { name: "Replace" })).toBeNull();
  });

  it("replaces a stored literal only on an explicit ask", async () => {
    const stored: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "sk-live-abc123" },
    };
    await openEditor(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Replace" }));
    fireEvent.change(screen.getByLabelText("API key"), { target: { value: "$HINDSIGHT_API_KEY" } });
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).memory.api_key).toBe("$HINDSIGHT_API_KEY");
  });

  // Replace is one click and it destroys a credential. The way back has to survive the click that
  // creates the need for it, or an accidental press plus any unrelated save silently leaves the
  // hindsight backend unauthenticated.
  it("takes back a Replace without losing the stored key or the rest of the edit", async () => {
    const stored: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "sk-live-abc123" },
    };
    await openEditor(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Replace" }));
    expect(screen.getByText(/the stored key is removed/)).toBeTruthy();
    // An unrelated edit made while in the replace state must survive the take-back.
    fireEvent.change(screen.getByLabelText("Bank prefix"), { target: { value: "team-" } });
    fireEvent.click(screen.getByRole("button", { name: "Keep existing" }));

    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: stored }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.memory.api_key).toBe("sk-live-abc123");
    expect(sent.memory.bank_prefix).toBe("team-");
    // And the take-back never put the secret on screen.
    expect(document.body.innerHTML).not.toContain("sk-live-abc123");
  });

  // The offer only makes sense when there is something to keep.
  it("offers no take-back when no key is stored", async () => {
    const noKey: TeamsConfig = {
      ...config,
      memory: { ...config.memory, backend: "hindsight", api_key: "" },
    };
    await openEditor(view({ present: true, config: noKey }));
    expect(screen.queryByRole("button", { name: "Keep existing" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Replace" })).toBeNull();
  });

  it("picks a default teammate from the roster rather than asking for a typed name", async () => {
    const twoUp: TeamsConfig = {
      ...config,
      roster: [
        { name: "alice", profile: "swe", labels: [], bank: "", max_concurrent: 0 },
        { name: "bob", profile: "reviewer", labels: [], bank: "", max_concurrent: 0 },
      ],
    };
    await openEditor(view({ present: true, config: twoUp }));
    fireEvent.click(screen.getByRole("button", { name: "none" }));
    fireEvent.click(screen.getByRole("option", { name: "bob" }));
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config: twoUp }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).manager.default_identity).toBe("bob");
  });

  // Layout discipline: the common path stays the clean three-field row, and the per-row overrides
  // are one disclosure away rather than always on screen.
  it("keeps a teammate's bank and concurrency overrides behind a per-row disclosure", async () => {
    await openEditor(view({ present: true, config }));
    expect(screen.queryByLabelText("Teammate 1 bank")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Teammate 1 advanced" }));
    fireEvent.change(screen.getByLabelText("Teammate 1 bank"), { target: { value: "shared" } });
    h.saveTeamsConfig.mockResolvedValue(view({ present: true, config }));
    fireEvent.click(screen.getByRole("button", { name: "Save teams.yaml" }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    expect((h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig).roster[0].bank).toBe("shared");
  });

  // A fresh operator must not be handed the triage model or the prompt budget on open.
  it("hides the advanced fields until the disclosure is opened", async () => {
    await openEditor(view({ present: true, config }));
    expect(screen.queryByLabelText("Triage model")).toBeNull();
    expect(screen.queryByLabelText("Bank directory")).toBeNull();
    const advanced = screen.getAllByRole("button", { name: /Advanced/ });
    for (const a of advanced) {
      expect(a.getAttribute("aria-expanded")).toBe("false");
      fireEvent.click(a);
    }
    expect(screen.getByLabelText("Triage model")).toBeTruthy();
    expect(screen.getByLabelText("Bank directory")).toBeTruthy();
  });
});
