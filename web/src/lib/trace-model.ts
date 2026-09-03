import type { LogEntry, RunSummary } from "@/lib/api";
import { fenceSpans, inlineText } from "@/lib/markdown";

// trace-model — the pure model behind the console's "Trace" run detail (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §4; slice 1 of its §9 plan). It turns the flat
// `LogEntry[]` that `GET /api/v1/runs/<id>/transcript` serves into the three things the slice-2
// view renders: phases with plain-language titles, a DID/SAID split inside each phase, and the
// Result card's headline + sectioned body. No view, no network — everything here is a pure
// function, which is this codebase's discipline for anything worth asserting on directly (see
// room-model, runs-model, console-job-detail).
//
// WHY EVERY RULE BELOW IS A HEURISTIC (design record §5's "honest caveat"). A `LogEntry` is
// `{seq, kind, tool, text}` and nothing else: the daemon serves no structured tool arguments, no
// exit codes and no file paths. `text` is whatever `crates/agent/src/humanize.rs` rendered, which
// for a `tool_use` is the tool's input object as SORTED `key=value` pairs with each value clipped
// to 60 runes. That is much more than free prose and much less than structured data, so the rules
// here are best-effort BY CONSTRUCTION and the view must keep the raw-transcript escape hatch the
// design record calls mandatory. Precise labels want the daemon emitting structured tool metadata
// (design record §5/§8 — a nice-to-have, not a blocker for this slice).
//
// The tables are not guesses: they were measured over the 784 real transcripts in
// `~/.rhapsody/logs` (~77k tool calls). The two findings that shape the whole module are called
// out at their rules — `Bash` is 78% of all calls, and room/memory tools arrive MCP-prefixed.

/** The plain-language phases the spine renders (design record §3/§4). */
export type PhaseKind =
  | "oriented"
  | "implemented"
  | "verified"
  | "coordinated"
  | "handoff"
  | "other";

/** The per-phase chips (design record §4, "Side effects"). */
export type SideEffectKind = "edited" | "room" | "memory" | "error";

export interface SideEffect {
  kind: SideEffectKind;
  /** "edited 4 files", "posted to room", "retained 1 fact", "error". */
  label: string;
}

/** One DID: a `tool_use` with its `tool_result` folded on, as the prototype's call-cards render. */
export interface DidCard {
  seq: number;
  /** The tool name VERBATIM, as served (`mcp__symphony__teams_post`). */
  tool: string;
  kind: PhaseKind;
  /** The humanized target — a path, a command, a pattern; falls back to the whole summary. */
  target: string;
  /** The parsed `key=value` summary; `{}` when it did not parse as one. */
  args: Record<string, string>;
  /** The folded result text; "" when the call has none (still in flight, or truncated). */
  result: string;
  /** The `seq` of the entry the result came from; null when unpaired. */
  resultSeq: number | null;
  failed: boolean;
}

/** One SAID: agent prose, rendered as markdown by the view (muted/collapsed per §2). */
export interface SaidBlock {
  seq: number;
  kind: "thinking" | "text";
  text: string;
}

export interface TracePhase {
  /** Stable within one model — the seq of the phase's first entry. */
  id: string;
  kind: PhaseKind;
  /** "Oriented", "Implemented", … */
  title: string;
  /** "read 2 files", "cargo test --workspace", "posted to room". */
  subtitle: string;
  /** 0-based index of the turn this phase belongs to. */
  turn: number;
  did: DidCard[];
  said: SaidBlock[];
  effects: SideEffect[];
  failed: boolean;
  /** Results with no call to fold onto (a truncated transcript) — surfaced, never dropped. */
  orphanResults: string[];
}

/**
 * What actually drove the split, so the view can be honest about it:
 * `turns` — the stream carried `event` dividers and they were used as phase boundaries;
 * `clusters` — no dividers at all, so the phases came from tool clustering alone;
 * `single` — too little to group, whatever the markers said. Never "flat": even `single` is one
 * phase, which is the design record's floor of "never worse than today's flat list".
 *
 * `turns` says the dividers were HONOURED, not that they split the run into several turns — the
 * common real shape is one `session started` and one `turn completed` either side of the work,
 * which is a divider that was read and used and happened to have nothing to separate.
 */
export type TraceGrouping = "turns" | "clusters" | "single";

export interface TraceModel {
  phases: TracePhase[];
  grouping: TraceGrouping;
  /** Every `event` entry's label, in order — the raw dividers, for the view's turn ruler. */
  events: string[];
}

const PHASE_TITLES: Record<PhaseKind, string> = {
  oriented: "Oriented",
  implemented: "Implemented",
  verified: "Verified",
  coordinated: "Coordinated",
  handoff: "Handed off",
  other: "Worked",
};

/**
 * A phase kind's plain-language title, for a caller that has a KIND but no phase to read it from —
 * the Jobs sparkline's reserved slots (STUDIO-743), which name a kind the run never reached.
 * The same table [`TracePhase.title`] is built from, so the two vocabularies cannot drift.
 */
export function phaseTitle(kind: PhaseKind): string {
  return PHASE_TITLES[kind];
}

// --- Tool classification ---------------------------------------------------------------------

