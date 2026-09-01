// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { TeamsConfig, TeamsConfigView } from "@/lib/api";

// The Manage-team form (STUDIO-681 §7) — one test per acceptance box in §10, sub-ticket 5.
//
// These drive the real view against a mocked `/api/v1/teams/config` rather than the reveal
// helpers directly: what §7 promises is about the RENDERED form — a field that is *absent*, a
// model input that is *disabled*, a warn Note that *appears* — and none of those are claims a
// pure function can make. The rules themselves are pinned in `lib/console-manage.test.ts`.

const h = vi.hoisted(() => ({ fetchTeamsConfig: vi.fn(), saveTeamsConfig: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchTeamsConfig: h.fetchTeamsConfig, saveTeamsConfig: h.saveTeamsConfig };
});

const { ManageTeamView } = await import("./ManageTeamView");

function config(patch: Partial<TeamsConfig> = {}): TeamsConfig {
  return {
    enabled: true,
    manager: {
      mode: "labels+model",
      default_identity: "alice",
      model: "claude-opus-5",
      max_tokens: 4000,
      timeout_ms: 60000,
    },
    memory: {
      backend: "local",
      path: "",
      endpoint: "",
      api_key: "",
      bank_prefix: "agent-",
      recall_top_k: 8,
    },
    quorum: { enabled: false, reviewers: 2 },
    roster: [
      { name: "alice", profile: "swe", labels: ["rust"], bank: "", max_concurrent: 0 },
      { name: "jimmy", profile: "swe", labels: [], bank: "", max_concurrent: 0 },
    ],
    prompt_budget_bytes: 16000,
    ...patch,
  };
}

function view(patch: Partial<TeamsConfigView> = {}): TeamsConfigView {
  return {
    path: "/home/op/.rhapsody/teams.yaml",
    present: true,
    error: "",
    config: config(),
    restart_required: true,
    ...patch,
  };
}

const onNavigate = vi.fn();

function mount() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ManageTeamView onNavigate={onNavigate} />
    </QueryClientProvider>,
  );
}

/** Mount and wait for the fetched teams.yaml to have hydrated the form. */
async function ready() {
  mount();
  await waitFor(() => expect(screen.getByRole("textbox", { name: "Teammate 1 name" })).toHaveProperty("value", "alice"));
}

/** A Seg by its accessible name — the group, so its option buttons can be scoped to it. */
function seg(name: string) {
  return screen.getByRole("group", { name });
}

function press(group: string, option: string) {
  fireEvent.click(within(seg(group)).getByRole("button", { name: option }));
}

function nameField(n: number) {
  return screen.getByRole("textbox", { name: `Teammate ${n} name` });
}

function yamlText(): string {
  const el = document.querySelector(".yaml");
  return el?.textContent ?? "";
}

beforeEach(() => {
  onNavigate.mockReset();
  h.fetchTeamsConfig.mockReset().mockResolvedValue(view());
  h.saveTeamsConfig.mockReset().mockImplementation(async (c: TeamsConfig) => view({ config: c }));
});

afterEach(cleanup);

describe("5.1 — roster rows add/remove; each field is editable", () => {
  it("renders one row per roster entry, loaded from teams.yaml", async () => {
    await ready();
    expect(nameField(1)).toHaveProperty("value", "alice");
    expect(nameField(2)).toHaveProperty("value", "jimmy");
    expect(screen.queryByRole("textbox", { name: "Teammate 3 name" })).toBeNull();
  });

  it("adds a row", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: /Add teammate/ }));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Teammate 3 name" })).toHaveProperty("value", ""));
  });

  it("removes a row, and the rows below it renumber", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Remove teammate 1" }));
    await waitFor(() => expect(nameField(1)).toHaveProperty("value", "jimmy"));
    expect(screen.queryByRole("textbox", { name: "Teammate 2 name" })).toBeNull();
  });

  it("edits the name", async () => {
    await ready();
    fireEvent.change(nameField(1), { target: { value: "carol" } });
    await waitFor(() => expect(nameField(1)).toHaveProperty("value", "carol"));
  });

  it("edits the profile, and offers a profile the file already names", async () => {
    h.fetchTeamsConfig.mockResolvedValue(
      view({
        config: config({
          roster: [{ name: "alice", profile: "data-eng", labels: [], bank: "", max_concurrent: 0 }],
        }),
      }),
    );
    mount();
    const profile = await screen.findByRole("combobox", { name: "Teammate 1 profile" });
    // A custom profile survives being rendered: the Select carries it rather than silently
    // snapping the row to the first built-in and saving that.
    expect(profile).toHaveProperty("value", "data-eng");
    fireEvent.change(profile, { target: { value: "reviewer" } });
    await waitFor(() => expect(profile).toHaveProperty("value", "reviewer"));
  });

  it("edits the extra labels as chips", async () => {
    await ready();
    const labels = screen.getByRole("textbox", { name: "Teammate 1 labels" });
    fireEvent.change(labels, { target: { value: "config" } });
    fireEvent.keyDown(labels, { key: "Enter" });
    await waitFor(() => expect(screen.getByRole("button", { name: "Remove config" })).toBeTruthy());
    // The label loaded from the file is still there — adding one does not replace the row.
    expect(screen.getByRole("button", { name: "Remove rust" })).toBeTruthy();
  });

  it("edits max-concurrent, and explains what 0 means", async () => {
    await ready();
    const max = screen.getByRole("spinbutton", { name: "Teammate 1 max concurrent" });
    fireEvent.click(screen.getByRole("button", { name: "Increase Teammate 1 max concurrent" }));
    await waitFor(() => expect(max).toHaveProperty("value", "1"));
    // The note §7 asks for, spanning a `<b>` — so it is read off the callout rather than matched
    // as one text node.
    const notes = [...document.querySelectorAll(".note")].map((n) => n.textContent ?? "");
    expect(notes.some((t) => /max concurrent 0.*unlimited/i.test(t))).toBe(true);
  });
});

