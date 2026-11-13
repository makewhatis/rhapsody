// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const h = vi.hoisted(() => ({
  fetchTeamsModels: vi.fn(),
  applyTeamsModels: vi.fn(),
  fetchProviderCatalog: vi.fn(),
}));
vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchTeamsModels: h.fetchTeamsModels,
    applyTeamsModels: h.applyTeamsModels,
    fetchProviderCatalog: h.fetchProviderCatalog,
  };
});
import { ModelsTab } from "@/components/settings/ModelsTab";
import type { TeamsModelsView } from "@/lib/api";

function view(): TeamsModelsView {
  return {
    identities: [
      {
        name: "jerry",
        profile: "swe-jr",
        resolved: {
          harness: "opencode",
          harness_origin: "profile",
          provider: "",
          provider_origin: "",
          model: "deepseek-v4p1-flash",
          model_origin: "profile",
        },
        configured: { harness: "opencode", provider: "", model: "deepseek-v4p1-flash", effort: "high" },
        identity: { harness: "", provider: "", model: "", effort: "" },
        code_reviewer: false,
      },
    ],
    review: [{ harness: "opencode", model: "", provider: "", effort: "" }],
    manager: {
      resolved: {
        harness: "claude",
        harness_origin: "default",
        provider: "",
        provider_origin: "",
        model: "claude-opus-5",
        model_origin: "manager",
      },
      configured: { harness: "", provider: "", model: "claude-opus-5", effort: "high" },
    },
    harnesses: ["claude", "opencode"],
    providers: ["fireworks"],
  };
}

function renderTab(v: TeamsModelsView = view()) {
  h.fetchTeamsModels.mockResolvedValue(v);
  h.applyTeamsModels.mockResolvedValue({ view: v, previous: [{ name: "jerry", routing: v.identities[0].configured }] });
  h.fetchProviderCatalog.mockResolvedValue({ provider_id: "fireworks", models: [], truncated: false, cache_age_ms: 0, manual_entry_allowed: true });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ModelsTab />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("ModelsTab", () => {
  it("shows the resolved tuple and the editable row per teammate", async () => {
    renderTab();
    expect(await screen.findByText("jerry")).toBeTruthy();
    // The resolved tuple is rendered with origins.
    expect(screen.getByText("deepseek-v4p1-flash")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Save jerry" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Save manager" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Move all workers" })).toBeTruthy();
    expect(h.applyTeamsModels).not.toHaveBeenCalled();
  });

  it("saves one teammate's profile routing", async () => {
    renderTab();
    fireEvent.click(await screen.findByRole("button", { name: "Save jerry" }));
    await waitFor(() => expect(h.applyTeamsModels).toHaveBeenCalledTimes(1));
    expect(h.applyTeamsModels.mock.calls[0][0]).toMatchObject({
      target: "identity",
      name: "jerry",
      routing: { harness: "opencode", model: "deepseek-v4p1-flash" },
    });
  });

  // B7: the model field suggests the harness's OWN known models with no provider configured — the
  // native-login case — instead of asking the provider catalog for the literal harness id.
  it("suggests harness-known models when no provider is set", async () => {
    renderTab();
    await screen.findByText("jerry");
    const values = Array.from(document.querySelectorAll("datalist option")).map(
      (o) => (o as HTMLOptionElement).value,
    );
    expect(values).toContain("openai/gpt-6.1-sol");
    expect(values).toContain("anthropic/claude-sonnet-4-6");
    // No provider is selected on any default row, so no catalog request is made.
    expect(h.fetchProviderCatalog).not.toHaveBeenCalled();
  });

  it("move-all then undo sends the previous tuples back", async () => {    renderTab();
    const moveAll = await screen.findByRole("button", { name: "Move all workers" });
    // A move-all needs a harness or model; the default draft is empty so the button is disabled.
    expect((moveAll as HTMLButtonElement).disabled).toBe(true);
    // Pick a harness through the custom Select.
    const harnessTriggers = screen.getAllByText("inherit");
    fireEvent.click(harnessTriggers[0]);
    fireEvent.click(await screen.findByRole("option", { name: "opencode" }));
    await waitFor(() => expect((screen.getByRole("button", { name: "Move all workers" }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole("button", { name: "Move all workers" }));
    await waitFor(() => expect(h.applyTeamsModels).toHaveBeenCalledWith(expect.objectContaining({ target: "move_all" })));
    // The undo control appears and sends the previous tuples back.
    const undo = await screen.findByRole("button", { name: /Undo move-all/ });
    fireEvent.click(undo);
    await waitFor(() =>
      expect(h.applyTeamsModels).toHaveBeenCalledWith(
        expect.objectContaining({ target: "undo", previous: [{ name: "jerry", routing: expect.anything() }] }),
      ),
    );
  });
});
