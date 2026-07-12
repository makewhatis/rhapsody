#!/usr/bin/env bash
# Gated Developer ID code-signing for Rhapsody.app and its embedded rhapsodyd sidecar.
# Parity port of $REF/desktop/scripts/sign.sh (symphonyd -> rhapsodyd, Symphony.app -> Rhapsody.app).
#
# No-op (exit 0) unless APPLE_SIGNING_IDENTITY is set, so an autonomous/unsigned build stays green.
# When set, signs under the hardened runtime with a secure timestamp, INSIDE-OUT: the nested
# sidecar first, then the app bundle. Order matters — re-signing the outer bundle reseals it,
# recording the freshly-signed sidecar; signing the app first would invalidate when the sidecar is
# signed afterwards. Entitlements are applied to the app (the process that supervises subprocesses).
# See SIGNING.md.
#
# Usage: sign.sh <App.app> <entitlements.plist>
# Env:   APPLE_SIGNING_IDENTITY  e.g. "Developer ID Application: Your Name (TEAMID)"
set -euo pipefail

APP="${1:?usage: sign.sh <App.app> <entitlements.plist>}"
ENTITLEMENTS="${2:?usage: sign.sh <App.app> <entitlements.plist>}"

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "sign: APPLE_SIGNING_IDENTITY not set; skipping code signing (unsigned build)"
  exit 0
fi

[ -d "$APP" ]          || { echo "sign: app bundle not found: $APP (run 'make app' first)" >&2; exit 1; }
[ -f "$ENTITLEMENTS" ] || { echo "sign: entitlements not found: $ENTITLEMENTS" >&2; exit 1; }

sidecar="$APP/Contents/Resources/rhapsodyd"
[ -f "$sidecar" ] || { echo "sign: embedded sidecar not found: $sidecar (run 'make app' first)" >&2; exit 1; }

echo "sign: signing sidecar $sidecar"
codesign --force --options runtime --timestamp \
  --sign "$APPLE_SIGNING_IDENTITY" "$sidecar"

echo "sign: signing app bundle $APP (hardened runtime + entitlements)"
codesign --force --options runtime --timestamp \
  --entitlements "$ENTITLEMENTS" \
  --sign "$APPLE_SIGNING_IDENTITY" "$APP"

echo "sign: verifying signature (recursively, incl. the nested sidecar)"
codesign --verify --deep --strict --verbose=2 "$APP"
echo "sign: done (signed $APP and its rhapsodyd sidecar)"
