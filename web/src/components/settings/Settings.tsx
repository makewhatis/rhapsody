import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeft,
  Boxes,
  Button,
  Check,
  type IconComponent,
  ScrollText,
  SkeletonCard,
  Sliders,
  StatusDot,
  Wrench,
} from "@/components/ui";
import {
  LINEAR_IDENTITY_QUERY_KEY,
  useLinearIdentity,
  useLinearProjects,
  useProjectStatuses,
  useSaveTypedConfig,
  useTypedConfigQuery,
} from "@/hooks/useConfig";
import { clearLinearToken, setLinearToken } from "@/lib/bindings";
import type { GlobalConfigDTO, LinearProject, ProjectConfigDTO } from "@/lib/api";
import { ConfigSaveError } from "@/lib/api";
import {
  applyUiAgent,
  applyUiGlobal,
  clampProjectCaps,
  duplicateSlugs,
  globalPromoteValid,
  newProjectConfig,
  reviewPromoteValid,
  toUiAgents,
  toUiGlobal,
  type UiAgent,
  type UiGlobal,
} from "@/lib/settings-model";
import { handleTablistKeyDown } from "@/components/shell/tabs";
import {
  ComingSoonPanel,
  SETTINGS_PANEL_ID,
  settingsRailTabId,
  type SettingsTabId,
} from "@/components/shell/placeholders";
import { useToast } from "@/components/shell/Toast";
import { GeneralTab } from "./GeneralTab";
import { ProjectsTab } from "./ProjectsTab";
import { AddAgentSheet } from "./AddAgentSheet";
import { ToolsTab } from "./ToolsTab";
import { LogsTab } from "./LogsTab";

const SETTINGS_NAV: { id: SettingsTabId; label: string; icon: IconComponent }[] = [
  { id: "general", label: "General", icon: Sliders },
  { id: "projects", label: "Projects", icon: Boxes },
  { id: "tools", label: "Tools", icon: Wrench },
  { id: "logs", label: "Logs", icon: ScrollText },
];

const TAB_META: Record<SettingsTabId, { title: string; desc: string }> = {
  general: { title: "General", desc: "Global defaults every agent inherits." },
  projects: {
    title: "Projects",
    desc: "Each agent watches one Linear project and runs coding agents on its tickets.",
  },
  tools: { title: "Tools", desc: "Detected CLIs and connection health, re-checked on launch." },
  logs: { title: "Logs", desc: "Live daemon process log — polling, dispatch, restarts, and errors." },
};

interface RailItemProps {
  tabId: SettingsTabId;
  label: string;
  icon: IconComponent;
  active: boolean;
  onClick: () => void;
  badge?: React.ReactNode;
}

// RailItem — the Settings left-rail nav item (active emerald bar + soft fill, mono count badge),
// ported from the design `app.jsx`.
function RailItem({ tabId, label, icon: Icon, active, onClick, badge }: RailItemProps) {
  return (
    <button
      type="button"
      role="tab"
      id={settingsRailTabId(tabId)}
      aria-selected={active}
      aria-controls={SETTINGS_PANEL_ID}
      tabIndex={active ? 0 : -1}
      onClick={onClick}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 11,
        width: "100%",
        height: 38,
        padding: "0 12px",
        borderRadius: 9,
        border: "none",
        cursor: "pointer",
        fontSize: 13.5,
        fontWeight: 500,
        textAlign: "left",
        background: active ? "var(--em-soft)" : "transparent",
        color: active ? "var(--em-bright)" : "var(--tx-2)",
        transition: "all .13s",
        position: "relative",
      }}
    >
      <span
        aria-hidden
        style={{
          position: "absolute",
          left: 0,
          top: 9,
          bottom: 9,
          width: 2.5,
          borderRadius: 2,
          background: "var(--em-bright)",
          opacity: active ? 1 : 0,
        }}
      />
      <Icon size={16} style={{ opacity: active ? 1 : 0.85 }} />
      <span style={{ flex: 1 }}>{label}</span>
      {badge != null ? (
        <span
          className="mono"
          style={{
            fontSize: 11,
            fontWeight: 600,
            color: active ? "var(--em-bright)" : "var(--tx-faint)",
            background: active ? "transparent" : "rgba(255,255,255,.05)",
            padding: active ? 0 : "2px 7px",
            borderRadius: 999,
          }}
        >
          {badge}
        </span>
      ) : null}
    </button>
  );
}

