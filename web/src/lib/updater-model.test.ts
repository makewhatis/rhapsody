import { describe, expect, it } from "vitest";
import { deferredSubline, downloadPercent, formatBytes, updatePending } from "@/lib/updater-model";

describe("downloadPercent", () => {
  it("is null with no progress yet (nothing to show)", () => {
    expect(downloadPercent(null)).toBeNull();
  });

  it("is null when the server sent no Content-Length (indeterminate)", () => {
    expect(downloadPercent({ downloaded: 500, total: null })).toBeNull();
  });

  it("is null when the total is zero (avoids a divide-by-zero bar)", () => {
    expect(downloadPercent({ downloaded: 0, total: 0 })).toBeNull();
  });

  it("is the rounded percentage of the known total", () => {
    expect(downloadPercent({ downloaded: 25, total: 100 })).toBe(25);
    expect(downloadPercent({ downloaded: 1, total: 3 })).toBe(33);
  });

  it("clamps to 100 when downloaded overshoots the reported total", () => {
    expect(downloadPercent({ downloaded: 120, total: 100 })).toBe(100);
  });
});

describe("formatBytes", () => {
  it("shows bytes below a kilobyte", () => {
    expect(formatBytes(512)).toBe("512 B");
  });

  it("shows one decimal of kilobytes", () => {
    expect(formatBytes(2048)).toBe("2.0 KB");
  });

  it("shows one decimal of megabytes for release-sized artifacts", () => {
    expect(formatBytes(48 * 1024 * 1024)).toBe("48.0 MB");
  });
});

describe("updatePending", () => {
  it("is true for every phase that means an update is waiting on the user", () => {
    for (const phase of ["available", "downloading", "ready", "installing", "deferred"] as const) {
      expect(updatePending(phase)).toBe(true);
    }
  });

  it("is false when there is nothing to act on", () => {
    for (const phase of ["idle", "checking", "up-to-date", "error"] as const) {
      expect(updatePending(phase)).toBe(false);
    }
  });
});

// STUDIO-880 — a deferral has three causes now, and only one of them is the pre-existing one.
describe("deferredSubline", () => {
  it("with no drain, says what it always said: it installs on the next quit", () => {
    expect(deferredSubline(null)).toMatch(/next quit/i);
    expect(deferredSubline(null)).not.toMatch(/draining/i);
  });

  // The one an operator must not miss: nothing was interrupted, AND the daemon is still drained.
  it("names the still-armed drain after an expired budget", () => {
    const sub = deferredSubline({ outcome: "expired", running: 2, waited_secs: 1800 });
    expect(sub).toMatch(/still draining/i);
    expect(sub).toMatch(/won't take new work|cancelled/i);
  });

  // Saying "we waited" when the daemon could not even be asked would be a plain lie.
  it("says no wait happened when the daemon could not be asked", () => {
    for (const outcome of ["request_failed", "not_running"] as const) {
      const d = outcome === "request_failed" ? { outcome, error: "connection refused" } : { outcome };
      expect(deferredSubline(d)).toMatch(/nothing waited/i);
    }
  });
});