describe("5.2 — manager mode Seg switches; off disables the model field", () => {
  it("reflects the mode on disk", async () => {
    await ready();
    expect(within(seg("Mode")).getByRole("button", { name: "labels + model" }).getAttribute("aria-pressed")).toBe("true");
  });

  // The order is part of the design, not an accident of the schema array: §7 and the prototype
  // both print it cheapest-first with the opt-out last.
  it("renders the modes in the order §7 prints them", async () => {
    await ready();
    const labels = within(seg("Mode"))
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(labels).toEqual(["labels", "labels + model", "off"]);
  });

  it("switches mode", async () => {
    await ready();
    press("Mode", "labels");
    await waitFor(() =>
      expect(within(seg("Mode")).getByRole("button", { name: "labels" }).getAttribute("aria-pressed")).toBe("true"),
    );
  });

  it("disables the model field in off, and only in off", async () => {
    await ready();
    const model = screen.getByRole("textbox", { name: "Model" });
    expect(model).toHaveProperty("disabled", false);
    press("Mode", "off");
    await waitFor(() => expect(model).toHaveProperty("disabled", true));
    press("Mode", "labels");
    await waitFor(() => expect(model).toHaveProperty("disabled", false));
  });
});

describe("5.3 — a turn timeout below 15000 ms shows the starvation warn Note", () => {
  it("shows no warning at the shipped default", async () => {
    await ready();
    expect(screen.queryByText(/always times out/i)).toBeNull();
  });

  it("warns below the floor, naming the operator's own number", async () => {
    await ready();
    fireEvent.change(screen.getByRole("spinbutton", { name: "Turn timeout" }), { target: { value: "5000" } });
    const note = await screen.findByText(/always times out/i);
    expect(note.closest(".note")?.className).toContain("warn");
    expect(note.closest(".note")?.textContent).toContain("5000");
  });

  it("clears the warning back at the floor", async () => {
    await ready();
    const timeout = screen.getByRole("spinbutton", { name: "Turn timeout" });
    fireEvent.change(timeout, { target: { value: "5000" } });
    await screen.findByText(/always times out/i);
    fireEvent.change(timeout, { target: { value: "15000" } });
    await waitFor(() => expect(screen.queryByText(/always times out/i)).toBeNull());
  });

  // The daemon only warns in `labels+model` (Teams::starved_manager_timeout_ms): no other mode
  // runs a model turn, so in `labels` the sentence would simply not be true.
  it("does not warn in a mode that runs no model turn", async () => {
    await ready();
    fireEvent.change(screen.getByRole("spinbutton", { name: "Turn timeout" }), { target: { value: "5000" } });
    await screen.findByText(/always times out/i);
    press("Mode", "labels");
    await waitFor(() => expect(screen.queryByText(/always times out/i)).toBeNull());
  });
});

