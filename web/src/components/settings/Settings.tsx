import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, Check, SkeletonCard, StatusDot } from "@/components/ui";
import {
  LINEAR_IDENTITY_QUERY_KEY,
  useLinearIdentity,
  useLinearProjects,
  useProjectStatuses,
  useSaveTypedConfig,
  useTypedConfigQuery,
} from "@/hooks/useConfig";
import { appVersion, clearLinearToken, setLinearToken, type VersionDTO } from "@/lib/bindings";
import type { GlobalConfigDTO, LinearProject, ProjectConfigDTO } from "@/lib/api";
import { ConfigSaveError } from "@/lib/api";
import {
  applyUiAgent,
  applyUiGlobal,
  autosaveView,
  clampProjectCaps,
  doctorHasWarnings,
  duplicateSlugs,
  globalPromoteValid,
  newProjectConfig,
  reviewPromoteValid,
  toUiAgents,
  toUiGlobal,
  type UiAgent,
  type UiGlobal,
} from "@/lib/settings-model";
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
import { ToolsTab } from "./ToolsTab";
import { LogsTab } from "./LogsTab";
import { UpdatesTab } from "./UpdatesTab";

// Autosave debounce: coalesce rapid edits (stepper clicks, typing) into one POST after the user
// pauses. The Save button is retired (mock 2a/2b) — edits persist on their own.
const AUTOSAVE_DEBOUNCE_MS = 600;

