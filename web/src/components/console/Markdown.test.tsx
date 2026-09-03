// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { readFileSync } from "node:fs";
import path from "node:path";
import { Markdown } from "./Markdown";

// STUDIO-739 — the renderer half. The parser's tests pin what the tree says; these pin what
// reaches the DOM: real elements for the subset, plain text for everything an agent could use
// to inject markup, and a code block that scrolls inside its own box.

afterEach(cleanup);

const css = readFileSync(path.resolve(__dirname, "../../theme/markdown.css"), "utf8");

describe("Markdown", () => {
  it("renders bold, italic and inline code as elements, not as syntax", () => {
    const { container } = render(<Markdown source="a **bold** and *italic* and `code` line" />);
    expect(container.querySelector("strong")?.textContent).toBe("bold");
    expect(container.querySelector("em")?.textContent).toBe("italic");
    expect(container.querySelector("code")?.textContent).toBe("code");
    expect(container.textContent).not.toContain("**");
  });

  it("renders a heading below the card's own h2, keeping the author's level", () => {
    const { container } = render(<Markdown source="# Summary" />);
    const heading = container.querySelector("h3");
    expect(heading?.textContent).toBe("Summary");
    expect(heading?.className).toContain("mdh1");
    // A deep heading never escapes the h1–h6 range.
    expect(render(<Markdown source="###### deep" />).container.querySelector("h6")).toBeTruthy();
  });

  it("renders bullets and their nested bullets as real lists", () => {
    const { container } = render(<Markdown source={"- outer\n  - inner\n- second"} />);
    const top = container.querySelector("ul");
    expect(top?.children).toHaveLength(2);
    expect(top?.querySelector("li ul li")?.textContent).toBe("inner");
    expect(container.querySelector("ol")).toBeNull();
  });

  it("renders an ordered list as an ol", () => {
    const { container } = render(<Markdown source={"1. first\n2. second"} />);
    expect(container.querySelectorAll("ol > li")).toHaveLength(2);
  });

  it("puts a fenced code block in its own scrolling box, in the mono face", () => {
    const { container } = render(<Markdown source={"```sh\nmake lint\n```"} />);
    const pre = container.querySelector("pre.mdpre");
    expect(pre?.textContent).toBe("make lint");
    expect(pre?.querySelector("code")?.className).toContain("mdcode");
    // A scrollable box that cannot take focus cannot be scrolled from the keyboard.
    expect(pre?.getAttribute("tabindex")).toBe("0");
    // The box scrolls, not the page — STUDIO-681's discipline for wide content.
    expect(css).toMatch(/\.md pre\.mdpre\s*\{[^}]*overflow-x:\s*auto/);
    expect(css).toMatch(/\.md pre\.mdpre\s*\{[^}]*font-family:\s*var\(--mono\)/);
    expect(css).toMatch(/\.md pre\.mdpre\s*\{[^}]*max-width:\s*100%/);
  });

  it("keeps a code block's markdown literal", () => {
    const { container } = render(<Markdown source={"```\n## not a heading **not bold**\n```"} />);
    expect(container.querySelector("h3")).toBeNull();
    expect(container.querySelector("strong")).toBeNull();
    expect(container.textContent).toBe("## not a heading **not bold**");
  });

  it("links only to a safe scheme, and opens it away from the console", () => {
    const { container } = render(<Markdown source="see [the PR](https://example.com/pr/1)" />);
    const anchor = container.querySelector("a");
    expect(anchor?.getAttribute("href")).toBe("https://example.com/pr/1");
    expect(anchor?.getAttribute("rel")).toContain("noreferrer");
    expect(anchor?.getAttribute("target")).toBe("_blank");
  });

  // --- the injection acceptance (STUDIO-739): agent text is DATA -----------------------------

  it("does not execute or emit a script an agent wrote", () => {
    const { container } = render(
      <Markdown source={"before\n\n<script>window.__pwned = 1;</script>\n\nafter"} />,
    );
    expect(container.querySelector("script")).toBeNull();
    expect((window as unknown as Record<string, unknown>).__pwned).toBeUndefined();
    expect(container.textContent).toContain("<script>window.__pwned = 1;</script>");
  });

  it("does not emit an img an agent wrote, so its onerror cannot fire", () => {
    const { container } = render(<Markdown source={'<img src=x onerror="window.__pwned = 1">'} />);
    expect(container.querySelector("img")).toBeNull();
    expect((window as unknown as Record<string, unknown>).__pwned).toBeUndefined();
  });

  it("refuses a javascript: link and shows the text the agent wrote instead", () => {
    const { container } = render(<Markdown source="[click](javascript:alert(1))" />);
    expect(container.querySelector("a")).toBeNull();
    expect(container.textContent).toBe("[click](javascript:alert(1))");
  });

  it("renders a lead node inline with the first paragraph", () => {
    const { container } = render(
      <Markdown source="ran the **suite**" lead={<code className="ttool">Bash</code>} />,
    );
    const first = container.querySelector("p");
    expect(first?.querySelector("code.ttool")?.textContent).toBe("Bash");
    expect(first?.textContent).toBe("Bash ran the suite");
  });

  it("keeps a lead node when the body opens with a block that is not a paragraph", () => {
    render(<Markdown source="- one" lead={<code className="ttool">Bash</code>} />);
    expect(screen.getByText("Bash")).toBeTruthy();
    expect(screen.getByText("one")).toBeTruthy();
  });

  it("renders nothing for an empty body", () => {
    const { container } = render(<Markdown source="" />);
    expect(container.querySelector(".md")?.textContent).toBe("");
  });
});
