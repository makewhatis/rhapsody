#!/usr/bin/env bash
# check-plugin_test.sh (STUDIO-867) — pins the leak scan in check-plugin.sh by MAKING IT FIRE.
#
# The first version of that scan was case-sensitive and required the workspace name to carry its
# space, so it reported clean on the two forms the strings actually travel in: the lower-case
# `david` of the front-matter key this plugin was extracted from, and the `linear.app/studio49/`
# slug carried by every pasted ticket link. A leak scan that misses the leak it was written for is
# worse than no scan, because it converts "nobody checked" into "we checked and it is clean" — so
# each pattern is proved here by reintroducing the leak into a throwaway copy of the shipped tree
# and asserting the scan reds on it. A pattern nobody has watched fire is not a guard.
#
# The green cases matter as much as the red ones: a pattern broad enough to red on an ordinary
# ticket id gets weakened by the first person it inconveniences, which is the same failure by a
# slower route.
#
# NOTE: like check-plugin.sh, this file is never itself scanned — the scan reads `.claude-plugin`
# and the marketplace's plugin source directories, never `.github/scripts` — so the leak forms
# below are spelled out plainly. They must be: a pin that cannot spell what it forbids cannot
# prove that it is forbidden.
#
# No dependencies beyond bash and python3. Run from anywhere: `.github/scripts/check-plugin_test.sh`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
check_rel=".github/scripts/check-plugin.sh"
fail=0
out=""
status=0

# A copy of the shipped tree, so a leak can be reintroduced without ever dirtying the real one.
# check-plugin.sh resolves the repo root from its own path, so the copy is what it checks.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

sources="$(python3 - "$root/.claude-plugin/marketplace.json" <<'PY'
import json, sys
for p in json.load(open(sys.argv[1])).get("plugins") or []:
    src = p.get("source") or ""
    if src.startswith("./"):
        print(src[2:])
PY
)"
[ -n "$sources" ] || { echo "FAIL - the marketplace lists no plugin sources to copy"; exit 1; }

mkdir -p "$work/.github/scripts"
cp "$root/$check_rel" "$work/$check_rel"
cp -R "$root/.claude-plugin" "$work/.claude-plugin"
while read -r src; do
    [ -n "$src" ] || continue
    mkdir -p "$work/$(dirname "$src")"
    cp -R "$root/$src" "$work/$src"
done <<< "$sources"

# run_check — runs the copied check and sets `out` (combined output) + `status`. Deliberately not a
# command substitution at the call site: a subshell's assignments would not survive it.
run_check() {
    set +e
    out="$("$work/$check_rel" 2>&1)"
    status=$?
    set -e
}

# The shipped tree must pass BEFORE anything is injected — a red baseline proves nothing about the
# cases below, since they would all "fail" for the wrong reason.
run_check
if [ "$status" -ne 0 ]; then
    echo "FAIL - baseline: the unmodified shipped tree must pass, got exit $status:"
    echo "$out"
    exit 1
fi
echo "ok   - baseline: the shipped tree passes the check"

