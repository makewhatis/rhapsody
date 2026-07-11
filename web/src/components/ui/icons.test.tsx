// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Settings, Linear, Dot, Git, Icons } from "@/components/ui/icons";

afterEach(cleanup);

describe("icon set", () => {
  it("renders a lucide-aliased icon with the package's 1.6 stroke + 16px default", () => {
    const { container } = render(<Settings />);
    const svg = container.querySelector("svg") as SVGElement;
    expect(svg).toBeTruthy();
    expect(svg.getAttribute("width")).toBe("16");
    expect(svg.getAttribute("stroke-width")).toBe("1.6");
  });

  it("lets callers override size and stroke", () => {
    const { container } = render(<Settings size={20} strokeWidth={2} />);
    const svg = container.querySelector("svg") as SVGElement;
    expect(svg.getAttribute("width")).toBe("20");
    expect(svg.getAttribute("stroke-width")).toBe("2");
  });

  it("renders the custom Linear and Dot glyphs", () => {
    const { container: a } = render(<Linear />);
    expect(a.querySelector("svg path")).toBeTruthy();
    const { container: b } = render(<Dot />);
    expect(b.querySelector("svg circle")).toBeTruthy();
  });

  it("exposes the full package icon set via the Icons namespace", () => {
    // the package's icons.jsx defines ~34 icons
    expect(Object.keys(Icons).length).toBeGreaterThanOrEqual(34);
    expect(Icons.Linear).toBeTruthy();
    expect(Icons.Refresh).toBeTruthy();
  });

  it("renders the design's custom 3-node Git fork glyph (not lucide GitBranch)", async () => {
    const { container } = render(<Git />);
    const svg = container.querySelector("svg") as SVGElement;
    expect(svg.getAttribute("width")).toBe("16");
    expect(svg.getAttribute("stroke-width")).toBe("1.6");
    const ds = Array.from(svg.querySelectorAll("path")).map((p) => p.getAttribute("d"));
    // Ported verbatim from the design package's icons.jsx: three circular nodes + a merge arc.
    expect(ds).toContain("M18 6a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM6 6a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM6 18a3 3 0 1 0 0 .01");
    expect(ds).toContain("M18 9a9 9 0 0 1-9 9M6 12v3");
    // ...and is therefore distinct from lucide's two-node GitBranch.
    const { GitBranch } = await import("lucide-react");
    const lucide = render(<GitBranch />).container.querySelector("svg")!.innerHTML;
    expect(svg.innerHTML).not.toBe(lucide);
  });
});
