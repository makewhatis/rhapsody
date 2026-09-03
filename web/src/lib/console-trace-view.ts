import type { RunDetail, RunSummary } from "@/lib/api";
import { formatTokens, runDuration } from "@/lib/format";
import { fenceSpans, inlineText } from "@/lib/markdown";
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
  const branch = runBranch(run);
  return {
    duration: runDuration(run.started_at, run.ended_at),
    turns: `${run.turns} ${run.turns === 1 ? "turn" : "turns"}`,
    tokens: `${run.usage_estimated ? "~" : ""}${formatTokens(run.total_tokens)}`,
    branch: branch === "" ? DASH : branch,
    tools: phases.reduce((n, phase) => n + phase.did.length, 0),
  };
}

/**
 * The workspace key a ticket identifier gets — the JS mirror of the daemon's `sanitize_key`
 * (`crates/workspace/src/sanitize.rs`): every scalar outside `[A-Za-z0-9._-]` becomes one `_`, and
 * a result that would name the workspace root or its parent becomes `_`.
 *
 * A Linear identifier ("STUDIO-742") passes through untouched, so this only ever matters for a
 * tracker whose keys are not; approximating it with the raw identifier would name a branch the
 * daemon never creates.
 */
function workspaceKey(identifier: string): string {
  const key = [...identifier].map((c) => (/[A-Za-z0-9._-]/.test(c) ? c : "_")).join("");
  return key === "" || key === "." || key === ".." ? "_" : key;
}

/**
 * The run's workspace branch: the row's own `branch` when the daemon served one, else the name the
 * daemon's branch naming DETERMINES for this ticket.
 *
 * `runs.branch` is empty on every row the store has ever written — `persist_start_run`
 * (`crates/orchestrator/src/persist.rs`) leaves it at its default "for Phase 4" and is the column's
 * only writer — so reading it alone leaves this vital, and the PR search built on it, permanently
 * blank. The fallback is a fact rather than a guess: `Repo::ensure_from_repo` and
 * `Repo::ensure_clone_from_repo` (`crates/workspace/src/repo.rs`) both name the branch
 * `symphony/{sanitize_key(identifier)}`, and that prefix is a frozen cross-process contract the
 * README's Divergences puts explicitly out of scope for renaming.
 *
 * The one run this does NOT describe is a review run, which gets a detached worktree and creates no
 * branch of its own (`crates/workspace/src/repo.rs`) — but the branch it reviews is the same
 * `symphony/<key>`, so the name still points at the work, and what is built from it below is a
 * SEARCH, which answers "no such branch" honestly rather than 404ing.
 */
