# harness-spike — real captured event streams, one per candidate harness

Captured by the STUDIO-869 spike on 2026-09-11 by running **one real multi-tool turn per harness
against a real paid provider**, in a git sandbox, with the daemon's own MCP server attached. These
are the samples the pluggable-harnesses adapter slices are built against.

The written findings — failure classification, resume, concurrency, the kill path, real-provider
behaviour, and the corrections they force on the design — live in
`~/.rhapsody/docs/STUDIO-869-harness-spike-findings.md`, not in this repo.

## These are NOT `harness/fixtures/` goldens — do not move them there

`harness/fixtures/` is exclusively Go-reference output: `harness/capture/capture.sh:39` does
`rm -rf "$FIX"` before every `make fixtures`, and `crates/harness-fixtures`'s `all_fixtures()`
recursively walks that tree and pushes every file through `normalize`. A capture from `claude` /
`codex` / `opencode` is neither Go output nor normalizable, so filing it there would both delete it
on the next recapture and drag it into the parity canary. Hence this sibling directory.

Nothing in the Rust workspace reads these files yet. They are inputs for the adapter slices (design
§8, "golden the event stream per harness"), which will decide how to consume them.

## What is here

| Path | What it is |
|---|---|
| `claude/happy.jsonl` | Full multi-tool turn: 6 tool calls (2 `Read`, 1 `Edit`, 1 `Bash`, 1 `ToolSearch`, 1 `mcp__symphony__symphony_state`), terminal `result`. |
| `claude/failure-unrecognized-model.jsonl` + `.stderr` | Deliberate failure (`--model no-such-model-xyz`). |
| `claude/resume.jsonl` | `--resume <session_id>`, same flags, answering from the prior turn's context. |
| `codex/happy.jsonl` | Same multi-tool turn, **with** `--dangerously-bypass-approvals-and-sandbox`. |
| `codex/mcp-refused-approval-policy.jsonl` | The same turn **without** that flag: both MCP calls refused, `turn.completed`, exit 0. |
| `codex/failure-401.jsonl` | Deliberate failure (bogus key): 5 non-terminal `error` retries, then `turn.failed`. |
| `codex/resume.jsonl` | `codex exec resume <thread_id>`. |
| `codex/config.toml` | The `$CODEX_HOME/config.toml` that produced every codex capture. |
| `opencode/happy.jsonl` | Same multi-tool turn. |
| `opencode/long-turn.jsonl` | A longer turn: 11 tool calls over 6 steps, all correct. |
| `opencode/failure-401.jsonl` | Deliberate failure: one in-band `error` event carrying `statusCode` and `isRetryable`. |
| `opencode/resume.jsonl` | `run -s <sessionID>`. |
| `opencode/opencode.json` | The project config that produced every opencode capture. |
| `goose/failure-401-exit0.stdout` | Goose failing a 401 while **exiting 0**, with the error on stdout. Failure path only — see below. |
| `sandbox/` | The scripts and prompts that produced all of the above. |

`sandbox/mksandbox.sh` builds the sandbox repo (honours `$RHAPSODYD` and `$WORKFLOW`);
`sandbox/drive.py` mimics the daemon's turn loop (own process group, stream stdout, close stdin on
the terminal line, reap); `sandbox/killtest.py` reproduces the daemon's exact kill
(`process_group(0)` + `kill(-pid, SIGKILL)`) and reports surviving descendants.

## Provenance — the exact command per capture

Common to all: sandbox built by `sandbox/mksandbox.sh`, prompt `sandbox/prompt-multitool.txt`,
MCP server `rhapsodyd mcp ~/.rhapsody/WORKFLOW.md` (the live daemon binary), tool exercised
`symphony_state`.

Versions pinned at capture time: **Claude Code 2.1.267**, **codex-cli 0.153.4**,
**opencode 1.18.30**, **goose 1.49.0**.

```sh
# claude/happy.jsonl  — provider: Anthropic (claude-haiku-4-5-20251001)
claude -p --output-format stream-json --input-format stream-json --verbose \
  --permission-mode bypassPermissions --model claude-haiku-4-5-20251001 \
  --mcp-config "$SB/.symphony-mcp.json" --strict-mcp-config
#   (prompt written to stdin as one stream-json user message, exactly as crates/agent does)

# claude/failure-unrecognized-model.jsonl — same, with --model no-such-model-xyz
# claude/resume.jsonl                     — same, plus --resume <session_id>

# codex/*  — provider: Fireworks (accounts/fireworks/models/deepseek-v4p1-flash) via CODEX_HOME/config.toml
CODEX_HOME=<scratch> FIREWORKS_API_KEY=<key> \
  codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check "$PROMPT" </dev/null
#   mcp-refused-approval-policy.jsonl: identical but `-s workspace-write` instead of the bypass flag
#   failure-401.jsonl:                 identical but FIREWORKS_API_KEY=fw_BOGUSKEY
#   resume.jsonl:                      `codex exec resume <thread_id> --json ... "$PROMPT"`

# opencode/*  — provider: Fireworks, same model; MCP from the project-local opencode.json
opencode run --format json --auto --dir "$SB" \
  -m fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash "$PROMPT"
#   long-turn.jsonl:  same, prompt sandbox/prompt-long.txt
#   failure-401.jsonl: same, run under XDG_DATA_HOME=<empty scratch> with a bogus key
#   resume.jsonl:      same, plus -s <sessionID>

# goose/failure-401-exit0.stdout — provider: OpenAI endpoint with a deliberately bogus key
HOME=<scratch> OPENAI_API_KEY=sk-BOGUSKEY GOOSE_DISABLE_KEYRING=1 \
  goose run --no-session -t "Read NOTES.md and tell me the counter value."
```

`opencode` must be the Homebrew build at `/opt/homebrew/Cellar/opencode/<v>/bin/opencode`. On the
capture machine `/opt/homebrew/bin/opencode` is a **broken npm-global symlink** whose postinstall
never ran; it exits 1 with an install error on stderr and runs nothing. A daemon resolving
`opencode` from `PATH` there would get the broken one.

## Reading these files honestly

- **Every byte here was executed.** Nothing in this directory is transcribed from documentation.
- **`goose/` is a failure-path capture only.** No goose provider is configured on the capture
  machine, so goose got no happy-path, resume, concurrency or kill run. The one file present
  confirms the design's §7.1 exit-0-on-failure claim by execution; everything else about goose
  remains unverified.
- **No 429 was ever observed.** 65 concurrent Fireworks requests all returned 200, so the
  rate-limit row is Claude's `rate_limit_event` (real, in `claude/happy.jsonl`) and nothing else.
- The `claude/*.jsonl` captures contain two machine-specific artefacts: a ~7.6KB
  `system/hook_response` line carrying the capture machine's `SessionStart` hook text, and
  `/Users/david/...` paths in the `system/init` line. Both are the operator's environment, not part
  of Claude's event schema — a fresh capture on another machine will differ there and only there.
- `codex/config.toml` and `opencode/opencode.json` keep the capture machine's absolute
  `rhapsodyd` and `WORKFLOW.md` paths on purpose: they record what actually ran. Point them
  somewhere else before reusing them.
