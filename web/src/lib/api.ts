// API client + types mirroring the Plan 6 HTTP API (design §13.7.2).
// Field names are the spec's verbatim recommended names (incl. codex_totals,
// last_codex_event) for tooling interop.

export interface RunningSession {
  issue_id: string;
  issue_identifier: string;
  title: string;
  state: string;
  project: string; // Linear project slug ("" in single-project mode)
  repo: string; // project's git remote URL ("" in single-project mode)
  run_id: number; // durable run-row id (0 when persistence is disabled); opens RunDetailView
  turn_count: number;
  last_codex_event: string;
  started_at: string; // RFC3339
  last_event_at: string; // RFC3339
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
}

export interface RetryEntry {
  issue_identifier: string;
  attempt: number;
  due_at: string; // RFC3339
  error: string;
}

export interface CodexTotals {
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  seconds_running: number;
}

export interface RateLimit {
  type: string;
  resets_at: string; // RFC3339
  used_percent: number;
}

// BlockedEntry is one held dependent on the live state snapshot (INF-318 emits; INF-320 renders): a
// ticket the daemon is holding back because its blockedBy predecessor hasn't cleared the project's
// dependency_mode threshold yet. Only ever populated under graphite/dag — a disabled project never
// holds a ticket this way, so `blocked` stays empty there and the waiting indicator never shows.
export interface BlockedEntry {
  issue_identifier: string; // the held dependent
  title: string; // for the row's title cell (may be "")
  project: string; // Linear project slug, for agent/colour resolution
  blocker_identifier: string; // the predecessor it is waiting on
  blocker_state: string; // the predecessor's current state, e.g. "In Review"
  mode?: string; // graphite|dag — which threshold applies (never "disabled")
}

export interface StateResponse {
  status: "ok" | "degraded";
  poll_interval_ms: number;
  running: RunningSession[];
  retrying: RetryEntry[];
  codex_totals: CodexTotals;
  rate_limits: RateLimit[];
  // Held dependents waiting on an uncleared blockedBy predecessor (INF-318/INF-320). Empty for a
  // disabled project (the hold is part of the opt-in orchestration). Defensive-coalesced in fetchState.
  blocked: BlockedEntry[];
}

// IssueEvent is one entry in a running issue's activity timeline (oldest -> newest).
export interface IssueEvent {
  at: string; // RFC3339
  event: string; // Symphony event name (e.g. notification, turn_completed)
  message: string; // short single-line human summary
}

// RunDetail is the GET /api/v1/runs/{id} payload: one run rendered identically whether it is
// live or finished. `outcome` is the run's state ("running" while live, else a terminal
// outcome) and is the ONLY live-vs-finished difference; `live` reports whether it came from
// the in-memory snapshot. The live-only fields (issue_state, last_codex_event, last_event_at)
// are "" for a finished run; ended_at is "" while running.
export interface RunDetail {
  run_id: number;
  issue_id: string;
  issue_identifier: string;
  title: string;
  project: string; // Linear project slug ("" in single-project mode)
  repo: string; // project's git remote URL ("" in single-project mode)
  attempt: number;
  outcome: string; // running|continued|completed|stopped|failed|interrupted (taxonomy v2)
  live: boolean; // true when sourced from the live snapshot (still in flight)
  issue_state: string; // tracker state (live only; "" finished)
  last_codex_event: string; // live only
  turn_count: number;
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  // true when the token totals are a floored estimate (the run ended without a clean
  // `result` event) rather than an authoritative total.
  usage_estimated: boolean;
  started_at: string; // RFC3339
  ended_at: string; // RFC3339 ("" while running)
  last_event_at: string; // RFC3339 (live only)
  error: string;
  recent_events: IssueEvent[];
  generated_at: string;
}

// LogEntry is one humanized line of an agent session transcript (oldest -> newest), as served
// by GET /api/v1/runs/<id>/transcript. kind ∈ {"thinking","text","tool_use","tool_result","event"};
// tool is set only on tool_use entries.
export interface LogEntry {
  seq: number;
  kind: "thinking" | "text" | "tool_use" | "tool_result" | "event";
  tool: string;
  text: string;
}

// --- History API (design §7) ---

// RunSummary is the read-side projection of one run row (GET /api/v1/history,
// /issues/<id>/history). outcome ∈ running|continued|completed|stopped|failed|interrupted
// (taxonomy v2). started_at/ended_at are RFC3339 ("" when not yet ended).
export interface RunSummary {
  id: number;
  issue_id: string;
  issue_identifier: string;
  title: string;
  attempt: number;
  session_uuid: string;
  branch: string;
  project_slug: string; // Linear project slug ("" in single-project mode)
  repo: string; // project's git remote URL ("" in single-project mode)
  started_at: string; // RFC3339
  ended_at: string; // RFC3339 ("" while running)
  outcome: string;
  turns: number;
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  // true when the token totals are a floored estimate (the run ended without a clean
  // `result` event) rather than an authoritative total.
  usage_estimated: boolean;
  error: string;
  transcript_path: string;
}

