#!/usr/bin/env bash
# Pure arg-construction tests for notarize.sh's sourceable lib — no Apple/network/xcrun calls.
# Parity port of $REF/desktop/scripts/notarize_args_test.sh.
#
# Covers the dual credential modes (local NOTARY_PROFILE keychain profile vs CI App Store Connect
# API key), their precedence, the loud partial-config error, and the base64 → chmod-600 temp-file
# key decoding. Each case runs in its own subshell with a scrubbed notary env so cases can't leak
# into each other. Kept bash-3.2 compatible (macOS system bash).
#
# Usage: bash desktop/scripts/notarize_args_test.sh
#
# shellcheck disable=SC2030,SC2031 # env mutations being subshell-local is the isolation mechanism
set -euo pipefail

cd "$(dirname "$0")"

# Scratch TMPDIR so resolve_asc_key's temp keys are contained and cleaned up.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/notarize-test.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

FAILS=0
check_eq() { # name expected actual
  if [ "$2" = "$3" ]; then
    echo "ok   $1"
  else
    echo "FAIL $1: expected [$2], got [$3]" >&2
    FAILS=$((FAILS + 1))
  fi
}

# scrub: unset every notary-related variable, then source the lib. Run inside a subshell.
scrub_and_source() {
  unset NOTARY_PROFILE NOTARY_KEYCHAIN ASC_KEY_ID ASC_ISSUER_ID ASC_API_KEY_P8 ASC_API_KEY_P8_BASE64
  # shellcheck disable=SC1091 # sources the sibling notarize.sh, linted on its own
  . ./notarize.sh --lib-only
}

# --- notary_auth_args ---

# 1. Local keychain-profile mode (the unchanged default for humans). Profile names may contain
#    spaces, hence the one-arg-per-line output contract.
out=$(
  scrub_and_source
  NOTARY_PROFILE="rhapsody notary" notary_auth_args
)
check_eq "profile mode" "--keychain-profile
rhapsody notary" "$out"

# 1b. NOTARY_KEYCHAIN set in profile mode appends --keychain so the profile resolves from the
#     dedicated signing keychain deterministically (TRA-257), not the login-keychain default.
out=$(
  scrub_and_source
  NOTARY_PROFILE=rhapsody-notary NOTARY_KEYCHAIN="$HOME/Library/Keychains/rhapsody-signing.keychain-db" notary_auth_args
)
check_eq "profile mode + NOTARY_KEYCHAIN" "--keychain-profile
rhapsody-notary
--keychain
$HOME/Library/Keychains/rhapsody-signing.keychain-db" "$out"

# 1c. NOTARY_KEYCHAIN is ignored in API-key mode (the ASC_* branch is untouched by TRA-257).
out=$(
  scrub_and_source
  ASC_KEY_ID=KEYID123 ASC_ISSUER_ID=issuer-uuid ASC_API_KEY_P8=/tmp/k.p8 \
    NOTARY_KEYCHAIN=/some/kc.keychain-db notary_auth_args
)
check_eq "NOTARY_KEYCHAIN ignored in api-key mode" "--key
/tmp/k.p8
--key-id
KEYID123
--issuer
issuer-uuid" "$out"

# 2. CI API-key mode: full ASC trio emits --key/--key-id/--issuer.
out=$(
  scrub_and_source
  ASC_KEY_ID=KEYID123 ASC_ISSUER_ID=issuer-uuid ASC_API_KEY_P8=/tmp/k.p8 notary_auth_args
)
check_eq "api-key mode" "--key
/tmp/k.p8
--key-id
KEYID123
--issuer
issuer-uuid" "$out"

# 3. Precedence: the API key wins when both modes are configured (CI sets only ASC_*, so this
#    only matters for a human with leftover env — the explicit trio is the stronger signal).
out=$(
  scrub_and_source
  NOTARY_PROFILE=prof ASC_KEY_ID=K ASC_ISSUER_ID=I ASC_API_KEY_P8=/tmp/k.p8 notary_auth_args
)
check_eq "api-key wins over profile" "--key
/tmp/k.p8
--key-id
K
--issuer
I" "$out"

# 4. Neither mode configured: rc 1 (callers skip notarization), nothing on stdout.
rc=0
out=$(
  scrub_and_source
  notary_auth_args
) || rc=$?
check_eq "unconfigured rc" "1" "$rc"
check_eq "unconfigured output" "" "$out"

