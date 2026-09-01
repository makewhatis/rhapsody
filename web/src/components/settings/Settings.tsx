import * as React from "react";
import { ArrowLeft, Check, SkeletonCard, StatusDot } from "@/components/ui";
import { useConfigDraft } from "@/hooks/useConfigDraft";
import { appVersion, type VersionDTO } from "@/lib/bindings";
import { autosaveView, doctorHasWarnings } from "@/lib/settings-model";
import { useToolDoctor } from "@/hooks/useToolDoctor";
import { handleTablistKeyDown } from "@/components/shell/tabs";
import {
  ComingSoonPanel,
  SETTINGS_PANEL_ID,
  settingsRailTabId,
  type SettingsTabId,
} from "@/components/shell/placeholders";
import type { Updater } from "@/hooks/useUpdater";
import { GeneralTab } from "./GeneralTab";
import { ProjectsTab } from "./ProjectsTab";
import { AddAgentSheet } from "./AddAgentSheet";
import { TeamsTab } from "./TeamsTab";
import { ToolsTab } from "./ToolsTab";
import { LogsTab } from "./LogsTab";
import { UpdatesTab } from "./UpdatesTab";

// The Settings rail is text-only (mock 2a–2d): no leading icons. "Projects" carries a live count
// badge; "Tools" carries an amber warning-dot slot lit by the doctor (wired in D6).
const SETTINGS_NAV: { id: SettingsTabId; label: string }[] = [
  { id: "general", label: "General" },
  { id: "projects", label: "Projects" },
  { id: "teams", label: "Teams" },
  { id: "tools", label: "Tools" },
  { id: "logs", label: "Logs" },
  { id: "updates", label: "Updates" },
];

const TAB_META: Record<SettingsTabId, { title: string; desc: string }> = {
  general: { title: "General", desc: "Global defaults every agent inherits." },
  projects: {
    title: "Projects",
    desc: "Each agent watches one Linear project and runs coding agents on its tickets.",
  },
  teams: {
    title: "Teams",
    desc: "Named teammates with their own profiles, memory and a shared room — off until you create teams.yaml.",
  },
  tools: { title: "Tools", desc: "Detected CLIs and connection health, re-checked on launch." },
  logs: { title: "Logs", desc: "Live daemon process log — polling, dispatch, restarts, and errors." },
  updates: { title: "Updates", desc: "Keep Rhapsody current — check for, download, and install new versions." },
};

interface RailItemProps {
  tabId: SettingsTabId;
  label: string;
  active: boolean;
  onClick: () => void;
  /** A mono count badge (e.g. the configured-agent count on "Projects"). */
  badge?: React.ReactNode;
  /** A 6px amber warning dot (e.g. "Tools" while the doctor has warnings — wired in D6). */
  warn?: boolean;
  /** A 6px rust dot (e.g. "Updates" while an in-app update is pending — P11-U3). */
  available?: boolean;
}

// RailItem — a Settings left-rail nav item (mock 2a): text-only, 12.5px, active rust text on the
// active-nav tint. "Projects" shows a mono count badge; "Tools" shows an amber warning dot when the
// doctor has warnings.
function RailItem({ tabId, label, active, onClick, badge, warn, available }: RailItemProps) {
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      role="tab"
      id={settingsRailTabId(tabId)}
      aria-selected={active}
      aria-controls={SETTINGS_PANEL_ID}
      tabIndex={active ? 0 : -1}
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 8,
        width: "100%",
        padding: "7px 10px",
        borderRadius: "var(--r-ctrl)",
        border: "none",
        cursor: "pointer",
        fontSize: 12.5,
        fontWeight: active ? 500 : 400,
        textAlign: "left",
        background: active ? "var(--tint-active-nav)" : hover ? "rgba(255,255,255,.04)" : "transparent",
        color: active ? "var(--rust-text)" : "var(--text-muted)",
        transition: "background .12s, color .12s",
      }}
    >
      <span style={{ flex: 1 }}>{label}</span>
      {warn ? (
        // 6px amber dot lit only while the doctor has warnings (mock 2a–2c). role="img"+aria-label
        // gives it an accessible name (StatusDot itself is decorative) so the warning is announced
        // and testable from the rail without opening the Tools tab.
        <span role="img" aria-label={`${label} — warnings`} style={{ display: "inline-flex" }}>
          <StatusDot color="var(--amber)" size={6} />
        </span>
      ) : null}
      {available ? (
        // 6px rust dot lit while an in-app update is pending (P11-U3) — the rail-level echo of the
        // toolbar gear dot, guiding the user to this tab. role="img"+aria-label names the decorative
        // StatusDot so it is announced and testable from the rail.
        <span role="img" aria-label={`${label} — available`} style={{ display: "inline-flex" }}>
          <StatusDot color="var(--rust-text)" size={6} />
        </span>
      ) : null}
      {badge != null ? (
        <span
          className="mono"
          style={{
            fontSize: 10,
            fontWeight: 600,
            color: active ? "var(--rust-text)" : "var(--faint)",
            background: active ? "var(--tint-rust)" : "rgba(255,255,255,.06)",
            padding: "1px 6px",
            borderRadius: "var(--r-keycap)",
          }}
        >
          {badge}
        </span>
      ) : null}
    </button>
  );
}