// HistoryResponse is the GET /api/v1/history payload. next_offset is the offset to
// request the next page when a full page came back, and null when there is no next page.
export interface HistoryResponse {
  runs: RunSummary[];
  next_offset: number | null;
}

// IssueRunsResponse is the GET /api/v1/history/issues payload (TRA-320): one entry per ISSUE —
// that issue's LATEST run — paged by issue. The Jobs list reads this instead of grouping a
// run-paged fetch, so an issue in a retry loop occupies one row rather than filling the page and
// hiding every other issue. `next_offset` follows the same rule as HistoryResponse, counting issues.
export interface IssueRunsResponse {
  issues: RunSummary[];
  next_offset: number | null;
}

// DaySummary is the GET /api/v1/history/summary payload (TRA-320): whole-store totals over the runs
// that STARTED at or after `since`, computed in the daemon's SQL rather than folded over whatever
// page the client happens to hold. `total_tokens` is the cache-INCLUSIVE billed total, so the
// header's `cached = total − in − out` reconciliation still adds up. `rhythm` is the most recent
// runs' token totals, oldest→newest, for the sparkline.
export interface DaySummary {
  since: string;
  runs: number;
  completed: number;
  input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  seconds: number;
  rhythm: number[];
}

// IssueHistoryResponse is the GET /api/v1/issues/<id>/history payload.
export interface IssueHistoryResponse {
  issue_identifier: string;
  runs: RunSummary[];
}

// RunTranscriptResponse is the GET /api/v1/runs/<id>/transcript payload: the RICH humanized
// transcript for one historical run, mirroring the live /log response's entry shape (same
// LogEntry type) so the shared renderer is fed identical data.
export interface RunTranscriptResponse {
  run_id: number;
  entries: LogEntry[];
  generated_at: string;
}

// EventHit is one cross-run event-search result (GET /api/v1/events?q=): the event plus
// its owning run's identity.
export interface EventHit {
  run_id: number;
  issue_identifier: string;
  seq: number;
  at: string; // RFC3339
  kind: string;
  tool: string;
  text: string;
}

export interface EventSearchResponse {
  hits: EventHit[];
}

// DayRollup is one row of the per-day metrics aggregation (GET /api/v1/metrics?days=).
export interface DayRollup {
  date: string; // YYYY-MM-DD (UTC)
  runs: number;
  completed: number; // taxonomy v2 (was `succeeded`)
  failed: number;
  total_tokens: number;
}

export interface MetricsResponse {
  days: DayRollup[];
}

// HistoryFilter is the client-side filter passed to fetchHistory; empty fields are
// omitted from the query string (the server applies its own defaults).
export interface HistoryFilter {
  issue?: string;
  outcome?: string;
  project?: string; // Linear project slug; omitted from the query when empty
  since?: string; // RFC3339 lower bound on started_at
  limit?: number;
  offset?: number;
}

export interface ApiError {
  error: { code: string; message: string };
}

async function getJSON<T>(url: string): Promise<T> {
  const res = await fetch(url, { headers: { Accept: "application/json" } });
  if (!res.ok) {
    let code = `http_${res.status}`;
    let message = res.statusText;
    try {
      const body = (await res.json()) as ApiError;
      if (body?.error) {
        code = body.error.code;
        message = body.error.message;
      }
    } catch {
      /* non-JSON body */
    }
    throw new Error(`${code}: ${message}`);
  }
  return (await res.json()) as T;
}

export async function fetchState(): Promise<StateResponse> {
  const s = await getJSON<StateResponse>("/api/v1/state");
  // Defensive: tolerate a server that sends null (or omits) the list fields, so
  // components can call .length/.map unconditionally without white-screening.
  s.running ??= [];
  s.retrying ??= [];
  s.rate_limits ??= [];
  s.blocked ??= [];
  return s;
}

// fetchRunDetail fetches one run's unified detail by run id (GET /api/v1/runs/{id}). It
// serves a running run (live snapshot) and a finished run (history store) identically — the
// caller polls while outcome === "running" and goes static once it terminates.
// DaemonVersion is the payload of GET /api/v1/version: which commit the DAEMON was built from
// (STUDIO-380). Distinct from the desktop shell's VersionDTO — the shell and the rhapsodyd sidecar
// are separate binaries that can drift apart, and it is the daemon's build that decides how runs are
// classified. Every field is always present; an unstamped build reports "unknown".
export interface DaemonVersion {
  version: string; // nearest release tag + distance, e.g. "v0.3.1" or "v0.3.1-8-g581e281"
  commit: string; // full git SHA, or "unknown"
  built_at: string; // RFC3339 UTC, or "unknown"
  // Whether Rhapsody Teams is on (STUDIO-652). THE gate: the app reads it from this one mount-time
  // request and, when it is false, never touches /api/v1/teams* at all — no chip, no panel, no
  // fetches. Optional because a daemon older than STUDIO-652 omits it, which reads as off.
  teams_enabled?: boolean;
}

