#!/usr/bin/env bash
# Package a built Rhapsody.app into a drag-to-Applications .dmg installer.
# Parity port of $REF/desktop/scripts/make-dmg.sh (Symphony.app -> Rhapsody.app).
#
# Prefers create-dmg (`brew install create-dmg`) for a polished installer window, but ALWAYS falls
# back to a robust image+ditto path — because on macOS 15+ (Sequoia/Tahoe) `hdiutil create
# -srcfolder <app>` fails with "hdiutil: create failed - Operation not permitted" (preceded by
# "could not access /Volumes/<vol>/<app>"). Root cause: a code-signed app acquires the kernel-only
# `com.apple.provenance` xattr the first time it is validated/run, and `hdiutil -srcfolder` (which
# create-dmg also uses internally) cannot replicate that protected xattr onto its synthesized volume.
# CI never hits this because it never launches the app; a developer who ran the app locally does.
#
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

# sign_dmg: Developer-ID-sign the finished dmg (TRA-258). Without this the dmg is "not signed at
# all" to Gatekeeper even though the app inside is signed — `spctl -a -t open` then reports no usable
# signature. Gated on APPLE_SIGNING_IDENTITY exactly like sign.sh, so the unsigned default path is a
# no-op. An intentional divergence from the Go reference, which signs only the app (see SIGNING.md).
# A dmg is a disk image, not executable code, so no `--options runtime` (that is for the app/sidecar).
sign_dmg() {
  if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
    return 0
  fi
  echo "make-dmg: signing $DMGOUT (Developer ID '$APPLE_SIGNING_IDENTITY')"
  codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$DMGOUT"
  codesign -dv "$DMGOUT"
}

# create-dmg stages a volume at /Volumes/$VOLNAME; a leftover attachment from an interrupted run — or
# the app opened from a distributed .dmg — makes it fail. Detach any stale volume there first.
detach_stale_volume() {
  local mp="/Volumes/$VOLNAME"
  [ -d "$mp" ] || return 0
  echo "make-dmg: a volume is already mounted at $mp; detaching it before packaging" >&2
  hdiutil detach -force "$mp" >/dev/null 2>&1 \
    || diskutil unmount force "$mp" >/dev/null 2>&1 \
    || echo "make-dmg: WARNING could not detach $mp — eject it manually if packaging fails" >&2
}

# Robust packaging that works regardless of the com.apple.provenance xattr: create an EMPTY read/write
# image, mount it at a PRIVATE mountpoint (not /Volumes/$VOLNAME, which collides with an open
# installer), copy the app in with `ditto` (faithful to the code signature — unlike `hdiutil
# -srcfolder`, and unlike `cp` which can drop the signature seal), add the drag-to-install alias, then
# compress to UDZO.
package_ditto() {
  echo "make-dmg: packaging via image+ditto -> $DMGOUT" >&2
  local size mnt rw
  size=$(( $(du -sm "$APP" | cut -f1) + 60 ))   # app size in MiB + headroom for the image
  mnt="$(mktemp -d)"
  rw="$(dirname "$DMGOUT")/.${VOLNAME}.rw.dmg"
  rm -f "$rw"
  # Always release the mount + scratch image, even if a step below fails under `set -e`.
  trap 'hdiutil detach -force "$mnt" >/dev/null 2>&1 || true; rm -f "$rw"; rmdir "$mnt" >/dev/null 2>&1 || true' RETURN
  hdiutil create -size "${size}m" -fs HFS+ -volname "$VOLNAME" -type UDIF -ov "$rw" >/dev/null
  hdiutil attach "$rw" -nobrowse -owners off -mountpoint "$mnt" >/dev/null
  ditto "$APP" "$mnt/$app_name"
  ln -s /Applications "$mnt/Applications"
  hdiutil detach -force "$mnt" >/dev/null
  hdiutil convert "$rw" -format UDZO -o "$DMGOUT" >/dev/null
}

if command -v create-dmg >/dev/null 2>&1; then
  echo "make-dmg: trying create-dmg -> $DMGOUT"
  detach_stale_volume
  # create-dmg drives Finder via AppleScript for the polished window; it can flake on the first run
  # and, on macOS 15+ signed+run apps, fails outright (the -srcfolder/provenance issue above). Either
  # way, fall through to the always-works ditto path rather than aborting the build.
  if create-dmg \
       --volname "$VOLNAME" \
       --window-size 540 380 \
       --icon "$app_name" 150 190 \
       --app-drop-link 390 190 \
       --hide-extension "$app_name" \
       "$DMGOUT" "$APP" 2>&1; then
    echo "make-dmg: built $DMGOUT (create-dmg)"
    sign_dmg
    exit 0
  fi
  echo "make-dmg: create-dmg did not succeed on this host; using the robust image+ditto fallback" >&2
  rm -f "$DMGOUT"
  detach_stale_volume
fi

package_ditto
sign_dmg
echo "make-dmg: built $DMGOUT"
