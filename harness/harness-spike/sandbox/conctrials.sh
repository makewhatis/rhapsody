#!/bin/bash
# conctrials.sh <label> <n> <scratch-root> -- <argv...>
#
# conctest.sh once proves nothing about a race: the interesting failures here are
# intermittent. This runs N *paired* trials of the same harness and reports how
# many individual turns survived, which is the number the design needs.
#
# `{}` in argv is replaced with that turn's own sandbox path. Two state-directory
# modes, both seeding opencode's auth.json because its credentials live in the
# very directory being redirected:
#   SEED_XDG=1   — a private XDG_DATA_HOME per turn at <sandbox>/xdg. The "does
#                  redirecting the state dir isolate them" case.
#   SHARED_XDG=1 — ONE state dir per trial, at <root>/<label>-<i>-xdg, that both
#                  turns are pointed at via `{XDG}` and that starts with no
#                  database. The "two turns initialise the same state dir" case.
# Any framing a transcript needs goes in $NOTE, so a committed transcript is this
# script's stdout with nothing added to it by hand.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
label="$1"; n="$2"; root="$3"; shift 3
[ "${1:-}" = "--" ] && shift
# SHARED_XDG rm -rf's a path built from these two, and mksandbox.sh wipes one per
# turn, so refuse the values that would make those paths dangerous.
case "$root" in ""|"/"|"$HOME"|"$HOME/") echo "conctrials.sh: refusing scratch root '${root:-<empty>}'" >&2; exit 2 ;; esac
case "$label" in ""|*/*) echo "conctrials.sh: <label> must be non-empty and contain no '/'" >&2; exit 2 ;; esac
export PROMPT="${PROMPT:-$(cat "$here/prompt-multitool.txt")}"
[ -n "${NOTE:-}" ] && printf '%s\n' "$NOTE" | sed 's/^/# /'
echo "### $label — $n paired trials, at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "### argv=$* <prompt>   SEED_XDG=${SEED_XDG:-0} SHARED_XDG=${SHARED_XDG:-0}"
auth="$HOME/.local/share/opencode/auth.json"
if [ "${SEED_XDG:-0}" = 1 ] || [ "${SHARED_XDG:-0}" = 1 ]; then
  # Loud on purpose. opencode keeps its credentials inside the very directory
  # these modes redirect, so a copy that quietly fails turns "isolation works"
  # into "10 turns were unauthenticated" with an identical-looking transcript.
  [ -s "$auth" ] || { echo "conctrials.sh: a redirected state dir needs $auth, which is missing or empty - the turns would run unauthenticated" >&2; exit 3; }
  echo "### state dir seeded from $auth ($(wc -c <"$auth" | tr -d ' ') bytes)"
fi
ok=0; bad=0
for i in $(seq 1 "$n"); do
  a="$root/$label-$i-a"; b="$root/$label-$i-b"
  if [ "${SHARED_XDG:-0}" = 1 ]; then
    # Rebuilt per trial: the case under test is the FIRST concurrent use of a
    # state dir, so carrying one over from the previous trial would measure the
    # warm case instead and silently pass.
    export XDG_DIR="$root/$label-$i-xdg"
    rm -rf "$XDG_DIR"; mkdir -p "$XDG_DIR/opencode"
    cp "$auth" "$XDG_DIR/opencode/auth.json" || exit 3
  fi
  for sb in "$a" "$b"; do
    "$here/mksandbox.sh" "$sb" >/dev/null
    # Dropped in unconditionally, including for claude/codex sandboxes: only opencode
    # reads it, and the prompt touches nothing but counter.txt, so an ignored config
    # file is cheaper than branching on the harness. It is not read by the others.
    [ -f "$here/../opencode/opencode.json" ] && cp "$here/../opencode/opencode.json" "$sb/opencode.json"
    if [ "${SEED_XDG:-0}" = 1 ]; then
      mkdir -p "$sb/xdg/opencode"
      cp "$auth" "$sb/xdg/opencode/auth.json" || exit 3
    fi
  done
  out=$("$here/conctest.sh" "$label-$i" "$a" "$b" -- "$@" 2>&1)
  # 'events;' keeps the per-turn event count and session ids that conctest.sh
  # prints. Without it the id half of every VERDICT is unauditable in the
  # transcript: the reader sees the conclusion and none of the evidence.
  echo "$out" | grep -E 'exit=|events;|VERDICT'
  for sb in "$a" "$b"; do
    if [ "$(cat "$sb/counter.txt")" = 8 ]; then ok=$((ok+1)); else
      bad=$((bad+1))
      # Whole stderr, not a one-line squash of it: the first two lines of an
      # opencode failure are the same generic "Unexpected error / database is
      # locked" whatever went wrong, and the statement that actually failed is
      # below them. Losing those lines cost this spike a whole review round.
      if [ -s "$sb/.stderr" ]; then
        echo "    FAILED TURN $(basename "$sb"), stderr follows:"
        tr -d '\033' <"$sb/.stderr" | sed 's/\[[0-9;]*m//g' | sed 's/^/      | /'
      else
        # Said out loud, because an empty block under a "stderr follows" header
        # reads like the transcript lost something.
        echo "    FAILED TURN $(basename "$sb"): stderr was empty"
      fi
    fi
  done
done
echo "### $label RESULT: $ok/$((ok+bad)) turns completed, $bad lost"
