// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { InstallReport, UpdateDownloadProgress, UpdateInfo } from "@/lib/bindings";

// Drive useUpdater by mocking U1's update bindings (TRA-260): the command wrappers are spies a test
// controls, and the two event subscribers capture their callbacks so a test can push the quiet
// launch-check event and per-chunk download progress the way the Tauri host would.
const h = vi.hoisted(() => ({
  checkForUpdate: vi.fn<() => Promise<UpdateInfo | null>>(),
  downloadUpdate: vi.fn<() => Promise<void>>(),
  installUpdate: vi.fn<(force?: boolean) => Promise<InstallReport | null>>(),
  activeRunCount: vi.fn<() => Promise<number>>(),
  availableCb: null as null | ((i: UpdateInfo) => void),
  progressCb: null as null | ((p: UpdateDownloadProgress) => void),
}));

vi.mock("@/lib/bindings", () => ({
  checkForUpdate: h.checkForUpdate,
  downloadUpdate: h.downloadUpdate,
  installUpdate: h.installUpdate,
  activeRunCount: h.activeRunCount,
  onUpdateAvailable: (cb: (i: UpdateInfo) => void) => {
    h.availableCb = cb;
    return () => {
      h.availableCb = null;
    };
  },
  onUpdateDownloadProgress: (cb: (p: UpdateDownloadProgress) => void) => {
    h.progressCb = cb;
    return () => {
      h.progressCb = null;
    };
  },
}));

import { useUpdater } from "@/hooks/useUpdater";

function info(over: Partial<UpdateInfo> = {}): UpdateInfo {
  return { available: true, version: "1.4.0", current_version: "1.3.0", notes: "Fixes bugs.", ...over };
}

beforeEach(() => {
  h.checkForUpdate.mockReset();
  h.downloadUpdate.mockReset().mockResolvedValue(undefined);
  h.installUpdate.mockReset();
  h.activeRunCount.mockReset().mockResolvedValue(0);
});

afterEach(() => {
  h.availableCb = null;
  h.progressCb = null;
});

describe("useUpdater", () => {
  it("starts idle with nothing pending", () => {
    const { result } = renderHook(() => useUpdater());
    expect(result.current.phase).toBe("idle");
    expect(result.current.pending).toBe(false);
    expect(result.current.info).toBeNull();
  });

  it("badges available when the quiet launch check emits update:available", () => {
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    expect(result.current.phase).toBe("available");
    expect(result.current.pending).toBe(true);
    expect(result.current.info?.version).toBe("1.4.0");
  });

  it("a manual check that finds a newer version goes available", async () => {
    h.checkForUpdate.mockResolvedValue(info());
    const { result } = renderHook(() => useUpdater());
    act(() => result.current.check());
    expect(result.current.phase).toBe("checking");
    await waitFor(() => expect(result.current.phase).toBe("available"));
    expect(result.current.info?.version).toBe("1.4.0");
  });

  it("a manual check that finds nothing goes up-to-date (dot dark)", async () => {
    h.checkForUpdate.mockResolvedValue(info({ available: false, version: "", notes: "" }));
    const { result } = renderHook(() => useUpdater());
    act(() => result.current.check());
    await waitFor(() => expect(result.current.phase).toBe("up-to-date"));
    expect(result.current.pending).toBe(false);
    expect(result.current.info?.current_version).toBe("1.3.0");
  });

  it("surfaces a check failure as an error phase with the message", async () => {
    h.checkForUpdate.mockRejectedValue(new Error("offline"));
    const { result } = renderHook(() => useUpdater());
    act(() => result.current.check());
    await waitFor(() => expect(result.current.phase).toBe("error"));
    expect(result.current.error).toContain("offline");
  });

  it("downloads with live progress and lands on ready", async () => {
    let release!: () => void;
    h.downloadUpdate.mockReturnValue(new Promise<void>((r) => (release = r)));
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    expect(result.current.phase).toBe("downloading");
    act(() => h.progressCb?.({ downloaded: 5, total: 10 }));
    expect(result.current.progress).toEqual({ downloaded: 5, total: 10 });
    await act(async () => {
      release();
    });
    await waitFor(() => expect(result.current.phase).toBe("ready"));
  });

  it("with no active runs, requestInstall installs directly (no warn dialog)", async () => {
    h.activeRunCount.mockResolvedValue(0);
    h.installUpdate.mockResolvedValue({ installed: true, blocked_active_runs: 0 });
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    await waitFor(() => expect(result.current.phase).toBe("ready"));
    act(() => result.current.requestInstall());
    await waitFor(() => expect(h.installUpdate).toHaveBeenCalledWith(false));
    expect(result.current.activeRunsPrompt).toBeNull();
  });

  it("with active runs, requestInstall opens the warn dialog instead of installing", async () => {
    h.activeRunCount.mockResolvedValue(3);
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    await waitFor(() => expect(result.current.phase).toBe("ready"));
    act(() => result.current.requestInstall());
    await waitFor(() => expect(result.current.activeRunsPrompt).toBe(3));
    expect(h.installUpdate).not.toHaveBeenCalled();
  });

  it("confirmInstallNow forces the install (stops the agents)", async () => {
    h.activeRunCount.mockResolvedValue(2);
    h.installUpdate.mockResolvedValue({ installed: true, blocked_active_runs: 0 });
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    await waitFor(() => expect(result.current.phase).toBe("ready"));
    act(() => result.current.requestInstall());
    await waitFor(() => expect(result.current.activeRunsPrompt).toBe(2));
    act(() => result.current.confirmInstallNow());
    await waitFor(() => expect(h.installUpdate).toHaveBeenCalledWith(true));
    expect(result.current.activeRunsPrompt).toBeNull();
  });

  it("deferToQuit installs on next quit and reflects the deferred phase", async () => {
    h.activeRunCount.mockResolvedValue(2);
    h.installUpdate.mockResolvedValue({ installed: false, blocked_active_runs: 2 });
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    await waitFor(() => expect(result.current.phase).toBe("ready"));
    act(() => result.current.requestInstall());
    await waitFor(() => expect(result.current.activeRunsPrompt).toBe(2));
    act(() => result.current.deferToQuit());
    await waitFor(() => expect(h.installUpdate).toHaveBeenCalledWith(false));
    await waitFor(() => expect(result.current.phase).toBe("deferred"));
    expect(result.current.pending).toBe(true);
  });

  it("dismissPrompt cancels the warn dialog without installing", async () => {
    h.activeRunCount.mockResolvedValue(1);
    const { result } = renderHook(() => useUpdater());
    act(() => h.availableCb?.(info()));
    act(() => result.current.download());
    await waitFor(() => expect(result.current.phase).toBe("ready"));
    act(() => result.current.requestInstall());
    await waitFor(() => expect(result.current.activeRunsPrompt).toBe(1));
    act(() => result.current.dismissPrompt());
    expect(result.current.activeRunsPrompt).toBeNull();
    expect(h.installUpdate).not.toHaveBeenCalled();
  });
});
