#!/usr/bin/env bash
# Renders the Homebrew cask for Rhapsody to stdout (TRA-241).
#
# A drastically simplified descendant of the Go reference's desktop/scripts/render-cask.sh: ONE stable
# channel served from GitHub Releases. The Go reference rendered multi-channel @rc/@feature casks from
# an internal `dist.*.interval.team` host with `conflicts_with` bookkeeping — we deliberately want NONE
# of that. There is a single `rhapsody` cask whose dmg is a github.com release asset, so it also carries
# no `verified:` stanza (Homebrew rejects `verified:` as unnecessary when the url and homepage share the
# github.com domain — Cask Cookbook "When url and homepage domains differ, add verified").
#
# Used both to author the committed cask (Casks/rhapsody.rb in makewhatis/homebrew-tap) AND by
# release.yml's release-time auto-bump job, which re-renders it with each release's version + sha256.
#
# Usage: render-cask.sh <version> <sha256>
#   e.g. render-cask.sh 0.2.0 02e4ed1cb9089500830174661feedf01fdb193b8f0097280adb320fc823b77af
#
# `#{version}` in the url is Homebrew's OWN interpolation, evaluated by brew at install time — it is
# intentionally emitted literally here, NOT expanded by this script (so the url tracks `version`).
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $(basename "$0") <version> <sha256>" >&2
  exit 2
fi
version="$1"
sha256="$2"

# Validate the shapes so a malformed release tag or checksum can't render a broken cask: a dotted
# version (optionally with a `-prerelease`/`+build` suffix, e.g. 0.2.0 or 1.0.0-rc1) and a 64-hex-char
# lowercase sha256 (`shasum -a 256` output). brew style/audit would reject a bad cask downstream, but
# failing loud here keeps the auto-bump job's error at the source.
if ! printf '%s' "$version" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
  echo "error: invalid version '${version}' (expected a dotted version like 0.2.0)" >&2
  exit 2
fi
if ! printf '%s' "$sha256" | grep -qE '^[0-9a-f]{64}$'; then
  echo "error: invalid sha256 '${sha256}' (expected 64 lowercase hex chars)" >&2
  exit 2
fi

# The heredoc is unquoted so ${version}/${sha256} expand; `#{version}` has no `$` so it passes through
# verbatim for brew to interpolate. This output is byte-for-byte `brew style`-clean (guarded by
# render_cask_test.sh) — keep any edit in lockstep with the committed cask so a re-render is a no-op.
cat <<EOF
cask "rhapsody" do
  version "${version}"
  sha256 "${sha256}"

  url "https://github.com/makewhatis/rhapsody/releases/download/v#{version}/Rhapsody.dmg"
  name "Rhapsody"
  desc "Supervises the rhapsodyd daemon and shows its dashboard"
  homepage "https://github.com/makewhatis/rhapsody"

  auto_updates true
  depends_on macos: :catalina

  app "Rhapsody.app"

  zap script: {
        executable:   "/usr/bin/security",
        args:         ["delete-generic-password", "-s", "is.makewhat.rhapsody"],
        must_succeed: false,
      },
      trash:  "~/.rhapsody"
end
EOF
