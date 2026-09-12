#!/bin/bash
# conctest.sh <label> <sandbox-a> <sandbox-b> -- <argv...>
#
# Item 4 of the spike: run two turns of one harness AT ONCE against that
# harness's shared global state directory, and report whether either turn is
# corrupted by the other. Each turn gets its own sandbox, so a turn that reads
# the other's files, or a session that is handed the other's history, shows up
# as a wrong counter or a shared session id.
#
# `{}` in argv is replaced with that turn's own sandbox path (opencode --dir).
# The prompt is appended as the final argument; it must be exported as $PROMPT.
set -u
label="$1"; sba="$2"; sbb="$3"; shift 3
[ "${1:-}" = "--" ] && shift
echo "[$label] at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[$label] argv=$* <prompt>"
for sb in "$sba" "$sbb"; do
  ( cd "$sb" || exit 1
    args=(); for a in "$@"; do args+=("${a//\{\}/$sb}"); done
    s=$(date +%s)
    # The event stream goes to its own file, NOT into a field of .result: it is
    # multi-line, so embedding it would leave only its first line readable and
    # the session-id scan below would silently see one event instead of all.
    "${args[@]}" "$PROMPT" > "$sb/.stream" 2>"$sb/.stderr"; rc=$?
    printf '%s\t%s\t%s\n' "$rc" "$(( $(date +%s) - s ))" "$sb" > "$sb/.result" ) &
done
wait
for sb in "$sba" "$sbb"; do
  IFS=$'\t' read -r rc secs _ < "$sb/.result"
  echo "[$label] $(basename "$sb") exit=$rc elapsed=${secs}s counter=$(cat "$sb/counter.txt") stderr_bytes=$(wc -c <"$sb/.stderr" | tr -d ' ')"
  python3 -c '
import json,sys
ids=set(); n=0
for ln in open(sys.argv[1]):
    ln=ln.strip()
    if not ln: continue
    try: d=json.loads(ln)
    except Exception: continue
    n+=1
    for k in ("session_id","sessionID","thread_id"):
        if d.get(k): ids.add(d[k])
print(f"    {n} events; session ids seen:", sorted(ids) or "(none in stream)")' "$sb/.stream"
done
# prompt-multitool.txt asks for counter.txt == 8 in each turn's OWN sandbox.
# A turn that edited the other's sandbox, or skipped the edit, shows up here.
a=$(cat "$sba/counter.txt"); b=$(cat "$sbb/counter.txt")
if [ "$a" = 8 ] && [ "$b" = 8 ]; then
  echo "[$label] VERDICT: both turns edited their own sandbox to 8 - no cross-talk"
else
  echo "[$label] VERDICT: UNEXPECTED counters a=$a b=$b (each should be 8)"
fi
