// Contract tests for the "Podium" warm-dark design tokens, keyframes, globals and the
// switch to system fonts (P10-D1, TRA-244). These assert the CSS source declares the
// canonical token set / animation set — a browser-free way to guard the "match the design
// spec detail-for-detail" acceptance for the foundation layer. Pixel/hex values come from
// the P10 design handoff's Design Tokens table.
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const cssPath = path.resolve(__dirname, "index.css");
const css = readFileSync(cssPath, "utf8");

// Match `--name:` <value>, tolerant of whitespace after the colon and regex-special chars.
function declares(name: string, value: string): RegExp {
  return new RegExp(`${name.replace(/-/g, "\\-")}\\s*:\\s*${value.replace(/[().#,\s]/g, (m) => (m.trim() === "" ? "\\s*" : "\\" + m))}`);
}

describe("Podium palette — canonical color tokens", () => {
  const colors: Record<string, string> = {
    // backgrounds
    "--ground": "#141210",
    "--surface": "#1c1916",
    "--well": "#100e0c",
    "--card": "#1a1714",
    // text ramp
    "--ink": "#ede7e1",
    "--text-2": "#d8d1c8",
    "--text-muted": "#a59c90",
    "--btn-label": "#c9c1b8",
    "--faint": "#6e675e",
    "--ghost": "#57504a",
    "--neutral": "#8b847b",
    // rust accent
    "--rust": "#c25b2e",
    "--rust-hover": "#b14f27",
    "--rust-text": "#e08653",
    "--on-rust": "#fff6f0",
    // status
    "--sage": "#97ae87",
    "--slate": "#86a9c6",
    "--amber": "#cda35a",
    "--red": "#e0574c",
  };

  for (const [name, value] of Object.entries(colors)) {
    it(`declares ${name}: ${value}`, () => {
      expect(css).toMatch(declares(name, value));
    });
  }
});

describe("Podium tints (pill/row backgrounds)", () => {
  it("declares the status tints at 10–12%", () => {
    expect(css).toMatch(/--tint-rust:\s*rgba\(224,\s*134,\s*83,\s*0?\.12\)/);
    expect(css).toMatch(/--tint-sage:\s*rgba\(151,\s*174,\s*135,\s*0?\.10?\)/);
    expect(css).toMatch(/--tint-amber:\s*rgba\(205,\s*163,\s*90,\s*0?\.10?\)/);
    expect(css).toMatch(/--tint-red:\s*rgba\(224,\s*87,\s*76,\s*0?\.10?\)/);
    expect(css).toMatch(/--tint-slate:\s*rgba\(134,\s*169,\s*198,\s*0?\.10?\)/);
    expect(css).toMatch(/--tint-neutral:\s*rgba\(139,\s*132,\s*123,\s*0?\.10?\)/);
  });

  it("declares the row/nav tints (playing row, warn row, active nav)", () => {
    expect(css).toMatch(/--tint-playing-row:\s*rgba\(194,\s*91,\s*46,\s*0?\.045\)/);
    expect(css).toMatch(/--tint-warn-row:\s*rgba\(205,\s*163,\s*90,\s*0?\.05\)/);
    expect(css).toMatch(/--tint-active-nav:\s*rgba\(224,\s*134,\s*83,\s*0?\.09\)/);
  });
});

