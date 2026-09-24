// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { GlobalConfigDTO, ProviderCatalogDTO, ProviderConfigDTO } from "@/lib/api";
import { ProviderConfigError } from "@/lib/api";
import { toUiGlobal } from "@/lib/settings-model";

const h = vi.hoisted(() => ({
  hasProviderBridge: vi.fn(() => false),
  providerStatuses: vi.fn(),
  providerPrepare: vi.fn(),
  providerRemove: vi.fn(),
  fetchProviderStatuses: vi.fn(),
  fetchProviderCatalog: vi.fn(),
  refreshProviderCatalog: vi.fn(),
  saveProviderConfig: vi.fn(),
}));

vi.mock("@/lib/provider-credentials", async (orig) => {
  const actual = await orig<typeof import("@/lib/provider-credentials")>();
  return {
    ...actual,
    hasProviderBridge: h.hasProviderBridge,
    providerStatuses: h.providerStatuses,
    providerPrepare: h.providerPrepare,
    providerRemove: h.providerRemove,
  };
});

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchProviderStatuses: h.fetchProviderStatuses,
    fetchProviderCatalog: h.fetchProviderCatalog,
    refreshProviderCatalog: h.refreshProviderCatalog,
    saveProviderConfig: h.saveProviderConfig,
  };
});

import { ProvidersTab } from "@/components/settings/ProvidersTab";

const PROVIDER: ProviderConfigDTO = {
  id: "fireworks",
  protocol: "openai-compatible",
  display_name: "Fireworks",
  base_url: "https://api.fireworks.ai/inference/v1",
  allow_insecure_http: false,
  credential: { source: "keychain" },
};

function makeGlobal(): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "e", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: {
      backend: "opencode",
      max_concurrent_agents: 8,
      max_turns: 20,
      max_retry_backoff_ms: 300000,
      provider: "fireworks",
      model: "accounts/fireworks/models/x",
    },
    claude: {
      command: "claude",
      model: "claude-sonnet-4-6",
      effort: "high",
      permission_mode: "acceptEdits",
      billing_guard: true,
      ultracode: false,
      turn_timeout_ms: 120000,
      read_timeout_ms: 0,
      stall_timeout_ms: 0,
      mcp_config: "",
    },
    workspace: { root: "/ws" },
    storage: { path: "/db", retention_days: 30 },
    otel: { enabled: false, endpoint: "", protocol: "grpc", service_name: "s", insecure: false },
    mcp: { enabled: true, allow_send_message: true, allow_stop: false, allow_resume: false },
    server: { port: 4317 },
    logging: { dir: "/logs" },
    repo: "",
    active_states: [],
    terminal_states: [],
    canceled_states: [],
    review_states: null,
    review_promote_state: "",
    summon_token: "",
    github_summons: false,
    milestone: "",
    labels: [],
    capabilities: [],
    prompt: "",
    prompt_file: "",
    git_flow: "",
    workspace_mode: "",
    dependency_mode: "",
    claim_mode: "",
    providers: { fireworks: PROVIDER },
  };
}

const CATALOG: ProviderCatalogDTO = {
  provider_id: "fireworks",
  models: [{ id: "accounts/fireworks/models/suggested" }],
  truncated: false,
  cache_age_ms: 1000,
  manual_entry_allowed: true,
};

function renderTab(onChange = vi.fn()) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return {
    onChange,
    ...render(
      <QueryClientProvider client={qc}>
        <ProvidersTab value={toUiGlobal(makeGlobal())} onChange={onChange} />
      </QueryClientProvider>,
    ),
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  h.hasProviderBridge.mockReturnValue(false);
});

beforeEach(() => {
  h.fetchProviderStatuses.mockResolvedValue([
    { provider_id: "fireworks", status: "configured", cache_age_ms: 42, refreshing: false, broker_available: true, recovery: null },
  ]);
  h.fetchProviderCatalog.mockResolvedValue(CATALOG);
});

