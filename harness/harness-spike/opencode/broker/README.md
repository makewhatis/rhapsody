# Managed OpenCode broker request fixtures (STUDIO-995 / PB0)

The committed evidence behind the managed-OpenCode compatibility row and the closed request schema
in `~/.rhapsody/docs/provider-broker-design.md` §9.1 / §5.3. Every request here was **measured** —
captured from the real pinned `opencode` talking to the loopback fake provider in this directory —
not transcribed from documentation. No real provider, no paid key, and no credential file was used.

## Pinned compatibility row

| | |
|---|---|
| OpenCode | `1.18.30` (`anomalyco/opencode` tag `v1.18.30`, tag commit `3104c1428ec91f809e5ab86631300de41eb6952e`) |
| Bundled adapter | `@ai-sdk/openai-compatible` `2.0.41` |

`compatibility.json` is the single source of truth for the row; `capture.sh` copies it into every
fixture, and `crates/agent/tests/opencode_broker_fixture.rs` asserts it against the compiled
`rhapsody_agent::opencode::SUPPORTED` table. Adding a version means rerunning this capture and every
managed-control fixture (design §9.1).

## Files

| File | Role |
|---|---|
| `fake_provider.py` | The loopback `POST /v1/chat/completions` SSE server. Records only the request *shape* (method, path, selected headers, closed top-level body keys, `model`, `max_tokens`, `stream`, `stream_options`, `tool_choice`, tool names, message roles) — never the raw prompt, which is large, machine-specific, and full of absolute paths. Redacts the capability to `<CAPABILITY>`. |
| `capture.sh` | Operator-machine only (like `make fixtures`): drives the real `opencode` against `fake_provider.py` for each scenario and writes `requests/*.json`. Refuses if the binary is not the pinned version or if a request did not carry the managed capability. |
| `requests/*.json` | The committed snapshots: `happy`, `retry`, `auth`, `compaction`, `subagent`. |
| `probe.txt` | The exact stdout of the managed probe (`opencode --version`) under the probe's allow-listed, credential-free environment. |
| `compatibility.json` | The pinned row above. |

## Scenarios

| Fixture | What it pins |
|---|---|
| `happy` | Title-disabled first turn makes **exactly one** request; `POST /v1/chat/completions`, `Bearer <CAPABILITY>`, `model: probe-model`, `max_tokens: 32000`, SSE, `stream_options.include_usage`, `tool_choice: auto`, and the closed key set. `argv` carries the explicit `build` agent and `-m <PROVIDER>/probe-model`. |
| `retry` | A forwarded retry is a second provider request (500 then 200). |
| `auth` | A 401 is non-retryable, single-request, exit 1. |
| `compaction` | `--agent compaction` (the tool-less native compaction agent) stays on the generated `probe-model` and the closed schema minus the tool fields. |
| `subagent` | The `task` tool spawns a native subagent: its request, and the resumed main turn, stay on `probe-model` and the closed schema. |

## Recapturing

```sh
OPENCODE_BIN=/path/to/opencode-1.18.30 harness/harness-spike/opencode/broker/capture.sh
```

Determinism contract: two runs against the same binary must produce byte-identical `requests/*.json`
and `probe.txt`. The unpredictable per-session provider id, the capability, the ephemeral port, and
the scratch paths are all normalized to placeholders (`<PROVIDER>`, `<CAPABILITY>`, `<WORK>`,
`<PROMPT>`); nothing session- or machine-specific is written. CI never runs this script — it pins
the committed output through `cargo test -p rhapsody-agent --test opencode_broker_fixture`.
