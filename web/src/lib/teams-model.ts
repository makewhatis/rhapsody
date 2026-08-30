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

// RosterDraft is one editable roster row. `labels` is the raw comma-separated text the user typed,
// kept as typed so a trailing comma mid-edit does not fight the field.
export interface RosterDraft {
  name: string;
  profile: string;
  labels: string;
}

export interface TeamsDraft {
  enabled: boolean;
  managerMode: string;
  backend: string;
  roster: RosterDraft[];
}

export const MANAGER_MODES = ["off", "labels", "labels+model"] as const;
export const MEMORY_BACKENDS = ["none", "local"] as const;

// The schema defaults (design §2.2), restated here because a NEW teams.yaml has no file to inherit
// them from. The daemon re-applies every one of them on read, so a draft that omits a key gets the
// same answer either way — these exist so the editor shows the truth before the first save.
const DEFAULT_MANAGER_MODE = "labels";
const DEFAULT_BACKEND = "local";

export function emptyDraft(): TeamsDraft {
  return {
    enabled: true,
    managerMode: DEFAULT_MANAGER_MODE,
    backend: DEFAULT_BACKEND,
    roster: [{ name: "", profile: "swe", labels: "" }],
  };
}

// toDraft loads an existing teams.yaml into the editor. An absent file yields the starter draft:
// the file does not exist, so there is nothing to preserve, and offering a blank row is what makes
// "Create teams.yaml…" one field away from useful.
export function toDraft(config: TeamsConfig | undefined, present: boolean): TeamsDraft {
  if (!config || !present) return emptyDraft();
  return {
    enabled: config.enabled,
    managerMode: config.manager?.mode || DEFAULT_MANAGER_MODE,
    backend: config.memory?.backend || DEFAULT_BACKEND,
    roster:
      config.roster?.length > 0
        ? config.roster.map((r) => ({
            name: r.name,
            profile: r.profile,
            labels: (r.labels ?? []).join(", "),
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

// LABEL_SAFE mirrors the daemon's `is_label_safe` (crates/config/src/teams.rs) EXACTLY. The charset
// is pinned because a name is interpolated into a `rhapsody:@<name>` Linear label and a
// `<bank_prefix><name>` bank id, so widening it would break two external namespaces at once.
//
// This is a *convenience* check, not the gate: the daemon runs its own `Teams::validate` on the
// POST and its complaint is what the UI shows. Checking here only means the obvious mistake is
// caught while typing instead of on save.
export const LABEL_SAFE = /^[a-z][a-z0-9-]*$/;

// draftErrors reports what the daemon would reject, so the editor can disable Save rather than
// invite a round-trip that cannot succeed. Deliberately the same three rules `Teams::validate`
// enforces, in the same order, and nothing more — a client that validated MORE than the daemon
// would block a config the daemon would happily load.
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

// toConfig turns the draft into the teams.yaml the daemon writes. Only the keys the editor owns are
// sent; everything else (manager.model, timeouts, bank_prefix, recall_top_k, prompt_budget_bytes)
// is left to the daemon's schema defaults rather than invented here, so this minimal editor can
// never silently pin a value a future default changes.
//
// `base` is the config already on disk, so editing an existing file preserves the keys this editor
// does not show — a Settings form must not quietly drop a hand-tuned `manager.model`.
export function toConfig(draft: TeamsDraft, base?: TeamsConfig): Partial<TeamsConfig> {
  const roster = draft.roster
    .filter((r) => r.name.trim() !== "")
    .map((r) => {
      const name = r.name.trim();
      // Carry a teammate's unexposed keys (`bank`, `max_concurrent`) forward by NAME, never by row
      // index: deleting or reordering a row would otherwise slide alice's bank override onto bob,
      // silently pointing one teammate at another's memory. A renamed row correctly keeps nothing —
      // a new name is a new identity, with a new bank.
      const prior = base?.roster?.find((b) => b.name === name);
      return { ...(prior ?? {}), name, profile: r.profile.trim() || "swe", labels: splitLabels(r.labels) };
    });
  const names = new Set(roster.map((r) => r.name));
  // `manager.default_identity` is preserved like every other unexposed key — EXCEPT when the roster
  // edit just removed the teammate it names. The daemon rejects that file ("default_identity is not
  // a roster entry"), and since this editor does not surface the field, keeping it would leave the
  // operator unable to save and unable to see why. Removing the teammate is the explicit act; the
  // dangling pointer to them is the thing that has to go.
  const priorDefault = base?.manager?.default_identity ?? "";
  const default_identity = names.has(priorDefault) ? priorDefault : "";
  return {
    ...(base ?? {}),
    enabled: draft.enabled,
    manager: { ...(base?.manager ?? {}), mode: draft.managerMode, default_identity },
    memory: { ...(base?.memory ?? {}), backend: draft.backend },
    roster,
  } as Partial<TeamsConfig>;
}

// teamsYamlSnippet renders the draft as the teams.yaml text it would write. It is what the enable
// flow shows BEFORE saving — "here is the file about to be created" — so an explicit save is
// explicit about what it does, and it doubles as the exact snippet to write by hand on a daemon
// with no on-disk runtime home to save into.
export function teamsYamlSnippet(draft: TeamsDraft): string {
  const lines = [
    "# ~/.rhapsody/teams.yaml",
    `enabled: ${draft.enabled}`,
    "",
    "manager:",
    `  mode: ${draft.managerMode}`,
    "",
    "memory:",
    `  backend: ${draft.backend}`,
    "",
    "roster:",
  ];
  const rows = draft.roster.filter((r) => r.name.trim() !== "");
  if (rows.length === 0) lines.push("  []");
  for (const r of rows) {
    lines.push(`  - name: ${r.name.trim()}`);
    lines.push(`    profile: ${r.profile.trim() || "swe"}`);
    const labels = splitLabels(r.labels);
    if (labels.length > 0) lines.push(`    labels: [${labels.join(", ")}]`);
  }
  return lines.join("\n");
}
