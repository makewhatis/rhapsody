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
| `claude/failure-unrecognized-model.exit` | Its observed exit status: **1**. |
| `claude/resume.jsonl` | `--resume <session_id>`, same flags, answering from the prior turn's context. |
| `claude/killtest.txt` | Item 5, executed: the daemon's kill against a claude turn running `./slow.sh`. |
| `claude/concurrency.txt` | Item 4, executed: two claude turns at once over the shared `~/.claude`. |
| `claude/concurrency-trials.txt` | The same, repeated: 4 paired trials, 8/8 turns completed. |
| `codex/happy.jsonl` | Same multi-tool turn, **with** `--dangerously-bypass-approvals-and-sandbox`. |
| `codex/mcp-refused-approval-policy.jsonl` | The same turn **without** that flag: both MCP calls refused, `turn.completed`, exit 0. |
| `codex/failure-401.jsonl` | Deliberate failure (bogus key): **6** non-terminal `error` events, then `turn.failed`. |
| `codex/failure-401.exit` | Its observed exit status: **1**. |
| `codex/resume.jsonl` | `codex exec resume <thread_id>`. |
| `codex/config.toml` | The `$CODEX_HOME/config.toml` that produced every codex capture. |
| `codex/killtest.txt` | Item 5, executed. |
| `codex/concurrency.txt` | Item 4, executed: two codex turns at once over one shared `$CODEX_HOME`. |
| `codex/concurrency-trials.txt` | The same, repeated: 4 paired trials, 8/8 turns completed. |
| `opencode/happy.jsonl` | Same multi-tool turn. |
| `opencode/long-turn.jsonl` | A longer turn: 11 tool calls over 4 steps, all correct. |
| `opencode/long-turn.timing` | Per-event arrival offsets and sizes for that turn, from `drive.py`. |
| `opencode/failure-401.jsonl` | Deliberate failure: one in-band `error` event carrying `statusCode` and `isRetryable`. |
| `opencode/failure-401.exit` | Its observed exit status: **1**. |
| `opencode/resume.jsonl` | `run -s <sessionID>`. |
| `opencode/opencode.json` | The project config that produced every opencode capture. |
| `opencode/killtest.txt` | Item 5, executed. |
| `opencode/concurrency.txt` | Item 4, executed: two opencode turns at once over the shared state dir — **one turn dies**. |
| `opencode/concurrency-trials-shared.txt` | 5 paired trials on the shared state dir: **6/10 turns completed, 4 lost to `database is locked`**. |
| `opencode/concurrency-trials-isolated-xdg.txt` | The same 5 trials with a private `XDG_DATA_HOME` per turn: **10/10**. |
| `goose/failure-401-exit0.stdout` | Goose failing a 401 while **exiting 0**, with the error on stdout. Failure path only — see below. |
| `goose/failure-401-exit0.exit` | Its observed exit status: **0**. The filename's claim, recorded rather than asserted. |
| `sandbox/` | The scripts and prompts that produced all of the above. |

`sandbox/mksandbox.sh` builds the sandbox repo (honours `$RHAPSODYD` and `$WORKFLOW`);
`sandbox/drive.py` mimics the daemon's turn loop (own process group, stream stdout, close stdin on
the terminal line, reap) and writes a `.timing` sidecar; `sandbox/killtest.py` reproduces the
daemon's exact kill (`process_group(0)` + `kill(-pid, SIGKILL)`) and reports surviving descendants;
`sandbox/conctest.sh` runs two turns of one harness at once against its shared global state dir and
checks each turn edited only its own sandbox; `sandbox/conctrials.sh` repeats that N times and
reports how many turns survived, which is the only honest way to measure an intermittent failure.

Every `killtest.txt` and `concurrency.txt` in this directory is the **verbatim stdout of those two
scripts**, timestamps and argv included — re-running the committed script reproduces the committed
transcript. Both carry their own elapsed times, which is why the kill and concurrency cases have no
separate `.timing` sidecar.

## What items 4 and 5 found

**Item 5, the kill path — all three harnesses escape their process group.** Each `killtest.txt`
spawns the harness exactly as the daemon does (`process_group(0)`, then `kill(-pid, SIGKILL)`),
waits 45s for the agent to get `./slow.sh` running, walks the ppid tree, kills, and re-checks:

| Harness | Descendants at kill | Survivors | What escaped |
|---|---|---|---|
| claude | 4 | **3** | the tool shell (`zsh` → `bash` → `sleep`), in its own pgid |
| codex | 4 | **2** | the tool shell (`bash` → `sleep`); its MCP server also left the group but died with the leader |
| opencode | 3 | **2** | the tool shell (`bash` → `sleep`), in its own pgid |

