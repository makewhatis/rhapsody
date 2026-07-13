// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  hasBridge,
  getStatus,
  startDaemon,
  stopDaemon,
  restartDaemon,
  probeTools,
  credentialStatus,
  setToolOverride,
  writeInitialConfig,
  openExternal,
  onNavigate,
} from "@/lib/bindings";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const invokeMock = vi.mocked(invoke);
const listenMock = vi.mocked(listen);

function setBridge(present: boolean) {
  if (present) (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {};
  else delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
}

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockReset();
  listenMock.mockResolvedValue(() => {});
});

afterEach(() => {
  setBridge(false);
  vi.unstubAllGlobals();
});

describe("bindings — browser-safe degradation (no Tauri host)", () => {
  beforeEach(() => setBridge(false));

  it("reports no bridge and resolves null/empty without the Tauri IPC globals", async () => {
    expect(hasBridge()).toBe(false);
    expect(await getStatus()).toBeNull();
    expect(await credentialStatus()).toBeNull();
    expect(await probeTools()).toEqual([]);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("lifecycle actions are no-ops that do not throw and never touch invoke", async () => {
    await expect(startDaemon()).resolves.toBeUndefined();
    await expect(stopDaemon()).resolves.toBeUndefined();
    await expect(restartDaemon()).resolves.toBeUndefined();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("onNavigate returns an unsubscribe no-op when the bridge is absent", () => {
    const unsub = onNavigate(() => {});
    expect(typeof unsub).toBe("function");
    expect(() => unsub()).not.toThrow();
    expect(listenMock).not.toHaveBeenCalled();
  });

  it("openExternal falls back to window.open in a plain browser", () => {
    const open = vi.fn();
    vi.stubGlobal("open", open);
    openExternal("https://linear.app");
    expect(open).toHaveBeenCalledWith("https://linear.app", "_blank", "noopener");
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("bindings — Tauri host present", () => {
  beforeEach(() => setBridge(true));

  it("getStatus invokes the `status` command and returns its payload", async () => {
    invokeMock.mockResolvedValueOnce({
      state: "running",
      pid: 1,
      restarts: 0,
      last_err: "",
      url: "http://127.0.0.1:8799",
      healthy: true,
      agent_count: 2,
      configured: true,
    });
    expect(hasBridge()).toBe(true);
    const st = await getStatus();
    expect(invokeMock).toHaveBeenCalledWith("status");
    expect(st?.agent_count).toBe(2);
  });

  it("passes named args through invoke (setToolOverride / writeInitialConfig)", async () => {
    invokeMock.mockResolvedValue(undefined);
    await setToolOverride("gh", "/usr/local/bin/gh");
    expect(invokeMock).toHaveBeenCalledWith("set_tool_override", {
      name: "gh",
      path: "/usr/local/bin/gh",
    });
    await writeInitialConfig("my-project");
    expect(invokeMock).toHaveBeenCalledWith("write_initial_config", { projectSlug: "my-project" });
  });

  it("openExternal invokes the open_external command", () => {
    openExternal("https://linear.app/foo");
    expect(invokeMock).toHaveBeenCalledWith("open_external", { url: "https://linear.app/foo" });
  });

  it("onNavigate subscribes to the tray navigate event and maps the payload", async () => {
    let seen = "";
    onNavigate((v) => (seen = v));
    expect(listenMock).toHaveBeenCalledWith("tray:navigate", expect.any(Function));
    const handler = listenMock.mock.calls[0][1] as (e: { payload: string }) => void;
    handler({ payload: "settings" });
    expect(seen).toBe("settings");
  });
});
