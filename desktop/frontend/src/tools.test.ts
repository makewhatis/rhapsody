import { describe, expect, it } from "vitest";
import { remediationHint, statusBadge, toolSummary } from "./tools";
import type { ToolResult } from "./bindings";

function tool(over: Partial<ToolResult>): ToolResult {
  return { name: "claude", path: "", found: false, healthy: false, version: "", detail: "", ...over };
}

describe("toolSummary", () => {
  it("counts healthy/missing/unhealthy and reports allHealthy", () => {
    const s = toolSummary([
      tool({ name: "claude", found: true, healthy: true }),
      tool({ name: "gh", found: false }),
      tool({ name: "gt", found: true, healthy: false }),
    ]);
    expect(s).toEqual({ total: 3, healthy: 1, missing: 1, unhealthy: 1, allHealthy: false });
  });

  it("allHealthy only when every tool is healthy", () => {
    const s = toolSummary([
      tool({ found: true, healthy: true }),
      tool({ name: "gh", found: true, healthy: true }),
    ]);
    expect(s.allHealthy).toBe(true);
  });

  it("allHealthy is false for an empty set", () => {
    expect(toolSummary([]).allHealthy).toBe(false);
  });
});

describe("remediationHint", () => {
  it("suggests install/override when missing", () => {
    expect(remediationHint(tool({ name: "gh", found: false }))).toMatch(/not found/i);
  });
  it("uses the probe detail when present-but-unhealthy", () => {
    expect(remediationHint(tool({ found: true, healthy: false, detail: "not logged in" }))).toBe("not logged in");
  });
  it("OK when healthy", () => {
    expect(remediationHint(tool({ found: true, healthy: true }))).toBe("OK");
  });
});

describe("statusBadge", () => {
  it("missing / error / ok", () => {
    expect(statusBadge(tool({ found: false }))).toBe("missing");
    expect(statusBadge(tool({ found: true, healthy: false }))).toBe("error");
    expect(statusBadge(tool({ found: true, healthy: true }))).toBe("ok");
  });
});
