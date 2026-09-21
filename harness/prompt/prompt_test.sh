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
#     instead. The prompt therefore routes a produced record to `~/.rhapsody/docs/<TICKET>-<slug>.md`,
#     which is the copy later runs READ and the one write that does not depend on anything being
#     reachable.
#   * That directory is a second read-only exception to the "stay in the workspace" rule, with a
#     one-file write carve-out for the run's own record.
#   * The invariant is the DEPENDENCY DIRECTION, not the absence of a token (STUDIO-600). 599 pinned
#     `save_document` as absent, which also banned the only container suited to a 16-57KB record; 600
#     reinstated it as the ticket half of a dual-write. Both were reasoning about a Linear write that
#     a dispatched run cannot make at all — see the next bullet. What survives from 600, and is still
#     pinned here, is that the record file is written FIRST and unconditionally, and that the report
#     of it is history and never the deliverable.
#   * There is NO Linear access from a dispatched run — not a write, not a read (STUDIO-957/958).
#     Neither harness configures a Linear MCP server. Unreachable writes were survivable; an
#     unreachable READ was not, because Phase 0.2 told the run to fetch its spec that way and Phase
#     1.3 says an unreadable required input must STOP the run. On 2026-09-20 STUDIO-957 and
#     STUDIO-958 each spent a turn discovering the tool was absent, stopped exactly as instructed,
#     and parked with no commits and no pull request. So the prompt names no Linear call, states the
#     description IS the spec, and says in as many words that citing no record is never a blocker.
#   * A run that cannot read a required input STOPS and hands off; it never reconstructs the input.
#   * None of this weakens the absolute rule that specs, plans and design docs never land in the repo —
#     the directory sits outside the repo precisely so that rule can stand.
#   * A statement of FACT about the repo can rot into a falsehood while every word around it stays
#     true. STUDIO-602: the prompt told every run the daemon binary was `symphonyd` — once as a fact
#     and once as a non-negotiable — long after the tree had renamed it. A run that took the
#     non-negotiable literally would defend a binary that does not exist, or flag the real one as a
#     violation. Such statements are checked against the tree, never against a literal in this file.
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

# --- the record goes to the filesystem; the run reports it in its final message -------------------
present "a produced record is routed to ~/.rhapsody/docs/<TICKET>-<slug>.md" \
        '~/\.rhapsody/docs/\{\{ *issue\.identifier *\}\}-<slug>\.md'

# --- no Linear access at all (STUDIO-957/958) ------------------------------------------------------
# STUDIO-600 reinstated `save_document` as the history container for a large record, on the premise
# that a dispatched run could make Linear writes. It cannot, and never could: neither harness
# configures a Linear MCP server, so every `mcp__claude_ai_Linear__*` call the prompt named was
# unreachable. That was survivable for the WRITES — a lost history copy costs nothing the filesystem
# record does not already hold — but Phase 0.2 also told the run to FETCH its spec that way, and
# Phase 1.3 says an unreadable required input must STOP the run. On 2026-09-20 STUDIO-957 and
# STUDIO-958 each burned a turn discovering the tool was absent, stopped exactly as instructed, and
# parked with no commits and no pull request. The agents were right; the prompt was wrong.
#
# So the invariant is inverted from 600's: the prompt must state plainly that there is NO Linear
# access, and must name no Linear call for the run to attempt.
present_i "the prompt states the run has no Linear access" \
          'You have no Linear access'
# NOT a bare `mcp__claude_ai_Linear__` grep: the ground rule above names the family as
# `mcp__claude_ai_Linear__*` to say it is absent, and that mention must stay. A CALL is the family
# prefix followed by a tool name, so require a letter after the underscores.
absent "no Linear MCP tool is named as something the run should call" \
       'mcp__claude_ai_Linear__[a-z]'
# The handoff is the daemon's own tool, which really is present, and it has no Linear fallback.
present "the handoff goes through the daemon's own MCP tool" \
        'mcp__symphony__symphony_handoff'
absent "no dead Linear fallback survives beside the handoff" \
       'save_issue|save_comment|save_document'

# --- the description IS the spec, and its absence is never a blocker -------------------------------
# The trap was not only the unreachable tool: Phase 0.2 asserted the spec and plan ARE Linear project
# documents, so an agent reading a self-contained ticket still went looking for one. Most tickets
# cite no record. Pin BOTH halves — where the spec actually is, and that finding no record is a
# normal outcome rather than the unreadable-required-input case that stops the run.
present_i "the ticket description is named as the spec" \
          'Your spec is the ticket description above'
