#!/usr/bin/env bash
# Gated notarization + stapling of a Developer-ID target: the Rhapsody.app bundle OR the Rhapsody.dmg.
# Parity port of $REF/desktop/scripts/notarize.sh (Symphony.dmg -> Rhapsody.dmg), EXTENDED (TRA-258)
# to also notarize + staple the .app itself. This is an intentional divergence from the Go reference,
# which staples only the dmg: stapling the .app makes a copied-to-/Applications app validate OFFLINE
# (the reference relies on an online Gatekeeper check at first launch). See SIGNING.md.
#
# No-op (exit 0) unless notary credentials are configured, so an autonomous/unsigned build stays
# green. Gated INDEPENDENTLY of signing: if APPLE_SIGNING_IDENTITY is set but no notary credentials
# are, the build still produces a signed-but-unnotarized target. When configured, submits the
# (already signed) target to Apple, waits for the result, then staples the ticket so it validates
# offline. The target must already be Developer-ID signed (run sign.sh first) or Apple rejects the
# submission.
#
# Two target kinds (notarize_target_kind picks the branch by extension):
#   .app  — notarytool cannot submit a directory, so `ditto -c -k --keepParent` zips the bundle,
#           the zip is submitted, then the ticket is stapled to the ORIGINAL .app (not the zip).
#   .dmg / .pkg — submitted + stapled directly (the unchanged, reference behavior).
#
# Two credential modes (the API key wins when both are set; a PARTIAL ASC_* trio is a loud error,
# never a silent fallback):
#
#   local (default):  NOTARY_PROFILE — a notarytool keychain profile name
#                     (created via `xcrun notarytool store-credentials <name> ...`). If
#                     NOTARY_KEYCHAIN is also set, the profile is resolved from THAT keychain
#                     (`--keychain`) rather than notarytool's login-keychain default — used in CI
#                     to read the profile from the dedicated rhapsody-signing keychain (TRA-257).
#   CI:               ASC_KEY_ID + ASC_ISSUER_ID + an App Store Connect API key, as a file path
#                     (ASC_API_KEY_P8) or base64 (ASC_API_KEY_P8_BASE64, decoded to a chmod-600
#                     temp file). Keychain profiles are created interactively per-machine, so this
#                     is the only mode that works on a throwaway runner.
#
# Usage: notarize.sh <target>          # target is a .app bundle, a .dmg, or a .pkg
#        source notarize.sh --lib-only   # functions only (notarize_args_test.sh)
set -euo pipefail

# resolve_asc_key: when the App Store Connect API key arrives as base64 (CI secrets can't carry
# files), decode it to a chmod-600 temp file and export its path as ASC_API_KEY_P8. An explicitly
# set ASC_API_KEY_P8 always wins; without either, a no-op.
resolve_asc_key() {
  if [ -z "${ASC_API_KEY_P8:-}" ] && [ -n "${ASC_API_KEY_P8_BASE64:-}" ]; then
    # Trailing Xs only: BSD mktemp leaves a template with a suffix after the Xs literal,
    # which collides on the second run. notarytool accepts a key file without a .p8 extension.
    ASC_API_KEY_P8="$(mktemp "${TMPDIR:-/tmp}/rhapsody-asc-key.XXXXXX")"
    chmod 600 "$ASC_API_KEY_P8"
    printf '%s' "$ASC_API_KEY_P8_BASE64" | base64 --decode > "$ASC_API_KEY_P8"
    export ASC_API_KEY_P8
  fi
}

# notary_auth_args: print the notarytool auth args ONE PER LINE (values may contain spaces, e.g.
# a keychain profile name; callers rebuild an array with `while read`). Returns 0 with args on
# stdout; 1 when no credentials are configured (caller skips notarization); 2 on a partial
# API-key trio (caller must fail — half-set CI secrets should never silently skip or fall back).
# Run resolve_asc_key first so ASC_API_KEY_P8_BASE64 counts as the key being present.
notary_auth_args() {
  if [ -n "${ASC_KEY_ID:-}" ] || [ -n "${ASC_ISSUER_ID:-}" ] || [ -n "${ASC_API_KEY_P8:-}" ] || [ -n "${ASC_API_KEY_P8_BASE64:-}" ]; then
    if [ -n "${ASC_KEY_ID:-}" ] && [ -n "${ASC_ISSUER_ID:-}" ] && [ -n "${ASC_API_KEY_P8:-}" ]; then
      printf '%s\n' --key "$ASC_API_KEY_P8" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID"
      return 0
    fi
    echo "notarize: partial App Store Connect API config — need ALL of ASC_KEY_ID, ASC_ISSUER_ID and ASC_API_KEY_P8 (or ASC_API_KEY_P8_BASE64)" >&2
    return 2
  fi
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    printf '%s\n' --keychain-profile "$NOTARY_PROFILE"
    # NOTARY_KEYCHAIN (TRA-257): resolve the profile from a specific keychain (the dedicated
    # rhapsody-signing keychain in CI), not notarytool's login-keychain default. Unset -> unchanged.
    if [ -n "${NOTARY_KEYCHAIN:-}" ]; then
      printf '%s\n' --keychain "$NOTARY_KEYCHAIN"
    fi
    return 0
  fi
  return 1
}

