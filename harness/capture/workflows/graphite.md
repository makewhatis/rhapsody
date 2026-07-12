---
# Graphite WORKFLOW.md: the documented validation PAIR dependency_mode: graphite + a non-empty
# review_states (the daemon rejects graphite with empty review_states at config load; see
# $REF/internal/config/validate.go validateReviewStatesForGraphite). review_promote_state must be
# one of active_states (validateReviewPromote). Only /api/v1/config is captured from this workflow.
# Same three placeholders as minimal.md.
tracker:
  kind: linear
  endpoint: http://127.0.0.1:__STUB_PORT__/graphql
  api_key: stub-key
  project_slug: 558008ab185c
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled]
  review_states: [In Review]
  review_promote_state: In Progress
  dependency_mode: graphite
  dep_mode_prompt_file: .rhapsody/PROMPT.dep_mod.md
git_flow: graphite
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
Work {{ issue.identifier }} on a Graphite stack: {{ issue.title }}.
