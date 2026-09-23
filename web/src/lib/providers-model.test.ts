import { describe, expect, it } from "vitest";
import type { GlobalConfigDTO, ProviderCatalogDTO, ProviderConfigDTO } from "@/lib/api";
import {
  MODEL_ID_MAX_BYTES,
  cacheAgeLabel,
  catalogFailed,
  catalogOptions,
  credentialStatusLabel,
  credentialTone,
  globalSelectionFeedback,
  isCredentialBlocked,
  manualEntryAllowed,
  providerRegistry,
  providerSelectionValid,
  providerViews,
  recoveryHint,
  selectionCompatibility,
} from "@/lib/providers-model";

function def(overrides: Partial<ProviderConfigDTO> = {}): ProviderConfigDTO {
  return {
    id: "fireworks",
    protocol: "openai-compatible",
    display_name: "Fireworks",
    base_url: "https://api.fireworks.ai/inference/v1",
    allow_insecure_http: false,
    credential: { source: "keychain" },
    ...overrides,
  };
}

function globalConfig(overrides: Partial<GlobalConfigDTO> = {}): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: {
      backend: "opencode",
      max_concurrent_agents: 3,
      max_turns: 60,
      max_retry_backoff_ms: 300000,
      provider: "fireworks",
      model: "accounts/fireworks/models/x",
    },
    claude: {
      command: "claude",
      model: "claude-opus-5",
      effort: "high",
      permission_mode: "acceptEdits",
      billing_guard: true,
      ultracode: false,
      turn_timeout_ms: 3600000,
      read_timeout_ms: 60000,
      stall_timeout_ms: 0,
      mcp_config: "",
    },
    workspace: { root: "~/w" },
    storage: { path: "~/db", retention_days: 30 },
    otel: {
      enabled: false,
      endpoint: "",
      protocol: "",
      service_name: "",
      insecure: false,
    },
    mcp: { enabled: true, allow_send_message: true, allow_stop: false, allow_resume: false },
    server: { port: 8799 },
    logging: { dir: "~/l" },
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
    providers: { fireworks: def() },
    ...overrides,
  };
}

describe("providerRegistry", () => {
  it("returns providers in id order and [] when none", () => {
    expect(providerRegistry(undefined)).toEqual([]);
    expect(providerRegistry({})).toEqual([]);
    const list = providerRegistry({
      zeta: def({ id: "zeta" }),
      alpha: def({ id: "alpha" }),
    });
    expect(list.map((p) => p.id)).toEqual(["alpha", "zeta"]);
  });
});

describe("providerViews", () => {
  it("merges config metadata and never derives status from the definition", () => {
    const [view] = providerViews([def()], [
      { provider_id: "fireworks", status: "unknown_refreshing", cache_age_ms: null, refreshing: true },
    ]);
    expect(view.display_name).toBe("Fireworks");
    expect(view.endpoint).toBe("https://api.fireworks.ai/inference/v1");
    expect(view.status).toBe("unknown_refreshing");
    expect(view.cache_age_ms).toBeNull();
  });

  // MUTATION GUARD: after an endpoint/protocol edit the daemon reports unknown_refreshing then
  // binding_mismatch. The view must report exactly the feed's status; a view that synthesised
  // "configured" from the config definition (or cached the old status) reds here.
  it("reflects the status feed, never a synthesised configured", () => {
    const registry = [def()];
    const refreshing = providerViews(registry, [
      { provider_id: "fireworks", status: "unknown_refreshing", cache_age_ms: null, refreshing: true },
    ]);
    expect(refreshing[0].status).toBe("unknown_refreshing");
    const mismatch = providerViews(registry, [
      { provider_id: "fireworks", status: "binding_mismatch", cache_age_ms: 12, recovery: "rebind" },
    ]);
    expect(mismatch[0].status).toBe("binding_mismatch");
    expect(credentialStatusLabel(mismatch[0].status)).not.toMatch(/connected/i);
  });

  it("treats a configured provider with no status row as unknown, never absent", () => {
    const [view] = providerViews([def()], []);
    expect(view.status).toBe("unknown_refreshing");
    expect(view.cache_age_ms).toBeNull();
    expect(credentialStatusLabel(view.status)).not.toMatch(/not connected/i);
  });

  it("keeps a status whose definition is absent visible with a minimal card", () => {
    const views = providerViews([], [
      { provider_id: "projp", status: "configured", endpoint: "https://p.example/v1" },
    ]);
    expect(views).toHaveLength(1);
    expect(views[0].provider_id).toBe("projp");
    expect(views[0].status).toBe("configured");
    expect(views[0].endpoint).toBe("https://p.example/v1");
  });

  it("surfaces a plaintext HTTP endpoint and the desktop action flags", () => {
    const [view] = providerViews([def({ allow_insecure_http: true, base_url: "http://x/v1" })], [
      {
        provider_id: "fireworks",
        status: "binding_mismatch",
        can_rebind: true,
        can_remove: true,
      },
    ]);
    expect(view.insecure_http).toBe(true);
    expect(view.can_rebind).toBe(true);
    expect(view.can_remove).toBe(true);
  });
});

