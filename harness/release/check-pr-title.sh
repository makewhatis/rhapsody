#!/usr/bin/env bash
# check-pr-title.sh (STUDIO-408) — fail a pull request whose TITLE is not a conventional-commit
# subject release-please can parse.
#
# Why the title and not the branch's commits: this repo squash-merges, and GitHub uses the PR title
# (plus " (#N)") as the squash subject. That single subject is the ONLY thing release-please sees on
# main. PR #27 (STUDIO-406) landed as "STUDIO-406: stop a null attachment field making an entire
# project invisible to the poller (#27)" — no conventional type, so release-please logged
# "commit could not be parsed", proposed nothing, and the whole downstream chain (tag -> signed dmg ->
# Homebrew cask bump -> .release-please-manifest.json) never ran. The release workflow stayed GREEN.
# That silence is the bug this guard exists to convert into a red check.
#
# Usage: check-pr-title.sh "<subject>"
#   exit 0 — parses as a conventional subject with a type release-please recognises
#   exit 1 — does not; the explanation on stderr says what that costs
#   exit 2 — usage error (no argument): a mis-wired workflow, NOT a rejected title
#
# Pinned by harness/release/pr_title_test.sh; run by .github/workflows/pr-title.yml.
set -euo pipefail

# The types release-please recognises: its default changelog sections (DEFAULT_HEADINGS in
# googleapis/release-please). feat/fix — and any type with a `!` breaking marker — are the ones that
# move the version; the rest parse cleanly and land in the changelog without bumping, which is why the
# guard accepts all twelve instead of forcing an unrelated PR to mislabel itself as a fix.
#
# Version semantics for this config (release-please-config.json: `simple`, pre-1.0 with
# bump-minor-pre-major + bump-patch-for-minor-pre-major): while we are on 0.x, feat: and fix: both bump
# the PATCH, and a breaking change (`!` / BREAKING CHANGE) bumps the MINOR.
TYPES=(build chore ci deps docs feat fix perf refactor revert style test)

if [ "$#" -lt 1 ]; then
  echo "usage: $(basename "$0") \"<pr-title>\"" >&2
  exit 2
fi
subject="$1"

# <type>[(scope)][!]: <description> — a lowercase type from the list, an optional parenthesised scope,
# an optional breaking-change `!`, then a colon, ONE space, and a non-empty description. The space is
# required by the conventional-commits grammar release-please parses, and the description is what
# swallows the " (#N)" GitHub appends when it writes the squash subject.
types_alt="$(IFS='|'; printf '%s' "${TYPES[*]}")"
if [[ "$subject" =~ ^($types_alt)(\([^()]+\))?!?:\ .+$ ]]; then
  exit 0
fi

# --- rejected: explain the CONSEQUENCE, not just the grammar ---------------------------------------
cat >&2 <<EOF
This pull request's title is not a conventional-commit subject:

    ${subject}

GitHub uses the PR title as the squash subject, and that subject is the only thing release-please
reads. It cannot parse this one, so merging it would mean:

  * no release PR and no version bump — release-please logs "commit could not be parsed" and stops,
  * no git tag and no signed/notarized dmg — the build job is gated on a release being created,
  * no Homebrew cask bump — \`brew upgrade rhapsody\` keeps serving the previous version,
  * no .release-please-manifest.json update — the repo's recorded version drifts from the tag.

None of that reports an error. The release workflow goes green and silently ships nothing, which is
exactly how 0.3.3 was lost (STUDIO-406 / #27 -> STUDIO-408).

Retitle the PR as:

    <type>[(scope)][!]: <description>

  types:    ${TYPES[*]}
  bumps:    fix:/feat: bump the patch while we are pre-1.0; a \`!\` breaking marker bumps the minor;
            every other type parses and lands in the changelog without releasing.
  examples: fix: null attachment fields no longer make a project invisible to the poller
            feat(config): add a capabilities field mirroring labels
            ci!: drop the ubuntu runner fallback

Keep the ticket id in the description (e.g. "... to the poller (STUDIO-408)") rather than in front of
the type — a leading ticket id is precisely what release-please cannot parse.
EOF

if [ -n "${GITHUB_ACTIONS:-}" ]; then
  # Single-line annotation so the failure is readable on the PR's Checks tab without opening the log.
  echo "::error title=PR title is not a conventional-commit subject::\"${subject}\" cannot be parsed by release-please. Merging it would silently skip the release, the signed dmg, the Homebrew cask bump and the manifest update — with a green workflow. Retitle as <type>[(scope)][!]: <description> (${TYPES[*]}). See the step log for detail."
fi

exit 1
