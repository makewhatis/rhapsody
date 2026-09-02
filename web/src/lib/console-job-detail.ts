// The Job-detail model — STUDIO-681 §4, built by STUDIO-683.
//
// Everything one ticket's page shows, derived from `GET /api/v1/issues/<KEY>/history` (the run
// rows) and `GET /api/v1/runs/<id>/transcript` (a run's humanized timeline). Both are existing
// endpoints; §9 maps this view to exactly them.
//
// DEPENDENCY (§9/§11): two of §4's fields have no daemon source and are NOT invented here.
//   - Run KIND (implement / review / rebase / …) — `store::RunSummary` records `outcome` and
//     `attempt`, never a kind. The run row labels itself by outcome instead.
//   - The PULL REQUEST — no endpoint serves a PR number, its checks or its mergeability. The
//     card below renders from a `PullRequestView` a caller supplies; the app has no such caller
//     yet and shows the dependency in its place rather than fabricating a PR.
import { formatTokens, runDuration } from "@/lib/format";
import type { LogEntry, RunSummary } from "@/lib/api";
import { CONSOLE_STATUS_LABELS, consoleJobStatus, relativeSince } from "@/lib/console-jobs";
import type { ConsoleJobStatus } from "@/lib/console-jobs";
import { jobStatus } from "@/lib/runs-model";

const DASH = "—";

/** Newest run first (§4, §10 box 2.10). Ties keep their incoming order via the run id. */
export function runsNewestFirst(runs: readonly RunSummary[]): RunSummary[] {
  return [...runs].sort((a, b) => {
    const delta = parseMs(b.started_at) - parseMs(a.started_at);
    return delta !== 0 ? delta : b.id - a.id;
  });
}

function parseMs(iso: string): number {
  if (!iso) return 0;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? 0 : ms;
}

