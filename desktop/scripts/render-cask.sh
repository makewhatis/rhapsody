#!/usr/bin/env bash
# Renders a Homebrew cask for Rhapsody to stdout (TRA-241, STUDIO-648).
#
# A drastically simplified descendant of the Go reference's desktop/scripts/render-cask.sh, served from
# GitHub Releases instead of an internal `dist.*.interval.team` host. Two channels:
#
#   stable (default)  cask token `rhapsody`      — tracks real releases (release-please tags)
#   rc                cask token `rhapsody@rc`   — tracks PRERELEASE tags (STUDIO-648)
#
# The Go reference's third shape, per-feature `symphony@<branch>` dogfood casks, is still deliberately
# NOT ported: it needs per-branch signed builds and a retention story, and is a named follow-up.
#
# Both channels install the same /Applications/Rhapsody.app, so at most one may be installed at a time.
# The rc cask therefore declares `conflicts_with cask: "rhapsody"`. The stable cask deliberately gains
# NOTHING from this — its body stays byte-identical to what shipped before the rc channel existed
# (pinned by render_cask_test.sh), so `brew upgrade --cask rhapsody` can never be handed an rc. That
# makes the conflict declaration one-way: brew blocks installing @rc over stable, and an operator who
# wants the reverse swap uninstalls @rc first.
#
# Neither cask carries a `verified:` stanza (Homebrew rejects `verified:` as unnecessary when the url
# and homepage share the github.com domain — Cask Cookbook "When url and homepage domains differ, add
# verified"), and both point at the SAME versioned release-asset path: an rc's dmg is just the
# `Rhapsody.dmg` asset on its own prerelease tag's Release (v0.3.4-rc.1/Rhapsody.dmg).
#
# Used to author the committed casks (Casks/rhapsody.rb, Casks/rhapsody@rc.rb in makewhatis/homebrew-tap)
# AND by release.yml's auto-bump jobs, which re-render them with each release's version + sha256.
#
# Usage: render-cask.sh <version> <sha256> [channel]
#   e.g. render-cask.sh 0.2.0 02e4ed1cb9089500830174661feedf01fdb193b8f0097280adb320fc823b77af
#        render-cask.sh 0.3.4-rc.1 02e4ed…b77af rc
#
# `#{version}` in the url is Homebrew's OWN interpolation, evaluated by brew at install time — it is
# intentionally emitted literally here, NOT expanded by this script (so the url tracks `version`).
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: $(basename "$0") <version> <sha256> [channel]" >&2
  exit 2
fi
version="$1"
sha256="$2"
# `${3-stable}` (not `${3:-stable}`) defaults only when the argument is UNSET: an explicitly passed
# EMPTY channel is an error, not a silent fallback to stable. That matters because the caller is a CI
# job interpolating a variable — an empty `$channel` must not quietly render the stable cask body into
# Casks/rhapsody@rc.rb.
channel="${3-stable}"

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

# Resolve the channel to its cask token and conflict stanza. `conflicts` is emitted verbatim when
# non-empty and skipped entirely when empty, which is what keeps the stable body byte-identical.
# Only the two channels above are accepted — a typo'd or not-yet-supported channel (`beta`, a feature
# branch name) must fail loudly rather than silently render a stable cask under the wrong filename.
case "$channel" in
  stable)
    token="rhapsody"
    conflicts=""
    ;;
  rc)
    token="rhapsody@rc"
    conflicts='  conflicts_with cask: "rhapsody"'
    # The rc channel exists to track PRERELEASE tags, so its version must carry a semver prerelease
    # part (0.3.4-rc.1, not 0.3.4). This is the renderer half of the channel-isolation invariant: the
    # @rc cask can never name a final version, so it can never shadow a stable install's version, and
    # an rc install's exit ramp stays the in-app updater (tauri semver: 0.3.4-rc.1 < 0.3.4).
    # The base regex above has already pinned the whole shape, so this only has to assert that the
    # suffix is a `-prerelease` and not `+build` metadata (0.2.0+build.7 is a final release).
    if ! printf '%s' "$version" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+-'; then
      echo "error: invalid rc version '${version}' (the rc channel tracks prerelease tags — expected a semver prerelease like 0.3.4-rc.1)" >&2
      exit 2
    fi
    ;;
  *)
    echo "error: invalid channel '${channel}' (expected 'stable' or 'rc'; per-feature-branch channels are a deliberate follow-up, not supported here)" >&2
    exit 2
    ;;
