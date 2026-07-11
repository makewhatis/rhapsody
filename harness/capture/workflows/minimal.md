---
# Minimal WORKFLOW.md: the smallest config the daemon accepts (mirrors harness/workflows/smoke.md,
# the R3 boot recipe). Drives the success/api/schema/run fixtures AND the error run (capture.sh
# substitutes __CLAUDE_CMD__ with fake-claude or fake-claude-error). capture.sh sed-substitutes:
#   __STUB_PORT__   -> the port linear-stub printed as "LISTENING <port>"
#   __CLAUDE_CMD__  -> absolute path to a fake-claude* copied under $CAPTURE_HOME/bin (so the
#                      claude.command normalizes via the single <HOME> rule)
#   __STORE_PATH__  -> $CAPTURE_HOME/symphony.db (the DB capture.sh reads .schema from)
# No repo: is set, so each per-issue checkout is a plain mkdir workspace (no git, nothing pushed).
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
  command: __CLAUDE_CMD__
server:
  port: 0
storage:
  path: __STORE_PATH__
otel:
  enabled: false
mcp:
  enabled: false
---
Work {{ issue.identifier }}: {{ issue.title }}.
