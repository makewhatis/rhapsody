// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { ProviderStatus } from "@/lib/provider-credentials";
import { ProviderCredentialForm } from "@/components/settings/ProviderCredentialForm";

const {
  hasProviderBridge,
  providerPrepare,
  providerConnect,
  providerReplace,
  providerRebind,
  providerRemove,
  providerTestConnection,
} = vi.hoisted(() => ({
  hasProviderBridge: vi.fn(() => true),
  providerPrepare: vi.fn(),
  providerConnect: vi.fn(),
  providerReplace: vi.fn(),
  providerRebind: vi.fn(),
  providerRemove: vi.fn(),
  providerTestConnection: vi.fn(),
}));

vi.mock("@/lib/provider-credentials", async () => {
  const actual = await vi.importActual<typeof import("@/lib/provider-credentials")>(
    "@/lib/provider-credentials",
  );
  return {
    ...actual,
    hasProviderBridge,
    providerPrepare,
    providerConnect,
    providerReplace,
    providerRebind,
    providerRemove,
    providerTestConnection,
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  hasProviderBridge.mockReturnValue(true);
  window.localStorage?.clear();
  window.sessionStorage?.clear();
});

function provider(overrides: Partial<ProviderStatus> = {}): ProviderStatus {
  return {
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
    ...overrides,
  };
}

describe("ProviderCredentialForm", () => {
  it("says credential changes need the desktop app in a plain browser", () => {
    hasProviderBridge.mockReturnValue(false);
    render(<ProviderCredentialForm provider={provider()} />);
    expect(screen.getByText(/require the Rhapsody desktop app/i)).toBeTruthy();
    // No mutation controls are offered without the bridge.
    expect(screen.queryByRole("button", { name: "Connect" })).toBeNull();
  });

  // MUTATION GUARD: the entered key is cleared from the field the instant it is submitted, and never
  // appears again in the rendered DOM or in any storage. A form that kept the value in state (or
  // echoed it) would red this.
  it("clears the entered key immediately and never persists or echoes it", async () => {
    providerPrepare.mockResolvedValue({
      provider_id: "fireworks",
      operation: "connect",
      endpoint: "https://api.fireworks.ai/inference/v1",
      insecure_http: false,
      nonce: "n1",
      expires_in_ms: 120000,
    });
    providerConnect.mockResolvedValue({
      provider_id: "fireworks",
      operation: "connect",
      mutated: true,
      status: "configured",
      sync: "stored_offline",
    });

    render(<ProviderCredentialForm provider={provider()} onChanged={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));

    const input = screen.getByLabelText("Provider credential") as HTMLInputElement;
    const secret = "sk-CANARY-secret";
    fireEvent.change(input, { target: { value: secret } });
    expect(input.value).toBe(secret);

    fireEvent.click(screen.getByRole("button", { name: "Store credential" }));

    await waitFor(() => expect(providerConnect).toHaveBeenCalledTimes(1));
    // The key is passed exactly once, to the single connect call.
    expect(providerConnect).toHaveBeenCalledWith("fireworks", "n1", secret);
    // The field is cleared, and the secret is nowhere in the DOM or storage.
    expect(input.value).toBe("");
    expect(document.body.textContent ?? "").not.toContain(secret);
    expect(document.body.innerHTML).not.toContain(secret);
    // No persistence surface may hold the key (this jsdom can lack localStorage; guard either way).
    if (window.localStorage) expect(window.localStorage.length).toBe(0);
    if (window.sessionStorage) expect(window.sessionStorage.length).toBe(0);
  });

  it("surfaces a typed failure without echoing the key", async () => {
    providerPrepare.mockRejectedValue({ code: "keychain_denied", message: "the login keychain refused access" });
    render(<ProviderCredentialForm provider={provider()} />);
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
    const input = screen.getByLabelText("Provider credential");
    fireEvent.change(input, { target: { value: "sk-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Store credential" }));
    await waitFor(() => expect(screen.getByText(/keychain refused access/i)).toBeTruthy());
    expect(document.body.innerHTML).not.toContain("sk-secret");
  });

  it("offers rebind and remove only when the status permits", () => {
    const { rerender } = render(
      <ProviderCredentialForm provider={provider({ status: "binding_mismatch", can_connect: false, can_rebind: true, can_remove: true })} />,
    );
    expect(screen.getByRole("button", { name: "Rebind to this endpoint" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Connect" })).toBeNull();
    rerender(<ProviderCredentialForm provider={provider()} />);
    expect(screen.queryByRole("button", { name: "Remove" })).toBeNull();
  });

  it("reports a stored-offline sync honestly", async () => {
    providerPrepare.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      endpoint: "https://api.fireworks.ai/inference/v1",
      insecure_http: false,
      nonce: "n2",
      expires_in_ms: 120000,
    });
    providerRemove.mockResolvedValue({
      provider_id: "fireworks",
      operation: "remove",
      mutated: true,
      status: "absent",
      sync: "stored_offline",
    });
    render(<ProviderCredentialForm provider={provider({ status: "configured", can_connect: false, can_remove: true })} />);
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.getByText(/no daemon is running/i)).toBeTruthy());
  });
});
