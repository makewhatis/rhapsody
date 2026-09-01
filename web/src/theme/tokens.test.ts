// Contract tests for the Rhapsody Console design tokens (STUDIO-682).
//
// These cover STUDIO-681 §10 sub-ticket 1's acceptance boxes 1.1, 1.2 and 1.7:
//   1.1  tokens.css declares every color/radius token from spec §1.1, and no view
//        file hardcodes a hex that duplicates one of them.
//   1.2  both font families load with a real fallback stack.
//   1.7  the nav rail collapses to icon-only below 860px.
// They assert against the CSS/HTML SOURCE rather than a rendered browser, the same
// browser-free approach `src/index.css.test.ts` already uses for the Podium layer.
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";

const themeDir = __dirname;
const srcDir = path.resolve(themeDir, "..");
const webDir = path.resolve(srcDir, "..");

const tokensCss = readFileSync(path.join(themeDir, "tokens.css"), "utf8");
const consoleCss = readFileSync(path.join(themeDir, "console.css"), "utf8");
const indexHtml = readFileSync(path.join(webDir, "index.html"), "utf8");

/** Every color + radius token of spec §1.1, with the value the prototype's `:root` sets. */
const TOKENS: Record<string, string> = {
  "--bg": "#0c0c0e",
  "--panel": "#131315",
  "--panel-2": "#17171a",
  "--raise": "#1c1c20",
  "--line": "#26262c",
  "--line-soft": "#1e1e23",
  "--ink": "#ededf0",
  "--ink-2": "#9a9aa4",
  "--ink-3": "#6b6b74",
  "--ink-4": "#48484f",
  "--accent": "#e79457",
  "--accent-soft": "#2a1d13",
  "--operator": "#57c3cc",
  "--operator-soft": "#0f2427",
  "--handoff": "#e79457",
  "--ok": "#63c07a",
  "--warn": "#e0b64c",
  "--bad": "#e0685f",
  "--bad-soft": "#2a1414",
  "--alice": "#e79457",
  "--jimmy": "#7fa7e6",
  "--info": "#7fa7e6",
  "--ticket": "#93a0c8",
  "--ticket-bg": "#1a2033",
  "--ticket-line": "#28324e",
  "--r": "12px",
  "--r-sm": "8px",
};

/** Parse `--name: value;` declarations out of a stylesheet into a map. */
function declarations(css: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const [, name, value] of css.matchAll(/(--[a-z0-9-]+)\s*:\s*([^;{}]+);/gi)) {
    out.set(name, value.trim());
  }
  return out;
}

/** Resolve a token to a literal, following `var(--x)` indirection. */
function resolve(map: Map<string, string>, name: string, seen = new Set<string>()): string {
  const raw = map.get(name);
  if (raw === undefined || seen.has(name)) return "";
  seen.add(name);
  const indirect = /^var\((--[a-z0-9-]+)\)$/i.exec(raw);
  return indirect ? resolve(map, indirect[1], seen) : raw;
}

const tokenMap = declarations(tokensCss);

describe("1.1 — tokens.css declares every §1.1 token", () => {
  for (const [name, value] of Object.entries(TOKENS)) {
    it(`declares ${name}: ${value}`, () => {
      expect(resolve(tokenMap, name)).toBe(value);
    });
  }

  it("declares --warn-soft, which the prototype's `.note.warn` needs", () => {
    expect(resolve(tokenMap, "--warn-soft")).toBe("#2a2312");
  });

  it("scopes the palette so it cannot repaint the Podium screens mid-migration", () => {
    // `--ink`, `--line` and `--accent` mean different things in src/index.css; publishing
    // the console values at `:root` would silently restyle the shipped dashboard.
    expect(tokensCss).toMatch(/\.rh-console\s*\{/);
    expect(tokensCss).not.toMatch(/(^|\})\s*:root\s*\{/);
  });

  it("paints the dark theme explicitly on the scope root rather than inheriting", () => {
    expect(tokensCss).toMatch(/background:\s*var\(--bg\)/);
    expect(tokensCss).toMatch(/color:\s*var\(--ink\)/);
  });
});

/** Every non-test source file that could hardcode a color, minus the two token sources. */
function viewFiles(): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
      } else if (/\.(tsx?|css)$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name)) {
        out.push(full);
      }
    }
  };
  walk(srcDir);
  const sources = new Set([path.join(srcDir, "index.css"), path.join(themeDir, "tokens.css")]);
  return out.filter((f) => !sources.has(f));
}

