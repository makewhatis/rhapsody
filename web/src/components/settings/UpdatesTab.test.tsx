// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { UpdatesTab } from "./UpdatesTab";
import type { Updater } from "@/hooks/useUpdater";
import type { UpdateInfo } from "@/lib/bindings";

afterEach(cleanup);

function info(over: Partial<UpdateInfo> = {}): UpdateInfo {
  return { available: true, version: "1.4.0", current_version: "1.3.0", notes: "Fixes the sync bug.", ...over };
}

// A stub Updater in a chosen phase, with spy actions so a test can assert what a control fires.
function stub(over: Partial<Updater> = {}): Updater {
  return {
    phase: "idle",
    info: null,
    progress: null,
    error: null,
    activeRunsPrompt: null,
    drainOutcome: null,
    pending: false,
    check: vi.fn(),
    download: vi.fn(),
    requestInstall: vi.fn(),
    confirmInstallNow: vi.fn(),
    drainThenInstall: vi.fn(),
    deferToQuit: vi.fn(),
    dismissPrompt: vi.fn(),
    ...over,
  };
}

describe("UpdatesTab", () => {
  it("idle: offers a manual check that fires updater.check", () => {
    const u = stub({ phase: "idle" });
    render(<UpdatesTab updater={u} />);
    fireEvent.click(screen.getByRole("button", { name: /check for updates/i }));
    expect(u.check).toHaveBeenCalledOnce();
  });

  it("up-to-date: shows the running version and keeps the check button", () => {
    const u = stub({ phase: "up-to-date", info: info({ available: false, version: "", notes: "" }) });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/up to date|latest/i)).toBeTruthy();
    expect(screen.getByText(/1\.3\.0/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /check for updates/i })).toBeTruthy();
  });

  it("available: shows the version, reveals release notes, and downloads", () => {
    const u = stub({ phase: "available", info: info(), pending: true });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/update available/i)).toBeTruthy();
    expect(screen.getByText(/1\.4\.0/)).toBeTruthy();
    // "What's new" is collapsed until opened.
    expect(screen.queryByText("Fixes the sync bug.")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /what.?s new/i }));
    expect(screen.getByText("Fixes the sync bug.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /^download/i }));
    expect(u.download).toHaveBeenCalledOnce();
  });

  it("downloading: renders a determinate progress readout", () => {
    const u = stub({
      phase: "downloading",
      info: info(),
      pending: true,
      progress: { downloaded: 5 * 1024 * 1024, total: 10 * 1024 * 1024 },
    });
    render(<UpdatesTab updater={u} />);
    const bar = screen.getByRole("progressbar");
    expect(bar.getAttribute("aria-valuenow")).toBe("50");
    expect(screen.getByText(/5\.0 MB of 10\.0 MB/)).toBeTruthy();
  });

  it("ready: 'Restart to finish' fires requestInstall", () => {
    const u = stub({ phase: "ready", info: info(), pending: true });
    render(<UpdatesTab updater={u} />);
    fireEvent.click(screen.getByRole("button", { name: /restart to finish/i }));
    expect(u.requestInstall).toHaveBeenCalledOnce();
  });

  it("deferred: explains the next-quit install and offers a restart-now override", () => {
    const u = stub({ phase: "deferred", info: info(), pending: true });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/next quit|when you quit/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /restart.*now/i }));
    expect(u.confirmInstallNow).toHaveBeenCalledOnce();
  });

  it("error: surfaces the message and lets the user retry the check", () => {
    const u = stub({ phase: "error", error: "network unreachable" });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/network unreachable/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /check for updates|try again/i }));
    expect(u.check).toHaveBeenCalledOnce();
  });

  it("active-runs warn dialog: confirm stops the agents, defer installs on next quit", () => {
    const u = stub({ phase: "ready", info: info(), pending: true, activeRunsPrompt: 3 });
    render(<UpdatesTab updater={u} />);
    // The warning names the count with the "playing" display label.
    expect(screen.getByText(/3 agents are playing/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /install on next quit/i }));
    expect(u.deferToQuit).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: /update now/i }));
    expect(u.confirmInstallNow).toHaveBeenCalledOnce();
  });

  // STUDIO-880 — the choice this ticket exists to make possible. Before it, the dialog offered
  // three and its body said "Updating now will stop them", which was accurate only BECAUSE no
  // button could ask for a drain.
  it("active-runs warn dialog: offers waiting for the agents, and says nothing is interrupted", () => {
    const u = stub({ phase: "ready", info: info(), pending: true, activeRunsPrompt: 1 });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/nothing is interrupted/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /wait, then update/i }));
    expect(u.drainThenInstall).toHaveBeenCalledOnce();
    expect(u.confirmInstallNow).not.toHaveBeenCalled();
  });

  it("draining: explains why nothing appears to be happening", () => {
    const u = stub({ phase: "draining", info: info(), pending: true });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/waiting for the agents to finish/i)).toBeTruthy();
    expect(screen.getByText(/no new work is being started/i)).toBeTruthy();
  });

  // An expired budget leaves the daemon drained. "Scheduled for your next quit" alone would send
  // the operator away from a daemon that has silently stopped taking work.
  it("deferred after an expired drain: says the daemon is still draining", () => {
    const u = stub({
      phase: "deferred",
      info: info(),
      pending: true,
      drainOutcome: { outcome: "expired", running: 2, waited_secs: 1800 },
    });
    render(<UpdatesTab updater={u} />);
    expect(screen.getByText(/still draining/i)).toBeTruthy();
  });
});
