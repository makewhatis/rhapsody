import * as React from "react";
import { type StatusDTO, toggleMaximiseWindow } from "@/lib/bindings";
import { viewForStatus, agentText } from "@/lib/daemon-status";
import { Pill } from "@/components/ui/pill";
import { StatusDot } from "@/components/ui/status-dot";
import { Play, Square, RotateCcw, Settings } from "@/components/ui/icons";
import { type HealthState, HEALTH } from "./health";
import { IconButton } from "./IconButton";

export interface TitlebarProps {
  status: StatusDTO | null;
  /** Daemon health (drives the pill). */
  health: HealthState;
  /** Daemon poll interval (ms) from the state API; renders the mono "poll Ns" indicator. */
  pollMs?: number;
  /** A lifecycle action is in flight (disables Start/Stop/Restart). */
  busy?: boolean;
  /** Settings is the active view (highlights the gear). */
  settingsActive?: boolean;
  onStart: () => void;
  onStop: () => void;
  onRestart: () => void;
  /** Toggle the main view between Runs and Settings. */
  onToggleSettings: () => void;
  /** When hosted by Wails (native traffic lights are inset), hide the decorative dots and
   *  inset the bar so it clears the real macOS window controls. Defaults to false (browser
   *  / demo): render the mockup's traffic-light dots. */
  nativeChrome?: boolean;
}

const TRAFFIC = ["#ff5f57", "#febc2e", "#28c840"];

// Titlebar — the macOS window bar. Left: traffic lights, wordmark, and the daemon health pill +
// activity + poll. Right: icon-only daemon controls (Start ▶ / Stop ■ / Restart ⟲) and a Settings
// gear that toggles the main view, each with a hover/focus tooltip. The bar is draggable; the
// control cluster opts out. Start is gated to stopped/errored, Stop is danger, the gear lights when
// Settings is open. (The former Linear/Tools shortcuts + text buttons were dropped in the top-bar
// consolidation — Runs is the whole main area and the gear reaches Settings.)
export function Titlebar({
  status,
  health,
  pollMs,
  busy,
  settingsActive,
  onStart,
  onStop,
  onRestart,
  onToggleSettings,
  nativeChrome = false,
}: TitlebarProps) {
  const view = viewForStatus(status);
  const canStart = view === "stopped" || view === "error";
  const canStop = !(view === "stopped" || view === "not-configured" || view === "loading");
  const canRestart = view !== "not-configured" && view !== "loading";

  const h = HEALTH[health];
  const pollSec = pollMs && pollMs > 0 ? Math.round(pollMs / 1000) : null;
  // Activity is only meaningful while running; otherwise the health pill already says it all
  // (Offline / Connecting / Degraded), so we omit the redundant lifecycle word.
  const activity =
    view === "running" ? (status && status.agent_count > 0 ? agentText(status.agent_count) : "idle") : null;

  // Double-click the bar to zoom the window (standard macOS behaviour). The custom drag region
  // swallows the native title-bar double-click, so drive it explicitly — but ignore double-clicks
  // that land on a control, which render as <button>s.
  const onToggleMaximise = (e: React.MouseEvent) => {
    if ((e.target as HTMLElement).closest("button")) return;
    toggleMaximiseWindow();
  };

  return (
    <div
      onDoubleClick={onToggleMaximise}
      style={
        {
          height: 44,
          flexShrink: 0,
          background: "var(--bg-titlebar)",
          borderBottom: "1px solid var(--line-2)",
          display: "flex",
          alignItems: "center",
          // Inset for the real macOS traffic lights when hosted natively: the left 92px clears the
          // ~74px light cluster, and a little top padding drops the row onto the lights' vertical
          // centre (our 44px bar otherwise centres the content slightly above them).
          padding: nativeChrome ? "6px 16px 0 92px" : "0 16px",
          gap: 14,
          userSelect: "none",
          "--wails-draggable": "drag",
        } as React.CSSProperties
      }
    >
      {!nativeChrome && (
        <div style={{ display: "flex", gap: 8 }} aria-hidden>
          {TRAFFIC.map((c) => (
            <span key={c} style={{ width: 12, height: 12, borderRadius: "50%", background: c }} />
          ))}
        </div>
      )}
      <div style={{ display: "flex", alignItems: "center", gap: 9, marginLeft: nativeChrome ? 0 : 6 }}>
        <span style={{ fontSize: 13.5, fontWeight: 700, letterSpacing: "-0.01em" }}>Symphony</span>
        <Pill tone={h.tone}>
          <StatusDot color={h.dot} pulse={h.pulse} size={6} />
          {h.label}
        </Pill>
        {activity ? <span style={{ fontSize: 12, color: "var(--tx-3)" }}>{activity}</span> : null}
        {pollSec != null ? (
          <span className="mono" style={{ fontSize: 12, color: "var(--tx-faint)" }}>
            poll {pollSec}s
          </span>
        ) : null}
      </div>
      <div style={{ flex: 1 }} />
      <div
        style={{ display: "flex", alignItems: "center", gap: 4, "--wails-draggable": "no-drag" } as React.CSSProperties}
      >
        <IconButton icon={Play} label="Start" onClick={onStart} disabled={busy || !canStart} />
        <IconButton icon={Square} label="Stop" onClick={onStop} danger disabled={busy || !canStop} />
        <IconButton icon={RotateCcw} label="Restart" onClick={onRestart} disabled={busy || !canRestart} />
        <span style={{ width: 1, height: 18, background: "var(--line)", margin: "0 6px" }} />
        <IconButton icon={Settings} label="Settings" onClick={onToggleSettings} active={settingsActive} />
      </div>
    </div>
  );
}
