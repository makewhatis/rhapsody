// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import {
  hasBridge,
  getStatus,
  startDaemon,
  stopDaemon,
  restartDaemon,
  probeTools,
  credentialStatus,
  onNavigate,
} from "@/lib/bindings";

afterEach(() => {
  delete (window as { go?: unknown }).go;
  delete (window as { runtime?: unknown }).runtime;
});

describe("bindings — browser-safe degradation (no Wails host)", () => {
  it("reports no bridge and resolves null/empty without window.go", async () => {
    expect(hasBridge()).toBe(false);
    expect(await getStatus()).toBeNull();
    expect(await credentialStatus()).toBeNull();
    expect(await probeTools()).toEqual([]);
  });

  it("lifecycle actions are no-ops that do not throw", async () => {
    await expect(startDaemon()).resolves.toBeUndefined();
    await expect(stopDaemon()).resolves.toBeUndefined();
    await expect(restartDaemon()).resolves.toBeUndefined();
  });

  it("onNavigate returns an unsubscribe no-op when the runtime is absent", () => {
    const unsub = onNavigate(() => {});
    expect(typeof unsub).toBe("function");
    expect(() => unsub()).not.toThrow();
  });

  it("calls through to the bridge when present", async () => {
    (window as unknown as { go: unknown }).go = {
      main: {
        App: {
          Status: async () => ({
            state: "running",
            pid: 1,
            restarts: 0,
            last_err: "",
            url: "http://127.0.0.1:8799",
            healthy: true,
            agent_count: 2,
            configured: true,
          }),
        },
      },
    };
    expect(hasBridge()).toBe(true);
    const st = await getStatus();
    expect(st?.agent_count).toBe(2);
  });
});