export function runBranch(run: RunSummary): string {
  const served = run.branch.trim();
  if (served !== "") return served;
  const identifier = run.issue_identifier.trim();
  // No ticket, no branch: the daemon would name this workspace `_`, and searching for
  // `symphony/_` finds nothing anyone was looking for.
  return identifier === "" ? "" : `symphony/${workspaceKey(identifier)}`;
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

/** The Result card's failure banner: the run's own `error` string, with the tone §3B gives it. */
export interface ResultBanner {
  /** "Error" for a failure, "Reason" for an operator stop — what the string in front of you IS. */
  label: string;
  tone: Extract<ResultTone, "fail" | "stop">;
  text: string;
}

/**
 * The banner the Result card shows above its headline, or `null` when the run recorded no error.
 *
 * Design record §3B: "Failed -> red banner + error; Stopped -> amber reason". This is deliberately
 * INDEPENDENT of whether the run wrote a hand-off: a run that hands off and then fails keeps its
 * prose headline, and the error is the whole reason an operator opened the view. The eyebrow says
 * THAT it ended badly; only this says why.
 */
export function resultBanner(run: RunSummary): ResultBanner | null {
  const text = run.error.trim();
  if (text === "") return null;
  const stopped = run.outcome === "stopped" || run.outcome === "interrupted";
  return { label: stopped ? "Reason" : "Error", tone: stopped ? "stop" : "fail", text };
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
 * the console ever asserting one exists. A search is also what makes [`runBranch`]'s fallback safe:
 * a branch that turns out not to exist returns an empty result page, never a wrong pull request.
 * "" only when the remote is not a GitHub one, and the action then names that dependency.
 */
export function prSearchUrl(run: RunSummary): string {
  const repo = githubRepo(run.repo);
  const branch = runBranch(run);
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

/** The lead's plain text, through the SAME inline parse the headline was stripped with. */
function plainLead(source: string): string {
  return inlineText(source.replace(/!\[/g, "["))
    .replace(/\s+/g, " ")
    .trim();
}

/** How many of a lead's boundaries — sentence ends, and line breaks — are worth testing. */
const LEAD_SENTENCE_SCAN = 8;

/**
 * The markdown the Result card should print under its H1, or "" when the H1 already says it.
 *
 * The slice-1 model GROWS the headline out of the lead's opening sentences, so the two are equal
 * only when the lead is exactly one sentence long. The far commoner real shape is "the headline,
 * and then more" — measured over the 441 recorded runs, requiring whole-lead equality left 184 of
 * them (41.7%) printing their own H1 again immediately under it. So the sentence PREFIX that
 * produced the headline is dropped and the rest kept, and a lead an UNCLIPPED headline already
 * contains whole is dropped entirely.
 *
 * Both comparisons run the lead through the STUDIO-739 renderer's own inline parse — the same pass
 * the headline went through — so a lead of `Photo attachment **shipped**.` is recognised as the
 * headline it produced. A headline the model CLIPPED ends in an ellipsis and is matched as a
 * prefix of a SENTENCE of the lead, since the sentence behind it is longer than the H1 that showed
 * it — never as a prefix of the lead as a whole, which it always is and which therefore says
 * nothing about how much of the lead the card may drop.
 */
export function cardLead(card: ResultCard): string {
  const lead = card.lead.trim();
  if (lead === "") return "";
  const clipped = card.headline.endsWith("…");
  const target = clipped ? card.headline.slice(0, -1).trimEnd() : card.headline;
  const isHeadline = (text: string) => (clipped ? text.startsWith(target) : text === target);
  const whole = plainLead(lead);
  // Deliberately NOT `isHeadline`. A clipped headline is a PREFIX of the sentence it was cut from,
  // so the whole lead starts with it by construction — treating that as "the H1 already said it"
  // deleted the entire hand-off from 15 of the 445 recorded runs, the worst a 3,355-char six-
  // paragraph body under a one-sentence H1. The clipped case belongs to the sentence walk below,
  // which drops the clipped sentence and keeps everything after it.
  if (!clipped && whole === target) return "";
  // The headline can also be GROWN past the lead — the model reaches into the next paragraph when
  // the opening sentence is a bare "Done." — in which case the H1 already carries the whole lead.
  // The lead must be a whole SENTENCE the headline then continues past, not merely a string
  // prefix of it: a one-word lead of "A" prefixes "Absolutely everything changed." and shares
  // nothing with it.
  if (!clipped && /[.!?]$/.test(whole) && target.startsWith(`${whole} `)) return "";
  // A line BREAK ends a sentence too. The corpus signs off on a bare URL constantly ("Done. Draft
  // PR: https://…\n") and a URL carries no `.!?` to end on, so a sentence-only walk found no
  // boundary at all and printed the H1 again underneath itself — 25 of the 446 recorded runs.
  // Only for an UNCLIPPED headline, where the match is exact equality and the dropped prefix is
  // therefore the headline and nothing else; under a clipped one a `startsWith` could match
  // mid-sentence at a soft wrap and take the rest of that sentence with it.
  const re = clipped ? /(?<=[.!?])\s+/g : /(?<=[.!?])\s+|\n/g;
  for (let n = 0, match = re.exec(lead); match !== null && n < LEAD_SENTENCE_SCAN; n += 1) {
    if (isHeadline(plainLead(lead.slice(0, match.index)))) return lead.slice(match.index).trim();
    match = re.exec(lead);
  }
  return lead;
}

/**
 * A body's lead paragraph — everything up to the first blank line, which is what the inspector
 * shows before the operator asks for more (design record §3C, "collapsed to a lead paragraph").
 *
 * A blank line INSIDE a fenced block is content, not a paragraph break: cutting there would show
 * half a command and hide the half that failed, so fenced spans are skipped and a body that opens
 * on a fence leads with the whole block.
 */
export function leadParagraph(source: string): string {
  const text = source.trim();
  const spans = fenceSpans(text);
  const re = /\n[ \t]*\n/g;
  let match = re.exec(text);
  while (match !== null) {
    const at = match.index;
    // `close`, not `end`: the newline that terminates the CLOSING fence line sits inside the
    // span but is a real paragraph break, so bounding on the block's content ends the lead there.
    if (!spans.some((span) => at >= span.start && at < span.close)) return text.slice(0, at).trim();
    match = re.exec(text);
  }
  return text;
}

// --- slice 3: the live playhead, the failed run's jump, and the attempt relay ----------------
//
// STUDIO-744. Three states the completed-run hero of slice 2 does not cover (design record §3,
// §4, §9 slice 3): a run still streaming, a run that died, and a ticket whose work relayed across
// more than one run.

/** The one outcome that means "still going" (`crates/store/src/types.rs`'s `OUTCOME_RUNNING`). */
export const OUTCOME_RUNNING = "running";

/**
 * The issue-history row with the 2s run-detail poll's fresher telemetry folded in.
 *
 * `GET /api/v1/issues/{id}/history` is fetched once and cached for 10s, which is the right cadence
 * for the ATTEMPT LIST and the wrong one for the attempt being watched: a live run's turns, tokens
 * and — the one that matters — its terminal outcome all move while the page is open. `GET
 * /api/v1/runs/{id}` is the endpoint that already answers that at 2s, so the header reads the row
 * for identity and the poll for telemetry rather than a second history fetch.
 *
 * Two guards, both about not asserting something the caller did not ask for. The detail must be
 * THIS run's — react-query hands back the previous key's data for one render on a key change, and
 * an overlay of run 522's turn count onto run 547 is a number nobody served. And a row that has
 * already ENDED is returned untouched: the live snapshot outlives the run by a poll interval or
 * two (`run_detail_from_running` reports `outcome: running` for anything still in the map), and
 * resurrecting a finished run would put the header back into Stop-and-streaming.
 */
export function liveRunRow(row: RunSummary, detail: RunDetail | undefined): RunSummary {
  if (detail === undefined || detail.run_id !== row.id) return row;
  if (row.outcome !== "" && row.outcome !== OUTCOME_RUNNING) return row;
  return {
    ...row,
    outcome: detail.outcome,
    ended_at: detail.ended_at,
    error: detail.error,
    turns: detail.turn_count,
    input_tokens: detail.input_tokens,
    output_tokens: detail.output_tokens,
    total_tokens: detail.total_tokens,
    usage_estimated: detail.usage_estimated,
  };
}

/**
 * The phase the playhead sits on: the NEWEST, which is the one a streaming run is writing into.
 *
 * The spine is in transcript order, so this is simply the last — but it is named, because it is
 * the whole difference between a live run and a finished one. A finished run opens on its first
 * phase (you read a trace forwards); a live run opens on its last and moves with it.
 */
export function playheadPhase(phases: readonly TracePhase[]): TracePhase | undefined {
  return phases.length === 0 ? undefined : phases[phases.length - 1];
}

/** Where the Result card's "jump to failing step →" lands (design record §3B). */
export interface FailingStep {
  phaseId: string;
  /** The `seq` of the first failing call in that phase, to auto-expand; null when it has none. */
  cardSeq: number | null;
}

/**
 * The first failing step of a trace, or `null` when nothing failed.
 *
 * FIRST, not last: a failure cascades — a failed build fails the test run after it — and the step
 * an operator has to read is the one that broke, not the last thing that noticed. The phase can be
 * failed with no failing CALL (the model raises `failed` from an error side effect too), which is
 * still a step worth jumping to; only the auto-expand has nothing to open.
 */
export function failingStep(phases: readonly TracePhase[]): FailingStep | null {
  const phase = phases.find((p) => p.failed);
  if (phase === undefined) return null;
  return { phaseId: phase.id, cardSeq: phase.did.find((card) => card.failed)?.seq ?? null };
}

/** The `pr:` prefix every ticketless review run's key carries (`crates/orchestrator/src/review.rs`). */
const REVIEW_KEY_PREFIX = "pr:";

/**
 * The teammate a run wore, as far as a SERVED field can say — "" when nothing can.
 *
 * Two sources, and deliberately no third. A ticketless review run's key IS
 * `pr:owner/repo#<n>@<reviewer>` (`crates/orchestrator/src/review.rs`), so the reviewer's identity
 * is carried by the run's own identifier and needs nothing else. Every other run falls back to the
 * ticket's DURABLE assignee (STUDIO-735), which is per-TICKET rather than per-run.
 *
 * That fallback is the honest limit of this slice: `runs` has no identity column — the routing
 * decision lives in a `teams.route` EVENT (`crates/store/src/types.rs` says so in as many words)
 * — and the one endpoint that resolves it, `/api/v1/history/issues`, answers per issue and not per
 * attempt. So two attempts of one ticket resolve to the same name today, and [`relayBatons`] says
 * "run 522 → run 547" rather than inventing a second teammate. Slice 5 (STUDIO-746) wires the
 * per-run identity in, and this is the only function it has to change.
 */
export function runTeammate(run: RunSummary, assignee: string): string {
  const key = run.issue_identifier.trim();
  if (key.startsWith(REVIEW_KEY_PREFIX)) {
    return key.slice(key.lastIndexOf("@") + 1).trim();
  }
  return assignee.trim();
}

/** One handoff baton — the relay between two of a ticket's runs (design record §3C/§6). */
export interface Baton {
  /** The teammate who handed off; "" when none resolves. */
  from: string;
  /** The teammate who picked up; "" when none resolves. */
  to: string;
  /** What the spine row reads: "alice → jimmy", or the run relay when one identity covers both. */
  text: string;
}

/** The batons either side of the attempt being read. */
export interface RelayBatons {
  /** Handed TO this run by the attempt before it; null when this is the ticket's first run. */
  incoming: Baton | null;
  /** Handed OUT of this run to the attempt after it; null when this is the newest. */
  outgoing: Baton | null;
}

/**
 * The relay markers the spine draws around one attempt.
 *
 * `runs` is newest-first, as `runsNewestFirst` orders the attempt selector, so a run's predecessor
 * is the element AFTER it. A run the list does not contain gets no baton at all rather than a
 * neighbour picked by position — the selector can hold a run the history has since re-paged out.
 */
export function relayBatons(
  runs: readonly RunSummary[],
  run: RunSummary,
  assignee: string,
): RelayBatons {
  const at = runs.findIndex((r) => r.id === run.id);
  if (at < 0) return { incoming: null, outgoing: null };
  const previous = runs[at + 1];
  const next = runs[at - 1];
  return {
    incoming: previous === undefined ? null : baton(previous, run, assignee),
    outgoing: next === undefined ? null : baton(run, next, assignee),
  };
}

function baton(from: RunSummary, to: RunSummary, assignee: string): Baton {
  const who = runTeammate(from, assignee);
  const took = runTeammate(to, assignee);
  if (who !== "" && took !== "" && who !== took) {
    return { from: who, to: took, text: `${who} → ${took}` };
  }
  // One identity, or none: naming it twice would read as a teammate handing to herself, which is
  // not what happened — the ticket relayed across two RUNS. So the runs are what the row names.
  const relay = `run ${from.id} → run ${to.id}`;
  const only = who === "" ? took : who;
  return { from: who, to: took, text: only === "" ? relay : `${only} · ${relay}` };
}
