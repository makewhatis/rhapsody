import type { TeamsConfig, TeamsOverview, TeamsRoomMessage } from "@/lib/api";

// teams-model — the pure logic behind the Teams dashboard surface (STUDIO-652): the status chip's
// counts, the room's provenance framing, and the client half of the enable flow's validation.
//
// It is deliberately separate from the components, which is this codebase's discipline for anything
// worth asserting on directly (see runs-model, settings-model): the rules below are the ones a
// reviewer wants pinned, and pinning them through rendered DOM would test the markup instead.

// TeamsChip is the toolbar chip's model: "Teams: N teammates, M live". Null ⇒ nothing to show.
export interface TeamsChip {
  teammates: number;
  live: number;
  label: string;
}

// teamsChip derives the status-strip chip. `null` for a daemon with Teams off or an overview that
// has not arrived yet — the chip is the ONLY dashboard change Teams makes, so its absence is what
// keeps a Teams-off app byte-for-byte what it was.
//
// `live` counts RUNS, not teammates: two runs as alice is "2 live", because the number an operator
// is reading the strip for is how much is in flight, not how many people are busy.
export function teamsChip(overview: TeamsOverview | undefined | null): TeamsChip | null {
  if (!overview || !overview.enabled) return null;
  const roster = overview.roster ?? [];
  const teammates = roster.length;
  const live = roster.reduce((n, r) => n + (r.live_runs || 0), 0);
  const plural = teammates === 1 ? "teammate" : "teammates";
  return { teammates, live, label: `Teams: ${teammates} ${plural}, ${live} live` };
}

// roomAuthorLine is the provenance prefix design §0.11.5 requires: a room post is rendered as
// QUOTED, attributed data — "alice wrote on <when>" — never as bare text that could read as an
// instruction. The UI owes this framing because the same messages are untrusted content that reach
// every teammate's prompt; one poisoned post should never look like the app talking.
export function roomAuthorLine(m: TeamsRoomMessage, formatAt: (at: string) => string): string {
  const who = m.from || "unknown";
  const when = formatAt(m.at);
  const audience = m.to && m.to !== "*" ? ` to ${m.to}` : "";
  return when ? `${who} wrote${audience} on ${when}` : `${who} wrote${audience}`;
}

// errText renders a thrown value for the operator. The API client throws the DAEMON's own message
// verbatim, and `String(err)` would prefix it with "Error: " — turning the daemon's sentence into
// something that reads like a client stack trace.
export function errText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

// --- the enable flow's editor ---
//
// STUDIO-667 widened this from the STUDIO-652 v1 cut (enable, roster, manager mode, memory backend)
// to EVERY field in `crates/config/src/teams.rs`, on David's principle: "we never want to make
// someone configure yaml." The two properties the v1 editor earned survive verbatim — nothing is
// written until Save, and keys the editor does not model are carried forward untouched — because
// they are what makes widening the editor safe rather than a new way to mangle a hand-written file.

// RosterDraft is one editable roster row. `labels` is the raw comma-separated text the user typed,
// kept as typed so a trailing comma mid-edit does not fight the field.
export interface RosterDraft {
  name: string;
  profile: string;
  labels: string;
  /** Bank id override; "" ⇒ `<bank_prefix><name>`. Advanced, per row. */
  bank: string;
  /** 0 ⇒ unlimited. Advanced, per row. */
  maxConcurrent: number;
}

export interface TeamsDraft {
  enabled: boolean;
  managerMode: string;
  defaultIdentity: string;
  managerModel: string;
  managerMaxTokens: number;
  managerTimeoutMs: number;
  quorumEnabled: boolean;
  quorumReviewers: number;
  backend: string;
  memoryPath: string;
  memoryEndpoint: string;
  /**
   * The api_key AS TYPED — an env-var name like `$HINDSIGHT_API_KEY`, a literal the operator just
   * entered, or "". It is NEVER loaded from a stored literal; see `apiKeyStored`.
   */
  apiKey: string;
  /**
   * True while a literal secret already in teams.yaml is being carried forward unread. The literal
   * itself never enters the draft, so no code path can render it back to the screen — the operator
   * either leaves it alone or replaces it outright.
   */
  apiKeyStored: boolean;
  bankPrefix: string;
  recallTopK: number;
  promptBudgetBytes: number;
  roster: RosterDraft[];
}