In every case the leader dies (`rc=-9`) and the command the agent was actually running does not.
This is [STUDIO-840](https://linear.app/studio49/issue/STUDIO-840) again one level down: that fix
put the *harness* in a killable group, and these transcripts show the harness then puts its own
tool children somewhere else. A stop still leaves real work running, whichever harness is in use.
Killing the leader's group is therefore not a containment boundary for any candidate.

**Item 4, concurrency — clean on claude and codex, BROKEN on opencode.**

| Harness | Shared state dir | Private state dir |
|---|---|---|
| claude | **8/8** turns completed (`claude/concurrency-trials.txt`) | not needed |
| codex | **8/8** turns completed (`codex/concurrency-trials.txt`) | not needed |
| opencode | **6/10** turns completed (`opencode/concurrency-trials-shared.txt`) | **10/10** (`opencode/concurrency-trials-isolated-xdg.txt`) |

⚠️ **opencode loses roughly one turn in three to `database is locked`** when two turns share the
default state dir. The losing turn dies in **under a second**, exits **1**, emits a completely
**empty event stream**, and writes `Error: Unexpected error` / `database is locked` to stderr —
`~/.local/share/opencode/opencode.db` is SQLite and the second turn to open it is refused outright.
It is a race, so a single pair often passes: that is why the evidence here is repeated trials
(`sandbox/conctrials.sh`) and not one run.

**Giving each turn a private `XDG_DATA_HOME` fixes it completely** — 10/10 across the same five
paired trials. The credentials have to be copied in with it (`auth.json` lives in that same
directory, so a bare redirect leaves the turn unauthenticated), and cold-start cost rises: isolated
turns took 18–50s against 12–19s shared. **An opencode adapter must isolate the state dir per run;
sharing it is not viable.**

Codex, by contrast, wrote one uniquely-named rollout per thread into a single shared `$CODEX_HOME`
with no collision, so redirecting `CODEX_HOME` is not required for *correctness* — only to stop two
runs sharing history. One codex turn took 128s against its partner's 19s in an earlier pair; that
is a single observation with no isolated cause, not a measured contention finding.

Every turn in every trial edited only its own `counter.txt` and reported a distinct session id.
**No cross-talk was ever observed on any harness** — opencode's failure is refusal to start, not
corruption of the other turn.

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
#   GOOSE_PROVIDER/GOOSE_MODEL are load-bearing. Without them a scratch HOME has no provider
#   configured, and goose exits *1* with "No provider configured" before ever reaching the API —
#   a different case entirely from the exit-0 one this file captures.
HOME=<scratch> OPENAI_API_KEY=sk-BOGUSKEY GOOSE_DISABLE_KEYRING=1 \
  GOOSE_PROVIDER=openai GOOSE_MODEL=gpt-4o-mini \
  goose run --no-session -t "Read NOTES.md and tell me the counter value."

# killtest.txt, per harness — the daemon's exact kill against a turn running ./slow.sh
sandbox/killtest.py <label> <sandbox> 45 <the harness argv above, prompt-slow-child.txt as the prompt>

# concurrency.txt, per harness — two turns at once over that harness's shared global state dir
PROMPT="$(cat sandbox/prompt-multitool.txt)" \
  sandbox/conctest.sh <label> <sandbox-a> <sandbox-b> -- <the harness argv above>
```

`opencode` must be the Homebrew build at `/opt/homebrew/Cellar/opencode/<v>/bin/opencode`. On the
capture machine `/opt/homebrew/bin/opencode` is a **broken npm-global symlink** whose postinstall
never ran; it exits 1 with an install error on stderr and runs nothing. A daemon resolving
`opencode` from `PATH` there would get the broken one.

## Reading these files honestly

- **Every byte here was executed.** Nothing in this directory is transcribed from documentation.
- **`goose/` is a failure-path capture only.** No goose provider with working credentials exists on
  the capture machine — the capture points goose at OpenAI with a deliberately bogus key — so goose
  got no happy-path, resume, concurrency or kill run. The one file present confirms the design's
  §7.1 exit-0-on-failure claim by execution; everything else about goose remains unverified.
- **No 429 was ever observed.** The rate-limit row is therefore Claude's `rate_limit_event` (real,
  in `claude/happy.jsonl`) and nothing else. The Fireworks side of that claim is a **[SURVEY]**
  leftover: an earlier ad-hoc burst of concurrent requests returned 200 across the board, but no
  transcript of it was kept, so it is not evidence and is not repeated here as a finding. What *is*
  committed is the two-turns-at-once case the ticket actually asked for, in each
  `concurrency.txt` and `concurrency-trials*.txt` — and none of those 36 turns saw a 429 either.
  opencode's concurrent failures are a local SQLite lock, not a provider rate limit.
- **A failing harness does not reliably say so in its exit code, and `claude` does not reliably say
  so in its stream.** The four `.exit` sidecars are the recorded spread: goose 401 → **0**,
  claude bad-model → 1, codex 401 → 1, opencode 401 → 1. Independently, `claude`'s terminal
  `result` line for the bad-model failure carries `"subtype":"success"` *and* `"is_error":true` in
  the same object, so `subtype` is not a verdict either.
- **The same prompt does not produce the same step grouping.** `opencode/long-turn.jsonl` and the
  original capture of the same prompt both make the same 11 tool calls in the same order-ish, but
  batched into 4 steps and 6 steps respectively. An adapter may key off tool calls; it must not
  assume a fixed number of `step_start`/`step_finish` pairs.
- **The three harnesses name the daemon's MCP tools three different ways** — claude
  `mcp__symphony__symphony_state`, opencode `symphony_symphony_state`, codex `symphony` +
  `symphony_state` as separate fields. Rhapsody's prompts name tools by their claude-side names, so
  the adapter owes a tool-name mapping, not only an event mapping.
- The `claude/*.jsonl` captures contain two machine-specific artefacts: a ~7.6KB
  `system/hook_response` line carrying the capture machine's `SessionStart` hook text, and
  `/Users/david/...` paths in the `system/init` line. Both are the operator's environment, not part
  of Claude's event schema — a fresh capture on another machine will differ there and only there.
- `codex/config.toml` and `opencode/opencode.json` keep the capture machine's absolute
  `rhapsodyd` and `WORKFLOW.md` paths on purpose: they record what actually ran. Point them
  somewhere else before reusing them.
