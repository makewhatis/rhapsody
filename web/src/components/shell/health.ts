import type { PillTone } from "@/components/ui/pill";
import type { StatusDTO } from "@/lib/bindings";

export type HealthState = "healthy" | "degraded" | "connecting" | "offline" | "not-configured" | "error";

// bridgeHealth maps the Wails supervisor status (from the Go bridge) onto a HealthState. Used
// when the UI runs inside the Wails host, where daemon health comes from the bridge rather than
// the HTTP /api/v1/state poll (the asset-server origin can't reach the daemon's loopback). A
// null status (not fetched yet) reads as "connecting".
//
// The supervisor's in-progress phase (state "starting", or "running" before the first healthy
// probe) maps to "connecting" so the pill reads "Connecting…" while the daemon comes up — the same
// phase viewForStatus calls "starting". Mapping it to "offline" used to read like a failed launch
// right after Start; "degraded" (amber) misrepresented a daemon that is merely warming up.
//
// The stopped lifecycle is split the same way viewForStatus splits it, so the pill stays honest:
//   - never configured (no WORKFLOW.md, first run) → "not-configured" (neutral "Not configured"),
//     not the same "Offline" a deliberately-stopped daemon shows;
//   - stopped carrying a last_err (the daemon crashed / failed to launch) → "error" (red), so a
//     dead daemon is visually distinct from a clean stop;
//   - stopped cleanly → "offline".
export function bridgeHealth(status: StatusDTO | null): HealthState {
  if (status == null) return "connecting";
  if (status.healthy) return "healthy";
  if (status.state === "running" || status.state === "starting") return "connecting";
  if (!status.configured) return "not-configured";
  if (status.last_err) return "error";
  return "offline";
}

// HEALTH renders each HealthState as a pill: tone + label + a status dot (color + pulse). The
// titlebar shows this inline next to the wordmark. Extracted from the former AppHeader so the
// health vocabulary lives in one place after the top-bar consolidation.
export const HEALTH: Record<HealthState, { tone: PillTone; label: string; pulse: boolean; dot: string }> = {
  healthy: { tone: "emerald", label: "Healthy", pulse: true, dot: "var(--em-bright)" },
  degraded: { tone: "amber", label: "Degraded", pulse: false, dot: "var(--amber)" },
  connecting: { tone: "neutral", label: "Connecting…", pulse: false, dot: "var(--tx-2)" },
  "not-configured": { tone: "neutral", label: "Not configured", pulse: false, dot: "var(--tx-3)" },
  error: { tone: "red", label: "Stopped — error", pulse: false, dot: "var(--red)" },
  offline: { tone: "neutral", label: "Offline", pulse: false, dot: "var(--tx-3)" },
};
