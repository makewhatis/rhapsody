import type { RunSummary } from "@/lib/api";
import { formatTokens, runDuration } from "@/lib/format";
import { baseToolName, type PhaseKind, type ResultCard, type TracePhase } from "@/lib/trace-model";

// console-trace-view — the derivations the "Trace" run detail needs on top of the slice-1 trace
// model (design record `~/.rhapsody/docs/console-run-detail-design.md` §3; slice 2 of its §9 plan).
//
// The model in `trace-model` answers "what did this run do"; this module answers the three things
// the VIEW asks that are not about the transcript at all: what the header's mono vitals read, what
// the spine's filter keeps, and which of the header's actions can be a real link.
//
// Pure functions in a `.ts` module, like `console-job-detail` and `console-jobs` next to it, so
// every rule the view leans on is asserted directly rather than through a render.

const DASH = "—";

/** The spine's filter chips (design record §3C), All first. */
export type TraceFilter = "all" | "edits" | "bash" | "errors";

export const TRACE_FILTERS: readonly TraceFilter[] = ["all", "edits", "bash", "errors"];

export const TRACE_FILTER_LABELS: Record<TraceFilter, string> = {
  all: "All",
  edits: "Edits",
  bash: "Bash",
  errors: "Errors",
};

/** The header's right-aligned mono strip, and the Result card's receipt (§3A/§3B). */
export interface RunVitals {
  /** ended − started; a dash while the run has not ended, never a fabricated 0s. */
  duration: string;
  turns: string;
  /** Prefixed "~" when the daemon's total is a floored estimate rather than authoritative. */
  tokens: string;
  branch: string;
  /** The trace's total tool calls — the receipt's fourth number. */
  tools: number;
}

export function runVitals(run: RunSummary, phases: readonly TracePhase[]): RunVitals {
  return {
    duration: runDuration(run.started_at, run.ended_at),
    turns: `${run.turns} ${run.turns === 1 ? "turn" : "turns"}`,
    tokens: `${run.usage_estimated ? "~" : ""}${formatTokens(run.total_tokens)}`,
    branch: run.branch.trim() === "" ? DASH : run.branch,
    tools: phases.reduce((n, phase) => n + phase.did.length, 0),
  };
}

/**
 * Whether a phase survives the chip.
 *
 * "Edits" keys on the phase's own `edited` side effect rather than on its title: the model only
 * raises that chip for a call that actually named — or, for a real edit tool, actually was — a file
 * write, so a phase titled Implemented because it ran `git push` is correctly not an edit.
 */
function matchesChip(phase: TracePhase, filter: TraceFilter): boolean {
  switch (filter) {
    case "edits":
      return phase.effects.some((effect) => effect.kind === "edited");
    case "bash":
      return phase.did.some((card) => baseToolName(card.tool) === "Bash");
    case "errors":
      return phase.failed;
    default:
      return true;
  }
}

/**
 * Whether a phase contains the grep. The haystack is everything the phase shows OR hides — its
 * title and subtitle, each call's tool, target and folded result, and its prose — because the
 * field exists to find the step whose OUTPUT mentions something, which is the one thing a spine of
 * collapsed one-liners cannot show.
 */
function matchesQuery(phase: TracePhase, needle: string): boolean {
  const hay = [
    phase.title,
    phase.subtitle,
    ...phase.did.flatMap((card) => [card.tool, card.target, card.result]),
    ...phase.said.map((block) => block.text),
    ...phase.orphanResults,
  ];
  return hay.some((text) => text.toLowerCase().includes(needle));
}

/** The spine's visible phases: the chip and the grep AND-ed, in transcript order. */
export function filterPhases(
  phases: readonly TracePhase[],
  filter: TraceFilter,
  query: string,
): TracePhase[] {
  const needle = query.trim().toLowerCase();
  return phases.filter(
    (phase) => matchesChip(phase, filter) && (needle === "" || matchesQuery(phase, needle)),
  );
}

/** The Result card's tone — which accent the card's rule and eyebrow take (§3B). */
export type ResultTone = "done" | "fail" | "stop";

export interface ResultEyebrow {
  text: string;
  tone: ResultTone;
}

/**
 * The eyebrow above the Result card's headline.
 *
 * A completed run reads "done · handed off" only when the slice-1 model found a handoff call, so
 * the card distinguishes a run that PARKED its ticket from one that merely stopped talking; an
 * outcome the taxonomy grows later names itself rather than being rounded to "done".
 */
export function resultEyebrow(run: RunSummary, source: ResultCard["source"]): ResultEyebrow {
  switch (run.outcome) {
    case "completed":
      return { text: source === "handoff" ? "done · handed off" : "done", tone: "done" };
    case "failed":
      return { text: "failed", tone: "fail" };
    case "stopped":
    case "interrupted":
      return { text: run.outcome, tone: "stop" };
    case "":
      return { text: "unknown", tone: "stop" };
    default:
      return { text: run.outcome, tone: "done" };
  }
}

/**
 * "owner/name" for a GitHub remote, "" for anything else.
 *
 * The host is matched exactly, never as a substring: `github.com.evil.example/a/b` contains the
 * string "github.com" and is not GitHub, and a link built from it would send the operator
 * somewhere the daemon never named.
 */
export function githubRepo(repo: string): string {
  const trimmed = repo.trim().replace(/\.git$/, "");
  if (trimmed === "") return "";
  const ssh = /^(?:ssh:\/\/)?(?:[^@/]+@)?github\.com[:/]+([^/]+)\/([^/]+)$/i.exec(trimmed);
  if (ssh !== null) return `${ssh[1]}/${ssh[2]}`;
  const https = /^https?:\/\/(?:[^@/]+@)?github\.com\/([^/]+)\/([^/]+)$/i.exec(trimmed);
  return https === null ? "" : `${https[1]}/${https[2]}`;
}

/**
 * Where "View PR" goes. No daemon endpoint serves a PR number (design record §5), so the link is a
 * head-branch SEARCH on the run's own remote — it resolves to this branch's pull request without
 * the console ever asserting one exists. "" when either half is missing, and the action then names
 * its dependency instead of offering a link that would 404.
 */
export function prSearchUrl(run: RunSummary): string {
  const repo = githubRepo(run.repo);
  const branch = run.branch.trim();
  if (repo === "" || branch === "") return "";
  return `https://github.com/${repo}/pulls?q=${encodeURIComponent(`is:pr head:${branch}`)}`;
}

/** The ticket's Linear deep link, built from the connected workspace's slug; "" when either is absent. */
export function ticketUrl(workspaceURLKey: string, issue: string): string {
  if (workspaceURLKey.trim() === "" || issue.trim() === "") return "";
  return `https://linear.app/${workspaceURLKey}/issue/${issue}`;
}

/**
 * The spine row's glyph. Text, not an icon component, because the same vocabulary has to fit the
 * Jobs worklist sparkline the design record's §6 puts on a table row.
 */
const PHASE_GLYPHS: Record<PhaseKind, string> = {
  oriented: "◎",
  implemented: "✎",
  verified: "✓",
  coordinated: "◇",
  handoff: "⇢",
  other: "•",
};

export function phaseGlyph(kind: PhaseKind): string {
  return PHASE_GLYPHS[kind];
}
