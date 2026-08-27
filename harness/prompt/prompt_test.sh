#!/usr/bin/env bash
# prompt_test.sh (STUDIO-599) — pins the invariants of `.rhapsody/PROMPT.md`, the prompt every
# dispatched Rhapsody run is given.
#
# Prompt text has no compiler, so a well-meaning edit can silently undo an instruction that exists
# because a run already failed without it. Each check below corresponds to one such failure:
#
#   * A design record that lives only in Linear is unreadable to a dispatched run, which is headless
#     and has no Linear access. STUDIO-594 dead-ended with no deliverable because it could not read
#     STUDIO-572's design; STUDIO-598 reconstructed STUDIO-594's trait surface from first principles
#     instead. The prompt therefore dual-writes a produced record to `~/.rhapsody/docs/<TICKET>-<slug>.md`
#     (the copy runs READ) and to the Linear ticket (the durable HISTORY), and the filesystem write is
#     the one that does not depend on Linear being reachable.
#   * That directory is a second read-only exception to the "stay in the workspace" rule, with a
#     one-file write carve-out for the run's own record.
#   * The invariant is the DEPENDENCY DIRECTION, not the absence of a token (STUDIO-600). 599 pinned
#     `save_document` as absent, which also banned the only container suited to a 16-57KB record and
#     left the ticket half of the dual-write as a document-sized comment paste. So the ticket copy now
#     scales with the record — always a summary plus the `~/.rhapsody/docs/` path, full text inline
#     below a named character threshold and a linked Linear document above it — and what is pinned is
#     that the file is written first and unconditionally, and that `save_document` is the HISTORY
#     container and never the deliverable.
#   * A run that cannot read a required input STOPS and hands off; it never reconstructs the input.
#   * None of this weakens the absolute rule that specs, plans and design docs never land in the repo —
#     the directory sits outside the repo precisely so that rule can stand.
#
# No dependencies beyond bash + git. Run from anywhere: `harness/prompt/prompt_test.sh`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
prompt="$root/.rhapsody/PROMPT.md"
fail=0

ok()   { echo "ok   - $1"; }
bad()  { echo "FAIL - $1"; fail=1; }

# present <description> <ere> — the prompt must contain a line matching the extended regex.
present() {
  if grep -qE "$2" "$prompt"; then ok "$1"; else bad "$1 (no line matches /$2/)"; fi
}

# present_i <description> <ere> — case-insensitive `present`, for prose whose capitalisation is
# incidental ("Do NOT reconstruct" vs "do not reconstruct").
present_i() {
  if grep -qiE "$2" "$prompt"; then ok "$1"; else bad "$1 (no line matches /$2/i)"; fi
}

# absent <description> <ere> — the prompt must NOT contain a line matching the extended regex.
absent() {
  if grep -qiE "$2" "$prompt"; then
    bad "$1 (still matches /$2/i: $(grep -m1 -iE "$2" "$prompt" | cut -c1-100))"
  else
    ok "$1"
  fi
}

if [ ! -f "$prompt" ]; then
  echo "FAIL - $prompt is missing"
  echo "prompt_test: FAILED"
  exit 1
fi

# --- the second read-only exception ---------------------------------------------------------------
# The workspace rule used to allow exactly ONE read-only exception (the Go reference). Leaving that
# count at ONE while adding the docs directory below it is the drift this catches.
absent "the workspace rule no longer claims a single read-only exception" \
       'ONE read-only exception'
present "the workspace rule counts TWO read-only exceptions" \
        'TWO read-only exceptions'
present "the docs directory is named as the second read-only exception" \
        'read-only exception.*~/\.rhapsody/docs/|~/\.rhapsody/docs/.*read-only exception'

# --- the write carve-out is exactly one file, this ticket's own -------------------------------------
present_i "the run may write exactly ONE file in that directory" \
          'write.*exactly ONE file'
present_i "the run must never touch another ticket's record" \
          "another ticket's record"

# --- dual-write: the filesystem copy is the deliverable ---------------------------------------------
present "a produced record is routed to ~/.rhapsody/docs/<TICKET>-<slug>.md" \
        '~/\.rhapsody/docs/\{\{ *issue\.identifier *\}\}-<slug>\.md'
present_i "the routing is described as a dual-write (filesystem + ticket)" \
          'dual-write'
# NOT a bare `save_comment` grep: that token already appears in the ground rules' write budget and in
# Phase 6 step 2 regardless of this change, so it would stay green with the ticket half of the
# dual-write deleted. Same for a bare `dual-write`, which the Phase-2 intro also says. Pin the
# instruction, plus the property that keeps the SMALL-record case affordable. That property is now
# branch-specific, not universal — a large record deliberately pays a second Linear write for its
# document — so the check names the branch it pins rather than overstating its subject.
present_i "the ticket always gets a copy of the record, for history" \
          'Give the ticket a copy, sized to the record'
