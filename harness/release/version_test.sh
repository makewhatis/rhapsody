#!/usr/bin/env bash
# version_test.sh (TRA-239) — proves the Makefile's VERSION default is derived from the most recent
# release-please tag (vX.Y.Z, leading `v` stripped), falls back to "dev" when the tree has no release
# tag yet, and still honors an explicit `VERSION=` override. It drives the REAL repo Makefile's
# `print-version` target inside throwaway git repos, so the assertions exercise the shipped logic
# (the `$(shell git describe ...)` default), not a copy of it.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
makefile="$repo_root/Makefile"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
fail=0

# new_repo <tag-or-empty> -> path to a fresh temp git repo (one commit, optional annotated-less tag).
# Uses mktemp for a unique dir per call: new_repo runs inside a $(...) command-substitution subshell,
# so a mutated counter would not persist across calls and every repo would collide on one path.
new_repo() {
  local tag="$1" dir
  dir="$(mktemp -d "$scratch/r.XXXXXX")"
  git init -q "$dir"
  git -C "$dir" config user.email t@t.test
  git -C "$dir" config user.name test
  git -C "$dir" commit -q --allow-empty -m "chore: seed"
  [ -n "$tag" ] && git -C "$dir" tag "$tag"
  printf '%s\n' "$dir"
}

# version_in <repo-dir> [make-args...] -> the Makefile's resolved VERSION for that repo's git state
version_in() {
  local dir="$1"
  shift
  (cd "$dir" && make -f "$makefile" print-version "$@")
}

# check <description> <expected> <actual>
check() {
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"
  else
    echo "FAIL - $1: expected '$2' got '$3'"
    fail=1
  fi
}

check "no release tag falls back to dev"        "dev"    "$(version_in "$(new_repo '')")"
check "release tag strips the leading v"        "1.2.3"  "$(version_in "$(new_repo v1.2.3)")"
check "multi-digit version is preserved"        "0.10.0" "$(version_in "$(new_repo v0.10.0)")"
check "explicit VERSION override wins over tag" "9.9.9"  "$(version_in "$(new_repo v1.2.3)" VERSION=9.9.9)"

if [ "$fail" -ne 0 ]; then
  echo "version_test: FAILED"
  exit 1
fi
echo "version_test: all passed"
