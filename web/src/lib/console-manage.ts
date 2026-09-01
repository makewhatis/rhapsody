import { MIN_MODEL_TIMEOUT_MS, splitLabels, type RosterDraft, type TeamsDraft } from "@/lib/teams-model";

// console-manage — the pure logic behind the console's Manage-team form (STUDIO-681 §7, built
// by STUDIO-686): what the form reveals, what it disables, and what it warns about.
//
// The form itself is deliberately thin, because the MODEL for teams.yaml already exists:
// `lib/teams-model.ts` owns loading a draft, validating it, rendering its YAML and turning it
// back into a config, and the Podium Settings editor (STUDIO-652/667) composes over the same
// functions. Two editors of one file must not disagree about what the file says, so §7 adds no
// second serializer and no second validator — only the §7-specific reveal rules below.

/** The profiles compiled into the daemon (`crates/config/src/profiles/builtin`). */
export const BUILTIN_PROFILES = ["swe", "sre", "reviewer"] as const;

/**
 * The options a roster row's profile Select offers: the built-ins, plus whatever the row
 * already holds.
 *
 * The prototype's Select lists three names, but a profile is any
 * `~/.rhapsody/teams/profiles/<name>.md` (`crates/config/src/profiles.rs`) — so a fixed list
 * would render a hand-written `data-eng` as `swe` and then SAVE that, silently retargeting a
 * teammate's role. Carrying the current value keeps the Select honest about a file it did not
 * write.
 */
export function profileOptions(current: string): string[] {
  const value = current.trim();
  const builtins = [...BUILTIN_PROFILES];
  if (value === "" || (BUILTIN_PROFILES as readonly string[]).includes(value)) return builtins;
  return [...builtins, value];
}

/** What an empty `manager.default_identity` reads as: no fallback teammate, so least-loaded wins. */
export const LEAST_LOADED_LABEL = "— least-loaded —";

/**
 * The default-identity Select's options (§7). Only NAMED rows appear: `toConfig` drops a
 * half-typed row, and the daemon rejects a `default_identity` naming nobody, so offering one
 * would let the operator choose a value the save then has to clear.
 */
export function defaultIdentityOptions(draft: TeamsDraft): { value: string; label: string }[] {
  const named = draft.roster.map((r) => r.name.trim()).filter((n) => n !== "");
  return [{ value: "", label: LEAST_LOADED_LABEL }, ...named.map((n) => ({ value: n, label: n }))];
}

/**
 * Box 5.2 — `off` is single-identity Teams (`ManagerMode::Off`): no routing at all, so no model
 * is ever consulted and the field that names one is inert.
 */
export function managerModelDisabled(mode: string): boolean {
  return mode === "off";
}

/** Box 5.4 — endpoint and API key exist only for the remote bank; `local`/`none` have neither. */
export function showsHindsightFields(backend: string): boolean {
  return backend === "hindsight";
}

/**
 * Box 5.3 — the starvation warning, mirroring `Teams::starved_manager_timeout_ms`
 * (`crates/config/src/teams.rs`) so the form warns about exactly what the daemon warns about at
 * boot, and returns the operator's own number to name back to them rather than a clamp.
 *
 * It deliberately inherits all three of that function's abstentions: it does not fire outside
 * `labels+model` (no other mode runs a model turn, so no other mode can be starved — the
 * warning's own sentence would be false), it does not fire on a non-positive value (that means
 * "no value", and the triage task substitutes the schema default), and it never clamps.
 */
export function starvedTimeoutMs(draft: TeamsDraft): number | null {
  const ms = draft.managerTimeoutMs;
  const starved = draft.enabled && draft.managerMode === "labels+model" && ms > 0 && ms < MIN_MODEL_TIMEOUT_MS;
  return starved ? ms : null;
}

/**
 * The bridge between `RosterDraft.labels` — the comma text the shared model stores — and the
 * chip list §7's TagInput renders. Kept here rather than inline in the view so both directions
 * are pinned by one test.
 */
export function rowLabels(row: RosterDraft): string[] {
  return splitLabels(row.labels);
}

export function joinRowLabels(tags: readonly string[]): string {
  return tags.join(", ");
}
