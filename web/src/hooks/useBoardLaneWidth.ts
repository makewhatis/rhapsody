import { useCallback, useEffect, useState } from "react";
import type { ViewStorage } from "@/hooks/useJobsViewMode";

/**
 * How wide a board lane is (STUDIO-930): a default and one step either way, named by the result
 * rather than by comparison. Each drives `--lane-w` in `console-views.css`.
 */
export type BoardLaneWidth = "compact" | "default" | "wide";

/** The Seg's options, in display order. */
export const BOARD_LANE_WIDTHS: readonly { value: BoardLaneWidth; label: string }[] = [
  { value: "compact", label: "Compact" },
  { value: "default", label: "Default" },
  { value: "wide", label: "Wide" },
];

/** The storage slot, beside `rhapsody.console.jobsView`. */
export const BOARD_LANE_WIDTH_KEY = "rhapsody.console.boardLaneWidth";

function isLaneWidth(value: string | null): value is BoardLaneWidth {
  return value === "compact" || value === "default" || value === "wide";
}

/**
 * The persisted width, or `default` when nothing valid is stored or storage is unreadable. A
 * locked-down webview throws on access, so the read is guarded: a preference must never cost the
 * operator the page.
 */
export function readBoardLaneWidth(storage: ViewStorage | undefined): BoardLaneWidth {
  if (storage === undefined) return "default";
  try {
    const stored = storage.getItem(BOARD_LANE_WIDTH_KEY);
    return isLaneWidth(stored) ? stored : "default";
  } catch {
    return "default";
  }
}

/** Best-effort persist; a failure only costs the choice on the next visit. */
export function writeBoardLaneWidth(storage: ViewStorage | undefined, width: BoardLaneWidth): void {
  if (storage === undefined) return;
  try {
    storage.setItem(BOARD_LANE_WIDTH_KEY, width);
  } catch {
    // Unwritable storage is not an error the operator can act on.
  }
}

/** `window.localStorage`, or undefined where it is absent (SSR, a bare test env). */
function browserStorage(): ViewStorage | undefined {
  if (typeof window === "undefined") return undefined;
  try {
    return window.localStorage ?? undefined;
  } catch {
    return undefined;
  }
}

/** The board's lane-width state, remembered across visits in `localStorage`. */
export function useBoardLaneWidth(): [BoardLaneWidth, (width: BoardLaneWidth) => void] {
  const [width, setWidth] = useState<BoardLaneWidth>(() => readBoardLaneWidth(browserStorage()));
  useEffect(() => {
    writeBoardLaneWidth(browserStorage(), width);
  }, [width]);
  const set = useCallback((next: BoardLaneWidth) => setWidth(next), []);
  return [width, set];
}
