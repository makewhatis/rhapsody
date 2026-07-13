// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { Toolbar, type ToolbarProps } from "@/components/Toolbar";
import { conductorStatus } from "@/lib/daemon-status";

afterEach(cleanup);

// A playing daemon with one agent, unless the case overrides the conductor/running signals.
function props(overrides: Partial<ToolbarProps> = {}): ToolbarProps {
  return {
    conductor: conductorStatus({
      connecting: false,
      reachable: true,
      running: true,
      degraded: false,
      agentCount: 1,
      pollMs: 2000,
    }),
    running: true,
    connecting: false,
    busy: false,
    settingsActive: false,
    onStart: vi.fn(),
    onStop: vi.fn(),
    onRestart: vi.fn(),
    onToggleSettings: vi.fn(),
    onOpenLinear: vi.fn(),
    onOpenTools: vi.fn(),
    ...overrides,
  };
}

describe("Toolbar", () => {
  it("renders the Rhapsody wordmark and the conductor status cluster", () => {
    render(<Toolbar {...props()} />);
    expect(screen.getByText("Rhapsody")).toBeTruthy();
    expect(screen.getByText("Playing — 1 agent")).toBeTruthy();
    expect(screen.getByText("daemon healthy · poll 2s")).toBeTruthy();
    // no fake "Symphony" wordmark, no fake traffic-light dots, no health pill / poll badge survive
    expect(screen.queryByText("Symphony")).toBeNull();
    expect(screen.queryByText("Healthy")).toBeNull();
  });

  it("marks the bar as a Tauri drag region so the window stays draggable", () => {
    const { container } = render(<Toolbar {...props()} />);
    expect(container.querySelector("[data-tauri-drag-region]")).not.toBeNull();
  });

  it("gates the transport while the daemon runs: Play off, Stop + Restart on", () => {
    const onStop = vi.fn();
    render(<Toolbar {...props({ running: true, onStop })} />);
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Restart" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(onStop).toHaveBeenCalledOnce();
  });

  it("gates the transport while the daemon is stopped: Play on, Stop + Restart off", () => {
    const onStart = vi.fn();
    const conductor = conductorStatus({ connecting: false, reachable: true, running: false, degraded: false, agentCount: 0 });
    render(<Toolbar {...props({ running: false, conductor, onStart })} />);
    expect(screen.getByText("Paused")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Start" }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Restart" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    expect(onStart).toHaveBeenCalledOnce();
  });

  it("disables the whole transport while an action is in flight", () => {
    render(<Toolbar {...props({ busy: true })} />);
    for (const name of ["Start", "Stop", "Restart"]) {
      expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true);
    }
  });

  it("disables the whole transport while the first status is still connecting", () => {
    const conductor = conductorStatus({ connecting: true, reachable: true, running: false, degraded: false, agentCount: 0 });
    render(<Toolbar {...props({ connecting: true, running: false, conductor })} />);
    for (const name of ["Start", "Stop", "Restart"]) {
      expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true);
    }
  });

  it("toggles Settings via the gear and reflects the active state", () => {
    const onToggleSettings = vi.fn();
    render(<Toolbar {...props({ settingsActive: true, onToggleSettings })} />);
    const gear = screen.getByRole("button", { name: "Settings" });
    expect(gear.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(gear);
    expect(onToggleSettings).toHaveBeenCalledOnce();
  });

  it("fires the Linear and Tools shortcuts", () => {
    const onOpenLinear = vi.fn();
    const onOpenTools = vi.fn();
    render(<Toolbar {...props({ onOpenLinear, onOpenTools })} />);
    fireEvent.click(screen.getByRole("button", { name: /Linear/ }));
    expect(onOpenLinear).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Tools" }));
    expect(onOpenTools).toHaveBeenCalledOnce();
  });
});
