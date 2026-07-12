// Pure view/label logic for the desktop shell, derived from the daemon status snapshot. Kept
// free of React and the Tauri bridge so it is straightforward to unit-test. Ported 1:1 from
// $REF/desktop/frontend/src/status.ts.
import type { StatusDTO } from "./bindings";

export type View = "loading" | "not-configured" | "starting" | "dashboard" | "stopped" | "error";

// viewForStatus decides which screen the shell renders from a status snapshot:
//   - no snapshot yet            → loading
//   - no WORKFLOW.md configured  → not-configured (onboarding)
//   - running & healthy          → dashboard (iframe the loopback UI)
//   - running but not yet healthy / starting → starting
//   - stopped with an error      → error
//   - stopped cleanly            → stopped
export function viewForStatus(s: StatusDTO | null): View {
  if (!s) return "loading";
  if (!s.configured) return "not-configured";
  switch (s.state) {
    case "running":
      return s.healthy ? "dashboard" : "starting";
    case "starting":
      return "starting";
    case "stopped":
      return s.last_err ? "error" : "stopped";
    default:
      return "loading";
  }
}

// statusLabel is the short human label for the header status pill.
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
