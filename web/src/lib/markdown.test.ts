import { describe, expect, it } from "vitest";
import { parseMarkdown, safeHref, type MdBlock, type MdInline } from "@/lib/markdown";

// STUDIO-739 — the agent-prose markdown subset. Agents write handoffs and self-reviews in
// markdown, so the transcript, the room and memory all carry it; these pin the subset the
// renderer promises (headings, emphasis, code, lists, links) and the two things that make it
// safe to render agent-authored text: no markup escapes as HTML, and no non-http scheme
// becomes a link.

/** The plain text of an inline run — the parser ships no such helper, so the tests own one. */
function inlineText(nodes: readonly MdInline[]): string {
  return nodes
    .map((node) => (node.type === "text" || node.type === "code" ? node.text : inlineText(node.children)))
    .join("");
}

/** The first block, for the many single-block cases below. */
function one(src: string): MdBlock {
  const blocks = parseMarkdown(src);
  expect(blocks).toHaveLength(1);
  return blocks[0];
}

describe("parseMarkdown — blocks", () => {
  it("reads an ATX heading and its level", () => {
    const block = one("## Verification");
    expect(block).toMatchObject({ type: "heading", level: 2 });
    expect(inlineText(block.type === "heading" ? block.children : [])).toBe("Verification");
  });

  it("caps a heading at six hashes and leaves a hash without a space as prose", () => {
    expect(one("####### seven").type).toBe("paragraph");
    expect(one("#nospace").type).toBe("paragraph");
  });

  it("keeps a paragraph's own line breaks and drops each line's indent", () => {
    const block = one("first line\n   second line");
    expect(block).toMatchObject({ type: "paragraph" });
    expect(inlineText(block.type === "paragraph" ? block.children : [])).toBe(
      "first line\nsecond line",
    );
  });

  it("splits paragraphs on a blank line", () => {
    expect(parseMarkdown("one\n\ntwo").map((b) => b.type)).toEqual(["paragraph", "paragraph"]);
  });

  it("reads a fenced code block verbatim, with its language", () => {
    expect(one("```rust\nlet x = **not bold**;\n```")).toEqual({
      type: "code",
      lang: "rust",
      text: "let x = **not bold**;",
    });
  });

  it("closes an unterminated fence at the end of the text", () => {
    expect(one("```\ncargo test\n")).toEqual({ type: "code", lang: "", text: "cargo test" });
  });

  it("reads a tilde fence and ignores a shorter fence inside it", () => {
    expect(one("~~~\n~~\nstill code\n~~~")).toEqual({ type: "code", lang: "", text: "~~\nstill code" });
  });

  it("groups consecutive bullets into one list", () => {
    const block = one("- one\n* two\n+ three");
    expect(block.type).toBe("list");
    if (block.type !== "list") return;
    expect(block.ordered).toBe(false);
    expect(block.items.map((i) => inlineText(i.children))).toEqual(["one", "two", "three"]);
  });

  it("reads an ordered list", () => {
    const block = one("1. first\n2) second");
    expect(block).toMatchObject({ type: "list", ordered: true });
  });

  it("nests a deeper-indented list under its parent item", () => {
    const block = one("- outer\n  - inner\n- second");
    if (block.type !== "list") throw new Error("expected a list");
    expect(block.items).toHaveLength(2);
    expect(inlineText(block.items[0].children)).toBe("outer");
    expect(block.items[0].list?.items.map((i) => inlineText(i.children))).toEqual(["inner"]);
    expect(block.items[1].list).toBeNull();
  });

  it("keeps every item when the indentation step is uneven", () => {
    // 4-then-2 is ordinary in generated markdown. A dedent that lands between two levels used to
    // rebuild a fresh sublist on a parent that already owned one, so `x` was not mis-nested — it
    // was deleted, and a hand-off's sub-bullet vanished from the only place an operator reads it.
    const block = one("- a\n    - x\n  - y");
    if (block.type !== "list") throw new Error("expected a list");
    expect(block.items.map((i) => inlineText(i.children))).toEqual(["a"]);
    expect(block.items[0].list?.items.map((i) => inlineText(i.children))).toEqual(["x", "y"]);
  });

  it("keeps a loose list (blank lines between items) as one list", () => {
    const block = one("- one\n\n- two");
    expect(block).toMatchObject({ type: "list" });
    if (block.type !== "list") return;
    expect(block.items).toHaveLength(2);
  });

  it("continues an item onto its lazy continuation line", () => {
    const block = one("- one that\n  wraps");
    if (block.type !== "list") throw new Error("expected a list");
    expect(inlineText(block.items[0].children)).toBe("one that\nwraps");
  });

  it("ends a list at a fence and starts the code block", () => {
    expect(parseMarkdown("- one\n```\ncode\n```").map((b) => b.type)).toEqual(["list", "code"]);
  });

  it("has nothing to say about empty text", () => {
    expect(parseMarkdown("")).toEqual([]);
    expect(parseMarkdown("\n\n  \n")).toEqual([]);
  });
});