describe("credential status presentation", () => {
  it("names every closed status and assigns a tone", () => {
    const cases: [string, ReturnType<typeof credentialTone>, boolean][] = [
      ["configured", "ok", false],
      ["absent", "warn", false],
      ["denied_or_locked", "blocked", true],
      ["malformed", "blocked", true],
      ["binding_mismatch", "blocked", true],
      ["owner_unavailable", "blocked", true],
      ["owner_unauthorized", "blocked", true],
      ["unknown_refreshing", "unknown", false],
    ];
    for (const [status, tone, blocked] of cases) {
      expect(credentialTone(status)).toBe(tone);
      expect(isCredentialBlocked(status)).toBe(blocked);
      // Every member of the closed set gets a distinct, non-raw label.
      expect(credentialStatusLabel(status)).not.toBe(status);
    }
    expect(credentialStatusLabel("unknown_refreshing")).not.toMatch(/not connected/i);
  });

  it("maps the closed recovery actions to actionable sentences", () => {
    for (const action of ["connect", "unlock", "remove", "open_desktop", "rebind", "refresh"]) {
      expect(recoveryHint(action)).toBeTruthy();
    }
    expect(recoveryHint(null)).toBeNull();
    expect(recoveryHint("nonsense")).toBeNull();
  });

  it("labels cache age, treating null as never-read rather than fresh", () => {
    expect(cacheAgeLabel(null)).toBe("not yet read");
    expect(cacheAgeLabel(undefined)).toBe("not yet read");
    expect(cacheAgeLabel(500)).toBe("just now");
    expect(cacheAgeLabel(5000)).toBe("5s ago");
    expect(cacheAgeLabel(120000)).toBe("2m ago");
    expect(cacheAgeLabel(7200000)).toBe("2h ago");
  });
});

describe("model catalogs", () => {
  it("builds suggestions from a catalog and allows manual entry", () => {
    const catalog: ProviderCatalogDTO = {
      provider_id: "fireworks",
      models: [
        { id: "accounts/fireworks/models/x", display_name: "X" },
        { id: "accounts/fireworks/models/y" },
      ],
      truncated: false,
      cache_age_ms: 1,
      manual_entry_allowed: true,
    };
    expect(catalogOptions(catalog)).toEqual([
      { value: "accounts/fireworks/models/x", label: "X" },
      { value: "accounts/fireworks/models/y", label: "accounts/fireworks/models/y" },
    ]);
    expect(manualEntryAllowed(catalog)).toBe(true);
    expect(catalogFailed(catalog)).toBe(false);
  });

  // MUTATION GUARD: a failed catalog must NEVER invalidate manual model input. A UI that keyed the
  // model field's enabled state off catalog success reds here.
  it("keeps manual entry available when the catalog failed or is absent", () => {
    const failed: ProviderCatalogDTO = {
      provider_id: "fireworks",
      models: [],
      truncated: false,
      cache_age_ms: 1,
      error: "timeout",
      error_message: "deadline",
      manual_entry_allowed: true,
    };
    expect(catalogFailed(failed)).toBe(true);
    expect(manualEntryAllowed(failed)).toBe(true);
    expect(manualEntryAllowed(null)).toBe(true);
    expect(manualEntryAllowed(undefined)).toBe(true);
  });
});

