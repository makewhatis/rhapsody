#!/usr/bin/env bash
# End-to-end tests for notarize.sh's submit + poll loop, driven by a FAKE `xcrun` on PATH — no
# Apple, no network, no real notarytool. Sibling of notarize_args_test.sh, which covers the pure
# arg-construction lib; this one covers the parts that shell out.
#
# Why this file exists (STUDIO-877): `notarytool submit --wait` 1.1.2 stack-overflows (SIGBUS)
# formatting its progress line, ~14s after Apple has already ACCEPTED the submission, so every
# release threw away work Apple had done. notarize.sh no longer uses `--wait`; it captures the
# submission id from `--output-format json` and polls `notarytool info` itself. That poll loop has
# five failure branches a release exercises once, under time pressure, so each one is pinned here:
# Accepted, Invalid (+ log fetch), Rejected, a status that never leaves In Progress, and a poll that
# EXITS NON-ZERO mid-loop (the crash itself — the loop must retry, not abort).
#
# Usage: bash desktop/scripts/notarize_poll_test.sh
#
# shellcheck disable=SC2030,SC2031 # env mutations being subshell-local is the isolation mechanism
set -euo pipefail

cd "$(dirname "$0")"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/notarize-poll-test.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

FAILS=0
check_eq() { # name expected actual
  if [ "$2" = "$3" ]; then
    echo "ok   $1"
  else
    echo "FAIL $1: expected [$2], got [$3]" >&2
    FAILS=$((FAILS + 1))
  fi
}
check_contains() { # name haystack needle
  case "$2" in
    *"$3"*) echo "ok   $1" ;;
    *)
      echo "FAIL $1: expected output to contain [$3], got:" >&2
      printf '%s\n' "$2" >&2
      FAILS=$((FAILS + 1))
      ;;
  esac
}
check_nonzero() { # name rc
  if [ "$2" -ne 0 ]; then
    echo "ok   $1"
  else
    echo "FAIL $1: expected a non-zero exit, got 0" >&2
    FAILS=$((FAILS + 1))
  fi
}
check_not_contains() { # name haystack needle
  case "$2" in
    *"$3"*)
      echo "FAIL $1: expected output NOT to contain [$3], got:" >&2
      printf '%s\n' "$2" >&2
      FAILS=$((FAILS + 1))
      ;;
    *) echo "ok   $1" ;;
  esac
}

FAKE_ID="5ccac09c-9301-42a7-bc95-a7fbab2b963b"
export FAKE_ID

# --- the fake xcrun --------------------------------------------------------------------------
# Answers `notarytool submit|info|log` and `stapler staple|validate` from files in $FAKE_XCRUN_DIR,
# and appends every invocation to calls.log so a test can assert what was (and was NOT) called.
FAKEBIN="$SCRATCH/bin"
mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/xcrun" <<'FAKE'
#!/usr/bin/env bash
set -u
dir="$FAKE_XCRUN_DIR"
printf '%s\n' "$*" >> "$dir/calls.log"
tool="${1:-}"; shift || true
case "$tool" in
  notarytool)
    sub="${1:-}"; shift || true
    case "$sub" in
      submit)
        cat "$dir/stderr_noise" >&2 2>/dev/null || true
        cat "$dir/submit_out" 2>/dev/null || true
        exit "$(cat "$dir/submit_rc" 2>/dev/null || echo 0)"
        ;;
      info)
        n=$(cat "$dir/info_n" 2>/dev/null || echo 0)
        n=$((n + 1))
        printf '%s' "$n" > "$dir/info_n"
        total=$(grep -c '' "$dir/status_seq")
        [ "$n" -gt "$total" ] && n="$total"
        line=$(sed -n "${n}p" "$dir/status_seq")
        if [ "$line" = "ERR" ]; then
          # The STUDIO-877 crash shape: notarytool dies on a signal, printing nothing usable.
          echo "fake notarytool: Bus error: 10" >&2
          exit 138
        fi
        cat "$dir/stderr_noise" >&2 2>/dev/null || true
        printf '{"id":"%s","status":"%s"}\n' "$FAKE_ID" "$line"
        ;;
      log)
        cat "$dir/log_out" 2>/dev/null || true
        ;;
      *) echo "fake xcrun: unexpected notarytool subcommand '$sub'" >&2; exit 64 ;;
    esac
    ;;
  stapler) exit 0 ;;
  *) echo "fake xcrun: unexpected tool '$tool'" >&2; exit 64 ;;