// fetchVersion reads the daemon's build identity. Unlike the shell's appVersion() this works in a
// plain browser, since it is served over the same loopback API as everything else.
export async function fetchVersion(): Promise<DaemonVersion> {
  return getJSON<DaemonVersion>("/api/v1/version");
}

export async function fetchRunDetail(runID: number): Promise<RunDetail> {
  const d = await getJSON<RunDetail>(`/api/v1/runs/${runID}`);
  // Defensive: tolerate a server that omits/nulls recent_events so the timeline can .map().
  d.recent_events ??= [];
  return d;
}

export async function postRefresh(): Promise<void> {
  const res = await fetch("/api/v1/refresh", { method: "POST" });
  if (!res.ok && res.status !== 202) {
    throw new Error(`refresh failed: ${res.status}`);
  }
}

// RunActionResult is the JSON payload of POST /api/v1/runs/{id}/stop|resume: the human ticket
// identifier, the state the ticket was moved to (Backlog on stop, Todo on resume; "" when the
// move failed) and, when the agent was killed but the move failed, the move error.
export interface RunActionResult {
  identifier: string;
  moved_to?: string;
  move_error?: string;
}

// postRunAction POSTs a run-action endpoint and parses the daemon's error envelope on failure
// (so the UI can toast a precise message, e.g. a Backlog/Todo move failure).
async function postRunAction(runID: number, action: "stop" | "resume"): Promise<RunActionResult> {
  const res = await fetch(`/api/v1/runs/${runID}/${action}`, {
    method: "POST",
    headers: { Accept: "application/json" },
  });
  const body = (await res.json().catch(() => null)) as RunActionResult | ApiError | null;
  if (!res.ok) {
    const message = body && "error" in body ? body.error.message : `${action} failed: ${res.status}`;
    throw new Error(message);
  }
  return (body ?? { identifier: "" }) as RunActionResult;
}

export function stopRun(runID: number): Promise<RunActionResult> {
  return postRunAction(runID, "stop");
}
export function resumeRun(runID: number): Promise<RunActionResult> {
  return postRunAction(runID, "resume");
}

// RunMessage is one operator "btw" sent to a run's agent (INF-250). body is the operator's
// original text; status moves sent → delivered (delivered_turn set) | expired (run ended first).
export interface RunMessage {
  id: number;
  run_id: number;
  body: string;
  created_at_ms: number;
  status: "sent" | "delivered" | "expired";
  delivered_turn?: number;
}

// sendRunMessage POSTs an operator message to a live run's agent (INF-250). It parses the daemon's
// error envelope on failure so the composer can surface a precise message (not_running /
// backlog_full → 409, empty_text / text_too_long → 400).
export async function sendRunMessage(
  runID: number,
  text: string,
): Promise<{ id: number; identifier: string; status: string }> {
  const res = await fetch(`/api/v1/runs/${runID}/message`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({ text }),
  });
  const body = (await res.json().catch(() => null)) as
    | { id: number; identifier: string; status: string }
    | ApiError
    | null;
  if (!res.ok) {
    const message =
      body && "error" in body ? body.error.message : `send message failed: ${res.status}`;
    throw new Error(message);
  }
  return (body ?? { id: 0, identifier: "", status: "sent" }) as {
    id: number;
    identifier: string;
    status: string;
  };
}

// fetchRunMessages lists a run's operator messages with their delivery status (INF-250). Tolerates
// a null body so the timeline can .map() safely.
export async function fetchRunMessages(runID: number): Promise<RunMessage[]> {
  const msgs = await getJSON<RunMessage[] | null>(`/api/v1/runs/${runID}/messages`);
  return msgs ?? [];
}

// historyQuery builds the /api/v1/history query string from a filter, omitting empty
// fields so the server applies its own defaults. Exported for unit testing.
export function historyQuery(f: HistoryFilter): string {
  const p = new URLSearchParams();
  if (f.issue) p.set("issue", f.issue);
  if (f.outcome) p.set("outcome", f.outcome);
  if (f.project) p.set("project", f.project);
  if (f.since) p.set("since", f.since);
  if (f.limit != null) p.set("limit", String(f.limit));
  if (f.offset != null && f.offset > 0) p.set("offset", String(f.offset));
  const qs = p.toString();
  return qs ? `?${qs}` : "";
}

export async function fetchHistory(f: HistoryFilter): Promise<HistoryResponse> {
  const h = await getJSON<HistoryResponse>(`/api/v1/history${historyQuery(f)}`);
  // Defensive: tolerate a server that omits/nulls runs so the table can .map() safely.
  h.runs ??= [];
  h.next_offset ??= null;
  return h;
}

// fetchIssueRuns lists ONE run per issue (each issue's latest), paged by issue — the Jobs list's
// query. Reuses historyQuery: /history/issues takes exactly the same filters as /history and differs
// only in what a page counts. (TRA-320)
export async function fetchIssueRuns(f: HistoryFilter): Promise<IssueRunsResponse> {
  const r = await getJSON<IssueRunsResponse>(`/api/v1/history/issues${historyQuery(f)}`);
  // Defensive: tolerate a server that omits/nulls issues so the table can .map() safely.
  r.issues ??= [];
  r.next_offset ??= null;
  return r;
}

