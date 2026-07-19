#!/usr/bin/env bash
# Pure-shell test for render-cask.sh (TRA-241). No network, no Ruby, no brew — just asserts the
# rendered Homebrew cask text, mirroring the Go reference's render_cask_test.sh (pared down to the
# single stable channel). Run from anywhere: `./render_cask_test.sh`. Also driven by
# desktop/src-tauri/tests/packaging_gate.rs so it runs in the `desktop` CI job's `cargo test`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RENDER="$SCRIPT_DIR/render-cask.sh"

VERSION="0.2.0"
SHA256="02e4ed1cb9089500830174661feedf01fdb193b8f0097280adb320fc823b77af"
# A second, distinct valid (version, sha256) pair for the substitution + no-leak assertions.
VERSION2="1.2.3"
SHA256_2="1111111111111111111111111111111111111111111111111111111111111111"

# The url stanza, asserted verbatim: it points at the github.com release asset and keeps `#{version}`
# LITERAL (Homebrew interpolates it at install time — the renderer must NOT shell-expand it).
URL='url "https://github.com/makewhatis/rhapsody/releases/download/v#{version}/Rhapsody.dmg"'

fails=0
pass() { printf 'ok   - %s\n' "$1"; }
fail() { printf 'FAIL - %s\n' "$1"; fails=$((fails + 1)); }

assert_contains() {
  # $1 = haystack, $2 = needle (literal, fgrep), $3 = description
  if printf '%s\n' "$1" | grep -qF -- "$2"; then
    pass "$3"
  else
    fail "$3 (missing: $2)"
  fi
}

assert_not_contains() {
  # $1 = haystack, $2 = needle (literal, fgrep), $3 = description
  if printf '%s\n' "$1" | grep -qF -- "$2"; then
    fail "$3 (unexpectedly present: $2)"
  else
    pass "$3"
  fi
}

assert_count() {
  # $1 = haystack, $2 = needle (literal, fgrep), $3 = expected line count, $4 = description
  count="$(printf '%s\n' "$1" | grep -cF -- "$2" || true)"
  if [ "$count" -eq "$3" ]; then
    pass "$4"
  else
    fail "$4 (expected $3 occurrence(s) of '$2', got $count)"
  fi
}

# --- the rendered stable cask -------------------------------------------------
out="$("$RENDER" "$VERSION" "$SHA256")"
assert_contains "$out" 'cask "rhapsody" do'                                    'declares the rhapsody cask'
assert_contains "$out" "version \"$VERSION\""                                  'emits the version'
assert_contains "$out" "sha256 \"$SHA256\""                                    'emits the sha256'
assert_contains "$out" "$URL"                                                  'url points at the github release with #{version} interpolation'
assert_contains "$out" 'name "Rhapsody"'                                       'emits the human name'
assert_contains "$out" 'desc "Supervises the rhapsodyd daemon and shows its dashboard"' 'emits the desc'
assert_contains "$out" 'homepage "https://github.com/makewhatis/rhapsody"'     'emits the homepage'
assert_contains "$out" 'auto_updates true'                                     'declares auto_updates (P11 in-app updater coexistence)'
assert_contains "$out" 'depends_on macos: :catalina'                           'depends on macOS Catalina or later'
assert_contains "$out" 'app "Rhapsody.app"'                                    'installs Rhapsody.app'
assert_contains "$out" 'end'                                                   'closes the cask block'

# zap removes BOTH the runtime home and the login-keychain credential item.
assert_contains "$out" 'zap script: {'                                         'zap has a script directive'
assert_contains "$out" 'delete-generic-password'                              'zap removes the keychain generic password'
assert_contains "$out" 'is.makewhat.rhapsody'                                  'zap targets the is.makewhat.rhapsody keychain service'
assert_contains "$out" 'trash:  "~/.rhapsody"'                                 'zap trashes the ~/.rhapsody runtime home'