present_i "an inlined ticket copy costs no extra Linear write" \
          'costs no extra write'

# --- the deliverable never DEPENDS on a Linear write (STUDIO-600) ---------------------------------
# The real 599 invariant: the record FILE is written first and unconditionally, so a fully headless
# run still produces the deliverable. Pin the dependency direction in all three places it is stated —
# the file write itself, what it does not wait on, and what the ticket copy is demoted to.
present "the record file is written first and never skipped" \
        'Write the file. Always, first, and never skipped'
present_i "the record file does not depend on Linear, gh, or a pull request" \
          'does not depend on Linear, on `gh`, or on there being a pull request'
present_i "the ticket copy is history, and the file write never waits on it" \
          'nothing in step 1 waits on it'

# The summary comment ALWAYS carries the path, at every record size — that is what keeps a large
# record findable from the ticket without pasting it there. Checked in both places that say it:
# Phase 2 states the rule, Phase 6 is where the comment is actually written.
present "Phase 2: the summary comment always carries a summary plus the record's docs path" \
        'always carries a summary of the record plus its'
present_i "the summary comment is never a document-sized paste" \
          'never a [0-9]+KB paste'
present "Phase 6: the summary comment cites the record's ~/.rhapsody/docs/ path" \
        '`~/\.rhapsody/docs/\{\{ *issue\.identifier *\}\}-<slug>\.md` path'

# A NAMED threshold, not a judgement call. The number itself is deliberately not frozen — retuning it
# is a legitimate edit; leaving the choice to the run's judgement is the regression.
present "a numeric threshold is named for inlining the full text" \
        'Under [0-9][0-9,]+ characters'
present "a numeric threshold is named for the linked-document route" \
        '[0-9][0-9,]+ characters or more'

# `save_document` is allowed BACK, but only as the history container for a large record. Neither check
# below is a bare `save_document` grep: that token now also appears in the ground rules' write budget,
# independently of this route, so deleting the route would leave a bare grep green (the same hole 599's
# own self-review found in its `save_comment` check). Pin the instruction, then the qualification —
# because a route reinstated as the DELIVERABLE is the thing 599 was right to prevent.
present "a large record's full text is published as a Linear document" \
        'mcp__claude_ai_Linear__save_document`, parented to the ticket'
present "save_document is the history container and never the deliverable" \
        '`save_document` is the HISTORY container and never the deliverable'

# A large record takes TWO Linear writes, which fail independently. Dropping the summary comment
# because the DOCUMENT write failed would strand the path citation and leave the worst case strictly
# worse than the single-write route this replaced, so the comment is unconditional.
present_i "a failed save_document still posts the summary comment carrying the path" \
          'still post the summary comment'

# The PR body stays dead as a home for the document (unchanged from STUDIO-599).
absent "the deliverable no longer lives in the pull request body" \
       '(full|whole|entire) (text|document).*in the pull request body|document.{0,20}under a .## Design document'

# --- a required input that cannot be read stops the run ---------------------------------------------
# Two distinct places, checked separately on purpose: Phase 1 has to MANDATE the read, and "When
# blocked" has to LIST an unreadable one as a blocker. A single "required input" grep would stay green
# with either half deleted, because the other half also says the words.
present_i "Phase 1 mandates reading every required input the ticket names, first" \
          'Read every required input the ticket names'
present_i "an unreadable required input is listed as a blocker for a human" \
          'required input the ticket names.*that you cannot read'
# Case-SENSITIVE on purpose: the same line goes on to quote the "stop rather than improvise" phrase,
# so a case-insensitive match would stay green with the actual STOP instruction deleted.
present "an unreadable required input stops the run and hands off" \
        'cannot be read.*STOP.*hand off'
present_i "reconstructing a missing input is explicitly forbidden" \
          'not reconstruct the missing input'
present_i "building on a reconstruction is forbidden in the blocked path too" \
          'never reconstruct a missing required input'

# --- none of this relaxes the never-commit-to-the-repo rule -----------------------------------------
present "specs, plans and design docs still never get committed" \
        'Never commit specs, plans, or design docs'
present_i "the docs directory is explicitly not a licence to relax that rule" \
          'sits outside the repo precisely so this rule can stand'

# The rule, enforced against the repo itself rather than only asserted in prose: no process-document
# directory is tracked. (`docs/` under a component — desktop/, harness/ — is that component's
# operational README territory and is not what this forbids; a top-level one is.)
tracked="$(cd "$root" && git ls-files -- 'docs/*' 'rfcs/*' 'crates/*/docs/*' 2>/dev/null || true)"
if [ -n "$tracked" ]; then
  bad "the repo tracks process-document paths it must not: $(printf '%s' "$tracked" | tr '\n' ' ')"
else
  ok "the repo tracks no docs/ or rfcs/ process-document tree"
fi

if [ "$fail" -ne 0 ]; then
  echo "prompt_test: FAILED"
  exit 1
fi
echo "prompt_test: all passed"
