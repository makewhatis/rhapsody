// A small markdown parser for AGENT-AUTHORED prose (STUDIO-739).
//
// Agents write their notifications, hand-offs and self-reviews in markdown, and the console
// showed them verbatim — a wall of literal `**bold**`, `## headings` and ``` fences. This turns
// that text into a typed tree; `components/console/Markdown.tsx` renders the tree as React
// elements.
//
// SAFETY IS THE POINT. The text is untrusted (it is whatever an agent, or whoever the agent read
// from, wrote), so it is parsed into DATA and never into markup: there is no HTML output here at
// all, so no `dangerouslySetInnerHTML` on the other side and nothing to sanitize after the fact.
// A `<script>` in a body is text because the parser has no rule that turns it into anything else,
// and a link only becomes an anchor when [`safeHref`] recognizes its scheme.
//
// The subset is deliberately the one agents actually emit — ATX headings, bold/italic, inline
// code, fenced code blocks, bullet/ordered lists (nested), and links. Everything outside it
// (tables, block quotes, thematic breaks, images, reference links, raw HTML) is left as the
// literal text the agent typed, which is exactly what the console did before this file existed.

export type MdInline =
  | { type: "text"; text: string }
  | { type: "code"; text: string }
  | { type: "strong"; children: MdInline[] }
  | { type: "em"; children: MdInline[] }
  | { type: "link"; href: string; children: MdInline[] };

export interface MdListItem {
  children: MdInline[];
  /** The list nested under this item, or `null` — an item's own sub-bullets. */
  list: MdList | null;
}

export interface MdList {
  ordered: boolean;
  items: MdListItem[];
}

export type MdBlock =
  | { type: "heading"; level: number; children: MdInline[] }
  | { type: "paragraph"; children: MdInline[] }
  | { type: "code"; lang: string; text: string }
  | ({ type: "list" } & MdList);

/** Only these three schemes ever become an `href` — see [`safeHref`]. */
const SAFE_SCHEME = /^(?:https?|mailto):/i;

/**
 * The href a link may carry, or `null` when it must not become one.
 *
 * An allow-list, never a block-list: `javascript:`, `data:` and every other scheme — including
 * the ones spelled to slip past a naive check, with embedded newlines or leading blanks — fail
 * by not being one of the three. A rejected link renders as the literal text the agent wrote.
 */
export function safeHref(href: string): string | null {
  const trimmed = href.trim();
  // A URL carries no raw control characters; a scheme "spelled" with one ("java\nscript:") is
  // not the scheme it looks like, and browsers strip them before resolving. Reject, don't unpick.
  if (/[\u0000-\u0020\u007f]/.test(trimmed)) return null;
  return SAFE_SCHEME.test(trimmed) ? trimmed : null;
}

