// version-stamp — renders a binary's build identity for the footer stamp (STUDIO-380).
//
// Two independent sources feed the footer: the desktop shell (build.rs, sentinels "dev"/"none") and
// the rhapsodyd daemon (GET /api/v1/version, sentinel "unknown"). Both are normalized here so the
// footer never prints a sentinel as if it were a real version.

// The values either build stamp uses to mean "not stamped". Treated as absent, never displayed as an
// identity — showing "vunknown" next to a real version would read as a build that exists.
const UNSET = ["", "dev", "none", "unknown"];

// stamp renders one binary's identity as "v1.2.0 · 581e281", eliding the SHA when unstamped and
// falling back to the bare "dev" label when the version is too. A short SHA is enough to compare two
// builds by eye; the full one stays in the API payload.
export function stamp(version: string, commit: string): string {
  const v = UNSET.includes(version) ? "dev" : version.startsWith("v") ? version : `v${version}`;
  const c = UNSET.includes(commit) ? "" : ` · ${shortCommit(commit)}`;
  return `${v}${c}`;
}

// shortCommit abbreviates the daemon's full 40-char SHA, and ONLY that. The desktop shell already
// stamps a short SHA carrying a "-dirty" suffix (Makefile: `$(COMMIT)$(DIRTY)`), so truncating
// unconditionally would silently drop that marker and show a modified build as a clean one.
function shortCommit(commit: string): string {
  return /^[0-9a-f]{40}$/i.test(commit) ? commit.slice(0, 7) : commit;
}
