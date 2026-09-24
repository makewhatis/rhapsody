// providers-presets — the Add-provider presets and the URL-joining rules the Settings form mirrors
// for immediate feedback (STUDIO-1048). Kept DOM-free so the rules are unit-testable.
//
// The daemon is ALWAYS the authority: these helpers exist so the form can offer presets and refuse an
// obviously-malformed endpoint before a round-trip, never to replace `rhapsody_config`'s validation.
// `normalizeProviderBaseUrl` mirrors `rhapsody_config::providers::normalize_provider_base_url`
// exactly (strip a trailing `/`; append `/v1` ONLY when the path does not already end in `/v1`) and
// `chatCompletionsUrl` mirrors `chat_completions_url` (append the fixed relative `chat/completions`).
// design §14.2: base `/v1` and `/inference/v1` must map to exactly ONE `chat/completions` suffix.

import type { ProviderConfigDTO, ProviderLimitsDTO } from "@/lib/api";

/** One Add-provider preset: a name, the protocol v1 supports, and the protocol base URL. */
export interface ProviderPreset {
  /** The suggested (editable) provider id — a canonical, label-safe default. */
  id: string;
  label: string;
  /** v1's broker supports only the OpenAI-compatible Chat Completions protocol. */
  protocol: string;
  base_url: string;
}

/** The protocol v1 supports. Mirrors `rhapsody_config::providers::PROTOCOL_OPENAI_COMPATIBLE`. */
export const PROTOCOL_OPENAI_COMPATIBLE = "openai-compatible";

/** The V1 default broker limits, mirroring `rhapsody_config::providers::BrokerLimits::default` (the
 *  `DEFAULT_*` constants in `providers.rs`). Shown when Add creates a provider so the operator sees
 *  today's values; because the writer prunes a field equal to its default, writing them back leaves
 *  the file untouched. */
export const DEFAULT_PROVIDER_LIMITS: ProviderLimitsDTO = {
  forwarded_requests_per_turn: 64,
  denied_requests_before_revocation: 16,
  concurrent_upstream_requests_per_turn: 4,
  json_request_bytes: 8 * 1024 * 1024,
  aggregate_request_bytes_per_turn: 32 * 1024 * 1024,
  response_bytes_per_request: 16 * 1024 * 1024,
  aggregate_response_bytes_per_turn: 64 * 1024 * 1024,
  requested_output_tokens_per_request: 32_000,
  reserved_token_units_per_turn: 1_000_000,
  reserved_token_units_per_session: 20_000_000,
  capability_lifetime_ms: 3_600_000,
  max_reserved_token_units_per_utc_day: null,
};

/** The broker-limit fields the editor exposes, in wire-key order. `capability_lifetime_ms` is shown
 *  (it is part of the definition) but only sent when the operator changes it — it is derived from the
 *  turn deadline when unset, and writing back an unchanged effective value would pin today's default. */
export const PROVIDER_LIMIT_FIELDS: { key: keyof ProviderLimitsDTO; label: string }[] = [
  { key: "forwarded_requests_per_turn", label: "Forwarded requests per turn" },
  { key: "denied_requests_before_revocation", label: "Denied requests before revocation" },
  { key: "concurrent_upstream_requests_per_turn", label: "Concurrent upstream requests per turn" },
  { key: "json_request_bytes", label: "JSON request bytes" },
  { key: "aggregate_request_bytes_per_turn", label: "Aggregate request bytes per turn" },
  { key: "response_bytes_per_request", label: "Response bytes per request" },
  { key: "aggregate_response_bytes_per_turn", label: "Aggregate response bytes per turn" },
  { key: "requested_output_tokens_per_request", label: "Requested output tokens per request" },
  { key: "reserved_token_units_per_turn", label: "Reserved token units per turn" },
  { key: "reserved_token_units_per_session", label: "Reserved token units per session" },
  { key: "capability_lifetime_ms", label: "Capability lifetime (ms)" },
];

