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

const PROVIDER: ProviderConfigDTO = {
  id: "fireworks",
  protocol: "openai-compatible",
  display_name: "Fireworks",
  base_url: "https://api.fireworks.ai/inference/v1",
  allow_insecure_http: false,
  credential: { source: "keychain" },
};

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
});
