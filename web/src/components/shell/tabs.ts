import type * as React from "react";

// Roving-focus keyboard handler for an ARIA tablist (WAI-ARIA APG "tabs" pattern): arrow keys
// move focus + selection between tabs and Home/End jump to the ends. The design package's nav
// (app.jsx) is mouse-only, but the shell exposes role="tablist"/role="tab", so it owes the
// matching keyboard model. Wire it on the tablist container's onKeyDown; `ids` is the tab
// order and `orientation` selects which arrow keys are live (Left/Right vs Up/Down).
export function handleTablistKeyDown<T extends string>(
  e: React.KeyboardEvent<HTMLElement>,
  ids: readonly T[],
  active: T,
  onChange: (id: T) => void,
  orientation: "horizontal" | "vertical",
): void {
  const nextKey = orientation === "horizontal" ? "ArrowRight" : "ArrowDown";
  const prevKey = orientation === "horizontal" ? "ArrowLeft" : "ArrowUp";
  const current = ids.indexOf(active);
  if (current < 0) return;

  let target = current;
  if (e.key === nextKey) target = (current + 1) % ids.length;
  else if (e.key === prevKey) target = (current - 1 + ids.length) % ids.length;
  else if (e.key === "Home") target = 0;
  else if (e.key === "End") target = ids.length - 1;
  else return;

  e.preventDefault();
  if (target !== current) onChange(ids[target]);
  // Move DOM focus to the target tab so keyboard navigation tracks the active tab (the tabs
  // render in `ids` order within the container).
  const tabs = e.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]');
  tabs[target]?.focus();
}
