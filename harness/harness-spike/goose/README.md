# goose captures — `goose acp` (ACP over stdio), STUDIO-872

The STUDIO-869 spike ran every harness but goose: no provider was configured on the capture
machine, so only goose's zero-cost failure path ran (`failure-401-exit0.stdout`, still here,
unchanged). The maintainer armed goose with a provider on 2026-09-12, and STUDIO-872 ran the four
things that were left: one real multi-tool turn, resume, concurrency, and the kill path.

**Everything here was driven over ACP**, not `goose run` — the design
(`~/.rhapsody/docs/pluggable-harnesses-design.md` §9 slice 9) picks ACP for goose precisely to
sidestep the stdout contamination and the exit-0 lie that `goose run` shows, so measuring
`goose run` would have measured a surface the adapter will never use.

The written findings live in `~/.rhapsody/docs/STUDIO-872-goose-acp-spike-findings.md`, not in
this repo.

## Versions and provider

**goose 1.49.0**, ACP `protocolVersion: 1`, provider **Fireworks**
`accounts/fireworks/models/deepseek-v4p1-flash` (the same provider and model STUDIO-869 used for
codex and opencode, so the numbers are comparable). MCP server: the live daemon binary,
`/Applications/Rhapsody.app/Contents/Resources/rhapsodyd mcp ~/.rhapsody/WORKFLOW.md`.

## Reading an ACP capture

ACP is bidirectional, so one capture is two files:

- `<name>.jsonl` — **the agent's stdout, byte for byte.** Every line is one JSON-RPC frame.
- `<name>.client.jsonl` — what the client (`sandbox/acp_drive.py`) wrote to the agent's stdin,
  byte for byte. Without it a capture cannot be replayed: the turn end is a *response* to the
  client's `session/prompt`, not a standalone event.

`*.driver.txt` is the driver's own stderr trace (ids, tool calls,
timings, exit code); `*.census.txt` is the kill test's process census.

## What is here

| Path | What it is |
|---|---|
| `acp-happy.jsonl` + `.client.jsonl` | The full multi-tool turn: shell read, typed `write`, shell `./check.sh`, the daemon MCP tool, final answer. 171 agent frames. |
| `acp-resume.jsonl` + `.client.jsonl` | `session/load` of the happy turn's session **in a fresh process**, then a follow-up answered from context ("8"). Lines 2–14 are the agent replaying history; the `session/load` response is line 15. |
| `acp-failure-401.jsonl` + `.client.jsonl` | A hard auth failure (bogus `FIREWORKS_API_KEY`): a JSON-RPC **error** on the `session/prompt` response. 7 frames, all valid JSON, stderr empty. |
| `acp-home-redirect-provider-not-set.jsonl` | `HOME` redirected to a scratch dir: goose isolates cleanly but loses the macOS keychain, and the turn fails with `"Provider not set"`. |
| `acp-client-delegation.jsonl` + `.client.jsonl` | The same turn with the client advertising `fs` **and** `terminal`. goose then asks the *client* to read files and to create terminals — and when the client refuses `terminal/create`, the model routes around it with a `delegate` subagent. 354 KB, 285 KB of which is 883 `agent_thought_chunk` frames. |
| `acp-concurrent-shared-{A,B}.driver.txt` | Two turns at once against the **shared** `~/.local/share/goose/sessions/sessions.db`. Distinct ids (`20260912_2`, `_3`), each sandbox edited correctly. |
| `acp-concurrent-isolated-{A,B}.driver.txt` | The same pair with per-run XDG state dirs. Both correct, the real store untouched — and **both got session id `20260912_1`**. |
| `acp-kill-{1,2,3}.census.txt` + `.driver.txt` | The kill path, `killtest.py`'s exact method over ACP, three runs. `0` survivors every time. |
| `acp-session-id-race.txt` | Six concurrent `session/new` calls against one store (no prompt, so no tokens): 6 distinct ids. |
| `failure-401-exit0.stdout` | **STUDIO-869's** capture, untouched: `goose run` exiting 0 on a 401 with the error on stdout. |

Full event streams were **not** committed for the four concurrency runs — `acp-happy.jsonl`
already shows the stream shape, and the concurrency finding rests on the ids, the per-sandbox
tool calls and the timings, which the driver logs carry. `.timing` files were not committed
either; they are derived from the two `.jsonl` files. No `.stderr` file is committed because every
run's stderr file came out **0 bytes** — an empty file is weaker evidence than saying so here. For
the runs that ended normally the driver also prints the count it measured (`STDERR_BYTES=0`) into
the `*.driver.txt`; the three kill runs have no such line, and their empty `.stderr` is **not**
evidence either way, because the driver kills the process before the stderr reader finishes.