// --- blocks -------------------------------------------------------------------------------

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^\s`]*)/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*)$/;
const BULLET = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;

/** One list line while the run is being gathered, before it becomes a tree. */
interface RawItem {
  indent: number;
  ordered: boolean;
  text: string;
}

/** Parses a whole body into its blocks. One linear pass — long transcripts are the normal case. */
export function parseMarkdown(source: string): MdBlock[] {
  // The empty tail `split` leaves on a body that ends in a newline is punctuation, not a line —
  // inside an unterminated fence it would otherwise become a blank line of code.
  const lines = source.replace(/\r\n?/g, "\n").replace(/\n$/, "").split("\n");
  const blocks: MdBlock[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      i += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const marker = fence[1];
      const body: string[] = [];
      i += 1;
      // An unterminated fence closes at the end of the text, as CommonMark has it — a truncated
      // transcript still renders its code as code.
      while (i < lines.length && !isClosingFence(lines[i], marker)) {
        body.push(lines[i]);
        i += 1;
      }
      if (i < lines.length) i += 1;
      blocks.push({ type: "code", lang: fence[2], text: body.join("\n") });
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      const text = heading[2].replace(/\s+#+\s*$/, "");
      blocks.push({ type: "heading", level: heading[1].length, children: parseInline(text) });
      i += 1;
      continue;
    }

    if (BULLET.test(line)) {
      const raw: RawItem[] = [];
      while (i < lines.length) {
        const item = BULLET.exec(lines[i]);
        if (item) {
          raw.push({ indent: item[1].length, ordered: /\d/.test(item[2]), text: item[3] });
          i += 1;
          continue;
        }
        if (lines[i].trim() === "") {
          // A loose list keeps its blank lines: the run continues only if another item follows.
          let peek = i;
          while (peek < lines.length && lines[peek].trim() === "") peek += 1;
          if (peek < lines.length && BULLET.test(lines[peek])) {
            i = peek;
            continue;
          }
          break;
        }
        if (FENCE.test(lines[i]) || HEADING.test(lines[i]) || raw.length === 0) break;
        // A lazy continuation belongs to the item above it.
        raw[raw.length - 1].text += `\n${lines[i].trim()}`;
        i += 1;
      }
      blocks.push({ type: "list", ...buildList(raw) });
      continue;
    }

    const paragraph: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !FENCE.test(lines[i]) &&
      !HEADING.test(lines[i]) &&
      !BULLET.test(lines[i])
    ) {
      // Trailing blanks only. A paragraph keeps its LEADING whitespace for the same reason it
      // keeps its line breaks (`.md p` is `pre-wrap`): the console shows what the agent wrote,
      // and a retained fact's `tree` listing, aligned columns or unfenced traceback were shown
      // with their indentation before this file existed (`.fact .body`, memory.css).
      paragraph.push(lines[i].replace(/\s+$/, ""));
      i += 1;
    }
    blocks.push({ type: "paragraph", children: parseInline(paragraph.join("\n")) });
  }
  return blocks;
}

function isClosingFence(line: string, marker: string): boolean {
  const close = new RegExp(`^ {0,3}${marker[0] === "`" ? "`" : "~"}{${marker.length},}\\s*$`);
  return close.test(line);
}

/**
 * The `[start, end)` offset of every fenced code block in `text`, in order.
 *
 * For callers that CUT a body before parsing it (the room feed truncates a long post). A cut
 * inside a fence is not a cosmetic problem: the head is left with an unterminated fence, which is
 * harmless, but the tail then STARTS with the closing fence — which opens a new unterminated one,
 * so every remaining word of the post renders as code. An unterminated block ends at the text's
 * end, matching how [`parseMarkdown`] closes one.
 */
export function fenceSpans(text: string): { start: number; end: number }[] {
  const spans: { start: number; end: number }[] = [];
  let start = -1;
  let marker = "";
  let at = 0;
  while (at <= text.length) {
    const eol = text.indexOf("\n", at);
    const line = text.slice(at, eol === -1 ? text.length : eol);
    if (start === -1) {
      const fence = FENCE.exec(line);
      if (fence) {
        start = at;
        marker = fence[1];
      }
    } else if (isClosingFence(line, marker)) {
      spans.push({ start, end: eol === -1 ? text.length : eol + 1 });
      start = -1;
    }
    if (eol === -1) break;
    at = eol + 1;
  }
  if (start !== -1) spans.push({ start, end: text.length });
  return spans;
}

/** Folds the gathered list lines into a tree, one level per step of indentation. */
function buildList(raw: readonly RawItem[]): MdList {
  const root: MdList = { ordered: raw.length > 0 && raw[0].ordered, items: [] };
  const stack: { indent: number; list: MdList }[] = [{ indent: raw[0]?.indent ?? 0, list: root }];
  for (const item of raw) {
    while (stack.length > 1 && item.indent < stack[stack.length - 1].indent) stack.pop();
    let top = stack[stack.length - 1];
    if (item.indent > top.indent) {
      const parent = top.list.items[top.list.items.length - 1];
      // A deeper line with no item above it (a malformed body) joins the level it landed on.
      if (parent) {
        // The parent's EXISTING sublist, when it has one. A dedent that lands between two levels
        // (`- a` / 4-space `- x` / 2-space `- y`) pops the deeper level and arrives back here, and
        // a fresh list assigned over `parent.list` would make the items already gathered under it
        // unreachable — the sub-bullet would not be mis-nested, it would be gone.
        const nested: MdList = parent.list ?? { ordered: item.ordered, items: [] };
        parent.list = nested;
        top = { indent: item.indent, list: nested };
        stack.push(top);
      }
    }
    top.list.items.push({ children: parseInline(item.text), list: null });
  }
  return root;
}

// --- inline -------------------------------------------------------------------------------

const ESCAPABLE = /[\\`*_[\]()#+\-.!>~|]/;
const WORD = /[\p{L}\p{N}]/u;
// Sticky, not anchored: `exec` starts at `lastIndex`, so a candidate is tried in place instead of
// against a fresh `source.slice(i)` copy at every `[`.
const LINK = /\[([^\]]*)\]\(([^()\s]*)\)/y;

/**
 * Parses one run of inline text.
 *
 * `scanned` is what keeps this linear on a long body: a closer is rejected only for reasons
 * that depend on the closer's own position (the character before it, and after it for `_`), so
 * a candidate one opener has already rejected is rejected for every later opener too. Without
 * the memo, prose shaped like `a *b a *b …` makes every opener re-scan the whole tail — 59KB of
 * it took 2.8 seconds, on the thread that draws the transcript.
 */
