---
# Full WORKFLOW.md: exercises the WIDE config surface so config/full.json anchors the parser's
# handling of every documented top-level knob (values chosen from $REF/WORKFLOW.example.md).
# Only /api/v1/config is captured from this workflow — it never needs to complete a run — so the
# pool claim_mode / mcp injection knobs below only have to DECODE, not dispatch. Same three
# placeholders as minimal.md (__STUB_PORT__, __CLAUDE_CMD__, __STORE_PATH__).
tracker:
  kind: linear
  endpoint: http://127.0.0.1:__STUB_PORT__/graphql
  api_key: stub-key
  project_slug: 558008ab185c
  active_states: [Todo, In Progress]
  terminal_states: [Done, Canceled, Cancelled, Duplicate]
  canceled_states: [Cancelled, Canceled, Duplicate]
  review_states: [In Review]
  review_promote_state: In Progress
  summon_token: "@symphony"
  github_summons: true
  milestone: "v2.0"
  labels: [ready, urgent]
  claim_mode: pool
  claim_ttl: 2m
  claim_settle_delay: 1s
polling:
  interval_ms: 500
agent:
  backend: claude
  max_concurrent_agents: 2
  max_turns: 10
  max_retry_backoff_ms: 120000
  max_concurrent_agents_by_state:
    todo: 1
    in progress: 2
claude:
  command: __CLAUDE_CMD__
  model: claude-opus-4-8
  effort: xhigh
  permission_mode: bypassPermissions
  turn_timeout_ms: 3600000
  read_timeout_ms: 5000
  stall_timeout_ms: 1200000
  billing_guard: true
  ultracode: true
hooks:
  after_create: "true"
  before_run: "true"
  after_run: "true"
  before_remove: "true"
  timeout_ms: 60000
workspace:
  root: ~/symphony_workspaces
server:
  port: 0
storage:
  path: __STORE_PATH__
  retention_days: 30
otel:
  enabled: false
mcp:
  enabled: true
  allow_send_message: true
  allow_stop: true
  allow_resume: true
---
You own {{ issue.identifier }} — {{ issue.title }}. Current state: {{ issue.state }}.
