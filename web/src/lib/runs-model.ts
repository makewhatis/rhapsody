// Pure domain model for the Runs dashboard re-skin (INF-227). Kept in a .ts module (no DOM,
// no React) so it is unit-testable in this repo's node-environment Vitest setup, mirroring
// lib/format.ts and lib/project.ts. Maps the daemon's run/state wire shapes onto the values
// the redesigned `runs.jsx` view renders.

import type { StatusKey } from "@/components/ui/status-chip";
import type { LinearProject, LogEntry, RunSummary, StateResponse } from "@/lib/api";
import { elapsedSeconds, formatDuration, formatTokens, runDuration } from "@/lib/format";
import { repoShortName } from "@/lib/project";
import { projShort } from "@/lib/settings-model";

// outcomeToStatus maps a stored per-SEGMENT outcome (taxonomy v2: running|continued|completed|
// stopped|failed|interrupted) 1:1 onto a StatusChip status key. This is the SEGMENT-level chip
// used by RunDetail's history panel; the job-level status (the four-state jobs list) is derived
// separately by `jobStatus`. `continued` and `interrupted` are detail-only chips — a finished job
// never reads "running" because of them. Empty/unknown fall back to "idle". (INF-272)
export function outcomeToStatus(outcome: string): StatusKey {
  switch (outcome) {
    case "running":
      return "running";
    case "continued":
      return "continued";
    case "completed":
      return "completed";
    case "stopped":
      return "stopped";
    case "failed":
      return "failed";
    case "interrupted":
      // Worker died mid-flight (typically a daemon restart). Distinct, honest label rather than
      // the generic "idle" — boot recovery starts a fresh run for the issue if it's still active.
      return "interrupted";
    default: // "" | unknown
      return "idle";
  }
}

// StatTile is one of the four summary tiles at the top of the Runs view.
export interface StatTile {
  key: "running" | "completed" | "tokens" | "runtime";
  label: string;
  value: string;
  sub: string;
  accent?: string;
  pulse?: boolean;
}

