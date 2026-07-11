#!/usr/bin/env bash
# gt-guard.sh — Symphony Graphite workflow guardrail (INF-251).
#
# A Claude Code PreToolUse hook, scoped to the Bash tool. It reads the PreToolUse
# JSON payload on stdin and, when the command is a raw mutating git invocation that
# Graphite owns, exits 2 — which makes Claude Code BLOCK the tool call and feed this
# script's stderr back to the agent as guidance. Every other command passes (exit 0).
#
# Blocked -> replacement:
#   git commit            -> gt create -m "…"  (new branch)  /  gt modify --update  (amend)
#   git push              -> gt submit --draft
#   git add -A | --all | .-> stage by explicit path: git add path/to/file
#
# Matching is intentionally a simple regex over the command string — we do NOT parse
# the shell. A blocked keyword inside an echo, comment, or string is tolerated
# collateral (per INF-251): correctness for real git invocations is what matters.
# The guard is over-broad, never under-broad: when jq is unavailable we match the raw
# payload (restricted to Bash tool calls), so a blocked command is still caught.
#
# The match allows git's global options between `git` and the subcommand, so
# `git -C <dir> commit`, `git -c k=v push`, and `git --git-dir=… add -A` are caught,
# not just the bare forms. It is NOT a full shell parser — e.g. `git -C` with the path
# split across an unusual quoting boundary, or a subcommand reached via an alias, can
# still slip through; those are out of scope (the rule targets the reflexive raw forms).

set -uo pipefail

payload="$(cat)"

# Extract the Bash command. Prefer jq (clean, exact); fall back to the raw payload so
# the guard still fires without jq. A non-Bash tool / absent command yields an empty
# string under jq and there is nothing to guard, so pass.
if command -v jq >/dev/null 2>&1; then
  cmd="$(printf '%s' "$payload" | jq -r '.tool_input.command // empty' 2>/dev/null)"
  [ -z "$cmd" ] && exit 0
else
  # No jq: match against the raw payload, but only for a Bash tool call. This keeps the
  # fallback from blocking a benign non-Bash tool (e.g. an Edit whose content merely
  # mentions "git commit"), mirroring the jq path's non-Bash => pass behavior.
  printf '%s' "$payload" | grep -Eq '"tool_name"[[:space:]]*:[[:space:]]*"Bash"' || exit 0
  cmd="$payload"
fi

block() {
  # $1 = what was blocked, $2 = the replacement guidance.
  printf 'Blocked by Symphony Graphite guard (git_flow=graphite): %s\n' "$1" >&2
  printf '%s\n' "$2" >&2
  exit 2
}

# A leading boundary so we match `git` as a word (after start, whitespace, or a shell
# separator like && ; |), not as part of `legit`/`mygit`. The trailing boundary allows
# whitespace, end-of-string, a shell separator () ; & |), a redirect (> <), or a closing
# quote ("), so `(git push)`, `git commit && x`, `git push>log`, and the JSON-embedded
# `"command":"git push"` (no-jq fallback) are caught while `git pushing`/`git committed`
# and a real path (`git add ./src`) are not.
lead='(^|[^[:alnum:]_./-])'
trail='([[:space:]);&|<>"]|$)'

# gopt matches an optional run of git GLOBAL options between `git` and the subcommand:
# each is a dash-token (-c, -C, --git-dir=…) optionally followed by a non-dash argument
# token (the value for `-C <dir>` / `-c <k=v>`). This catches `git -C <dir> commit` etc.
# without parsing the shell.
gopt='([[:space:]]+-[^[:space:]]+([[:space:]]+[^[:space:]-][^[:space:]]*)?)*'

# git commit -> gt create / gt modify
if printf '%s' "$cmd" | grep -Eq "${lead}git${gopt}[[:space:]]+commit${trail}"; then
  block "raw 'git commit'" \
    "Use Graphite: 'gt create -m \"…\"' to start a new branch/commit, or 'gt modify --update' to amend the current branch's single commit."
fi

# git push -> gt submit
if printf '%s' "$cmd" | grep -Eq "${lead}git${gopt}[[:space:]]+push${trail}"; then
  block "raw 'git push'" \
    "Use Graphite: 'gt submit --draft' to push the stack and open/update draft PRs."
fi

# git add -A | --all | . | -- .  -> explicit-path staging (a real path like ./src or file.go passes)
if printf '%s' "$cmd" | grep -Eq "${lead}git${gopt}[[:space:]]+add[[:space:]]+(--[[:space:]]+)?(-A|--all|\.)${trail}"; then
  block "bulk 'git add -A' / 'git add .'" \
    "Stage by explicit path instead: 'git add path/to/file' (bulk staging is disallowed under git_flow=graphite)."
fi

exit 0
