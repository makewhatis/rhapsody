#!/usr/bin/env bash
# prune-tmp-rhapsody.sh (STUDIO-1031) — delete stale `rhapsody-*` scratch dirs from the temp dir.
#
# Runner hygiene, not a test gate. Even with every guard in place, a killed test process (a CI
# cancellation, a SIGKILL, a hard test-binary crash) can leave its scratch dir behind, and a shared
# developer `$TMPDIR` is exactly where such a backlog grew to six figures before. This prunes
# entries older than a day so a leak can never build back up; the CI leak gate
# (`check-tmp-leaks.sh`) keeps the immediate run honest, and this keeps the machine honest.
#
# Install as a daily launchd job on the Mac runners — see `harness/ci/README.md`. Never write it
# into anyone's dotfiles; the plist in that directory is a template the operator installs.
#
# Usage: prune-tmp-rhapsody.sh
# Env:   TMPDIR                  scanned instead of /tmp when set
#        RHAPSODY_TMP_PRUNE_DAYS  age threshold in whole days (default 1 => older than ~24h)
set -euo pipefail

tmp="${TMPDIR:-/tmp}"
tmp="${tmp%/}"
if [ ! -d "$tmp" ]; then
    exit 0
fi

days="${RHAPSODY_TMP_PRUNE_DAYS:-1}"
# `-mtime +0` is "older than 24h", so the threshold is `+$((days - 1))` for a whole-day `days`.
threshold=$((days > 0 ? days - 1 : 0))

find "$tmp" -maxdepth 1 -name 'rhapsody-*' -type d -mtime "+${threshold}" -print -exec rm -rf {} + 2>/dev/null || true
