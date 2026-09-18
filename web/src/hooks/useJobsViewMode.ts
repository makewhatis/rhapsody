import { useCallback, useEffect, useState } from "react";

/**
 * Which view the Jobs home is showing (STUDIO-925). `board` is an ADDITIONAL view over the table,
 * not a replacement, so the operator's choice is remembered rather than reset on every visit.
 */
export type JobsViewMode = "list" | "board";

/** The storage slot. Namespaced so it cannot collide with another console preference. */
export const JOBS_VIEW_KEY = "rhapsody.console.jobsView";

/** The slice of `Storage` this needs, so a test can pass a fake without a DOM. */
export interface ViewStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/**
 * The persisted value, or `list` — the view the console had before this existed — when nothing is
 * stored or storage is unreadable. A locked-down webview or private mode throws on access rather
 * than returning null, so the read is guarded: a preference must never cost the operator the page.
 */
export function readJobsView(storage: ViewStorage | undefined): JobsViewMode {
  if (storage === undefined) return "list";
  try {
    return storage.getItem(JOBS_VIEW_KEY) === "board" ? "board" : "list";
  } catch {
    return "list";
  }
}

/** Best-effort persist; a failure only costs the choice on the next visit. */
export function writeJobsView(storage: ViewStorage | undefined, view: JobsViewMode): void {
  if (storage === undefined) return;
  try {
    storage.setItem(JOBS_VIEW_KEY, view);
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

/** The Jobs view toggle's state, remembered across visits in `localStorage`. */
export function useJobsViewMode(): [JobsViewMode, (view: JobsViewMode) => void] {
  const [view, setView] = useState<JobsViewMode>(() => readJobsView(browserStorage()));
  useEffect(() => {
    writeJobsView(browserStorage(), view);
  }, [view]);
  const set = useCallback((next: JobsViewMode) => setView(next), []);
  return [view, set];
}