// RailVersion — the rail's build stamp footer (mock 2a: "0.1.0-dev · 8d288f8"), pinned to the rail
// bottom. Reads the compiled-in stamp via the desktop bridge; falls back to "dev" in a plain browser
// (no bridge), so the footer slot is always present.
function RailVersion() {
  const [v, setV] = React.useState<VersionDTO | null>(null);
  React.useEffect(() => {
    void appVersion().then(setV);
  }, []);
  const label = !v || !v.version || v.version === "dev" ? "dev" : v.version;
  const commit = v && v.commit && v.commit !== "none" ? ` · ${v.commit}` : "";
  return (
    <div
      className="mono"
      style={{ marginTop: "auto", paddingTop: 16, fontSize: 10, color: "var(--ghost)" }}
    >
      {label}
      {commit}
    </div>
  );
}

// AutosaveIndicator — the header's save state (mock 2a/2b), derived by autosaveView: "Saving…" while
// edits are pending or in flight, "✓ All changes saved" (sage) once settled, or the block/failure
// message in red. It replaces the retired Save button.
function AutosaveIndicator({ dirty, saving, blocked, error }: {
  dirty: boolean;
  saving: boolean;
  blocked: string | null;
  error: string | null;
}) {
  const view = autosaveView({ dirty, saving, blocked, error });
  if (view.kind === "error") {
    return (
      <span style={{ fontSize: 11.5, color: "var(--red)", maxWidth: 340, textAlign: "right" }}>
        {view.message}
      </span>
    );
  }
  if (view.kind === "saving") {
    return <span style={{ fontSize: 11.5, color: "var(--text-muted)" }}>Saving…</span>;
  }
  return (
    <span style={{ fontSize: 11.5, color: "var(--sage)", display: "inline-flex", alignItems: "center", gap: 5 }}>
      <Check size={13} style={{ color: "var(--sage)" }} />
      All changes saved
    </span>
  );
}

export interface SettingsProps {
  tab: SettingsTabId;
  onTab: (tab: SettingsTabId) => void;
  /** Leave Settings and return to the Runs view (the titlebar gear also toggles this). */
  onBack: () => void;
  /** The shell-owned update model (P11-U3): drives the "Updates" tab + its rail dot. */
  updater: Updater;
}

