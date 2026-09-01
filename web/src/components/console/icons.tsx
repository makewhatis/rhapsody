// The console's inline SVG glyphs, traced from the prototype's markup
// (`~/.rhapsody/docs/STUDIO-681-prototype.html`). They inherit `currentColor` so the
// nav's active/inactive treatment and the Note variants tint them with no extra props.
import type { ReactNode, SVGProps } from "react";

type Glyph = Omit<SVGProps<SVGSVGElement>, "viewBox" | "children">;

function Stroke({ strokeWidth = 1.5, children, ...rest }: Glyph & { children: ReactNode }) {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={strokeWidth} aria-hidden="true" {...rest}>
      {children}
    </svg>
  );
}

export function JobsIcon(props: Glyph) {
  return (
    <Stroke {...props}>
      <rect x="2" y="2.5" width="12" height="3" rx="1" />
      <rect x="2" y="7" width="12" height="3" rx="1" />
      <rect x="2" y="11.5" width="12" height="2.5" rx="1" />
    </Stroke>
  );
}

export function TeamsIcon(props: Glyph) {
  return (
    <Stroke {...props}>
      <circle cx="5.5" cy="6" r="2.2" />
      <circle cx="11" cy="6.5" r="1.8" />
      <path d="M1.5 13a4 4 0 0 1 8 0M9.5 12.5a3.4 3.4 0 0 1 5 .5" />
    </Stroke>
  );
}

export function MemoryIcon(props: Glyph) {
  return (
    <Stroke {...props}>
      <path d="M8 2.5c-2 0-3 1.2-3 2.6 0 .6.2 1 .5 1.4-.6.4-1 1-1 1.9 0 1.5 1.2 2.6 3 2.6M8 2.5c2 0 3 1.2 3 2.6 0 .6-.2 1-.5 1.4.6.4 1 1 1 1.9 0 1.5-1.2 2.6-3 2.6M8 2.5v9" />
    </Stroke>
  );
}

export function SettingsIcon(props: Glyph) {
  return (
    <Stroke {...props}>
      <circle cx="8" cy="8" r="2.2" />
      <path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M12.6 3.4l-1.4 1.4M4.8 11.2l-1.4 1.4" />
    </Stroke>
  );
}

/** The warn triangle a `<Note variant="warn">` leads with. */
export function WarnIcon(props: Glyph) {
  return (
    <Stroke strokeWidth={1.6} {...props}>
      <path d="M8 6v3M8 11h.01M8 1.5 14.5 13h-13z" />
    </Stroke>
  );
}

/** The info disc a `<Note variant="info">` leads with. */
export function InfoIcon(props: Glyph) {
  return (
    <Stroke strokeWidth={1.6} {...props}>
      <path d="M8 2a6 6 0 100 12A6 6 0 008 2M8 5v.01M8 7.5v4" />
    </Stroke>
  );
}
