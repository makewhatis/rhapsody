#!/bin/bash
# conctrials.sh <label> <n> <scratch-root> -- <argv...>
#
# conctest.sh once proves nothing about a race: the interesting failures here are
# intermittent. This runs N *paired* trials of the same harness and reports how
# many individual turns survived, which is the number the design needs.
#
# `{}` in argv is replaced with that turn's own sandbox path. Set $SEED_XDG=1 to
# give each turn a private XDG_DATA_HOME at <sandbox>/xdg seeded with opencode's
# auth.json — that is the "does redirecting the state dir isolate them" case, and
# the credentials have to be copied in because they live in that same directory.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
label="$1"; n="$2"; root="$3"; shift 3
[ "${1:-}" = "--" ] && shift
export PROMPT="${PROMPT:-$(cat "$here/prompt-multitool.txt")}"
echo "### $label — $n paired trials, at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "### argv=$* <prompt>   SEED_XDG=${SEED_XDG:-0}"
ok=0; bad=0
for i in $(seq 1 "$n"); do
  a="$root/$label-$i-a"; b="$root/$label-$i-b"
  for sb in "$a" "$b"; do
    "$here/mksandbox.sh" "$sb" >/dev/null
    # Dropped in unconditionally, including for claude/codex sandboxes: only opencode
    # reads it, and the prompt touches nothing but counter.txt, so an ignored config
    # file is cheaper than branching on the harness. It is not read by the others.
    [ -f "$here/../opencode/opencode.json" ] && cp "$here/../opencode/opencode.json" "$sb/opencode.json"
    if [ "${SEED_XDG:-0}" = 1 ]; then
      mkdir -p "$sb/xdg/opencode"
      cp "$HOME/.local/share/opencode/auth.json" "$sb/xdg/opencode/auth.json" 2>/dev/null || true
    fi
  done
  out=$("$here/conctest.sh" "$label-$i" "$a" "$b" -- "$@" 2>&1)
  echo "$out" | grep -E 'exit=|VERDICT'
  for sb in "$a" "$b"; do
    if [ "$(cat "$sb/counter.txt")" = 8 ]; then ok=$((ok+1)); else
      bad=$((bad+1))
      echo "    FAILED TURN $(basename "$sb"): $(tr -d '\033' <"$sb/.stderr" | sed 's/\[[0-9;]*m//g' | tr '\n' ' ')"
    fi
  done
done
echo "### $label RESULT: $ok/$((ok+bad)) turns completed, $bad lost"
