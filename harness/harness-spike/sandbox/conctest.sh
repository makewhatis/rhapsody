#!/bin/bash
# conctest.sh <label> <sandbox-a> <sandbox-b> -- <argv...>
#
# Item 4 of the spike: run two turns of one harness AT ONCE against that
# harness's shared global state directory, and report whether either turn is
# corrupted by the other. Each turn gets its own sandbox, so a turn that reads
# the other's files, or a session that is handed the other's history, shows up
# as a wrong counter or a shared session id.
#
# `{}` in argv is replaced with that turn's own sandbox path (opencode --dir);
# `{XDG}` is replaced with $XDG_DIR, one state directory BOTH turns are pointed at.
# The prompt is appended as the final argument; it must be exported as $PROMPT.
# Any framing a transcript needs goes in $NOTE, so a committed transcript is this
# script's stdout with nothing added to it by hand.
set -u
label="$1"; sba="$2"; sbb="$3"; shift 3
[ "${1:-}" = "--" ] && shift
[ -n "${NOTE:-}" ] && printf '%s\n' "$NOTE" | sed 's/^/# /'
echo "[$label] at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[$label] argv=$* <prompt>"
for sb in "$sba" "$sbb"; do
  ( cd "$sb" || exit 1
    args=(); for a in "$@"; do a="${a//\{\}/$sb}"; args+=("${a//\{XDG\}/${XDG_DIR:-}}"); done
    s=$(date +%s)
    # The event stream goes to its own file, NOT into a field of .result: it is
    # multi-line, so embedding it would leave only its first line readable and
    # the session-id scan below would silently see one event instead of all.
    # stdin from /dev/null, as the daemon gives a turn whose prompt is an
    # argument. Without it `codex exec` prints "Reading additional input from
    # stdin..." and waits there AFTER emitting turn.completed, so whether a
    # trial ever ends depends on what the operator's shell handed the script.
    "${args[@]}" "$PROMPT" > "$sb/.stream" 2>"$sb/.stderr" < /dev/null; rc=$?
    printf '%s\t%s\t%s\n' "$rc" "$(( $(date +%s) - s ))" "$sb" > "$sb/.result" ) &
done
wait
for sb in "$sba" "$sbb"; do
  IFS=$'\t' read -r rc secs _ < "$sb/.result"
  echo "[$label] $(basename "$sb") exit=$rc elapsed=${secs}s counter=$(cat "$sb/counter.txt") stderr_bytes=$(wc -c <"$sb/.stderr" | tr -d ' ')"
  # Every id this turn reported goes to .ids as well as to stdout, because the
  # VERDICT below compares the two turns' id SETS. Printing them is not enough:
  # a turn handed the other's session still writes a correct counter, so the
  # counters alone cannot see that half of the cross-talk this script looks for.
  python3 -c '
import json,sys
ids=set(); n=0
for ln in open(sys.argv[1]):
    ln=ln.strip()
    if not ln: continue
    try: d=json.loads(ln)
    except Exception: continue
    n+=1
    if not isinstance(d, dict): continue
    for k in ("session_id","sessionID","thread_id"):
        if d.get(k): ids.add(str(d[k]))
open(sys.argv[2],"w").write("".join(i+"\n" for i in sorted(ids)))
print(f"    {n} events; session ids seen:", sorted(ids) or "(none in stream)")' "$sb/.stream" "$sb/.ids"
done
# Two independent signals, both required for a pass:
#  - prompt-multitool.txt asks for counter.txt == 8 in each turn's OWN sandbox, so
#    a turn that edited the other's sandbox, or skipped the edit, shows up here;
#  - each turn must report at least one session id and the two sets must be
#    disjoint, so a turn resumed into its partner's session shows up here.
a=$(cat "$sba/counter.txt"); b=$(cat "$sbb/counter.txt")
shared=$(LC_ALL=C comm -12 <(LC_ALL=C sort -u "$sba/.ids") <(LC_ALL=C sort -u "$sbb/.ids") | tr '\n' ' ' | sed 's/ *$//')
problems=""
if [ "$a" != 8 ] || [ "$b" != 8 ]; then
  problems="counters a=$a b=$b (each should be 8)"
fi
for sb in "$sba" "$sbb"; do
  [ -s "$sb/.ids" ] && continue
  problems="${problems:+$problems; }$(basename "$sb") reported no session id"
done
if [ -n "$shared" ]; then
  problems="${problems:+$problems; }session id shared by both turns: $shared"
fi
if [ -z "$problems" ]; then
  echo "[$label] VERDICT: both turns edited their own sandbox to 8 and reported distinct session ids - no cross-talk"
else
  echo "[$label] VERDICT: UNEXPECTED $problems"
fi
