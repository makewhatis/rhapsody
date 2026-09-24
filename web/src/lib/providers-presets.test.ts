import { describe, expect, it } from "vitest";
import {
  PROVIDER_PRESETS,
  chatCompletionsUrl,
  endpointChanged,
  endpointJoinsCleanly,
  normalizeProviderBaseUrl,
} from "@/lib/providers-presets";

describe("provider presets", () => {
  // MUTATION GUARD (design §14.2): every shipped preset must map to exactly ONE chat/completions
  // suffix. A preset whose base URL would produce `/v1/v1/chat/completions` or a doubled suffix reds.
  it("every preset joins to exactly one chat/completions suffix", () => {
    for (const preset of PROVIDER_PRESETS) {
      expect(preset.protocol).toBe("openai-compatible");
      expect(endpointJoinsCleanly(preset.base_url)).toBe(true);
      const url = chatCompletionsUrl(preset.base_url);
      expect(url).not.toBeNull();
      expect(url!.split("chat/completions").length - 1).toBe(1);
      expect(url!.includes("/v1/v1")).toBe(false);
    }
  });

  // The three bases the design names explicitly: no path, /v1, /inference/v1.
  it("normalizes the design's pinned bases without doubling /v1", () => {
    expect(normalizeProviderBaseUrl("https://api.example.com")).toBe("https://api.example.com/v1");
    expect(normalizeProviderBaseUrl("https://api.example.com/v1")).toBe("https://api.example.com/v1");
    expect(normalizeProviderBaseUrl("https://api.example.com/inference/v1")).toBe(
      "https://api.example.com/inference/v1",
    );
    expect(chatCompletionsUrl("https://api.example.com/inference/v1")).toBe(
      "https://api.example.com/inference/v1/chat/completions",
    );
    expect(normalizeProviderBaseUrl("https://api.example.com/v1/")).toBe("https://api.example.com/v1");
  });

  it("refuses a base that already ends in chat/completions", () => {
    // normalize() would leave it and chatCompletionsUrl() would double the suffix.
    expect(endpointJoinsCleanly("https://api.example.com/v1/chat/completions")).toBe(false);
  });

  it("returns null for a non-absolute endpoint", () => {
    expect(normalizeProviderBaseUrl("api.example.com/v1")).toBeNull();
  });
});

describe("endpointChanged", () => {
  it("ignores a cosmetic trailing slash but flags a real endpoint change", () => {
    const provider = { base_url: "https://api.example.com/v1", protocol: "openai-compatible" };
    expect(
      endpointChanged(provider, { base_url: "https://api.example.com/v1/", protocol: "openai-compatible" }),
    ).toBe(false);
    expect(
      endpointChanged(provider, { base_url: "https://api.example.com/v2", protocol: "openai-compatible" }),
    ).toBe(true);
  });
});