describe("ProvidersTab", () => {
  // MUTATION GUARD: in a plain browser (no Tauri bridge) the tab must NOT render any credential
  // action. A UI that rendered Connect/Replace/Rebind/Remove without the bridge reds here.
  it("shows status but no credential action in browser-only mode", async () => {
    h.hasProviderBridge.mockReturnValue(false);
    renderTab();
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    expect(screen.getByText(/require the Rhapsody desktop app/i)).toBeTruthy();
    for (const name of ["Connect", "Replace key", "Rebind to this endpoint", "Remove", "Test connection"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });

  it("offers credential actions in the desktop app", async () => {
    h.hasProviderBridge.mockReturnValue(true);
    h.providerStatuses.mockResolvedValue([
      {
        provider_id: "fireworks",
        display_name: "Fireworks",
        endpoint: "https://api.fireworks.ai/inference/v1",
        adapter: "openai-chat-completions-bearer-v1",
        insecure_http: false,
        status: "absent",
        recovery: "connect",
        can_connect: true,
        can_replace: false,
        can_rebind: false,
        can_remove: false,
      },
    ]);
    renderTab();
    await waitFor(() => expect(screen.getByRole("button", { name: "Connect" })).toBeTruthy());
  });

  // MUTATION GUARD: after an endpoint/protocol edit the daemon reports unknown_refreshing, then
  // binding_mismatch. A view that kept showing "Connected" (or derived status from the config
  // definition) reds here.
  it("renders the feed's status through unknown_refreshing to binding_mismatch", async () => {
    h.fetchProviderStatuses.mockResolvedValueOnce([
      { provider_id: "fireworks", status: "unknown_refreshing", cache_age_ms: null, refreshing: true, broker_available: true, recovery: "refresh" },
    ]);
    const first = renderTab();
    await waitFor(() => expect(screen.getByText("Checking…")).toBeTruthy());
    expect(screen.queryByText("Connected")).toBeNull();
    first.unmount();

    h.fetchProviderStatuses.mockResolvedValue([
      { provider_id: "fireworks", status: "binding_mismatch", cache_age_ms: 5, refreshing: false, broker_available: true, recovery: "rebind" },
    ]);
    renderTab();
    await waitFor(() => expect(screen.getByText(/Endpoint changed/i)).toBeTruthy());
    expect(screen.queryByText("Connected")).toBeNull();
  });

  // MUTATION GUARD: a failed catalog must not disable manual model entry. A UI that disabled the
  // model field (or refused the value) when the catalog errored reds here.
  it("keeps manual model entry when the catalog fails", async () => {
    h.fetchProviderCatalog.mockResolvedValue({
      provider_id: "fireworks",
      models: [],
      truncated: false,
      cache_age_ms: 1,
      error: "timeout",
      error_message: "deadline exceeded",
      manual_entry_allowed: true,
    });
    const { onChange } = renderTab();
    const input = (await screen.findByLabelText("Global model")) as HTMLInputElement;
    // Await the failed-catalog state BEFORE asserting usability: reading `disabled` as soon as the
    // field first renders would pass even if the failed catalog then disabled it. The failure copy
    // only renders once the query has resolved with `error`, so this pins the real defect.
    expect(await screen.findByText(/Manual entry remains available/i)).toBeTruthy();
    expect(input.disabled).toBe(false);
    expect(input.value).toBe("accounts/fireworks/models/x");
    // Typing still reaches the parent after the failure — suggestions are not an allow-list.
    fireEvent.change(input, { target: { value: "accounts/fireworks/models/typed" } });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ agentModel: "accounts/fireworks/models/typed" }),
    );
  });

  // MUTATION GUARD (secrets): a status row carrying forbidden fields (a credential value, a stored
  // binding, a revision) must never surface any of them in the DOM.
  it("never renders a credential, binding, or revision from a status row", async () => {
    const canary = "sk-CANARY-secret";
    h.fetchProviderStatuses.mockResolvedValue([
      {
        provider_id: "fireworks",
        status: "configured",
        cache_age_ms: 1,
        refreshing: false,
        broker_available: true,
        recovery: null,
        // Hostile extras a real daemon would never send; the view must ignore them.
        credential: canary,
        binding_fingerprint: "binding-FFFF",
        revision: "rev-1234",
      } as never,
    ]);
    renderTab();
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    const text = document.body.textContent ?? "";
    expect(text).not.toContain(canary);
    expect(text).not.toContain("binding-FFFF");
    expect(text).not.toContain("rev-1234");
  });

  // ACCEPTANCE: definitions are non-secret configuration, so the browser dashboard can add/edit/remove
  // them (only the KEY actions are desktop-only).
  it("offers definition actions in browser-only mode", async () => {
    h.hasProviderBridge.mockReturnValue(false);
    renderTab();
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    expect(screen.getByRole("button", { name: "Add provider" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Edit fireworks" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove fireworks" })).toBeTruthy();
  });

  // ACCEPTANCE: a refused removal lists EVERY reference the daemon reported.
  it("refuses to remove a referenced provider and lists every reference", async () => {
    h.saveProviderConfig.mockRejectedValue(
      new ProviderConfigError("provider \"fireworks\" is still selected", "provider_in_use", [
        { kind: "global", label: "the global default (agent.provider)" },
        { kind: "manager", label: "the manager (manager.provider)" },
        { kind: "roster", label: "roster entry \"jerry\"" },
      ]),
    );
    renderTab();
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Remove fireworks" }));
    const confirm = await screen.findByRole("button", { name: "Remove provider" });
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.getByText(/the global default/)).toBeTruthy());
    const body = screen.getByRole("dialog").textContent ?? "";
    expect(body).toContain("the manager (manager.provider)");
    expect(body).toContain('roster entry "jerry"');
  });

  // ACCEPTANCE: an unreferenced removal goes straight to the daemon and succeeds.
  it("removes an unreferenced provider", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    const onDefinitionsChanged = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <ProvidersTab
          value={toUiGlobal(makeGlobal())}
          onChange={vi.fn()}
          onDefinitionsChanged={onDefinitionsChanged}
        />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Remove fireworks" }));
    fireEvent.click(await screen.findByRole("button", { name: "Remove provider" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith({
        op: "remove",
        provider_id: "fireworks",
      }),
    );
    await waitFor(() => expect(onDefinitionsChanged).toHaveBeenCalled());
  });

  // MUTATION GUARD: an unavailable broker renders the CLOSED reason code and nothing about the
  // broker's internals (a listener address, capability, or credential). A view that rendered a
  // raw address/capability here reds; one that hid the reason entirely reds the positive assert.
  it("renders the closed broker-unavailable reason without internals", async () => {
    h.fetchProviderStatuses.mockResolvedValue([
      {
        provider_id: "fireworks",
        status: "configured",
        cache_age_ms: 2,
        refreshing: false,
        broker_available: false,
        broker_reason: "provider_broker_unavailable",
        recovery: null,
        // Hostile extras a real daemon never sends; the view must ignore them.
        broker_address: "127.0.0.1:54321",
        capability: "cap-CANARY",
      } as never,
    ]);
    renderTab();
    await waitFor(() => expect(screen.getByText(/broker unavailable/i)).toBeTruthy());
    const text = document.body.textContent ?? "";
    expect(text).toContain("provider_broker_unavailable");
    expect(text).not.toContain("127.0.0.1:54321");
    expect(text).not.toContain("cap-CANARY");
  });

  // ACCEPTANCE (REVIEW B3): on desktop the removal dialog offers to delete the stored key, and the
  // key is removed BEFORE the definition (the desktop command derives the binding from the
  // definition, which must still exist).
  it("offers to remove the stored key on desktop and does so before the definition", async () => {
    h.hasProviderBridge.mockReturnValue(true);
    h.providerStatuses.mockResolvedValue([
      {
        provider_id: "fireworks",
        display_name: "Fireworks",
        endpoint: "https://api.fireworks.ai/inference/v1",
        adapter: "openai-chat-completions-bearer-v1",
        insecure_http: false,
        status: "configured",
        recovery: null,
        can_connect: false,
        can_replace: true,
        can_rebind: false,
        can_remove: true,
      },
    ]);
    h.providerPrepare.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      endpoint: "https://api.fireworks.ai/inference/v1",
      insecure_http: false,
      nonce: "nonce-1",
      expires_in_ms: 1000,
    });
    h.providerRemove.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      mutated: true,
      status: "absent",
      sync: "synchronized",
    });
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });

    renderTab();
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    // Wait for the desktop status feed so the card's can_remove is known before opening the dialog.
    await waitFor(() => expect(screen.getAllByText("Connected").length).toBeGreaterThan(0));
    fireEvent.click(screen.getByRole("button", { name: "Remove fireworks" }));
    fireEvent.click(await screen.findByLabelText("Also remove the stored key"));
    fireEvent.click(screen.getByRole("button", { name: "Remove provider" }));
    // The daemon's reference check runs FIRST as a `dry_run`; only then is the key removed, and only
    // then is the real definition removal posted.
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith({
        op: "remove",
        provider_id: "fireworks",
        dry_run: true,
      }),
    );
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith({
        op: "remove",
        provider_id: "fireworks",
      }),
    );
    expect(h.providerPrepare).toHaveBeenCalledWith("fireworks", "remove");
    expect(h.providerRemove).toHaveBeenCalledWith("fireworks", "nonce-1");
    const dryRun = h.saveProviderConfig.mock.invocationCallOrder[0];
    const keyRemoval = h.providerRemove.mock.invocationCallOrder[0];
    const realRemove = h.saveProviderConfig.mock.invocationCallOrder[1];
    expect(dryRun).toBeLessThan(keyRemoval);
    expect(keyRemoval).toBeLessThan(realRemove);
  });

  // REVIEW A1 (MUTATION GUARD): when the daemon REFUSES the definition removal, the stored key must
  // never be destroyed. Removing the credential before the `dry_run` pre-flight reds this — the
  // operator would orphan every run on a provider that is still selected.
  it("never removes the stored key when the daemon refuses the removal", async () => {
    h.hasProviderBridge.mockReturnValue(true);
    h.providerStatuses.mockResolvedValue([
      {
        provider_id: "fireworks",
        display_name: "Fireworks",
        endpoint: "https://api.fireworks.ai/inference/v1",
        adapter: "openai-chat-completions-bearer-v1",
        insecure_http: false,
        status: "configured",
        recovery: null,
        can_connect: false,
        can_replace: true,
        can_rebind: false,
        can_remove: true,
      },
    ]);
    h.providerPrepare.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      endpoint: "https://api.fireworks.ai/inference/v1",
      insecure_http: false,
      nonce: "nonce-1",
      expires_in_ms: 1000,
    });
    h.providerRemove.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      mutated: true,
      status: "absent",
      sync: "synchronized",
    });
    h.saveProviderConfig.mockRejectedValue(
      new ProviderConfigError("provider \"fireworks\" is still selected", "provider_in_use", [
        { kind: "global", label: "the global default (agent.provider)" },
      ]),
    );

    renderTab();
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    await waitFor(() => expect(screen.getAllByText("Connected").length).toBeGreaterThan(0));
    fireEvent.click(screen.getByRole("button", { name: "Remove fireworks" }));
    fireEvent.click(await screen.findByLabelText("Also remove the stored key"));
    fireEvent.click(screen.getByRole("button", { name: "Remove provider" }));
    // The refusal is surfaced, and neither the prepare nor the removal of the key ever ran.
    await waitFor(() => expect(screen.getByText(/the global default/)).toBeTruthy());
    expect(h.providerPrepare).not.toHaveBeenCalled();
    expect(h.providerRemove).not.toHaveBeenCalled();
  });

  // MUTATION GUARD: a browser has no Keychain, so the dialog must NOT offer to remove a stored key.
  it("never offers to remove a stored key in browser-only mode", async () => {
    h.hasProviderBridge.mockReturnValue(false);
    renderTab();
    await waitFor(() => expect(screen.getByTestId("provider-card-fireworks")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Remove fireworks" }));
    await screen.findByRole("button", { name: "Remove provider" });
    expect(screen.queryByLabelText("Also remove the stored key")).toBeNull();
  });
});
