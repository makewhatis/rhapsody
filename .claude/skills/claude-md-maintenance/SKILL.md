---
name: claude-md-maintenance
description: Use when a ticket's whole purpose is a CLAUDE.md maintenance sweep for this repo — detecting and fixing drift between the nested CLAUDE.md files and the code they describe. Dedicated-sweep-ticket only; never invoke as a side effect of unrelated feature work.
---

# CLAUDE.md Maintenance Sweep

## Overview

This repo carries a tree of nested `CLAUDE.md` files (25 at the time of writing: the root file plus
24 subdirectories), generated in one pass by a live-composed Claude Code `Workflow` run. They are
snapshots. As code changes underneath them they go stale — a renamed function, a changed placeholder
name, a directory that no longer exists, a new crate with no file at all. This skill finds that drift
and fixes it with *targeted* edits, never a full regeneration of the tree.

Run this only when the ticket that dispatched you exists specifically to run this sweep. If you were
dispatched for unrelated work and happen to notice a stale `CLAUDE.md` nearby, that is out of scope —
leave it and mention it in your handoff notes instead.

## Step 1 — scoped structural discovery

Take the baseline from the **oldest** last-touch across the `CLAUDE.md` set, not the newest. Using
the newest makes the diff empty right after a sweep commits its own updates, and hides every
directory added before that commit:

```bash
cd "$(git rev-parse --show-toplevel)" || exit 1
BASE=$(git ls-files '*CLAUDE.md' | while IFS= read -r f; do
	git log -1 --format='%ct %H' -- "$f"
done | sort -n | head -1 | cut -d' ' -f2)
git diff --name-status "$BASE"..HEAD
```

The diff catches recent structure. Sweep current coverage directly too, so a meaningful unit that
predates every `CLAUDE.md` commit is still caught:

```bash
git ls-files '*CLAUDE.md' | while IFS= read -r f; do dirname "$f"; done | sort -u
```

Compare that covered-directory list against the tree as it stands, and read the diff's file list,
for two things:

- **New directories that now warrant a `CLAUDE.md`** — a new crate under `crates/`, a new top-level
  subsystem. Apply the original methodology's structure test: does this directory have enough
  non-obvious, non-derivable behavior to justify its own file, and at what depth relative to its
  nearest existing ancestor `CLAUDE.md`?
- **Covered directories that were deleted or gutted**, to the point their `CLAUDE.md` now describes
  something that no longer exists.

## Step 2 — per-file drift check

For every existing `CLAUDE.md`, diff its own directory since **that file's own** last commit — not
Step 1's `$BASE`. A file updated last week is only asked about last week's changes:

```bash
cd "$(git rev-parse --show-toplevel)" || exit 1
git ls-files '*CLAUDE.md' | while IFS= read -r f; do
	last=$(git log -1 --format=%H -- "$f")
	[ -z "$last" ] && continue
	dir=$(dirname "$f")
	if [ "$dir" = "." ]; then spec=":/"; else spec="$dir"; fi
	changed=$(git diff --name-only "$last"..HEAD -- "$spec" | grep -v 'CLAUDE\.md$')
	if [ -n "$changed" ]; then
		printf 'DRIFTED: %s (since %s)\n%s\n' "$f" "$last" "$changed"
	fi
done
```

Three details that matter: reading the file list with `while IFS= read -r` rather than
`for f in $(...)` keeps paths containing spaces intact; the root `CLAUDE.md` (whose `dirname` is `.`)
is scoped to the whole repo via `:/`; and filtering out `CLAUDE.md` paths stops a docs-only commit
from flagging its own directory as drifted.

A file with no output here is skipped entirely for the rest of the sweep. That is what keeps this
targeted rather than a regeneration.

**A diff is not automatically drift.** For each `DRIFTED` file, read the actual diff
(`git diff "$last"..HEAD -- "$dir"`) and judge whether it genuinely invalidates a claim the
`CLAUDE.md` makes, or merely touches unrelated lines in the same directory. Only files with a real
inaccuracy proceed to Step 3.

## Step 3 — fix the drift via a fresh inline Workflow

For every file flagged by Step 1 (new or retired) or Step 2 (confirmed inaccuracy), author and run a
Claude Code `Workflow` **inline**, via the tool's `script` parameter.

**Never write a persisted `.claude/workflows/*.js` file.** A stored script would freeze the repo's
current directory list exactly the way a stale `CLAUDE.md` freezes its directory's facts — new
crates, removed subsystems, or restructured folders would silently fall out of sync with it.
Composing it fresh each run means the script starts from the tree as it actually is.

The script must, in order:

1. **Draft depth-first.** Process flagged files parent-directory-first, then children. A child's
   non-repetition check has to see its parent's *already-fixed* content, not the stale version that
   was true when the sweep started. This means a barrier per depth level spanning both drafting and
   verification — a depth-1 file still being redrafted blocks depth-2 files that would read it.
2. **Verify independently.** A *separate* agent from the one that drafted each edit checks: no
   content repeated from an ancestor `CLAUDE.md` in its load chain, under the 200-line ceiling, every
   claim concrete and not derivable by simply reading the code, and the exact header
   `# CLAUDE.md — <path>` (or plain `# CLAUDE.md` for the root file).
3. **One bounded redraft** on a failed verification. If the redraft fails too, leave that file
   untouched and flag it in your handoff summary for human review — never ship a low-confidence edit,
   and never loop retrying.

## Step 4 — normal ticket flow

Commit, open a PR, and end your final message with the literal line `HANDOFF: in-review`. Nothing
about this step is sweep-specific — it is the same flow every other dispatched ticket follows.