esac
FAKE
chmod +x "$FAKEBIN/xcrun"

# fake_reset <case-name> [status...]: fresh control dir for one case, seeded with a status sequence
# consumed one entry per `notarytool info` call (the last entry repeats forever). "ERR" makes that
# poll die on a signal instead of answering.
fake_reset() {
  FAKE_XCRUN_DIR="$SCRATCH/fake-$1"
  shift
  rm -rf "$FAKE_XCRUN_DIR"
  mkdir -p "$FAKE_XCRUN_DIR"
  : > "$FAKE_XCRUN_DIR/calls.log"
  printf '{"id":"%s","message":"Successfully uploaded file","path":"/x"}\n' "$FAKE_ID" \
    > "$FAKE_XCRUN_DIR/submit_out"
  printf '%s\n' "$@" > "$FAKE_XCRUN_DIR/status_seq"
  printf 'fake notarytool log: "message": "The signature does not include a secure timestamp."\n' \
    > "$FAKE_XCRUN_DIR/log_out"
  export FAKE_XCRUN_DIR
}

calls() { cat "$FAKE_XCRUN_DIR/calls.log"; }
count_calls() { grep -c -- "$1" "$FAKE_XCRUN_DIR/calls.log" || true; }

# run_notarize <target>: run the real notarize.sh against the fake xcrun with notary credentials
# configured and the poll cadence POLL_INTERVAL/POLL_TIMEOUT currently select. Prints combined
# stdout+stderr; sets RC. (Set the two knobs explicitly around a case rather than as an env prefix
# on this call — bash leaks a prefixed assignment on a *function* past the call.)
POLL_INTERVAL=0
POLL_TIMEOUT=5
# Sets OUT and RC. It must NOT be called inside `$(...)`: a command substitution is a subshell, so
# an RC assigned in there is discarded and every failure case reads as a pass.
RC=0
OUT=""
run_notarize() {
  RC=0
  (
    unset ASC_KEY_ID ASC_ISSUER_ID ASC_API_KEY_P8 ASC_API_KEY_P8_BASE64 NOTARY_KEYCHAIN
    export PATH="$FAKEBIN:$PATH"
    export NOTARY_PROFILE=fake-notary
    export NOTARY_POLL_INTERVAL="$POLL_INTERVAL"
    export NOTARY_POLL_TIMEOUT="$POLL_TIMEOUT"
    bash ./notarize.sh "$1"
  ) > "$SCRATCH/run.out" 2>&1 || RC=$?
  OUT="$(cat "$SCRATCH/run.out")"
}

new_dmg() { # <name> <contents> -> path
  local p="$SCRATCH/$1"
  printf '%s' "$2" > "$p"
  printf '%s' "$p"
}

# --- 1. Accepted on the first poll -------------------------------------------------------------
fake_reset accepted Accepted
dmg=$(new_dmg one.dmg "dmg-one")
run_notarize "$dmg"; out="$OUT"
check_eq "accepted: rc" "0" "$RC"
check_contains "accepted: reports Accepted" "$out" "Accepted"
check_contains "accepted: staples" "$out" "stapling ticket"
check_eq "accepted: one submit" "1" "$(count_calls 'notarytool submit')"
check_eq "accepted: one info poll" "1" "$(count_calls 'notarytool info')"
check_eq "accepted: staple + validate" "2" "$(count_calls stapler)"

# The whole point of the ticket: --wait is the crashing code path and must never be passed again.
check_not_contains "accepted: never passes --wait" "$(calls)" "--wait"
# And the id must come from machine-readable output, not a scraped human line.
check_contains "accepted: submits as json" "$(calls)" "--output-format json"

# --- 2. In Progress twice, then Accepted -------------------------------------------------------
fake_reset inprogress "In Progress" "In Progress" Accepted
dmg=$(new_dmg two.dmg "dmg-two")
run_notarize "$dmg"; out="$OUT"
check_eq "in-progress: rc" "0" "$RC"
check_eq "in-progress: polled three times" "3" "$(count_calls 'notarytool info')"
check_contains "in-progress: reports waiting" "$out" "In Progress"

