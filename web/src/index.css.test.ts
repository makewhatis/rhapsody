// Contract tests for the Symphony dark-emerald design tokens, keyframes, scrollbars
// and self-hosted Geist fonts ported from the Claude Design package
// (`Symphony Settings.html :root`). These assert the CSS source declares the canonical
// token set / animation set and that the font files are present — a browser-free way to
// guard the "match the mockup detail-for-detail" acceptance for the foundation layer.
import { describe, it, expect } from "vitest";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";

const cssPath = path.resolve(__dirname, "index.css");
const css = readFileSync(cssPath, "utf8");
const publicDir = path.resolve(__dirname, "../public");

describe("design tokens (Symphony dark-emerald palette)", () => {
  // Exact hex/rgba values from the design package's :root block. Map of token -> value.
  const tokens: Record<string, string> = {
    // surfaces
    "--bg-window": "#060807",
    "--bg-app": "#0a0d0c",
    "--bg-titlebar": "#0d100f",
    "--bg-card": "#101513",
    "--bg-card-2": "#0d1110",
    "--bg-input": "#0c0f0e",
    "--bg-raised": "#161b19",
    "--bg-hover": "#151a18",
    "--bg-active": "#1c2320",
    // text
    "--tx": "#e9ede9",
    "--tx-2": "#98a39c",
    "--tx-3": "#6a736d",
    "--tx-faint": "#4b524d",
    // emerald accent
    "--em": "#10b981",
    "--em-bright": "#34d399",
    "--em-dim": "#0e8c63",
    "--on-em": "#04140d",
    // status
    "--sky": "#38bdf8",
    "--violet": "#a78bfa",
    "--amber": "#f5b544",
    "--rose": "#f4708a",
    "--red": "#ef5350",
    // shape
    "--r-window": "11px",
    "--r-card": "14px",
    "--r-ctrl": "9px",
    "--r-pill": "999px",
    // density (comfortable defaults)
    "--gut": "28px",
    "--row-h": "38px",
  };

  for (const [name, value] of Object.entries(tokens)) {
    it(`declares ${name}: ${value}`, () => {
      // tolerate arbitrary whitespace after the colon
      const re = new RegExp(`${name.replace(/[-]/g, "\\-")}\\s*:\\s*${value.replace(/[().#]/g, (m) => "\\" + m)}`);
      expect(css).toMatch(re);
    });
  }

  it("declares the soft emerald fills + glow used by chips/rings", () => {
    expect(css).toMatch(/--em-glow:\s*rgba\(16,\s*185,\s*129,\s*0?\.16\)/);
    expect(css).toMatch(/--em-soft:\s*rgba\(16,\s*185,\s*129,\s*0?\.10?\)/);
  });

  it("declares the soft status fills", () => {
    expect(css).toMatch(/--amber-soft:\s*rgba\(245,\s*181,\s*68/);
    expect(css).toMatch(/--red-soft:\s*rgba\(239,\s*83,\s*80/);
    expect(css).toMatch(/--sky-soft:\s*rgba\(56,\s*189,\s*248/);
  });

  it("declares the line/elevation tokens and the emerald focus ring", () => {
    expect(css).toMatch(/--line:\s*rgba\(255,\s*255,\s*255,\s*0?\.07\)/);
    expect(css).toMatch(/--line-2:\s*rgba\(255,\s*255,\s*255,\s*0?\.04\)/);
    expect(css).toMatch(/--line-strong:\s*rgba\(255,\s*255,\s*255,\s*0?\.12\)/);
    expect(css).toMatch(/--focus:\s*rgba\(16,\s*185,\s*129,\s*0?\.55\)/);
  });

  it("declares the card/pop/sheet shadows", () => {
    expect(css).toMatch(/--shadow-card:/);
    expect(css).toMatch(/--shadow-pop:/);
    expect(css).toMatch(/--shadow-sheet:/);
  });

  it("declares the dense density override", () => {
    expect(css).toMatch(/\[data-density=["']?dense["']?\]/);
    expect(css).toMatch(/--gut:\s*20px/);
    expect(css).toMatch(/--row-h:\s*34px/);
  });

  it("reconciles shadcn semantic tokens onto the package palette", () => {
    // the emerald accent must drive the primary/ring semantic tokens so shadcn
    // utilities (bg-primary, ring) resolve to the package values
    expect(css).toMatch(/--primary:\s*var\(--em-bright\)/);
    expect(css).toMatch(/--ring:\s*var\(--(focus|em)[^)]*\)/);
    expect(css).toMatch(/--card:\s*var\(--bg-card\)/);
    expect(css).toMatch(/--border:\s*var\(--line\)/);
    expect(css).toMatch(/--foreground:\s*var\(--tx\)/);
  });
});

describe("keyframes (the 8 package animations)", () => {
  for (const name of ["pulseDot", "spin", "shimmer", "fadeUp", "sheetIn", "overlayIn", "blink", "toastIn"]) {
    it(`defines @keyframes ${name}`, () => {
      expect(css).toMatch(new RegExp(`@keyframes\\s+${name}\\b`));
    });
  }
});

describe("global treatments", () => {
  it("defines the custom 11px webkit scrollbars", () => {
    expect(css).toMatch(/::-webkit-scrollbar\s*\{[^}]*width:\s*11px/);
    expect(css).toMatch(/::-webkit-scrollbar-thumb/);
  });

  it("defines the emerald ::selection highlight", () => {
    expect(css).toMatch(/::selection\s*\{[^}]*rgba\(16,\s*185,\s*129/);
  });

  it("defines the .mono helper with the ss01 feature", () => {
    expect(css).toMatch(/\.mono\s*\{/);
    expect(css).toMatch(/font-feature-settings:\s*["']ss01["']/);
  });

  it("paints the emerald focus ring on native controls (not just outline:none)", () => {
    // The global input/textarea/select focus rule must apply the package's 0 0 0 3px --focus
    // ring — stripping the outline with no replacement would leave bare native controls with
    // no visible focus indicator.
    expect(css).toMatch(
      /input:focus-visible[\s\S]{0,160}box-shadow:\s*0 0 0 3px var\(--focus\)/,
    );
  });
});

describe("self-hosted Geist fonts (offline desktop app — no CDN)", () => {
  it("does not load fonts from the Google Fonts CDN", () => {
    expect(css).not.toMatch(/fonts\.googleapis\.com/);
    expect(css).not.toMatch(/fonts\.gstatic\.com/);
  });

  it("declares @font-face for Geist and Geist Mono", () => {
    const faces = css.match(/@font-face\s*\{[^}]*\}/g) ?? [];
    const families = faces.join("\n");
    expect(families).toMatch(/font-family:\s*["']Geist["']/);
    expect(families).toMatch(/font-family:\s*["']Geist Mono["']/);
    expect(families).toMatch(/\.woff2/);
  });

  it("maps the Tailwind sans/mono theme fonts onto Geist", () => {
    expect(css).toMatch(/--font-sans:\s*["']?Geist/);
    expect(css).toMatch(/--font-mono:\s*["']?Geist Mono/);
  });

  it("ships the woff2 font files under public/fonts", () => {
    expect(existsSync(path.join(publicDir, "fonts", "Geist-Variable.woff2"))).toBe(true);
    expect(existsSync(path.join(publicDir, "fonts", "GeistMono-Variable.woff2"))).toBe(true);
  });
});
