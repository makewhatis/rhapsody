// providers-model — the pure logic behind the Settings Providers surface (STUDIO-992, design §P11).
//
// Kept in a DOM-free .ts module (like settings-model/teams-model) so the rules that matter are
// unit-testable in the node-environment Vitest setup. The component stays a thin shell over these.
//
// Two hard rules this module exists to keep honest:
//   1. Credential status is read from the daemon's status feed — it is NEVER derived from the config
//      definition. After an endpoint/protocol edit the daemon reports `unknown_refreshing` and then
//      `binding_mismatch`; a view that kept showing "configured" would be the defect the ticket names.
//   2. Manual model entry is always available. A failed or absent catalog adds no suggestions and
//      blocks nothing.

import type { GlobalConfigDTO, ProviderCatalogDTO, ProviderConfigDTO } from "@/lib/api";
import type { SelectOption } from "@/components/ui/select";

/** The transport bound the daemon enforces on a model id (`rhapsody_config::providers`,
 *  MODEL_ID_MAX_BYTES). Mirrored here so the UI refuses to POST what the daemon will reject. */
export const MODEL_ID_MAX_BYTES = 512;

/** UTF-8 byte length, matching the daemon's `str::len()`. `String.prototype.length` counts UTF-16
 *  code units, so a multibyte id could pass the local gate and then be 400'd by the daemon. */
const utf8Len = (s: string): number => new TextEncoder().encode(s).length;

/** One provider status row, from EITHER source: the daemon's HTTP `ProviderStatusView` or the desktop
 *  bridge's richer `ProviderStatus` DTO. Both structure-typed inputs satisfy the fields read here. */
export interface ProviderStatusInput {
  provider_id: string;
  status: string;
  recovery?: string | null;
  cache_age_ms?: number | null;
  refreshing?: boolean;
  broker_available?: boolean;
  /** The closed reason the broker is unavailable, when it is (`provider_broker_unavailable`). */
  broker_reason?: string | null;
  display_name?: string;
  endpoint?: string;
  insecure_http?: boolean;
  /** Present only on the desktop bridge DTO; carried through for the credential form. */
  adapter?: string;
  can_connect?: boolean;
  can_replace?: boolean;
  can_rebind?: boolean;
  can_remove?: boolean;
}

/** One rendered provider card: the config definition merged with whatever status was observed. */
export interface ProviderView {
  provider_id: string;
  display_name: string;
  endpoint: string;
  protocol: string;
  insecure_http: boolean;
  credential_source: string;
  status: string;
  recovery: string | null;
  cache_age_ms: number | null;
  refreshing: boolean;
  broker_available: boolean;
  broker_reason: string | null;
  can_connect: boolean;
  can_replace: boolean;
  can_rebind: boolean;
  can_remove: boolean;
}

/** The registry as an ordered list (provider-id order), or [] when the config defines none. */
export function providerRegistry(
  providers: Record<string, ProviderConfigDTO> | undefined,
): ProviderConfigDTO[] {
  if (!providers) return [];
  return Object.values(providers).sort((a, b) => a.id.localeCompare(b.id));
}

function viewFromStatus(s: ProviderStatusInput): ProviderView {
  return {
    provider_id: s.provider_id,
    display_name: s.display_name?.trim() || s.provider_id,
    endpoint: s.endpoint ?? "",
    protocol: "",
    insecure_http: Boolean(s.insecure_http),
    credential_source: "",
    status: s.status,
    recovery: s.recovery ?? null,
    cache_age_ms: s.cache_age_ms ?? null,
    refreshing: Boolean(s.refreshing),
    broker_available: s.broker_available ?? true,
    broker_reason: s.broker_reason ?? null,
    can_connect: Boolean(s.can_connect),
    can_replace: Boolean(s.can_replace),
    can_rebind: Boolean(s.can_rebind),
    can_remove: Boolean(s.can_remove),
  };
}

