// Mapping layer between the daemon's typed config view (api.ts DTOs) and the design's UI model
// (the camelCase shapes from the Claude Design `data.jsx`). Kept in a pure .ts module (no DOM)
// so it is unit-testable in this repo's node-environment Vitest setup. The Settings components
// edit the UI model; this module converts to/from the daemon shape at the save boundary.
//
// Inherit/override is presence-based: `overrides` only carries the keys an agent diverges on
// (model/effort/permission). Reset deletes the key (back to inherit); Override seeds the global.

import type {
  ClaudeOverridesDTO,
  GlobalConfigDTO,
  LinearProject,
  ProjectConfigDTO,
  ProjectStatus,
} from "@/lib/api";
import { repoShortName } from "@/lib/project";
import { GLOBAL_DEFAULTS } from "@/lib/settings-data";

// REPO_PROMPT_PATH is the canonical repo-relative prompt path the "use this repo's prompt" checkbox
// writes into prompt_file (repo-level prompt feature, INF-279). Mirror of config.DefaultRepoPromptFile
// in the daemon — reference this constant instead of the literal.
export const REPO_PROMPT_PATH = ".symphony/PROMPT.md";

// UiOverrides is the sparse per-agent override map (presence = override, absence = inherit).
export interface UiOverrides {
  model?: string;
  effort?: string;
  permission?: string;
  ultracode?: boolean;
  // Per-agent timeouts are edited in MINUTES (the daemon stores turn_timeout_ms / stall_timeout_ms);
  // billingGuard and command map 1:1 to the daemon's per-project claude knobs.
  turnTimeoutMin?: number;
  stallTimeoutMin?: number;
  billingGuard?: boolean;
  command?: string;
  // git_flow override ("any" | "graphite"); presence = override, absence = inherit the global.
  gitFlow?: string;
  // workspace_mode override ("worktree" | "clone"); presence = override, absence = inherit the
  // global. (INF-418)
  workspaceMode?: string;
  // dependency_mode override ("disabled" | "graphite" | "dag"); presence = override, absence = inherit
  // the global. (INF-318/INF-320)
  dependencyMode?: string;
  // claim_mode override ("assignee" | "pool"); presence = override, absence = inherit the global.
  // (INF-477)
  claimMode?: string;
}

// UiGlobal is the General tab's editable model. The Linear API token is intentionally absent —
// it is keychain-only (Go binding) and never round-trips through config.
export interface UiGlobal {
  model: string;
  effort: string;
  permission: string;
  ultracode: boolean;
  maxConcurrent: number;
  maxTurns: number;
  backoff: string; // fixed | linear | exponential (UI strategy; see backoffToMs)
  billingGuard: boolean;
  command: string;
  // Turn/stall timeouts are edited in MINUTES (the daemon stores ms). stallTimeoutMin + command have
  // no General-tab control yet; they are surfaced so a per-agent override can show the inherited value.
  requestTimeoutMin: number;
  stallTimeoutMin: number;
  extraArgs: string;
  workspaceRoot: string;
  historyRetentionDays: number;
  persistArtifacts: boolean;
  dashboardPort: number;
  pollIntervalSec: number;
  /** OTel export on/off (otel.enabled). Independent of the endpoint so a user can turn export
   *  off without losing the seeded hub endpoint, and back on without re-typing it. */
  telemetryEnabled: boolean;
  telemetryEndpoint: string;
  logsPath: string;
  /** The inline prompt body (the global default every agent inherits). */
  prompt: string;
  /** The global prompt-source-file path. Non-empty => the prompt is read from this file per-run
   *  (a relative path is repo-relative; an absolute / ~ path is local to the daemon host). */
  promptFile: string;
  /** Global git-workflow policy ("any" | "graphite"); the default every agent inherits. */
  gitFlow: string;
  /** Global workspace-provisioning policy ("worktree" | "clone"); the default every agent inherits.
   *  Seed "worktree" (today's shared-mirror behavior; clone is opt-in). (INF-418) */
  workspaceMode: string;
  /** Global required-labels default (tracker.labels). No General-tab control yet; surfaced read-only
   *  so the per-agent "Required labels" editor can show the inherited value when its own list is
   *  empty. Preserved verbatim on save via applyUiGlobal's `...g` base spread. */
  labels: string[];
  /** Global dependency-sequencing mode ("disabled" | "graphite" | "dag"); the default every agent
   *  inherits. Seed "disabled" (opt-in orchestration; not git_flow-derived). (INF-318/INF-320) */
  dependencyMode: string;
  /** Global ticket-claim policy ("assignee" | "pool"); the default every agent inherits. Seed
   *  "assignee" (today's assignee-locked fetch; pool sharing is opt-in). (INF-477) */
  claimMode: string;
  /** tracker.github_summons: re-engage an In-Review ticket from an @symphony comment on its
   *  unmerged linked GitHub PR. Tracker-global; default false (opt-in). (AIE-299/AIE-302) */
  githubSummons: boolean;
  /** `symphony mcp` local-facade toggles (INF-473). mcpEnabled injects a `symphony` MCP server
   *  into DISPATCHED agents' config (default on — the opt-out); mcpAllowSendMessage is the one
   *  default-on write tool; mcpAllowStop/mcpAllowResume are opt-in (default off). Read tools are
   *  always on and have no toggle. */
  mcpEnabled: boolean;
  mcpAllowSendMessage: boolean;
  mcpAllowStop: boolean;
  mcpAllowResume: boolean;
}

