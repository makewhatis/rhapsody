# CLAUDE.md — web

See root CLAUDE.md's Architecture section for *why* this package has two consumers.
The mechanics of that split live here:

1. **Embedding into the daemon.** `vite build` writes into `crates/httpapi/web-dist/`
   (`vite.config.ts`'s `outDir`). The bundle itself is git-ignored; only
   `web-dist/.gitkeep` is committed so a clean, Node-less checkout still compiles.
   `vite.config.ts`'s `keepEmbedAnchor` plugin re-creates that `.gitkeep` after every
   build (Vite's `emptyOutDir` deletes it first) — don't remove that plugin or a local
   `npm run build` followed by `git add -A` will stage the anchor's deletion and break
   the clean-checkout build.
2. **Serving the desktop window.** `desktop/src-tauri/tauri.conf.json`'s
   `beforeDevCommand` runs `npm --prefix ../../web run build` before pointing
   `frontendDist` at that same output directory. There is no separate desktop bundle
   config.

Because of this, a component must work standing alone in a plain browser (dashboard,
`vite dev`, tests) **and** inside the Tauri webview. `src/lib/bindings.ts` is the seam:
every Tauri `invoke`/`listen` wrapper there starts with a `tauriAvailable()` guard
(checks `window.__TAURI_INTERNALS__`) and degrades to a safe no-op/null/`[]` when the
bridge is absent. Follow that pattern for any new desktop-only capability — don't let a
bare `invoke()` call escape into shared component code.

## Dev server / env

`npm run dev` proxies `/api` to the daemon at `SYMPHONY_API_URL` (default
`http://localhost:8799`, matching the Makefile's `PORT` default) — see the `server.proxy`
block in `vite.config.ts`. `SYMPHONY_API_URL` is a dev-only convenience var, not one of
the cross-process `SYMPHONY_*` contract vars from the root CLAUDE.md.

## Testing

`vitest.config.ts` defaults `environment` to `node` (fast, for `lib/*.test.ts` pure-logic
tests). Component tests that need a DOM opt in per-file with a
`// @vitest-environment jsdom` pragma at the top of the `.test.tsx` file — don't flip the
global default to `jsdom` for everyone's sake; most tests here are logic tests and don't
need it.

## Source layout notes

- `src/components/ui/` is a vendored/shadcn-style primitive set (ported from
  the Claude Design package's `ui.jsx`/`icons.jsx`) plus a few hand-rolled additions,
  re-exported from `ui/index.ts`. Treat it as generic infrastructure — pull from it,
  don't casually restyle individual primitives; a real design change starts in the
  design tokens (`src/index.css`), not in `ui/*.tsx`.
- `src/index.css` is the single source of truth for design tokens (the "Podium"
  warm-dark palette) consumed both by the Tailwind v4 `@theme` map and directly by the
  `ui/` primitives. The app is **dark-only, single accent, system fonts only** — there
  is no light-theme variant to keep in sync.
- `src/components/shell/AppShell.tsx` is the current top-level shell (replacing an older
  Live/History/Settings dashboard whose components still exist for the Runs re-skin to
  build on top of); the rest of `src/components/`, `src/hooks/`, and `src/lib/` are
  ordinary source organization.
- `#/demo` (`src/components/demo/`) is a verification-only primitive gallery, deliberately
  excluded from app nav and `React.lazy`-loaded from `App.tsx` so it never ships in the
  main bundle. Don't wire it into real navigation.

## Naming

`package.json`'s `name` is still `symphony-web`, predating the Rhapsody rename — the same
root-CLAUDE.md rule that keeps `SYMPHONY_*` env vars unrenamed covers it too. Don't rename
it as a drive-by.
