// View glyphs for §3/§4/§8, traced from the committed prototype's markup
// (`~/.rhapsody/docs/STUDIO-681-prototype.html`). The rail's own icons live in
// `components/console/icons.tsx` (STUDIO-682); these are the ones only a view draws — the
// transcript timeline's line markers, the CI check states and the Settings row badges.
//
// All inherit `currentColor`, so `.tline.done`, `.chk.ok` and `.chk.bad` tint them with no
// extra props, exactly as the prototype's CSS does.
import type { ReactNode, SVGProps } from "react";

type Glyph = Omit<SVGProps<SVGSVGElement>, "viewBox" | "children">;

function Stroke({ strokeWidth = 1.6, children, ...rest }: Glyph & { children: ReactNode }) {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={strokeWidth} aria-hidden="true" {...rest}>
      {children}
    </svg>
  );
}

/** A turn boundary — the clock the prototype leads "Turn started" with. */
export function ClockGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <circle cx="8" cy="8" r="6" />
      <path d="M8 5v3l2 1" />
    </Stroke>
  );
}

/** A tool call — the angle brackets. */
export function ToolGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <path d="M5 4L2 8l3 4M11 4l3 4-3 4M9 3l-2 10" />
    </Stroke>
  );
}

/** A room post (`teams_post`) — the outgoing arrow. */
export function PostGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <path d="M2 8h9M8 4l4 4-4 4" />
    </Stroke>
  );
}

/** A memory retention (`teams_retain`) — the same lobed glyph the Memory nav uses. */
export function RetainGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <path d="M8 2.5c-2 0-3 1.2-3 2.6 0 .6.2 1 .5 1.4-.6.4-1 1-1 1.9 0 1.5 1.2 2.6 3 2.6M8 2.5v9" />
    </Stroke>
  );
}

/** Model prose or a tool result — the document. */
export function NoteGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <path d="M3 3h10v10H3zM6 6h4M6 8.5h4M6 11h2" />
    </Stroke>
  );
}

/** A pass — the tick the prototype uses for both a completed turn and a green check. */
export function TickGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.8} {...props}>
      <path d="M3 8l3.5 3.5L13 5" />
    </Stroke>
  );
}

/** A failing check. */
export function CrossGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.8} {...props}>
      <path d="M4 4l8 8M12 4l-8 8" />
    </Stroke>
  );
}

/** A check still running. */
export function PendingGlyph(props: Glyph) {
  return (
    <Stroke {...props}>
      <circle cx="8" cy="8" r="6" />
      <path d="M8 4.5V8l2.5 1.5" />
    </Stroke>
  );
}

/** The Settings "Teams" row badge. */
export function TeamsRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <circle cx="5.5" cy="6" r="2" />
      <circle cx="11" cy="6.5" r="1.6" />
      <path d="M2 13a3.5 3.5 0 0 1 7 0M9 12.5a3 3 0 0 1 4.5.5" />
    </Stroke>
  );
}

/** The Settings "Workflow" row badge. */
export function WorkflowRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M3 3h10v10H3zM6 6h4M6 8.5h4M6 11h2.5" />
    </Stroke>
  );
}

/** The Settings "Storage" row badge. */
export function StorageRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M2 4h12v8H2zM2 7h12" />
    </Stroke>
  );
}

/** The Settings "Telemetry" row badge. */
export function TelemetryRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M8 2v12M2 8h12" />
    </Stroke>
  );
}

/** The Settings "Tools" row badge — the tool doctor's wrench (STUDIO-691, §8.1). */
export function ToolsRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M10.5 2a3.5 3.5 0 0 0-3.2 4.9L2.6 11.6a1.4 1.4 0 0 0 2 2l4.7-4.7A3.5 3.5 0 0 0 13.4 3.6l-2 2-1.5-1.5 2-2A3.5 3.5 0 0 0 10.5 2z" />
    </Stroke>
  );
}

/** The Settings "Logs" row badge — the log tail's lines (STUDIO-691, §8.1). */
export function LogsRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M2 3h12M2 6.5h8M2 10h12M2 13.5h6" />
    </Stroke>
  );
}

/** The Settings "Updates" row badge — the updater's download arrow (STUDIO-691, §8.1). */
export function UpdatesRowGlyph(props: Glyph) {
  return (
    <Stroke strokeWidth={1.5} {...props}>
      <path d="M8 2v8M5 7.5 8 10.5l3-3M2.5 13h11" />
    </Stroke>
  );
}
