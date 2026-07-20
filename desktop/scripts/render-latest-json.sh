#!/usr/bin/env bash
# Renders the Tauri updater manifest (latest.json) for Rhapsody to stdout (TRA-261, P11-U2).
#
# tauri-plugin-updater (wired in TRA-260/U1, configured in desktop/src-tauri/tauri.conf.json) polls
#   https://github.com/makewhatis/rhapsody/releases/latest/download/latest.json
# and, when its `version` is newer than the running app, downloads `platforms.<target>.url` (the
# `Rhapsody.app.tar.gz` updater bundle) and verifies it against `platforms.<target>.signature` (a
# minisign signature) using the pubkey pinned in tauri.conf.json. A bad/absent signature aborts the
# update — nothing unsigned is ever installed.
#
# `darwin-aarch64` is the ONLY shipped target — Rhapsody ships Apple-Silicon-only (see tauri.conf.json).
#
# Emitted by release.yml's `build` job AFTER the .app is signed + notarized + stapled: the signature
# is `cargo tauri signer sign`'s output over the tarball built from THAT notarized bundle, and the url
# points at the tarball uploaded to the same GitHub Release. Like render-cask.sh, this is the single
# source of truth for the manifest body and is pinned byte-for-behavior by render_latest_json_test.sh
# (run in the `desktop` CI job via src-tauri/tests/packaging_gate.rs).
#
# Usage: render-latest-json.sh <version> <pub_date> <signature> <url> [notes]
#   e.g. render-latest-json.sh 0.4.0 2026-07-20T04:52:28Z "dW50cnVzdGVk..." \
#          https://github.com/makewhatis/rhapsody/releases/download/v0.4.0/Rhapsody.app.tar.gz
set -euo pipefail

if [ "$#" -lt 4 ] || [ "$#" -gt 5 ]; then
  echo "usage: $(basename "$0") <version> <pub_date> <signature> <url> [notes]" >&2
  exit 2
fi
version="$1"
pub_date="$2"
signature="$3"
url="$4"
notes="${5:-Rhapsody ${version}.}"

# jq builds the JSON so every value is correctly escaped (notes is free text). jq ships at
# /usr/bin/jq on the macOS runners this pipeline targets.
command -v jq >/dev/null 2>&1 || { echo "error: jq is required (ships at /usr/bin/jq on macOS)" >&2; exit 3; }

# Validate the input shapes so a malformed release input can't emit a manifest the updater would
# choke on (mirrors render-cask.sh's fail-loud-at-the-source posture). Anchored bash `=~` matches the
# WHOLE string (unlike a line-oriented `grep`, which would accept an embedded newline as long as one
# line matched) — every field here is single-line by construction.
#
# Dotted version (optionally with a -prerelease/+build suffix, e.g. 0.4.0 or 1.0.0-rc1), matching
# render-cask.sh so the cask and the updater agree on what a release version looks like.
version_re='^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'
if [[ ! $version =~ $version_re ]]; then
  echo "error: invalid version '${version}' (expected a dotted version like 0.4.0)" >&2
  exit 2
fi
# RFC 3339 UTC instant, e.g. 2026-07-20T04:52:28Z — the `date -u +%Y-%m-%dT%H:%M:%SZ` shape the
# updater parses for pub_date.
pub_date_re='^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$'
if [[ ! $pub_date =~ $pub_date_re ]]; then
  echo "error: invalid pub_date '${pub_date}' (expected RFC 3339 UTC like 2026-07-20T04:52:28Z)" >&2
  exit 2
fi
# A tauri/minisign signature is the single-line base64 blob `cargo tauri signer sign` writes to the
# .sig file — reject anything with whitespace or non-base64 characters.
signature_re='^[A-Za-z0-9+/=]+$'
if [[ ! $signature =~ $signature_re ]]; then
  echo "error: invalid signature (expected the single-line base64 blob from 'cargo tauri signer sign')" >&2
  exit 2
fi
# An https GitHub-Release .tar.gz asset URL (the updater downloads it over TLS; a .dmg/plain URL is a
# misconfiguration).
url_re='^https://[^[:space:]]+\.tar\.gz$'
if [[ ! $url =~ $url_re ]]; then
  echo "error: invalid url '${url}' (expected an https .tar.gz release asset URL)" >&2
  exit 2
fi

# Build the manifest with jq so values are escaped and the key order is stable. Object-literal order
# is preserved by jq, so the output is deterministic (pinned by render_latest_json_test.sh).
jq -n \
  --arg version "$version" \
  --arg notes "$notes" \
  --arg pub_date "$pub_date" \
  --arg signature "$signature" \
  --arg url "$url" \
  '{
    version: $version,
    notes: $notes,
    pub_date: $pub_date,
    platforms: {
      "darwin-aarch64": {
        signature: $signature,
        url: $url
      }
    }
  }'
