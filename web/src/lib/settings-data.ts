// Settings UI option lists + global-default fallbacks, ported verbatim from the Claude Design
// package's `data.jsx`. These are the canonical Select options for the General tab and the
// per-agent Claude overrides (the design is the detail-for-detail source of truth). The live
// values come from the daemon config API; these constants supply the dropdown choices and the
// seed used before config loads.

import type { SelectOption } from "@/components/ui/select";

// GLOBAL_DEFAULTS — the design's seed for the General tab before the daemon config loads, and
// the fallback when a global field is unset. `backoff` is a UI retry strategy (see settings-model
// for the mapping onto the daemon's `max_retry_backoff_ms`).
export const GLOBAL_DEFAULTS = {
  model: "claude-opus-5",
  effort: "high",
  permission: "acceptEdits",
  maxConcurrent: 3,
  maxTurns: 60,
  backoff: "exponential",
  gitFlow: "any",
  workspaceMode: "worktree",
  // dependency_mode default seed. A flat "disabled" (orchestration is opt-in); NOT derived from
  // git_flow. Only the pre-config-load fallback — once the daemon config loads, toUiGlobal reads the
  // resolved value verbatim (ticket #1 returns a concrete "disabled"). (INF-320)
  dependencyMode: "disabled",
  // claim_mode default seed. "assignee" (today's assignee-locked fetch; pool sharing is opt-in).
  // Pre-config-load fallback only — toUiGlobal reads the resolved value verbatim once loaded. (INF-477)
  claimMode: "assignee",
} as const;

export const MODELS: SelectOption[] = [
  { value: "claude-fable-5", label: "claude-fable-5", note: "Most capable" },
  { value: "claude-opus-5", label: "claude-opus-5", note: "Default — flagship" },
  { value: "claude-opus-4-8", label: "claude-opus-4-8", note: "Previous flagship" },
  { value: "claude-sonnet-5", label: "claude-sonnet-5", note: "Balanced" },
  { value: "claude-haiku-4-5", label: "claude-haiku-4-5", note: "Fast & cheap" },
];

export const EFFORTS: SelectOption[] = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "xhigh", label: "Extra high" },
  { value: "max", label: "Max" },
];

// EFFORT_LABEL maps an effort value to its display label (used for the override "global default"
// preview, which shows the human label rather than the raw value).
export const EFFORT_LABEL: Record<string, string> = {
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Extra high",
  max: "Max",
};

export const PERMISSIONS: SelectOption[] = [
  { value: "default", label: "default", note: "Prompt for each tool" },
  { value: "acceptEdits", label: "acceptEdits", note: "Auto-accept file edits" },
  { value: "plan", label: "plan", note: "Plan only, no writes" },
  { value: "bypassPermissions", label: "bypassPermissions", note: "Run unattended" },
];

export const BACKOFFS: SelectOption[] = [
  { value: "fixed", label: "Fixed (30s)" },
  { value: "linear", label: "Linear" },
  { value: "exponential", label: "Exponential" },
];

// GIT_WORKFLOWS — the git_flow policy choices (INF-251). "Any" leaves git untouched (today's
// behavior); "Graphite" injects a PreToolUse guard hook into the agent's worktree that blocks
// raw mutating git commands and steers the agent to the `gt …` equivalents.
export const GIT_WORKFLOWS: SelectOption[] = [
  { value: "any", label: "Any", note: "No enforcement (default)" },
  { value: "graphite", label: "Graphite", note: "Enforce Graphite (gt) via a guard hook" },
];

// WORKSPACE_MODES — the workspace_mode provisioning choices (INF-418). "Worktree" is today's shared
// bare-mirror + git worktree per issue (fast, but branches lock across tickets); "Clone" provisions
// an independent git clone per dispatch (no cross-ticket checkout lock, enabling whole-stack ops,
// at the cost of a full clone per dispatch).
export const WORKSPACE_MODES: SelectOption[] = [
  { value: "worktree", label: "Worktree", note: "Shared mirror + worktree per issue (default)" },
  { value: "clone", label: "Clone", note: "Independent clone per dispatch — no checkout lock" },
];

