#!/usr/bin/env bash
# Gated notarization + stapling of the Rhapsody.dmg.
# Parity port of $REF/desktop/scripts/notarize.sh (Symphony.dmg -> Rhapsody.dmg).
#
# No-op (exit 0) unless notary credentials are configured, so an autonomous/unsigned build stays
# green. Gated INDEPENDENTLY of signing: if APPLE_SIGNING_IDENTITY is set but no notary credentials
# are, the build still produces a signed-but-unnotarized dmg. When configured, submits the (already
# signed) dmg to Apple, waits for the result, then staples the ticket so the installer validates
# offline. The app inside the dmg must already be Developer-ID signed (run sign.sh first) or Apple
# rejects the submission. See SIGNING.md.
#
# Two credential modes (the API key wins when both are set; a PARTIAL ASC_* trio is a loud error,
# never a silent fallback):
#
#   local (default):  NOTARY_PROFILE — a notarytool keychain profile name
#                     (created via `xcrun notarytool store-credentials <name> ...`). If
#                     NOTARY_KEYCHAIN is also set, the profile is resolved from THAT keychain
#                     (`--keychain`) rather than notarytool's login-keychain default — used in CI
#                     to read the profile from the dedicated rhapsody-signing keychain (TRA-257).
#   CI:               ASC_KEY_ID + ASC_ISSUER_ID + an App Store Connect API key, as a file path
#                     (ASC_API_KEY_P8) or base64 (ASC_API_KEY_P8_BASE64, decoded to a chmod-600
#                     temp file). Keychain profiles are created interactively per-machine, so this
#                     is the only mode that works on a throwaway runner.
#
# Usage: notarize.sh <out.dmg>
#        source notarize.sh --lib-only   # functions only (notarize_args_test.sh)
set -euo pipefail

# resolve_asc_key: when the App Store Connect API key arrives as base64 (CI secrets can't carry
# files), decode it to a chmod-600 temp file and export its path as ASC_API_KEY_P8. An explicitly
# set ASC_API_KEY_P8 always wins; without either, a no-op.
resolve_asc_key() {
  if [ -z "${ASC_API_KEY_P8:-}" ] && [ -n "${ASC_API_KEY_P8_BASE64:-}" ]; then
    # Trailing Xs only: BSD mktemp leaves a template with a suffix after the Xs literal,
    # which collides on the second run. notarytool accepts a key file without a .p8 extension.
    ASC_API_KEY_P8="$(mktemp "${TMPDIR:-/tmp}/rhapsody-asc-key.XXXXXX")"
    chmod 600 "$ASC_API_KEY_P8"
    printf '%s' "$ASC_API_KEY_P8_BASE64" | base64 --decode > "$ASC_API_KEY_P8"
    export ASC_API_KEY_P8
  fi
}

# notary_auth_args: print the notarytool auth args ONE PER LINE (values may contain spaces, e.g.
# a keychain profile name; callers rebuild an array with `while read`). Returns 0 with args on
# stdout; 1 when no credentials are configured (caller skips notarization); 2 on a partial
# API-key trio (caller must fail — half-set CI secrets should never silently skip or fall back).
# Run resolve_asc_key first so ASC_API_KEY_P8_BASE64 counts as the key being present.
notary_auth_args() {
  if [ -n "${ASC_KEY_ID:-}" ] || [ -n "${ASC_ISSUER_ID:-}" ] || [ -n "${ASC_API_KEY_P8:-}" ] || [ -n "${ASC_API_KEY_P8_BASE64:-}" ]; then
    if [ -n "${ASC_KEY_ID:-}" ] && [ -n "${ASC_ISSUER_ID:-}" ] && [ -n "${ASC_API_KEY_P8:-}" ]; then
      printf '%s\n' --key "$ASC_API_KEY_P8" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID"
      return 0
    fi
    echo "notarize: partial App Store Connect API config — need ALL of ASC_KEY_ID, ASC_ISSUER_ID and ASC_API_KEY_P8 (or ASC_API_KEY_P8_BASE64)" >&2
    return 2
  fi
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    printf '%s\n' --keychain-profile "$NOTARY_PROFILE"
    # NOTARY_KEYCHAIN (TRA-257): resolve the profile from a specific keychain (the dedicated
    # rhapsody-signing keychain in CI), not notarytool's login-keychain default. Unset -> unchanged.
    if [ -n "${NOTARY_KEYCHAIN:-}" ]; then
      printf '%s\n' --keychain "$NOTARY_KEYCHAIN"
    fi
    return 0
  fi
  return 1
}

# When sourced (`source notarize.sh --lib-only`), stop here: expose the functions without
# requiring a dmg argument or touching xcrun/the network.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

DMGOUT="${1:?usage: notarize.sh <out.dmg>}"

resolve_asc_key
auth_rc=0
auth_out="$(notary_auth_args)" || auth_rc=$?
if [ "$auth_rc" -eq 1 ]; then
  echo "notarize: no notary credentials set (NOTARY_PROFILE or ASC_* API key); skipping notarization + stapling"
  exit 0
elif [ "$auth_rc" -ne 0 ]; then
  exit 1 # notary_auth_args already explained the partial config on stderr
fi

[ -f "$DMGOUT" ] || { echo "notarize: dmg not found: $DMGOUT (run 'make dmg' first)" >&2; exit 1; }

auth_args=()
while IFS= read -r arg; do auth_args+=("$arg"); done <<< "$auth_out"

if [ -n "${ASC_KEY_ID:-}" ]; then
  echo "notarize: submitting $DMGOUT to Apple (notarytool, App Store Connect API key '$ASC_KEY_ID')"
else
  echo "notarize: submitting $DMGOUT to Apple (notarytool, profile '$NOTARY_PROFILE')"
fi
xcrun notarytool submit "$DMGOUT" "${auth_args[@]}" --wait

echo "notarize: stapling ticket to $DMGOUT"
xcrun stapler staple "$DMGOUT"
xcrun stapler validate "$DMGOUT"
echo "notarize: done (notarized + stapled $DMGOUT)"
