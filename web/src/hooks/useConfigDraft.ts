import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  LINEAR_IDENTITY_QUERY_KEY,
  useLinearIdentity,
  useLinearProjects,
  useProjectStatuses,
  useSaveTypedConfig,
  useTypedConfigQuery,
} from "@/hooks/useConfig";
import { clearLinearToken, setLinearToken } from "@/lib/bindings";
import { ConfigSaveError } from "@/lib/api";
import type { GlobalConfigDTO, LinearIdentity, LinearProject, ProjectConfigDTO } from "@/lib/api";
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

// useConfigDraft — the WORKFLOW.md editing model: the working draft of the daemon's typed
// config, every edit that folds into it, and the debounced autosave that POSTs it.
//
// It was the body of `components/settings/Settings.tsx` (INF-226) until STUDIO-690 gave the new
// console its own Workflow editor (§8). Two editors of one file must not disagree about what a
// save does — the debounce, the validation gate that holds a POST the daemon would reject, the
// edit-sequence race guard, and the keychain token flush are subtle enough that a second copy
// would drift — so the logic lives here once and both surfaces render it. The Podium Settings
// rail and the console page contribute only their own chrome.
//
// Autosave debounce: coalesce rapid edits (stepper clicks, typing) into one POST after the user
// pauses. There is no Save button (mock 2a/2b) — edits persist on their own.
export const AUTOSAVE_DEBOUNCE_MS = 600;

export interface ConfigDraft {
  global: GlobalConfigDTO;
  projects: ProjectConfigDTO[];
}

export interface ConfigDraftModel {
  /** The working draft; null until the daemon's typed config has loaded. */
  draft: ConfigDraft | null;
  /** The draft's global defaults in UI shape; null while `draft` is. */
  uiGlobal: UiGlobal | null;
  /** The draft's per-agent rows, resolved against the global defaults and live status. */
  agents: UiAgent[];
  /** Configured agents — from the draft once loaded, else the server's count. */
  projectCount: number;
  /** The daemon could not serve a typed config (unreachable, or WORKFLOW.md unparseable). */
  unavailable: boolean;
  dirty: boolean;
  saving: boolean;
  /** The last save failure, verbatim from the daemon. */
  error: string | null;
  /** Why the autosave is held back (the daemon would reject the POST), or null. */
  blocked: string | null;
  /** The workspace's Linear projects (Add-agent picker + per-agent colour). */
  linearProjects: LinearProject[];
  /** The connected-as Linear account, or null while loading / unauthenticated. */
  account: LinearIdentity | null;
  /** The pending API token — kept out of the config, written to the keychain on save. */
  token: string;
  onGlobalChange: (ui: UiGlobal) => void;
  onTokenChange: (token: string) => void;
  onAgentChange: (index: number, ui: UiAgent) => void;
  onToggleAgent: (index: number, enabled: boolean) => void;
  onRemoveAgent: (index: number) => void;
  onCreateAgent: (project: LinearProject, repo: string) => void;
  onDisconnect: () => void;
}

export function useConfigDraft(): ConfigDraftModel {
  const cfg = useTypedConfigQuery();
  const identity = useLinearIdentity();
  const linearProjects = useLinearProjects();
  const statuses = useProjectStatuses();
  const save = useSaveTypedConfig();
  const qc = useQueryClient();

  const [draft, setDraft] = React.useState<ConfigDraft | null>(null);
  const [dirty, setDirty] = React.useState(false);
  const [token, setToken] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
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
  const persist = (snapshot: ConfigDraft, seqAtStart: number): Promise<void> =>
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

  // Creating an agent appends it to the draft as a dirty edit. On a save failure the new agent
  // stays in the draft as a pending edit (surfaced via the error indicator) rather than being lost.
  const onCreateAgent = (project: LinearProject, repo: string) => {
    if (!draft) return;
    editSeq.current++;
    setDraft({ global: draft.global, projects: [...draft.projects, newProjectConfig(project, repo)] });
    setDirty(true);
    setError(null);
  };

  const onDisconnect = () => {
    void clearLinearToken()
      .then(() => qc.invalidateQueries({ queryKey: LINEAR_IDENTITY_QUERY_KEY }))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : "Disconnect failed"));
  };

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
  const blocked = slugConflict
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

  return {
    draft,
    uiGlobal,
    agents,
    projectCount,
    unavailable: cfg.isError || (!!cfg.data && !cfg.data.global),
    dirty,
    saving,
    error,
    blocked,
    linearProjects: linearProjects.data ?? [],
    account: identity.data ?? null,
    token,
    onGlobalChange,
    onTokenChange,
    onAgentChange,
    onToggleAgent,
    onRemoveAgent,
    onCreateAgent,
    onDisconnect,
  };
}