describe("Podium hairlines, focus, radii", () => {
  it("declares the hairline ladder (rows .05 → dashed .12)", () => {
    expect(css).toMatch(/--hair-row:\s*rgba\(255,\s*255,\s*255,\s*0?\.05\)/);
    expect(css).toMatch(/--hair-section:\s*rgba\(255,\s*255,\s*255,\s*0?\.06\)/);
    expect(css).toMatch(/--hair-card:\s*rgba\(255,\s*255,\s*255,\s*0?\.07\)/);
    expect(css).toMatch(/--hair-control:\s*rgba\(255,\s*255,\s*255,\s*0?\.09\)/);
    expect(css).toMatch(/--hair-dashed:\s*rgba\(255,\s*255,\s*255,\s*0?\.12\)/);
  });

  it("declares the rust focus border + ring", () => {
    expect(css).toMatch(/--focus:\s*#c25b2e/);
    expect(css).toMatch(/--focus-ring:\s*rgba\(194,\s*91,\s*46,\s*0?\.25\)/);
  });

  it("declares the radius scale (999 pill · 10 card · 7 ctrl · 5 chip · 4 keycap)", () => {
    expect(css).toMatch(/--r-pill:\s*999px/);
    expect(css).toMatch(/--r-card:\s*10px/);
    expect(css).toMatch(/--r-ctrl:\s*7px/);
    expect(css).toMatch(/--r-chip:\s*5px/);
    expect(css).toMatch(/--r-keycap:\s*4px/);
  });
});

describe("legacy INF-225 aliases re-point onto the Podium palette", () => {
  // The not-yet-restructured screens still read the `--em*` / `--tx*` names; they must
  // resolve to the Podium palette so nothing renders on the retired emerald values.
  it("aliases the emerald accent onto rust", () => {
    expect(css).toMatch(/--em-bright:\s*var\(--rust-text\)/);
    expect(css).toMatch(/--em:\s*var\(--rust\)/);
    expect(css).toMatch(/--on-em:\s*var\(--on-rust\)/);
  });
  it("aliases the text ramp onto the Podium ink/muted/faint", () => {
    expect(css).toMatch(/--tx:\s*var\(--ink\)/);
    expect(css).toMatch(/--tx-2:\s*var\(--text-muted\)/);
  });
});

describe("shadcn semantic tokens resolve to the Podium palette", () => {
  it("drives primary/ring/card/border/foreground off the Podium tokens", () => {
    expect(css).toMatch(/--primary:\s*var\(--rust\)/);
    expect(css).toMatch(/--ring:\s*var\(--focus-ring\)/);
    expect(css).toMatch(/--card:\s*#1a1714/);
    expect(css).toMatch(/--border:\s*var\(--hair-card\)/);
    expect(css).toMatch(/--foreground:\s*var\(--ink\)/);
  });
});

describe("keyframes + reduced-motion", () => {
  it("defines the Podium pulse (live dots) and blink (live caret) keyframes", () => {
    expect(css).toMatch(/@keyframes\s+pulse\b/);
    expect(css).toMatch(/@keyframes\s+blink\b/);
  });

  it("keeps the shell animation set (spin/shimmer/fadeUp/sheetIn/overlayIn/toastIn)", () => {
    for (const name of ["spin", "shimmer", "fadeUp", "sheetIn", "overlayIn", "toastIn"]) {
      expect(css).toMatch(new RegExp(`@keyframes\\s+${name}\\b`));
    }
  });

  it("guards all motion behind prefers-reduced-motion: reduce", () => {
    expect(css).toMatch(/@media\s*\(prefers-reduced-motion:\s*reduce\)/);
    expect(css).toMatch(/animation-duration:\s*0?\.001ms\s*!important/);
  });
});

describe("global treatments", () => {
  it("defines the custom 11px scrollbars", () => {
    expect(css).toMatch(/::-webkit-scrollbar\s*\{[^}]*width:\s*11px/);
    expect(css).toMatch(/::-webkit-scrollbar-thumb/);
  });

  it("defines a rust ::selection highlight", () => {
    expect(css).toMatch(/::selection\s*\{[^}]*rgba\(194,\s*91,\s*46/);
  });

  it("defines the .mono helper with tabular figures", () => {
    expect(css).toMatch(/\.mono\s*\{[\s\S]*?font-variant-numeric:\s*tabular-nums/);
  });

  it("paints the rust focus ring on native controls (not just outline:none)", () => {
    expect(css).toMatch(
      /input:focus-visible[\s\S]{0,160}box-shadow:\s*0 0 0 3px var\(--focus-ring\)/,
    );
  });
});

describe("system fonts only — no webfonts", () => {
  it("declares no @font-face and loads nothing from a CDN", () => {
    expect(css).not.toMatch(/@font-face/);
    expect(css).not.toMatch(/fonts\.googleapis\.com/);
    expect(css).not.toMatch(/fonts\.gstatic\.com/);
    expect(css).not.toMatch(/\.woff2?/);
    expect(css).not.toMatch(/Geist/);
  });

  it("maps the Tailwind sans/mono theme fonts onto the system stack", () => {
    expect(css).toMatch(/--font-sans:\s*-apple-system/);
    expect(css).toMatch(/--font-mono:\s*ui-monospace/);
  });
});
