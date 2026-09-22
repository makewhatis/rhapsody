#!/bin/bash
# PB0 capture: drive the REAL pinned OpenCode binary against the committed loopback fake provider
# and record the request snapshots the broker contract is pinned to (STUDIO-995).
#
# Operator-machine only, exactly like `make fixtures`: it needs a real `opencode` 1.18.30 and the
# `python3` the harness already assumes. CI never runs this; CI pins the committed output through
# `crates/agent/tests/opencode_broker_fixture.rs`. No real provider and no paid credential is used
# — every request goes to 127.0.0.1 and carries a fake capability.
#
#   OPENCODE_BIN=/path/to/opencode harness/harness-spike/opencode/broker/capture.sh
#
# Determinism: two runs of this script against the same binary must produce byte-identical
# `requests/*.json`. The unpredictable per-session provider id, the capability, the ephemeral
# port, and the scratch paths are all normalized to placeholders; nothing else varies.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OPENCODE_BIN="${OPENCODE_BIN:-$(command -v opencode || true)}"
if [[ -z "$OPENCODE_BIN" || ! -x "$OPENCODE_BIN" ]]; then
  echo "capture.sh: set OPENCODE_BIN to the pinned opencode 1.18.30 executable" >&2
  exit 2
fi

PINNED_OPENCODE="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["opencode_version"])' "$HERE/compatibility.json")"
ACTUAL_OPENCODE="$("$OPENCODE_BIN" --version 2>/dev/null | head -1 | tr -d '[:space:]')"
if [[ "$ACTUAL_OPENCODE" != "$PINNED_OPENCODE" ]]; then
  echo "capture.sh: opencode $ACTUAL_OPENCODE is not the pinned $PINNED_OPENCODE — refusing" >&2
  exit 3
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/pb0-capture.XXXXXX")"
PROVIDER="rhapsody-$(python3 -c 'import os,base64;print(base64.urlsafe_b64encode(os.urandom(16)).decode().rstrip("="))')"
CAPABILITY="rhp-fake-capability-$(python3 -c 'import os,base64;print(base64.urlsafe_b64encode(os.urandom(12)).decode().rstrip("="))')"
cleanup() {
  [[ -n "${PROVIDER_PID:-}" ]] && kill "$PROVIDER_PID" 2>/dev/null || true
  rm -rf "$SCRATCH"
}
trap cleanup EXIT

write_config() {
  local base_url="$1"
  python3 - "$PROVIDER" "$base_url" > "$SCRATCH/config.json" <<'PY'
import json, sys
provider, base_url = sys.argv[1:3]
# The managed generated config of provider-broker-design.md §9.1: one OpenAI-compatible provider,
# `enabled_providers` exactly that id, top-level/small/compaction/all native model-calling agents on
# it, sharing disabled, title generation disabled, and the explicit built-in `build` agent.
model = f"{provider}/probe-model"
print(json.dumps({
    "enabled_providers": [provider],
    "provider": {
        provider: {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Rhapsody Broker",
            "options": {"baseURL": base_url},
            "models": {"probe-model": {"name": "probe-model"}},
        }
    },
    "model": model,
    "small_model": model,
    "default_agent": "build",
    "share": "disabled",
    "agent": {
        "title": {"disable": True},
        "build": {"model": model},
        "plan": {"model": model},
        "general": {"model": model},
        "explore": {"model": model},
        "compaction": {"model": model},
        "summary": {"model": model},
    },
}, indent=2))
PY
}