// WORKSPACE_MODE_HINT — the trade-off help text shown on every workspace_mode control (INF-418).
export const WORKSPACE_MODE_HINT =
  "Clone: an independent git clone per dispatch — no cross-ticket checkout lock, enabling whole-stack operations (gt get); costs a full clone per dispatch. Worktree (default): shared mirror + worktree per issue — faster, but branches lock across tickets.";

// WORKSPACE_MODE_RECOMMEND_RATIONALE — the rationale shown when the UI recommends clone for a
// stacking project (effective dependency_mode graphite/dag + unset workspace_mode). (INF-418)
export const WORKSPACE_MODE_RECOMMEND_RATIONALE =
  "Recommended for stacked projects: independent clones remove the cross-ticket checkout lock, enabling whole-stack ops like a stack-wide BugBot sweep and a simpler agent stacking recipe; costs more disk/setup per ticket.";

// DEPENDENCY_MODES — the dependency_mode policy choices (INF-318/INF-320). A three-valued enum that
// controls how the daemon sequences a DAG of dependent Linear tickets from blockedBy edges. The
// default is "disabled" (today's behavior; the orchestration is fully opt-in) and is NOT derived
// from git_flow. The dropdown `note`s give the one-line summary; DEPENDENCY_MODE_HINT (below) gives
// the fuller explanation shared by the per-agent override row and the global General-tab field.
export const DEPENDENCY_MODES: SelectOption[] = [
  { value: "disabled", label: "Disabled", note: "Default · no auto-sequencing (today's behavior)" },
  { value: "graphite", label: "Graphite", note: "Stacked chain · unblocks at In Review" },
  { value: "dag", label: "DAG", note: "Parallel · unblocks only when merged" },
];

// DEPENDENCY_MODE_HINT — the shared help copy for both the per-agent override row and the global
// default field (the locked design's "well-documented" requirement; the enum is never surfaced
// bare). Names all three options, their unblock thresholds (graphite → In Review; dag → merged), the
// parallel-vs-stacked trade-off, and that disabled is the default (opt-in). It must NOT mention any
// git_flow-derived default — that derivation was removed from the locked design.
export const DEPENDENCY_MODE_HINT =
  "How dependent tickets are sequenced from Linear blockedBy edges. " +
  "Disabled (default): the daemon does not auto-sequence — today's behavior, fully opt-in; " +
  "tickets are dispatched as you flip them, with no auto-promote, unblock, or stacking. " +
  "Graphite: one stacked chain — a dependent starts as soon as its blocker reaches a review state " +
  "(In Review), stacking on the predecessor's branch. " +
  "DAG: independent tickets run in parallel, and a dependent starts only once every blocker is merged " +
  "(a terminal state), branching from a clean main.";

// CLAIM_MODES — the claim_mode policy choices (INF-477). Controls how a daemon acquires tickets in a
// project. "assignee" (default) is today's behavior — fetch tickets assigned to this API key's owner,
// no election. "pool" lets many teammates' daemons share one project of UNASSIGNED tickets, running a
// single-claimant claim protocol (comment election → assign self → read-back) so exactly one daemon
// works a ticket at a time. The dropdown `note`s summarize; CLAIM_MODE_HINT (below) is the fuller copy.
export const CLAIM_MODES: SelectOption[] = [
  { value: "assignee", label: "Assignee", note: "Default · work only tickets assigned to you" },
  { value: "pool", label: "Pool", note: "Shared project · claim any unassigned ticket, one worker each" },
];

// CLAIM_MODE_HINT — shared help copy for the global General-tab field and the per-agent override row.
export const CLAIM_MODE_HINT =
  "How the daemon acquires tickets. " +
  "Assignee (default): fetch only tickets assigned to this API key's owner — today's behavior, one " +
  "daemon per assignee, no coordination. " +
  "Pool: share one project of unassigned tickets across many teammates' daemons. Before working a " +
  "ticket, a daemon posts a claim comment, waits briefly, and — if it wins the earliest-claim " +
  "election — assigns the ticket to itself and re-reads the assignee to confirm, so exactly one daemon " +
  "works a given ticket at a time. A crashed claim expires (claim_ttl) and returns the ticket to the pool.";
