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
