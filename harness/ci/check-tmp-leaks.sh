#!/usr/bin/env bash
# check-tmp-leaks.sh (STUDIO-1031) — fail a test job that left scratch dirs in the temp dir.
#
# The bug: test helpers created `rhapsody-*` directories under the OS temp dir and never removed
# them, so a shared dev/CI `$TMPDIR` accumulated six figures of entries — slowing every create,
# lookup and exec, pinning `fseventsd`, and making short-timeout tests fail at random. The fix is a
# guard per helper; this check is what proves the guards hold. After `cargo test`, any `rhapsody-*`
# entry NEWER than the job's start marker is a leak that some test failed to remove.
#
# Only entries newer than the marker count. Older entries predate this run (another worktree's
# tests, or a leak from before the guards landed) and are not this run's failure to report; the Mac
# runner's historical backlog is cleared by the launchd pruner, not this gate — see
# `harness/ci/README.md`.
#
# Usage: check-tmp-leaks.sh <marker-file>   (touch the marker BEFORE the test run)
# Env:   TMPDIR   scanned instead of /tmp when set (macOS puts it in a private per-user dir)
# Exits 0 when clean or when the temp dir does not exist (nothing to scan); 1 when it names leaks.
set -euo pipefail

marker="${1:-}"
if [ -z "$marker" ] || [ ! -e "$marker" ]; then
    echo "check-tmp-leaks: usage: check-tmp-leaks.sh <marker-file> (the marker must exist)" >&2
    exit 2
fi

tmp="${TMPDIR:-/tmp}"
tmp="${tmp%/}"
if [ ! -d "$tmp" ]; then
    echo "check-tmp-leaks: $tmp does not exist; nothing to check"
    exit 0
fi

leaks="$(find "$tmp" -maxdepth 1 -name 'rhapsody-*' -newer "$marker" 2>/dev/null | sort || true)"
if [ -n "$leaks" ]; then
    echo "check-tmp-leaks: test scratch directories leaked into $tmp (newer than $marker):" >&2
    printf '%s\n' "$leaks" >&2
    echo "check-tmp-leaks: a guard is missing or was dropped early — bind the TempDir guard for the" >&2
    echo "                 test's lifetime, or return one from the helper. Set RHAPSODY_KEEP_TEST_DIRS=1" >&2
    echo "                 only to keep dirs on purpose while debugging." >&2
    exit 1
fi
echo "check-tmp-leaks: clean — no test scratch dirs newer than the marker in $tmp"
