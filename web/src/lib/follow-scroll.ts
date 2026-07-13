// Pure follow-mode geometry for a "stick to the bottom" scroll container (P10-D4). Kept DOM-free
// so it is unit-testable in this repo's node-environment Vitest setup (like format.ts / runs-model.ts),
// and shared verbatim by the transcript follow hook here and D6's logs follow — one source of truth
// for the "are we at the bottom?" decision the auto-scroll and the "jump to latest ↓" affordance both
// key off.

export interface ScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

// FOLLOW_THRESHOLD_PX — how close to the bottom (px) still counts as "at the bottom". A small slack
// so sub-pixel rounding, a trailing partial line, or a freshly-appended line landing a hair below the
// viewport doesn't spuriously drop follow-mode the instant new content streams in.
export const FOLLOW_THRESHOLD_PX = 24;

// distanceFromBottom returns how many pixels the container is scrolled up from its bottom (0 when
// pinned to the bottom, larger the further up the user has scrolled). Never negative — an
// over-scroll (rubber-band) or zero-height container clamps to 0.
export function distanceFromBottom(m: ScrollMetrics): number {
  return Math.max(0, m.scrollHeight - m.clientHeight - m.scrollTop);
}

// isAtBottom reports whether a scroll container is at (or within the slack threshold of) its bottom —
// the condition that keeps follow-mode engaged and that resumes it when the user scrolls back down.
export function isAtBottom(m: ScrollMetrics, threshold = FOLLOW_THRESHOLD_PX): boolean {
  return distanceFromBottom(m) <= threshold;
}
