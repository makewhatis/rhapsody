// The room's per-kind glyphs, traced from the prototype's markup
// (`~/.rhapsody/docs/STUDIO-681-prototype.html`, the `teams` view). They inherit `currentColor`,
// so the kind's rail color in `theme/teams-console.css` tints the icon with no extra props —
// which is what makes §5's "color rail + icon" one decision instead of two that can disagree.
import type { SVGProps } from "react";
import type { RoomKind } from "@/lib/room-model";

type Glyph = Omit<SVGProps<SVGSVGElement>, "viewBox" | "children">;

const PATHS: Record<RoomKind, string> = {
  // A person: the operator's own voice.
  operator: "M8 8a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM3 13.5a5 5 0 0 1 10 0",
  // An arrow handing something on.
  handoff: "M2 8h9M8 4l4 4-4 4",
  // A tag: work routed to a teammate.
  assign: "M2.5 8.5 8 3h5v5l-5.5 5.5zM10.5 5.5h.01",
  // A cycle: the stray-label sweep putting things back.
  reconcile: "M3 8a5 5 0 0 1 9-3M13 8a5 5 0 0 1-9 3M11 2v3H8M5 14v-3h3",
  // A warning triangle: the review that did not happen.
  quorum: "M8 5v4M8 11h.01M8 1.5 14.5 13h-13z",
};

/** The glyph for one room event kind. */
export function KindIcon({ kind, ...rest }: Glyph & { kind: RoomKind }) {
  return (
    <svg
      className="ic"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={kind === "reconcile" ? 1.5 : 1.6}
      aria-hidden="true"
      {...rest}
    >
      <path d={PATHS[kind]} />
    </svg>
  );
}

/** The magnifier the room's search box leads with. */
export function SearchIcon(props: Glyph) {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.6} aria-hidden="true" {...props}>
      <circle cx="7" cy="7" r="4.5" />
      <path d="M11 11l3 3" />
    </svg>
  );
}