// Settings — the Settings surface (mock 2a–2d): a 188px rail (← Jobs, nav, count badge, Tools
// warning-dot slot, version footer) beside a titled content pane. It owns the working config draft
// (deep-cloned from the daemon's typed view), the dirty flag, and the pending Linear token (kept out
// of config — written to the keychain on save). Every edit — General fields, the agent detail
// editor, the list enable/pause toggle, remove, and Add-agent — is a draft edit AUTOSAVED after a
// short debounce (the Save button is retired); the header shows "Saving…" → "✓ All changes saved".
export function Settings({ tab, onTab, onBack, updater }: SettingsProps) {
  // The working config draft + its debounced autosave (STUDIO-690 lifted them into a hook so the
  // console's Workflow editor renders the SAME model — see hooks/useConfigDraft.ts).
  const cfg = useConfigDraft();
  const [sheetOpen, setSheetOpen] = React.useState(false);
  // The preflight/doctor probe, shared with the Tools tab via TanStack's cache (same query key). It
  // mounts here so the probe runs as soon as Settings opens ("re-checked on launch"), lighting the
  // rail's Tools amber dot even before the Tools tab is visited. The Tools tab's "Re-run preflight"
  // updates the same cache, so the dot re-derives without any extra wiring.
  const doctor = useToolDoctor();
  const toolsWarn = doctorHasWarnings(doctor.data ?? []);

  // Creating an agent closes the sheet; the draft edit itself is the hook's.
  const onCreate = (project: Parameters<typeof cfg.onCreateAgent>[0], repo: string) => {
    cfg.onCreateAgent(project, repo);
    setSheetOpen(false);
  };

  const meta = TAB_META[tab];

  // Tools, Logs, and Updates are read-only, config-independent panels: they don't read the config
  // draft, so they render before the config load/skeleton guards (and hide the autosave indicator).
  // "Teams" joins them (STUDIO-652): it reads teams.yaml, not WORKFLOW.md, and it deliberately has
  // no autosave — autosaving a file whose ABSENCE is the off state would create it on open.
  const readOnlyTab = tab === "tools" || tab === "logs" || tab === "updates" || tab === "teams";

  let body: React.ReactNode;
  if (tab === "teams") {
    body = <TeamsTab />;
  } else if (tab === "tools") {
    body = <ToolsTab />;
  } else if (tab === "logs") {
    body = <LogsTab />;
  } else if (tab === "updates") {
    body = <UpdatesTab updater={updater} />;
  } else if (cfg.unavailable) {
    body = <ComingSoonPanel note="Couldn't load the daemon configuration. Is the daemon running?" />;
  } else if (!cfg.draft || !cfg.uiGlobal) {
    body = (
      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <SkeletonCard />
        <SkeletonCard />
      </div>
    );
  } else if (tab === "general") {
    body = (
      <GeneralTab
        value={cfg.uiGlobal}
        onChange={cfg.onGlobalChange}
        account={cfg.account}
        token={cfg.token}
        onTokenChange={cfg.onTokenChange}
        onDisconnect={cfg.onDisconnect}
      />
    );
  } else {
    body = (
      <ProjectsTab
        agents={cfg.agents}
        global={cfg.uiGlobal}
        linearProjects={cfg.linearProjects}
        mode="quiet"
        listStyle="rows"
        onToggle={cfg.onToggleAgent}
        onAgentChange={cfg.onAgentChange}
        onRemove={cfg.onRemoveAgent}
        openSheet={() => setSheetOpen(true)}
      />
    );
  }

  return (
    <>
      <div style={{ display: "grid", gridTemplateColumns: "188px minmax(0,1fr)", alignItems: "start" }}>
        <div
          style={{
            position: "sticky",
            top: 0,
            // Fill the viewport (approx: toolbar 46px + content top pad ~26px + breathing room) so
            // the version footer pins to the rail bottom, mirroring the mock.
            height: "calc(100vh - 118px)",
            display: "flex",
            flexDirection: "column",
            paddingRight: 16,
            borderRight: "1px solid var(--hair-section)",
            overflowY: "auto",
          }}
        >
          <button
            type="button"
            onClick={onBack}
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 5,
              alignSelf: "flex-start",
              border: "none",
              background: "transparent",
              cursor: "pointer",
              color: "var(--text-muted)",
              fontSize: 12,
              padding: "2px 4px 2px 0",
              marginBottom: 10,
            }}
          >
            <ArrowLeft size={14} /> Jobs
          </button>
          <div
            style={{
              fontSize: 10,
              fontWeight: 600,
              letterSpacing: ".12em",
              textTransform: "uppercase",
              color: "var(--faint)",
              padding: "0 10px 8px",
            }}
          >
            Settings
          </div>
          <div
            role="tablist"
            aria-label="Settings"
            aria-orientation="vertical"
            onKeyDown={(e) =>
              handleTablistKeyDown(
                e,
                SETTINGS_NAV.map((s) => s.id),
                tab,
                onTab,
                "vertical",
              )
            }
            style={{ display: "flex", flexDirection: "column", gap: 2 }}
          >
            {SETTINGS_NAV.map((s) => (
              <RailItem
                key={s.id}
                tabId={s.id}
                label={s.label}
                active={tab === s.id}
                onClick={() => onTab(s.id)}
                badge={s.id === "projects" ? cfg.projectCount : undefined}
                // The Tools warning dot lights whenever the preflight/doctor probe reports a warning
                // (a required CLI missing from PATH or unhealthy) — derived from the shared doctor query.
                warn={s.id === "tools" ? toolsWarn : undefined}
                // The Updates rust dot echoes the toolbar gear dot while an in-app update is pending.
                available={s.id === "updates" ? updater.pending : undefined}
              />
            ))}
          </div>
          <RailVersion />
        </div>
        <div
          id={SETTINGS_PANEL_ID}
          role="tabpanel"
          aria-labelledby={settingsRailTabId(tab)}
          tabIndex={0}
          style={{ display: "flex", flexDirection: "column", gap: 20, minWidth: 0, outline: "none", padding: "0 0 0 26px" }}
        >
          <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 16 }}>
            <div>
              <h1 style={{ fontSize: 17, fontWeight: 600, letterSpacing: "-0.02em" }}>{meta.title}</h1>
              <p style={{ fontSize: 12, color: "var(--text-muted)", marginTop: 4, maxWidth: 560, lineHeight: 1.5 }}>
                {meta.desc}
              </p>
            </div>
            {!readOnlyTab ? (
              <AutosaveIndicator
                dirty={cfg.dirty}
                saving={cfg.saving}
                blocked={cfg.blocked}
                error={cfg.error}
              />
            ) : null}
          </div>
          {body}
        </div>
      </div>
      {cfg.draft && cfg.uiGlobal ? (
        <AddAgentSheet
          open={sheetOpen}
          onClose={() => setSheetOpen(false)}
          onCreate={onCreate}
          projects={cfg.linearProjects}
          usedSlugs={cfg.draft.projects.flatMap((p) => p.slugs)}
          blockedReason={cfg.blocked}
          global={cfg.uiGlobal}
        />
      ) : null}
    </>
  );
}
