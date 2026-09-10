import type { RunCheck } from "@/lib/api";

// console-diff — the model behind the run-detail Diff tab (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §5, §9 slice 7).
//
// The daemon serves one string: the pull request's unified diff, exactly as `gh pr diff` printed
// it. Turning that into something colorized and scannable is parsing, and parsing belongs in a
// module a test can assert on directly rather than in a render — the same argument
// `console-trace-view` and `console-watch` are split out for, and a sharper one here, because
// every rule below is a rule about a TEXT FORMAT and the failure mode is a wrong colour on a line
// nobody double-checks.
//
// Nothing here decides anything. Whether there IS a diff, which pull request it belongs to and
// whether the daemon could read it are the daemon's answers, made before this module sees a
// character (`rundiff`), and a console that re-derived them would be guessing at state it does not
// hold.

/** How one line of a unified diff should be read — and, by the view, coloured. */
export type DiffLineKind = "add" | "del" | "context" | "hunk" | "meta";

export interface DiffLine {
  kind: DiffLineKind;
  /** The line verbatim, INCLUDING its leading `+`/`-`/space — the view renders it as `gh` wrote it. */
  text: string;
}

/** One file's worth of a patch: its post-image path, and its lines already classified. */
export interface DiffFile {
  /**
   * The file as it exists AFTER the change (git's `b/` side), or `""` for a patch fragment with no
   * `diff --git` header at all. The post-image is the name an operator is looking for, and it is
   * the one that is right on a rename.
   */
  path: string;
  lines: DiffLine[];
}

/** What a patch changed, in the three numbers a header line has room for. */
export interface DiffStat {
  files: number;
  added: number;
  removed: number;
}

/** `diff --git a/<path> b/<path>` — the only reliable file boundary in a unified diff. */
const FILE_HEADER = /^diff --git /;

/**
 * Splits a unified diff into files, with every line classified.
 *
 * Two things it is careful about, both of which are the difference between a diff that reads right
 * and one that reads plausibly:
 *
 * * **`---` and `+++` are metadata, not a deletion and an addition — but only before the first
 *   `@@`.** They begin with the same characters as a changed line, so a classifier that looks only
 *   at the first character marks two phantom changed lines in every file of every patch and
 *   miscounts the stat beside it. Recognising them by their PREFIX alone is wrong the other way
 *   round and just as real: a deleted `--i;` renders as `---i;` and a deleted `--` as `---`, so a
 *   prefix test greys out genuine deletions in any C-like or YAML file. Git writes the pair
 *   exactly once per file, immediately before that file's first hunk, so POSITION is what
 *   separates them and it separates them exactly.
 * * **A cut patch still renders.** `MAX_DIFF_BYTES` truncates on a line boundary but not a file
 *   one, so the last file arrives without a following `diff --git`; it is flushed at the end
 *   rather than dropped for lacking a terminator.
 *
 * A fragment with no `diff --git` at all is kept under an empty path rather than discarded: the
 * daemon said there was a diff, and rendering nothing for a non-empty patch would be the panel
 * contradicting it.
 */
export function diffFiles(patch: string): DiffFile[] {
  const files: DiffFile[] = [];
  let current: DiffFile | null = null;
  /** Whether the CURRENT file has reached its first `@@`. Reset by every `diff --git`. */
  let seenHunk = false;
  // Only fully-blank input has no lines worth showing. `split("\n")` on "" yields [""], which
  // would otherwise become one file holding one empty context line.
  if (patch.trim() === "") return files;
  // A trailing newline is the normal shape of a patch and yields an empty final element; dropping
  // it here keeps a phantom blank context line off the end of every diff.
  const lines = patch.split("\n");
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();

  for (const text of lines) {
    if (FILE_HEADER.test(text)) {
      if (current) files.push(current);
      current = { path: headerPath(text), lines: [] };
      seenHunk = false;
      continue;
    }
    if (!current) current = { path: "", lines: [] };
    // Past this file's first `@@`, a leading `---`/`+++` is content and not a header — see
    // [`diffFiles`]. The flag is per FILE, so the next `diff --git` starts a fresh preamble.
    const kind = lineKind(text, seenHunk);
    if (kind === "hunk") seenHunk = true;
    current.lines.push({ kind, text });
  }
  if (current) files.push(current);
  return files;
}

