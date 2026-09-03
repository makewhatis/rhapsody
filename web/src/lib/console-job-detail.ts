// The Job-detail model — STUDIO-681 §4, built by STUDIO-683.
//
// What survives of it. STUDIO-742 rebuilt the run detail into the "Trace" three zones, and the
// summary strip, the run `.rmeta` line, the run one-liner and the flat §4 timeline that this
// module served went with the view that showed them (`buildJobSummary`, `runMeta`,
// `runDescription`, `transcriptTimeline`) — the zones read `lib/trace-model` and
// `lib/console-trace-view` instead. What is left is what the new view still asks of this module:
// the run ordering, the local clock time, a run's own outcome Pill, and the §4 pull-request card's
// model.
//
// DEPENDENCY (§9/§11): the PULL REQUEST has no daemon source and is NOT invented here — no
// endpoint serves a PR number, its checks or its mergeability.
//
// DORMANT, deliberately: STUDIO-745 folded the §4 side cards into the run detail's watch-tabs
// rail, and the Diff tab names that dependency now — so `PullRequestView`, `checksSummary` and
// `mergeNote` below currently have NO consumer outside their own tests. They are kept rather than
// deleted because slice 7 of the Trace plan (the run-branch diff endpoint plus a live Merge) is
// what will supply the `PullRequestView` this was written for, and a Merge control cannot decide
// anything without exactly these two rules. Delete them if that slice is ever dropped.
import type { RunSummary } from "@/lib/api";
import type { ConsoleJobStatus } from "@/lib/console-jobs";

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
