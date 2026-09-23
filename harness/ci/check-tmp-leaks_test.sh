#!/usr/bin/env bash
# check-tmp-leaks_test.sh (STUDIO-1031) — pins check-tmp-leaks.sh by MAKING IT FIRE.
#
# A leak scan nobody has watched fire is not a guard: it converts "nobody checked" into "we checked
# and it is clean". This mutation check points the real `check-tmp-leaks.sh` at a scratch root,
# simulates a test run that leaks a `rhapsody-*` dir (the exact shape a reverted guard produces —
# a directory that exists AFTER the run), and asserts the scan reds and NAMES it. The green cases
# matter as much: a pre-existing (older) entry and a non-`rhapsody-` entry must NOT red, or the
# gate would be weakened by the first person an over-broad match inconvenienced.
#
# No dependencies beyond bash. Run from anywhere: `harness/ci/check-tmp-leaks_test.sh`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
check="$root/harness/ci/check-tmp-leaks.sh"
[ -x "$check" ] || { echo "FAIL - $check is missing or not executable"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
scan="$work/tmp"
marker="$work/marker"
mkdir -p "$scan"
touch "$marker"

fail=0
out=""
status=0

# run — runs the real check against the scratch root, capturing combined output + exit status.
# Deliberately not command substitution at the call site: a subshell's assignments would not survive.
run() {
    set +e
    out="$(TMPDIR="$scan" "$check" "$marker" 2>&1)"
    status=$?
    set -e
}

expect() {
    local desc="$1" want="$2"
    if [ "$status" -ne "$want" ]; then
        echo "FAIL - $desc: exit $status (want $want)"
        echo "$out"
        fail=1
    else
        echo "ok - $desc"
    fi
}

# 1. Nothing at all: clean.
run
expect "an empty temp dir is clean" 0

# 2. A pre-existing entry (older than the marker) is ignored — it is not this run's leak.
mkdir -p "$scan/rhapsody-orchestrator-1-1-0"
touch -t 202001010000 "$scan/rhapsody-orchestrator-1-1-0"
run
expect "an older rhapsody-* entry is ignored" 0

# 3. A newer entry that is NOT rhapsody-* is ignored — the gate must not over-match.
mkdir -p "$scan/something-else-1"
run
expect "a newer non-rhapsody entry is ignored" 0

# 4. The mutation: a `rhapsody-*` dir newer than the marker (what a reverted guard leaves behind)
#    must red the check AND be named in the output.
sleep 1
leaked="$scan/rhapsody-store-test-2-3-4"
mkdir -p "$leaked"
run
expect "a leaked rhapsody-* dir reds the check" 1
case "$out" in
    *"$leaked"*) echo "ok - the leak is named in the output" ;;
    *)
        echo "FAIL - the leaked dir is not named in the output:"
        echo "$out"
        fail=1
        ;;
esac

if [ "$fail" -ne 0 ]; then
    echo "check-tmp-leaks_test: FAILURES"
    exit 1
fi
echo "check-tmp-leaks_test: all cases passed"