// UiAgent is the design's agent shape (one entry per Linear project the agent watches).
export interface UiAgent {
  id: string;
  name: string;
  color: string;
  projectSlug: string;
  /** The watched Linear project's display name (distinct from the agent's own `name`). */
  projectName: string;
  repo: string;
  repoShort: string;
  milestone: string;
  labels: string[];
  enabled: boolean;
  status: string;
  running: number;
  activeStates: string[];
  terminalStates: string[];
  reviewStates: string[];
  reviewPromote: string;
  cap: number;
  prompt: string;
  /** Per-agent prompt-source-file override. Empty => inherit the global `promptFile`. */
  promptFile: string;
  overrides: UiOverrides;
  /** Display-only: the daemon recommends clone for this stacking project (effective dependency_mode
   *  graphite/dag + unset workspace_mode). Drives a non-binding UI nudge; never persisted. (INF-418) */
  workspaceModeRecommended: boolean;
}

const DEFAULT_COLOR = "#34d399";

// A freshly created agent runs one ticket at a time by default (the design's Add-agent sheet
// advertises this as the inherited per-agent cap). Conservative + bounded by the global max.
export const NEW_AGENT_CAP = 1;

// projShort strips a Linear slug's trailing hex id for a compact display fallback when the
// project isn't in the fetched Linear list (matches the design's `projShort`).
export function projShort(slug: string): string {
  return slug.replace(/-[0-9a-f]{8,}$/, "");
}

// The daemon models retry behaviour as a single max-delay-in-ms knob (`max_retry_backoff_ms`).
// The design exposes the three strategies the team thinks in; map each onto a representative
// delay and back. Default daemon delay (300000) round-trips to "exponential", matching the
// design's GLOBAL_DEFAULTS.backoff.
export function backoffToMs(backoff: string): number {
  switch (backoff) {
    case "fixed":
      return 30000;
    case "linear":
      return 120000;
    default:
      return 300000;
  }
}

export function msToBackoff(ms: number): string {
  if (ms <= 30000) return "fixed";
  if (ms <= 120000) return "linear";
  return "exponential";
}

// effort/model/permission may arrive empty or as a value outside the design's option list (the
// daemon accepts a wider set). Coalesce empties to the design default so the Select always has a
// concrete value; pass through any other configured value verbatim.
const coalesce = (v: string, fallback: string): string => (v && v.trim() !== "" ? v : fallback);

