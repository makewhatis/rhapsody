#!/usr/bin/env bash
# Structural + leak checks for the Claude Code plugin this repo ships (STUDIO-867).
#
# Two things about a plugin are mechanically checkable, and both have failed in the wild:
#
#   1. A `source` path that does not match a real plugin directory produces a marketplace that
#      adds cleanly and installs NOTHING. Reading the JSON does not catch it; resolving the path
#      does.
#   2. The shipped files are public. They were extracted from one operator's machine-local
#      skills, where a private tailnet host, a person's name and a tracker workspace name were
#      all fine. Here they are not.
#
# Prose has no compiler, so — exactly like `harness/prompt/prompt_test.sh` — this rides the
# existing `lint` job rather than adding a branch-protection context that would not be required.
#
# NOTE: the leak scan searches ONLY the shipped directories, never this script, so the patterns
# below can be spelled out plainly without the check passing on its own text.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

MARKETPLACE=".claude-plugin/marketplace.json"
fail() { echo "FAIL: $*" >&2; exit 1; }

[ -f "$MARKETPLACE" ] || fail "$MARKETPLACE is missing"
command -v python3 >/dev/null 2>&1 || fail "python3 is required to parse the plugin manifests"

# --- 1. every `source` resolves, and the two manifests agree on the version -------------------
SOURCES=$(python3 - "$MARKETPLACE" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
plugins = m.get("plugins") or []
if not plugins:
    sys.exit("marketplace lists no plugins")
for p in plugins:
    src = p.get("source")
    if not isinstance(src, str) or not src.startswith("./"):
        sys.exit(f"plugin {p.get('name')!r}: source must be a relative './dir' path, got {src!r}")
    print(f"{p.get('name')}\t{src}\t{p.get('version')}")
PY
)

while IFS=$'\t' read -r name src version; do
    [ -n "$name" ] || continue
    dir="${src#./}"
    [ -d "$dir" ] || fail "plugin '$name' has source '$src' but '$dir/' does not exist"
    manifest="$dir/.claude-plugin/plugin.json"
    [ -f "$manifest" ] || fail "plugin '$name': '$manifest' is missing — the marketplace would install nothing"
    python3 - "$manifest" "$name" "$version" <<'PY'
import json, sys
manifest, entry_name, entry_version = sys.argv[1], sys.argv[2], sys.argv[3]
p = json.load(open(manifest))
if p.get("name") != entry_name:
    sys.exit(f"{manifest}: name {p.get('name')!r} != marketplace entry {entry_name!r}")
if p.get("version") != entry_version:
    sys.exit(f"{manifest}: version {p.get('version')!r} != marketplace entry {entry_version!r}")
PY

    # --- 2. every skill is loadable: a SKILL.md carrying a front-matter description -----------
    [ -d "$dir/skills" ] || fail "plugin '$name': no '$dir/skills/' directory"
    found=0
    for skill in "$dir"/skills/*/; do
        [ -d "$skill" ] || continue
        found=$((found + 1))
        [ -f "$skill/SKILL.md" ] || fail "skill '$skill' has no SKILL.md"
        head -1 "$skill/SKILL.md" | grep -q '^---$' \
            || fail "$skill/SKILL.md does not open with YAML front matter"
        grep -q '^description:' "$skill/SKILL.md" \
            || fail "$skill/SKILL.md has no front-matter 'description:' — it would never trigger"
    done
    [ "$found" -gt 0 ] || fail "plugin '$name' ships no skills"
    echo "ok: plugin '$name' -> $dir ($found skill(s)), manifests agree at v$version"
done <<< "$SOURCES"

# --- 3. nothing machine-local or private leaks into a public, installable artefact ------------
# Each entry is "<what it is>|<extended regex>".
LEAKS=(
    "a private tailnet hostname|[A-Za-z0-9_-]+\.ts\.net"
    "a personal name|David"
    "a tracker workspace name|Studio 49"
)
scan_dirs=(".claude-plugin")
while IFS=$'\t' read -r _ src _; do
    [ -n "$src" ] && scan_dirs+=("${src#./}")
done <<< "$SOURCES"

for entry in "${LEAKS[@]}"; do
    what="${entry%%|*}"
    pattern="${entry#*|}"
    if hits=$(grep -rInE -- "$pattern" "${scan_dirs[@]}" 2>/dev/null); then
        echo "$hits" >&2
        fail "shipped plugin files must not contain $what"
    fi
done
echo "ok: no tailnet hostname, personal name or workspace name in ${scan_dirs[*]}"

# --- 4. the real validator, when it is on PATH ------------------------------------------------
# Authoritative but optional: CI runners do not necessarily have the Claude Code CLI, and this
# check must stay bash-only there. When it IS present it outranks everything above.
if command -v claude >/dev/null 2>&1; then
    claude plugin validate . --strict
    while IFS=$'\t' read -r _ src _; do
        [ -n "$src" ] && claude plugin validate "$src" --strict
    done <<< "$SOURCES"
else
    echo "note: the 'claude' CLI is not on PATH — skipped 'claude plugin validate --strict'"
fi

echo "plugin checks passed"
