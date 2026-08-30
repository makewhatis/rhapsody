import * as React from "react";
import { StatusDot } from "@/components/ui";
import { Play, Square, RotateCcw, Settings } from "@/components/ui/icons";
import type { ConductorModel } from "@/lib/daemon-status";
import type { TeamsChip } from "@/lib/teams-model";

export interface ToolbarProps {
  /** The derived conductor-status model (see `conductorStatus`) — dot + label + mono detail. */
  conductor: ConductorModel;
  /** The daemon is up. Gates the transport: Play off while running, Stop/Restart off while stopped. */
  running: boolean;
  /** First status still resolving — the transport is disabled until the state is known. */
  connecting?: boolean;
  /** A lifecycle action (start/stop/restart) is in flight — disables the whole transport. */
  busy?: boolean;
  /** Settings is the active route — lights the gear rust. */
  settingsActive?: boolean;
  /** An in-app update is waiting on the user — badges the gear with a rust dot (P11-U3). */
  updateAvailable?: boolean;
  onStart: () => void;
  onStop: () => void;
  onRestart: () => void;
  onToggleSettings: () => void;
  /** Open Linear in the browser (the "Linear ↗" shortcut). */
  onOpenLinear: () => void;
  /** Jump to the Tools settings tab (the "Tools" shortcut). */
  onOpenTools: () => void;
  /**
   * The Teams status chip (STUDIO-652), or null/undefined when Rhapsody Teams is off — which is
   * the SHIPPED state, and why this is optional: absent ⇒ the bar renders exactly as it did before
   * Teams existed, with no chip and no reserved space.
   */
  teams?: TeamsChip | null;
  /** The Teams panel is the active route — lights the chip rust. */
  teamsActive?: boolean;
  /** Open the Teams panel. Only ever called from the chip, which only renders when `teams` is set. */
  onOpenTeams?: () => void;
}

// Toolbar — the single 46px "Podium" toolbar rendered as the first row on every route (P10-D2). It
// replaces the web app's former fake window chrome (traffic-light dots, "Symphony" title, health
// pill, poll badge, play/stop/restart row, standalone gear) AND, in the packaged desktop app, the
// native control strip: one unified bar with the native macOS traffic lights overlaid on its left.
//
// Left→right: the "Rhapsody" wordmark, the conductor-status cluster (a live dot + label + mono
// "daemon healthy · poll Ns"), a spacer, the "Linear ↗" / "Tools" subtle shortcuts, the transport
// segment (Play / Stop / Restart), and the Settings gear. The whole bar is a `data-tauri-drag-region`
// so the window stays draggable, and its left 78px is reserved for the native lights (overlay
// titlebar). Wired to the daemon lifecycle by the shell; inert (no bridge) in a plain browser.
export function Toolbar({
  conductor,
  running,
  connecting = false,
  busy = false,
  settingsActive = false,
  updateAvailable = false,
  onStart,
  onStop,
  onRestart,
  onToggleSettings,
  onOpenLinear,
  onOpenTools,
  teams,
  teamsActive = false,
  onOpenTeams,
}: ToolbarProps) {
  // Transport gating (design spec): while the daemon runs, Play is disabled and Stop/Restart are
  // live; while it is stopped, Play is live and Stop/Restart are disabled. Everything is disabled
  // while a lifecycle action is in flight or the first status is still resolving.
  const canStart = !running && !connecting && !busy;
  const canStop = running && !busy;
  const canRestart = running && !busy;

  return (
    <div
      data-tauri-drag-region=""
      style={{
        height: 46,
        flexShrink: 0,
        display: "flex",
        alignItems: "center",
        gap: 9,
        background: "var(--surface)",
        borderBottom: "1px solid var(--hair-card)",
        // 78px left reserve for the native macOS traffic lights (overlay titlebar); 14px on the right.
        padding: "0 14px 0 78px",
        userSelect: "none",
      }}
    >
      <span style={{ fontSize: 13, fontWeight: 600, color: "var(--ink)", letterSpacing: ".01em" }}>Rhapsody</span>

      {/* Conductor status: live dot + label + mono suffix. */}
      <span style={{ display: "inline-flex", alignItems: "center", gap: 9 }}>
        <StatusDot color={conductor.dot} pulse={conductor.pulse} size={6} />
        <span style={{ fontSize: 12, color: "var(--text-muted)" }}>{conductor.label}</span>
        {conductor.detail ? (
          <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
            {conductor.detail}
          </span>
        ) : null}
      </span>

      <div style={{ flex: 1 }} />

      {teams && onOpenTeams ? <TeamsChipButton chip={teams} active={teamsActive} onClick={onOpenTeams} /> : null}

      <SubtleButton label="Open Linear" onClick={onOpenLinear}>
        Linear <span style={{ color: "var(--faint)", fontSize: 11 }}>↗</span>
      </SubtleButton>
      <SubtleButton label="Tools" onClick={onOpenTools}>
        Tools
      </SubtleButton>

      <Divider />

      <div style={{ display: "inline-flex", border: "1px solid var(--hair-strong)", borderRadius: 7, overflow: "hidden" }}>
        <TransportCell label="Start" title="Start daemon" enabled={canStart} onClick={onStart} first>
          <Play size={13} />
        </TransportCell>
        <TransportCell label="Stop" title="Stop daemon" enabled={canStop} onClick={onStop}>
          <Square size={11} />
        </TransportCell>
        <TransportCell label="Restart" title="Restart daemon" enabled={canRestart} onClick={onRestart}>
          <RotateCcw size={13} />
        </TransportCell>
      </div>

      <Divider />

      <GearButton active={settingsActive} onClick={onToggleSettings} updateAvailable={updateAvailable} />
    </div>
  );
}

