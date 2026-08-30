#!/usr/bin/env bash
# Pure-shell test for render-cask.sh (TRA-241, STUDIO-648). No network, no Ruby, no brew — just
# asserts the rendered Homebrew cask text, mirroring the Go reference's render_cask_test.sh (pared
# down to the two channels we ship: stable + rc, no per-feature-branch casks). Run from anywhere:
# `./render_cask_test.sh`. Also driven by desktop/src-tauri/tests/packaging_gate.rs so it runs in the
# `desktop` CI job's `cargo test`.
#
# The two GOLDENs below are the load-bearing assertions: they pin each channel's cask body
# byte-for-byte. The stable golden is a verbatim copy of the pre-STUDIO-648 single-channel output, so
# adding the rc channel cannot perturb the stable cask by so much as a blank line.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RENDER="$SCRIPT_DIR/render-cask.sh"

VERSION="0.2.0"
SHA256="02e4ed1cb9089500830174661feedf01fdb193b8f0097280adb320fc823b77af"
# A second, distinct valid (version, sha256) pair for the substitution + no-leak assertions.
VERSION2="1.2.3"
SHA256_2="1111111111111111111111111111111111111111111111111111111111111111"
# A semver PRERELEASE version — the only shape the rc channel accepts (STUDIO-648).
RC_VERSION="0.3.4-rc.1"

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

assert_eq() {
  # $1 = actual, $2 = expected, $3 = description. Whole-output equality — the byte-for-byte pin.
  if [ "$1" = "$2" ]; then
    pass "$3"
  else
    fail "$3"
    printf '     --- expected ---\n%s\n     --- actual ---\n%s\n' "$2" "$1" >&2
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

# The complete expected cask body for each channel. `#{version}` carries no `$`, so it survives the
# unquoted heredoc verbatim — exactly as the renderer must emit it.
#
# STABLE_GOLDEN is a verbatim copy of the renderer's output BEFORE the rc channel existed. Any edit to
# the shared cask body — including a stray blank line where the optional `conflicts_with` is spliced
# in — fails here, which is what makes "the stable render stays byte-identical" a test rather than a
# claim (STUDIO-648).
STABLE_GOLDEN="$(cat <<EOF
cask "rhapsody" do
  version "${VERSION}"
  sha256 "${SHA256}"

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
)"

# RC_GOLDEN is STABLE_GOLDEN with exactly two differences: the `rhapsody@rc` token and the
# `conflicts_with cask: "rhapsody"` stanza (placed between `auto_updates` and `depends_on`, Homebrew's
# canonical stanza order). Everything else — url path included — is shared, because an rc's dmg is
# just the Rhapsody.dmg asset on its own prerelease tag's Release.
RC_GOLDEN="$(cat <<EOF
cask "rhapsody@rc" do
  version "${RC_VERSION}"
  sha256 "${SHA256}"

  url "https://github.com/makewhatis/rhapsody/releases/download/v#{version}/Rhapsody.dmg"
  name "Rhapsody"
  desc "Supervises the rhapsodyd daemon and shows its dashboard"
  homepage "https://github.com/makewhatis/rhapsody"

  auto_updates true
  conflicts_with cask: "rhapsody"
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
)"

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
#   - no residual "Symphony" branding (this is the Rhapsody cask).
assert_not_contains "$out" 'verified'                                          'no verified: stanza (keeps brew audit clean)'
assert_not_contains "$out" 'interval.team'                                     'no internal dist host'
assert_not_contains "$out" 'Symphony'                                          'no residual Symphony branding'

# CHANNEL ISOLATION, stable side (STUDIO-648): the stable cask gained NOTHING from the rc channel. It
# names no `rhapsody@` token and declares no `conflicts_with`, so `brew upgrade --cask rhapsody` can
# only ever be offered a stable version — the rc channel is invisible to it.
assert_not_contains "$out" 'conflicts_with'                                    'stable: no conflicts_with (the stable cask gains nothing from the rc channel)'
assert_not_contains "$out" 'rhapsody@'                                         'stable: no @rc channel token'

# Structural single-ness: exactly one url and one zap stanza.
assert_count "$out" 'url "https://github.com/makewhatis/rhapsody' 1            'exactly one url stanza'
assert_count "$out" 'zap script:' 1                                           'exactly one zap stanza'

# The byte-for-byte pin: the whole stable body, unchanged from before the rc channel existed.
assert_eq "$out" "$STABLE_GOLDEN"                                              'stable: renders the golden cask body byte-for-byte'

# Passing the channel explicitly must be indistinguishable from defaulting it, so release.yml's
# existing 2-arg `homebrew-bump` call and an explicit `stable` call can never diverge.
assert_eq "$("$RENDER" "$VERSION" "$SHA256" stable)" "$out"                    'stable: an explicit "stable" channel is identical to the default'

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