// localDayStartISO renders the start of the LOCAL calendar day containing `nowMs` as a whole-second
// RFC3339 UTC instant — the `since` the dashboard sends to /history/summary. Local, not UTC: the
// header cells have always counted a local day, and moving the sum into the daemon must not
// silently shift that boundary for anyone off UTC. Seconds precision (no milliseconds) because the
// daemon compares `since` against RFC3339 timestamps stored at seconds precision.
export function localDayStartISO(nowMs: number): string {
  const d = new Date(nowMs);
  d.setHours(0, 0, 0, 0);
  return `${d.toISOString().slice(0, 19)}Z`;
}

// fetchDaySummary reads the daemon-computed totals for the day containing `nowMs`. These are a SQL
// aggregate over every run in the window, so they are identical no matter how much history the
// client has fetched — the whole point of TRA-320's Defect 2.
export async function fetchDaySummary(nowMs: number): Promise<DaySummary> {
  const since = localDayStartISO(nowMs);
  const s = await getJSON<DaySummary>(
    `/api/v1/history/summary?since=${encodeURIComponent(since)}`,
  );
  s.rhythm ??= [];
  return s;
}

export async function fetchIssueHistory(identifier: string): Promise<IssueHistoryResponse> {
  const h = await getJSON<IssueHistoryResponse>(
    `/api/v1/issues/${encodeURIComponent(identifier)}/history`,
  );
  h.runs ??= [];
  return h;
}

export async function fetchRunTranscript(runID: number): Promise<RunTranscriptResponse> {
  const r = await getJSON<RunTranscriptResponse>(`/api/v1/runs/${runID}/transcript`);
  // Defensive: tolerate a server that omits/nulls entries so the pane can .map() safely.
  r.entries ??= [];
  return r;
}

export async function searchEvents(q: string, limit = 100): Promise<EventSearchResponse> {
  const p = new URLSearchParams();
  if (q) p.set("q", q);
  if (limit > 0) p.set("limit", String(limit));
  const r = await getJSON<EventSearchResponse>(`/api/v1/events?${p.toString()}`);
  r.hits ??= [];
  return r;
}

export async function fetchMetrics(days = 30): Promise<MetricsResponse> {
  const r = await getJSON<MetricsResponse>(`/api/v1/metrics?days=${days}`);
  r.days ??= [];
  return r;
}

// --- Config API (INF-220, daemon change §2) ---

// ConfigResponse is GET/POST /api/v1/config: the on-disk WORKFLOW.md split into its
// front-matter map (`config`, edited by the Settings form) and the Liquid prompt body, both
// VERBATIM (pre $VAR resolution, so the api_key indirection is preserved — no secret leaks).
export interface ConfigResponse {
  config: Record<string, unknown>;
  prompt_body: string;
  generated_at?: string;
}

// ConfigRequest is the POST body that rewrites WORKFLOW.md.
export interface ConfigRequest {
  config: Record<string, unknown>;
  prompt_body: string;
}

export async function fetchConfig(): Promise<ConfigResponse> {
  const c = await getJSON<ConfigResponse>("/api/v1/config");
  c.config ??= {};
  c.prompt_body ??= "";
  return c;
}

// saveConfig POSTs the edited config. On a validation failure the daemon returns 400 with an
// error envelope ({error:{code,message}}); surface that message verbatim so the form can show
// exactly why the daemon rejected the change (e.g. "review_promote_state not in active_states").
export async function saveConfig(req: ConfigRequest): Promise<ConfigResponse> {
  const res = await fetch("/api/v1/config", {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(req),
  });
  if (!res.ok) {
    let message = res.statusText;
    try {
      const body = (await res.json()) as ApiError;
      if (body?.error) {
        message = body.error.message;
      }
    } catch {
      /* non-JSON body */
    }
    throw new Error(message);
  }
  return (await res.json()) as ConfigResponse;
}

// --- Typed multi-agent config API (INF-224, consumed by the Settings UI / INF-226) ---
//
// GET /api/v1/config also returns a typed view alongside the legacy `config`/`prompt_body`
// map: `global` (the defaults every agent inherits) + `projects` (one entry per agent, each
// watching one or more Linear project slugs). `overrides` is a SPARSE presence-map — an absent
// key means "inherit the global default", a present key means "override". `effective` is a
// display-only resolution computed by the daemon and is IGNORED on POST. POST with `global`
// present takes the typed path: the submitted global + projects are patched onto the on-disk
// config, re-validated, and atomically rewritten (secrets like the api_key are preserved).

export interface GlobalTrackerDTO {
  kind: string;
  endpoint: string;
  /** Whether a Linear api_key resolves on disk. The secret itself is never serialized. */
  api_key_set: boolean;
}

export interface GlobalAgentDTO {
  backend: string;
  max_concurrent_agents: number;
  max_turns: number;
  max_retry_backoff_ms: number;
  max_concurrent_agents_by_state?: Record<string, number>;
}