# The file the leaks get injected into: the first shipped SKILL.md. Appending to its body leaves the
# front matter intact, so only the leak scan can be what reds.
target_rel=""
while read -r src; do
    [ -n "$src" ] || continue
    for skill in "$work/$src"/skills/*/; do
        [ -f "$skill/SKILL.md" ] || continue
        target_rel="$src/skills/$(basename "${skill%/}")/SKILL.md"
        break 2
    done
done <<< "$sources"
[ -n "$target_rel" ] || { echo "FAIL - no shipped SKILL.md to inject a leak into"; exit 1; }

# inject <text> — appends to the copied skill file, runs the check, then restores the copy.
inject() {
    printf '\n%s\n' "$1" >> "$work/$target_rel"
    run_check
    cp "$root/$target_rel" "$work/$target_rel"
}

# reds <what> <leak> — <leak> in a shipped file must fail the check, and as <what>, not incidentally.
reds() {
    inject "$2"
    if [ "$status" -eq 0 ]; then
        echo "FAIL - reds on '$2': the scan passed, so the leak would ship"
        fail=1
    elif ! grep -qF -- "must not contain $1" <<<"$out"; then
        echo "FAIL - reds on '$2': failed, but not as $1: $out"
        fail=1
    else
        echo "ok   - reds on '$2' ($1)"
    fi
}

# green <note> <text> — <text> must NOT trip the scan; it is ordinary content, not a leak.
green() {
    inject "$2"
    if [ "$status" -ne 0 ]; then
        echo "FAIL - green on '$2' ($1): the scan red on content that is not a leak: $out"
        fail=1
    else
        echo "ok   - green on '$2' ($1)"
    fi
}

# --- the personal name, in the forms it travels in ---------------------------------------------
# Lower case first: `metadata.author: david` is the exact front-matter key this plugin's skills were
# extracted with, and the exact string the case-sensitive scan let through.
reds "a personal name" "metadata.author: david"
reds "a personal name" "Ask David before changing the roster."
reds "a personal name" "OWNER: DAVID"

# --- the tracker workspace name ------------------------------------------------------------------
# The URL form is how a workspace name really reaches a file: it rides in on every ticket link.
reds "a tracker workspace name" "https://linear.app/studio49/issue/STUDIO-867/ship-the-skills"
reds "a tracker workspace name" "the Studio 49 workspace"
reds "a tracker workspace name" "the studio 49 workspace"
reds "a tracker workspace name" "STUDIO49"

# --- the private tailnet hostname -----------------------------------------------------------------
reds "a private tailnet hostname" "http://some-host.tail1234.ts.net:8080/api/v1/state"
reds "a private tailnet hostname" "HTTPS://SOME-HOST.TS.NET"

# --- content that must stay green ------------------------------------------------------------------
# A bare ticket id is not a workspace name. If the slug pattern were widened to accept any separator
# it would red on STUDIO-490..499, and the guard would be loosened by whoever hit that first.
green "a bare ticket id" "Fixes STUDIO-491, and see STUDIO-867."
green "an ordinary sentence" "Teams route work by label; the room is read back into future prompts."

# --- both shipped Teams skills must document the selection lifecycles (STUDIO-993) ----------------
# Stripping one marker from ONE skill copy must red the check: this is the "partial docs copy" defect
# the ticket calls a release defect. The copy is restored before the next case runs.
lifecycle_rel="plugin/skills/rhapsody-team-setup/SKILL.md"
if [ ! -f "$work/$lifecycle_rel" ]; then
    echo "FAIL - expected $lifecycle_rel in the copied tree"
    fail=1
else
    set +e
    python3 - "$work/$lifecycle_rel" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read().replace("hot-reload", "HOT-RELOAD")
open(p, "w").write(s)
PY
    run_check
    cp "$root/$lifecycle_rel" "$work/$lifecycle_rel"
    set -e
    if [ "$status" -eq 0 ] || ! grep -qF -- "boot/hot-reload/dispatch selection lifecycles" <<<"$out"; then
        echo "FAIL - stripping a lifecycle marker from one skill must red the check: exit $status, $out"
        fail=1
    else
        echo "ok   - reds when one shipped skill drops a lifecycle marker"
    fi
fi

# --- 2c. a plugin change without a version bump must red (STUDIO-993) ------------------------------
# "Omit the version bump" is a named mutation, and a MISSING bump is only checkable against a base
# ref — so build a scratch git repo whose base commit carries the plugin at an OLD version, change a
# shipped plugin file without bumping, and require the check to red. Bumping afterwards must green
# it, proving the red is the version check and not some incidental failure of the scratch tree.
# (The scratch repo is needed because check-plugin.sh resolves its root from its own path and the
# copied `$work` tree above is deliberately NOT a git repo.)
bump_repo="$work/bump-repo"
mkdir -p "$bump_repo/.github/scripts"
cp -R "$root/.claude-plugin" "$bump_repo/.claude-plugin"
cp -R "$root/plugin" "$bump_repo/plugin"
cp "$root/$check_rel" "$bump_repo/$check_rel"

# set_version <repo> <version> — rewrites the version in all three shipped fields (marketplace top
# level + its rhapsody entry + plugin/.claude-plugin/plugin.json), the same trio check-plugin.sh
# already requires to agree.
set_version() {
    python3 - "$1" "$2" <<'PY'
import json, pathlib, sys
root, version = pathlib.Path(sys.argv[1]), sys.argv[2]
mp = root / ".claude-plugin/marketplace.json"
m = json.loads(mp.read_text())
m["version"] = version
for p in m.get("plugins") or []:
    if p.get("source") == "./plugin":
        p["version"] = version
mp.write_text(json.dumps(m, indent=2) + "\n")
pj = root / "plugin/.claude-plugin/plugin.json"
p = json.loads(pj.read_text())
p["version"] = version
pj.write_text(json.dumps(p, indent=2) + "\n")
PY
}

run_bump_check() {
    set +e
    bump_out="$(cd "$bump_repo" && RHAPSODY_PLUGIN_BASE_REF="$base_sha" "$bump_repo/$check_rel" 2>&1)"
    bump_status=$?
    set -e
}

set_version "$bump_repo" "0.0.1"
git -C "$bump_repo" init -q
git -C "$bump_repo" config user.email test@example.com
git -C "$bump_repo" config user.name test
git -C "$bump_repo" add -A
git -C "$bump_repo" commit -qm base
base_sha="$(git -C "$bump_repo" rev-parse HEAD)"

# A real plugin edit — a whole comment line appended to a SKILL.md body — with the version left at
# 0.0.1. This is exactly the "omit version bump" mutation.
printf '\n<!-- docs drift -->\n' >> "$bump_repo/plugin/skills/rhapsody-teams/SKILL.md"
git -C "$bump_repo" add -A
git -C "$bump_repo" commit -qm drift
run_bump_check
if [ "$bump_status" -eq 0 ] || ! grep -qF -- "the plugin version did not" <<<"$bump_out"; then
    echo "FAIL - a plugin change without a version bump must red the check: exit $bump_status, $bump_out"
    fail=1
else
    echo "ok   - reds when a shipped plugin file changes without a version bump"
fi

set_version "$bump_repo" "0.0.2"
git -C "$bump_repo" add -A
git -C "$bump_repo" commit -qm bump
run_bump_check
if [ "$bump_status" -ne 0 ]; then
    echo "FAIL - bumping the plugin version must green the version-bump check: exit $bump_status, $bump_out"
    fail=1
else
    echo "ok   - greens once the plugin version is bumped"
fi

# --- `.claude-plugin/` is scanned too, not just the plugin source dirs ------------------------------
# The marketplace manifest is shipped and installable; a leak in its description is as public as one
# in a skill. Injected through the JSON so the manifest stays parseable and only the scan can red.
python3 - "$work/.claude-plugin/marketplace.json" <<'PY'
import json, sys
path = sys.argv[1]
m = json.load(open(path))
m["description"] = (m.get("description") or "") + " maintained by david"
json.dump(m, open(path, "w"), indent=2)
PY
run_check
cp "$root/.claude-plugin/marketplace.json" "$work/.claude-plugin/marketplace.json"
if [ "$status" -eq 0 ] || ! grep -qF -- "must not contain a personal name" <<<"$out"; then
    echo "FAIL - a leak in .claude-plugin/marketplace.json must red the scan: exit $status, $out"
    fail=1
else
    echo "ok   - reds on a leak in .claude-plugin/marketplace.json"
fi

if [ "$fail" -ne 0 ]; then
    echo "check-plugin_test.sh FAILED"
    exit 1
fi
echo "check-plugin_test.sh passed"
