export type SettingsTabId = "general" | "projects" | "tools" | "logs" | "updates";

// The shell shows one of two top-level views: the Runs dashboard (default) or Settings. There is
// no longer a visible tab strip — Runs is the whole main area and the titlebar's gear button
// toggles Settings (INF-… top-bar simplification). Kept as a named union so the state + tray
// navigation stay typed.
export type TopTabId = "runs" | "settings";

// The single top-level panel that holds whichever view (Runs or Settings) is active.
export const TOP_PANEL_ID = "shell-top-panel";

// The Settings rail is a vertical tablist controlling a single titled panel; each rail item
// points its aria-controls at the panel and the panel labels itself by the active rail item.
// Shared with components/settings/Settings so the a11y contract stays in one place.
export const SETTINGS_PANEL_ID = "shell-settings-panel";
export const settingsRailTabId = (id: SettingsTabId) => `shell-railtab-${id}`;

// ComingSoonPanel — a dashed "ships in a follow-on ticket" panel. The Settings tab bodies
// (INF-226) reuse it for not-yet-built tabs as the stack is built up branch by branch. (The Runs
// route shipped its re-skinned view in INF-227, replacing the former RunsPlaceholder.)
export function ComingSoonPanel({ note }: { note: string }) {
  return (
    <div
      style={{
        border: "1px dashed var(--line-strong)",
        borderRadius: "var(--r-card)",
        background: "var(--bg-card-2)",
        padding: "40px 22px",
        textAlign: "center",
        color: "var(--tx-3)",
        fontSize: 13,
        lineHeight: 1.6,
      }}
    >
      {note}
    </div>
  );
}