// SaveBar — the dirty-state indicator + Save button in the panel header. Owned by INF-226 (the
// foundation shipped only the toast + sheet host). Saving POSTs the typed config and the toast
// confirms the daemon hot-reload.
function SaveBar({
  dirty,
  saving,
  error,
  invalid,
  onSave,
}: {
  dirty: boolean;
  saving: boolean;
  error: string | null;
  /** A validation error blocks Save (e.g. a review-promote state outside an agent's active states). */
  invalid?: boolean;
  onSave: () => void;
}) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 12, flexShrink: 0 }}>
      {error ? (
        <span style={{ fontSize: 12, color: "var(--red)", maxWidth: 320, textAlign: "right" }}>{error}</span>
      ) : (
        <span
          style={{
            fontSize: 12,
            color: dirty ? "var(--amber)" : "var(--tx-faint)",
            display: "inline-flex",
            alignItems: "center",
            gap: 6,
          }}
        >
          {dirty ? (
            <>
              <StatusDot color="var(--amber)" size={6} />
              Unsaved changes
            </>
          ) : (
            "All changes saved"
          )}
        </span>
      )}
      <Button variant="primary" icon={dirty ? undefined : Check} onClick={onSave} disabled={!dirty || saving || invalid}>
        {saving ? "Saving…" : "Save"}
      </Button>
    </div>
  );
}

type Draft = { global: GlobalConfigDTO; projects: ProjectConfigDTO[] };

// BackToRuns — the explicit "leave Settings" affordance. The titlebar gear toggles Settings ↔ Runs,
// but that's not discoverable on its own, so Settings always shows an obvious back link at the top.
function BackToRuns({ onBack }: { onBack: () => void }) {
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      onClick={onBack}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 6,
        alignSelf: "flex-start",
        border: "1px solid transparent",
        borderRadius: 7,
        cursor: "pointer",
        color: hover ? "var(--tx)" : "var(--tx-3)",
        background: hover ? "var(--bg-hover)" : "transparent",
        borderColor: hover ? "var(--line-strong)" : "transparent",
        fontSize: 13,
        fontWeight: 500,
        padding: "5px 11px 5px 8px",
        marginBottom: 16,
        marginLeft: -8,
        transition: "all .12s",
      }}
    >
      <ArrowLeft size={15} /> Back to Runs
    </button>
  );
}

export interface SettingsProps {
  tab: SettingsTabId;
  onTab: (tab: SettingsTabId) => void;
  /** Leave Settings and return to the Runs view (the titlebar gear also toggles this). */
  onBack: () => void;
}