/** "19:11" in the viewer's local zone; "" when the instant is absent or unparseable. */
export function clockTime(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return "";
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The §4 summary strip — one `.kv` cell each. */
export interface JobSummary {
  status: ConsoleJobStatus;
  statusLabel: string;
  /** Teammate name, or "" when solo/unassigned. */
  assignee: string;
  /** PR reference + state, or "—" while no endpoint serves one. */
  pullRequest: string;
  /** Newest run's branch, or "—". */
  branch: string;
  runs: number;
  updated: string;
  /** The ticket's title, from its newest run row. */
  title: string;
  /** Project display slug, or "". */
  project: string;
}

/**
 * Builds the summary strip from a ticket's run history. `live` reports whether the ticket is
 * in the daemon's running snapshot — the one signal the history rows cannot carry, since a run
 * row is only written as the run progresses.
 *
 * `lifecycle` is the TICKET's own state (STUDIO-702), which the run rows this view is built
 * from also cannot carry: `/api/v1/issues/<KEY>/history` serves runs, and only the issue-level
 * listing decorates a ticket with its tracker state. The caller supplies it from there, and it
 * is passed straight through to the worklist's own `consoleJobStatus` rather than being
 * re-interpreted here — the header and the row must not be able to disagree (STUDIO-706).
 * Absent, it falls back to the run outcome exactly as before.
 */
export function buildJobSummary(
  runs: readonly RunSummary[],
  opts: {
    live?: boolean;
    assignee?: string;
    pullRequest?: string;
    lifecycle?: string;
    nowMs: number;
  },
): JobSummary {
  const ordered = runsNewestFirst(runs);
  const newest = ordered[0];
  // Liveness is its own signal, prepended rather than stamped onto a stored row: the run that
  // is in flight may not have been persisted yet, so marking the newest HISTORY row live would
  // both misdescribe that row and, for a ticket with no history at all, lose the signal
  // entirely — a just-dispatched ticket would read "queued".
  const signals = ordered.map((r) => ({ outcome: r.outcome, live: false, queued: false }));
  if (opts.live ?? false) signals.unshift({ outcome: "running", live: true, queued: false });
  const status = consoleJobStatus(jobStatus(signals), opts.lifecycle);
  const updatedAtMs = ordered.reduce(
    (max, r) => Math.max(max, parseMs(r.ended_at) || parseMs(r.started_at)),
    0,
  );
  return {
    status,
    statusLabel: CONSOLE_STATUS_LABELS[status],
    assignee: opts.assignee ?? "",
    pullRequest: opts.pullRequest ?? "",
    branch: newest?.branch || DASH,
    runs: runs.length,
    updated: relativeSince(updatedAtMs, opts.nowMs),
    title: newest?.title ?? "",
    project: newest?.project_slug ?? "",
  };
}

/** The `.rmeta` line of an expanded run (§4): who ran it, when, for how long, at what cost. */
export interface RunMeta {
  /** Teammate name, or "" — a stored run row carries no identity (see the DEPENDENCY note). */
  identity: string;
  /** "19:11 → 19:15" — the run's window; the end is omitted while it is still going. */
  window: string;
  duration: string;
  turns: string;
  tokens: string;
}

export function runMeta(run: RunSummary, identity = ""): RunMeta {
  const start = clockTime(run.started_at);
  const end = clockTime(run.ended_at);
  return {
    identity,
    window: start === "" ? DASH : end === "" ? `${start} →` : `${start} → ${end}`,
    duration: runDuration(run.started_at, run.ended_at),
    turns: `${run.turns} ${run.turns === 1 ? "turn" : "turns"}`,
    tokens: `${run.usage_estimated ? "~" : ""}${formatTokens(run.total_tokens)} tokens`,
  };
}

/**
 * A run's one-line description in the collapsed row. No kind is recorded (see the DEPENDENCY
 * note), so a run identifies itself by what it did: its error when it failed, else its outcome.
 */
export function runDescription(run: RunSummary): string {
  if (run.error !== "") return run.error;
  return run.outcome === "" ? DASH : run.outcome;
}

/**
 * The Pill a RUN's own outcome paints in the runs list. This is the run's state, not the
 * ticket's, so it keeps the taxonomy-v2 spelling: only a clean `completed` is blue-`done`, a
 * `failed` run is red, and `stopped`/`interrupted` are grey — a stopped run is emphatically
 * not "done".
 */
export function runOutcomePill(outcome: string): ConsoleJobStatus {
  switch (outcome) {
    case "running":
      return "run";
    case "completed":
      return "done";
    case "failed":
      return "blocked";
    default:
      return "queued";
  }
}

/** The transcript timeline's line kinds (§4) — each paints its own glyph. */
export type TimelineKind = "turn" | "tool" | "post" | "retain" | "note" | "done";

export interface TimelineEntry {
  seq: number;
  kind: TimelineKind;
  text: string;
  /** Tool name on a `tool`/`post`/`retain` line; "" otherwise. */
  tool: string;
  /** The tool's folded result line, "" when it produced none. */
  result: string;
}

/**
 * Folds a run's humanized transcript into the §4 timeline: the turn's start, its tool calls
 * (with each call's result folded onto the call that produced it, as the prototype renders
 * them), its room posts and memory retentions, and its completion.
 *
 * The `done` tint is presentation only — an event line the daemon words differently still
 * renders, just with the neutral turn glyph. Nothing downstream branches on it.
 */
export function transcriptTimeline(entries: readonly LogEntry[]): TimelineEntry[] {
  const out: TimelineEntry[] = [];
  for (const entry of entries) {
    if (entry.kind === "tool_result") {
      // Belongs to the call above it. With no call to attach to (a truncated transcript) it
      // becomes its own note rather than being dropped.
      const last = out[out.length - 1];
      if (last && last.result === "" && (last.kind === "tool" || last.kind === "post" || last.kind === "retain")) {
        last.result = entry.text;
        continue;
      }
      out.push({ seq: entry.seq, kind: "note", text: entry.text, tool: "", result: "" });
      continue;
    }
    out.push({
      seq: entry.seq,
      kind: timelineKind(entry),
      text: entry.text,
      tool: entry.kind === "tool_use" ? entry.tool : "",
      result: "",
    });
  }
  return out;
}

function timelineKind(entry: LogEntry): TimelineKind {
  if (entry.kind === "tool_use") {
    if (entry.tool === "teams_post") return "post";
    if (entry.tool === "teams_retain") return "retain";
    return "tool";
  }
  if (entry.kind === "event") {
    return /\bcomplet(ed|e)\b/i.test(entry.text) ? "done" : "turn";
  }
  return "note";
}

// --- Pull request card (§4) ---------------------------------------------------------------
// Rendered from data a caller supplies. NO endpoint serves it yet — see the DEPENDENCY note.

export type CheckState = "pass" | "fail" | "pending";

export interface PullRequestCheck {
  name: string;
  state: CheckState;
  /** "passed", "12s", "failing" — the right-hand mono cell. */
  detail: string;
}

export interface PullRequestView {
  /** "#230". */
  number: string;
  url: string;
  draft: boolean;
  /** Commits behind the base branch. */
  behind: number;
  checks: PullRequestCheck[];
}

export interface ChecksSummary {
  passed: number;
  failed: number;
  pending: number;
}

export function checksSummary(checks: readonly PullRequestCheck[]): ChecksSummary {
  const summary: ChecksSummary = { passed: 0, failed: 0, pending: 0 };
  for (const check of checks) {
    if (check.state === "pass") summary.passed += 1;
    else if (check.state === "fail") summary.failed += 1;
    else summary.pending += 1;
  }
  return summary;
}

export interface MergeNote {
  blocked: boolean;
  text: string;
}

/**
 * The card's mergeable/blocked note (§10 box 2.11). A failing check blocks first, then a
 * pending one, then draft state: the operator is told the NEAREST thing standing in the way,
 * not every one of them.
 */
export function mergeNote(pr: PullRequestView): MergeNote {
  const { failed, pending } = checksSummary(pr.checks);
  const behind = `${pr.behind} behind`;
  if (failed > 0) {
    return { blocked: true, text: `Blocked — ${failed} failing ${plural(failed, "check")}.` };
  }
  if (pending > 0) {
    return { blocked: true, text: `Checks running — ${pending} still pending.` };
  }
  if (pr.draft) {
    return { blocked: true, text: `Draft · ${behind} · all checks passed. Mark ready to merge.` };
  }
  return { blocked: false, text: `${behind} · mergeable. All checks passed.` };
}

function plural(n: number, word: string): string {
  return n === 1 ? word : `${word}s`;
}