// The Settings rail is text-only (mock 2a–2d): no leading icons. "Projects" carries a live count
// badge; "Tools" carries an amber warning-dot slot lit by the doctor (wired in D6).
const SETTINGS_NAV: { id: SettingsTabId; label: string }[] = [
  { id: "general", label: "General" },
  { id: "projects", label: "Projects" },
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

type Draft = { global: GlobalConfigDTO; projects: ProjectConfigDTO[] };

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
  const cfg = useTypedConfigQuery();
  const identity = useLinearIdentity();
  const linearProjects = useLinearProjects();
  const statuses = useProjectStatuses();
  const save = useSaveTypedConfig();
  const qc = useQueryClient();
  // The preflight/doctor probe, shared with the Tools tab via TanStack's cache (same query key). It
  // mounts here so the probe runs as soon as Settings opens ("re-checked on launch"), lighting the
  // rail's Tools amber dot even before the Tools tab is visited. The Tools tab's "Re-run preflight"
  // updates the same cache, so the dot re-derives without any extra wiring.
  const doctor = useToolDoctor();
  const toolsWarn = doctorHasWarnings(doctor.data ?? []);

  const [draft, setDraft] = React.useState<Draft | null>(null);
  const [dirty, setDirty] = React.useState(false);
  const [token, setToken] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [sheetOpen, setSheetOpen] = React.useState(false);
  const [flushing, setFlushing] = React.useState(false);
  // The originally-loaded global, so a persist-artifacts off→on toggle can restore the real path.
  const baseGlobal = React.useRef<GlobalConfigDTO | null>(null);
  // Monotonic counter bumped synchronously on every draft/token edit. A save captures it at start
  // and only marks the form clean if it's unchanged when the POST resolves — so an edit racing the
  // in-flight save is never silently discarded (a render-updated ref would be subject to paint
  // timing and could miss the race).
  const editSeq = React.useRef(0);

  // Re-sync the draft from the server whenever a fresh config arrives and there are no local edits
  // in flight (so a background refetch / post-save echo never clobbers an in-progress edit).
  React.useEffect(() => {
    if (cfg.data?.global && !dirty) {
      // Only remember a baseline with a REAL storage path, so a persist-artifacts off→on toggle can
      // restore the on-disk database path even after a save that wrote "off" (see applyUiGlobal).
      if (cfg.data.global.storage.path !== "off") baseGlobal.current = cfg.data.global;
      setDraft({
        global: structuredClone(cfg.data.global),
        projects: structuredClone(cfg.data.projects ?? []),
      });
    }
  }, [cfg.data, dirty]);

  const onGlobalChange = (ui: UiGlobal) => {
    editSeq.current++;
    setDraft((d) => (d ? { ...d, global: applyUiGlobal(d.global, ui, baseGlobal.current ?? undefined) } : d));
    setError(null);
    setDirty(true);
  };

  const onTokenChange = (t: string) => {
    editSeq.current++;
    setToken(t);
    setError(null);
    setDirty(true);
  };

  // onAgentChange folds a detail-editor edit into the draft. review_promote_state is global in the
  // daemon, so an agent's promote selection is written onto the global (validated per-agent in the
  // editor); the rest of the edit lands on that agent's project entry.
  const onAgentChange = (index: number, ui: UiAgent) => {
    editSeq.current++;
    setDraft((d) => {
      if (!d) return d;
      const projects = d.projects.slice();
      projects[index] = applyUiAgent(d.projects[index], ui, d.global);
      const global =
        ui.reviewPromote !== d.global.review_promote_state
          ? { ...d.global, review_promote_state: ui.reviewPromote }
          : d.global;
      return { global, projects };
    });
    setError(null);
    setDirty(true);
  };

  // flushPendingToken writes a pasted Linear token to the macOS keychain (Go binding). Returns true
  // when a token was flushed (so the caller refreshes the connected-as identity). The raw token
  // never enters the config payload.
  const flushPendingToken = async (): Promise<boolean> => {
    const pending = token.trim();
    if (pending === "") return false;
    setFlushing(true);
    try {
      await setLinearToken(pending);
    } finally {
      setFlushing(false);
    }
    return true;
  };

  // persist flushes any pending token, POSTs `snapshot`, and — only if no edit raced the in-flight
  // save (the edit sequence captured at `seqAtStart` is unchanged) — marks the form clean. Staying
  // dirty on a race keeps the resync effect (gated on !dirty) from clobbering the newer edit and
  // re-arms the autosave for the racing edit.
  const persist = (snapshot: Draft, seqAtStart: number): Promise<void> =>
    (async () => {
      const flushed = await flushPendingToken();
      // Clamp per-agent caps to the (possibly just-lowered) global max before POST.
      const projects = clampProjectCaps(snapshot.projects, snapshot.global.agent.max_concurrent_agents);
      await save.mutateAsync({ global: snapshot.global, projects });
      if (flushed) void qc.invalidateQueries({ queryKey: LINEAR_IDENTITY_QUERY_KEY });
      if (editSeq.current === seqAtStart) {
        setDirty(false);
        setToken("");
      }
    })();

  // The list enable/pause toggle and remove are plain draft edits: they mark the form dirty and the
  // autosave persists them with the rest of the config (one atomic POST).
  const onToggleAgent = (index: number, enabled: boolean) => {
    editSeq.current++;
    setDraft((d) => (d ? { ...d, projects: d.projects.map((p, i) => (i === index ? { ...p, enabled } : p)) } : d));
    setError(null);
    setDirty(true);
  };

  const onRemoveAgent = (index: number) => {
    editSeq.current++;
    setDraft((d) => (d ? { ...d, projects: d.projects.filter((_, i) => i !== index) } : d));
    setError(null);
    setDirty(true);
  };

  // Creating an agent appends it to the draft as a dirty edit and closes the sheet; the autosave
  // persists it with the rest of the config. On a save failure the new agent stays in the draft as a
  // pending edit (surfaced via the error indicator) rather than being lost.
  const onCreate = (project: LinearProject, repo: string) => {
    if (!draft) return;
    editSeq.current++;
    setDraft({ global: draft.global, projects: [...draft.projects, newProjectConfig(project, repo)] });
    setDirty(true);
    setSheetOpen(false);
    setError(null);
  };

  const onDisconnect = () => {
    void clearLinearToken()
      .then(() => qc.invalidateQueries({ queryKey: LINEAR_IDENTITY_QUERY_KEY }))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : "Disconnect failed"));
  };

  const meta = TAB_META[tab];
  const uiGlobal = draft ? toUiGlobal(draft.global) : null;
  const agents =
    draft ? toUiAgents(draft.projects, draft.global, linearProjects.data ?? [], statuses.data ?? []) : [];
  const projectCount = draft?.projects.length ?? cfg.data?.projects?.length ?? 0;
  // Block autosave when the review-promote state would fail the daemon's validation — at the global
  // scope (global review on → promote ∈ global active states) and/or any agent's per-project scope.
  // The daemon would reject such a POST; the detail editor flags an offending agent inline.
  const promoteGlobalInvalid = draft ? !globalPromoteValid(draft.global) : false;
  const promoteAgentInvalid = agents.some((a) => !reviewPromoteValid(a));
  // Each agent must watch a unique Linear project; the daemon rejects duplicate slugs.
  const slugConflict = draft ? duplicateSlugs(draft.projects) : false;
  const saveBlocked = promoteGlobalInvalid || promoteAgentInvalid || slugConflict;
  // Scope-specific message so a global-scope failure doesn't point the user at the per-agent editors.
  const blockMessage = slugConflict
    ? "Each agent must watch a unique Linear project."
    : promoteGlobalInvalid
      ? "Review-promote state must be one of the global active states."
      : promoteAgentInvalid
        ? "Review-promote state must be one of each agent's active states."
        : null;

  const saving = save.isPending || flushing;

  // Autosave: debounce edits, then persist — but never while blocked by validation (the daemon would
  // reject the POST) or while a save/token-flush is already in flight. The effect re-runs on every
  // edit (draft/token change), so the timer always fires with the latest snapshot.
  React.useEffect(() => {
    if (!draft || !dirty || saveBlocked || saving) return;
    const seq = editSeq.current;
    const snapshot = draft;
    const timer = setTimeout(() => {
      setError(null);
      void persist(snapshot, seq).catch((e: unknown) => {
        setError(e instanceof ConfigSaveError || e instanceof Error ? e.message : "Save failed");
      });
    }, AUTOSAVE_DEBOUNCE_MS);
    return () => clearTimeout(timer);
    // Deps intentionally cover the edit signals (draft/token/dirty) + the save gates
    // (saveBlocked/saving); `persist` is a fresh per-render closure captured at fire time, so it is
    // deliberately not a dependency (adding it would re-arm the timer every render).
  }, [draft, token, dirty, saveBlocked, saving]);

  // Tools, Logs, and Updates are read-only, config-independent panels: they don't read the config
  // draft, so they render before the config load/skeleton guards (and hide the autosave indicator).
  const readOnlyTab = tab === "tools" || tab === "logs" || tab === "updates";

  let body: React.ReactNode;
  if (tab === "tools") {
    body = <ToolsTab />;
  } else if (tab === "logs") {
    body = <LogsTab />;
  } else if (tab === "updates") {
    body = <UpdatesTab updater={updater} />;
  } else if (cfg.isError || (cfg.data && !cfg.data.global)) {
    body = <ComingSoonPanel note="Couldn't load the daemon configuration. Is the daemon running?" />;
  } else if (!draft || !uiGlobal) {
    body = (
      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <SkeletonCard />
        <SkeletonCard />
      </div>
    );
  } else if (tab === "general") {
    body = (
      <GeneralTab
        value={uiGlobal}
        onChange={onGlobalChange}
        account={identity.data ?? null}
        token={token}
        onTokenChange={onTokenChange}
        onDisconnect={onDisconnect}
      />
    );
  } else {
    body = (
      <ProjectsTab
        agents={agents}
        global={uiGlobal}
        linearProjects={linearProjects.data ?? []}
        mode="quiet"
        listStyle="rows"
        onToggle={onToggleAgent}
        onAgentChange={onAgentChange}
        onRemove={onRemoveAgent}
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
                badge={s.id === "projects" ? projectCount : undefined}
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
              <AutosaveIndicator dirty={dirty} saving={saving} blocked={blockMessage} error={error} />
            ) : null}
          </div>
          {body}
        </div>
      </div>
      {draft && uiGlobal ? (
        <AddAgentSheet
          open={sheetOpen}
          onClose={() => setSheetOpen(false)}
          onCreate={onCreate}
          projects={linearProjects.data ?? []}
          usedSlugs={draft.projects.flatMap((p) => p.slugs)}
          blockedReason={saveBlocked ? blockMessage : null}
          global={uiGlobal}
        />
      ) : null}
    </>
  );
}