/**
 * The tool name with any `mcp__<server>__` prefix removed.
 *
 * The daemon passes the tool name through verbatim (`humanize.rs` copies the block's `name`), so
 * the room and memory tools arrive as `mcp__symphony__teams_post`, never as `teams_post`. A
 * classifier written against the bare names in the design record would silently never fire on a
 * real transcript — which is why this exists rather than a direct `Set` lookup. The server segment
 * is matched lazily because server names themselves contain underscores
 * (`mcp__claude_ai_Linear__save_comment`).
 */
export function baseToolName(tool: string): string {
  const mcp = /^mcp__.+?__(.+)$/.exec(tool);
  return mcp === null ? tool : mcp[1];
}

const ORIENT_TOOLS = new Set([
  "Read",
  "Grep",
  "Glob",
  "LS",
  "NotebookRead",
  "WebFetch",
  "WebSearch",
  "ToolSearch",
  "ListAgents",
  "Explore",
  "TaskOutput",
  // The daemon's own read-mostly MCP facade — an agent looking up its run, ticket or the roster.
  "symphony_runs",
  "symphony_run",
  "symphony_run_status",
  "symphony_ticket",
  "symphony_state",
  "symphony_events",
  "symphony_logs",
  "rhapsody_runs",
  "rhapsody_run",
  "rhapsody_run_status",
  "rhapsody_ticket",
  "rhapsody_state",
  "rhapsody_events",
  "rhapsody_logs",
  "teams_room_read",
  "teams_roster",
]);
const EDIT_TOOLS = new Set(["Edit", "MultiEdit", "Write", "NotebookEdit"]);
// Communication with people or teammates. `teams_retain` is memory rather than talk, but the
// design record's §4 maps it to Coordinated alongside `teams_post`, and the record wins.
const COORDINATE_TOOLS = new Set([
  "teams_post",
  "teams_retain",
  "teams_invalidate",
  "teams_reinstate",
  "teams_recall",
  "SendMessage",
  "agent_send_message",
  "symphony_send_message",
  "save_comment",
  "save_issue",
  "save_document",
  "create_comment",
  "create_issue",
]);
const HANDOFF_TOOLS = new Set(["symphony_handoff", "rhapsody_handoff"]);

/** The phase a single tool call belongs to. `text` is consulted for `Bash`, which needs its verb. */
export function toolPhaseKind(tool: string, text: string): PhaseKind {
  const base = baseToolName(tool);
  if (HANDOFF_TOOLS.has(base)) return "handoff";
  if (EDIT_TOOLS.has(base)) return "implemented";
  if (COORDINATE_TOOLS.has(base)) return "coordinated";
  if (ORIENT_TOOLS.has(base)) return "oriented";
  if (base === "Bash" || base === "BashOutput") {
    const command = parseToolArgs(text).command ?? "";
    return command === "" ? "other" : bashPhaseKind(command);
  }
  return "other";
}

// Shell verbs that WRITE. Deliberately short: a write misread as a read only costs a phase title,
// but a read misread as a write invents an "edited" chip for something that never happened.
const WRITE_COMMANDS = new Set([
  "tee",
  "mkdir",
  "mv",
  "cp",
  "rm",
  "touch",
  "chmod",
  "patch",
  "install",
  "ln",
]);
const READ_COMMANDS = new Set([
  "cat",
  "head",
  "tail",
  "grep",
  "rg",
  "ag",
  "ls",
  "find",
  "fd",
  "jq",
  "wc",
  "awk",
  "tree",
  "which",
  "file",
  "diff",
  "curl",
  "sort",
  "uniq",
  "cut",
  "echo",
  "printf",
  "date",
  "env",
  "python3",
  "python",
  "node",
  "basename",
  "dirname",
  "realpath",
  "stat",
  "du",
  "df",
  "ps",
]);
// Test/build runners → Verified, keyed on the SUBCOMMAND where the tool has one.
//
// A `Map`, not an object literal: the key here is an arbitrary command name parsed out of agent
// output, and an object lookup answers `constructor`, `toString`, `__proto__` and the rest of
// `Object.prototype` with something that is not a `Set` — which threw out of `buildTrace` and took
// the whole run-detail view down rather than mis-titling one phase.
const RUNNER_SUBCOMMANDS = new Map<string, Set<string>>([
  ["cargo", new Set(["test", "build", "clippy", "fmt", "check", "bench", "nextest"])],
  ["go", new Set(["test", "build", "vet"])],
  ["npm", new Set(["test", "run"])],
  ["pnpm", new Set(["test", "run", "build", "lint"])],
  ["yarn", new Set(["test", "run", "build", "lint"])],
  ["npx", new Set(["vitest", "jest", "tsc", "eslint", "playwright", "prettier"])],
  ["bun", new Set(["test", "run"])],
  ["deno", new Set(["test", "lint", "check"])],
]);
// Runners with no subcommand worth inspecting — the whole invocation is a check.
const RUNNER_COMMANDS = new Set([
  "make",
  "task",
  "pytest",
  "vitest",
  "jest",
  "tsc",
  "eslint",
  "gradle",
  "mvn",
  "just",
  "gofmt",
  "golangci-lint",
]);
const GIT_WRITE = new Set([
  "commit",
  "push",
  "add",
  "checkout",
  "switch",
  "branch",
  "merge",
  "rebase",
  "apply",
  "restore",
  "reset",
  "tag",
  "stash",
  "worktree",
  "cherry-pick",
  "revert",
  "mv",
  "rm",
  "init",
]);
// Tools whose SUBCOMMAND decides whether the call inspected infrastructure or changed it.
const INFRA_TOOLS = new Set(["kubectl", "docker", "helm", "terraform", "pulumi", "flux", "argocd"]);
const GH_COORDINATE = new Set([
  "comment",
  "create",
  "review",
  "merge",
  "ready",
  "close",
  "edit",
  "reopen",
]);

