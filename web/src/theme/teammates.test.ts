// STUDIO-681 §1.5 / §10 box 1.6 — teammate colors are assigned by ROSTER POSITION,
// never hardcoded per name, and the ramp scales to N teammates.
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import {
  TEAMMATE_COLORS,
  UNKNOWN_TEAMMATE_COLOR,
  teammateColor,
  teammateColorAt,
} from "@/theme/teammates";

const tokensCss = readFileSync(path.resolve(__dirname, "tokens.css"), "utf8");

/** Resolve `var(--mate-n)` to the literal hex it ultimately carries in tokens.css. */
function hexOf(cssVar: string): string {
  let name = /^var\((--[a-z0-9-]+)\)$/i.exec(cssVar)?.[1] ?? cssVar;
  for (let hop = 0; hop < 8; hop++) {
    const value = new RegExp(`${name}\\s*:\\s*([^;]+);`).exec(tokensCss)?.[1]?.trim();
    if (!value) return "";
    const next = /^var\((--[a-z0-9-]+)\)$/i.exec(value)?.[1];
    if (!next) return value.toLowerCase();
    name = next;
  }
  return "";
}

describe("assignment is positional, not per name", () => {
  it("gives the first roster entry the first ramp color whatever it is called", () => {
    expect(teammateColor(["alice", "jimmy"], "alice")).toBe(TEAMMATE_COLORS[0]);
    expect(teammateColor(["zed", "alice"], "zed")).toBe(TEAMMATE_COLORS[0]);
  });

  it("moves a teammate's color when the roster reorders — the name carries nothing", () => {
    // If "alice" were hardcoded to amber this would fail: she is second here.
    expect(teammateColor(["zed", "alice"], "alice")).toBe(TEAMMATE_COLORS[1]);
    expect(teammateColor(["alice", "zed"], "alice")).toBe(TEAMMATE_COLORS[0]);
  });

  it("never resolves a color from the legacy --alice/--jimmy tokens", () => {
    expect(TEAMMATE_COLORS.join(" ")).not.toMatch(/--alice|--jimmy/);
  });
});

describe("the ramp scales to N teammates", () => {
  it("gives a 5-teammate roster 5 distinct colors", () => {
    const roster = ["alice", "jimmy", "robin", "sam", "wren"];
    const assigned = roster.map((name) => teammateColor(roster, name));
    expect(new Set(assigned).size).toBe(5);
  });

  it("gives a 5-teammate roster 5 distinct RENDERED colors, not 5 distinct var names", () => {
    const roster = ["alice", "jimmy", "robin", "sam", "wren"];
    const hexes = roster.map((name) => hexOf(teammateColor(roster, name)));
    expect(hexes.every((h) => /^#[0-9a-f]{6}$/.test(h))).toBe(true);
    expect(new Set(hexes).size).toBe(5);
  });

  it("wraps around the ramp rather than running off its end", () => {
    const n = TEAMMATE_COLORS.length;
    expect(teammateColorAt(n)).toBe(TEAMMATE_COLORS[0]);
    expect(teammateColorAt(n + 2)).toBe(TEAMMATE_COLORS[2]);
  });

  it("keeps every ramp entry distinct so no two adjacent teammates collide", () => {
    const hexes = TEAMMATE_COLORS.map(hexOf);
    expect(new Set(hexes).size).toBe(TEAMMATE_COLORS.length);
  });

  it("keeps the blocked-red out of the ramp", () => {
    expect(TEAMMATE_COLORS.map(hexOf)).not.toContain("#e0685f");
  });
});

describe("unknown input degrades instead of throwing", () => {
  it("falls back to a muted color for a name that is not on the roster", () => {
    expect(teammateColor(["alice"], "nobody")).toBe(UNKNOWN_TEAMMATE_COLOR);
    expect(teammateColor([], "alice")).toBe(UNKNOWN_TEAMMATE_COLOR);
  });

  it("falls back for a negative or non-integer position", () => {
    expect(teammateColorAt(-1)).toBe(UNKNOWN_TEAMMATE_COLOR);
    expect(teammateColorAt(Number.NaN)).toBe(UNKNOWN_TEAMMATE_COLOR);
  });
});