// Settings — the Settings surface: a vertical rail of tabs and a titled panel with the dirty Save
// bar, plus the Add-agent sheet host. It owns the working config draft (deep-cloned from the
// daemon's typed view), the dirty flag, and the pending Linear token (kept out of config — written
// to the keychain on save). Nearly everything is a draft edit persisted atomically by the Save bar
// (General fields, the agent detail editor, the list enable/pause toggle, and remove). The lone
// immediate-persist action is creating an agent from the Add-agent sheet — an explicit "add + save".
export function Settings({ tab, onTab, onBack }: SettingsProps) {
  const cfg = useTypedConfigQuery();
  const identity = useLinearIdentity();
  const linearProjects = useLinearProjects();
  const statuses = useProjectStatuses();
  const save = useSaveTypedConfig();
  const qc = useQueryClient();
  const { toast } = useToast();

  const [draft, setDraft] = React.useState<Draft | null>(null);
  const [dirty, setDirty] = React.useState(false);
  const [token, setToken] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [sheetOpen, setSheetOpen] = React.useState(false);
  const [flushing, setFlushing] = React.useState(false);
  // The originally-loaded global, so a persist-artifacts off→on toggle can restore the real path.
  const baseGlobal = React.useRef<GlobalConfigDTO | null>(null);
  // Monotonic counter bumped synchronously on every draft/token edit. A save captures it at start
  // and only marks the form clean (+ toasts) if it's unchanged when the POST resolves — so an edit
  // racing the in-flight save is never silently discarded (a render-updated ref would be subject to
  // paint timing and could miss the race).
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
  // save (the edit sequence captured at `seqAtStart` is unchanged) — marks the form clean + toasts.
  // Staying dirty on a race keeps the resync effect (gated on !dirty) from clobbering the newer edit
  // and keeps the indicator/toast honest.
  const persist = (snapshot: Draft, title: string, seqAtStart: number): Promise<void> =>
    (async () => {
      const flushed = await flushPendingToken();
      // Clamp per-agent caps to the (possibly just-lowered) global max before POST.
      const projects = clampProjectCaps(snapshot.projects, snapshot.global.agent.max_concurrent_agents);
      await save.mutateAsync({ global: snapshot.global, projects });
      if (flushed) void qc.invalidateQueries({ queryKey: LINEAR_IDENTITY_QUERY_KEY });
      if (editSeq.current === seqAtStart) {
        setDirty(false);
        setToken("");
        toast(title, "Daemon reloaded configuration ✓");
      }
    })();

  // The list enable/pause toggle and remove are plain draft edits: they mark the form dirty and the
  // Save bar persists them with the rest of the config (one atomic POST). This keeps a single source
  // of truth — a casual toggle never silently writes unrelated pending edits, and nothing clears the
  // dirty flag without a Save.
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

  // Creating an agent is an explicit "add + save" action (per the Add-agent flow): it appends the
  // agent to the draft and persists immediately, toasting "Agent created". On failure the new agent
  // stays in the draft as a pending edit (surfaced via the error + Save bar) rather than being lost.
  const onCreate = (project: LinearProject, repo: string) => {
    // Same gate as the Save bar — never POST a config that fails validation (promote or unique slug).
    if (!draft || save.isPending || flushing || saveBlocked) return;
    const next: Draft = { global: draft.global, projects: [...draft.projects, newProjectConfig(project, repo)] };
    editSeq.current++; // the create is itself an edit
    setDraft(next);
    setDirty(true);
    setSheetOpen(false);
    setError(null);
    void persist(next, "Agent created", editSeq.current).catch((e: unknown) => {
      setError(e instanceof ConfigSaveError || e instanceof Error ? e.message : "Save failed");
    });
  };

  const doSave = () => {
    if (!draft || save.isPending || flushing || saveBlocked) return;
    setError(null);
    void persist(draft, "Settings saved", editSeq.current).catch((e: unknown) => {
      setError(e instanceof ConfigSaveError || e instanceof Error ? e.message : "Save failed");
    });
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
  // Block Save when the review-promote state would fail the daemon's validation — at the global
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

  // Tools and Logs are read-only, daemon-direct panels: they don't read the config draft,
  // so they render before the config load/skeleton guards (and hide the Save bar below).
  const readOnlyTab = tab === "tools" || tab === "logs";

  let body: React.ReactNode;
  if (tab === "tools") {
    body = <ToolsTab />;
  } else if (tab === "logs") {
    body = <LogsTab />;
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
      <BackToRuns onBack={onBack} />
      <div style={{ display: "grid", gridTemplateColumns: "208px minmax(0,1fr)", gap: 36, alignItems: "start" }}>
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
          style={{ position: "sticky", top: 0, display: "flex", flexDirection: "column", gap: 3 }}
        >
          <div
            style={{
              fontSize: 11,
              fontWeight: 600,
              letterSpacing: ".08em",
              textTransform: "uppercase",
              color: "var(--tx-faint)",
              padding: "4px 12px 8px",
            }}
          >
            Settings
          </div>
          {SETTINGS_NAV.map((s) => (
            <RailItem
              key={s.id}
              tabId={s.id}
              label={s.label}
              icon={s.icon}
              active={tab === s.id}
              onClick={() => onTab(s.id)}
              badge={s.id === "projects" ? projectCount : undefined}
            />
          ))}
        </div>
        <div
          id={SETTINGS_PANEL_ID}
          role="tabpanel"
          aria-labelledby={settingsRailTabId(tab)}
          tabIndex={0}
          style={{ display: "flex", flexDirection: "column", gap: 20, minWidth: 0, outline: "none" }}
        >
          <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 16 }}>
            <div>
              <h1 style={{ fontSize: 21, fontWeight: 600, letterSpacing: "-0.025em" }}>{meta.title}</h1>
              <p style={{ fontSize: 13, color: "var(--tx-3)", marginTop: 5, maxWidth: 560, lineHeight: 1.5 }}>
                {meta.desc}
              </p>
            </div>
            {!readOnlyTab ? (
              <SaveBar
                dirty={dirty}
                saving={save.isPending || flushing}
                error={error ?? blockMessage}
                invalid={saveBlocked}
                onSave={doSave}
              />
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