describe("5.4 — backend hindsight reveals endpoint + masked key; local/none hide them", () => {
  it("hides both for the local backend on disk", async () => {
    await ready();
    expect(screen.queryByRole("textbox", { name: "Hindsight endpoint" })).toBeNull();
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
  });

  it("reveals both for hindsight, and hides them again for none", async () => {
    await ready();
    press("Memory backend", "hindsight");
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Hindsight endpoint" })).toBeTruthy());
    expect(screen.getByRole("textbox", { name: "API key" })).toBeTruthy();
    press("Memory backend", "none");
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Hindsight endpoint" })).toBeNull());
    expect(screen.queryByRole("textbox", { name: "API key" })).toBeNull();
  });

  // A literal in teams.yaml is never rendered back: `toDraft` refuses to load it, so the field
  // shows a fixed-width mask that leaks not even a length, and Replace is the only way to change it.
  it("masks a key stored in the file rather than echoing it", async () => {
    h.fetchTeamsConfig.mockResolvedValue(
      view({
        config: config({
          memory: {
            backend: "hindsight",
            path: "",
            endpoint: "https://hindsight.example.ts.net/mcp/",
            api_key: "sk-live-do-not-echo",
            bank_prefix: "agent-",
            recall_top_k: 8,
          },
        }),
      }),
    );
    mount();
    const key = await screen.findByRole("textbox", { name: "API key" });
    expect(key).toHaveProperty("value", "••••••••••••");
    expect(document.body.textContent).not.toContain("sk-live-do-not-echo");
    expect(key.hasAttribute("readonly")).toBe(true);
  });

  // An env-var name is a POINTER, not a secret — it stays visible and editable.
  it("shows a $ENV_VAR reference as ordinary editable text", async () => {
    h.fetchTeamsConfig.mockResolvedValue(
      view({
        config: config({
          memory: {
            backend: "hindsight",
            path: "",
            endpoint: "",
            api_key: "$HINDSIGHT_API_KEY",
            bank_prefix: "agent-",
            recall_top_k: 8,
          },
        }),
      }),
    );
    mount();
    const key = await screen.findByRole("textbox", { name: "API key" });
    expect(key).toHaveProperty("value", "$HINDSIGHT_API_KEY");
    expect(key.hasAttribute("readonly")).toBe(false);
  });
});

describe("5.5 — the quorum toggle and reviewers stepper reflect and edit config", () => {
  it("reflects an enabled quorum and its reviewer count", async () => {
    h.fetchTeamsConfig.mockResolvedValue(view({ config: config({ quorum: { enabled: true, reviewers: 3 } }) }));
    mount();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Review quorum" }).getAttribute("aria-pressed")).toBe("true"),
    );
    expect(screen.getByRole("spinbutton", { name: "Reviewers" })).toHaveProperty("value", "3");
  });

  it("reflects the off state teams.yaml ships with", async () => {
    await ready();
    expect(screen.getByRole("button", { name: "Review quorum" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("edits both, and the edit reaches the saved config", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Review quorum" }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Review quorum" }).getAttribute("aria-pressed")).toBe("true"),
    );
    fireEvent.change(screen.getByRole("spinbutton", { name: "Reviewers" }), { target: { value: "3" } });
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalled());
    expect(h.saveTeamsConfig.mock.calls[0][0].quorum).toEqual({ enabled: true, reviewers: 3 });
  });
});

describe("5.6 — View as YAML renders a teams.yaml consistent with the form", () => {
  it("is hidden until asked for", async () => {
    await ready();
    expect(document.querySelector(".yaml")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /View as YAML/ }));
    await waitFor(() => expect(document.querySelector(".yaml")).toBeTruthy());
  });

  it("renders the values currently in the form, not the ones on disk", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: /View as YAML/ }));
    await waitFor(() => expect(yamlText()).toContain("- name: alice"));
    expect(yamlText()).toContain("mode: labels+model");

    press("Mode", "labels");
    fireEvent.change(nameField(2), { target: { value: "carol" } });
    await waitFor(() => expect(yamlText()).toContain("mode: labels"));
    expect(yamlText()).toContain("- name: carol");
    expect(yamlText()).not.toContain("- name: jimmy");
  });

  it("follows a roster row being added and removed", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: /View as YAML/ }));
    await waitFor(() => expect(yamlText()).toContain("- name: jimmy"));
    fireEvent.click(screen.getByRole("button", { name: "Remove teammate 2" }));
    await waitFor(() => expect(yamlText()).not.toContain("- name: jimmy"));
  });

  // The preview is the file, so the one value it must never render verbatim is the same one the
  // form masks.
  it("masks a stored key in the preview too", async () => {
    h.fetchTeamsConfig.mockResolvedValue(
      view({
        config: config({
          memory: {
            backend: "hindsight",
            path: "",
            endpoint: "https://hindsight.example.ts.net/mcp/",
            api_key: "sk-live-do-not-echo",
            bank_prefix: "agent-",
            recall_top_k: 8,
          },
        }),
      }),
    );
    mount();
    await screen.findByRole("textbox", { name: "API key" });
    fireEvent.click(screen.getByRole("button", { name: /View as YAML/ }));
    await waitFor(() => expect(yamlText()).toContain("backend: hindsight"));
    expect(yamlText()).toContain("api_key: ••••••••••••");
    expect(yamlText()).not.toContain("sk-live-do-not-echo");
  });
});