# notarize_target_kind: classify a notarization TARGET by how it must be submitted to Apple, by
# extension alone (so it is unit-testable without a real bundle/xcrun). Prints "bundle" for a .app
# (zip with ditto, submit the zip, staple the .app itself) or "flat" for a .dmg/.pkg (submit + staple
# the file directly). Returns 2 (loud) for anything else so a typo never silently notarizes the wrong
# thing.
notarize_target_kind() {
  case "$1" in
    *.app) printf 'bundle\n' ;;
    *.dmg | *.pkg) printf 'flat\n' ;;
    *)
      echo "notarize: unrecognized target '$1' (expected a .app bundle, a .dmg, or a .pkg)" >&2
      return 2
      ;;
  esac
}

# When sourced (`source notarize.sh --lib-only`), stop here: expose the functions without
# requiring a target argument or touching xcrun/the network.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

TARGET="${1:?usage: notarize.sh <target: .app | .dmg | .pkg>}"

resolve_asc_key
auth_rc=0
auth_out="$(notary_auth_args)" || auth_rc=$?
if [ "$auth_rc" -eq 1 ]; then
  echo "notarize: no notary credentials set (NOTARY_PROFILE or ASC_* API key); skipping notarization + stapling"
  exit 0
elif [ "$auth_rc" -ne 0 ]; then
  exit 1 # notary_auth_args already explained the partial config on stderr
fi

# Classify the target (bundle vs flat) before touching the filesystem or Apple; an unknown extension
# is a loud failure, not a silent skip.
kind="$(notarize_target_kind "$TARGET")" || exit 1

auth_args=()
while IFS= read -r arg; do auth_args+=("$arg"); done <<< "$auth_out"

# --- submit + poll (STUDIO-877) ----------------------------------------------------------------
# We do NOT use `notarytool submit --wait`. notarytool 1.1.2 (Xcode 26.6) stack-overflows (SIGBUS,
# "Could not determine thread index for stack guard region") inside CoreFoundation while formatting
# the progress line `--wait` prints, ~14s after the submission id has been printed and Apple has
# already ACCEPTED the submission. Every release failed there, discarding work Apple had done and
# leaving an orphaned In Progress submission behind.
#
# So: submit once, keep the id, and poll `notarytool info` ourselves. That removes the crashing code
# path and is strictly more robust — the id is ours, so a crashed or disconnected poll no longer
# throws the submission away; it is re-pollable, including by a later run (see STATE_FILE).
#
# Cadence knobs (seconds), overridable for a slow notary queue or a fast test:
NOTARY_POLL_INTERVAL="${NOTARY_POLL_INTERVAL:-30}"
NOTARY_POLL_TIMEOUT="${NOTARY_POLL_TIMEOUT:-1800}"
case "$NOTARY_POLL_INTERVAL" in *[!0-9]* | "") echo "notarize: NOTARY_POLL_INTERVAL must be a whole number of seconds, got '$NOTARY_POLL_INTERVAL'" >&2; exit 1 ;; esac
case "$NOTARY_POLL_TIMEOUT" in *[!0-9]* | "") echo "notarize: NOTARY_POLL_TIMEOUT must be a whole number of seconds, got '$NOTARY_POLL_TIMEOUT'" >&2; exit 1 ;; esac

# Resumability: the submission id is recorded beside the TARGET (under NOTARY_STATE_DIR if set) as
# "<sha256-of-submitted-file> <id>", so a re-run polls the existing submission instead of paying
# Apple for a second one. The digest is the guard: it is the fingerprint of the exact bytes that were
# submitted, so a REBUILT artifact never resumes an id that belongs to different bytes (which would
# staple a ticket that does not match). `desktop/build/bin/` is gitignored, so this leaves no
# tracked file behind.
STATE_FILE="${NOTARY_STATE_DIR:-$(dirname "$TARGET")}/$(basename "$TARGET").notary-id"

