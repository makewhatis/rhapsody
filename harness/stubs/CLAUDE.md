# CLAUDE.md — harness/stubs

`harness/CLAUDE.md`'s "stubs/" section already covers the protocol contract, env knobs, and
`linear-stub`'s scope — read that first. This file covers what's only visible by reading the
scripts themselves: deployment/colocation constraints and runtime quirks not written down there.

## The three files are a deployment unit, not three independent scripts

`fake-claude-error` and `fake-claude-hang` resolve their target with
`exec "$(dirname "$0")/fake-claude" "$@"` — `$0` is whatever path the caller invoked, so this only
finds `fake-claude` if the wrapper was invoked by path (not a bare name resolved through `$PATH`)
and `fake-claude` sits next to it at that path. Both real callers respect this by copying (not
symlinking) the set together:

- `capture/capture.sh` copies all three (`fake-claude`, `-error`, `-hang`) into
  `$CAPTURE_HOME/bin/` as one `cp` before any scenario runs, even though a given scenario only
  points `claude.command` at one of them — because `-error`/`-hang` need the real `fake-claude`
  alongside them to `exec` into.
- `e2e/boot.sh` copies only `fake-claude` itself, since `boot.sh` only ever drives the success
  path.

If you add a fourth variant or restructure this directory, preserve the "copy all three together,
same directory" invariant — splitting `fake-claude` from its wrappers breaks them at exec time, not
at edit time, so the failure won't show up until a capture/e2e run tries to invoke the copy.

## Runtime quirks worth knowing before you assert against a run

- **Exit code is always 0**, including `FAKE_CLAUDE_OUTCOME=error`. Success/failure is only
  encoded in the terminal JSONL line's `is_error` field, never in the process exit status — a
  check against `$?` will see every run as clean.
- **`FAKE_CLAUDE_HANG=1` short-circuits before the sleep.** It emits `init` + one `assistant` line,
  then loops `sleep 3600` forever — it never reaches the `FAKE_CLAUDE_SLEEP_S` sleep or the
  "finishing" line. Setting `FAKE_CLAUDE_SLEEP_S` alongside `FAKE_CLAUDE_HANG=1` has no effect.
- **`SESSION_ID` is the literal constant `fake-claude-session`** on every invocation, not
  generated per run — don't use it to distinguish concurrent or sequential fake-claude runs in a
  test.
- **Token usage is hardcoded** to `{"input_tokens":1,"output_tokens":1}` on the terminal result
  regardless of outcome — it carries no signal about the run.
- The background stdin-drain (`cat >/dev/null &`) is only explicitly `kill`ed on the normal-exit
  path (`|| true`, so a missing PID isn't fatal). On the hang path nothing inside the script ever
  reaps it — the caller's process-group SIGKILL (the turn-timeout/kill path being exercised) is
  what cleans it up.
