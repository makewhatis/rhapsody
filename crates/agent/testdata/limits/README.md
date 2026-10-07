# Claude limit captures

Real recorded lines; only `uuid` and `session_id` were removed. No utilization,
status, reset or credits field was edited. These are additive observation inputs,
not Go parity goldens.

| File | Source under `~/.rhapsody/logs/` | Line |
| --- | --- | --- |
| `allowed.jsonl` | `STUDIO-1117/20261006T181924.889749000Z-33.jsonl` | 6 |
| `warning.jsonl` | `pr_makewhatis_rhapsody_157_alice/20260912T172313.258041000Z-67.jsonl` | 6 |
| `rejected.jsonl` | `STUDIO-35/20261006T181222.083490000Z-1.jsonl` | 13586 |

OpenCode unit cases are source-derived boundary tests, not recorded limit fixtures:
v1.18.30 `packages/opencode/src/provider/error.ts` forwards `statusCode`,
`responseHeaders` and `responseBody`; `cli/cmd/run.ts` emits the full error as
`{type: "error", error: ...}`. No recorded ChatGPT 429 was found in the initial
measurement. No quota was deliberately consumed to reach a rejection.

`chatgpt-usage.json` is the sanitized, non-inference HTTP 200 response projection
measured on 2026-10-07 from `https://chatgpt.com/backend-api/wham/usage` using only
the operator's OpenAI access token (refresh blank). It is an HTTP measurement,
not a JSONL stream line: account identifiers and unrelated fields are omitted;
all included provider window/credits fields are unchanged. The successful minimal
OpenCode 1.18.30 run emitted `step_start`, `text`, `step_finish`, cost 0 and 968
tokens, with no account utilization in stdout or `--print-logs`. Detection is
therefore **probe**, with stream errors as the wall fallback.