describe("selectionCompatibility", () => {
  it("accepts an unset selection and a valid opencode selection", () => {
    expect(
      selectionCompatibility({ backend: "claude", providers: [], provider: "", model: "" }).ok,
    ).toBe(true);
    expect(
      selectionCompatibility({
        backend: "opencode",
        providers: [def()],
        provider: "fireworks",
        model: "accounts/fireworks/models/x",
      }),
    ).toEqual({ ok: true, reason: null });
  });

  it("refuses an explicit provider on a non-opencode backend", () => {
    const v = selectionCompatibility({
      backend: "claude",
      providers: [def()],
      provider: "fireworks",
      model: "m",
    });
    expect(v.ok).toBe(false);
    expect(v.reason).toMatch(/only opencode/);
  });

  it("refuses an unknown provider and a missing model", () => {
    const unknown = selectionCompatibility({
      backend: "opencode",
      providers: [def()],
      provider: "nope",
      model: "m",
    });
    expect(unknown.ok).toBe(false);
    expect(unknown.reason).toMatch(/not defined/);
    const noModel = selectionCompatibility({
      backend: "opencode",
      providers: [def()],
      provider: "fireworks",
      model: "",
    });
    expect(noModel.ok).toBe(false);
    expect(noModel.reason).toMatch(/requires an exact model/);
  });

  it("refuses a whitespace-wrapped, over-long, or control-bearing model", () => {
    expect(
      selectionCompatibility({ backend: "opencode", providers: [def()], provider: "fireworks", model: " m " })
        .reason,
    ).toMatch(/whitespace/);
    expect(
      selectionCompatibility({
        backend: "opencode",
        providers: [def()],
        provider: "fireworks",
        model: "a".repeat(MODEL_ID_MAX_BYTES + 1),
      }).reason,
    ).toMatch(/at most/);
    expect(
      selectionCompatibility({
        backend: "opencode",
        providers: [def()],
        provider: "fireworks",
        model: "bad\u0000model",
      }).reason,
    ).toMatch(/control/);
  });

  it("measures the model bound in UTF-8 bytes, not UTF-16 code units", () => {
    // `é` is one UTF-16 code unit but two UTF-8 bytes, so 300 of them are legal by `.length` (300)
    // yet over the 512-byte daemon bound — this must be refused locally rather than 400'd by the
    // daemon. 256 of them (512 bytes) is exactly at the bound and stays legal.
    const over = selectionCompatibility({
      backend: "opencode",
      providers: [def()],
      provider: "fireworks",
      model: "é".repeat(300),
    });
    expect(over.ok).toBe(false);
    expect(over.reason).toMatch(/at most/);

    const atBound = selectionCompatibility({
      backend: "opencode",
      providers: [def()],
      provider: "fireworks",
      model: "é".repeat(256),
    });
    expect(atBound.ok).toBe(true);
  });

  it("validates a provider-less model against transport bounds", () => {
    const v = selectionCompatibility({
      backend: "claude",
      providers: [],
      provider: "",
      model: "a".repeat(MODEL_ID_MAX_BYTES + 1),
    });
    expect(v.ok).toBe(false);
  });

  it("derives the whole-config verdict", () => {
    const good = globalConfig();
    expect(providerSelectionValid(good)).toBe(true);
    expect(globalSelectionFeedback(good).ok).toBe(true);
    const bad = globalConfig({ agent: { ...good.agent, provider: "nope" } });
    expect(providerSelectionValid(bad)).toBe(false);
    expect(globalSelectionFeedback(bad).reason).toMatch(/not defined/);
  });
});