// isSameLocalDay reports whether an RFC3339 timestamp falls on the same local calendar day as
// the reference epoch ms. An empty/invalid timestamp is never "today".
function isSameLocalDay(iso: string, nowMs: number): boolean {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return false;
  const a = new Date(t);
  const b = new Date(nowMs);
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

// durationSeconds returns whole seconds between two RFC3339 timestamps (0 on invalid/negative).
function durationSeconds(startedISO: string, endedISO: string): number {
  const start = Date.parse(startedISO);
  const end = Date.parse(endedISO);
  if (Number.isNaN(start) || Number.isNaN(end)) return 0;
  return Math.max(0, Math.floor((end - start) / 1000));
}

// deriveStatTiles computes the four Runs stat tiles from the live state snapshot plus the
// history rows, sourcing everything from the real API (never the mock):
//   - Running   : live `state.running` + store-running rows absent from the snapshot, deduped,
//                 plus an active-agents (distinct project) hint.
//   - Completed : runs whose stored outcome is `completed` (taxonomy v2 — replaces "In review").
//   - Tokens    : today's in/out/total tokens, summed per-run over the runs that STARTED today
//                 (live running rows + finished history rows, de-duplicated).
//   - Runtime   : today's total seconds (running rows' elapsed + finished rows' durations) and
//                 the count of runs that started today.
export function deriveStatTiles(
  state: StateResponse | undefined,
  history: RunSummary[],
  nowMs: number,
): StatTile[] {
  const running = state?.running ?? [];
  const liveRunIds = new Set<number>();
  for (const r of running) if (r.run_id > 0) liveRunIds.add(r.run_id);

  // The Running tile counts the live snapshot PLUS any history rows still running but momentarily
  // absent from the snapshot (deduped by run id), so it stays consistent with the merged jobs list
  // and never under-reads after a snapshot blip.
  const storeRunning = history.filter(
    (h) => h.outcome === "running" && !(h.id > 0 && liveRunIds.has(h.id)),
  );
  const runningCount = running.length + storeRunning.length;
  const agentsActive = new Set([
    ...running.map((r) => r.project),
    ...storeRunning.map((h) => h.project_slug),
  ]).size;
  const completedCount = history.filter((r) => r.outcome === "completed").length;

  // "Today" aggregates sum per-run over the runs that STARTED today, de-duplicated (a live run that
  // also has a history row counts once — the live row wins). We deliberately do NOT use
  // codex_totals: it is a process-lifetime cumulative total persisted across daemon restarts
  // (internal/orchestrator/persist.go), so adding it would both double-count today's finished runs
  // and conflate all-time history into a "today" label.
  let inToday = 0;
  let outToday = 0;
  let totalToday = 0;
  let secondsToday = 0;
  let runsToday = 0;

  for (const r of running) {
    if (!isSameLocalDay(r.started_at, nowMs)) continue;
    inToday += r.input_tokens;
    outToday += r.output_tokens;
    totalToday += r.total_tokens;
    secondsToday += elapsedSeconds(r.started_at, nowMs);
    runsToday += 1;
  }
  for (const h of history) {
    if (h.id > 0 && liveRunIds.has(h.id)) continue; // already counted as a live row
    if (!isSameLocalDay(h.started_at, nowMs)) continue;
    inToday += h.input_tokens;
    outToday += h.output_tokens;
    totalToday += h.total_tokens;
    secondsToday +=
      h.outcome === "running"
        ? elapsedSeconds(h.started_at, nowMs)
        : durationSeconds(h.started_at, h.ended_at);
    runsToday += 1;
  }

  return [
    {
      key: "running",
      label: "Running",
      value: String(runningCount),
      sub: `${agentsActive} agent${agentsActive === 1 ? "" : "s"} active`,
      accent: "var(--em-bright)",
      pulse: true,
    },
    {
      key: "completed",
      label: "Completed",
      value: String(completedCount),
      sub: "agent hand-off verified",
    },
    {
      key: "tokens",
      // The headline sums total_tokens, which is the cache-INCLUSIVE billed total
      // (Input + Output + CacheCreation + CacheRead — see internal/agent/claude/parse.go).
      // The subtext reconciles to it by surfacing the cache portion explicitly:
      // cached = total − in − out (clamped at 0 against any ordering/skew edge), so the three
      // parts always add up to the headline within formatTokens rounding. Under prompt caching
      // `cached` (mostly cache_read) typically dominates. (INF-282)
      label: "Tokens today",
      value: formatTokens(totalToday),
      sub: `${formatTokens(inToday)} in · ${formatTokens(outToday)} out · ${formatTokens(Math.max(0, totalToday - inToday - outToday))} cached`,
    },
    {
      key: "runtime",
      label: "Runtime today",
      value: formatDuration(secondsToday),
      sub: `across ${runsToday} run${runsToday === 1 ? "" : "s"}`,
    },
  ];
}

// --- Unified jobs list (merged Live + History) ---

export type JobFilterId = "all" | "running" | "completed" | "stopped" | "failed" | "waiting";

// JobStatusKey is the user-facing JOB status: the four taxonomy-v2 states plus "waiting" (a held
// dependent under graphite/dag orchestration — INF-320), derived from a job's segments + live/queued/
// waiting signals by `jobStatus`. A subset of StatusKey.
export type JobStatusKey = "running" | "completed" | "stopped" | "failed" | "waiting";

// The segmented filter for the jobs list (the four job states + the held "waiting" state).
export const JOB_FILTERS: { id: JobFilterId; label: string }[] = [
  { id: "all", label: "All" },
  { id: "running", label: "Running" },
  { id: "completed", label: "Completed" },
  { id: "stopped", label: "Stopped" },
  { id: "failed", label: "Failed" },
  { id: "waiting", label: "Waiting" },
];

// JobRow is one unified row in the merged Live+History jobs list. Every display value is
// pre-derived from the real wire shapes so the row component stays presentational.
export interface JobRow {
  /** Stable React key. */
  key: string;
  /** Durable run id; 0 means persistence is off (row is not clickable). */
  runId: number;
  issue: string;
  title: string;
  /** Resolved agent/project display name (Linear project name, else projShort, else repo). */
  agent: string;
  /** Resolved agent dot colour (Linear project colour, else the emerald accent token). */
  agentColor: string;
  status: StatusKey;
  /** Raw Linear project slug (the config key). */
  project: string;
  /** Resolved project display name (Linear name, else projShort, else the raw slug; "—" if none). */
  projectShort: string;
  turn: number;
  /** Pre-formatted token total (e.g. "84.2k"). */
  tokens: string;
  /** Pre-formatted duration (live elapsed, else run span). */
  duration: string;
  /** Render the duration in the live accent colour. */
  durationAccent: boolean;
  /** True while a segment of this job is genuinely in flight (a live snapshot row). */
  live: boolean;
  /** Parsed started_at epoch ms for sorting. */
  startedAtMs: number;
  /** Secondary label under the row; today only set for `failed` jobs (the failure reason). */
  subLabel?: string;
}

export interface ProjectMeta {
  name: string;
  color: string;
}

// projectColorMap indexes the fetched Linear projects by slug for agent name + colour lookup.
export function projectColorMap(projects: LinearProject[]): Map<string, ProjectMeta> {
  const m = new Map<string, ProjectMeta>();
  for (const p of projects) m.set(p.slug, { name: p.name, color: p.color });
  return m;
}

function agentName(slug: string, repo: string, meta: Map<string, ProjectMeta>): string {
  const m = meta.get(slug);
  if (m?.name) return m.name;
  if (slug && slug.trim() !== "") return projShort(slug);
  return repoShortName(repo); // single-project mode: fall back to the repo short name (or "—")
}

function agentColor(slug: string, meta: Map<string, ProjectMeta>): string {
  return meta.get(slug)?.color || "var(--em-bright)";
}

// resolveAgent resolves a single run's agent display name + dot colour from the fetched Linear
// projects (used by the run-detail header, where there's no pre-built jobs row). Mirrors the
// jobs-list resolution: Linear project name/colour, else projShort + the emerald accent token.
export function resolveAgent(
  slug: string,
  repo: string,
  projects: LinearProject[],
): { name: string; color: string } {
  const meta = projectColorMap(projects);
  return { name: agentName(slug, repo, meta), color: agentColor(slug, meta) };
}

// projectDisplayName resolves a project slug to a human label: the Linear project name when the
// project is in the fetched list, else projShort (strips a trailing hex id), else "—" for an empty
// slug. An id-only slug not in the list passes projShort through unchanged, so the unreadable raw
// config slug surfaces only as a last resort when the project genuinely can't be resolved. Mirrors
// the Agent column's resolution so Project cells never render the raw slug id. (INF-272)
function projectDisplayName(slug: string, meta: Map<string, ProjectMeta>): string {
  if (!slug || slug.trim() === "") return "—";
  return meta.get(slug)?.name || projShort(slug);
}

// resolveProject resolves a single run's project label from the fetched Linear projects (used by
// the run-detail meta cell, where there's no pre-built jobs row). Mirrors the jobs-list resolution.
export function resolveProject(slug: string, projects: LinearProject[]): string {
  return projectDisplayName(slug, projectColorMap(projects));
}

function parseMs(iso: string): number {
  const t = Date.parse(iso);
  return Number.isNaN(t) ? 0 : t;
}

// MergedRow is one segment/signal contributing to a job: a live snapshot row, a synthetic
// queued row (a pending retry/continuation from `state.retrying`), or a finished history row.
// mergeJobs groups these by issue and collapses each group into a single JobRow.
interface MergedRow {
  key: string;
  runId: number;
  issue: string;
  title: string;
  agent: string;
  agentColor: string;
  project: string;
  projectShort: string;
  turn: number;
  tokens: string;
  duration: string;
  durationAccent: boolean;
  startedAtMs: number;
  /** The stored segment outcome ("queued" for the synthetic retry row). */
  outcome: string;
  /** A genuinely in-flight live snapshot row. */
  live: boolean;
  /** A synthetic pending-retry/continuation row (from state.retrying). */
  queued: boolean;
  /** A synthetic held-dependent row (from state.blocked) — held by an uncleared blockedBy
   *  predecessor under graphite/dag orchestration (INF-320). */
  waiting: boolean;
  /** The formatted "<blocker> · <state>" a waiting row is held on (waiting rows only). */
  waitingOn?: string;
  /** Failure reason (history rows only; drives the failed sub-label). */
  error: string;
}

// failureSubLabel maps a failed segment's reason onto a compact sub-label so a turn-timeout or
// stall is identifiable without opening the run (taxonomy v2 folds timed_out/stalled into failed,
// so the reason string is the only discriminator).
export function failureSubLabel(reason: string): string {
  if (reason.startsWith("turn_timeout")) return "turn timeout";
  if (reason === "stalled") return "stalled";
  const trimmed = reason.trim();
  if (trimmed === "") return "";
  return trimmed.length > 40 ? `${trimmed.slice(0, 40)}…` : trimmed;
}

// jobStatus derives a job's user-facing status from its segments (newest-first) plus live/queued
// signals. Running comes ONLY from a genuinely-live row or a synthetic queued (retry/continuation
// pending) row — historical `continued` segments must not pin a finished job to "running". When
// nothing is live/queued the NEWEST segment decides: completed→completed, failed→failed, and
// everything else (stopped | interrupted | a continued segment whose claim is gone) → stopped.
export function jobStatus(
  group: { outcome: string; live: boolean; queued: boolean; waiting?: boolean }[],
): JobStatusKey {
  if (group.some((r) => r.live || r.queued)) return "running";
  // A held dependent reads "waiting" (INF-320) — but ONLY when the whole group is synthetic-waiting:
  // a ticket that is live/retrying (handled above) or has a real finished segment is no longer purely
  // waiting, so its real run status wins and the run stays openable. In practice the daemon never holds
  // a ticket that has already run, so this is a defensive guard (every() ⇒ no real/history sibling).
  if (group.some((r) => r.waiting) && group.every((r) => r.waiting)) return "waiting";
  const newest = group[0];
  switch (newest?.outcome) {
    case "completed":
      return "completed";
    case "failed":
      return "failed";
    default:
      return "stopped";
  }
}

// mergeJobs merges the live running sessions (`state.running`), the pending retries/continuations
// (`state.retrying`, surfaced as synthetic queued rows — this is the until-now-ignored signal that
// keeps a job "running" between continuation segments), and the finished history rows into a
// JOB-centric list: one row per issue_identifier (rows with an empty identifier stay individual,
// keyed by run id). Each group's status is derived by `jobStatus`; running jobs sort first, then by
// most-recent activity. (taxonomy v2, INF-272)
export function mergeJobs(
  state: StateResponse | undefined,
  history: RunSummary[],
  projects: LinearProject[],
  nowMs: number,
): JobRow[] {
  const meta = projectColorMap(projects);
  const liveIds = new Set<number>();
  const merged: MergedRow[] = [];

  for (const r of state?.running ?? []) {
    if (r.run_id > 0) liveIds.add(r.run_id);
    merged.push({
      key: `live-${r.run_id || r.issue_identifier}`,
      runId: r.run_id,
      issue: r.issue_identifier,
      title: r.title,
      agent: agentName(r.project, r.repo, meta),
      agentColor: agentColor(r.project, meta),
      project: r.project,
      projectShort: projectDisplayName(r.project, meta),
      turn: r.turn_count,
      tokens: formatTokens(r.total_tokens),
      duration: formatDuration(elapsedSeconds(r.started_at, nowMs)),
      durationAccent: true,
      startedAtMs: parseMs(r.started_at),
      outcome: "running",
      live: true,
      queued: false,
      waiting: false,
      error: "",
    });
  }

  // Synthetic queued rows: one per pending retry/continuation. RetryEntry carries only the issue
  // identifier (no run id, title, or project), so the row's rich display fields fall back to its
  // sibling history rows when the group is collapsed.
  for (const q of state?.retrying ?? []) {
    merged.push({
      key: `queued-${q.issue_identifier}`,
      runId: 0,
      issue: q.issue_identifier,
      title: "",
      agent: agentName("", "", meta),
      agentColor: agentColor("", meta),
      project: "",
      projectShort: "—",
      turn: 0,
      tokens: formatTokens(0),
      duration: "",
      durationAccent: false,
      startedAtMs: parseMs(q.due_at),
      outcome: "queued",
      live: false,
      queued: true,
      waiting: false,
      error: q.error,
    });
  }

  // Synthetic waiting rows: one per held dependent (state.blocked, INF-318/INF-320). BlockedEntry
  // carries its own title + project, so unlike the queued row it resolves its display fields directly.
  // Only ever populated under graphite/dag — a disabled project produces no entries, so no rows.
  for (const b of state?.blocked ?? []) {
    merged.push({
      key: `blocked-${b.issue_identifier}`,
      runId: 0,
      issue: b.issue_identifier,
      title: b.title,
      agent: agentName(b.project, "", meta),
      agentColor: agentColor(b.project, meta),
      project: b.project,
      projectShort: projectDisplayName(b.project, meta),
      turn: 0,
      tokens: formatTokens(0),
      duration: "",
      durationAccent: false,
      startedAtMs: 0,
      outcome: "waiting",
      live: false,
      queued: false,
      waiting: true,
      waitingOn: `${b.blocker_identifier} · ${b.blocker_state}`,
      error: "",
    });
  }

  for (const h of history) {
    if (h.id > 0 && liveIds.has(h.id)) continue; // already represented by the live row
    const live = h.outcome === "running";
    merged.push({
      key: `hist-${h.id}`,
      runId: h.id,
      issue: h.issue_identifier,
      title: h.title,
      agent: agentName(h.project_slug, h.repo, meta),
      agentColor: agentColor(h.project_slug, meta),
      project: h.project_slug,
      projectShort: projectDisplayName(h.project_slug, meta),
      turn: h.turns,
      tokens: formatTokens(h.total_tokens),
      duration: live
        ? formatDuration(elapsedSeconds(h.started_at, nowMs))
        : runDuration(h.started_at, h.ended_at),
      durationAccent: live,
      startedAtMs: parseMs(h.started_at),
      outcome: h.outcome,
      live,
      queued: false,
      waiting: false,
      error: h.error,
    });
  }

  // Group by issue_identifier; rows with an empty identifier stay individual (keyed by their own
  // row key) so an unattributed run never collapses other jobs into one synthetic row.
  const groups = new Map<string, MergedRow[]>();
  for (const row of merged) {
    const gk = row.issue ? `issue:${row.issue}` : `solo:${row.key}`;
    const g = groups.get(gk);
    if (g) g.push(row);
    else groups.set(gk, [row]);
  }

  const out: JobRow[] = [];
  for (const g of groups.values()) {
    g.sort((a, b) => b.startedAtMs - a.startedAtMs); // newest-first
    const status = jobStatus(g);
    const liveRow = g.find((r) => r.live);
    const waitingRow = g.find((r) => r.waiting);
    const newestReal = g.find((r) => !r.queued && !r.waiting); // a live or history row (never synthetic)
    const isWaiting = status === "waiting";
    // For a held job the waiting row owns the display (its title/project come from BlockedEntry) and
    // the row is never clickable (it has never run → runId 0). Otherwise the live/newest-real row wins.
    const rep = liveRow ?? (isWaiting ? waitingRow : undefined) ?? newestReal ?? g[0];
    out.push({
      key: rep.key,
      runId: isWaiting ? 0 : (newestReal?.runId ?? 0), // click opens the newest real segment's detail
      issue: rep.issue,
      title: rep.title,
      agent: rep.agent,
      agentColor: rep.agentColor,
      status,
      project: rep.project,
      projectShort: rep.projectShort,
      turn: rep.turn,
      tokens: rep.tokens,
      duration: rep.duration,
      durationAccent: rep.durationAccent,
      live: !!liveRow,
      startedAtMs: rep.startedAtMs,
      subLabel: isWaiting
        ? `waiting on ${waitingRow?.waitingOn ?? ""}`
        : status === "failed"
          ? failureSubLabel(newestReal?.error ?? "") || undefined
          : undefined,
    });
  }

  out.sort((a, b) => {
    const ar = a.status === "running" ? 0 : 1;
    const br = b.status === "running" ? 0 : 1;
    if (ar !== br) return ar - br;
    return b.startedAtMs - a.startedAtMs;
  });
  return out;
}

// matchFilter applies a segmented-filter id to a job row (taxonomy v2: the four job states).
// "stopped" also matches a residual `interrupted` job status defensively, though jobStatus folds
// interrupted into stopped at the job level.
export function matchFilter(row: JobRow, filter: JobFilterId): boolean {
  switch (filter) {
    case "all":
      return true;
    case "running":
      return row.status === "running";
    case "completed":
      return row.status === "completed";
    case "stopped":
      return row.status === "stopped" || row.status === "interrupted";
    case "failed":
      return row.status === "failed";
    case "waiting":
      return row.status === "waiting";
    default:
      return true;
  }
}

// searchJobs filters rows by a case-insensitive substring over issue + title + agent, plus the
// sub-label (so a waiting row's predecessor identifier in "waiting on <blocker> · <state>" is
// matchable — INF-320). Ported from the `runs.jsx` search box.
export function searchJobs(rows: JobRow[], q: string): JobRow[] {
  const needle = q.trim().toLowerCase();
  if (needle === "") return rows;
  return rows.filter((r) =>
    `${r.issue} ${r.title} ${r.agent} ${r.subLabel ?? ""}`.toLowerCase().includes(needle),
  );
}

// --- Transcript rendering ---

export type TranscriptEntryType = "divider" | "text" | "tool" | "out";

// transcriptEntryType maps a humanized LogEntry.kind onto the redesigned transcript's visual
// entry type (runs.jsx): event→divider, tool_use→tool chip, tool_result→out line, text/thinking→
// prose text.
export function transcriptEntryType(kind: LogEntry["kind"]): TranscriptEntryType {
  switch (kind) {
    case "event":
      return "divider";
    case "tool_use":
      return "tool";
    case "tool_result":
      return "out";
    case "text":
    case "thinking":
    default:
      return "text";
  }
}

// isMcpTool reports whether a tool name is an MCP tool (mcp__server__method), which the design
// renders as an emerald MCP chip.
export function isMcpTool(tool: string): boolean {
  return tool.startsWith("mcp__");
}