## Provenance — the exact command per capture

Common to all: sandbox built by `sandbox/mksandbox.sh`, driven by `sandbox/acp_drive.py`, agent
argv `goose acp`, MCP server attached per-session through ACP's own `session/new` `mcpServers`
array (not a goose config file), prompt `sandbox/prompt-multitool.txt` unless stated.

```sh
SB=/tmp/goosespike
MCP="symphony=/Applications/Rhapsody.app/Contents/Resources/rhapsodyd,mcp,$HOME/.rhapsody/WORKFLOW.md"

# acp-happy — the multi-tool turn
sandbox/mksandbox.sh $SB/sb1
sandbox/acp_drive.py out/happy $SB/sb1 sandbox/prompt-multitool.txt --mcp "$MCP"

# acp-resume — a fresh process, session/load of the id the happy turn was given
sandbox/acp_drive.py out/resume $SB/sb1 sandbox/prompt-resume.txt --load 20260912_1 --mcp "$MCP"

# acp-failure-401 — isolated state dir so the env key wins over the keychain
XDG_CONFIG_HOME=$SB/xdgF/config XDG_DATA_HOME=$SB/xdgF/share XDG_STATE_HOME=$SB/xdgF/state \
  FIREWORKS_API_KEY=fw_BOGUSKEY \
  sandbox/acp_drive.py out/failure-401 $SB/sbX sandbox/prompt-multitool.txt --mcp "$MCP"

# acp-home-redirect-provider-not-set — HOME redirected, config.yaml copied in
HOME=$SB/homeA sandbox/acp_drive.py out/iso-A $SB/sbC sandbox/prompt-multitool.txt --mcp "$MCP"

# acp-client-delegation — client advertises fs + terminal
sandbox/acp_drive.py out/clientcaps $SB/sbT sandbox/prompt-multitool.txt \
  --client-fs --client-terminal --mcp "$MCP"

# acp-concurrent-shared-{A,B} — both at once, real (shared) state dir
sandbox/acp_drive.py out/conc-A $SB/sbA sandbox/prompt-multitool.txt --mcp "$MCP" &
sandbox/acp_drive.py out/conc-B $SB/sbB sandbox/prompt-multitool.txt --mcp "$MCP" &
wait

# acp-concurrent-isolated-{A,B} — both at once, one XDG state tree each
#   (config.yaml copied into $SB/xdg{A,B}/config/goose/ first; HOME left alone so the
#    keychain still resolves the provider key)
XDG_CONFIG_HOME=$SB/xdgA/config XDG_DATA_HOME=$SB/xdgA/share XDG_STATE_HOME=$SB/xdgA/state \
  sandbox/acp_drive.py out/xdg-A $SB/sbE sandbox/prompt-multitool.txt --mcp "$MCP" &
XDG_CONFIG_HOME=$SB/xdgB/config XDG_DATA_HOME=$SB/xdgB/share XDG_STATE_HOME=$SB/xdgB/state \
  sandbox/acp_drive.py out/xdg-B $SB/sbF sandbox/prompt-multitool.txt --mcp "$MCP" &
wait

# acp-kill-{1,2,3} — prompt-slow-child.txt needs slow.sh, which mksandbox.sh does not write
sandbox/mksandbox.sh $SB/sbK1 && sandbox/mkslow.sh $SB/sbK1
sandbox/acp_drive.py out/kill-1 $SB/sbK1 sandbox/prompt-slow-child.txt --mcp "$MCP" --kill-after 30

# acp-session-id-race — zero-cost: initialize + session/new only, no prompt
sandbox/idrace.py 6 $SB/sb1
```

## Reading these files honestly

- **Every byte here was executed on 2026-09-12.** Nothing is transcribed from documentation.
- **The exit code is useless either way.** `goose acp` exits **0** after a clean turn, after a
  JSON-RPC `Authentication required` error, and after `"Provider not set"`. The verdict is the
  `session/prompt` response, never the exit status.
- **The session ids are not UUIDs.** They are `YYYYMMDD_<ordinal>` allocated from whichever
  session store is in scope, so the ids in these captures are machine-, day- and
  store-specific — and two runs with *isolated* state dirs both legitimately get `_1`.
- `*.driver.txt` and the `cwd`/`path` fields carry the capture machine's absolute paths
  (`/tmp/goosespike/...`, `/Users/david/...`) on purpose: they record what actually ran.
- The kill captures list the MCP server as `ESCAPED` (its own pgid) and still report 0
  survivors — it dies when its stdio pipe to goose closes, not from the signal.
