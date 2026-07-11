// agentState derives a human-friendly "what is the agent doing right now" phase from a
// running session's last stream event. Every session in the live Sessions list has an
// active worker, so the phase is always an active state — distinct from the Linear issue
// state (which stays e.g. "Todo" until the agent hands off at the very end).
export type AgentBadgeVariant = "default" | "secondary" | "muted" | "destructive" | "outline";

export function agentState(lastEvent: string): { label: string; variant: AgentBadgeVariant } {
  const e = (lastEvent || "").toLowerCase();
  if (e === "" || e.includes("started")) return { label: "Starting", variant: "default" };
  if (e.includes("fail") || e.includes("error") || e.includes("timeout"))
    return { label: "Recovering", variant: "secondary" };
  if (e.includes("notification")) return { label: "Working", variant: "default" };
  // turn_completed (between turns) and anything else: actively running.
  return { label: "Running", variant: "default" };
}
