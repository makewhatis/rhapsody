import type { LeadDecision } from "@/lib/api";

/** Lead keys identify manager work, never tracker tickets (STUDIO-1145). */
export function isLeadRun(key: string): boolean {
  return key.startsWith("lead:");
}

export function leadNeedsOperator(decision: LeadDecision): boolean {
  return decision.overruled_at == null && decision.decision.startsWith("escalate:");
}

export function leadNotifications(decisions: readonly LeadDecision[]): LeadDecision[] {
  return decisions.filter((d) => d.overruled_at == null && d.decision !== "applying");
}

/** A bounded, single-line summary; full reasoning lives only on the Lead page. */
export function leadSummary(decision: LeadDecision): string {
  return decision.decision.replace(/^(?:escalate|proposed|done):\s*/, "")
    .replace(/\s+/g, " ").trim().slice(0, 160);
}