/**
 * The `b/` path out of a `diff --git a/<path> b/<path>` header, or `""` when it cannot be read.
 *
 * Split on `" b/"` from the RIGHT rather than on spaces, because git emits these paths UNQUOTED
 * when they are ordinary, and a path containing a space would defeat a whitespace split — a real
 * case in this repo, which has directories with spaces in no place today and no guarantee about
 * tomorrow. Searching from the right is what makes `a/a b/c b/a b/c` resolve to the post-image.
 */
function headerPath(header: string): string {
  const at = header.lastIndexOf(" b/");
  if (at < 0) return "";
  return header.slice(at + 3).trim();
}

/**
 * How to read one line, given whether this file has already reached its first hunk.
 *
 * `seenHunk` is the whole reason this takes a second argument: it is what tells `--- a/x` (a file
 * header, before any `@@`) from `---i;` (a deleted `--i;`, after one). See [`diffFiles`].
 */
function lineKind(text: string, seenHunk: boolean): DiffLineKind {
  if (text.startsWith("@@")) return "hunk";
  if (!seenHunk && (text.startsWith("---") || text.startsWith("+++"))) return "meta";
  if (text.startsWith("+")) return "add";
  if (text.startsWith("-")) return "del";
  if (text.startsWith(" ") || text === "") return "context";
  // `index 1111..2222`, `new file mode`, `similarity index`, `\ No newline at end of file` — git's
  // per-file preamble. Anything unrecognised lands here too, which is the safe default: metadata
  // renders plainly, where mis-colouring it as a change would assert something false.
  return "meta";
}

/** The patch's headline numbers. Counts CHANGED lines only — `---`/`+++` are already `meta`. */
export function diffStat(files: readonly DiffFile[]): DiffStat {
  let added = 0;
  let removed = 0;
  for (const f of files) {
    for (const l of f.lines) {
      if (l.kind === "add") added += 1;
      else if (l.kind === "del") removed += 1;
    }
  }
  return { files: files.length, added, removed };
}

/**
 * GitHub check conclusions that are a PASS — one value, and deliberately a closed list of what IS
 * rather than a list of what is not: GitHub's vocabulary is its own and has grown before, and
 * defaulting an unknown state to "passing" is the one wrong answer available here — it would tell
 * an operator a pull request is green because the console had not heard of the state that says it
 * is not.
 */
const PASSING = new Set(["SUCCESS"]);

/** Conclusions that mean the check will not pass. `NEUTRAL` and `SKIPPED` are neither. */
const FAILING = new Set([
  "FAILURE",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
  "ERROR",
]);

/** Conclusions that mean the check has not finished. */
const RUNNING = new Set(["IN_PROGRESS", "QUEUED", "PENDING", "WAITING", "REQUESTED"]);

/**
 * The status-check rollup as one sentence, or `""` when there is nothing to say.
 *
 * The order is the order an operator cares about: a failure first, then what is still running,
 * then the count that passed. Empty is SILENCE and never "0 checks passing", because the daemon
 * answers an empty list both for a pull request with no checks configured and when it could not
 * read them (`rundiff` degrades rather than losing the diff) — neither of which is a verdict.
 */
export function checksSummary(checks: readonly RunCheck[]): string {
  if (checks.length === 0) return "";
  const total = checks.length;
  const state = (c: RunCheck) => (c.state ?? "").trim().toUpperCase();
  const failing = checks.filter((c) => FAILING.has(state(c))).length;
  if (failing > 0) return `${failing} of ${total} checks failing`;
  const running = checks.filter((c) => RUNNING.has(state(c))).length;
  if (running > 0) return `${running} of ${total} checks still running`;
  const passing = checks.filter((c) => PASSING.has(state(c))).length;
  // Only when every one of them passed does the sentence drop the denominator. A rollup holding a
  // state this console does not recognise keeps it, so "2 of 3" is visibly not "3".
  return passing === total ? `${total} checks passing` : `${passing} of ${total} checks passing`;
}
