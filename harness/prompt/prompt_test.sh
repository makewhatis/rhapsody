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
# instruction and the property that makes it affordable — that it costs no second Linear write.
present_i "the record's ticket copy is posted to Linear for history" \
          'Post the same text to the Linear ticket'
present_i "the ticket copy costs no extra Linear write" \
          'costs no extra write'

# The deliverable must no longer DEPEND on a Linear write or on a pull request existing. Both of the
# superseded routes are pinned as absent: publishing the document as a Linear document, and carrying
# its full text in the PR body. Reinstating either turns this red.
absent "the deliverable no longer depends on publishing a Linear document" \
       'save_document'
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