export const MANAGER_MODES = ["off", "labels", "labels+model"] as const;
// `hindsight` is the cloud bank (STUDIO-660). It was missing from the v1 list, which is exactly the
// gap this ticket closes: switching a team onto the shared bank meant hand-editing teams.yaml.
export const MEMORY_BACKENDS = ["none", "local", "hindsight"] as const;

// The schema defaults (`crates/config/src/teams.rs`), restated here because a NEW teams.yaml has no
// file to inherit them from. The daemon re-applies every one of them on read, so a draft that omits
// a key gets the same answer either way — these exist so the editor shows the truth before the
// first save. Keep them in step with the Rust constants they mirror.
const DEFAULT_MANAGER_MODE = "labels";
const DEFAULT_BACKEND = "local";
export const DEFAULT_MAX_TOKENS = 4000;
export const DEFAULT_TIMEOUT_MS = 60000;
export const DEFAULT_BANK_PREFIX = "agent-";
export const DEFAULT_RECALL_TOP_K = 8;
export const DEFAULT_PROMPT_BUDGET_BYTES = 16000;
export const DEFAULT_QUORUM_REVIEWERS = 2;
/** `MIN_QUORUM_REVIEWERS` — a quorum of zero is not a quorum, it is `enabled: false`. */
export const MIN_QUORUM_REVIEWERS = 1;

export function emptyRow(): RosterDraft {
  return { name: "", profile: "swe", labels: "", bank: "", maxConcurrent: 0 };
}

export function emptyDraft(): TeamsDraft {
  return {
    enabled: true,
    managerMode: DEFAULT_MANAGER_MODE,
    defaultIdentity: "",
    managerModel: "",
    managerMaxTokens: DEFAULT_MAX_TOKENS,
    managerTimeoutMs: DEFAULT_TIMEOUT_MS,
    quorumEnabled: false,
    quorumReviewers: DEFAULT_QUORUM_REVIEWERS,
    backend: DEFAULT_BACKEND,
    memoryPath: "",
    memoryEndpoint: "",
    apiKey: "",
    apiKeyStored: false,
    bankPrefix: DEFAULT_BANK_PREFIX,
    recallTopK: DEFAULT_RECALL_TOP_K,
    promptBudgetBytes: DEFAULT_PROMPT_BUDGET_BYTES,
    roster: [emptyRow()],
  };
}

// ENV_INDIRECTION mirrors `resolve_var` (crates/config/src/resolve.rs): a bare `$NAME` whose name is
// a shell identifier is read from the daemon's environment instead of used literally. It is the
// spelling this editor encourages for `memory.api_key`, because it keeps the secret out of the file.
export const ENV_INDIRECTION = /^\$[A-Za-z_][A-Za-z0-9_]*$/;

export function isEnvIndirection(value: string): boolean {
  return ENV_INDIRECTION.test(value.trim());
}

// isStoredSecret reports an api_key that is a LITERAL credential rather than an env-var name — the
// one value this editor must never render back. Empty is not a secret; `$NAME` is a pointer, not a
// secret, so it stays visible and editable like any other field.
export function isStoredSecret(apiKey: string | undefined): boolean {
  const v = (apiKey ?? "").trim();
  return v !== "" && !isEnvIndirection(v);
}

/** What a stored literal renders as. Fixed-width and value-independent: it leaks not even a length. */
export const MASKED_API_KEY = "••••••••••••";

