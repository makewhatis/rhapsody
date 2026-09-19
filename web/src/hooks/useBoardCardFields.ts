import { useCallback, useEffect, useState } from "react";
import type { ViewStorage } from "@/hooks/useJobsViewMode";

/**
 * The board card's optional elements (STUDIO-932). The ticket key, title and status pill are the
 * card's IDENTITY and are deliberately absent: these are the five details an operator may hide to
 * make a card scan faster, never the things that say which ticket it is.
 */
export type BoardCardField = "assignee" | "project" | "harness" | "reviews" | "pullRequest";

/** The toggle chips, in display order. */
export const BOARD_CARD_FIELDS: readonly { id: BoardCardField; label: string }[] = [
  { id: "assignee", label: "Assignee" },
  { id: "project", label: "Project" },
  { id: "harness", label: "Harness" },
  { id: "reviews", label: "Reviews" },
  { id: "pullRequest", label: "Pull request" },
];

/** Whether each card element is shown. */
export type BoardCardFields = Record<BoardCardField, boolean>;

/** The default: every element shown, so the board reads exactly as it did before the chips existed. */
export const DEFAULT_BOARD_CARD_FIELDS: BoardCardFields = {
  assignee: true,
  project: true,
  harness: true,
  reviews: true,
  pullRequest: true,
};

/** The storage slot, beside `rhapsody.console.jobsView` and `.boardLaneWidth`. */
export const BOARD_CARD_FIELDS_KEY = "rhapsody.console.cardFields";

function isField(value: string): value is BoardCardField {
  return BOARD_CARD_FIELDS.some((field) => field.id === value);
}

/** True when every element is shown — the state the trigger draws no dot for. */
export function boardCardFieldsAreDefault(fields: BoardCardFields): boolean {
  return BOARD_CARD_FIELDS.every((field) => fields[field.id]);
}

/**
 * The persisted visibility set, or every field shown when nothing is stored or storage is
 * unreadable. The value is the ENABLED field ids joined with commas, so an all-hidden choice is the
 * empty string and stays distinguishable from an absent key. A locked-down webview throws on access
 * rather than returning null, so the read is guarded: a preference must never cost the operator the
 * page.
 */
export function readBoardCardFields(storage: ViewStorage | undefined): BoardCardFields {
  const fallback = { ...DEFAULT_BOARD_CARD_FIELDS };
  if (storage === undefined) return fallback;
  try {
    const stored = storage.getItem(BOARD_CARD_FIELDS_KEY);
    if (stored === null) return fallback;
    const on = new Set(stored.split(",").filter(isField));
    return {
      assignee: on.has("assignee"),
      project: on.has("project"),
      harness: on.has("harness"),
      reviews: on.has("reviews"),
      pullRequest: on.has("pullRequest"),
    };
  } catch {
    return fallback;
  }
}

/** Best-effort persist; a failure only costs the choice on the next visit. */
export function writeBoardCardFields(storage: ViewStorage | undefined, fields: BoardCardFields): void {
  if (storage === undefined) return;
  try {
    storage.setItem(
      BOARD_CARD_FIELDS_KEY,
      BOARD_CARD_FIELDS.filter((field) => fields[field.id])
        .map((field) => field.id)
        .join(","),
    );
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

/**
 * The board's card-field visibility, remembered across visits in `localStorage`. The setter takes
 * the field and its next state so a chip neither has to spread the whole record nor read a stale
 * closure; `reset` returns every element to shown for the popover's Reset.
 */
export function useBoardCardFields(): [
  BoardCardFields,
  (field: BoardCardField, on: boolean) => void,
  () => void,
] {
  const [fields, setFields] = useState<BoardCardFields>(() => readBoardCardFields(browserStorage()));
  useEffect(() => {
    writeBoardCardFields(browserStorage(), fields);
  }, [fields]);
  const set = useCallback(
    (field: BoardCardField, on: boolean) => setFields((prev) => ({ ...prev, [field]: on })),
    [],
  );
  const reset = useCallback(() => setFields({ ...DEFAULT_BOARD_CARD_FIELDS }), []);
  return [fields, set, reset];
}
