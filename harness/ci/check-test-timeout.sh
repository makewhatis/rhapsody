#!/usr/bin/env bash
# Build outside the measured window, then prove a blocking test fails under the shipped CI profile.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
metadata="$(mktemp)"
trap 'rm -f "$metadata"' EXIT
cargo nextest list -p rhapsodyd --features test-timeout-canary --profile ci \
  --list-type binaries-only --message-format json >"$metadata"
python3 - "$metadata" <<'PY'
import subprocess
import sys
import time

name = "testutil::deliberately_hanging_timeout_canary"
started = time.monotonic()
result = subprocess.run(
    ["cargo", "nextest", "run", "--binaries-metadata", sys.argv[1],
     "--profile", "ci", "-E", f"test(={name})"],
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=15,
)
elapsed = time.monotonic() - started
print(result.stdout, end="")
assert result.returncode == 100, f"expected test-failure exit 100, got {result.returncode}"
assert "TIMEOUT" in result.stdout and name in result.stdout, "must name the timed-out test"
assert elapsed < 10, f"blocking test exceeded its 1s limit + startup margin: {elapsed:.2f}s"
print(f"timeout canary: PASS (named blocking test terminated in {elapsed:.2f}s)")
PY