// toDraft loads an existing teams.yaml into the editor. An absent file yields the starter draft: the
// file does not exist, so there is nothing to preserve, and offering a blank row is what makes
// "Create teams.yaml…" one field away from useful.
//
// `??` rather than `||` throughout: 0 is a value the schema gives meaning to (`max_concurrent: 0` is
// unlimited, a non-positive `recall_top_k` / `prompt_budget_bytes` falls back to the default), so a
// stored 0 must round-trip as 0 rather than being silently rewritten to a default the file never had.
export function toDraft(config: TeamsConfig | undefined, present: boolean): TeamsDraft {
  if (!config || !present) return emptyDraft();
  const d = emptyDraft();
  const storedKey = config.memory?.api_key ?? "";
  const secret = isStoredSecret(storedKey);
  return {
    enabled: config.enabled,
    managerMode: config.manager?.mode || DEFAULT_MANAGER_MODE,
    defaultIdentity: config.manager?.default_identity ?? "",
    managerModel: config.manager?.model ?? "",
    managerMaxTokens: config.manager?.max_tokens ?? d.managerMaxTokens,
    managerTimeoutMs: config.manager?.timeout_ms ?? d.managerTimeoutMs,
    quorumEnabled: config.quorum?.enabled ?? false,
    quorumReviewers: config.quorum?.reviewers ?? DEFAULT_QUORUM_REVIEWERS,
    backend: config.memory?.backend || DEFAULT_BACKEND,
    memoryPath: config.memory?.path ?? "",
    memoryEndpoint: config.memory?.endpoint ?? "",
    // A literal is deliberately dropped on the floor here and carried forward from `base` in
    // `toConfig` instead. That is what makes "never rendered back in cleartext" a property of the
    // MODEL rather than a discipline every component has to remember.
    apiKey: secret ? "" : storedKey,
    apiKeyStored: secret,
    bankPrefix: config.memory?.bank_prefix ?? DEFAULT_BANK_PREFIX,
    recallTopK: config.memory?.recall_top_k ?? DEFAULT_RECALL_TOP_K,
    promptBudgetBytes: config.prompt_budget_bytes ?? DEFAULT_PROMPT_BUDGET_BYTES,
    roster:
      config.roster?.length > 0
        ? config.roster.map((r) => ({
            name: r.name,
            profile: r.profile,
            labels: (r.labels ?? []).join(", "),
            bank: r.bank ?? "",
            maxConcurrent: r.max_concurrent ?? 0,
          }))
        : emptyDraft().roster,
  };
}