// providerViews merges the configured registry with the observed statuses. A status with no matching
// definition (a per-project-only provider) still renders a minimal card so it is never invisible.
// The STATUS always comes from the feed; only display metadata comes from the definition.
export function providerViews(
  registry: ProviderConfigDTO[],
  statuses: ProviderStatusInput[],
): ProviderView[] {
  const byId = new Map(statuses.map((s) => [s.provider_id, s]));
  const seen = new Set<string>();
  const out: ProviderView[] = [];
  for (const def of registry) {
    seen.add(def.id);
    const s = byId.get(def.id);
    // A configured provider with NO status row has not been read yet (the feed failed, or the cache
    // has not published): report `unknown_refreshing`, never "absent", so an unknown state is not
    // misreported as a missing credential.
    const base = s
      ? viewFromStatus(s)
      : viewFromStatus({ provider_id: def.id, status: "unknown_refreshing", cache_age_ms: null });
    out.push({
      ...base,
      display_name: base.display_name === def.id ? def.display_name.trim() || def.id : base.display_name,
      endpoint: base.endpoint || def.base_url,
      protocol: def.protocol,
      insecure_http: base.insecure_http || def.allow_insecure_http,
      credential_source: def.credential?.source ?? "",
    });
  }
  for (const s of statuses) {
    if (seen.has(s.provider_id)) continue;
    seen.add(s.provider_id);
    out.push(viewFromStatus(s));
  }
  return out;
}

// ------------------------------------------------------------------------------------------------
// Credential status presentation
// ------------------------------------------------------------------------------------------------

/** The visual tone of a credential status: usable, attention, blocked (needs desktop recovery), or
 *  still being determined. `unknown_refreshing` is deliberately its own tone — never "absent". */
export type CredentialTone = "ok" | "warn" | "blocked" | "unknown";

export function credentialTone(status: string): CredentialTone {
  switch (status) {
    case "configured":
      return "ok";
    case "binding_mismatch":
    case "denied_or_locked":
    case "malformed":
    case "owner_unavailable":
    case "owner_unauthorized":
      return "blocked";
    case "unknown_refreshing":
      return "unknown";
    default:
      return "warn";
  }
}

/** Whether the status is a blocked state that only an explicit desktop recovery can clear. */
export function isCredentialBlocked(status: string): boolean {
  return credentialTone(status) === "blocked";
}

/** Whether the provider is known to hold a STORED key (STUDIO-1048). A change to its base URL or
 *  protocol then needs an explicit desktop Rebind. `absent` plainly has none, and
 *  `unknown_refreshing` is unknown — neither is treated as "has a key". */
export function hasStoredKey(status: string): boolean {
  return status !== "" && status !== "absent" && status !== "unknown_refreshing";
}

/** The human label for a credential status. Every member of the daemon's closed set is named. */
export function credentialStatusLabel(status: string): string {
  switch (status) {
    case "configured":
      return "Connected";
    case "absent":
      return "Not connected";
    case "denied_or_locked":
      return "Keychain locked or denied";
    case "malformed":
      return "Unreadable credential";
    case "binding_mismatch":
      return "Endpoint changed — rebind required";
    case "owner_unavailable":
      return "Desktop credential owner unavailable";
    case "owner_unauthorized":
      return "Desktop credential owner rejected the daemon";
    case "unknown_refreshing":
      return "Checking…";
    default:
      return status;
  }
}

/** The actionable sentence for the daemon's one closed recovery action, or null. */
export function recoveryHint(recovery: string | null | undefined): string | null {
  switch (recovery) {
    case "connect":
      return "Connect a credential to use this provider.";
    case "unlock":
      return "Unlock the login keychain, then retry.";
    case "remove":
      return "Remove the unreadable item, then connect again.";
    case "open_desktop":
      return "Open the Rhapsody desktop app to restore credential ownership.";
    case "rebind":
      return "Only the desktop app can rebind this credential to the new endpoint.";
    case "refresh":
      return "Status is being re-read; check again shortly.";
    default:
      return null;
  }
}

