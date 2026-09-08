import { memo, useMemo, type ReactNode } from "react";
import { ExternalLink } from "./ExternalLink";
import { parseMarkdown, type MdBlock, type MdInline, type MdList } from "@/lib/markdown";
import "@/theme/markdown.css";

// Markdown — renders one body of AGENT-AUTHORED prose (STUDIO-739).
//
// Every transcript line, room post and retained fact is markdown the agent typed, and the
// console used to print it verbatim. This renders `lib/markdown`'s tree as React ELEMENTS: no
// `dangerouslySetInnerHTML` anywhere, so a `<script>` or an `<img onerror=…>` in a body is a
// string React escapes, and there is no sanitizer to keep in step with a parser.
//
// Memoized on the body: a long transcript re-renders (a new poll, a row opening) without
// re-parsing bodies that did not change.

export interface MarkdownProps {
  /** The raw body. Anything outside the supported subset renders as the literal text. */
  source: string;
  /**
   * A node to sit inline ahead of the body — the transcript's tool chip. It joins the first
   * paragraph when there is one, so `Bash git rebase` still reads as one line.
   */
  lead?: ReactNode;
  /** Added to the wrapper, for a surface that tints or sizes its own prose. */
  className?: string;
}

export const Markdown = memo(function Markdown({ source, lead, className }: MarkdownProps) {
  const blocks = useMemo(() => parseMarkdown(source), [source]);
  const leadsFirst = lead !== undefined && blocks.length > 0 && blocks[0].type === "paragraph";
  return (
    <div className={className === undefined ? "md" : `md ${className}`}>
      {lead !== undefined && !leadsFirst ? <p className="mdlead">{lead}</p> : null}
      {blocks.map((block, i) => (
        <Block key={i} block={block} lead={leadsFirst && i === 0 ? lead : undefined} />
      ))}
    </div>
  );
});

function Block({ block, lead }: { block: MdBlock; lead?: ReactNode }) {
  switch (block.type) {
    case "heading":
      return <Heading level={block.level}>{<Inlines nodes={block.children} />}</Heading>;
    case "code":
      // The wide-content rule (STUDIO-681): the block scrolls inside its own box so the page
      // body never scrolls sideways. `data-lang` records the fence's language for a later
      // highlighter without claiming one now.
      // `tabIndex` is what makes the scroll reachable without a mouse: a scrollable region that
      // cannot take focus cannot be scrolled from the keyboard at all.
      return (
        <pre className="mdpre" tabIndex={0} data-lang={block.lang === "" ? undefined : block.lang}>
          <code className="mdcode">{block.text}</code>
        </pre>
      );
    case "list":
      return <List list={block} />;
    default:
      return (
        <p>
          {lead === undefined ? null : (
            <>
              {lead}{" "}
            </>
          )}
          <Inlines nodes={block.children} />
        </p>
      );
  }
}

/**
 * A body's heading sits INSIDE a card whose own title is an `h2`, so the author's level is
 * shifted two down (`#` → `h3`) and clamped at `h6`. The class keeps the author's intended
 * relative size, which the shift would otherwise flatten.
 */
function Heading({ level, children }: { level: number; children: ReactNode }) {
  const className = `mdh mdh${level}`;
  switch (Math.min(level + 2, 6)) {
    case 3:
      return <h3 className={className}>{children}</h3>;
    case 4:
      return <h4 className={className}>{children}</h4>;
    case 5:
      return <h5 className={className}>{children}</h5>;
    default:
      return <h6 className={className}>{children}</h6>;
  }
}

function List({ list }: { list: MdList }) {
  const items = list.items.map((item, i) => (
    <li key={i}>
      <Inlines nodes={item.children} />
      {item.list === null ? null : <List list={item.list} />}
    </li>
  ));
  return list.ordered ? <ol>{items}</ol> : <ul>{items}</ul>;
}

function Inlines({ nodes }: { nodes: readonly MdInline[] }) {
  return (
    <>
      {nodes.map((node, i) => (
        <Inline key={i} node={node} />
      ))}
    </>
  );
}

function Inline({ node }: { node: MdInline }) {
  switch (node.type) {
    case "text":
      return <>{node.text}</>;
    case "code":
      return <code>{node.text}</code>;
    case "strong":
      return (
        <strong>
          <Inlines nodes={node.children} />
        </strong>
      );
    case "em":
      return (
        <em>
          <Inlines nodes={node.children} />
        </em>
      );
    default:
      // The parser only builds a link for an http/https/mailto href; every other scheme
      // arrives here as text. `ExternalLink` keeps the console out of the target's referrer
      // and hands an http(s) click to the OS browser (STUDIO-765) — a `mailto:` stays a
      // plain anchor there, since the host command opens web URLs only.
      return (
        <ExternalLink href={node.href}>
          <Inlines nodes={node.children} />
        </ExternalLink>
      );
  }
}
