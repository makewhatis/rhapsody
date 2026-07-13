// Pure onboarding validators + step logic, free of React and the Wails bridge so they unit-test
// cleanly. Ported from the desktop reference (creds.ts / wizard.ts) into the shipped web app, then
// grown into the P10 "Podium" first-run wizard (three steps + a sound-check derivation).

import type { ToolResult } from "@/lib/bindings";
import { GLOBAL_DEFAULTS, MODELS } from "@/lib/settings-data";

export type OnboardStep = "token" | "project";

// onboardingStep picks the credential-gated stage from whether a Linear token is already stored: no
// token → collect it first (wizard step 1); token present → the daemon-observable state that gates
// the project stage onward (steps 2–3, distinguished by local wizard state). The 1↔2 transition
// stays credential-driven so a partial prior attempt (token already in the Keychain) resumes past
// step 1, exactly as the original two-step flow did.
export function onboardingStep(hasToken: boolean): OnboardStep {
  return hasToken ? "project" : "token";
}

// --- P10 first-run wizard (mock 2e): three steps with a shared progress footer ---

export const TOTAL_STEPS = 3;

// WizardStep is the 1-indexed step the wizard is showing. Step 1 (Connect Linear) is gated on the
// stored token (see onboardingStep); steps 2 (Choose what to watch) and 3 (Sound check) are both
// "token present" and are distinguished by local wizard state (whether the user advanced past the
// project picker).
export type WizardStep = 1 | 2 | 3;

const STEP_TITLES: Record<WizardStep, string> = {
  1: "CONNECT LINEAR",
  2: "CHOOSE WHAT TO WATCH",
  3: "SOUND CHECK",
};

// stepCapsLabel renders the caps step marker shown above each step's heading, e.g.
// "STEP 1 OF 3 — CONNECT LINEAR" (mock 2e, 10px/600 .14em rust).
export function stepCapsLabel(step: WizardStep): string {
  return `STEP ${step} OF ${TOTAL_STEPS} — ${STEP_TITLES[step]}`;
}

// --- Model select (step 2) ---

// The wizard's model choices reuse the canonical Settings list so onboarding and Settings never
// drift, and default to the same model the daemon seeds into a fresh WORKFLOW.md
// (onboarding.RenderInitialWorkflow → claude.model). NOTE: the WriteInitialConfig binding takes only
// the project slug, so a NON-default pick is presentational here — the seeded config uses this
// default, and the model is editable per-agent in Settings once the daemon is up.
export const MODEL_OPTIONS = MODELS;
export const DEFAULT_MODEL: string = GLOBAL_DEFAULTS.model;

// stripModel drops the "claude-" prefix for the compact model display (e.g. "claude-opus-4-8" →
// "opus-4-8"), matching the mock and the Settings agent-list cell.
export function stripModel(model: string): string {
  return model.replace(/^claude-/, "");
}

// --- Sound check (step 3) ---

// SoundCheckItem is one row of the step-3 checklist: a name, a mono detail, and whether the check
// passed (sage tick) or needs attention (amber). Pure data so the derivation is unit-testable.
export interface SoundCheckItem {
  key: string;
  name: string;
  detail: string;
  ok: boolean;
}

// The external CLIs the sound check verifies (mock 2e). The daemon shells out to these; `gt`
// (Graphite) is optional and not part of the first-run required set, so it is not listed here.
export const SOUND_CHECK_CLIS = ["claude", "git", "gh"] as const;

// The Rhapsody runtime home the daemon provisions per-issue workspaces under (README Divergences).
// There is no pre-daemon binding that reports the resolved path, so the sound check shows the seeded
// default the fresh config will use.
export const WORKSPACE_ROOT_DEFAULT = "~/.rhapsody/workspaces";

