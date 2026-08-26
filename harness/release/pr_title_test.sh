#!/usr/bin/env bash
# pr_title_test.sh (STUDIO-408) — pins check-pr-title.sh, the validator the `pr-title` workflow runs
# against every PR title. The title IS the squash subject GitHub writes onto main, and that subject is
# the only thing release-please parses, so this case table is the real contract: anything it accepts
# must be a subject release-please can turn into a release, and anything it rejects must be one that
# would otherwise skip a release SILENTLY (STUDIO-406/#27 is exactly that failure).
#
# The valid-type list below is deliberately a SECOND copy of the set in check-pr-title.sh: it is
# release-please's default section set (DEFAULT_HEADINGS), so a drive-by edit to the validator that
# drops or invents a type fails here instead of shipping.
#
# No dependencies beyond bash. Run from anywhere: `harness/release/pr_title_test.sh`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$here/check-pr-title.sh"
fail=0
out=""
status=0

# run <subject...> — runs the validator and sets `out` (combined output) + `status` (exit code) for
# the caller. Deliberately NOT `$(run ...)`: a command substitution runs the function in a subshell,
# so the assignments would not survive it (the same trap version_test.sh's new_repo works around).
run() {
  set +e
  out="$("$check" "$@" 2>&1)"
  status=$?
  set -e
}

# accepts <subject> — the validator must exit 0.
accepts() {
  run "$1"
  if [ "$status" -eq 0 ]; then
    echo "ok   - accepts '$1'"
  else
    echo "FAIL - accepts '$1': exit $status, output: $out"
    fail=1
  fi
}

# rejects <subject> — the validator must exit 1 (a rejection, not a usage error).
rejects() {
  run "$1"
  if [ "$status" -eq 1 ]; then
    echo "ok   - rejects '$1'"
  else
    echo "FAIL - rejects '$1': exit $status, output: $out"
    fail=1
  fi
}

# --- every type release-please recognises (its default changelog sections) ------------------------
# feat/fix (and a breaking change) are the only ones that bump a version, but the rest parse cleanly
# and belong in the changelog, so the guard must not force an unrelated PR to lie about its type.
for type in build chore ci deps docs feat fix perf refactor revert style test; do
  accepts "$type: a perfectly ordinary subject"
done

# --- the grammar around the type -------------------------------------------------------------------
accepts "fix(poller): scope is optional but allowed"
accepts "feat(http-api/v1): scopes may contain dashes and slashes"
accepts "feat!: a breaking change marker is allowed"
accepts "feat(config)!: ...with a scope too"
accepts "fix: null attachment fields no longer make a project invisible to the poller (STUDIO-408)"
# GitHub appends " (#N)" to the squash subject — the description swallows it, so it must stay valid.
accepts "fix: null attachment fields no longer make a project invisible to the poller (#28)"

# --- the subjects that silently skipped the release -------------------------------------------------
# The real STUDIO-406 squash subject: a ticket id is not a conventional type.
rejects "STUDIO-406: stop a null attachment field making an entire project invisible to the poller"
rejects "TRA-320: stop the dashboard truncating to 50 runs"
rejects "Fix the poller"                          # no type at all
rejects "fix the poller: it was broken"           # 'fix the poller' is not a type
rejects "Fix: capitalised type"                   # release-please's parser wants a lowercase type
rejects "FIX: shouty type"
rejects "chore:missing space after the colon"
rejects "chore: "                                 # empty description
rejects "chore:"
rejects "feature: 'feature' is not a conventional type ('feat' is)"
rejects "bugfix: nor is 'bugfix'"
rejects "wip: not a recognised type"
rejects " fix: leading whitespace breaks the parse"
rejects "Revert \"fix: something\""               # GitHub's default revert title; use 'revert: ...'
rejects ""

# --- the failure message has to explain the CONSEQUENCE, not just the grammar ----------------------
run "STUDIO-406: stop a null attachment field making an entire project invisible to the poller"
msg="$out"
for phrase in "no release" "cask" "manifest" "silently"; do
  if printf '%s' "$msg" | grep -qi -- "$phrase"; then
    echo "ok   - rejection message mentions '$phrase'"
  else
    echo "FAIL - rejection message never mentions '$phrase': $msg"
    fail=1
  fi
done

# The offending subject is echoed back so the annotation is actionable without opening the log.
if printf '%s' "$msg" | grep -q "STUDIO-406: stop a null attachment field"; then
  echo "ok   - rejection message quotes the offending subject"
else
  echo "FAIL - rejection message does not quote the offending subject: $msg"
  fail=1
fi

# --- the run prompt must mandate a title this validator accepts (STUDIO-593) ----------------------
# .rhapsody/PROMPT.md tells every Rhapsody run how to title its PR. It used to mandate
# "<ticket-id>: <summary>" — the one shape this validator rejects — so every run failed `pr-title` on
# its first attempt and then self-corrected. Pinning the prompt's worked examples here means the
# instruction and the gate that judges it cannot drift apart again.
prompt="$here/../../.rhapsody/PROMPT.md"
if [ ! -f "$prompt" ]; then
  echo "FAIL - prompt examples: $prompt is missing"
  fail=1
else
  examples=0
  # Everything between the pr-title-examples markers, minus the markers, the code fence and blanks.
  while IFS= read -r line; do
    accepts "$line"
    examples=$((examples + 1))
  done < <(sed -n '/pr-title-examples:begin/,/pr-title-examples:end/p' "$prompt" \
             | sed -e 's/^[[:space:]]*//' -e '/^<!--/d' -e '/^```/d' -e '/^$/d')

  if [ "$examples" -lt 1 ]; then
    echo "FAIL - prompt examples: no titles between the pr-title-examples markers in $prompt"
    fail=1
  else
    echo "ok   - all $examples PR-title example(s) in .rhapsody/PROMPT.md are accepted"
  fi

  # The mandate itself, not just the examples: a backticked title template starting with the issue-id
  # placeholder (`{{ issue.identifier }}: ...`) is the rejected form. Prose that merely NAMES that
  # shape does not match — the placeholder there is closed by a backtick, not followed by a colon and
  # a description.
  if grep -q '`{{ issue.identifier }}:[^`]' "$prompt"; then
    echo "FAIL - $prompt still mandates a leading-ticket-id PR title, the shape this validator rejects"
    fail=1
  else
    echo "ok   - .rhapsody/PROMPT.md does not mandate a leading-ticket-id PR title"
  fi
fi

# --- usage --------------------------------------------------------------------------------------
# No argument at all is an operator/workflow wiring error, NOT a bad title: exit 2 so a mis-wired
# workflow can never be mistaken for (or reported as) a rejected title.
run
if [ "$status" -eq 2 ]; then
  echo "ok   - missing argument is a usage error (exit 2)"
else
  echo "FAIL - missing argument: expected exit 2, got $status"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "pr_title_test: FAILED"
  exit 1
fi
echo "pr_title_test: all passed"