// TeamsChipButton — the "Teams: N teammates, M live" status chip (STUDIO-652). It sits with the
// toolbar's other shortcuts and opens the Teams panel. A live dot pulses whenever anything is
// running as a teammate, so the strip answers "is the team working right now" at a glance.
//
// It renders ONLY when the daemon reports Teams enabled. That is the whole inertness contract at
// the UI layer: no chip, no reserved gap, no layout change of any kind on a Teams-off daemon.
function TeamsChipButton({ chip, active, onClick }: { chip: TeamsChip; active: boolean; onClick: () => void }) {
  const [hover, setHover] = React.useState(false);
  const live = chip.live > 0;
  return (
    <button
      type="button"
      aria-label={chip.label}
      aria-pressed={active}
      title="Open the Teams panel"
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 7,
        fontSize: 12,
        color: active ? "var(--rust-text)" : hover ? "var(--ink)" : "var(--btn-label)",
        padding: "5px 11px",
        borderRadius: 7,
        border: `1px solid ${active || hover ? "rgba(255,255,255,.14)" : "var(--hair-control)"}`,
        background: active ? "var(--tint-active-nav)" : hover ? "rgba(255,255,255,.06)" : "rgba(255,255,255,.03)",
        cursor: "pointer",
        transition: "color .12s, background .12s, border-color .12s",
        whiteSpace: "nowrap",
      }}
    >
      <StatusDot color={live ? "var(--emerald)" : "var(--tx-faint)"} size={6} pulse={live} />
      {chip.label}
    </button>
  );
}

// SetupToolbar — the first-run wizard's toolbar variant (mock 2e): a 42px bar with only the native
// traffic-lights reserve, the "Rhapsody" wordmark, and a right-aligned "SETUP" caps marker. There is
// no daemon yet, so it deliberately drops the conductor-status cluster, the transport segment, and
// the Settings gear. Like the full Toolbar it is a `data-tauri-drag-region` (window stays draggable)
// with the left 78px reserved for the overlay-titlebar lights.
export function SetupToolbar() {
  return (
    <div
      data-tauri-drag-region=""
      style={{
        height: 42,
        flexShrink: 0,
        display: "flex",
        alignItems: "center",
        gap: 9,
        background: "var(--surface)",
        borderBottom: "1px solid var(--hair-card)",
        padding: "0 16px 0 78px",
        userSelect: "none",
      }}
    >
      <span style={{ fontSize: 13, fontWeight: 600, color: "var(--ink)", letterSpacing: ".01em" }}>Rhapsody</span>
      <div style={{ flex: 1 }} />
      <span
        style={{
          fontSize: 10,
          fontWeight: 600,
          letterSpacing: ".14em",
          textTransform: "uppercase",
          color: "var(--faint)",
        }}
      >
        Setup
      </span>
    </div>
  );
}

