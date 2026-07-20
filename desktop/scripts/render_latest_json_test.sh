#!/usr/bin/env bash
# Pure-shell test for render-latest-json.sh (TRA-261, P11-U2). Asserts the emitted Tauri updater
# manifest is valid JSON carrying exactly the fields tauri-plugin-updater consumes, that inputs flow
# through with no leak, and that malformed inputs fail loud. No network, no signing key, no Ruby/brew
# — just jq. Run from anywhere: `./render_latest_json_test.sh`. Also driven by
# desktop/src-tauri/tests/packaging_gate.rs so it runs in the `desktop` CI job's `cargo test`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RENDER="$SCRIPT_DIR/render-latest-json.sh"

VERSION="0.4.0"
PUB_DATE="2026-07-20T04:52:28Z"
# A base64 blob shaped like a real `cargo tauri signer sign` .sig (single line, base64 alphabet).
SIGNATURE="dW50cnVzdGVkIGNvbW1lbnQ6c2lnCg=="
URL="https://github.com/makewhatis/rhapsody/releases/download/v0.4.0/Rhapsody.app.tar.gz"

fails=0
pass() { printf 'ok   - %s\n' "$1"; }
fail() { printf 'FAIL - %s\n' "$1"; fails=$((fails + 1)); }

# assert_jq <jq-filter> <expected> <description>: run the filter over the rendered manifest.
assert_jq() {
  local got
  got="$(printf '%s' "$out" | jq -r "$1")"
  if [ "$got" = "$2" ]; then
    pass "$3"
  else
    fail "$3 (filter '$1' expected '$2' got '$got')"
  fi
}

# --- the rendered manifest ----------------------------------------------------
out="$("$RENDER" "$VERSION" "$PUB_DATE" "$SIGNATURE" "$URL")"

if printf '%s' "$out" | jq -e . >/dev/null 2>&1; then
  pass 'emits valid JSON'
else
  fail 'emits valid JSON'
fi

assert_jq '.version'                              "$VERSION"        'carries the version'
assert_jq '.pub_date'                             "$PUB_DATE"       'carries the pub_date'
assert_jq '.platforms."darwin-aarch64".signature' "$SIGNATURE"      'carries the darwin-aarch64 signature'
assert_jq '.platforms."darwin-aarch64".url'       "$URL"           'points the darwin-aarch64 url at the tar.gz asset'
assert_jq '(.notes | length > 0) | tostring'      'true'           'has non-empty notes'

# darwin-aarch64 is the ONLY platform (Rhapsody ships Apple-Silicon-only).
assert_jq '.platforms | keys | length | tostring' '1'              'exactly one platform (Apple-Silicon-only)'
assert_jq '.platforms | keys[0]'                  'darwin-aarch64' 'the platform key is darwin-aarch64'

# The default note references the version; an explicit note flows through verbatim.
assert_jq '.notes | contains("0.4.0") | tostring' 'true'           'default note references the version'
out="$("$RENDER" "$VERSION" "$PUB_DATE" "$SIGNATURE" "$URL" "explicit note here")"
assert_jq '.notes'                                'explicit note here' 'passes an explicit note through'

# The updater url must NOT leak into the signature (a copy/paste swap would break verification).
out="$("$RENDER" "$VERSION" "$PUB_DATE" "$SIGNATURE" "$URL")"
assert_jq '.platforms."darwin-aarch64".signature | contains("http") | tostring' 'false' 'signature is not the url'

# --- argument arity -----------------------------------------------------------
check_rejects() {
  # $1 = description; $2.. = args passed to the renderer (expected to fail)
  local desc="$1"; shift
  if "$RENDER" "$@" >/dev/null 2>&1; then
    fail "$desc"
  else
    pass "$desc"
  fi
}

check_rejects 'rejects a missing url (3 args)'      "$VERSION" "$PUB_DATE" "$SIGNATURE"
check_rejects 'rejects no args'
check_rejects 'rejects too many args'               "$VERSION" "$PUB_DATE" "$SIGNATURE" "$URL" note extra

# --- shape validation ---------------------------------------------------------
for bad in "1.2" "v0.4.0" "abc" "0.4.0 " ""; do
  check_rejects "rejects a malformed version ('$bad')" "$bad" "$PUB_DATE" "$SIGNATURE" "$URL"
done

for bad in "2026-07-20" "2026-07-20T04:52:28" "2026-07-20 04:52:28Z" "not-a-date" ""; do
  check_rejects "rejects a malformed pub_date ('$bad')" "$VERSION" "$bad" "$SIGNATURE" "$URL"
done

for bad in "has space" "not*base64" "line1
line2" ""; do
  check_rejects "rejects a malformed signature ('$bad')" "$VERSION" "$PUB_DATE" "$bad" "$URL"
done

for bad in "http://x/Rhapsody.app.tar.gz" "https://x/Rhapsody.dmg" "ftp://x/a.tar.gz" "https://x/a.tar.gz has space" ""; do
  check_rejects "rejects a malformed url ('$bad')" "$VERSION" "$PUB_DATE" "$SIGNATURE" "$bad"
done

# --- summary ------------------------------------------------------------------
if [ "$fails" -ne 0 ]; then
  printf '\n%d assertion(s) failed\n' "$fails" >&2
  exit 1
fi
printf '\nall assertions passed\n'