describe("1.1 — no view file hardcodes a hex that duplicates a token", () => {
  const hexes = new Map<string, string>();
  for (const [name, value] of Object.entries({ ...TOKENS, "--warn-soft": "#2a2312" })) {
    if (value.startsWith("#")) hexes.set(value.toLowerCase(), name);
  }

  it("finds view files to scan (guards the walker against silently matching nothing)", () => {
    expect(viewFiles().length).toBeGreaterThan(50);
  });

  it("finds no duplicated token hex outside tokens.css", () => {
    const offenders: string[] = [];
    for (const file of viewFiles()) {
      const body = readFileSync(file, "utf8");
      for (const [, hex] of body.matchAll(/(#[0-9a-f]{6})\b/gi)) {
        const token = hexes.get(hex.toLowerCase());
        if (token) offenders.push(`${path.relative(webDir, file)} hardcodes ${hex} (use var(${token}))`);
      }
    }
    expect(offenders).toEqual([]);
  });
});

describe("1.2 — both fonts load with a real fallback stack", () => {
  it("loads IBM Plex Sans and IBM Plex Mono at the weights the spec lists", () => {
    expect(indexHtml).toMatch(/fonts\.googleapis\.com\/css2\?[^"]*IBM\+Plex\+Mono:wght@400;500;600/);
    expect(indexHtml).toMatch(/IBM\+Plex\+Sans:wght@400;450;500;600;700/);
  });

  it("preconnects to the font hosts so the stylesheet is not a cold round-trip", () => {
    expect(indexHtml).toMatch(/rel="preconnect"\s+href="https:\/\/fonts\.googleapis\.com"/);
    expect(indexHtml).toMatch(/rel="preconnect"\s+href="https:\/\/fonts\.gstatic\.com"\s+crossorigin/);
  });

  it("swaps rather than blocking, so an offline daemon still paints text", () => {
    expect(indexHtml).toMatch(/display=swap/);
  });

  it("falls back to a real system stack when the webfont never arrives", () => {
    expect(resolve(tokenMap, "--sans")).toMatch(/"IBM Plex Sans",\s*system-ui,\s*-apple-system,\s*sans-serif/);
    expect(resolve(tokenMap, "--mono")).toMatch(/"IBM Plex Mono",\s*ui-monospace,\s*"SF Mono",\s*monospace/);
  });

  it("puts mono on ids, ticket keys, timestamps, SHAs and code", () => {
    for (const selector of ["\\.mono", "\\.tk", "\\.at", "code"]) {
      expect(consoleCss).toMatch(new RegExp(`${selector}[^{]*\\{[^}]*font-family:\\s*var\\(--mono\\)`));
    }
  });

  it("gives columnar numbers tabular figures", () => {
    expect(consoleCss).toMatch(/font-variant-numeric:\s*tabular-nums/);
  });
});

describe("1.7 — the rail collapses to icon-only below 860px", () => {
  const block = /@media\s*\(max-width:\s*860px\)\s*\{([\s\S]*?)\n\s*\}\n/.exec(consoleCss);

  it("declares the 860px breakpoint", () => {
    expect(block).not.toBeNull();
  });

  it("narrows the shell's rail column from 214px to the 52px icon rail", () => {
    expect(consoleCss).toMatch(/grid-template-columns:\s*214px 1fr/);
    expect(block?.[1]).toMatch(/grid-template-columns:\s*52px 1fr/);
  });

  it("hides the wordmark, nav labels, counts and the foot, leaving only icons", () => {
    const collapsed = block?.[1] ?? "";
    expect(collapsed).toMatch(/\.rail \.logo b/);
    expect(collapsed).toMatch(/\.nav a > span:not\(\.ic\)/);
    expect(collapsed).toMatch(/\.nav a \.ct/);
    expect(collapsed).toMatch(/\.rail \.foot/);
    expect(collapsed).toMatch(/display:\s*none/);
  });
});