# `#{version}` is Homebrew's interpolation, emitted LITERALLY — never shell-expanded by the renderer.
assert_contains "$out" 'v#{version}/Rhapsody.dmg'                              'keeps #{version} literal in the url'

# Deliberate simplifications vs the Go reference's multi-channel render-cask.sh, pinned so a future
# edit cannot silently reintroduce them:
#   - no `verified:` (github url + github homepage share a domain -> brew audit rejects it),
#   - no internal `dist.*.interval.team` host (we serve from GitHub Releases),
#   - no `conflicts_with` / `@rc` / `@feature` channels (single stable cask),
#   - no residual "Symphony" branding (this is the Rhapsody cask).
assert_not_contains "$out" 'verified'                                          'no verified: stanza (keeps brew audit clean)'
assert_not_contains "$out" 'interval.team'                                     'no internal dist host'
assert_not_contains "$out" 'conflicts_with'                                    'no conflicts_with (single channel)'
assert_not_contains "$out" 'rhapsody@'                                         'no @rc/@feature channel token'
assert_not_contains "$out" 'Symphony'                                          'no residual Symphony branding'

# Structural single-ness: exactly one url and one zap stanza.
assert_count "$out" 'url "https://github.com/makewhatis/rhapsody' 1            'exactly one url stanza'
assert_count "$out" 'zap script:' 1                                           'exactly one zap stanza'

# --- version + sha256 substitution (the auto-bump path) -----------------------
# A different (version, sha256) must flow through with NO leak of the previous pair, and `#{version}`
# must STILL be literal (proving it is brew's interpolation, not a shell variable that got expanded).
out2="$("$RENDER" "$VERSION2" "$SHA256_2")"
assert_contains "$out2" "version \"$VERSION2\""                               'substitutes a new version'
assert_contains "$out2" "sha256 \"$SHA256_2\""                               'substitutes a new sha256'
assert_not_contains "$out2" "$VERSION"                                        'does not leak the previous version'
assert_not_contains "$out2" "$SHA256"                                         'does not leak the previous sha256'
assert_contains "$out2" 'v#{version}/Rhapsody.dmg'                            'still keeps #{version} literal after substitution'
assert_not_contains "$out2" "v$VERSION2/Rhapsody.dmg"                         'never expands #{version} to the concrete version'

# --- argument validation ------------------------------------------------------
if "$RENDER" "$VERSION" >/dev/null 2>&1; then
  fail 'exits non-zero when the sha256 argument is missing'
else
  pass 'exits non-zero when the sha256 argument is missing'
fi

if "$RENDER" >/dev/null 2>&1; then
  fail 'exits non-zero when all arguments are missing'
else
  pass 'exits non-zero when all arguments are missing'
fi

if "$RENDER" "$VERSION" "$SHA256" extra >/dev/null 2>&1; then
  fail 'exits non-zero with too many arguments'
else
  pass 'exits non-zero with too many arguments'
fi

# Malformed versions (not a dotted x.y.z) are rejected.
for bad in "1.2" "abc" "v0.2.0" "0.2.0 " ""; do
  if "$RENDER" "$bad" "$SHA256" >/dev/null 2>&1; then
    fail "exits non-zero for a malformed version ('$bad')"
  else
    pass "exits non-zero for a malformed version ('$bad')"
  fi
done

# Malformed sha256 values (wrong length, non-hex, uppercase) are rejected.
for bad in "deadbeef" "z111111111111111111111111111111111111111111111111111111111111111" "02E4ED1CB9089500830174661FEEDF01FDB193B8F0097280ADB320FC823B77AF"; do
  if "$RENDER" "$VERSION" "$bad" >/dev/null 2>&1; then
    fail "exits non-zero for a malformed sha256 ('$bad')"
  else
    pass "exits non-zero for a malformed sha256 ('$bad')"
  fi
done

# --- summary ------------------------------------------------------------------
if [ "$fails" -ne 0 ]; then
  printf '\n%d assertion(s) failed\n' "$fails" >&2
  exit 1
fi
printf '\nall assertions passed\n'