esac

# The heredocs are unquoted so ${token}/${version}/${sha256} expand; `#{version}` has no `$` so it
# passes through verbatim for brew to interpolate. This output is byte-for-byte `brew style`-clean
# (guarded by render_cask_test.sh) — keep any edit in lockstep with the committed cask so a re-render
# is a no-op. The body is split at a line boundary around the optional `conflicts_with` so that, on
# the stable channel, NOTHING is inserted (no stray blank line) and the rendered text is identical to
# the pre-STUDIO-648 single-channel output. `conflicts_with` sits between `auto_updates` and
# `depends_on` because that is Homebrew's canonical stanza order (rubocop-cask Cask/StanzaOrder).
cat <<EOF
cask "${token}" do
  version "${version}"
  sha256 "${sha256}"

  url "https://github.com/makewhatis/rhapsody/releases/download/v#{version}/Rhapsody.dmg"
  name "Rhapsody"
  desc "Supervises the rhapsodyd daemon and shows its dashboard"
  homepage "https://github.com/makewhatis/rhapsody"

  auto_updates true
EOF

if [ -n "$conflicts" ]; then
  printf '%s\n' "$conflicts"
fi

# `depends_on macos:` tracks HOMEBREW's oldest expressible version, not the app's own floor — but do
# NOT "fix" the apparent mismatch with tauri.conf.json's `minimumSystemVersion` (10.15): the two
# floors already agree in practice. Rhapsody ships Apple-Silicon-only (see desktop/README.md), and no
# Apple Silicon Mac ever ran 10.15 — Big Sur 11.0 was the first OS on that hardware — so the shipped
# binary's real floor is 11 too, exactly what this line declares. That 10.15 is an inert scaffold
# value from TRA-231 which has never been revisited; raising it to "11.0" would settle the confusion
# permanently, but that changes the shipped bundle, so it was considered and declined here (STUDIO-781
# scoped it out) rather than overlooked.
#
# :big_sur is also the durable answer to the question a reader actually arrives with — "are we cutting
# off older users?" — because Homebrew does not RUN below macOS 11 at all: brew.sh sets
# HOMEBREW_MACOS_OLDEST_ALLOWED="11", and under it `brew` refuses outright with "too old to run
# Homebrew!". So this excludes nobody who could have installed via brew in the first place. It could
# not say 10.15 anyway: MacOSRequirement::DISABLED_MACOS_VERSIONS disables :catalina and everything
# older, leaving :big_sur the oldest symbol that still parses (STUDIO-777, where :catalina broke
# `brew` here).
#
# `Cask::Audit#audit_min_os` does read the app bundle's real LSMinimumSystemVersion and complain when
# the cask disagrees, but it returns early while that floor is <= HOMEBREW_MACOS_OLDEST_ALLOWED — and
# both it and `cask_bundle_min_os` open with `return unless online?`, so a plain `brew audit` never
# reaches the check at all. Revisit when the plist string is finally raised above 11 (the audit then
# starts caring, and compares exact SYMBOLS, so a floor of "12.3" would need :monterey here, not a
# version string), or when Homebrew disables :big_sur the way it disabled :catalina — its own RELEASES
# table already schedules that for September 2027 or later.
cat <<EOF
  depends_on macos: :big_sur

  app "Rhapsody.app"

  zap script: {
        executable:   "/usr/bin/security",
        args:         ["delete-generic-password", "-s", "is.makewhat.rhapsody"],
        must_succeed: false,
      },
      trash:  "~/.rhapsody"
end
EOF
