// Teammate color assignment — STUDIO-681 §1.5.
//
// Avatar and name colors come from an ordered ramp indexed by the teammate's
// POSITION IN THE ROSTER. Nothing here knows the names "alice" or "jimmy": a
// roster of five gets five colors, a roster of twenty wraps the ramp, and
// reordering the roster reorders the colors. The ramp itself is declared in
// `theme/tokens.css` as `--mate-1 … --mate-6` so no hex appears in a view file.

/** The ordered ramp, longest-lived first. Extend by adding a `--mate-N` token. */
export const TEAMMATE_COLORS: readonly string[] = [
  "var(--mate-1)",
  "var(--mate-2)",
  "var(--mate-3)",
  "var(--mate-4)",
  "var(--mate-5)",
  "var(--mate-6)",
];

/**
 * Shown for a teammate the roster does not contain — a stale room post, a
 * teammate removed from `teams.yaml` between a run and the page load. Muted on
 * purpose: it must not read as one of the live ramp colors.
 */
export const UNKNOWN_TEAMMATE_COLOR = "var(--ink-3)";

/** The ramp color for a roster position, wrapping past the end of the ramp. */
export function teammateColorAt(index: number): string {
  if (!Number.isInteger(index) || index < 0) return UNKNOWN_TEAMMATE_COLOR;
  return TEAMMATE_COLORS[index % TEAMMATE_COLORS.length];
}

/** The ramp color for `name` given the roster it belongs to, in roster order. */
export function teammateColor(roster: readonly string[], name: string): string {
  return teammateColorAt(roster.indexOf(name));
}
