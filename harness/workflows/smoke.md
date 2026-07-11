---
# Template WORKFLOW.md for daemon-vs-stub smoke + capture runs (R3 Interfaces contract).
# The capture script / e2e `sed`-substitutes the three placeholders below:
#   __STUB_PORT__    -> the port linear-stub printed as "LISTENING <port>"
#   __FAKE_CLAUDE__  -> absolute path to harness/stubs/fake-claude
#   __STORE_PATH__   -> SQLite history-store path for this run
# Every key is validated against $REF/internal/config/config.go yaml tags. No `repo:` is set:
# an empty repo makes the daemon provision each per-issue checkout as a plain mkdir workspace
# (EnsureFromRepo -> createForIssue), so fake-claude runs with no git and nothing is pushed.
tracker:
  kind: linear
  endpoint: http://127.0.0.1:__STUB_PORT__/graphql
  api_key: stub-key
  project_slug: 558008ab185c
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
polling:
  interval_ms: 500
agent:
  backend: claude
  max_concurrent_agents: 1
claude:
  command: __FAKE_CLAUDE__
server:
  port: 0
storage:
  path: __STORE_PATH__
# Keep the smoke hermetic: no outbound telemetry export, no MCP server injected into the
# dispatched (fake) agent. Both are plain config.go keys; both default ON in production.
otel:
  enabled: false
mcp:
  enabled: false
---
Work the ticket {{ issue.identifier }}: {{ issue.title }}.
