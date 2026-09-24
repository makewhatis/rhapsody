import type { ReactNode } from "react";
import { LogsTab } from "@/components/settings/LogsTab";
import { ToolsTab } from "@/components/settings/ToolsTab";
import { UpdatesTab } from "@/components/settings/UpdatesTab";
import { ProvidersTab } from "@/components/settings/ProvidersTab";
import { useConfigDraft } from "@/hooks/useConfigDraft";
import type { Updater } from "@/hooks/useUpdater";
import "@/theme/console-settings-tabs.css";

// Tools, Logs and Updates — the three Settings surfaces the console was missing (STUDIO-681 §8.1,
// built by STUDIO-691). They exist for one reason: the §2.2.1 go-live flip may not cost the
// operator a capability the shipped Podium Settings has, and the STUDIO-687 audit found these three
// unreachable from the console (gaps G4, G5 and G3). Updates is the sharpest of them — it is the
// desktop app's ENTIRE auto-update path.
//
// They follow STUDIO-690's WorkflowView pattern exactly, and for the same reason: they do not
// rebuild anything. `ToolsTab`, `LogsTab` and `UpdatesTab` ARE the shipped surfaces, embedded as
// they are, over the same hooks and therefore the same data paths — `useToolDoctor`'s `probeTools`
// binding, `useLogStream`'s `GET /api/v1/logs/stream` (or its Tauri channel bridge), and
// `useUpdater`'s U1 update commands. No endpoint is invented and no capability is re-implemented,
// so parity is a property of the code rather than a claim: one surface, rendered in two shells.
// What the console contributes is the §8 chrome — the breadcrumb back to Settings, the heading and
// the lead — from §1 components.
//
// The three views share this file because they are one pattern rather than three views; splitting
// them would mean three copies of `Page`.

export interface SettingsTabViewProps {
  /** Route away — the breadcrumb returns to the Settings hub each row lives on (§8). */
  onNavigate: (route: "settings") => void;
}

/** Tools — the tool doctor: required-CLI preflight, path overrides, the Linear connection mirror. */
export function ToolsView({ onNavigate }: SettingsTabViewProps) {
  return (
    <Page
      onNavigate={onNavigate}
      title="Tools"
      lead="The CLIs Rhapsody shells out to, re-checked on launch. Override a path when a binary is not on your PATH."
    >
      <ToolsTab />
    </Page>
  );
}

/** Logs — the live daemon log tail: polling, dispatch, restarts and errors. */
export function LogsView({ onNavigate }: SettingsTabViewProps) {
  return (
    <Page
      onNavigate={onNavigate}
      title="Logs"
      lead="The daemon's live process log — polling, dispatch, restarts and errors."
    >
      <LogsTab />
    </Page>
  );
}

/** Updates — the desktop auto-update surface (P11 U3). */
export function UpdatesView({ onNavigate, updater }: SettingsTabViewProps & { updater: Updater }) {
  return (
    <Page
      onNavigate={onNavigate}
      title="Updates"
      lead="Keep Rhapsody current — check for, download and install new versions."
    >
      <UpdatesTab updater={updater} />
    </Page>
  );
}

/**
 * Providers — the provider registry, credential status, and global provider/model selection
 * (STUDIO-992). Unlike Tools/Logs/Updates this surface reads (and edits) the WORKFLOW.md draft, so
 * it renders the SAME `useConfigDraft` model the Podium Settings and the Workflow editor use — one
 * editor of one file, not a second copy. It is not teams-gated: providers exist on a solo daemon.
 */
export function ProvidersView({ onNavigate }: SettingsTabViewProps) {
  const cfg = useConfigDraft();
  return (
    <Page
      onNavigate={onNavigate}
      title="Providers"
      lead="Inference provider definitions, credential status, and the global provider/model selection."
    >
      {cfg.unavailable ? (
        <p style={{ fontSize: 13, color: "var(--tx-3)" }}>
          Couldn't load the daemon configuration. Is the daemon running?
        </p>
      ) : !cfg.uiGlobal ? (
        <p style={{ fontSize: 13, color: "var(--tx-3)" }}>Loading configuration…</p>
      ) : (
        <ProvidersTab
          value={cfg.uiGlobal}
          onChange={cfg.onGlobalChange}
          onDefinitionsChanged={cfg.reload}
        />
      )}
    </Page>
  );
}

function Page({
  onNavigate,
  title,
  lead,
  children,
}: SettingsTabViewProps & { title: string; lead: string; children: ReactNode }) {
  return (
    <section>
      <div className="crumbs">
        {/* A button, not a link: it performs an action (routing), not a document jump — the same
            call WorkflowView's breadcrumb makes. */}
        <button type="button" className="link" onClick={() => onNavigate("settings")}>
          Settings
        </button>{" "}
        · {title}
      </div>
      <div className="head">
        <h1>{title}</h1>
      </div>
      <p className="lead">{lead}</p>
      <div className="tabembed">{children}</div>
    </section>
  );
}
