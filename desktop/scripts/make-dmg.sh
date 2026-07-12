#!/usr/bin/env bash
# Package a built Rhapsody.app into a drag-to-Applications .dmg installer.
# Parity port of $REF/desktop/scripts/make-dmg.sh (Symphony.app -> Rhapsody.app).
#
# Prefers create-dmg (`brew install create-dmg`) for a polished installer window; falls back to
# hdiutil so the target works on a no-dependency baseline (e.g. a fresh machine or headless build).
# Invoked by the Makefile's `make dmg` / `make _dmg` targets.
#
# Usage: make-dmg.sh <App.app> <out.dmg> [volume-name]
set -euo pipefail

APP="${1:?usage: make-dmg.sh <App.app> <out.dmg> [volume-name]}"
DMGOUT="${2:?usage: make-dmg.sh <App.app> <out.dmg> [volume-name]}"
VOLNAME="${3:-Rhapsody}"

if [ ! -d "$APP" ]; then
  echo "make-dmg: app bundle not found: $APP (run 'make app' first)" >&2
  exit 1
fi

# The output dir may not exist yet (Tauri's bundle output is a distinct tree from desktop/build/bin).
mkdir -p "$(dirname "$DMGOUT")"

# Start from a clean output so a stale .dmg can't masquerade as a fresh build.
rm -f "$DMGOUT"

app_name="$(basename "$APP")"

if command -v create-dmg >/dev/null 2>&1; then
  echo "make-dmg: packaging with create-dmg -> $DMGOUT"
  # create-dmg drives Finder via AppleScript and occasionally fails on the first run (a
  # Finder/AppleScript race, or a leftover attached device); retry once before giving up.
  attempt() {
    create-dmg \
      --volname "$VOLNAME" \
      --window-size 540 380 \
      --icon "$app_name" 150 190 \
      --app-drop-link 390 190 \
      --hide-extension "$app_name" \
      "$DMGOUT" "$APP"
  }
  if ! attempt; then
    echo "make-dmg: create-dmg failed once; retrying (known Finder/AppleScript flake)..." >&2
    rm -f "$DMGOUT"
    attempt
  fi
else
  echo "make-dmg: create-dmg not found; using hdiutil fallback -> $DMGOUT"
  # Stage the app plus an /Applications symlink so the mounted volume offers drag-to-install,
  # then compress it into a read-only UDZO image.
  dmgroot="$(dirname "$DMGOUT")/dmgroot"
  rm -rf "$dmgroot"
  mkdir -p "$dmgroot"
  cp -R "$APP" "$dmgroot/"
  ln -s /Applications "$dmgroot/Applications"
  hdiutil create -volname "$VOLNAME" -srcfolder "$dmgroot" -ov -format UDZO "$DMGOUT"
  rm -rf "$dmgroot"
fi

echo "make-dmg: built $DMGOUT"
