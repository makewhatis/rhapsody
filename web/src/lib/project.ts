// Pure presentation helpers for the Project column / drawer (multi-project routing,
// design §6). Kept in a .ts module (not a .tsx component) so they are unit-testable in
// this repo's node-environment Vitest setup (no DOM).

const DASH = "—";

// projectLabel renders a Linear project slug for a table cell / drawer row, falling back
// to an em dash in legacy single-project mode (empty/undefined slug).
export function projectLabel(slug: string | undefined): string {
  return slug && slug.trim() !== "" ? slug : DASH;
}

// repoShortName condenses a git remote URL to "owner/name" for compact display, handling
// both ssh (git@host:owner/name.git) and https (https://host/owner/name.git) forms. It
// falls back to the raw string for anything that does not parse, and a dash for empty.
export function repoShortName(repo: string | undefined): string {
  if (!repo || repo.trim() === "") return DASH;
  // Strip a trailing ".git", then take the last two path-ish segments.
  const noGit = repo.replace(/\.git$/, "");
  // ssh form: git@github.com:owner/name  -> split on ':' then '/'
  // https form: https://github.com/owner/name -> split on '/'
  const afterColon =
    noGit.includes(":") && !noGit.startsWith("http")
      ? noGit.slice(noGit.lastIndexOf(":") + 1)
      : noGit;
  const segs = afterColon
    .split("/")
    .filter((s) => s.length > 0 && !s.includes("github.com") && !s.startsWith("http"));
  if (segs.length >= 2) return `${segs[segs.length - 2]}/${segs[segs.length - 1]}`;
  return repo;
}
