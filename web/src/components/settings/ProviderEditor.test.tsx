// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ProviderConfigDTO } from "@/lib/api";

const h = vi.hoisted(() => ({
  saveProviderConfig: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, saveProviderConfig: h.saveProviderConfig };
});

import { ProviderEditor } from "@/components/settings/ProviderEditor";
import { DEFAULT_PROVIDER_LIMITS } from "@/lib/providers-presets";

const PROVIDER: ProviderConfigDTO = {
  id: "fireworks",
  protocol: "openai-compatible",
  display_name: "Fireworks",
  base_url: "https://api.fireworks.ai/inference/v1",
  allow_insecure_http: false,
  credential: { source: "keychain" },
  broker_limits: {
    ...DEFAULT_PROVIDER_LIMITS,
    forwarded_requests_per_turn: 5,
    max_reserved_token_units_per_utc_day: 1_000_000,
  },
};

/** The limits block an Add posts: every visible field at its default, capability omitted (unchanged),
 *  and a blank daily cap encoded as 0 (the wire's "no cap"). */
function defaultAddLimits(): Record<string, number> {
  const out: Record<string, number> = {};
  for (const [key, value] of Object.entries(DEFAULT_PROVIDER_LIMITS)) {
    if (key === "capability_lifetime_ms" || key === "max_reserved_token_units_per_utc_day") continue;
    out[key] = value as number;
  }
  out.max_reserved_token_units_per_utc_day = 0;
  return out;
}

function renderEditor(props: Partial<React.ComponentProps<typeof ProviderEditor>> = {}) {
  return render(
    <ProviderEditor
      open
      editing={null}
      usedIds={[]}
      hasStoredKey={false}
      onClose={vi.fn()}
      onSaved={vi.fn()}
      {...props}
    />,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("ProviderEditor", () => {
  // ACCEPTANCE: a preset fills in the protocol and base URL.
  it("prefills a preset's protocol and base URL", () => {
    renderEditor();
    expect((screen.getByLabelText("Provider id") as HTMLInputElement).value).toBe("fireworks");
    expect((screen.getByLabelText("Base URL") as HTMLInputElement).value).toBe(
      "https://api.fireworks.ai/inference/v1",
    );
    expect((screen.getByLabelText("Protocol") as HTMLInputElement).value).toBe(
      "openai-compatible",
    );
  });

  // ACCEPTANCE: add posts the expected definition through the daemon path.
  it("posts an add with the expected definition", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    const onSaved = vi.fn();
    renderEditor({ onSaved });
    fireEvent.click(screen.getByRole("button", { name: "Add provider" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith({
        op: "add",
        provider_id: "fireworks",
        definition: {
          protocol: "openai-compatible",
          display_name: "Fireworks",
          base_url: "https://api.fireworks.ai/inference/v1",
          allow_insecure_http: false,
          limits: defaultAddLimits(),
        },
      }),
    );
    expect(onSaved).toHaveBeenCalled();
  });

  // ACCEPTANCE: plain http shows the visible warning + opt-in, and the opt-in is what is posted.
  it("warns about plaintext HTTP and posts the opt-in", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    renderEditor();
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "http://plain.example/v1" },
    });
    expect(screen.getByRole("alert").textContent).toMatch(/plaintext HTTP/i);
    fireEvent.click(screen.getByLabelText("Allow insecure HTTP"));
    fireEvent.click(screen.getByRole("button", { name: "Add provider" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith(
        expect.objectContaining({
          definition: expect.objectContaining({ allow_insecure_http: true }),
        }),
      ),
    );
  });

  // ACCEPTANCE: the daemon's own validation error text is surfaced, not a local paraphrase.
  it("surfaces the daemon's validation error", async () => {
    h.saveProviderConfig.mockRejectedValue(
      new Error("invalid_config: base_url \"http://plain.example/v1\" is http but allow_insecure_http is not true"),
    );
    renderEditor();
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "http://plain.example/v1" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add provider" }));
    await waitFor(() =>
      expect(screen.getByText(/is http but allow_insecure_http is not true/)).toBeTruthy(),
    );
  });

  // ACCEPTANCE: editing the endpoint of a provider with a stored key warns about the Rebind.
  it("warns before saving an endpoint change on a keyed provider", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    renderEditor({ editing: PROVIDER, hasStoredKey: true });
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://api.fireworks.ai/inference/v2" },
    });
    expect(screen.getByText(/rebind required/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith(
        expect.objectContaining({ op: "edit", provider_id: "fireworks" }),
      ),
    );
  });

  it("does not warn about a rebind when the endpoint is unchanged", () => {
    renderEditor({ editing: PROVIDER, hasStoredKey: true });
    fireEvent.change(screen.getByLabelText("Display name"), { target: { value: "Renamed" } });
    expect(screen.queryByText(/rebind required/i)).toBeNull();
  });

  // ACCEPTANCE (REVIEW B5): the form shows the provider's limits and sends them back on save, so an
  // edit cannot erase the daily spend cap.
  it("prefills the stored limits and sends them back", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    renderEditor({ editing: PROVIDER, hasStoredKey: true });
    expect((screen.getByLabelText("forwarded_requests_per_turn") as HTMLInputElement).value).toBe("5");
    expect(
      (screen.getByLabelText("max_reserved_token_units_per_utc_day") as HTMLInputElement).value,
    ).toBe("1000000");
    fireEvent.change(screen.getByLabelText("Display name"), { target: { value: "Renamed" } });
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith(
        expect.objectContaining({
          definition: expect.objectContaining({
            limits: expect.objectContaining({
              forwarded_requests_per_turn: 5,
              max_reserved_token_units_per_utc_day: 1_000_000,
            }),
          }),
        }),
      ),
    );
  });

  // ACCEPTANCE (REVIEW B5): clearing the daily cap posts 0 (the wire's "no cap") so the daemon drops
  // it; a non-numeric field is refused locally.
  it("clears the daily cap and refuses a non-numeric limit", async () => {
    h.saveProviderConfig.mockResolvedValue({ config: {}, prompt_body: "" });
    renderEditor({ editing: PROVIDER, hasStoredKey: true });
    fireEvent.change(screen.getByLabelText("max_reserved_token_units_per_utc_day"), {
      target: { value: "" },
    });
    fireEvent.change(screen.getByLabelText("forwarded_requests_per_turn"), {
      target: { value: "abc" },
    });
    // The invalid value blocks the save and is reported.
    expect((screen.getByRole("button", { name: "Save changes" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/must be a positive whole number/)).toBeTruthy();

    fireEvent.change(screen.getByLabelText("forwarded_requests_per_turn"), {
      target: { value: "5" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() =>
      expect(h.saveProviderConfig).toHaveBeenCalledWith(
        expect.objectContaining({
          definition: expect.objectContaining({
            limits: expect.objectContaining({ max_reserved_token_units_per_utc_day: 0 }),
          }),
        }),
      ),
    );
  });
});