present_i "a ticket citing no record is explicitly not a blocker" \
          'absence of a plan document is not a missing input and is never a blocker'

# --- the deliverable never DEPENDS on a Linear write (STUDIO-600) ---------------------------------
# The real 599 invariant, and the one that survives 957/958 unchanged: the record FILE is written
# first and unconditionally, so a fully headless run still produces the deliverable. Pin the
# dependency direction in all three places it is stated — the file write itself, what it does not
# wait on, and what the report is demoted to.
present "the record file is written first and never skipped" \
        'Write the file. Always, first, and never skipped'
present_i "the record file does not depend on Linear, gh, or a pull request" \
          'does not depend on Linear, on `gh`, or on there being a pull request'
present_i "the report is history, and the file write never waits on it" \
          'nothing in step 1 waits on it'

# The report ALWAYS carries the path, at every record size — that is what keeps a large record
# findable without pasting it. Checked in both places that say it: Phase 2 states the rule, Phase 6
# is where the report is actually written.
present "Phase 2: the report always carries a summary plus the record's docs path" \
        'always carries a summary of the record plus its'
present_i "the report is never a document-sized paste" \
          'never a [0-9]+KB paste'
present "Phase 6: the report cites the record's ~/.rhapsody/docs/ path" \
        '`~/\.rhapsody/docs/\{\{ *issue\.identifier *\}\}-<slug>\.md` path'

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

# --- the sidecar binary name the prompt states matches the one the tree builds (STUDIO-602) ---------
# Derived from the tree on every run, never written as a literal here: a literal would have to be
# edited by the same rename that moved the tree, which is exactly the edit that got missed. The
# contract is `BINARY_NAME` in the desktop supervisor — the constant the app resolves the sidecar by —
# and it is cross-checked against the crate that actually builds a binary of that name.
resolve_rs="$root/desktop/src-tauri/src/supervisor/resolve.rs"

# first_line <text> — the first line of a captured blob. Used instead of `| head -1` throughout this
# section: the script runs under `set -euo pipefail`, where a `head` that closes the pipe early can
# SIGPIPE its producer, and where a failing producer (a source file that moved) aborts the whole
# script at the assignment — silently, before the check below can report WHY. Capture, then slice.
first_line() { printf '%s' "${1%%$'\n'*}"; }

# The name the desktop app resolves the sidecar by.
binary_name="$(first_line "$(sed -n 's/.*BINARY_NAME[^=]*= *"\([^"]*\)".*/\1/p' "$resolve_rs" 2>/dev/null || true)")"