export interface GlobalClaudeDTO {
  command: string;
  model: string;
  effort: string;
  permission_mode: string;
  billing_guard: boolean;
  ultracode: boolean;
  turn_timeout_ms: number;
  read_timeout_ms: number;
  stall_timeout_ms: number;
  mcp_config: string;
  extra_args?: string[];
}

export interface GlobalConfigDTO {
  tracker: GlobalTrackerDTO;
  polling: { interval_ms: number };
  agent: GlobalAgentDTO;
  claude: GlobalClaudeDTO;
  workspace: { root: string };
  storage: { path: string; retention_days: number | null };
  otel: {
    enabled: boolean;
    endpoint: string;
    protocol: string;
    service_name: string;
    insecure: boolean;
    headers?: Record<string, string>;
  };
  // `symphony mcp` local-facade toggles (INF-473). Read tools are always on and not
  // represented here; these gate dispatch-time injection and the opt-in write tools.
  mcp: {
    enabled: boolean;
    allow_send_message: boolean;
    allow_stop: boolean;
    allow_resume: boolean;
  };
  server: { port: number | null };
  logging: { dir: string };
  repo: string;
  active_states: string[];
  terminal_states: string[];
  canceled_states: string[];
  review_states: string[] | null;
  review_promote_state: string;
  summon_token: string;
  /** tracker.github_summons: re-engage an In-Review ticket from an @symphony comment on its
   *  unmerged linked GitHub PR. Tracker-global (no per-project override). Default false. */
  github_summons: boolean;
  milestone: string;
  labels: string[];
  /** Global agent-capabilities default (tracker.capabilities): the extra practices every agent
   *  inherits. Mirrors the `labels` plumbing (BO-10/BO-13); surfaced as an opt-in checklist. */
  capabilities: string[];
  prompt: string;
  /** Global prompt-source-file path. Empty => use the inline `prompt`; when set it WINS (read per-run). */
  prompt_file: string;
  /** Global git-workflow policy: "" (== "any", no enforcement) or "graphite". Per-agent overrides
   *  live in ProjectConfigDTO.overrides.git_flow (INF-251). */
  git_flow: string;
  /** Global workspace-provisioning policy: "" (== "worktree", today's shared-mirror behavior) or
   *  "clone" (an independent git clone per dispatch, no cross-ticket checkout lock). Per-agent
   *  overrides live in ProjectConfigDTO.overrides.workspace_mode (INF-418). */
  workspace_mode: string;
  /** Global dependency-sequencing mode: the three-valued enum "disabled"|"graphite"|"dag", default
   *  "disabled" (orchestration is opt-in; NOT derived from git_flow). Per-agent overrides live in
   *  ProjectConfigDTO.overrides.dependency_mode (INF-318/INF-320). */
  dependency_mode: string;
  /** Global ticket-claim policy: "assignee" (default — fetch assignee==me, no election) or "pool"
   *  (fetch unassigned pool tickets and run the single-claimant claim protocol). Per-agent overrides
   *  live in ProjectConfigDTO.overrides.claim_mode (INF-477). */
  claim_mode: string;
}

// ClaudeOverridesDTO is the sparse per-agent override map. A field is present (non-null) only
// when the agent overrides that global default; absent/null means inherit.
export interface ClaudeOverridesDTO {
  model?: string | null;
  effort?: string | null;
  permission?: string | null;
  ultracode?: boolean | null;
  turn_timeout_ms?: number | null;
  stall_timeout_ms?: number | null;
  billing_guard?: boolean | null;
  command?: string | null;
  /** Per-agent git_flow override (absent/null => inherit the global). Surfaced in the overrides
   *  block but maps to the top-level Project.GitFlow config field (INF-251). */
  git_flow?: string | null;
  /** Per-agent workspace_mode override (absent/null => inherit the global). Surfaced in the
   *  overrides block but maps to the top-level Project.WorkspaceMode config field (INF-418). */
  workspace_mode?: string | null;
  /** Per-agent dependency_mode override (absent/null => inherit the global). Surfaced in the overrides
   *  block but maps to the top-level Project.DependencyMode config field (INF-318/INF-320). */
  dependency_mode?: string | null;
  /** Per-agent claim_mode override (absent/null => inherit the global). Surfaced in the overrides
   *  block but maps to the top-level Project.ClaimMode config field (INF-477). */
  claim_mode?: string | null;
}

