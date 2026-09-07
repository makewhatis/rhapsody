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

Find the newest commit that touched any existing `CLAUDE.md`, then look at everything that changed
since:

```bash
newest_commit=""
newest_ts=0
for f in $(git ls-files '*CLAUDE.md'); do
	ts=$(git log -1 --format=%ct -- "$f")
	if [ "$ts" -gt "$newest_ts" ]; then
		newest_ts="$ts"
		newest_commit=$(git log -1 --format=%H -- "$f")
	fi
done
git diff "$newest_commit"..HEAD --stat
```

Read that diff's file list for two things:

- **New directories that now warrant a `CLAUDE.md`** — a new crate under `crates/`, a new top-level
  subsystem. Apply the original methodology's structure test: does this directory have enough
  non-obvious, non-derivable behavior to justify its own file, and at what depth relative to its
  nearest existing ancestor `CLAUDE.md`?
- **Covered directories that were deleted or gutted**, to the point their `CLAUDE.md` now describes
  something that no longer exists.

## Step 2 — per-file drift check

For every existing `CLAUDE.md`, diff its own directory since **that file's own** last commit — not
the global newest from Step 1:

```bash
for f in $(git ls-files '*CLAUDE.md'); do
	dir=$(dirname "$f")
	last=$(git log -1 --format=%H -- "$f")
	if ! git diff --quiet "$last"..HEAD -- "$dir"; then
		echo "DRIFTED: $f"
	fi
done
```

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