# crate_bin_names <manifest> <has-src-main> — every binary the crate can build, one per line.
# NOT "the `[[bin]]` override, else the package name": with `autobins` (default on 2018+ editions) an
# explicit `[[bin]]` does not replace the `src/main.rs` target, cargo builds BOTH. Treating an added
# helper binary as having renamed the sidecar would turn this red on a change that broke nothing, and
# a guard that cries wolf gets deleted — which would undo the whole point of this section. So it is a
# membership test over the full set: every `[[bin]]` name, plus the package name when `src/main.rs`
# exists and no `[[bin]]` has claimed that path (cargo's own suppression rule).
crate_bin_names() {
  awk -v has_main="$2" '
    function val(  ) { return match($0, /"[^"]*"/) ? substr($0, RSTART + 1, RLENGTH - 2) : "" }
    /^[[:space:]]*\[/ { tbl=$1 }
    tbl=="[package]" && /^[[:space:]]*name[[:space:]]*=/ { if (pkg == "") pkg = val() }
    tbl=="[[bin]]"   && /^[[:space:]]*name[[:space:]]*=/ { v = val(); if (v != "") names[++n] = v }
    tbl=="[[bin]]"   && /^[[:space:]]*path[[:space:]]*=/ { if (val() ~ /(^|\/)src\/main\.rs$/) main_claimed = 1 }
    END {
      for (i = 1; i <= n; i++) print names[i]
      if (pkg != "" && has_main == "1" && !main_claimed) print pkg
    }
  ' "$1"
}

# Exactly one crate must build `$binary_name`, or the sidecar the desktop app looks for is not built
# (none), or two crates disagree about who owns the name (more than one).
producers=""
if [ -n "$binary_name" ]; then
  for manifest in "$root"/crates/*/Cargo.toml; do
    [ -f "$manifest" ] || continue
    crate_dir="$(dirname "$manifest")"
    has_main=0; [ -f "$crate_dir/src/main.rs" ] && has_main=1
    # Captured, then compared line by line — deliberately not `| grep -Fxq`, which exits on the first
    # match and can SIGPIPE the awk feeding it; under `pipefail` that reads back as "no match".
    while IFS= read -r bin_nm; do
      if [ "$bin_nm" = "$binary_name" ]; then
        producers="$producers $(basename "$crate_dir")"
        break
      fi
    done <<<"$(crate_bin_names "$manifest" "$has_main" 2>/dev/null || true)"
  done
fi

if [ -z "$binary_name" ]; then
  bad "the daemon binary name can be derived from $resolve_rs (no BINARY_NAME found — did it move?)"
elif [ "$(printf '%s' "$producers" | wc -w | tr -d ' ')" != "1" ]; then
  bad "exactly one crate builds the '$binary_name' sidecar (found:${producers:-" none"})"
else
  ok "the '$binary_name' sidecar is built by crates/$(printf '%s' "$producers" | tr -d ' ')"
fi

# stated_binary <description> <sed-extract-expression> — pull the binary name the prompt states at one
# position and require it to equal the derived one. An empty extract fails too: the phrasing that
# carries the claim was reworded away, and a reworded claim is unchecked until this test is updated
# alongside it.
stated_binary() {
  local desc="$1" extract="$2" stated
  stated="$(first_line "$(sed -n "$extract" "$prompt" 2>/dev/null || true)")"
  if [ -z "$stated" ]; then
    bad "$desc (the prompt no longer states it in the expected phrasing)"
  elif [ "$stated" != "$binary_name" ]; then
    bad "$desc (prompt says '$stated', the tree builds '$binary_name')"
  else
    ok "$desc ('$stated')"
  fi
}

# Both places the prompt makes the claim. Checked separately: the first is a statement of fact in the
# repo tour, the second is a NON-NEGOTIABLE a run is told to defend, and either can rot alone.
stated_binary "the repo tour names the bin crate the tree actually builds" \
              's/.*plus the `\([^`]*\)` bin crate.*/\1/p'
stated_binary "the non-negotiable names the binary the tree actually builds" \
              's/.*the binary stays `\([^`]*\)`.*/\1/p'

# --- the run never merges its own pull request ------------------------------------------------------
# The prompt tells a run FOUR times that merging is not its job (the opening paragraph, Phase 4's
# "do NOT enable auto-merge and do NOT merge", Phase 6's "You do NOT merge", and the hand-off's
# "A reviewer merges") — and pinned it zero times. So when the git-hygiene bullet came to say the
# opposite ("You DO merge your own PR — but only in Phase 6..."), nothing caught the contradiction
# and it sat there through every run.
#
# It stayed harmless only by accident: under `review.mode: tickets` the review ticket's own prompt
# said "Never merge", which outranked it in the reviewer's context. Moving the install to
# `review.mode: ticketless` removed that second prompt, and the reviewer's hand-off began telling
# the AUTHOR they own the merge — so the contradiction became live, on the one instruction whose
# failure mode is an unreviewed merge to `main`.
#
# Pinned as a pair on purpose: the `absent` check alone would pass on a prompt that simply stopped
# mentioning merging, which is the same silence that let this drift in.
# Matching the AFFIRMATIVE construction directly, rather than matching "merge your own PR" and then
# filtering out lines that negate somewhere. That filtering approach was tried first and is a trap:
# the offending line was `You DO merge your own PR — ... never merge early`, so it carried its own
# negation and a filter dropped the very line it existed to catch. The check passed against the bug.
#
# `merge` must follow `you`/`you do` IMMEDIATELY, which is what separates the two readings: in
# "You do NOT merge your own PR" the word `NOT` sits in that gap, so it cannot match however the
# case falls.
absent "no line tells the run to merge its own pull request" \
       '[Yy]ou (DO |do )?merge your own (PR|pull request)'
present_i "the run is told merging is not its job" \
          'do NOT merge|does not merge|never merge'
present_i "the prompt names who does merge instead" \
          'a reviewer( or the maintainer)? merges|driver agent \(or a human\) reviews and merges|the maintainer merges'

if [ "$fail" -ne 0 ]; then
  echo "prompt_test: FAILED"
  exit 1
fi
echo "prompt_test: all passed"