# 5. Partial API-key config is a loud error (rc 2), NOT a silent fallback to the profile.
rc=0
out=$(
  scrub_and_source
  NOTARY_PROFILE=prof ASC_KEY_ID=K notary_auth_args 2>/dev/null
) || rc=$?
check_eq "partial asc config rc" "2" "$rc"
check_eq "partial asc config output" "" "$out"

# 6. A base64-only key without the rest of the trio is also partial config.
rc=0
out=$(
  scrub_and_source
  ASC_API_KEY_P8_BASE64=AAAA notary_auth_args 2>/dev/null
) || rc=$?
check_eq "base64-only partial rc" "2" "$rc"

# --- resolve_asc_key ---

# 7. Base64 key decodes to a chmod-600 temp file and exports ASC_API_KEY_P8.
key_b64=$(printf '%s' "fake p8 contents" | base64)
out=$(
  scrub_and_source
  export TMPDIR="$SCRATCH"
  export ASC_API_KEY_P8_BASE64="$key_b64"
  resolve_asc_key
  printf '%s\n' "${ASC_API_KEY_P8:-UNSET}"
  cat "$ASC_API_KEY_P8"
)
keyfile=$(printf '%s\n' "$out" | sed -n 1p)
content=$(printf '%s\n' "$out" | sed -n 2p)
case "$keyfile" in
  "$SCRATCH"/*) echo "ok   resolve_asc_key writes under TMPDIR" ;;
  *) echo "FAIL resolve_asc_key path: got [$keyfile]" >&2; FAILS=$((FAILS + 1)) ;;
esac
check_eq "resolve_asc_key decoded content" "fake p8 contents" "$content"
perms=$(stat -f '%Lp' "$keyfile" 2>/dev/null || stat -c '%a' "$keyfile" 2>/dev/null || echo none)
check_eq "resolve_asc_key key perms" "600" "$perms"

# 8. A second decode in the SAME TMPDIR must also work (BSD mktemp only substitutes TRAILING
#    Xs — a template with a suffix after the Xs creates a literal name that collides on rerun).
out=$(
  scrub_and_source
  export TMPDIR="$SCRATCH"
  export ASC_API_KEY_P8_BASE64="$key_b64"
  resolve_asc_key
  cat "$ASC_API_KEY_P8"
)
check_eq "resolve_asc_key second decode in same TMPDIR" "fake p8 contents" "$out"

# 9. An explicit ASC_API_KEY_P8 path is left alone (no decode, no temp file).
out=$(
  scrub_and_source
  export ASC_API_KEY_P8=/already/there.p8 ASC_API_KEY_P8_BASE64="$key_b64"
  resolve_asc_key
  printf '%s' "$ASC_API_KEY_P8"
)
check_eq "resolve_asc_key keeps explicit path" "/already/there.p8" "$out"

# 10. With no key env at all, resolve_asc_key is a no-op.
out=$(
  scrub_and_source
  resolve_asc_key
  printf '%s' "${ASC_API_KEY_P8:-UNSET}"
)
check_eq "resolve_asc_key no-op" "UNSET" "$out"

# --- notarize_target_kind (bundle-vs-flat branch selection, TRA-258) ---
# notarize.sh now notarizes both the .app bundle (zip -> submit the zip -> staple the bundle) and a
# .dmg/.pkg (submit + staple the file). notarize_target_kind picks the branch by extension alone, so
# it is unit-testable here without a real bundle, xcrun, or the network.

# 11. A .app target selects the bundle path.
out=$(
  scrub_and_source
  notarize_target_kind /path/to/Rhapsody.app
)
check_eq "app -> bundle" "bundle" "$out"

# 12. A .dmg target selects the flat path (unchanged submit + staple the file directly).
out=$(
  scrub_and_source
  notarize_target_kind /path/to/Rhapsody.dmg
)
check_eq "dmg -> flat" "flat" "$out"

# 13. A .pkg target also selects the flat path.
out=$(
  scrub_and_source
  notarize_target_kind /path/to/Installer.pkg
)
check_eq "pkg -> flat" "flat" "$out"

# 14. An unrecognized target is a loud error (rc 2), not a silent skip — a typo must never quietly
#     submit/staple the wrong thing.
rc=0
out=$(
  scrub_and_source
  notarize_target_kind /path/to/Rhapsody.zip 2>/dev/null
) || rc=$?
check_eq "unknown target rc" "2" "$rc"
check_eq "unknown target output" "" "$out"

if [ "$FAILS" -gt 0 ]; then
  echo "FAIL: $FAILS test(s) failed" >&2
  exit 1
fi
echo "PASS"
