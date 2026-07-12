// Pure helpers for the Tool-doctor panel, derived from the probe results. Ported from
// $REF/desktop/frontend/src/tools.ts. Kept free of React/Tauri so they are easy to unit-test.
import type { ToolResult } from "./bindings";

export interface ToolSummary {
  total: number;
  healthy: number;
  missing: number;
  unhealthy: number;
  allHealthy: boolean;
}

export function toolSummary(results: ToolResult[]): ToolSummary {
  let healthy = 0;
  let missing = 0;
  let unhealthy = 0;
  for (const r of results) {
    if (!r.found) missing++;
    else if (r.healthy) healthy++;
    else unhealthy++;
  }
  return {
    total: results.length,
    healthy,
    missing,
    unhealthy,
    allHealthy: results.length > 0 && healthy === results.length,
  };
}

// remediationHint is the actionable next step for a tool's state (spec §6 remediation).
export function remediationHint(r: ToolResult): string {
  if (!r.found) return `${r.name} not found — install it or set an override path.`;
  if (!r.healthy) return r.detail || `${r.name} failed its version check.`;
  return "OK";
}

// statusBadge maps a result to a short label for the UI.
export function statusBadge(r: ToolResult): "ok" | "missing" | "error" {
  if (!r.found) return "missing";
  if (!r.healthy) return "error";
  return "ok";
}