# --- 3. Invalid: distinct failure, and the reason is FETCHED and PRINTED -----------------------
fake_reset invalid Invalid
dmg=$(new_dmg three.dmg "dmg-three")
run_notarize "$dmg"; out="$OUT"
check_eq "invalid: rc" "1" "$RC"
check_contains "invalid: says Invalid" "$out" "Invalid"
check_eq "invalid: fetched the log" "1" "$(count_calls 'notarytool log')"
check_contains "invalid: prints the log body" "$out" "does not include a secure timestamp"
check_eq "invalid: never staples" "0" "$(count_calls stapler)"

# --- 4. Rejected: a DIFFERENT outcome from Invalid and from a timeout --------------------------
fake_reset rejected Rejected
dmg=$(new_dmg four.dmg "dmg-four")
run_notarize "$dmg"; out="$OUT"
check_eq "rejected: rc" "2" "$RC"
check_contains "rejected: says rejected" "$out" "Rejected"
check_not_contains "rejected: not reported as a timeout" "$out" "timed out"
check_eq "rejected: never staples" "0" "$(count_calls stapler)"

# --- 5. Never leaves In Progress: times out, distinctly, keeping the id ------------------------
fake_reset timeout "In Progress"
dmg=$(new_dmg five.dmg "dmg-five")
POLL_INTERVAL=1 POLL_TIMEOUT=1
run_notarize "$dmg"; out="$OUT"
POLL_INTERVAL=0 POLL_TIMEOUT=5
check_eq "timeout: rc" "3" "$RC"
check_contains "timeout: says timed out" "$out" "timed out"
check_contains "timeout: names the submission id" "$out" "$FAKE_ID"
check_eq "timeout: never staples" "0" "$(count_calls stapler)"

# --- 6. A poll that EXITS NON-ZERO mid-loop must RETRY, not abort ------------------------------
#     This is the STUDIO-877 crash itself. Apple had already accepted the submission; the only
#     thing that failed was our read of it.
fake_reset pollcrash ERR ERR Accepted
dmg=$(new_dmg six.dmg "dmg-six")
run_notarize "$dmg"; out="$OUT"
check_eq "poll crash: rc" "0" "$RC"
check_eq "poll crash: retried past both failures" "3" "$(count_calls 'notarytool info')"
check_contains "poll crash: warns about the failed poll" "$out" "poll failed"
check_eq "poll crash: still staples" "2" "$(count_calls stapler)"

# --- 7. Every poll fails: distinct from a plain timeout ----------------------------------------
fake_reset allpollsfail ERR
dmg=$(new_dmg seven.dmg "dmg-seven")
POLL_INTERVAL=1 POLL_TIMEOUT=1
run_notarize "$dmg"; out="$OUT"
POLL_INTERVAL=0 POLL_TIMEOUT=5
check_eq "all polls fail: rc" "4" "$RC"
check_contains "all polls fail: says never read a status" "$out" "never returned a readable status"
check_contains "all polls fail: names the submission id" "$out" "$FAKE_ID"
check_eq "all polls fail: never staples" "0" "$(count_calls stapler)"

# --- 8. Resumability: the id is recorded, and a re-run polls it instead of re-submitting -------
fake_reset resume Accepted
dmg=$(new_dmg eight.dmg "dmg-eight")
run_notarize "$dmg"; out="$OUT"
check_eq "resume: first run rc" "0" "$RC"
if [ -f "$dmg.notary-id" ]; then
  echo "ok   resume: records the submission id next to the target"
else
  echo "FAIL resume: expected a state file at $dmg.notary-id" >&2
  FAILS=$((FAILS + 1))
fi
check_contains "resume: state file holds the id" "$(cat "$dmg.notary-id")" "$FAKE_ID"

: > "$FAKE_XCRUN_DIR/calls.log"
rm -f "$FAKE_XCRUN_DIR/info_n"
run_notarize "$dmg"; out="$OUT"
check_eq "resume: second run rc" "0" "$RC"
check_eq "resume: does NOT pay Apple again" "0" "$(count_calls 'notarytool submit')"
check_eq "resume: polls the existing submission" "1" "$(count_calls 'notarytool info')"
check_contains "resume: says it resumed" "$out" "resuming"

