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
  subscribeLogStream,
} from "@/lib/bindings";

// A minimal stand-in for @tauri-apps/api/core's Channel: a unique numeric id + a settable onmessage,
// enough to assert subscribeLogStream wires start/stop_log_stream to the right channel.
vi.mock("@tauri-apps/api/core", () => {
  let counter = 0;
  class Channel {
    id = ++counter;
    onmessage: ((m: unknown) => void) | null = null;
  }
  return { invoke: vi.fn(), Channel };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

// Flush the microtask queue so subscribeLogStream's promise-chained stop (start.then(stop)) settles.
const flush = () => new Promise((r) => setTimeout(r, 0));

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

  it("subscribeLogStream returns an unsubscribe no-op and never touches invoke without a host", async () => {
    const unsub = subscribeLogStream(() => {});
    expect(typeof unsub).toBe("function");
    expect(() => unsub()).not.toThrow();
    await flush();
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

  it("subscribeLogStream starts the host tail and stops it by channel id on unsubscribe", async () => {
    invokeMock.mockResolvedValue(undefined);
    const unsub = subscribeLogStream(() => {});
    // start_log_stream is invoked with the created channel...
    expect(invokeMock).toHaveBeenCalledWith("start_log_stream", { channel: expect.anything() });
    const channel = (invokeMock.mock.calls[0][1] as { channel: { id: number } }).channel;
    // ...and its onmessage was wired to the caller's handler (a Channel instance).
    expect(channel).toBeTruthy();

    unsub();
    await flush(); // the stop is chained on start's resolution
    expect(invokeMock).toHaveBeenCalledWith("stop_log_stream", { streamId: channel.id });
  });
});