# --- the rendered @rc cask (STUDIO-648) ---------------------------------------
# The opt-in prerelease channel. Same app, same release-asset url shape, different token — plus the
# conflicts_with that stops brew installing it over a stable install of /Applications/Rhapsody.app.
rc="$("$RENDER" "$RC_VERSION" "$SHA256" rc)"
assert_contains "$rc" 'cask "rhapsody@rc" do'                                  'rc: declares the rhapsody@rc cask'
assert_contains "$rc" "version \"$RC_VERSION\""                                'rc: emits the prerelease version'
assert_contains "$rc" "sha256 \"$SHA256\""                                     'rc: emits the sha256'
assert_contains "$rc" "$URL"                                                   'rc: url points at the github release with #{version} interpolation'
assert_contains "$rc" 'v#{version}/Rhapsody.dmg'                               'rc: keeps #{version} literal in the url'
assert_not_contains "$rc" "v$RC_VERSION/Rhapsody.dmg"                          'rc: never expands #{version} to the concrete version'
assert_contains "$rc" 'app "Rhapsody.app"'                                     'rc: installs the same Rhapsody.app'
assert_contains "$rc" 'trash:  "~/.rhapsody"'                                  'rc: zaps the same ~/.rhapsody runtime home'

# Conflict DIRECTION and arity: the rc cask points at "rhapsody", never at itself, and Homebrew
# permits the stanza only ONCE per cask.
assert_contains "$rc" 'conflicts_with cask: "rhapsody"'                        'rc: conflicts with the stable cask'
assert_not_contains "$rc" 'conflicts_with cask: "rhapsody@rc"'                 'rc: does not conflict with itself'
assert_count "$rc" 'conflicts_with' 1                                          'rc: exactly one conflicts_with stanza'

# The same simplifications the stable cask keeps.
assert_not_contains "$rc" 'verified'                                           'rc: no verified: stanza'
assert_not_contains "$rc" 'interval.team'                                      'rc: no internal dist host'
assert_not_contains "$rc" 'Symphony'                                           'rc: no residual Symphony branding'

# The byte-for-byte pin for the rc body.
assert_eq "$rc" "$RC_GOLDEN"                                                   'rc: renders the golden cask body byte-for-byte'

# The rc cask is the stable cask plus a token change and one stanza — nothing else. Proving it as a
# diff (rather than eyeballing two goldens) is what stops a future edit drifting the channels apart.
rc_from_stable="$(printf '%s\n' "$STABLE_GOLDEN" \
  | sed -e 's/^cask "rhapsody" do$/cask "rhapsody@rc" do/' \
        -e "s/^  version \"$VERSION\"\$/  version \"$RC_VERSION\"/" \
        -e 's|^  depends_on macos: :catalina$|  conflicts_with cask: "rhapsody"\
  depends_on macos: :catalina|')"
assert_eq "$rc" "$rc_from_stable"                                              'rc: differs from stable ONLY by the token, the version and the conflicts_with stanza'

# --- channel validation (STUDIO-648) ------------------------------------------
# The rc channel TRACKS PRERELEASE TAGS: a final version must be refused, so the @rc cask can never
# name a stable release and shadow it.
for finalver in "0.3.4" "1.0.0" "0.2.0+build.7"; do
  if "$RENDER" "$finalver" "$SHA256" rc >/dev/null 2>&1; then
    fail "rc rejects a non-prerelease version ('$finalver')"
  else
    pass "rc rejects a non-prerelease version ('$finalver')"
  fi
done

# …and it accepts the prerelease shapes a tag can realistically take. (A version carrying BOTH a
# prerelease and `+build` metadata is out: the shared version regex has never allowed it, on either
# channel — see render-latest-json.sh, which pins the same shape for the updater manifest.)
for prever in "0.3.4-rc.1" "0.3.4-rc1" "1.0.0-beta.2" "0.3.4-rc.1.2"; do
  if "$RENDER" "$prever" "$SHA256" rc >/dev/null 2>&1; then
    pass "rc accepts a prerelease version ('$prever')"
  else
    fail "rc accepts a prerelease version ('$prever')"
  fi
done

# Unknown channels fail loudly rather than silently rendering a stable cask under the wrong filename.
# `feature`/`dag` are here on purpose: per-feature-branch casks are the Go reference's other half and
# a NAMED follow-up, so they must be rejected until that work lands, not half-supported.
for badchan in "beta" "RC" "rhapsody@rc" "stable " "" "feature" "dag"; do
  if "$RENDER" "$RC_VERSION" "$SHA256" "$badchan" >/dev/null 2>&1; then
    fail "exits non-zero for an unknown channel ('$badchan')"
  else
    pass "exits non-zero for an unknown channel ('$badchan')"
  fi
done

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

if "$RENDER" "$VERSION" "$SHA256" stable extra >/dev/null 2>&1; then
  fail 'exits non-zero with too many arguments (4)'
else
  pass 'exits non-zero with too many arguments (4)'
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
