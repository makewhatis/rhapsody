import { describe, expect, it } from "vitest";
import { stamp } from "@/lib/version-stamp";

describe("stamp", () => {
  it("renders a daemon identity, shortening the full SHA", () => {
    expect(stamp("v0.3.1-8-g581e281", "581e28193d420970a04d545e65087ebf9bbc45e4")).toBe(
      "v0.3.1-8-g581e281 · 581e281",
    );
  });

  it("prefixes a bare release version with v (the shell reports '1.2.0', the daemon 'v1.2.0')", () => {
    expect(stamp("1.2.0", "581e281")).toBe("v1.2.0 · 581e281");
    expect(stamp("v1.2.0", "581e281")).toBe("v1.2.0 · 581e281");
  });

  // Each source has its own "not stamped" sentinel; none may reach the screen as an identity.
  it.each([
    ["dev", "none"],
    ["unknown", "unknown"],
    ["", ""],
  ])("elides the unstamped sentinels (%s/%s)", (version, commit) => {
    expect(stamp(version, commit)).toBe("dev");
  });

  it("keeps a real version when only the commit is unstamped", () => {
    expect(stamp("v0.3.1", "unknown")).toBe("v0.3.1");
  });

  // The shell stamps `$(COMMIT)$(DIRTY)` — an already-short SHA plus a "-dirty" marker. Truncating
  // that to 7 chars would show a modified build as a clean one, so only a full 40-char SHA (the
  // daemon's format) is abbreviated.
  it("preserves the shell's -dirty marker instead of truncating it away", () => {
    expect(stamp("dev", "581e281-dirty")).toBe("dev · 581e281-dirty");
  });

  it("abbreviates only a full 40-character SHA", () => {
    expect(stamp("v1.0.0", "581e28193d420970a04d545e65087ebf9bbc45e4")).toBe("v1.0.0 · 581e281");
    expect(stamp("v1.0.0", "581e281")).toBe("v1.0.0 · 581e281");
  });

  // The footer collapses to one line when the two builds match and shows both when they do not, so
  // equality of the rendered stamps is what decides whether drift is surfaced.
  it("renders equal stamps for the same build and differing ones across builds", () => {
    // The shell reports "0.3.1", the daemon "v0.3.1", for one and the same build — normalization is
    // what lets the footer recognize them as equal and collapse to a single line.
    expect(stamp("0.3.1", "581e281aaa")).toBe(stamp("v0.3.1", "581e281aaa"));
    expect(stamp("v0.3.1", "581e281aaa")).not.toBe(stamp("v0.2.2", "581e281aaa"));
  });
});