describe("parseMarkdown — inline", () => {
  function inline(src: string) {
    const block = one(src);
    if (block.type !== "paragraph") throw new Error(`expected a paragraph, got ${block.type}`);
    return block.children;
  }

  it("reads bold, italic and bold-italic", () => {
    expect(inline("**b**")).toEqual([{ type: "strong", children: [{ type: "text", text: "b" }] }]);
    expect(inline("*i*")).toEqual([{ type: "em", children: [{ type: "text", text: "i" }] }]);
    expect(inline("__b__")).toMatchObject([{ type: "strong" }]);
    expect(inline("***both***")).toEqual([
      { type: "strong", children: [{ type: "em", children: [{ type: "text", text: "both" }] }] },
    ]);
  });

  it("leaves an underscore inside a word alone — `run_id` is not emphasis", () => {
    expect(inline("call symphony_run_status now")).toEqual([
      { type: "text", text: "call symphony_run_status now" },
    ]);
  });

  it("does not open emphasis on whitespace", () => {
    expect(inline("2 * 3 * 4")).toEqual([{ type: "text", text: "2 * 3 * 4" }]);
  });

  it("reads an inline code span and leaves its content literal", () => {
    expect(inline("run `make lint` first")).toEqual([
      { type: "text", text: "run " },
      { type: "code", text: "make lint" },
      { type: "text", text: " first" },
    ]);
    expect(inline("``a ` b``")).toEqual([{ type: "code", text: "a ` b" }]);
  });

  it("reads a link and keeps its label formatted", () => {
    expect(inline("see [the **PR**](https://example.com/pr/1)")).toEqual([
      { type: "text", text: "see " },
      {
        type: "link",
        href: "https://example.com/pr/1",
        children: [
          { type: "text", text: "the " },
          { type: "strong", children: [{ type: "text", text: "PR" }] },
        ],
      },
    ]);
  });

  it("leaves a link with an unsafe scheme as literal text", () => {
    expect(inline("[click](javascript:alert(1))")).toEqual([
      { type: "text", text: "[click](javascript:alert(1))" },
    ]);
  });

  it("keeps raw angle brackets as text — never as markup", () => {
    expect(inline("<img src=x onerror=alert(1)>")).toEqual([
      { type: "text", text: "<img src=x onerror=alert(1)>" },
    ]);
  });

  it("honours a backslash escape", () => {
    expect(inline("\\*not italic\\*")).toEqual([{ type: "text", text: "*not italic*" }]);
  });

  it("leaves an unclosed delimiter as text", () => {
    expect(inline("**unclosed")).toEqual([{ type: "text", text: "**unclosed" }]);
    expect(inline("a ` b")).toEqual([{ type: "text", text: "a ` b" }]);
  });
});

describe("safeHref", () => {
  it("passes http, https and mailto", () => {
    expect(safeHref("https://example.com")).toBe("https://example.com");
    expect(safeHref("http://example.com")).toBe("http://example.com");
    expect(safeHref("mailto:a@b.c")).toBe("mailto:a@b.c");
    expect(safeHref("  https://example.com  ")).toBe("https://example.com");
  });

  it("rejects every other scheme, however it is spelled", () => {
    for (const href of [
      "javascript:alert(1)",
      "JaVaScRiPt:alert(1)",
      " javascript:alert(1)",
      "java\nscript:alert(1)",
      "data:text/html;base64,PHNjcmlwdD4=",
      "vbscript:msgbox",
      "/relative",
      "",
    ]) {
      expect(safeHref(href), href).toBeNull();
    }
  });
});

describe("parseMarkdown — long input", () => {
  // A hand-off summary is big, and the parse happens on the thread that draws the transcript.
  // The shape below is the one that used to be quadratic: every `*` opens emphasis whose every
  // candidate closer is preceded by a space, so each opener re-scanned the whole tail (59KB
  // took 2.8s before the closer memo).
  it("does not degrade on prose full of unclosed emphasis", () => {
    const src = "a *b ".repeat(20_000);
    const started = Date.now();
    parseMarkdown(src);
    expect(Date.now() - started).toBeLessThan(1000);
  });

  it("stays linear on a big transcript body", () => {
    const src = Array.from({ length: 2000 }, (_, i) => `- item ${i} with **bold** and \`code\``).join(
      "\n",
    );
    const started = Date.now();
    const blocks = parseMarkdown(src);
    expect(blocks).toHaveLength(1);
    expect(Date.now() - started).toBeLessThan(1000);
  });
});