export function toUiGlobal(g: GlobalConfigDTO): UiGlobal {
  return {
    model: coalesce(g.claude.model, GLOBAL_DEFAULTS.model),
    effort: coalesce(g.claude.effort, GLOBAL_DEFAULTS.effort),
    permission: coalesce(g.claude.permission_mode, GLOBAL_DEFAULTS.permission),
    ultracode: g.claude.ultracode,
    maxConcurrent: g.agent.max_concurrent_agents,
    maxTurns: g.agent.max_turns,
    backoff: msToBackoff(g.agent.max_retry_backoff_ms),
    billingGuard: g.claude.billing_guard,
    command: g.claude.command,
    // Floor to 1 minute: a sub-minute configured cap (e.g. 20000ms) would otherwise round to 0,
    // which an unrelated General-tab save writes back as turn_timeout_ms:0 — and the daemon floors
    // <=0 to 1 HOUR (runner.go), silently corrupting a deliberate short cap. Round sub-minute configs
    // UP to the 1-min UI floor instead.
    requestTimeoutMin: Math.max(1, Math.round(g.claude.turn_timeout_ms / 60000)),
    // Stall preserves exact 0 (= disabled) and ceilings nonzero sub-minute to 1 (see stallMsToMin):
    // 0 must stay disabled, and Math.round would flip a real sub-minute stall into 0 (= disabled).
    stallTimeoutMin: stallMsToMin(g.claude.stall_timeout_ms) ?? 0,
    extraArgs: (g.claude.extra_args ?? []).join(" "),
    workspaceRoot: g.workspace.root,
    historyRetentionDays: g.storage.retention_days ?? 30,
    persistArtifacts: g.storage.path !== "off",
    // server.port is *int (null = use the daemon's default dashboard port, 8799). Fall back to
    // that default rather than a stray value so saving an unchanged port re-asserts the default.
    dashboardPort: g.server.port ?? 8799,
    pollIntervalSec: Math.max(1, Math.round(g.polling.interval_ms / 1000)),
    telemetryEnabled: g.otel.enabled,
    telemetryEndpoint: g.otel.endpoint,
    logsPath: g.logging.dir,
    prompt: g.prompt,
    promptFile: g.prompt_file,
    gitFlow: coalesce(g.git_flow, GLOBAL_DEFAULTS.gitFlow),
    workspaceMode: coalesce(g.workspace_mode, GLOBAL_DEFAULTS.workspaceMode),
    labels: g.labels ?? [],
    // A blank/unset dependency_mode coalesces to the flat "disabled" seed — never git_flow-derived.
    dependencyMode: coalesce(g.dependency_mode ?? "", GLOBAL_DEFAULTS.dependencyMode),
    // A blank/unset claim_mode coalesces to the "assignee" seed (today's assignee-locked fetch).
    claimMode: coalesce(g.claim_mode ?? "", GLOBAL_DEFAULTS.claimMode),
    // github_summons is a plain bool in the daemon (absent => false); read it straight through.
    githubSummons: Boolean(g.github_summons),
    // mcp.* are resolved bools in the daemon DTO (always present, INF-473); read straight through.
    mcpEnabled: g.mcp.enabled,
    mcpAllowSendMessage: g.mcp.allow_send_message,
    mcpAllowStop: g.mcp.allow_stop,
    mcpAllowResume: g.mcp.allow_resume,
  };
}

