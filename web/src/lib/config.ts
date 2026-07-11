// Pure mapping between the daemon's WORKFLOW.md front-matter map (GET/POST /api/v1/config) and
// the Settings form. Kept free of React so the round-trip is straightforward to unit-test.
//
// The form edits a typed subset of "core" fields (spec §5) while everything else in the map —
// including the `tracker.api_key: $LINEAR_API_KEY` indirection and any advanced blocks (otel,
// hooks, storage) — is preserved verbatim, so saving the form never drops or rewrites config
// the UI does not surface.

export type ConfigMap = Record<string, unknown>;

// SettingsForm holds the editable core fields. Numbers are kept as strings (raw input values);
// list fields are comma-separated strings. They are parsed back to typed YAML values on save.
export interface SettingsForm {
  projectSlug: string;
  activeStates: string;
  terminalStates: string;
  reviewStates: string;
  reviewPromoteState: string;
  milestone: string;
  maxConcurrentAgents: string;
  maxTurns: string;
  model: string;
  permissionMode: string;
  billingGuard: boolean;
  workspaceRoot: string;
  serverPort: string;
}

function obj(m: ConfigMap, key: string): Record<string, unknown> {
  const v = m[key];
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
}

function str(o: Record<string, unknown>, key: string): string {
  const v = o[key];
  return v == null ? "" : String(v);
}

function joinList(o: Record<string, unknown>, key: string): string {
  const v = o[key];
  return Array.isArray(v) ? v.map((x) => String(x)).join(", ") : "";
}

function splitList(s: string): string[] {
  return s
    .split(",")
    .map((x) => x.trim())
    .filter((x) => x.length > 0);
}

// formFromConfig extracts the editable fields from a config map (with sensible defaults for
// absent keys). billing_guard defaults to true (nil => enabled, matching the daemon).
export function formFromConfig(config: ConfigMap): SettingsForm {
  const tracker = obj(config, "tracker");
  const agent = obj(config, "agent");
  const claude = obj(config, "claude");
  const workspace = obj(config, "workspace");
  const server = obj(config, "server");
  return {
    projectSlug: str(tracker, "project_slug"),
    activeStates: joinList(tracker, "active_states"),
    terminalStates: joinList(tracker, "terminal_states"),
    reviewStates: joinList(tracker, "review_states"),
    reviewPromoteState: str(tracker, "review_promote_state"),
    milestone: str(tracker, "milestone"),
    maxConcurrentAgents: str(agent, "max_concurrent_agents"),
    maxTurns: str(agent, "max_turns"),
    model: str(claude, "model"),
    permissionMode: str(claude, "permission_mode"),
    billingGuard: claude.billing_guard == null ? true : Boolean(claude.billing_guard),
    workspaceRoot: str(workspace, "root"),
    serverPort: str(server, "port"),
  };
}

// configWithForm returns a NEW config map with the form's edits applied over a deep clone of
// the input, preserving every key the form does not manage. Empty string / list fields are
// omitted (so daemon defaults apply) rather than written as "" or 0.
export function configWithForm(config: ConfigMap, form: SettingsForm): ConfigMap {
  const out = structuredClone(config) as ConfigMap;
  const tracker = ensure(out, "tracker");
  const agent = ensure(out, "agent");
  const claude = ensure(out, "claude");
  const workspace = ensure(out, "workspace");
  const server = ensure(out, "server");

  setStr(tracker, "project_slug", form.projectSlug);
  setList(tracker, "active_states", form.activeStates);
  setList(tracker, "terminal_states", form.terminalStates);
  setList(tracker, "review_states", form.reviewStates);
  setStr(tracker, "review_promote_state", form.reviewPromoteState);
  setStr(tracker, "milestone", form.milestone);
  setNum(agent, "max_concurrent_agents", form.maxConcurrentAgents);
  setNum(agent, "max_turns", form.maxTurns);
  setStr(claude, "model", form.model);
  setStr(claude, "permission_mode", form.permissionMode);
  // Only write billing_guard when it was already present on disk OR the user set the non-default
  // false. Writing the default (true) into a config that has no `claude` block would inject a
  // block the user never set and break the preservation/idempotency contract — and nil vs true
  // are equivalent to the daemon anyway.
  if (hadBillingGuard(config) || form.billingGuard === false) {
    claude.billing_guard = form.billingGuard;
  }
  setStr(workspace, "root", form.workspaceRoot);
  setNum(server, "port", form.serverPort);

  // Drop sub-objects that ended up empty so we don't introduce bare `agent: {}` blocks.
  pruneEmpty(out, ["tracker", "agent", "claude", "workspace", "server"]);
  return out;
}

function ensure(m: ConfigMap, key: string): Record<string, unknown> {
  if (!m[key] || typeof m[key] !== "object" || Array.isArray(m[key])) {
    m[key] = {};
  }
  return m[key] as Record<string, unknown>;
}

function setStr(o: Record<string, unknown>, key: string, value: string) {
  const v = value.trim();
  if (v === "") {
    delete o[key];
  } else {
    o[key] = v;
  }
}

function setList(o: Record<string, unknown>, key: string, value: string) {
  const list = splitList(value);
  if (list.length === 0) {
    delete o[key];
  } else {
    o[key] = list;
  }
}

function setNum(o: Record<string, unknown>, key: string, value: string) {
  const v = value.trim();
  if (v === "") {
    delete o[key];
    return;
  }
  const n = Number(v);
  if (!Number.isFinite(n)) {
    // Non-numeric input: drop the key rather than silently retaining a stale cloned value.
    delete o[key];
    return;
  }
  o[key] = n;
}

// hadBillingGuard reports whether the source config explicitly carried claude.billing_guard.
function hadBillingGuard(config: ConfigMap): boolean {
  const claude = config.claude;
  return (
    !!claude &&
    typeof claude === "object" &&
    !Array.isArray(claude) &&
    "billing_guard" in (claude as Record<string, unknown>)
  );
}

function pruneEmpty(m: ConfigMap, keys: string[]) {
  for (const k of keys) {
    const v = m[k];
    if (v && typeof v === "object" && !Array.isArray(v) && Object.keys(v).length === 0) {
      delete m[k];
    }
  }
}
