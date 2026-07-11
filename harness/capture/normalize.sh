#!/usr/bin/env bash
# normalize — canonicalize captured daemon output so committed fixtures are deterministic
# and diff cleanly across recaptures. Reads stdin, writes stdout. JSON inputs are piped
# through `jq -S .` FIRST (by capture.sh's grab()) so object key order is already stable;
# this pass only rewrites the machine-specific / wall-clock VALUES to fixed placeholders.
#
# SINGLE SOURCE OF TRUTH for the placeholder rules. crates/harness-fixtures::normalize
# (Task R5) mirrors these EXACTLY — change them in lockstep or the drift canary fails.
#
# Placeholder classes (spec §3.2, plan R4 Global Constraints): <TIMESTAMP> <UUID> <HOME>
# <PORT> <NUM>. Requires CAPTURE_HOME to be exported (the daemon's $HOME for this capture).
# Output stays VALID JSON: every placeholder lands inside an existing string ("..."), and
# the one that replaces a bare number (<NUM>) is emitted QUOTED as "<NUM>".
#
# RULES (each line below), in order:
#   1. RFC3339 timestamps, quoted        -> "<TIMESTAMP>"   (started_at/ended_at/at/due_at/
#                                                             generated_at/log time; Z or ±hh:mm)
#   2. bare YYYY-MM-DD dates, quoted      -> "<TIMESTAMP>"   (metrics `date` day bucket)
#   3. compact run-transcript timestamps  -> <TIMESTAMP>     (obslog filenames embed
#      (20060102T150405.000000000Z), unquoted INSIDE transcript_path — the nanosecond field
#      makes them change every run; the surrounding path is caught by rule 5.)
#   4. UUIDs (inside a string)            -> <UUID>
#   5. the capture HOME dir (inside a str) -> <HOME>         ($CAPTURE_HOME: db, workspaces,
#                                                             transcripts, fake-claude command)
#   6. loopback host:port (inside a str)  -> 127.0.0.1:<PORT>
#   7. wall-clock measurement numerics    -> "<NUM>"         (seconds_running; and, forward-
#      (keys ending _at_ms / *duration* / *_running)         looking, unix-ms and duration
#      fields). QUOTED so the fixture stays valid JSON.
#
# Deviations from the plan's five base rules — recorded here and mirrored by the Rust normalizer:
#   * Rules 2 and 3 are ADDITIONS: the double-capture diff (plan R4 Step 5) is otherwise non-empty
#     because the metrics day bucket and the obslog transcript filename are not RFC3339-with-time.
#   * Rule 7 QUOTES the placeholder ("<NUM>", not a bare <NUM>) so fixtures remain parseable JSON
#     (harness-fixtures::load_json / the R5 canary parse them). A bare <NUM> in a numeric position
#     is invalid JSON.
#   * Rule 7 matches wall-clock MEASUREMENT suffixes (_at_ms / duration / _running), NOT the plan's
#     broad `_ms`. Every `_ms` field in the v0.4.0 fixtures is a DETERMINISTIC config constant
#     (polling.interval_ms, claude.*_timeout_ms, agent.max_retry_backoff_ms, hooks.timeout_ms,
#     state.poll_interval_ms); normalizing those would erase parity-checkable values (weakening the
#     config golden) for zero determinism gain. Only genuinely nondeterministic numerics (the
#     seconds_running wall-clock float) are normalized.
set -euo pipefail
sed -E \
  -e 's/"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+(Z|[+-][0-9:]+)"/"<TIMESTAMP>"/g' \
  -e 's/"[0-9]{4}-[0-9]{2}-[0-9]{2}"/"<TIMESTAMP>"/g' \
  -e 's/[0-9]{8}T[0-9]{6}\.[0-9]+Z/<TIMESTAMP>/g' \
  -e 's/[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/<UUID>/g' \
  -e "s|${CAPTURE_HOME:?set CAPTURE_HOME to the daemon home before normalizing}|<HOME>|g" \
  -e 's/127\.0\.0\.1:[0-9]+/127.0.0.1:<PORT>/g' \
  -e 's/"([a-z_]*(_at_ms|duration|_running))": *[0-9]+(\.[0-9]+)?/"\1": "<NUM>"/g'