// splitLabels parses the comma-separated label field. Blank entries are dropped rather than stored
// as empty labels, which would match nothing and read as a bug in the router.
export function splitLabels(text: string): string[] {
  return text
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

// namedRoster is the roster the save would actually write: rows the operator started and abandoned
// are not teammates. Shared so the quorum copy counts the same rows `toConfig` sends.
function namedRoster(draft: TeamsDraft): RosterDraft[] {
  return draft.roster.filter((r) => r.name.trim() !== "");
}

// effectiveReviewers is what the daemon's `select_reviewers` (crates/orchestrator/src/quorum.rs)
// will actually fan out to: `quorum.reviewers` floored at 1, then clamped to the roster MINUS the
// author — a teammate never reviews their own handoff. A roster of two therefore buys one reviewer,
// and a roster of one buys none, which is the degradation the copy has to say out loud.
export function effectiveReviewers(draft: TeamsDraft): number {
  const others = namedRoster(draft).length - 1;
  return Math.max(0, Math.min(Math.max(draft.quorumReviewers, MIN_QUORUM_REVIEWERS), others));
}

// quorumNote is the cost sentence. §0.6 calls the quorum "the most expensive item in the design" and
// §0.12 makes it opt-in for exactly that reason, so the editor states the price in runs before the
// toggle is flipped rather than after the bill arrives.
export function quorumNote(draft: TeamsDraft): string {
  const size = namedRoster(draft).length;
  const n = effectiveReviewers(draft);
  const team = `${size} teammate${size === 1 ? "" : "s"}`;
  if (n === 0) {
    return `Nothing would fan out: a teammate never reviews their own handoff, and this roster has ${team}. Add another teammate.`;
  }
  const runs = `${n} review run${n === 1 ? "" : "s"}`;
  const clamp =
    n < Math.max(draft.quorumReviewers, MIN_QUORUM_REVIEWERS)
      ? ` Reviewers are clamped to the roster minus the author, so ${team} means ${n}.`
      : "";
  return `Each handoff fans out ${runs} — ${n} extra agent run${n === 1 ? "" : "s"}, and their cost, per handoff.${clamp}`;
}

// LABEL_SAFE mirrors the daemon's `is_label_safe` (crates/config/src/teams.rs) EXACTLY. The charset
// is pinned because a name is interpolated into a `rhapsody:@<name>` Linear label and a
// `<bank_prefix><name>` bank id, so widening it would break two external namespaces at once.
//
// This is a *convenience* check, not the gate: the daemon runs its own `Teams::validate` on the
// POST and its complaint is what the UI shows. Checking here only means the obvious mistake is
// caught while typing instead of on save.
export const LABEL_SAFE = /^[a-z][a-z0-9-]*$/;

// draftErrors reports what the daemon would reject, so the editor can disable Save rather than
// invite a round-trip that cannot succeed. Deliberately the label-safe and duplicate-name rules
// `Teams::validate` enforces, in the same order, and — apart from the empty-roster rule below —
// nothing more: a client that validated MORE than the daemon would block a config the daemon would
// happily load. In particular the fields STUDIO-667 added contribute NO client rules, because
// `Teams::validate` has no opinion on a reviewer count, a bank prefix or a budget either.
//
// The one client-only rule, inherited from STUDIO-652 and deliberately left as it is: the empty
// roster. `Teams::validate` accepts one (`validates_a_default_identity ... even with an empty
// roster`), but a roster-less team can do nothing, and this editor is the surface that creates the
// file. Called out because the "nothing more" claim above was previously written as absolute.
export function draftErrors(draft: TeamsDraft): string[] {
  const errors: string[] = [];
  const seen = new Set<string>();
  for (const row of draft.roster) {
    const name = row.name.trim();
    if (name === "") {
      errors.push("every teammate needs a name");
      continue;
    }
    if (!LABEL_SAFE.test(name)) {
      errors.push(`roster name "${name}" is not label-safe (must match ^[a-z][a-z0-9-]*$)`);
    }
    if (seen.has(name)) errors.push(`duplicate roster name "${name}"`);
    seen.add(name);
  }
  if (draft.roster.length === 0) errors.push("a roster needs at least one teammate");
  return errors;
}

// toConfig turns the draft into the teams.yaml the daemon writes.
//
// `base` is the config already on disk, and it is still spread FIRST even though the editor now
// models every field the schema declares: the spread is what carries forward anything a NEWER
// daemon serves that this build has never heard of. That is the property the STUDIO-652 round-trip
// test pinned, and widening the editor must not quietly retire it — an editor that sends only the
// keys it knows would silently delete a field added after it shipped.
//
// The v1 note that this editor "can never silently pin a value a future default changes" no longer
// holds for the fields it now models, and cannot: exposing `max_tokens` means sending one. It costs
// nothing — `Teams::save` writes the CANONICAL serialization with every schema default made
// explicit, so the file has always pinned every key the daemon models the moment it is written from
// the app. That same rewrite is why the carry-forward below is a property of THIS CLIENT and not of
// the file: a key the daemon does not model is dropped by its serializer no matter what we send.
export function toConfig(draft: TeamsDraft, base?: TeamsConfig): Partial<TeamsConfig> {
  const roster = namedRoster(draft).map((r) => {
    const name = r.name.trim();
    // Carry a teammate's unmodelled keys forward by NAME, never by row index: deleting or
    // reordering a row would otherwise slide one teammate's settings onto another. A renamed row
    // correctly keeps nothing — a new name is a new identity.
    const prior = base?.roster?.find((b) => b.name === name);
    return {
      ...(prior ?? {}),
      name,
      profile: r.profile.trim() || "swe",
      labels: splitLabels(r.labels),
      bank: r.bank.trim(),
      max_concurrent: r.maxConcurrent,
    };
  });
  const names = new Set(roster.map((r) => r.name));
  // The daemon rejects a `default_identity` naming nobody, so a roster edit that removed the chosen
  // teammate clears it rather than leaving the operator unable to save. The dropdown cannot produce
  // a dangling value, but a rename after choosing one can.
  const default_identity = names.has(draft.defaultIdentity) ? draft.defaultIdentity : "";
  // A stored literal never entered the draft, so "unchanged" means "whatever is on disk". Anything
  // else — an env-var name, a freshly typed literal, or "" to clear — is what the operator typed.
  const api_key = draft.apiKeyStored ? (base?.memory?.api_key ?? "") : draft.apiKey.trim();
  return {
    ...(base ?? {}),
    enabled: draft.enabled,
    manager: {
      ...(base?.manager ?? {}),
      mode: draft.managerMode,
      default_identity,
      model: draft.managerModel.trim(),
      max_tokens: draft.managerMaxTokens,
      timeout_ms: draft.managerTimeoutMs,
    },
    memory: {
      ...(base?.memory ?? {}),
      backend: draft.backend,
      path: draft.memoryPath.trim(),
      endpoint: draft.memoryEndpoint.trim(),
      api_key,
      // Trimmed like every other string this form sends: a prefix is interpolated straight into a
      // `<bank_prefix><name>` bank id, so stray whitespace is a typo that silently points a
      // teammate at a bank nobody else resolves.
      bank_prefix: draft.bankPrefix.trim(),
      recall_top_k: draft.recallTopK,
    },
    quorum: { ...(base?.quorum ?? {}), enabled: draft.quorumEnabled, reviewers: draft.quorumReviewers },
    roster,
    prompt_budget_bytes: draft.promptBudgetBytes,
  } as Partial<TeamsConfig>;
}

// teamsYamlSnippet renders the draft as the teams.yaml text it would write. It is what the enable
// flow shows BEFORE saving — "here is the file about to be created" — so an explicit save is
// explicit about what it does, and it doubles as the exact snippet to write by hand on a daemon
// with no on-disk runtime home to save into.
//
// Schema defaults are omitted rather than spelled out: the daemon re-applies every one on read, and
// a preview that listed every key would bury the two the operator just changed. `api_key` is
// the one field that is never shown verbatim — a stored literal renders masked, exactly as the form
// above renders it.
export function teamsYamlSnippet(draft: TeamsDraft): string {
  const lines = ["# ~/.rhapsody/teams.yaml", `enabled: ${draft.enabled}`, "", "manager:", `  mode: ${draft.managerMode}`];
  const named = namedRoster(draft);
  if (draft.defaultIdentity && named.some((r) => r.name.trim() === draft.defaultIdentity)) {
    lines.push(`  default_identity: ${draft.defaultIdentity}`);
  }
  if (draft.managerModel.trim()) lines.push(`  model: ${draft.managerModel.trim()}`);
  if (draft.managerMaxTokens !== DEFAULT_MAX_TOKENS) lines.push(`  max_tokens: ${draft.managerMaxTokens}`);
  if (draft.managerTimeoutMs !== DEFAULT_TIMEOUT_MS) lines.push(`  timeout_ms: ${draft.managerTimeoutMs}`);

  if (draft.quorumEnabled) {
    // The RAW value, not `effectiveReviewers`: this card previews the FILE, and the daemon applies
    // its own floor on read. `quorumNote` beside the toggle is where the effective count is stated,
    // so the two surfaces stay honest about different things rather than both being half-right.
    lines.push("", "quorum:", "  enabled: true", `  reviewers: ${draft.quorumReviewers}`);
  }

  lines.push("", "memory:", `  backend: ${draft.backend}`);
  if (draft.backend === "hindsight") {
    if (draft.memoryEndpoint.trim()) lines.push(`  endpoint: ${draft.memoryEndpoint.trim()}`);
    const key = draft.apiKeyStored ? MASKED_API_KEY : draft.apiKey.trim();
    if (key) lines.push(`  api_key: ${key}`);
  }
  if (draft.backend === "local" && draft.memoryPath.trim()) lines.push(`  path: ${draft.memoryPath.trim()}`);
  if (draft.backend !== "none") {
    if (draft.bankPrefix !== DEFAULT_BANK_PREFIX) lines.push(`  bank_prefix: ${draft.bankPrefix}`);
    if (draft.recallTopK !== DEFAULT_RECALL_TOP_K) lines.push(`  recall_top_k: ${draft.recallTopK}`);
  }

  if (draft.promptBudgetBytes !== DEFAULT_PROMPT_BUDGET_BYTES) {
    lines.push("", `prompt_budget_bytes: ${draft.promptBudgetBytes}`);
  }

  lines.push("", "roster:");
  if (named.length === 0) lines.push("  []");
  for (const r of named) {
    lines.push(`  - name: ${r.name.trim()}`);
    lines.push(`    profile: ${r.profile.trim() || "swe"}`);
    const labels = splitLabels(r.labels);
    if (labels.length > 0) lines.push(`    labels: [${labels.join(", ")}]`);
    if (r.bank.trim()) lines.push(`    bank: ${r.bank.trim()}`);
    if (r.maxConcurrent !== 0) lines.push(`    max_concurrent: ${r.maxConcurrent}`);
  }
  return lines.join("\n");
}