// buildSoundCheck derives the step-3 checklist from the app-side tool probe plus the Linear
// connection: a "Linear API" row (authenticated once a token is stored and its project list loaded),
// one row per required CLI (from the `probeTools` result — version · path when healthy, else the
// missing/unhealthy state), and the workspace-home row. It never gates completion: onboarding writes
// the config and starts the daemon regardless (the checklist is informational), so a not-yet-perfect
// row (e.g. gh missing) reads amber but does not block "Start playing".
export function buildSoundCheck(
  tools: ToolResult[],
  opts: { linearConnected: boolean; account?: string },
): SoundCheckItem[] {
  const byName = new Map(tools.map((t) => [t.name, t]));
  const account = opts.account?.trim();
  const linear: SoundCheckItem = {
    key: "linear",
    name: "Linear API",
    detail: account || (opts.linearConnected ? "Authenticated" : "Not connected"),
    ok: opts.linearConnected,
  };
  const clis = SOUND_CHECK_CLIS.map((name): SoundCheckItem => {
    const t = byName.get(name);
    if (!t) return { key: name, name, detail: "Not detected", ok: false };
    if (!t.found) return { key: name, name, detail: "Not found on PATH", ok: false };
    const detail = [t.version, t.path].map((s) => s?.trim()).filter(Boolean).join(" · ");
    return { key: name, name, detail: detail || "installed", ok: t.healthy };
  });
  const workspace: SoundCheckItem = {
    key: "workspace",
    name: "workspace",
    detail: WORKSPACE_ROOT_DEFAULT,
    ok: true,
  };
  return [linear, ...clis, workspace];
}

// tokenLooksValid is a light client-side sanity check before we hit the Keychain: a Linear personal
// API key starts with "lin_"; we also accept any sufficiently long string so a future token shape
// isn't rejected outright. The daemon is the real validator.
export function tokenLooksValid(token: string): boolean {
  const t = token.trim();
  return t.length > 0 && (t.startsWith("lin_") || t.length >= 40);
}

// slugValid accepts any non-empty project slug — the user types it freehand and the daemon
// validates it against Linear at poll time (onboarding runs before the daemon exists, so the
// workspace's project list isn't available to pick from yet).
export function slugValid(slug: string): boolean {
  return slug.trim().length > 0;
}

// NormalizeResult is the outcome of normalizeProjectSlug: either the bare Linear slugId we will
// write to tracker.project_slug, or a human-readable error explaining why the input couldn't be
// reduced to a slugId.
export type NormalizeResult = { ok: true; slug: string } | { ok: false; error: string };

// normalizeProjectSlug reduces what a user pastes into the manual fallback to the Linear project
// `slugId` the daemon's dispatch query filters on (project: { slugId: { eq } }) — i.e. the EXACT
// string `ListProjects` returns as a project's slug and the picker writes verbatim.
//
// IMPORTANT: the slugId is the FULL trailing path segment of a Linear project URL, NOT a bare hex
// id. Per GETTING_STARTED.md ("…/project/my-project-9c29e9ade060" → project_slug
// "my-project-9c29e9ade060") and the daemon's own client fixtures (slugId "example-infra",
// "core-proj"), a slugId is commonly "<name>-<id>" or even a plain word. (This intentionally
// diverges from the INF-277 ticket's "extract the trailing [0-9a-f]{8,}" wording, which was based
// on a mistaken premise — stripping to the hex tail would write a value that never equals the
// configured slugId, so dispatch would never match. See the PR thread for the evidence.)
//
// Accepted shapes:
//   • bare slug          "my-project-9c29e9ade060" / "example-infra" / "872639248532" → as-is
//   • full project URL   "https://linear.app/<org>/project/<slug>[/<view>][?…][#…]"     → <slug>
// A URL with no /project/<slug> segment is un-normalizable and returns an error so the caller can
// show it inline and refuse to write a config that would never dispatch. The result is lowercased
// (Linear slugs are lowercase) so a stray uppercased paste still matches.
export function normalizeProjectSlug(input: string): NormalizeResult {
  const raw = input.trim();
  if (raw === "") {
    return { ok: false, error: "Enter a Linear project slug or URL." };
  }
  // Full URL → the slug is the path segment after "/project/" (query string, fragment and any
  // trailing view segment like "/overview" are dropped).
  const urlMatch = raw.match(/\/project\/([^/?#]+)/i);
  if (urlMatch) {
    return { ok: true, slug: urlMatch[1].toLowerCase() };
  }
  // Looks like a URL/path but has no project segment → can't extract a project slug.
  if (/^https?:\/\//i.test(raw) || raw.includes("/")) {
    return {
      ok: false,
      error: `Couldn't find a Linear project slug in "${raw}" — paste the project URL or its slug.`,
    };
  }
  // Otherwise it is already a bare slug (e.g. "my-project-9c29e9ade060" or "example-infra").
  return { ok: true, slug: raw.toLowerCase() };
}