// applyUiGlobal overlays the General tab's edits onto the daemon global, preserving every
// untouched field (secrets, summon token, prompt, otel transport, …). `base` is the originally
// loaded global; it lets a persist-artifacts off→on toggle restore the real storage path rather
// than losing it.
export function applyUiGlobal(
  g: GlobalConfigDTO,
  ui: UiGlobal,
  base?: GlobalConfigDTO,
): GlobalConfigDTO {
  const persistPath = ui.persistArtifacts
    ? g.storage.path !== "off"
      ? g.storage.path
      : base && base.storage.path !== "off"
        ? base.storage.path
        : ":memory:"
    : "off";
  const endpoint = ui.telemetryEndpoint.trim();
  return {
    ...g,
    prompt: ui.prompt,
    // A relative prompt_file is repo-relative; an absolute / ~ path is local to the daemon host. We
    // round-trip the trimmed value verbatim (the daemon decides where to read it at run time).
    prompt_file: ui.promptFile.trim(),
    git_flow: ui.gitFlow,
    workspace_mode: ui.workspaceMode,
    dependency_mode: ui.dependencyMode,
    claim_mode: ui.claimMode,
    // Tracker-global toggle; write explicitly (the `...g` spread carries the loaded value, not the
    // user's edit). The daemon prunes an absent/false key, so saving false is a clean no-op on disk.
    github_summons: ui.githubSummons,
    polling: { ...g.polling, interval_ms: ui.pollIntervalSec * 1000 },
    agent: {
      ...g.agent,
      max_concurrent_agents: ui.maxConcurrent,
      max_turns: ui.maxTurns,
      max_retry_backoff_ms: backoffToMs(ui.backoff),
    },
    claude: {
      // command + stall_timeout_ms have no General-tab control; they are preserved verbatim from
      // `...g.claude` (read-only in the UI — surfaced only as the per-agent inherited default).
      ...g.claude,
      model: ui.model,
      effort: ui.effort,
      permission_mode: ui.permission,
      ultracode: ui.ultracode,
      billing_guard: ui.billingGuard,
      turn_timeout_ms: ui.requestTimeoutMin * 60000,
      extra_args: ui.extraArgs.split(/\s+/).filter((s) => s.length > 0),
    },
    workspace: { ...g.workspace, root: ui.workspaceRoot },
    storage: { ...g.storage, path: persistPath, retention_days: ui.historyRetentionDays },
    // enabled is an explicit toggle (NOT derived from endpoint-presence): the onboarding seed ships
    // a default-on hub endpoint, and a user opts out via the toggle while keeping the endpoint, so
    // re-enabling needs no re-typing (INF-299). protocol/service_name/insecure/headers ride through
    // ...g.otel untouched (no General-tab control).
    otel: { ...g.otel, endpoint, enabled: ui.telemetryEnabled },
    // All four MCP toggles are General-tab controlled; write them straight through (INF-473).
    mcp: {
      enabled: ui.mcpEnabled,
      allow_send_message: ui.mcpAllowSendMessage,
      allow_stop: ui.mcpAllowStop,
      allow_resume: ui.mcpAllowResume,
    },
    server: { ...g.server, port: ui.dashboardPort },
    logging: { ...g.logging, dir: ui.logsPath },
  };
}

// effectiveModel resolves an agent's model: its override if present, else the global default
// (matching the design's `effModel = a.overrides.model || G.model`). Callers strip the "claude-"
// prefix for compact display.
export function effectiveModel(agent: UiAgent, global: UiGlobal): string {
  return agent.overrides.model || global.model;
}

// duplicateSlugs is true when any Linear project slug is configured on more than one agent. The
// daemon rejects configs with duplicate slugs (each must be globally unique), so the UI blocks
// Save/Create rather than letting the POST fail after the selection was accepted.
export function duplicateSlugs(projects: ProjectConfigDTO[]): boolean {
  const seen = new Set<string>();
  for (const p of projects) {
    for (const slug of p.slugs) {
      if (seen.has(slug)) return true;
      seen.add(slug);
    }
  }
  return false;
}

// clampProjectCaps bounds each project's per-agent `max_concurrent_agents` to the global max.
// Applied at the save boundary so lowering the global limit immediately tightens any per-agent cap
// that exceeded it — otherwise the daemon rejects the POST (per-project cap > global), and the UI
// already displays the clamped value (see toUiAgent).
export function clampProjectCaps(projects: ProjectConfigDTO[], globalMax: number): ProjectConfigDTO[] {
  return projects.map((p) =>
    p.max_concurrent_agents != null && p.max_concurrent_agents > globalMax
      ? { ...p, max_concurrent_agents: globalMax }
      : p,
  );
}

// normalizeState mirrors the daemon's core.NormalizeState (trim + lowercase) so the UI's promote
// validation matches the backend's case-insensitive comparison — otherwise a config the daemon
// accepts (e.g. promote "in progress" vs active "In Progress") would be falsely flagged.
const normalizeState = (s: string) => s.trim().toLowerCase();

