import { describe, expect, it } from "vitest";
import { DEPENDENCY_MODES, DEPENDENCY_MODE_HINT, GLOBAL_DEFAULTS } from "@/lib/settings-data";

// dependency_mode UI options (INF-320). The value set is the three-valued enum disabled|graphite|dag
// (seed "disabled"), mirroring GIT_WORKFLOWS' shape only. The default is NOT derived from git_flow.
describe("DEPENDENCY_MODES options + seed + hint copy", () => {
  it("offers exactly three options disabled/graphite/dag with disabled first and non-empty notes", () => {
    expect(DEPENDENCY_MODES.map((o) => o.value)).toEqual(["disabled", "graphite", "dag"]);
    for (const o of DEPENDENCY_MODES) {
      expect(o.label.trim()).not.toBe("");
      expect((o.note ?? "").trim()).not.toBe("");
    }
    // The disabled option flags itself as the default / today's behavior in its dropdown note.
    expect(DEPENDENCY_MODES[0].note?.toLowerCase()).toContain("default");
  });

  it("seeds the global default to a flat 'disabled' (the live value comes from the daemon)", () => {
    expect(GLOBAL_DEFAULTS.dependencyMode).toBe("disabled");
  });

  it("documents all three modes, their thresholds, the trade-off, and that disabled is the default", () => {
    expect(DEPENDENCY_MODE_HINT).toContain("Disabled");
    expect(DEPENDENCY_MODE_HINT).toContain("Graphite");
    expect(DEPENDENCY_MODE_HINT).toContain("DAG");
    expect(DEPENDENCY_MODE_HINT.toLowerCase()).toContain("default");
    expect(DEPENDENCY_MODE_HINT).toContain("In Review");
    expect(DEPENDENCY_MODE_HINT.toLowerCase()).toContain("merged");
    expect(DEPENDENCY_MODE_HINT.toLowerCase()).toContain("parallel");
  });

  it("never mentions git_flow / git workflow (the derivation was removed from the locked design)", () => {
    expect(DEPENDENCY_MODE_HINT.toLowerCase()).not.toContain("git_flow");
    expect(DEPENDENCY_MODE_HINT.toLowerCase()).not.toContain("git workflow");
  });
});