/** A short age label for a status/catalog cache read. `null` means "never published" (unknown). */
export function cacheAgeLabel(ms: number | null | undefined): string {
  if (ms == null) return "not yet read";
  if (ms < 1000) return "just now";
  const sec = Math.floor(ms / 1000);
  if (sec < 60) return `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  return `${Math.floor(min / 60)}h ago`;
}

// ------------------------------------------------------------------------------------------------
// Model catalogs — suggestions are ADDITIVE; manual entry is first-class
// ------------------------------------------------------------------------------------------------

/** The model suggestions for a provider. A failed/empty/absent catalog yields no options; it never
 *  removes the manual text field, so this list is a hint, never an allow-list. */
export function catalogOptions(catalog: ProviderCatalogDTO | null | undefined): SelectOption[] {
  if (!catalog) return [];
  return catalog.models.map((m) => ({
    value: m.id,
    label: m.display_name?.trim() || m.id,
  }));
}

/** Whether a catalog refresh failed (an actionable, bounded error is present). */
export function catalogFailed(catalog: ProviderCatalogDTO | null | undefined): boolean {
  return Boolean(catalog?.error);
}

/** Manual model entry is ALWAYS allowed (the daemon pins `manual_entry_allowed` true); kept as a
 *  function so a test can pin that a failed catalog does not disable it. */
export function manualEntryAllowed(catalog: ProviderCatalogDTO | null | undefined): boolean {
  return catalog == null ? true : catalog.manual_entry_allowed !== false;
}

// ------------------------------------------------------------------------------------------------
// Global selection compatibility (mirrors the daemon's selection validation)
// ------------------------------------------------------------------------------------------------

export interface SelectionCompatibility {
  ok: boolean;
  /** A typed, actionable reason when not ok; null when ok. */
  reason: string | null;
}

// selectionCompatibility mirrors `rhapsody_config::validate::validate_providers`' SELECTION half so
// the Settings surface refuses locally what the daemon would 400 — never a silent fallback to another
// provider, model, or auth source. It deliberately does not re-derive or "correct" a value.
export function selectionCompatibility(input: {
  backend: string;
  providers: ProviderConfigDTO[];
  provider: string;
  model: string;
}): SelectionCompatibility {
  const model = input.model.trim();
  if (input.model !== "" && input.model !== model) {
    return { ok: false, reason: "Model id must not have surrounding whitespace." };
  }
  if (model !== "") {
    if (utf8Len(model) > MODEL_ID_MAX_BYTES) {
      return { ok: false, reason: `Model id must be at most ${MODEL_ID_MAX_BYTES} UTF-8 bytes.` };
    }
    // C0/C1 control characters (includes NUL), mirroring `char::is_control`.
    if (/[\u0000-\u001f\u007f-\u009f]/.test(model)) {
      return { ok: false, reason: "Model id must not contain control or NUL bytes." };
    }
  }
  if (input.provider === "") {
    return { ok: true, reason: null };
  }
  if (input.backend !== "opencode") {
    return {
      ok: false,
      reason: `The global agent backend is ${input.backend || "unset"}; only opencode can consume a provider.`,
    };
  }
  if (!input.providers.some((p) => p.id === input.provider)) {
    return { ok: false, reason: `Provider "${input.provider}" is not defined in providers:.` };
  }
  if (model === "") {
    return { ok: false, reason: `Provider "${input.provider}" requires an exact model.` };
  }
  return { ok: true, reason: null };
}

/** The compatibility verdict for a whole global config (the autosave gate's provider half). */
export function providerSelectionValid(g: GlobalConfigDTO): boolean {
  return selectionCompatibility({
    backend: g.agent.backend,
    providers: providerRegistry(g.providers),
    provider: g.agent.provider ?? "",
    model: g.agent.model ?? "",
  }).ok;
}

/** The compatibility verdict plus reason, for inline feedback. */
export function globalSelectionFeedback(g: GlobalConfigDTO): SelectionCompatibility {
  return selectionCompatibility({
    backend: g.agent.backend,
    providers: providerRegistry(g.providers),
    provider: g.agent.provider ?? "",
    model: g.agent.model ?? "",
  });
}

// ------------------------------------------------------------------------------------------------
// Desktop-only copy
// ------------------------------------------------------------------------------------------------

/** The one literal the browser dashboard shows where the desktop app offers credential actions. The
 *  shared web build serves both, so a browser must state the limitation rather than pretend. */
export const DESKTOP_ONLY_MESSAGE =
  "Credential changes require the Rhapsody desktop app; this browser dashboard can only show status.";

/** The explicit statement that credential mutations only affect not-yet-accepted work. */
export const ACTIVE_SESSION_NOTICE =
  "Connect, Replace, Rebind, and Remove affect only sessions not yet accepted for dispatch. " +
  "A run already in progress keeps the credential binding it started with until it ends.";