// A 1×18px hairline divider with 4px horizontal margins (design spec).
function Divider() {
  return <span aria-hidden style={{ width: 1, height: 18, background: "var(--hair-control)", margin: "0 4px" }} />;
}

// SubtleButton — the toolbar's low-emphasis text shortcut ("Linear ↗", "Tools"). Bordered, faint
// label; hover lifts the border/background and brightens the text (design Interactions).
function SubtleButton({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      aria-label={label}
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        fontSize: 12,
        color: hover ? "var(--ink)" : "var(--btn-label)",
        padding: "5px 11px",
        borderRadius: 7,
        border: `1px solid ${hover ? "rgba(255,255,255,.14)" : "var(--hair-control)"}`,
        background: hover ? "rgba(255,255,255,.06)" : "rgba(255,255,255,.03)",
        cursor: "pointer",
        transition: "color .12s, background .12s, border-color .12s",
        whiteSpace: "nowrap",
      }}
    >
      {children}
    </button>
  );
}

// TransportCell — one 34×28 cell of the bordered transport segment. Enabled cells carry the icon on
// a faint fill and brighten on hover; disabled cells dim (design spec).
function TransportCell({
  label,
  title,
  enabled,
  onClick,
  first,
  children,
}: {
  label: string;
  title: string;
  enabled: boolean;
  onClick: () => void;
  first?: boolean;
  children: React.ReactNode;
}) {
  const [hover, setHover] = React.useState(false);
  const bg = !enabled ? "rgba(255,255,255,.02)" : hover ? "rgba(255,255,255,.08)" : "rgba(255,255,255,.04)";
  return (
    <button
      type="button"
      aria-label={label}
      title={title}
      disabled={!enabled}
      onClick={enabled ? onClick : undefined}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        width: 34,
        height: 28,
        display: "grid",
        placeItems: "center",
        border: "none",
        borderLeft: first ? "none" : "1px solid rgba(255,255,255,.08)",
        background: bg,
        color: enabled ? "var(--text-2)" : "var(--faint)",
        cursor: enabled ? "pointer" : "default",
        transition: "background .12s",
      }}
    >
      {children}
    </button>
  );
}

// GearButton — the 30×28 Settings gear. Muted by default; rust while any Settings route is active. A
// rust dot rides the top-right corner when an in-app update is waiting (P11-U3), guiding the user to
// the Settings "Updates" surface without stealing the gear's own active/hover tint.
function GearButton({
  active,
  onClick,
  updateAvailable,
}: {
  active: boolean;
  onClick: () => void;
  updateAvailable: boolean;
}) {
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      aria-label="Settings"
      aria-pressed={active}
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        position: "relative",
        width: 30,
        height: 28,
        display: "grid",
        placeItems: "center",
        border: "1px solid transparent",
        borderRadius: 7,
        background: hover ? "rgba(255,255,255,.06)" : "transparent",
        color: active ? "var(--rust-text)" : "var(--text-muted)",
        cursor: "pointer",
        transition: "background .12s, color .12s",
      }}
    >
      <Settings size={16} />
      {updateAvailable ? (
        // role="img" + aria-label gives the decorative StatusDot an accessible, testable name so the
        // pending update is announced from the toolbar (mirrors the rail's Tools warning-dot idiom).
        <span
          role="img"
          aria-label="Update available"
          style={{ position: "absolute", top: 3, right: 3, display: "inline-flex" }}
        >
          <StatusDot color="var(--rust-text)" size={6} />
        </span>
      ) : null}
    </button>
  );
}