# artifact_digest <file>: sha256 of the bytes actually handed to notarytool.
artifact_digest() {
  shasum -a 256 "$1" | awk '{print $1}'
}

# submit_or_resume <file>: obtain a notarytool submission id for an already-signed file (a dmg/pkg,
# or a zipped .app) — by resuming the one recorded for these exact bytes, or by submitting. Sets
# SUBMISSION_ID. Returns non-zero (loudly) if notarytool fails or hands back no id; we never poll on
# a guessed or empty id.
SUBMISSION_ID=""
submit_or_resume() {
  local file="$1" digest saved_digest saved_id out rc
  digest="$(artifact_digest "$file")"

  if [ -f "$STATE_FILE" ]; then
    saved_digest=""
    saved_id=""
    read -r saved_digest saved_id < "$STATE_FILE" || true
    if [ -n "$saved_id" ] && [ "$saved_digest" = "$digest" ]; then
      echo "notarize: resuming submission $saved_id from $STATE_FILE (same artifact — not re-submitting)"
      SUBMISSION_ID="$saved_id"
      return 0
    fi
    if [ -n "$saved_id" ]; then
      echo "notarize: $STATE_FILE holds submission $saved_id for different bytes; submitting the rebuilt artifact instead"
    fi
  fi

  if [ -n "${ASC_KEY_ID:-}" ]; then
    echo "notarize: submitting $file to Apple (notarytool, App Store Connect API key '$ASC_KEY_ID')"
  else
    echo "notarize: submitting $file to Apple (notarytool, profile '$NOTARY_PROFILE')"
  fi

  rc=0
  # --output-format json: the id is read from machine-readable output, never scraped off the human
  # progress line (which is exactly the kind of thing a notarytool update breaks).
  # stdout only feeds jq: notarytool writes warnings and progress to stderr, and one such line on a
  # successful submit would make the JSON unparseable and lose the id. stderr goes to a side file
  # that is shown only when something fails.
  errf="$(mktemp "${TMPDIR:-/tmp}/notarize-submit.XXXXXX")" || { echo "notarize: cannot create a scratch file" >&2; return 1; }
  out="$(xcrun notarytool submit "$file" "${auth_args[@]}" --output-format json 2>"$errf")" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "notarize: submission failed — notarytool submit exited $rc" >&2
    printf '%s\n' "$out" >&2
    cat "$errf" >&2
    rm -f "$errf"
    return 1
  fi
  SUBMISSION_ID="$(printf '%s' "$out" | jq -re '.id // empty' 2>/dev/null)" || SUBMISSION_ID=""
  if [ -z "$SUBMISSION_ID" ]; then
    echo "notarize: notarytool reported success but returned no submission id — refusing to poll blindly" >&2
    printf '%s\n' "$out" >&2
    cat "$errf" >&2
    rm -f "$errf"
    return 1
  fi
  rm -f "$errf"
  echo "notarize: submission id $SUBMISSION_ID"

  if ! printf '%s %s\n' "$digest" "$SUBMISSION_ID" > "$STATE_FILE" 2>/dev/null; then
    # Not fatal: the run continues and polls normally, it just cannot be resumed cheaply.
    echo "notarize: warning: could not record the submission id at $STATE_FILE — a re-run will submit again" >&2
  fi
}