function parseInline(source: string): MdInline[] {
  const scanned = new Map<string, number>();
  // No `[` below this offset can open a link — see the bail-out at the `[` branch.
  let linkDead = 0;
  const out: MdInline[] = [];
  let text = "";
  const flush = () => {
    if (text !== "") {
      out.push({ type: "text", text });
      text = "";
    }
  };

  let i = 0;
  while (i < source.length) {
    const ch = source[i];

    if (ch === "\\" && i + 1 < source.length && ESCAPABLE.test(source[i + 1])) {
      text += source[i + 1];
      i += 2;
      continue;
    }

    if (ch === "`") {
      const span = matchCode(source, i);
      if (span) {
        flush();
        out.push({ type: "code", text: span.text });
        i = span.end;
        continue;
      }
    }

    if (ch === "[" && i >= linkDead) {
      LINK.lastIndex = i;
      const link = LINK.exec(source);
      if (!link) {
        // The candidate failed at the first `]` after `i`. Every `[` before that `]` finds the
        // SAME `]` and fails on the same character, so none of them is worth trying — without
        // this each one re-scans the tail, and 32KB of `[` took 356ms.
        const close = source.indexOf("]", i);
        linkDead = close === -1 ? source.length : close;
      }
      if (link) {
        const href = safeHref(link[2]);
        if (href !== null) {
          flush();
          out.push({ type: "link", href, children: parseInline(link[1]) });
          i += link[0].length;
          continue;
        }
        // An unsafe scheme is not a link and is not silently dropped either: the operator sees
        // exactly the text the agent wrote.
        text += link[0];
        i += link[0].length;
        continue;
      }
    }

    if (ch === "*" || ch === "_") {
      const span = matchEmphasis(source, i, scanned);
      if (span) {
        flush();
        out.push(span.node);
        i = span.end;
        continue;
      }
    }

    text += ch;
    i += 1;
  }
  flush();
  return out;
}

/** A code span: N backticks, then the next run of exactly N. Its content stays literal. */
function matchCode(source: string, start: number): { text: string; end: number } | null {
  let open = start;
  while (open < source.length && source[open] === "`") open += 1;
  const ticks = open - start;
  const marker = "`".repeat(ticks);
  let from = open;
  for (;;) {
    const close = source.indexOf(marker, from);
    if (close === -1) return null;
    // A longer run is not the closer — `` a ``` b `` keeps looking.
    if (source[close + ticks] === "`") {
      from = close + ticks;
      while (from < source.length && source[from] === "`") from += 1;
      continue;
    }
    if (close === open) return null;
    let text = source.slice(open, close);
    if (text.startsWith(" ") && text.endsWith(" ") && text.trim() !== "") text = text.slice(1, -1);
    return { text, end: close + ticks };
  }
}

/**
 * `*em*`, `**strong**` and `***both***`.
 *
 * The flanking rules are the ones that matter for this codebase's prose: an underscore inside a
 * word never opens emphasis (`symphony_run_status` is an identifier, not italics), and a
 * delimiter followed by a space never opens one either (`2 * 3 * 4` is arithmetic).
 */
function matchEmphasis(
  source: string,
  start: number,
  scanned: Map<string, number>,
): { node: MdInline; end: number } | null {
  const delim = source[start];
  let open = start;
  while (open < source.length && source[open] === delim) open += 1;
  const run = Math.min(open - start, 3);
  const marker = delim.repeat(run);
  const before = start === 0 ? "" : source[start - 1];

  if (delim === "_" && before !== "" && WORD.test(before)) return null;
  const first = source[start + run];
  if (first === undefined || /\s/.test(first)) return null;

  // Everything below `scanned` was already rejected as a closer for this marker.
  let from = Math.max(start + run, scanned.get(marker) ?? 0);
  for (;;) {
    const close = source.indexOf(marker, from);
    if (close === -1) {
      scanned.set(marker, source.length);
      return null;
    }
    if (close === start + run) return null;
    const prev = source[close - 1];
    const after = source[close + run] ?? "";
    if (/\s/.test(prev) || (delim === "_" && after !== "" && WORD.test(after))) {
      from = close + 1;
      scanned.set(marker, from);
      continue;
    }
    const children = parseInline(source.slice(start + run, close));
    const node: MdInline =
      run === 1
        ? { type: "em", children }
        : run === 2
          ? { type: "strong", children }
          : { type: "strong", children: [{ type: "em", children }] };
    return { node, end: close + run };
  }
}
