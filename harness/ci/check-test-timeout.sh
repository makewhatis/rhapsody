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
import os
import re
import signal
import subprocess
import sys
import time

name = "testutil::deliberately_hanging_timeout_canary"
started = time.monotonic()
process = subprocess.Popen(
    ["cargo", "nextest", "run", "--binaries-metadata", sys.argv[1],
     "--profile", "ci", "-E", f"test(={name})"],
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True,
)
try:
    output, _ = process.communicate(timeout=60)
except subprocess.TimeoutExpired:
    # Kill the canary as well as nextest if its process-level timeout stops working.
    os.killpg(process.pid, signal.SIGKILL)
    output, _ = process.communicate()
    print(output, end="")
    sys.exit("timeout canary: nextest exceeded the 60s wall-clock backstop")
elapsed = time.monotonic() - started
print(output, end="")
assert process.returncode == 100, f"expected test-failure exit 100, got {process.returncode}"
timeout = re.search(
    rf"^\s*TIMEOUT\s+\[\s*([0-9]+(?:\.[0-9]+)?)s\]\s+[^\n]*[ \t]{re.escape(name)}[ \t]*$",
    output, re.MULTILINE,
)
assert timeout is not None, "must name the timed-out test with its duration"
duration = float(timeout.group(1))
assert duration < 5, f"blocking test exceeded its 1s limit + termination margin: {duration:.3f}s"
assert elapsed < 60, f"nextest exceeded the 60s wall-clock backstop: {elapsed:.2f}s"
print(f"timeout canary: PASS (named blocking test terminated in {duration:.3f}s; nextest took {elapsed:.2f}s)")
PY
