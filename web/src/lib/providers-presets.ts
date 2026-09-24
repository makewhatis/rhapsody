// providers-presets — the Add-provider presets and the URL-joining rules the Settings form mirrors
// for immediate feedback (STUDIO-1048). Kept DOM-free so the rules are unit-testable.
//
// The daemon is ALWAYS the authority: these helpers exist so the form can offer presets and refuse an
// obviously-malformed endpoint before a round-trip, never to replace `rhapsody_config`'s validation.
// `normalizeProviderBaseUrl` mirrors `rhapsody_config::providers::normalize_provider_base_url`
// exactly (strip a trailing `/`; append `/v1` ONLY when the path does not already end in `/v1`) and
// `chatCompletionsUrl` mirrors `chat_completions_url` (append the fixed relative `chat/completions`).
// design §14.2: base `/v1` and `/inference/v1` must map to exactly ONE `chat/completions` suffix.

import type { ProviderConfigDTO } from "@/lib/api";

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