describe("5.7 — Save posts to /teams/config and states that changes apply on restart", () => {
  // The endpoint itself, not just "the client function was called": box 5.7 names the route, and
  // nothing else in the suite pins it. The real `saveTeamsConfig` runs here against a stubbed
  // fetch, so a change to the URL, the method or the `{config}` envelope turns this red.
  it("POSTs the {config} envelope to /api/v1/teams/config", async () => {
    const realApi = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
    h.saveTeamsConfig.mockImplementation(realApi.saveTeamsConfig);
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(view()), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    try {
      await ready();
      fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
      await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
      const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
      expect(url).toBe("/api/v1/teams/config");
      expect(init.method).toBe("POST");
      expect(JSON.parse(String(init.body)).config.roster.map((r: { name: string }) => r.name)).toEqual([
        "alice",
        "jimmy",
      ]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("posts the form's config", async () => {
    await ready();
    fireEvent.change(nameField(1), { target: { value: "carol" } });
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalledTimes(1));
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.roster.map((r) => r.name)).toEqual(["carol", "jimmy"]);
    expect(sent.manager.mode).toBe("labels+model");
  });

  it("says restart to apply before the save, and after it — never that it is live", async () => {
    await ready();
    expect(screen.getByText(/restart to apply/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    const saved = await screen.findByRole("status");
    expect(saved.textContent).toMatch(/restart/i);
    // The claim the box forbids: nothing may say the running daemon has taken the change.
    expect(saved.textContent).not.toMatch(/\b(now (live|active)|applied|in effect)\b/i);
  });

  // `restart_required` is the daemon's field, not an assumption: the day teams.yaml hot-reloads,
  // the note has to disappear by itself rather than keep telling the operator to restart.
  it("drops the restart copy when the daemon says it is not required", async () => {
    h.fetchTeamsConfig.mockResolvedValue(view({ restart_required: false }));
    h.saveTeamsConfig.mockImplementation(async (c: TeamsConfig) => view({ config: c, restart_required: false }));
    await ready();
    expect(screen.queryByText(/restart to apply/i)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    const saved = await screen.findByRole("status");
    expect(saved.textContent).not.toMatch(/restart/i);
  });

  it("surfaces the daemon's own rejection verbatim and leaves the form as it was", async () => {
    await ready();
    h.saveTeamsConfig.mockRejectedValue(new Error('roster name "Alice" is not label-safe'));
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain('roster name "Alice" is not label-safe'));
    expect(nameField(1)).toHaveProperty("value", "alice");
  });

  it("does not post a roster the daemon would reject", async () => {
    await ready();
    fireEvent.change(nameField(1), { target: { value: "Alice" } });
    await waitFor(() => expect(screen.getByRole("button", { name: /Save changes/ })).toHaveProperty("disabled", true));
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    expect(h.saveTeamsConfig).not.toHaveBeenCalled();
  });

  it("cancels back to the Teams console without saving", async () => {
    await ready();
    fireEvent.change(nameField(1), { target: { value: "carol" } });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onNavigate).toHaveBeenCalledWith("teams");
    expect(h.saveTeamsConfig).not.toHaveBeenCalled();
  });
});

describe("the view's own edges", () => {
  it("reports a daemon with nowhere to keep a teams.yaml rather than rendering a doomed form", async () => {
    h.fetchTeamsConfig.mockRejectedValue(new Error("this daemon has no on-disk runtime home"));
    mount();
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain("this daemon has no on-disk runtime home"),
    );
    expect(screen.queryByRole("button", { name: /Save changes/ })).toBeNull();
  });

  it("crumbs back to the Teams console", async () => {
    await ready();
    fireEvent.click(screen.getByRole("button", { name: "Teams" }));
    expect(onNavigate).toHaveBeenCalledWith("teams");
  });

  it("edits the prompt budget under Advanced", async () => {
    await ready();
    const budget = screen.getByRole("spinbutton", { name: "Prompt budget" });
    expect(budget).toHaveProperty("value", "16000");
    fireEvent.change(budget, { target: { value: "20000" } });
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalled());
    expect(h.saveTeamsConfig.mock.calls[0][0].prompt_budget_bytes).toBe(20000);
  });

  it("edits recall top-k and the default identity", async () => {
    await ready();
    fireEvent.change(screen.getByRole("spinbutton", { name: "Recall top-k" }), { target: { value: "4" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Default identity" }), { target: { value: "jimmy" } });
    fireEvent.click(screen.getByRole("button", { name: /Save changes/ }));
    await waitFor(() => expect(h.saveTeamsConfig).toHaveBeenCalled());
    const sent = h.saveTeamsConfig.mock.calls[0][0] as TeamsConfig;
    expect(sent.memory.recall_top_k).toBe(4);
    expect(sent.manager.default_identity).toBe("jimmy");
  });
});
