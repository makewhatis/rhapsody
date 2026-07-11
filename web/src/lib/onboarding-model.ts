// Pure onboarding validators + step logic, free of React and the Wails bridge so they unit-test
// cleanly. Ported from the desktop reference (creds.ts / wizard.ts) into the shipped web app.

export type OnboardStep = "token" | "project";

// onboardingStep picks the wizard step from whether a Linear token is already stored: no token →
// collect it first; token present → collect the project slug.
export function onboardingStep(hasToken: boolean): OnboardStep {
  return hasToken ? "project" : "token";
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
