---
# Hang WORKFLOW.md: the dedicated stall variant (minimal.md + a SHORT claude.turn_timeout_ms) so
# fake-claude-hang's never-ending turn is killed in ~3s and the run is recorded `failed`. NOTE: the
# plan names stall_timeout_ms, but the /proc-based stall detector does NOT fire on macOS (the
# capture host: "CPU-based liveness unavailable (no readable /proc); stall detection will not fire"
# — internal/agent/claude). turn_timeout_ms is a plain per-turn context deadline (runner.go), so it
# fires on any platform and yields the same outcome=failed for a hung agent. Drives the stalled-run
# fixtures only. Same three placeholders as minimal.md; capture.sh substitutes __CLAUDE_CMD__ with
# fake-claude-hang.
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
  turn_timeout_ms: 3000
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