/** The limits block the editor starts from: the provider's own, or the V1 defaults for an Add. */
export function providerLimitsOf(
  provider: Pick<ProviderConfigDTO, "broker_limits"> | null | undefined,
): ProviderLimitsDTO {
  return provider?.broker_limits ?? DEFAULT_PROVIDER_LIMITS;
}

/**
 * The shipped presets. Every one speaks OpenAI Chat Completions and is verified against the
 * URL-joining rules by `providers-presets.test.ts` (and listed with its base URL in the PR body).
 * A custom endpoint is always available beside these.
 */
export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: "fireworks",
    label: "Fireworks",
    protocol: PROTOCOL_OPENAI_COMPATIBLE,
    base_url: "https://api.fireworks.ai/inference/v1",
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    protocol: PROTOCOL_OPENAI_COMPATIBLE,
    base_url: "https://openrouter.ai/api/v1",
  },
  {
    id: "openai",
    label: "OpenAI",
    protocol: PROTOCOL_OPENAI_COMPATIBLE,
    base_url: "https://api.openai.com/v1",
  },
  {
    id: "together",
    label: "Together",
    protocol: PROTOCOL_OPENAI_COMPATIBLE,
    base_url: "https://api.together.xyz/v1",
  },
  {
    id: "groq",
    label: "Groq",
    protocol: PROTOCOL_OPENAI_COMPATIBLE,
    base_url: "https://api.groq.com/openai/v1",
  },
];

/** Strips redundant trailing slashes (never the scheme's `//`). */
function trimTrailingSlashes(url: string): string {
  let out = url;
  while (out.length > "https://".length && out.endsWith("/")) {
    out = out.slice(0, -1);
  }
  return out;
}

/**
 * Mirrors `normalize_provider_base_url`: the protocol base immediately above `chat/completions`.
 * Returns `null` when the value is not an absolute `http(s)` URL (the form then lets the daemon give
 * its authoritative message rather than guessing).
 */
export function normalizeProviderBaseUrl(baseUrl: string): string | null {
  const trimmed = trimTrailingSlashes(baseUrl.trim());
  if (!/^https?:\/\//.test(trimmed)) return null;
  if (trimmed.endsWith("/v1")) return trimmed;
  return `${trimmed}/v1`;
}

/** Mirrors `chat_completions_url`: the normalized base plus exactly one `chat/completions`. */
export function chatCompletionsUrl(baseUrl: string): string | null {
  const base = normalizeProviderBaseUrl(baseUrl);
  return base == null ? null : `${base}/chat/completions`;
}

/**
 * Whether a base URL maps to exactly one unambiguous `chat/completions` endpoint — the §14.2 rule
 * every preset must pass. A base already ending in `chat/completions` is refused (the daemon refuses
 * it too), as is anything that would produce a doubled `/v1`.
 */
export function endpointJoinsCleanly(baseUrl: string): boolean {
  const url = chatCompletionsUrl(baseUrl);
  if (url == null) return false;
  const suffixCount = url.split("chat/completions").length - 1;
  return suffixCount === 1 && !url.includes("/v1/v1");
}

/** The suggested provider id for a fresh form, chosen from the preset or a generic default. */
export function defaultProviderId(presets: ProviderPreset[] = PROVIDER_PRESETS): string {
  return presets[0]?.id ?? "provider";
}

/** The provider-ids already taken, so the Add form can refuse a duplicate before the round-trip. */
export function usedProviderIds(providers: ProviderConfigDTO[]): string[] {
  return providers.map((p) => p.id);
}

/**
 * Whether the endpoint or protocol differs from the provider's stored definition — the change that
 * requires an explicit desktop Rebind when a key is stored. Compared on the NORMALIZED base URL, so
 * a cosmetic trailing slash is not treated as a rebind.
 */
export function endpointChanged(
  provider: Pick<ProviderConfigDTO, "base_url" | "protocol">,
  next: { base_url: string; protocol: string },
): boolean {
  const before = normalizeProviderBaseUrl(provider.base_url) ?? provider.base_url;
  const after = normalizeProviderBaseUrl(next.base_url) ?? next.base_url.trim();
  return before !== after || provider.protocol !== next.protocol;
}