run_scenario() {
  local scenario="$1" agent="$2" prompt="$3"
  local record="$HERE/requests/${scenario}.json"
  local raw="$SCRATCH/${scenario}.raw.json"

  python3 "$HERE/fake_provider.py" --scenario "$scenario" --record "$raw" \
    --capability "$CAPABILITY" > "$SCRATCH/${scenario}.provider.log" 2>&1 &
  PROVIDER_PID=$!
  local port=""
  for _ in $(seq 1 50); do
    port="$(sed -n 's/^LISTENING //p' "$SCRATCH/${scenario}.provider.log" | head -1)"
    [[ -n "$port" ]] && break
    sleep 0.1
  done
  if [[ -z "$port" ]]; then
    echo "capture.sh: fake provider did not start for $scenario" >&2
    exit 4
  fi

  write_config "http://127.0.0.1:${port}/v1"
  local auth="{\"$PROVIDER\":{\"type\":\"api\",\"key\":\"$CAPABILITY\"}}"

  local work="$SCRATCH/work-${scenario}"
  mkdir -p "$work" "$SCRATCH/home-${scenario}" "$SCRATCH/xdg-${scenario}" "$SCRATCH/cfg-${scenario}"

  local exit_code=0
  perl -e 'alarm 90; exec @ARGV' env -i \
    PATH=/usr/bin:/bin \
    HOME="$SCRATCH/home-${scenario}" \
    XDG_DATA_HOME="$SCRATCH/xdg-${scenario}" \
    OPENCODE_CONFIG_CONTENT="$(cat "$SCRATCH/config.json")" \
    OPENCODE_AUTH_CONTENT="$auth" \
    OPENCODE_CONFIG_DIR="$SCRATCH/cfg-${scenario}" \
    OPENCODE_DISABLE_PROJECT_CONFIG=1 \
    OPENCODE_DISABLE_EXTERNAL_SKILLS=1 \
    OPENCODE_DISABLE_MODELS_FETCH=1 \
    OPENCODE_DISABLE_AUTOUPDATE=1 \
    OPENCODE_DISABLE_SHARE=1 \
    OPENCODE_PRINT_LOGS=0 \
    OPENCODE_LOG_LEVEL=INFO \
    "$OPENCODE_BIN" run --format json --pure --auto --dir "$work" --agent "$agent" \
    -m "$PROVIDER/probe-model" "$prompt" \
    > "$SCRATCH/${scenario}.jsonl" 2> "$SCRATCH/${scenario}.stderr" || exit_code=$?

  kill "$PROVIDER_PID" 2>/dev/null || true
  wait "$PROVIDER_PID" 2>/dev/null || true
  unset PROVIDER_PID

  python3 - "$HERE/compatibility.json" "$raw" "$record" "$scenario" "$exit_code" \
    "$PROVIDER" "$CAPABILITY" "$work" "$agent" <<'PY'
import json, sys
compat_path, raw_path, out_path, scenario, exit_code, provider, capability, work, agent = sys.argv[1:10]

with open(compat_path) as fh:
    compatibility = json.load(fh)
with open(raw_path) as fh:
    raw = json.load(fh)

def sanitize(value):
    if isinstance(value, str):
        return (
            value.replace(capability, "<CAPABILITY>")
            .replace(provider, "<PROVIDER>")
            .replace(work, "<WORK>")
        )
    if isinstance(value, list):
        return [sanitize(v) for v in value]
    if isinstance(value, dict):
        return {k: sanitize(v) for k, v in value.items()}
    return value

requests = sanitize(raw)
for request in requests:
    auth = request.get("headers", {}).get("authorization", "")
    if auth != "Bearer <CAPABILITY>":
        raise SystemExit(
            f"capture.sh: {scenario} did not carry the managed capability (saw {auth!r}); "
            "refusing to commit a fixture that proves the wrong auth path"
        )

argv = [
    "run", "--format", "json", "--pure", "--auto", "--dir", "<WORK>",
    "--agent", agent, "-m", "<PROVIDER>/probe-model", "<PROMPT>",
]
doc = {
    "scenario": scenario,
    "compatibility": compatibility,
    "exit_code": int(exit_code),
    "argv": argv,
    "requests": requests,
}
with open(out_path, "w") as fh:
    json.dump(doc, fh, indent=2, sort_keys=True)
    fh.write("\n")
print(f"captured {scenario}: {len(requests)} request(s), exit {exit_code}")
PY
}

run_scenario happy build "Reply with P0_OK"
run_scenario retry build "Reply with P0_OK"
run_scenario auth build "Reply with P0_OK"
run_scenario compaction compaction "Summarize the session so far."
run_scenario subagent build "Delegate this to a subagent, then report P0_OK"

# The credential-free, config-free version probe (provider-broker-design.md §9.1). The pinned
# environment is exactly the one the probe function installs: an allow-listed PATH plus the
# discovery-disable flags and NO credential, HOME, or auth/config content. `--version` is the
# command the Rust probe in `crates/agent/src/opencode/probe.rs` runs.
env -i \
  PATH=/usr/bin:/bin \
  OPENCODE_DISABLE_PROJECT_CONFIG=1 \
  OPENCODE_DISABLE_EXTERNAL_SKILLS=1 \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  OPENCODE_DISABLE_AUTOUPDATE=1 \
  OPENCODE_DISABLE_SHARE=1 \
  OPENCODE_DISABLE_DEFAULT_PLUGINS=1 \
  "$OPENCODE_BIN" --version > "$HERE/probe.txt" 2>/dev/null

echo "capture.sh: wrote $HERE/requests/*.json and $HERE/probe.txt"