function promoteInActive(promote: string, active: string[]): boolean {
  const n = normalizeState(promote);
  return active.some((s) => normalizeState(s) === n);
}

// reviewPromoteValid mirrors the daemon's per-project rule: ONLY when an agent has review enabled
// (non-empty review states) must its promote state be one of its active states. With review off the
// daemon skips the check, so the UI must not block it either. An empty promote is vacuously valid.
// agt_docs (review on, promote "Shipped" ∉ active) exercises the failing case.
export function reviewPromoteValid(a: {
  activeStates: string[];
  reviewStates: string[];
  reviewPromote: string;
}): boolean {
  if (a.reviewStates.length === 0) return true;
  return !a.reviewPromote || promoteInActive(a.reviewPromote, a.activeStates);
}

// globalPromoteValid mirrors the daemon's top-level review-promote rule: when the global review
// states are non-empty, the global review_promote_state must be one of the global active states.
// (A project can widen its own active states, so a per-agent pass doesn't imply the global scope.)
export function globalPromoteValid(g: GlobalConfigDTO): boolean {
  // The daemon returns review_states: null (not []) when no review states are configured, so treat
  // null/empty alike as "review off" — never read .length off a possibly-null list (blank-screen bug).
  if (!g.review_states || g.review_states.length === 0) return true;
  return !g.review_promote_state || promoteInActive(g.review_promote_state, g.active_states);
}

// turnMsToMin converts a nullable per-agent turn_timeout_ms to whole minutes, or undefined when the
// knob is absent (inherit). A present value floors to 1 minute (mirrors toUiGlobal's requestTimeoutMin):
// a sub-minute override would otherwise round to 0 and be written back as turn_timeout_ms:0, which the
// daemon floors <=0 to 1 HOUR (runner.go) — silently corrupting a deliberate short cap. null/undefined
// stays undefined (inherit the global).
const turnMsToMin = (ms: number | null | undefined): number | undefined =>
  ms == null ? undefined : Math.max(1, Math.round(ms / 60000));

// stallMsToMin is the stall-specific analogue. The stall knob has DIFFERENT zero-semantics from turn:
// the daemon treats stall_timeout_ms <= 0 as "stall detection disabled" (reconcile_run.go skips the
// entry when `stall <= 0`), and the stall Stepper is min=0 (0 = disabled) — so a plain 1-min floor
// would corrupt a deliberately-disabled (0) override into a 1-minute stall kill. We therefore PRESERVE
// exact 0 (disabled), and ceiling any nonzero sub-minute value up to the 1-min UI floor. Ceiling
// (not Math.round) is required: Math.round(20000/60000)=0 would flip a real 20s stall into "disabled"
// — lossy the other way. The UI edits whole minutes, so a sub-minute stall can't round-trip exactly
// regardless; mapping nonzero sub-minute -> 1 min is the most conservative choice that never disables.
// null/undefined stays undefined (inherit the global).
const stallMsToMin = (ms: number | null | undefined): number | undefined =>
  ms == null ? undefined : ms === 0 ? 0 : Math.max(1, Math.round(ms / 60000));

// sparseOverrides prunes a UI override map to its PRESENT keys (presence = override). Used when
// reading a project into the UI so an absent/undefined knob never materializes as a key.
function sparseOverrides(o: UiOverrides): UiOverrides {
  const out: UiOverrides = {};
  if (o.model !== undefined) out.model = o.model;
  if (o.effort !== undefined) out.effort = o.effort;
  if (o.permission !== undefined) out.permission = o.permission;
  if (o.ultracode !== undefined) out.ultracode = o.ultracode;
  if (o.turnTimeoutMin !== undefined) out.turnTimeoutMin = o.turnTimeoutMin;
  if (o.stallTimeoutMin !== undefined) out.stallTimeoutMin = o.stallTimeoutMin;
  if (o.billingGuard !== undefined) out.billingGuard = o.billingGuard;
  if (o.command !== undefined) out.command = o.command;
  if (o.gitFlow !== undefined) out.gitFlow = o.gitFlow;
  if (o.workspaceMode !== undefined) out.workspaceMode = o.workspaceMode;
  if (o.dependencyMode !== undefined) out.dependencyMode = o.dependencyMode;
  if (o.claimMode !== undefined) out.claimMode = o.claimMode;
  return out;
}

