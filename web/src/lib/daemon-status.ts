// Pure view/label logic for the app shell, derived from the supervisor status snapshot.
// Kept free of React and the Wails bridge so it is straightforward to unit-test. Moved into
// `web/` from the desktop shell (INF-225).
import type { StatusDTO } from "./bindings";

export type DaemonView = "loading" | "not-configured" | "starting" | "running" | "stopped" | "error";

// viewForStatus reduces a status snapshot to a single lifecycle phase:
//   - no snapshot yet (e.g. plain browser, bridge absent) → loading
//   - no WORKFLOW.md configured                            → not-configured
//   - running & healthy                                    → running
//   - running but not yet healthy / starting               → starting
//   - stopped with an error                                → error
//   - stopped cleanly                                      → stopped
export function viewForStatus(s: StatusDTO | null): DaemonView {
  if (!s) return "loading";
  if (!s.configured) return "not-configured";
  switch (s.state) {
    case "running":
      return s.healthy ? "running" : "starting";
    case "starting":
      return "starting";
    case "stopped":
      return s.last_err ? "error" : "stopped";
    default:
      return "loading";
  }
}

// statusLabel is the short human label for the titlebar status (e.g. "Running — idle").
export function statusLabel(s: StatusDTO | null): string {
  if (!s) return "Loading…";
  if (!s.configured) return "Not configured";
  switch (s.state) {
    case "running":
      if (!s.healthy) return "Starting…";
      return s.agent_count > 0 ? `Running — ${agentText(s.agent_count)}` : "Running — idle";
    case "starting":
      return "Starting…";
    case "stopped":
      return s.last_err ? "Stopped (error)" : "Stopped";
    default:
      return s.state;
  }
}

// agentText renders the active agent count with correct pluralization.
export function agentText(n: number): string {
  return n === 1 ? "1 agent" : `${n} agents`;
}

// ---- Conductor status (P10-D2 "Podium" unified toolbar) ---------------------------------------
// The toolbar's conductor-status cluster reduces the daemon's reachability + running state + active
// ("playing") agent count to one model — the design spec's four states (Playing / Idle / Paused /
// Unreachable) plus a neutral Connecting… while the first status is still resolving. Kept pure and
// React-free so it is unit-tested here and the Toolbar simply renders the result. The colors are the
// Podium status tokens (rust-text / neutral / amber / red); the mono `detail` is the toolbar's small
// "daemon healthy · poll Ns" suffix (or "retrying…" while unreachable).

export type ConductorPhase = "playing" | "idle" | "degraded" | "paused" | "unreachable" | "connecting";

// The normalized signals the toolbar derives from whichever status source it has: the Wails bridge
// (native host) or the HTTP /api/v1/state poll (the daemon's own origin / desktop reverse-proxy).
export interface ConductorSignals {
  /** The first status is not known yet (initial load) → a neutral "Connecting…". */
  connecting: boolean;
  /** The daemon can be reached at all; false → "Daemon unreachable" (retrying). */
  reachable: boolean;
  /** The daemon process is up and serving; false (but reachable) → "Paused". */
  running: boolean;
  /** Up but reporting degraded health — tints the dot amber. */
  degraded: boolean;
  /** Active ("playing") agent count; drives Playing vs Idle and the pluralized label. */
  agentCount: number;
  /** Poll cadence (ms) for the mono suffix "daemon healthy · poll Ns"; omitted → no suffix. */
  pollMs?: number;
}

export interface ConductorModel {
  phase: ConductorPhase;
  /** Primary label, e.g. "Playing — 1 agent", "Idle — watching for tickets", "Paused". */
  label: string;
  /** Status-dot color (a CSS custom-property reference). */
  dot: string;
  /** Whether the status dot pulses (only a live "playing" ensemble does). */
  pulse: boolean;
  /** Mono suffix, e.g. "daemon healthy · poll 2s" / "retrying…" (may be ""). */
  detail: string;
}

export function conductorStatus(sig: ConductorSignals): ConductorModel {
  const withPoll = (head: string): string => {
    const sec = sig.pollMs && sig.pollMs > 0 ? Math.round(sig.pollMs / 1000) : null;
    return sec != null ? `${head} · poll ${sec}s` : head;
  };
  if (sig.connecting) {
    return { phase: "connecting", label: "Connecting…", dot: "var(--neutral)", pulse: false, detail: "" };
  }
  if (!sig.reachable) {
    return { phase: "unreachable", label: "Daemon unreachable", dot: "var(--red)", pulse: false, detail: "retrying…" };
  }
  if (!sig.running) {
    return { phase: "paused", label: "Paused", dot: "var(--amber)", pulse: false, detail: "" };
  }
  // Running: Playing (>=1 agent) or Idle (0). Degraded keeps the running label so the agent count
  // stays visible, but tints the dot amber and annotates the suffix.
  const playing = sig.agentCount > 0;
  const label = playing ? `Playing — ${agentText(sig.agentCount)}` : "Idle — watching for tickets";
  return {
    phase: sig.degraded ? "degraded" : playing ? "playing" : "idle",
    label,
    dot: sig.degraded ? "var(--amber)" : playing ? "var(--rust-text)" : "var(--neutral)",
    pulse: playing && !sig.degraded,
    detail: withPoll(sig.degraded ? "daemon degraded" : "daemon healthy"),
  };
}
