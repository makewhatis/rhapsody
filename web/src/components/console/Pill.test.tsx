// @vitest-environment jsdom
// STUDIO-681 §10 box 1.4 — "Pill renders the correct color per variant
// (run/review/queued/done/blocked)."
//
// A DOM assertion alone would only prove the class name is spelled right, so each variant
// is checked twice: the rendered element carries the variant class, AND `theme/console.css`
// paints that class the color the spec assigns it. Together they close the loop from prop
// to pixel without a browser.
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { PILL_COLORS, Pill, type PillVariant } from "./Pill";

afterEach(cleanup);

const consoleCss = readFileSync(path.resolve(__dirname, "../../theme/console.css"), "utf8");

/** The spec's §1.3 variant → token mapping: green / amber / grey / blue / red. */
const EXPECTED: Record<PillVariant, string> = {
  run: "var(--ok)",
  review: "var(--accent)",
  queued: "var(--ink-3)",
  done: "var(--info)",
  blocked: "var(--bad)",
};

describe("variant colors", () => {
  for (const [variant, color] of Object.entries(EXPECTED) as [PillVariant, string][]) {
    it(`renders ${variant} in ${color}`, () => {
      const { container } = render(<Pill variant={variant}>{variant}</Pill>);
      const pill = container.querySelector(".pill");
      expect(pill?.classList.contains(variant)).toBe(true);

      const rule = new RegExp(`\\.pill\\.${variant}\\s*\\{[^}]*color:\\s*${color.replace(/[()-]/g, "\\$&")}`);
      expect(consoleCss).toMatch(rule);
      expect(PILL_COLORS[variant]).toBe(color);
    });

    it(`tints ${variant}'s dot the same color as its text`, () => {
      const rule = new RegExp(`\\.pill\\.${variant} \\.d\\s*\\{[^}]*background:\\s*${color.replace(/[()-]/g, "\\$&")}`);
      // `queued` is the one exception the prototype makes: a fainter dot than its label.
      if (variant === "queued") {
        expect(consoleCss).toMatch(/\.pill\.queued \.d\s*\{[^}]*background:\s*var\(--ink-4\)/);
      } else {
        expect(consoleCss).toMatch(rule);
      }
    });
  }

  it("gives every variant a distinct color — no two statuses read alike", () => {
    expect(new Set(Object.values(PILL_COLORS)).size).toBe(Object.keys(PILL_COLORS).length);
  });
});

describe("rendering", () => {
  it("renders the label and a decorative dot", () => {
    const { container } = render(<Pill variant="run">running</Pill>);
    expect(container.textContent).toBe("running");
    expect(container.querySelector(".pill > .d")?.getAttribute("aria-hidden")).toBe("true");
  });

  it("keeps caller classes alongside the variant class", () => {
    const { container } = render(<Pill variant="done" className="extra" />);
    const pill = container.querySelector(".pill");
    expect(pill?.classList.contains("done")).toBe(true);
    expect(pill?.classList.contains("extra")).toBe(true);
  });
});