/**
 * The phase a shell command belongs to.
 *
 * This carries most of the classification, because `Bash` is 78% of real tool calls (60,283 of
 * ~77k across `~/.rhapsody/logs`): agents read with `cat`/`sed`/`grep` and edit with `sed -i` far
 * more often than they reach for `Read`/`Edit`. Keying on the tool name alone would drop nearly a
 * whole run into one phase.
 *
 * Every segment of a compound command is classified and the most consequential wins, because the
 * single most common first word is `cd` (8,458 calls) — commands are written `cd /w && <verb>`, so
 * reading only the first word misclassifies almost everything.
 */
export function bashPhaseKind(command: string): PhaseKind {
  const kinds = splitSegments(command).map(segmentKind);
  for (const kind of ["implemented", "verified", "coordinated", "oriented"] as const) {
    if (kinds.includes(kind)) return kind;
  }
  return "other";
}

/** Splits a compound shell command on `&&`, `||`, `;` and `|` into its separate invocations. */
function splitSegments(command: string): string[] {
  return command
    .split(/\|\||&&|[;|]/)
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

// Shell grammar and wrappers that PRECEDE the real verb. Skipping them is not cosmetic: `for`,
// `do`, `until`, `export`, `set` and `read` opened 997 otherwise-unclassified segments in the
// corpus, because a loop body (`for f in *; do grep …; done`) splits into segments that start with
// the keyword rather than the command.
const SHELL_KEYWORDS = new Set([
  "for", "do", "done", "while", "until", "if", "then", "else", "elif", "fi", "case", "esac",
  "sudo", "export", "set", "read", "local", "declare", "eval", "exec", "time", "nohup", "xargs",
  "command", "builtin", "source", ".", "!", "{", "}", "(", ")", "[", "[[",
  // Wrappers that run another command: `timeout 30 cargo test` is a cargo call.
  "timeout", "bash", "sh", "zsh",
]);
// Cluster inspection vs cluster mutation — `kubectl` alone opened 488 unclassified segments.
const INFRA_READ = new Set([
  "get", "logs", "describe", "top", "explain", "version", "config", "ps", "images", "context",
  "diff", "history", "status", "list", "kustomize", "port-forward", "wait", "auth", "api-resources",
  "events", "plan", "show", "inspect",
]);
const INFRA_WRITE = new Set([
  "apply", "delete", "create", "patch", "rollout", "scale", "edit", "annotate", "label", "cp",
  "exec", "install", "upgrade", "uninstall", "run", "build", "push", "restart", "drain", "cordon",
]);

/**
 * A command's positional arguments — its subcommand chain — with flags removed.
 *
 * A long flag's VALUE is dropped along with it, because `kubectl --context prod get pods` otherwise
 * reads `prod` as the subcommand and the call falls through unclassified: `--context` alone hid 415
 * corpus segments. Only long flags consume a following token; a short flag does not, since `-q` and
 * friends are far more often bare (`cargo -q test`) and eating `test` would be the worse error.
 */
function subcommands(words: readonly string[]): string[] {
  const out: string[] = [];
  for (let i = 0; i < words.length; i += 1) {
    const word = words[i];
    if (!word.startsWith("-")) {
      out.push(word);
      continue;
    }
    if (word.startsWith("--") && !word.includes("=") && word !== "--") i += 1;
  }
  return out;
}

/** One shell invocation's phase, or null when it carries no signal (`cd`, an empty segment). */
function segmentKind(segment: string): PhaseKind | null {
  const words = segment.split(/\s+/).filter((w) => w !== "");
  let i = 0;
  // Step over leading env assignments (`FOO=bar cmd`), shell grammar and wrappers.
  while (
    i < words.length &&
    (/^[A-Za-z_][A-Za-z0-9_]*=/.test(words[i]) ||
      SHELL_KEYWORDS.has(words[i]) ||
      /^\d+[smh]?$/.test(words[i]) ||
      words[i].startsWith("-"))
  ) {
    i += 1;
  }
  if (i >= words.length) return null;
  const cmd = words[i].replace(/^.*\//, ""); // /usr/bin/grep -> grep
  const rest = words.slice(i + 1);
  const flagless = subcommands(rest);
  const sub = flagless[0] ?? "";
  if (cmd === "cd" || cmd === "") return null;
  // A command the humanizer clipped at 60 runes cannot be read past the cut — better unknown than
  // guessed from a fragment.
  if (cmd.endsWith("…")) return null;
  // `sed -i` edits in place; every other sed is a reader.
  if (cmd === "sed") return rest.some((w) => /^-[a-zA-Z]*i/.test(w)) ? "implemented" : "oriented";
  if (cmd === "git") return GIT_WRITE.has(sub) ? "implemented" : "oriented";
  if (cmd === "gh") return GH_COORDINATE.has(flagless[1] ?? "") ? "coordinated" : "oriented";
  if (WRITE_COMMANDS.has(cmd)) return "implemented";
  if (RUNNER_COMMANDS.has(cmd)) return "verified";
  const subs = RUNNER_SUBCOMMANDS.get(cmd);
  if (subs !== undefined) return subs.has(sub) ? "verified" : "oriented";
  if (READ_COMMANDS.has(cmd)) return "oriented";
  if (INFRA_TOOLS.has(cmd)) {
    // Scan ALL positionals for a known verb rather than trusting the first one. These tools are
    // written with interleaved short flags carrying values (`kubectl --context home -n flagsmith
    // get deploy`), and `-n` does not consume `flagsmith` above — so position 0 is the NAMESPACE,
    // not the verb. That single pattern accounted for the 287 kubectl calls still unclassified
    // after the long-flag fix.
    for (const word of flagless) {
      if (INFRA_WRITE.has(word)) return "implemented";
      if (INFRA_READ.has(word)) return "oriented";
    }
    return null;
  }
  return null;
}

// --- The humanizer's `key=value` summary -------------------------------------------------------

/**
 * Parses the `key=value` summary `humanize.rs::summarize_input` writes for a `tool_use`.
 *
 * The humanizer emits the input object's keys in SORTED order (Go's `sort.Strings`, mirrored by
 * Rust's `BTreeMap`) joined by single spaces, with each value collapsed to one line and clipped to
 * 60 runes. Values may therefore contain spaces AND `=`, so the only way to find the next key is a
 * `key=` token — and the sorted-order guarantee is what makes that safe: a candidate key is only
 * accepted when it sorts strictly after the previous one, which rejects the `=` inside a value
 * (`command=CGO_ENABLED=1 go test` keeps its whole command, since `CGO_ENABLED` sorts before
 * `command`). Still best-effort: a value containing a genuinely later-sorting `word=` splits early.
 *
 * Returns `{}` for text that is not a `key=value` summary at all (prose, or an empty input).
 */
export function parseToolArgs(text: string): Record<string, string> {
  const candidates: { key: string; at: number; valueAt: number }[] = [];
  const re = /(?:^|\s)([A-Za-z_][A-Za-z0-9_]*)=/g;
  let match = re.exec(text);
  while (match !== null) {
    candidates.push({ key: match[1], at: match.index, valueAt: match.index + match[0].length });
    match = re.exec(text);
  }
  const accepted: typeof candidates = [];
  for (const candidate of candidates) {
    // The first key must open the summary; later keys must sort strictly after the last accepted.
    if (accepted.length === 0) {
      if (candidate.at === 0) accepted.push(candidate);
      continue;
    }
    if (candidate.key > accepted[accepted.length - 1].key) accepted.push(candidate);
  }
  const args: Record<string, string> = {};
  accepted.forEach((candidate, idx) => {
    const end = idx + 1 < accepted.length ? accepted[idx + 1].at : text.length;
    args[candidate.key] = text.slice(candidate.valueAt, end).trim();
  });
  return args;
}

/** The argument keys that name a call's subject, in the order we prefer to show them. */
const TARGET_KEYS = [
  "command",
  "file_path",
  "path",
  "pattern",
  "url",
  "query",
  "body",
  "content",
  "notebook_path",
];

/** The one-line subject of a call — a path, a command, a pattern; the raw summary otherwise. */
function cardTarget(args: Record<string, string>, text: string): string {
  for (const key of TARGET_KEYS) {
    const value = args[key];
    if (value !== undefined && value !== "") return value;
  }
  return text;
}

// --- Failure detection -------------------------------------------------------------------------

/**
 * Whether a folded `tool_result` reports a failure.
 *
 * A `LogEntry` carries no `is_error`, so this reads the result text — but ONLY at anchored
 * markers, never as a substring search. The corpus is full of PASSING results that contain the
 * words "failed" and "errors": `test result: ok. 5 passed; 0 failed; 0 ignored` and
 * `18 problems (0 errors, 18 warnings)` are both successes, and a `/fail|error/` test would paint
 * an error chip on a green run. `humanize.rs` reduces a result to its FIRST non-empty line, so
 * these anchors are matched against exactly that.
 */
export function resultFailed(result: string): boolean {
  const text = result.trim();
  if (text === "") return false;
  const exit = /^Exit code (\d+)/.exec(text);
  if (exit !== null) return exit[1] !== "0";
  return (
    text.startsWith("<tool_use_error>") ||
    /^(FAIL|FAILED)\b/.test(text) ||
    /^-{2,}\s*FAIL:/.test(text) ||
    /^error(\[[A-Za-z]\d+\])?:/i.test(text) ||
    /\bpanicked at\b/.test(text) ||
    /\bcommand not found\b/.test(text) ||
    /^Traceback \(most recent call last\)/.test(text)
  );
}

/** Whether an `event` divider reports a failed turn (`humanize.rs` writes "turn failed[: …]"). */
function eventFailed(text: string): boolean {
  return /^turn failed\b/.test(text.trim());
}

// --- Phase grouping ----------------------------------------------------------------------------

interface OpenPhase {
  kind: PhaseKind;
  turn: number;
  id: string;
  did: DidCard[];
  said: SaidBlock[];
  orphanResults: string[];
  failed: boolean;
}

/** One transcript entry after pairing, before grouping — the hand-off between the two passes. */
type TraceItem =
  | { type: "did"; card: DidCard }
  | { type: "said"; block: SaidBlock }
  | { type: "event"; text: string }
  | { type: "orphan"; seq: number; text: string };

/**
 * Pass one: turn entries into items, folding each `tool_result` onto the call that produced it.
 *
 * Pairing is FIFO over EVERY call still awaiting a result, deliberately not per phase. An agent
 * batches parallel calls of different kinds — a `Read` and a `Bash` in one message — whose results
 * both arrive afterwards; those two calls land in different phases, so a per-phase queue folds the
 * first result onto the second call. That failure is silent, because a wrong result still reads
 * like a result.
 */
function pairEntries(entries: readonly LogEntry[]): TraceItem[] {
  const items: TraceItem[] = [];
  const awaiting: DidCard[] = [];
  for (const entry of entries) {
    switch (entry.kind) {
      case "event":
        items.push({ type: "event", text: entry.text });
        break;
      case "tool_use": {
        const args = parseToolArgs(entry.text);
        const card: DidCard = {
          seq: entry.seq,
          tool: entry.tool,
          kind: toolPhaseKind(entry.tool, entry.text),
          target: cardTarget(args, entry.text),
          args,
          result: "",
          resultSeq: null,
          failed: false,
        };
        awaiting.push(card);
        items.push({ type: "did", card });
        break;
      }
      case "tool_result": {
        const card = awaiting.shift();
        if (card === undefined) {
          // No call to fold onto (a truncated transcript) — surfaced, never dropped.
          items.push({ type: "orphan", seq: entry.seq, text: entry.text });
          break;
        }
        card.result = entry.text;
        card.resultSeq = entry.seq;
        card.failed = resultFailed(entry.text);
        break;
      }
      default:
        items.push({ type: "said", block: { seq: entry.seq, kind: entry.kind, text: entry.text } });
    }
  }
  return items;
}

/**
 * Groups a run's transcript into phases (design record §4).
 *
 * Turns split at `event` dividers, and within a turn consecutive same-kind tool calls cluster into
 * one phase. Both halves matter, and the second more than the design record implies: over 784 real
 * transcripts the ONLY dividers the daemon emits are "session started", "turn completed" and
 * "turn failed", roughly one of each per session — so most runs carry no usable turn structure and
 * tool-cluster grouping is the ORDINARY path, not the fallback.
 *
 * Prose never breaks a cluster and never forms a phase of its own while work follows it: SAID is
 * buffered and flushed into the phase of the next tool call, because an agent thinks and THEN acts
 * — a thought that introduces a cluster belongs to it, and a thought between two Reads keeps one
 * "Oriented" phase rather than splitting it in three. Prose with no call after it (a run that ends
 * on its hand-off summary) flushes into the phase in progress, or becomes one when there is none.
 */
export function buildTrace(entries: readonly LogEntry[]): TraceModel {
  const phases: TracePhase[] = [];
  const events: string[] = [];
  let open: OpenPhase | null = null;
  let pendingSaid: SaidBlock[] = [];
  let turn = 0;
  let turnHasPhase = false;

  const flushSaid = (into: OpenPhase) => {
    into.said.push(...pendingSaid);
    pendingSaid = [];
  };

  /** Ends the phase in progress, leaving any buffered prose for the phase that comes next. */
  const endPhase = () => {
    if (open !== null) {
      phases.push(finishPhase(open));
      open = null;
    }
  };

  /**
   * A real boundary — a turn divider or the end of the stream. Buffered prose has no call left to
   * claim it, so it settles here: onto the phase in progress, or into one of its own.
   */
  const close = () => {
    if (pendingSaid.length > 0) {
      if (open === null) {
        open = newPhase("other", turn, pendingSaid[0].seq);
        turnHasPhase = true;
      }
      flushSaid(open);
    }
    endPhase();
  };

  // Pass two: group the paired items. Every card's result is already known here, so a phase's
  // failed flag is settled before the phase is finished — which a single pass could not do for a
  // result that arrives after its phase has closed.
  for (const item of pairEntries(entries)) {
    if (item.type === "event") {
      events.push(item.text);
      // A failing turn marks the phase it closes: that is the step the operator must jump to.
      if (eventFailed(item.text) && open !== null) open.failed = true;
      close();
      if (turnHasPhase) {
        turn += 1;
        turnHasPhase = false;
      }
      continue;
    }

    if (item.type === "orphan") {
      if (open === null) {
        open = newPhase("other", turn, item.seq);
        turnHasPhase = true;
      }
      open.orphanResults.push(item.text);
      continue;
    }

    if (item.type === "did") {
      if (open === null || open.kind !== item.card.kind) {
        // `endPhase`, not `close`: the prose buffered just before this call introduces it, so it
        // belongs to the phase about to open rather than to the one that is ending.
        endPhase();
        open = newPhase(item.card.kind, turn, item.card.seq);
        turnHasPhase = true;
      }
      flushSaid(open);
      open.did.push(item.card);
      if (item.card.failed) open.failed = true;
      continue;
    }

    // thinking / text — SAID. Buffered until the next call claims it (see the doc comment).
    pendingSaid.push(item.block);
  }
  close();

  const grouping: TraceGrouping =
    phases.length < 2 ? "single" : events.length > 0 ? "turns" : "clusters";
  return { phases, grouping, events };
}

function newPhase(kind: PhaseKind, turn: number, seq: number): OpenPhase {
  return { kind, turn, id: `p${seq}`, did: [], said: [], orphanResults: [], failed: false };
}

function finishPhase(open: OpenPhase): TracePhase {
  return {
    id: open.id,
    kind: open.kind,
    title: PHASE_TITLES[open.kind],
    subtitle: phaseSubtitle(open),
    turn: open.turn,
    did: open.did,
    said: open.said,
    effects: phaseEffects(open),
    failed: open.failed,
    orphanResults: open.orphanResults,
  };
}

// --- Side effects ------------------------------------------------------------------------------

/**
 * The distinct files a phase's edit calls touched.
 *
 * A write whose `file_path` did not survive the humanizer's clipping still counts as one edit, so
 * an unnamed write is never silently lost from the count — but ONLY for a real edit tool. A `Bash`
 * card classified `implemented` never carries a path (`git push`, `mkdir`, `tee test.log`), so
 * counting it the same way does not recover a clipped name, it invents a file: over the 435 real
 * transcripts in `~/.rhapsody/logs` that fabricated 2,448 files across 1,759 of the 3,057 phases
 * showing this chip. A shell write is left out of the count entirely — the phase title still says
 * Implemented, which is the acknowledged §5 cost of a fuzzy label, but the number stays true.
 */
function editedFiles(did: readonly DidCard[]): string[] {
  const files = new Set<string>();
  let unnamed = 0;
  for (const card of did) {
    if (card.kind !== "implemented") continue;
    const path = card.args.file_path ?? card.args.path ?? "";
    if (path !== "") files.add(path);
    else if (EDIT_TOOLS.has(baseToolName(card.tool))) unnamed += 1;
  }
  return [...files, ...Array.from({ length: unnamed }, (_, i) => `#${i}`)];
}

function countTool(did: readonly DidCard[], name: string): number {
  return did.filter((card) => baseToolName(card.tool) === name).length;
}

/**
 * The phase's chips (design record §4). Heuristic and best-effort like everything else here: the
 * edit count is DISTINCT parsed paths, so two edits to one file read as one file.
 */
function phaseEffects(open: OpenPhase): SideEffect[] {
  const effects: SideEffect[] = [];
  const edited = editedFiles(open.did).length;
  if (edited > 0) {
    effects.push({ kind: "edited", label: `edited ${edited} ${plural(edited, "file")}` });
  }
  const posts = countTool(open.did, "teams_post");
  if (posts > 0) {
    effects.push({ kind: "room", label: posts === 1 ? "posted to room" : `${posts} room posts` });
  }
  const retained = countTool(open.did, "teams_retain");
  if (retained > 0) {
    effects.push({ kind: "memory", label: `retained ${retained} ${plural(retained, "fact")}` });
  }
  if (open.failed) effects.push({ kind: "error", label: "error" });
  return effects;
}

function plural(n: number, word: string): string {
  return n === 1 ? word : `${word}s`;
}

/** The phase's one-line summary under its title. */
function phaseSubtitle(open: OpenPhase): string {
  const { did, kind } = open;
  if (did.length === 0) return open.said.length > 0 ? "notes" : "";
  switch (kind) {
    case "oriented": {
      const reads = did.filter((card) => {
        const base = baseToolName(card.tool);
        return base === "Read" || base === "NotebookRead";
      }).length;
      return reads > 0
        ? `read ${reads} ${plural(reads, "file")}`
        : `${did.length} ${plural(did.length, "look")}`;
    }
    case "implemented": {
      const edited = editedFiles(did).length;
      // Zero means the phase's writes were all shell commands, which name no file — show what ran
      // instead of an "edited 0 files" that reads as a claim about the work.
      if (edited === 0) return did[0].target;
      return `edited ${edited} ${plural(edited, "file")}`;
    }
    case "verified":
      return did[0].target;
    case "coordinated": {
      const parts: string[] = [];
      const posts = countTool(did, "teams_post");
      const retained = countTool(did, "teams_retain");
      if (posts > 0) parts.push("posted to room");
      if (retained > 0) parts.push(`retained ${retained} ${plural(retained, "fact")}`);
      return parts.length > 0 ? parts.join(" · ") : did[0].target;
    }
    case "handoff":
      return "handed off";
    default:
      return `${did.length} ${plural(did.length, "call")}`;
  }
}

// --- The Result card ---------------------------------------------------------------------------

/** The three labelled sub-blocks §2 asks for, plus the catch-all for a heading that fits none. */
export type ResultSectionLabel = "What changed" | "How verified" | "Follow-ups" | "Notes";

export interface ResultSection {
  label: ResultSectionLabel;
  /** The author's OWN heading, kept verbatim so the card can show what they actually wrote. */
  heading: string;
  /** The section's markdown source, for the view to render with the STUDIO-739 renderer. */
  body: string;
}

export interface ResultCard {
  /** A verb phrase, markdown stripped — the card's H1. Never empty. */
  headline: string;
  /** Markdown before the first heading. */
  lead: string;
  sections: ResultSection[];
  /**
   * Where the card came from: `handoff` — the trailing prose, with a handoff tool call after it;
   * `text` — trailing prose with no such call; `fallback` — no usable prose, headline synthesized.
   */
  source: "handoff" | "text" | "fallback";
}

// The heading families runs actually write. Measured over the 784 transcripts in `~/.rhapsody/logs`:
// 499 DISTINCT trailing-prose headings, of which "verification" (207), "what shipped" (148),
// "what landed" (49), "what i did" (48), "what changed" (44) and "what i built" (34) lead. Matching
// the three literal display labels would file almost every real handoff under Notes.
const SECTION_PATTERNS: { label: ResultSectionLabel; re: RegExp }[] = [
  {
    label: "Follow-ups",
    re: /follow[-\s]?ups?|next steps?|remaining|left (undone|out|to do)|not fixed|flag(ged)?\b|todo|deferred|open questions?|out of scope|carried|handoff (notes|steps)|worth (your attention|flagging|knowing)|to flag|you should know|to know/i,
  },
  {
    label: "How verified",
    re: /verif|\btests?\b|evidence|how (i )?(verified|tested)|proof|\bci\b|\bgreen\b|\bchecks?\b|\blint\b/i,
  },
  {
    label: "What changed",
    re: /what (i )?(shipped|landed|changed|did|built|wrote|added|fixed)|\bchanges?\b|\bshipped\b|\blanded\b|\bbuilt\b|\bfix(ed|es)?\b|implementation|the change\b|root cause|what was wrong|what happened/i,
  },
];

/**
 * Which labelled sub-block a heading belongs to. Follow-ups is tested FIRST on purpose: a heading
 * like "Follow-ups — tests still to write" mentions tests but is not the verification block.
 */
export function sectionLabel(heading: string): ResultSectionLabel {
  const text = heading.trim().replace(/[:.]+$/, "");
  for (const { label, re } of SECTION_PATTERNS) {
    if (re.test(text)) return label;
  }
  return "Notes";
}

/**
 * Builds the Result card (design record §2) from the run's trailing prose.
 *
 * The design record's §8 notes there is no conventional handoff marker to key on, so the source is
 * the LAST non-empty `text` entry — in practice a run's closing summary.
 *
 * `source` distinguishes a run that HANDED OFF from one that merely stopped talking, and the test
 * for it is a `symphony_handoff` call ANYWHERE in the run, not one after the prose: agents call the
 * handoff tool and then write their closing message, so requiring it to follow the prose recognised
 * 1 of 351 real runs. Anywhere in the run recognises them all.
 */
export function buildResult(entries: readonly LogEntry[], run?: RunSummary): ResultCard {
  let prose = "";
  entries.forEach((entry) => {
    if (entry.kind === "text" && entry.text.trim() !== "") prose = entry.text;
  });
  if (prose.trim() === "") {
    return { headline: fallbackHeadline(run), lead: "", sections: [], source: "fallback" };
  }
  const handedOff = entries.some(
    (entry) => entry.kind === "tool_use" && HANDOFF_TOOLS.has(baseToolName(entry.tool)),
  );
  const { lead, sections } = splitSections(prose);
  const headline = extractHeadline(prose);
  return {
    headline: headline === "" ? fallbackHeadline(run) : headline,
    lead,
    sections,
    source: handedOff ? "handoff" : "text",
  };
}

/** Splits markdown at its ATX headings, ignoring any that sit inside a fenced code block. */
function splitSections(source: string): { lead: string; sections: ResultSection[] } {
  const masked = maskFences(source);
  const heads: { at: number; end: number; text: string }[] = [];
  const re = /^ {0,3}#{1,6}[ \t]+(.+?)[ \t]*#*[ \t]*$/gm;
  let match = re.exec(masked);
  while (match !== null) {
    heads.push({ at: match.index, end: match.index + match[0].length, text: match[1].trim() });
    match = re.exec(masked);
  }
  if (heads.length === 0) return { lead: source.trim(), sections: [] };
  const lead = source.slice(0, heads[0].at).trim();
  const sections = heads.map((head, idx) => {
    const end = idx + 1 < heads.length ? heads[idx + 1].at : source.length;
    return {
      label: sectionLabel(head.text),
      heading: head.text,
      body: source.slice(head.end, end).trim(),
    };
  });
  // A hand-off that OPENS with an unclassified heading is titling itself, not opening a section:
  // 543 of the 554 real handoffs carrying headings start with a lead paragraph instead, and the 11
  // that do not head themselves "STUDIO-354 — complete" / "Done". Left as a section, that title's
  // body renders as a "Notes" block repeating the headline, so it becomes the lead instead.
  if (lead === "" && sections.length > 0 && sections[0].label === "Notes") {
    return { lead: sections[0].body, sections: sections.slice(1) };
  }
  return { lead, sections };
}

/**
 * Blanks out every fenced code block, preserving offsets and line structure, so a `## Verification`
 * written INSIDE a fence cannot split the card. Offsets survive because the caller slices the
 * ORIGINAL source with indexes found in the mask.
 */
function maskFences(source: string): string {
  let masked = source;
  for (const span of fenceSpans(source)) {
    const blanked = source.slice(span.start, span.end).replace(/[^\n]/g, " ");
    masked = masked.slice(0, span.start) + blanked + masked.slice(span.end);
  }
  return masked;
}

/** A headline shorter than this reads as a stub ("Done."), so the next sentence is pulled in. */
const MIN_HEADLINE = 24;
const MAX_HEADLINE = 160;

/**
 * The card's H1: the first real sentence of the prose, markdown stripped. "" when there is none.
 *
 * Scanned LINE by line rather than paragraph by paragraph, because a hand-off routinely opens
 * `## Handoff` with its first real sentence on the very next line — skipping the whole paragraph
 * would throw that sentence away and fall through to the synthesized fallback.
 */
function extractHeadline(source: string): string {
  // The opening paragraph, and the one after it. A paragraph ends at the first blank line, heading
  // or list item — a bullet is a new thought, and running it onto the sentence makes an H1 out of
  // two unrelated clauses. Blanks, headings, quotes and a fenced block's blanked remains are
  // skipped between paragraphs, never joined into one.
  const paragraphs: string[][] = [];
  let current: string[] = [];
  for (const raw of maskFences(source).split("\n")) {
    const line = raw.trim();
    if (line === "" || /^#{1,6}\s/.test(line) || /^([-*+>]|\d+[.)])\s/.test(line)) {
      if (current.length > 0) {
        paragraphs.push(current);
        current = [];
        if (paragraphs.length === 2) break;
      }
      continue;
    }
    current.push(line);
  }
  if (current.length > 0 && paragraphs.length < 2) paragraphs.push(current);

  const text = paragraphs.length === 0 ? "" : stripInlineMarkdown(paragraphs[0].join(" "));
  if (text === "") return "";
  let headline = growSentence(text);
  // `growSentence` can only reach a second sentence WITHIN the opening paragraph, and the corpus
  // writes the stub as its own paragraph ("Done." on its own line, then a heading), so the floor
  // was silently unmet for 6 of 435 real runs — two of them an H1 reading literally "Done." over a
  // 2,500-char body. Still under it, the next paragraph's opening sentence comes along.
  if (headline.length < MIN_HEADLINE && paragraphs.length > 1) {
    const next = growSentence(stripInlineMarkdown(paragraphs[1].join(" ")));
    if (next !== "") headline = `${headline} ${next}`;
  }
  return clip(headline, MAX_HEADLINE);
}

/**
 * The leading sentence, extended until it carries something. The corpus opens with a bare "Done."
 * or "Shipped it." constantly, and a two-word H1 tells the operator nothing.
 */
function growSentence(text: string): string {
  const parts = text.split(/(?<=[.!?])\s+/);
  let out = "";
  for (const part of parts) {
    out = out === "" ? part : `${out} ${part}`;
    if (out.length >= MIN_HEADLINE) break;
  }
  return out.trim();
}

/**
 * Removes the inline markdown a heading must not show: emphasis, code ticks, link syntax.
 *
 * Delegated to the STUDIO-739 renderer's own inline parse, which renders the card BODY directly
 * under this H1 — a second, plainer stripping pass here made the same identifier read two ways in
 * one card, because it took an underscore inside a word for emphasis
 * (`load_live_review_watch` -> `loadlivereview_watch`, and backticks did not protect it). An image
 * is spelled as its link first: a heading cannot show the image, but its alt text is prose.
 */
function stripInlineMarkdown(text: string): string {
  return inlineText(text.replace(/!\[/g, "["))
    .replace(/\s+/g, " ")
    .trim();
}

function clip(text: string, max: number): string {
  if (text.length <= max) return text;
  const cut = text.slice(0, max - 1);
  const space = cut.lastIndexOf(" ");
  return `${(space > max / 2 ? cut.slice(0, space) : cut).trimEnd()}…`;
}

/**
 * The headline for a run that wrote no usable prose — a crash, a stop, or a run still going. The
 * acceptance is that this is never empty: an empty Result card would read as "nothing happened".
 *
 * It names the ENDING and not the run's `error` string, which has its own home: the Result card's
 * §3B banner renders that string for every failed or stopped run, whether or not there was prose,
 * so repeating it here printed the same sentence twice in adjacent lines of the same card.
 */
function fallbackHeadline(run?: RunSummary): string {
  if (run === undefined) return "No hand-off recorded.";
  switch (run.outcome) {
    case "failed":
      return "The run failed before handing off.";
    case "running":
    case "continued":
      return "Still running — no hand-off yet.";
    case "stopped":
      return "Stopped before handing off.";
    case "interrupted":
      return "Interrupted before handing off.";
    case "completed":
      return "Completed without a written hand-off.";
    default:
      return "No hand-off recorded.";
  }
}
