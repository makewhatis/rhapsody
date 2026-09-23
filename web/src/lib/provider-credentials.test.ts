// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  DESKTOP_ONLY_MESSAGE,
  failureCode,
  failureMessage,
  hasProviderBridge,
  isCommandFailure,
  isConfigured,
  providerConnect,
  providerPrepare,
  providerRemove,
  providerStatuses,
  providerTestConnection,
  statusLabel,
} from "@/lib/provider-credentials";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

function setBridge(present: boolean) {
  if (present) (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {};
  else delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
}

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  setBridge(false);
});

describe("provider-credentials — browser-safe degradation", () => {
  beforeEach(() => setBridge(false));

  it("reports no bridge and lists nothing in a plain browser", async () => {
    expect(hasProviderBridge()).toBe(false);
    expect(await providerStatuses()).toEqual([]);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("says credential changes need the desktop app", () => {
    expect(DESKTOP_ONLY_MESSAGE).toContain("desktop app");
  });
});

describe("provider-credentials — desktop bridge", () => {
  beforeEach(() => setBridge(true));

  it("lists provider statuses through the command bridge", async () => {
    invokeMock.mockResolvedValueOnce([
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
    const statuses = await providerStatuses();
    expect(invokeMock).toHaveBeenCalledWith("provider_statuses");
    expect(statuses[0].status).toBe("absent");
    expect(hasProviderBridge()).toBe(true);
  });

  it("passes the entered key to exactly one connect invocation", async () => {
    invokeMock.mockResolvedValueOnce({
      provider_id: "fireworks",
      operation: "connect",
      mutated: true,
      status: "configured",
      sync: "stored_offline",
    });
    const result = await providerConnect("fireworks", "nonce-1", "sk-secret");
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("provider_connect", {
      providerId: "fireworks",
      nonce: "nonce-1",
      secret: "sk-secret",
    });
    expect(result.mutated).toBe(true);
  });

  it("prepares a nonce without ever sending a binding or a revision", async () => {
    invokeMock.mockResolvedValueOnce({
      provider_id: "fireworks",
      operation: "connect",
      endpoint: "https://api.fireworks.ai/inference/v1",
      insecure_http: false,
      nonce: "n1",
      expires_in_ms: 120000,
    });
    await providerPrepare("fireworks", "connect");
    expect(invokeMock).toHaveBeenCalledWith("provider_prepare", {
      providerId: "fireworks",
      operation: "connect",
    });
  });

  it("removes with only the provider id and the nonce", async () => {
    invokeMock.mockResolvedValueOnce({
      provider_id: "fireworks",
      operation: "remove",
      mutated: false,
      status: "absent",
      sync: "synchronized",
    });
    await providerRemove("fireworks", "n2");
    expect(invokeMock).toHaveBeenCalledWith("provider_remove", {
      providerId: "fireworks",
      nonce: "n2",
    });
  });

  it("tests the connection through the bounded command", async () => {
    invokeMock.mockResolvedValueOnce({ ok: false, code: "unauthorized", message: "rejected" });
    const verdict = await providerTestConnection("fireworks");
    expect(invokeMock).toHaveBeenCalledWith("provider_test_connection", {
      providerId: "fireworks",
    });
    expect(verdict.code).toBe("unauthorized");
  });
});

describe("provider-credentials — failure classification and labels", () => {
  it("recognizes the typed command failure object and nothing else", () => {
    expect(isCommandFailure({ code: "nonce_expired", message: "expired" })).toBe(true);
    expect(isCommandFailure(new Error("boom"))).toBe(false);
    expect(isCommandFailure("boom")).toBe(false);
    expect(failureCode({ code: "generation_changed", message: "x" })).toBe("generation_changed");
    expect(failureCode(new Error("boom"))).toBe("unknown");
    expect(failureMessage({ code: "c", message: "start again" })).toBe("start again");
    expect(failureMessage(new Error("boom"))).not.toContain("boom");
  });

  it("labels every status distinctly", () => {
    expect(statusLabel("configured")).toBe("Connected");
    expect(statusLabel("absent")).toBe("Not connected");
    expect(statusLabel("denied_or_locked")).toBe("Keychain locked");
    expect(statusLabel("binding_mismatch")).toBe("Endpoint changed");
    expect(isConfigured("configured")).toBe(true);
    expect(isConfigured("absent")).toBe(false);
  });
});
