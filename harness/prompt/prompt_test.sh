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

if [ "$fail" -ne 0 ]; then
  echo "prompt_test: FAILED"
  exit 1
fi
echo "prompt_test: all passed"