// EffectiveConfigDTO is the daemon's display-only resolution of an agent's inherited + overridden
// knobs. It is returned on GET and IGNORED on POST.
export interface EffectiveConfigDTO {
  name: string;
  repo: string;
  model: string;
  effort: string;
  permission: string;
  ultracode: boolean;
  turn_timeout_ms: number;
  stall_timeout_ms: number;
  active_states: string[];
  terminal_states: string[];
  canceled_states: string[];
  review_states: string[] | null;
  review_promote_state: string;
  max_concurrent_agents: number;
  milestone: string;
  labels: string[];
  /** Resolved agent-capabilities (the agent's own list, else the inherited global). Display-only. */
  capabilities: string[];
  prompt: string;
  /** Resolved prompt-source-file path (the agent's override, else the inherited global). */
  prompt_file: string;
  /** Resolved git-workflow policy (the agent's override, else the inherited global). */
  git_flow: string;
  /** Resolved workspace-provisioning policy (the agent's override, else the inherited global);
   *  display-only ("worktree" | "clone", default "worktree"). (INF-418) */
  workspace_mode: string;
  /** Resolved dependency-sequencing mode (the agent's override, else the inherited global);
   *  display-only (the three-valued enum, default "disabled"). (INF-318/INF-320) */
  dependency_mode: string;
  /** Resolved ticket-claim policy (the agent's override, else the inherited global); display-only
   *  ("assignee" | "pool", default "assignee"). (INF-477) */
  claim_mode: string;
  enabled: boolean;
}

export interface ProjectConfigDTO {
  name: string;
  slugs: string[];
  repo?: string;
  milestone?: string;
  labels?: string[] | null;
  /** Per-agent capabilities override (absent/null => inherit the global `capabilities`). */
  capabilities?: string[] | null;
  enabled: boolean | null;
  active_states?: string[] | null;
  terminal_states?: string[] | null;
  canceled_states?: string[] | null;
  review_states?: string[] | null;
  max_concurrent_agents?: number | null;
  prompt?: string;
  /** Per-agent prompt-source-file override. Empty/absent => inherit the global `prompt_file`. */
  prompt_file?: string;
  overrides: ClaudeOverridesDTO;
  effective?: EffectiveConfigDTO;
  /** Display-only hint (emit-only; ignored on POST): true when this project's effective
   *  dependency_mode is graphite/dag AND workspace_mode is unset, so the UI RECOMMENDS (never forces)
   *  clone for the stacking project. Never a stored override (INF-418). */
  workspace_mode_recommended?: boolean;
}

// TypedConfigResponse is the GET /api/v1/config payload with the typed multi-agent view present
// (`global` + `projects` are omitted when the on-disk config fails to parse).
export interface TypedConfigResponse extends ConfigResponse {
  global?: GlobalConfigDTO;
  projects?: ProjectConfigDTO[];
}

// FieldError is one structured validation error from a typed POST, anchored to a config path
// (e.g. "review_promote_state") so the form can attach the message inline.
export interface FieldError {
  path: string;
  message: string;
}

export interface TypedApiError {
  error: { code: string; message: string; fields?: FieldError[] };
}

// ConfigSaveError carries the daemon's stable error code + structured field errors so the
// Settings form can surface a validation failure on the offending control.
export class ConfigSaveError extends Error {
  code: string;
  fields: FieldError[];
  constructor(message: string, code: string, fields: FieldError[]) {
    super(message);
    this.name = "ConfigSaveError";
    this.code = code;
    this.fields = fields;
  }
}

export async function fetchTypedConfig(): Promise<TypedConfigResponse> {
  const c = await getJSON<TypedConfigResponse>("/api/v1/config");
  c.config ??= {};
  c.prompt_body ??= "";
  return c;
}