# poll_until_done <id>: poll notarytool until the submission leaves "In Progress". Distinct exit
# codes, because these are distinct outcomes that must not collapse into one error:
#   0  Accepted
#   1  Invalid   — Apple examined it and refused; the notary log is fetched and printed
#   2  Rejected  — Apple refused the submission itself
#   3  timed out while still In Progress
#   4  timed out without ever reading a status (every poll failed)
poll_until_done() {
  local id="$1" start now elapsed=0 rc out status attempts=0 reads=0 last="unknown" errf
  errf="$(mktemp "${TMPDIR:-/tmp}/notarize-info.XXXXXX")" || { echo "notarize: cannot create a scratch file" >&2; return 1; }
  trap 'rm -f "${errf:-}"' RETURN
  start="$(date +%s)"
  echo "notarize: waiting for Apple — polling every ${NOTARY_POLL_INTERVAL}s, up to ${NOTARY_POLL_TIMEOUT}s (submission $id)"
  while :; do
    attempts=$((attempts + 1))
    rc=0
    out="$(xcrun notarytool info "$id" "${auth_args[@]}" --output-format json 2>"$errf")" || rc=$?
    status=""
    if [ "$rc" -eq 0 ]; then
      status="$(printf '%s' "$out" | jq -re '.status // empty' 2>/dev/null)" || status=""
    fi
    now="$(date +%s)"
    elapsed=$((now - start))

    if [ -z "$status" ]; then
      # A failed or unreadable poll is NOT a verdict. Apple's copy of the submission is unaffected
      # by our crash (that is the whole STUDIO-877 lesson), so retry until the deadline.
      echo "notarize: poll failed (attempt $attempts, notarytool info exited $rc) — the submission is unaffected, retrying" >&2
      printf '%s\n' "$out" >&2
      cat "$errf" >&2
    else
      reads=$((reads + 1))
      last="$status"
      case "$status" in
        Accepted)
          echo "notarize: Apple Accepted submission $id after ${elapsed}s"
          return 0
          ;;
        Invalid)
          echo "notarize: Apple returned Invalid for submission $id — fetching the notary log" >&2
          # A rejection with no reason in the CI log is a dead end for whoever reads it next.
          xcrun notarytool log "$id" "${auth_args[@]}" >&2 \
            || echo "notarize: could not fetch the notary log; run: xcrun notarytool log $id" >&2
          return 1
          ;;
        Rejected)
          # Distinct from Invalid: Apple refused the submission itself rather than examining and
          # failing its contents, and produces no notary log for it — so we print the command
          # instead of an empty fetch.
          echo "notarize: Apple Rejected submission $id after ${elapsed}s — inspect it with: xcrun notarytool info $id" >&2
          return 2
          ;;
        "In Progress")
          echo "notarize: status In Progress (attempt $attempts, ${elapsed}s elapsed)"
          ;;
        *)
          echo "notarize: unrecognized status '$status' for submission $id (attempt $attempts) — continuing to poll" >&2
          ;;
      esac
    fi

    if [ "$elapsed" -ge "$NOTARY_POLL_TIMEOUT" ]; then
      if [ "$reads" -eq 0 ]; then
        echo "notarize: gave up after ${elapsed}s — notarytool never returned a readable status for submission $id. The submission is NOT lost: re-run to resume polling it, or check 'xcrun notarytool info $id'." >&2
        return 4
      fi
      echo "notarize: timed out after ${elapsed}s waiting for submission $id (last status: $last). The submission is NOT lost: re-run to resume polling it." >&2
      return 3
    fi
    sleep "$NOTARY_POLL_INTERVAL"
  done
}

# submit_to_apple <file>: submit (or resume) and wait for Apple's verdict, without `--wait`.
submit_to_apple() {
  submit_or_resume "$1"
  poll_until_done "$SUBMISSION_ID"
}

if [ "$kind" = bundle ]; then
  [ -d "$TARGET" ] || { echo "notarize: app bundle not found: $TARGET (run 'make app' first)" >&2; exit 1; }
  # notarytool won't accept a directory; zip the bundle (keepParent preserves the .app dir inside the
  # archive), submit the zip, then staple the ORIGINAL .app — the ticket attaches to the bundle, not
  # the throwaway zip. A temp DIR (fixed inner name) sidesteps BSD mktemp's trailing-Xs-only rule,
  # which mangles a `.zip` suffix; the trap removes it even if submission fails under `set -e`.
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/rhapsody-notarize.XXXXXX")"
  trap 'rm -rf "$tmpdir"' EXIT
  zip="$tmpdir/$(basename "$TARGET").zip"
  echo "notarize: zipping bundle $TARGET -> $zip"
  ditto -c -k --keepParent "$TARGET" "$zip"
  submit_to_apple "$zip"
  echo "notarize: stapling ticket to $TARGET"
  xcrun stapler staple "$TARGET"
  xcrun stapler validate "$TARGET"
  echo "notarize: done (notarized + stapled $TARGET)"
else
  [ -f "$TARGET" ] || { echo "notarize: file not found: $TARGET (run 'make dmg' first)" >&2; exit 1; }
  submit_to_apple "$TARGET"
  echo "notarize: stapling ticket to $TARGET"
  xcrun stapler staple "$TARGET"
  xcrun stapler validate "$TARGET"
  echo "notarize: done (notarized + stapled $TARGET)"
fi
