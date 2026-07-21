// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
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
  checkForUpdate,
  downloadUpdate,
  installUpdate,
  activeRunCount,
  onUpdateAvailable,
  onUpdateDownloadProgress,
  pickDirectory,
  pickFile,
} from "@/lib/bindings";
import type { UpdateInfo, UpdateDownloadProgress } from "@/lib/bindings";

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
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

// Flush the microtask queue so subscribeLogStream's promise-chained stop (start.then(stop)) settles.
const flush = () => new Promise((r) => setTimeout(r, 0));

const invokeMock = vi.mocked(invoke);
const listenMock = vi.mocked(listen);
const openMock = vi.mocked(open);

function setBridge(present: boolean) {
  if (present) (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {};
  else delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
}

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockReset();
  openMock.mockReset();
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

  it("update wrappers degrade to null/0/no-op and never touch invoke without a host", async () => {
    expect(await checkForUpdate()).toBeNull();
    expect(await installUpdate()).toBeNull();
    expect(await installUpdate(true)).toBeNull();
    expect(await activeRunCount()).toBe(0);
    await expect(downloadUpdate()).resolves.toBeUndefined();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("pickDirectory/pickFile resolve to '' and never open a native dialog without a host", async () => {
    expect(await pickDirectory("Choose logs folder")).toBe("");
    expect(await pickFile("Choose git executable")).toBe("");
    expect(openMock).not.toHaveBeenCalled();
  });

  it("update event subscriptions return unsubscribe no-ops without a host", () => {
    const a = onUpdateAvailable(() => {});
    const b = onUpdateDownloadProgress(() => {});
    expect(typeof a).toBe("function");
    expect(typeof b).toBe("function");
    expect(() => {
      a();
      b();
    }).not.toThrow();
    expect(listenMock).not.toHaveBeenCalled();
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

  it("checkForUpdate invokes update_check and returns its payload", async () => {
    invokeMock.mockResolvedValueOnce({
      available: true,
      version: "0.3.0",
      current_version: "0.2.0",
      notes: "Bug fixes",
    });
    const info = await checkForUpdate();
    expect(invokeMock).toHaveBeenCalledWith("update_check");
    expect(info?.available).toBe(true);
    expect(info?.version).toBe("0.3.0");
  });

  it("downloadUpdate invokes update_download", async () => {
    invokeMock.mockResolvedValue(undefined);
    await downloadUpdate();
    expect(invokeMock).toHaveBeenCalledWith("update_download");
  });

  it("installUpdate passes the force flag and returns the blocked-run report", async () => {
    invokeMock.mockResolvedValueOnce({ installed: false, blocked_active_runs: 2 });
    const report = await installUpdate(); // defaults to force=false
    expect(invokeMock).toHaveBeenCalledWith("update_install", { force: false });
    expect(report?.blocked_active_runs).toBe(2);

    invokeMock.mockResolvedValueOnce({ installed: true, blocked_active_runs: 0 });
    await installUpdate(true);
    expect(invokeMock).toHaveBeenLastCalledWith("update_install", { force: true });
  });

  it("activeRunCount invokes active_run_count and returns the count", async () => {
    invokeMock.mockResolvedValueOnce(3);
    expect(await activeRunCount()).toBe(3);
    expect(invokeMock).toHaveBeenCalledWith("active_run_count");
  });

  it("onUpdateAvailable subscribes to the update:available event and maps the payload", async () => {
    let seen: string | undefined;
    onUpdateAvailable((info) => (seen = info.version));
    expect(listenMock).toHaveBeenCalledWith("update:available", expect.any(Function));
    const handler = listenMock.mock.calls[0][1] as (e: { payload: UpdateInfo }) => void;
    handler({ payload: { available: true, version: "9.9.9", current_version: "1.0.0", notes: "" } });
    expect(seen).toBe("9.9.9");
  });

  it("pickDirectory opens a folder chooser and returns the chosen path", async () => {
    openMock.mockResolvedValueOnce("/Users/me/logs");
    expect(await pickDirectory("Choose logs folder")).toBe("/Users/me/logs");
    expect(openMock).toHaveBeenCalledWith({
      directory: true,
      multiple: false,
      title: "Choose logs folder",
    });
  });

  it("pickFile opens a file chooser and returns the chosen path", async () => {
    openMock.mockResolvedValueOnce("/usr/local/bin/git");
    expect(await pickFile("Choose git executable")).toBe("/usr/local/bin/git");
    expect(openMock).toHaveBeenCalledWith({
      directory: false,
      multiple: false,
      title: "Choose git executable",
    });
  });

  it("normalizes a cancelled (null) pick and an array result to a single string", async () => {
    openMock.mockResolvedValueOnce(null); // user cancelled the dialog
    expect(await pickDirectory("t")).toBe("");
    openMock.mockResolvedValueOnce(["/first/path", "/second/path"]); // defensive: multiple:false never arrays
    expect(await pickFile("t")).toBe("/first/path");
    openMock.mockResolvedValueOnce([]); // empty array → ""
    expect(await pickFile("t")).toBe("");
  });

  it("onUpdateDownloadProgress subscribes and forwards progress ticks", async () => {
    let last: UpdateDownloadProgress | undefined;
    onUpdateDownloadProgress((p) => (last = p));
    expect(listenMock).toHaveBeenCalledWith("update:download-progress", expect.any(Function));
    const handler = listenMock.mock.calls[0][1] as (e: {
      payload: UpdateDownloadProgress;
    }) => void;
    handler({ payload: { downloaded: 512, total: 2048 } });
    expect(last).toEqual({ downloaded: 512, total: 2048 });
  });
});