// overridesToDTO is the save-boundary inverse of toUiAgent's override mapping: it converts the sparse
// UI override map back to the daemon's ClaudeOverridesDTO, emitting ONLY the keys the agent overrides
// (an absent key inherits the global). The two timeouts convert minutes -> ms.
function overridesToDTO(o: UiOverrides): ClaudeOverridesDTO {
  const out: ClaudeOverridesDTO = {};
  if (o.model !== undefined) out.model = o.model;
  if (o.effort !== undefined) out.effort = o.effort;
  if (o.permission !== undefined) out.permission = o.permission;
  if (o.ultracode !== undefined) out.ultracode = o.ultracode;
  if (o.turnTimeoutMin !== undefined) out.turn_timeout_ms = o.turnTimeoutMin * 60000;
  if (o.stallTimeoutMin !== undefined) out.stall_timeout_ms = o.stallTimeoutMin * 60000;
  if (o.billingGuard !== undefined) out.billing_guard = o.billingGuard;
  // A blank command inherits the global binary (mirrors strOverride's blank-as-inherit): clearing
  // the field must not POST command:"" and clobber the inherited default.
  if (o.command !== undefined && o.command.trim() !== "") out.command = o.command;
  // A blank git_flow inherits the global (mirrors command's blank-as-inherit).
  if (o.gitFlow !== undefined && o.gitFlow.trim() !== "") out.git_flow = o.gitFlow;
  // A blank workspace_mode inherits the global (mirrors git_flow's blank-as-inherit).
  if (o.workspaceMode !== undefined && o.workspaceMode.trim() !== "") out.workspace_mode = o.workspaceMode;
  // A blank dependency_mode inherits the global (mirrors git_flow's blank-as-inherit).
  if (o.dependencyMode !== undefined && o.dependencyMode.trim() !== "") out.dependency_mode = o.dependencyMode;
  // A blank claim_mode inherits the global (mirrors git_flow's blank-as-inherit).
  if (o.claimMode !== undefined && o.claimMode.trim() !== "") out.claim_mode = o.claimMode;
  return out;
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

// effectiveList resolves a per-project state list against the global default for DISPLAY: a
// non-empty project list wins; an empty/absent one inherits the global, matching the daemon (which
// treats an empty per-project list as "inherit global", not "none").
function effectiveList(
  projList: string[] | null | undefined,
  globalList: string[] | null | undefined,
): string[] {
  if (projList && projList.length > 0) return projList;
  return globalList ?? [];
}

// effectiveStr is the single-string analogue (repo / milestone): a non-empty project value wins,
// else inherit the global.
function effectiveStr(projVal: string | undefined, globalVal: string): string {
  return projVal && projVal.trim() !== "" ? projVal : globalVal;
}

// listOverride is the inverse, applied at the SAVE boundary: a per-project list is written only when
// it is non-empty AND diverges from the global. Empty or equal-to-global → undefined (inherit), so
// an unrelated edit can't pin an inherited list as a per-project override and freeze it against
// future global changes.
function listOverride(uiList: string[], globalList: string[] | null | undefined): string[] | undefined {
  if (uiList.length === 0 || sameList(uiList, globalList ?? [])) return undefined;
  return uiList;
}

// strOverride is the single-string analogue of listOverride (repo / milestone): write the value as
// a per-project override only when it is non-empty AND diverges from the global — empty or
// equal-to-global → undefined (inherit), so an unrelated edit can't pin the inherited global value.
function strOverride(uiVal: string, globalVal: string): string | undefined {
  return uiVal.trim() === "" || uiVal === globalVal ? undefined : uiVal;
}

function deriveStatus(enabled: boolean, st?: ProjectStatus): { status: string; running: number } {
  if (!enabled) return { status: "paused", running: 0 };
  const running = st?.running ?? 0;
  if (running > 0) return { status: "running", running };
  const s = st?.status ?? "";
  // "active"/"running"/"" with no live runs read as idle; pass through review/handoff/etc.
  if (s === "" || s === "active" || s === "running") return { status: "idle", running: 0 };
  return { status: s, running: 0 };
}

export function toUiAgent(
  p: ProjectConfigDTO,
  g: GlobalConfigDTO,
  linearBySlug: Map<string, LinearProject>,
  statusBySlug: Map<string, ProjectStatus>,
): UiAgent {
  const slug = p.slugs[0] ?? "";
  const lin = linearBySlug.get(slug);
  const enabled = p.enabled ?? true;
  const { status, running } = deriveStatus(enabled, statusBySlug.get(slug));
  // Resolve every effective value from the DRAFT (project overlay + the possibly-edited global),
  // not the daemon's `effective` snapshot — that snapshot is only accurate at load and goes stale
  // once the draft changes (e.g. the global max), which would desync display + validation from the
  // config that actually gets POSTed.
  const repo = effectiveStr(p.repo, g.repo);
  const overrides = sparseOverrides({
    model: p.overrides.model ?? undefined,
    effort: p.overrides.effort ?? undefined,
    permission: p.overrides.permission ?? undefined,
    ultracode: p.overrides.ultracode ?? undefined,
    turnTimeoutMin: turnMsToMin(p.overrides.turn_timeout_ms),
    stallTimeoutMin: stallMsToMin(p.overrides.stall_timeout_ms),
    billingGuard: p.overrides.billing_guard ?? undefined,
    command: p.overrides.command ?? undefined,
    gitFlow: p.overrides.git_flow ?? undefined,
    workspaceMode: p.overrides.workspace_mode ?? undefined,
    dependencyMode: p.overrides.dependency_mode ?? undefined,
    claimMode: p.overrides.claim_mode ?? undefined,
  });
  const cap = p.max_concurrent_agents ?? g.agent.max_concurrent_agents;
  return {
    id: slug || p.name,
    name: p.name,
    color: lin?.color ?? DEFAULT_COLOR,
    projectSlug: slug,
    projectName: lin?.name ?? projShort(slug),
    repo,
    repoShort: repoShortName(repo),
    milestone: effectiveStr(p.milestone, g.milestone),
    // Labels bind to the project's RAW override (NOT the effective merge): the editor's chips must be
    // the per-project list so removing the last chip empties the field instead of re-displaying the
    // inherited global default. The inherited value is surfaced via UiGlobal.labels for the hint.
    labels: p.labels ?? [],
    enabled,
    status,
    running,
    // Each state list shows the effective value: the project's own list, else (when empty/absent)
    // the inherited global list.
    activeStates: effectiveList(p.active_states, g.active_states),
    terminalStates: effectiveList(p.terminal_states, g.terminal_states),
    reviewStates: effectiveList(p.review_states, g.review_states),
    // review_promote_state is GLOBAL in the daemon (no per-project override), so always read it
    // from the (possibly edited) global — never from a project's stale `effective` snapshot, which
    // would otherwise revert another agent's promote edit when this agent is opened.
    reviewPromote: g.review_promote_state,
    cap: Math.min(Math.max(1, cap), g.agent.max_concurrent_agents),
    prompt: p.prompt ?? "",
    // The raw override (empty => inherit the global prompt_file), mirroring `prompt`: the editor
    // shows the inherited global path as a placeholder, so an empty value stays inherited.
    promptFile: p.prompt_file ?? "",
    overrides,
    // Display-only recommendation from the daemon (emit-only DTO field): clone is suggested for this
    // stacking project when workspace_mode is unset. Surfaced as a non-binding UI nudge (INF-418).
    workspaceModeRecommended: p.workspace_mode_recommended ?? false,
  };
}

export function toUiAgents(
  projects: ProjectConfigDTO[],
  g: GlobalConfigDTO,
  linear: LinearProject[],
  statuses: ProjectStatus[],
): UiAgent[] {
  const linearBySlug = new Map(linear.map((l) => [l.slug, l]));
  const statusBySlug = new Map(statuses.map((s) => [s.slug, s]));
  return projects.map((p) => toUiAgent(p, g, linearBySlug, statusBySlug));
}

// applyUiAgent overlays the detail editor's edits onto the original project DTO. It collapses to
// the selected slug (preserving any fan-out slugs when the project is unchanged), writes each state
// list / cap as a per-project override ONLY when it diverges from the global (else inherits, so an
// unrelated edit never pins an inherited value), and rebuilds the sparse Claude override map.
// review_promote is global in the daemon, so the controller writes it to `global` separately.
export function applyUiAgent(
  orig: ProjectConfigDTO,
  ui: UiAgent,
  g: GlobalConfigDTO,
): ProjectConfigDTO {
  const slugs = orig.slugs[0] === ui.projectSlug ? orig.slugs : [ui.projectSlug];
  const cap = Math.max(1, Math.min(ui.cap, g.agent.max_concurrent_agents));
  return {
    ...orig,
    name: ui.name,
    slugs,
    repo: strOverride(ui.repo, g.repo),
    milestone: strOverride(ui.milestone, g.milestone),
    labels: listOverride(ui.labels, g.labels),
    enabled: ui.enabled,
    active_states: listOverride(ui.activeStates, g.active_states),
    terminal_states: listOverride(ui.terminalStates, g.terminal_states),
    review_states: listOverride(ui.reviewStates, g.review_states),
    // A per-agent cap equal to (or above) the global max means "use the global" → inherit (null).
    max_concurrent_agents: cap >= g.agent.max_concurrent_agents ? undefined : cap,
    prompt: ui.prompt,
    // Write the per-agent prompt_file only when set; an empty value clears the override (inherit
    // the global). The trim keeps a whitespace-only path from pinning an empty override.
    prompt_file: ui.promptFile.trim() === "" ? undefined : ui.promptFile.trim(),
    overrides: overridesToDTO(ui.overrides),
    // `effective` is display-only and dropped before POST.
    effective: undefined,
  };
}

// ProjectSelectOption is one entry of the agent's Linear-project picker. `note` is the slugId
// subtext (or, for an unmatched saved slug, the "not found in Linear" hint).
export interface ProjectSelectOption {
  value: string;
  label: string;
  note: string;
}

// projectSelectOptions builds the options for an agent's Linear-project <Select> and reports
// whether the agent's saved slug matches no known project (INF-277). A pre-INF-277 config can hold
// a free-text value that was never a real slugId; the daemon's <Select> matched options by exact
// slugId, so an unmatched value rendered the bare placeholder and the project looked EMPTY even
// though the YAML held the user's string. To avoid that, when the saved slug matches nothing we
// append a synthetic option (label = the raw slug, note = "not found in Linear") so the trigger
// shows the actual saved value; `unmatched` lets the caller flag the field invalid + show a hint.
// An empty saved slug is not "unmatched" (it's simply unset → placeholder).
export function projectSelectOptions(
  projects: LinearProject[],
  savedSlug: string,
): { options: ProjectSelectOption[]; unmatched: boolean } {
  const options: ProjectSelectOption[] = projects.map((p) => ({
    value: p.slug,
    label: p.name,
    note: p.slug,
  }));
  const slug = savedSlug.trim();
  const unmatched = slug !== "" && !projects.some((p) => p.slug === slug);
  if (unmatched) {
    options.push({ value: slug, label: slug, note: "not found in Linear" });
  }
  return { options, unmatched };
}

// newProjectConfig builds a fresh agent entry for the Add-agent sheet: it watches the chosen
// Linear project, points at the given repo, and inherits everything else (empty overrides,
// null/empty per-agent knobs) from the global defaults. New agents DEFAULT to the repo's own prompt
// (prompt_file: .symphony/PROMPT.md) — safe because a missing relative file soft-falls-back to the
// inline prompt (INF-279).
export function newProjectConfig(project: LinearProject, repo: string): ProjectConfigDTO {
  return {
    name: project.name,
    slugs: [project.slug],
    repo: repo.trim(),
    enabled: true,
    max_concurrent_agents: NEW_AGENT_CAP,
    prompt_file: REPO_PROMPT_PATH,
    overrides: {},
  };
}