# --- 9. A CHANGED artifact must not reuse the old id -------------------------------------------
printf '%s' "dmg-eight-rebuilt" > "$dmg"
: > "$FAKE_XCRUN_DIR/calls.log"
rm -f "$FAKE_XCRUN_DIR/info_n"
run_notarize "$dmg"; out="$OUT"
check_eq "changed artifact: rc" "0" "$RC"
check_eq "changed artifact: submits a fresh submission" "1" "$(count_calls 'notarytool submit')"

# --- 10. submit itself failing is a loud failure, not a poll on nothing ------------------------
fake_reset submitfail Accepted
printf '%s' "3" > "$FAKE_XCRUN_DIR/submit_rc"
dmg=$(new_dmg ten.dmg "dmg-ten")
run_notarize "$dmg"; out="$OUT"
check_nonzero "submit failure: rc is non-zero" "$RC"
check_contains "submit failure: explains itself" "$out" "submission failed"
check_eq "submit failure: never polls" "0" "$(count_calls 'notarytool info')"
check_eq "submit failure: never staples" "0" "$(count_calls stapler)"

# --- 11. submit output with no id is a loud failure, never an empty-id poll --------------------
fake_reset noid Accepted
printf '%s\n' '{"message":"Successfully uploaded file"}' > "$FAKE_XCRUN_DIR/submit_out"
dmg=$(new_dmg eleven.dmg "dmg-eleven")
run_notarize "$dmg"; out="$OUT"
check_nonzero "no id: rc is non-zero" "$RC"
check_contains "no id: explains itself" "$out" "no submission id"
check_eq "no id: never polls" "0" "$(count_calls 'notarytool info')"

# --- 12. The .app bundle path takes the same crash-free route (decision 4 in the ticket) -------
#     It works today, but it is the same call through the same crashing code, so it must not keep
#     using --wait either.
fake_reset appbundle "In Progress" Accepted
app="$SCRATCH/Rhapsody.app"
mkdir -p "$app/Contents/MacOS"
printf '%s' "fake binary" > "$app/Contents/MacOS/Rhapsody"
run_notarize "$app"; out="$OUT"
check_eq "app bundle: rc" "0" "$RC"
check_eq "app bundle: submits the zip once" "1" "$(count_calls 'notarytool submit')"
check_eq "app bundle: polls twice" "2" "$(count_calls 'notarytool info')"
check_not_contains "app bundle: never passes --wait" "$(calls)" "--wait"
check_contains "app bundle: staples the bundle, not the zip" "$out" "$app"
if [ -f "$app.notary-id" ]; then
  echo "ok   app bundle: records its submission id beside the bundle"
else
  echo "FAIL app bundle: expected a state file at $app.notary-id" >&2
  FAILS=$((FAILS + 1))
fi

# --- 12b. stderr noise on a SUCCESSFUL submit/info must not corrupt the JSON ------------------------
fake_reset stderrnoise Accepted
printf 'notarytool: warning: something on stderr\n' > "$FAKE_XCRUN_DIR/stderr_noise"
dmg=$(new_dmg noise.dmg "dmg-noise")
run_notarize "$dmg"; out="$OUT"
check_eq "stderr noise: rc" "0" "$RC"
check_contains "stderr noise: id still read" "$out" "submission id $FAKE_ID"
check_contains "stderr noise: id recorded" "$(cat "$dmg.notary-id" 2>/dev/null)" "$FAKE_ID"

# --- 13. Unconfigured credentials still short-circuit before any xcrun call --------------------
fake_reset unconfigured Accepted
dmg=$(new_dmg thirteen.dmg "dmg-thirteen")
RC=0
out=$(
  unset NOTARY_PROFILE NOTARY_KEYCHAIN ASC_KEY_ID ASC_ISSUER_ID ASC_API_KEY_P8 ASC_API_KEY_P8_BASE64
  export PATH="$FAKEBIN:$PATH"
  bash ./notarize.sh "$dmg" 2>&1
) || RC=$?
check_eq "unconfigured: rc" "0" "$RC"
check_contains "unconfigured: says it skipped" "$out" "skipping notarization"
check_eq "unconfigured: called no xcrun at all" "" "$(calls)"

if [ "$FAILS" -gt 0 ]; then
  echo "FAIL: $FAILS test(s) failed" >&2
  exit 1
fi
echo "PASS"