// saveTypedConfig POSTs the typed multi-agent view. `effective` is stripped from each project
// (it is display-only); the daemon patches `global` + `projects` onto the on-disk config and
// re-validates. A 400 surfaces as a ConfigSaveError carrying the code + structured fields.
export async function saveTypedConfig(
  global: GlobalConfigDTO,
  projects: ProjectConfigDTO[],
): Promise<TypedConfigResponse> {
  const body = {
    global,
    projects: projects.map(({ effective: _effective, ...p }) => p),
  };
  const res = await fetch("/api/v1/config", {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    let message = res.statusText;
    let code = `http_${res.status}`;
    let fields: FieldError[] = [];
    try {
      const parsed = (await res.json()) as TypedApiError;
      if (parsed?.error) {
        message = parsed.error.message;
        code = parsed.error.code;
        fields = parsed.error.fields ?? [];
      }
    } catch {
      /* non-JSON body */
    }
    throw new ConfigSaveError(message, code, fields);
  }
  const saved = (await res.json()) as TypedConfigResponse;
  saved.config ??= {};
  saved.prompt_body ??= "";
  return saved;
}

// --- Linear identity + project listing (INF-224) ---

// LinearIdentity is GET /api/v1/linear/identity: the connected-as account. `token` is already
// masked by the daemon (e.g. "lin_api_••••3f2a"); the raw key is never exposed.
export interface LinearIdentity {
  connected: boolean;
  name: string;
  display_name: string;
  email: string;
  token: string;
  /** The workspace's Linear slug (organization.urlKey) for building issue deep links. */
  workspace_url_key: string;
}

export async function fetchLinearIdentity(): Promise<LinearIdentity> {
  return getJSON<LinearIdentity>("/api/v1/linear/identity");
}

// LinearProject is one row of GET /api/v1/linear/projects: the workspace's Linear projects for
// the Add-agent picker (and the per-agent team-colour swatch).
export interface LinearProject {
  id: string;
  name: string;
  slug: string;
  team: string;
  color: string;
}

export async function fetchLinearProjects(): Promise<LinearProject[]> {
  const r = await getJSON<{ projects?: LinearProject[] }>("/api/v1/linear/projects");
  return r.projects ?? [];
}

// ProjectStatus is one row of GET /api/v1/projects: an agent's live run status (status + the
// number of in-flight runs), keyed by Linear project slug.
export interface ProjectStatus {
  slug: string;
  name: string;
  status: string;
  running: number;
}

export async function fetchProjectStatuses(): Promise<ProjectStatus[]> {
  const r = await getJSON<{ projects?: ProjectStatus[] }>("/api/v1/projects");
  return r.projects ?? [];
}

// --- Agent-capabilities registry (BO-11/BO-13; Rhapsody-only, no Go v0.4.0 mirror) ---

// CapabilityDefDTO is one row of GET /api/v1/capabilities: an opt-in practice the per-project
// config screen renders as a checklist. The daemon also serves an `instruction` field (the prompt
// text it injects); the UI only needs the human-facing name/label/description, so we omit it.
export interface CapabilityDefDTO {
  name: string;
  label: string;
  description: string;
}

// fetchCapabilitiesRegistry lists the daemon's capability registry so the config UI can render the
// checklist without hardcoding options. Serves [] when no registry is loaded yet.
export async function fetchCapabilitiesRegistry(): Promise<CapabilityDefDTO[]> {
  return getJSON<CapabilityDefDTO[]>("/api/v1/capabilities");
}

// --- Rhapsody Teams (STUDIO-652; Rhapsody-only, no Go v0.4.0 mirror) ---
//
// Every route below is additive and answers `teams_disabled` (409) on a daemon with Teams off, so
// nothing here may be fetched speculatively. The gate is `DaemonVersion.teams_enabled`, read from
// the version request the app already makes — once it answers, and no more — see `useTeamsEnabled`.

// TeamsRosterRow is one identity as the daemon reports it: the configured record plus the status
// derived from the runs live as that identity RIGHT NOW.
export interface TeamsRosterRow {
  name: string;
  profile: string;
  labels: string[];
  /** The memory bank id the store actually uses (`<bank_prefix><name>` unless overridden). */
  bank: string;
  /** 0 ⇒ unlimited. */
  max_concurrent: number;
  live_runs: number;
  /** Which tickets those runs are working, sorted by the daemon for a stable response. */
  tickets: string[];
}

// TeamsOverview is GET /api/v1/teams: the roster plus the two settings that make it legible —
// how tickets are assigned (`manager_mode`) and whether anything is remembered (`backend`).
export interface TeamsOverview {
  enabled: boolean;
  /** "off" | "labels" | "labels+model" — the teams.yaml wire spelling. */
  manager_mode: string;
  /** Who takes a ticket nothing matched; "" ⇒ run without an identity. */
  default_identity: string;
  /** "none" | "local" | "hindsight". */
  backend: string;
  roster: TeamsRosterRow[];
}

// TeamsRoomMessage is one post in the team room. `from` is HOST-stamped (design §0.11.4): a run
// cannot supply it. `body` is untrusted content and is rendered quoted, never as instructions.
export interface TeamsRoomMessage {
  /** `file:seq` — stable across reads. */
  id: string;
  from: string;
  /** "*" for a room-wide post, else the addressed identity. */
  to: string;
  at: string; // RFC3339
  body: string;
  /** Ticket ids, PR urls, commit SHAs — what proves it. */
  refs: string[];
}

// TeamsRoomPost is the daemon's echo of a post it just appended: what was written, and who it was
// written as. `from` is always "operator" here — the daemon stamps it, and there is no request
// field that can change that (design §0.11.4).
export interface TeamsRoomPost {
  /** `file:seq` — the same id a later room read serves for this message. */
  id: string;
  /** Always "operator": the reserved name the daemon stamps on a human post. */
  from: string;
  /** Always "*": v1 is room-wide only. */
  to: string;
  at: string; // RFC3339, rendered exactly as the log stored it
  refs: string[];
  /** Always 0 for a room post — the room is a log, not a delivery bus. */
  delivered: number;
}

export interface TeamsRoomResponse {
  /** Oldest first, bounded by the room's own window. */
  messages: TeamsRoomMessage[];
  /** Log lines that could not be parsed — reported rather than hidden. */
  skipped: string[];
}

// TeamsFact is one record in an identity's memory bank. Untrusted content, same as a room post.
export interface TeamsFact {
  id: string;
  identity: string;
  document_id: string;
  ticket: string;
  commit_sha: string;
  pr: string;
  run_id: string;
  at: string;
  /** "valid" | "invalidated"; recall only ever returns valid records. */
  state: string;
  reason: string;
  content: string;
}

export interface TeamsRecallResponse {
  identity: string;
  facts: TeamsFact[];
  /** Bank files that could not be read — reported rather than hidden. */
  skipped: string[];
}

export interface TeamsInvalidateResponse {
  identity: string;
  fact_id: string;
  /** false ⇒ the record was already invalidated (a no-op, not a failure). */
  invalidated: boolean;
  reason: string;
}

// --- teams.yaml, for the enable flow ---

export interface TeamsIdentityConfig {
  name: string;
  profile: string;
  labels: string[];
  bank: string;
  max_concurrent: number;
}

export interface TeamsManagerConfig {
  mode: string;
  default_identity: string;
  model: string;
  max_tokens: number;
  timeout_ms: number;
}

export interface TeamsMemoryConfig {
  backend: string;
  path: string;
  endpoint: string;
  bank_prefix: string;
  recall_top_k: number;
}

// TeamsConfig mirrors `~/.rhapsody/teams.yaml` field for field (design §2.2). The daemon applies
// every schema default on read, so a partial POST is legal — an omitted key means "the default",
// not "empty".
export interface TeamsConfig {
  enabled: boolean;
  manager: TeamsManagerConfig;
  memory: TeamsMemoryConfig;
  roster: TeamsIdentityConfig[];
  prompt_budget_bytes: number;
}

// TeamsConfigView is GET/POST /api/v1/teams/config. `present: false` is the SHIPPED state: an
// absent teams.yaml means Teams is off, and nothing — including reading this — ever creates it.
export interface TeamsConfigView {
  path: string;
  present: boolean;
  /** Why a PRESENT file did not load, verbatim from the daemon's loader; "" when it did. */
  error: string;
  config: TeamsConfig;
  /** teams.yaml is boot-loaded (no watcher), so a save takes effect on the next daemon start. */
  restart_required: boolean;
}

export async function fetchTeamsOverview(): Promise<TeamsOverview> {
  const t = await getJSON<TeamsOverview>("/api/v1/teams");
  t.roster ??= [];
  return t;
}

// fetchTeamsRoom reads the newest posts in the room. `limit` can only NARROW — the daemon clamps
// it to the room's own ceiling — and reading advances no identity's cursor, so the panel can poll
// without ever eating a teammate's catch-up.
export async function fetchTeamsRoom(limit?: number): Promise<TeamsRoomResponse> {
  const q = limit && limit > 0 ? `?limit=${limit}` : "";
  const r = await getJSON<TeamsRoomResponse>(`/api/v1/teams/room${q}`);
  r.messages ??= [];
  r.skipped ??= [];
  return r;
}

// fetchTeamsRecall lists what an identity remembers. An EMPTY query is a browse — "everything,
// bounded by recall_top_k" — which is what the memory panel wants: a wrong fact has to be visible
// before it can be invalidated (design §5.2.3).
export async function fetchTeamsRecall(identity: string, query = ""): Promise<TeamsRecallResponse> {
  const params = new URLSearchParams({ identity, query });
  const r = await getJSON<TeamsRecallResponse>(`/api/v1/teams/recall?${params}`);
  r.facts ??= [];
  r.skipped ??= [];
  return r;
}

// postJSON POSTs `body` and surfaces the daemon's error envelope verbatim — the Teams write paths
// (invalidate, save teams.yaml) both want the daemon's own complaint on screen rather than a
// paraphrase, because the daemon's answer is the one that decides what happens.
async function postJSON<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    let message = res.statusText;
    try {
      const parsed = (await res.json()) as ApiError;
      if (parsed?.error) message = parsed.error.message;
    } catch {
      /* non-JSON body */
    }
    throw new Error(message);
  }
  return (await res.json()) as T;
}

// postTeamsInvalidate marks one record non-valid WITH its reason (design §5.3). Nothing is
// deleted and the reason is stored, so the correction is readable by whoever finds it later; the
// daemon rejects a reasonless invalidate, which is why the UI requires one too.
export async function postTeamsInvalidate(
  identity: string,
  factID: string,
  reason: string,
): Promise<TeamsInvalidateResponse> {
  return postJSON<TeamsInvalidateResponse>("/api/v1/teams/invalidate", {
    identity,
    fact_id: factID,
    reason,
  });
}

// postTeamsRoom is the operator's OWN post to the team room (STUDIO-661) — the human door the
// panel's compose box goes through. The body carries the prose and optional refs and NOTHING else:
// there is no `from` (the daemon stamps `operator`) and no `to` (v1 is room-wide only; a live
// instruction to a running agent is the operator-message mailbox, not the room).
export async function postTeamsRoom(body: string, refs: string[] = []): Promise<TeamsRoomPost> {
  return postJSON<TeamsRoomPost>("/api/v1/teams/room", { body, refs });
}

export async function fetchTeamsConfig(): Promise<TeamsConfigView> {
  return getJSON<TeamsConfigView>("/api/v1/teams/config");
}

// saveTeamsConfig writes teams.yaml — the ONE explicit act that creates it. The daemon validates
// with the same `Teams::validate` it uses at boot and writes nothing on a rejection, so a failure
// here means the on-disk file is exactly as it was.
export async function saveTeamsConfig(config: TeamsConfig): Promise<TeamsConfigView> {
  return postJSON<TeamsConfigView>("/api/v1/teams/config", { config });
}
