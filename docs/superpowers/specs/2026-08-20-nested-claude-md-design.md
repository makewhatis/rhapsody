# Nested CLAUDE.md Generation — Design

**Date:** 2026-08-20
**Status:** Approved for planning
**Repo:** `makewhatis/rhapsody` (local clone `~/repos/rhapsody`)

## Problem

Claude Code sessions in this repo have no persistent guidance below the root: a single
`CLAUDE.md` (just added via `/init`, see `docs/superpowers/specs/` sibling commit) can state
repo-wide facts, but has no way to carry directory-specific build quirks, non-obvious
architecture, or pitfalls without either growing unboundedly or staying too generic to be
useful. Anthropic's own guidance is that nested `CLAUDE.md` files solve exactly this — but
authoring them by hand, correctly, without repeating the root file, across a 12-crate + desktop
+ web tree, is the actual work.

## Goals

- Generate `CLAUDE.md` files in every directory (up to 5 levels deep) that constitutes a
  meaningful, independently-navigable unit — never every directory.
- Each file: concrete, verifiable, non-obvious content only. Never restate anything already
  covered by an ancestor `CLAUDE.md` in its load chain.
- Target ~100 lines per file, hard ceiling 200 (Anthropic's own limit — see Research).
- Do this as a **live-composed Workflow** run in-session against this repo now — not a stored
  `.js` script. The same phase design becomes the natural-language brief for a companion skill
  (a later, separate ticket) that re-runs it — composing a fresh script each time — for future
  maintenance passes.

## Non-goals

- The maintenance/delta-finding skill itself (detecting drift in already-generated files,
  updating only what changed). Related, explicitly deferred to its own design.
- The general-purpose `bo-mode` repo that will eventually host this methodology for other
  repos. Deferred to its own short follow-on design.
- A persisted `.claude/workflows/*.js` file. The script is authored fresh, in-session, by
  whichever agent invokes it — see "Why not a stored script" below.

---

## Research summary

**Anthropic's official docs (code.claude.com, current — no staleness risk, these are
continuously-updated live docs not a dated post):**

- *Loading mechanism* (`/docs/en/memory`): CLAUDE.md files above the working directory load in
  full at session start; files in subdirectories **below** it load lazily, only when Claude
  reads a file in that subdirectory. This is what makes deep, granular nesting cheap — a
  level-5 file costs nothing unless Claude is actually working there.
- *Size*: target under 200 lines per file; shorter measurably improves instruction adherence.
- *Content filter*: `/doctor` prunes anything Claude can derive from the codebase itself
  (directory layouts, dependency lists, architecture overviews) and keeps only pitfalls,
  rationale, and conventions that differ from tool defaults. Adopted directly as this project's
  litmus test for "does this line belong."
- *Canonical monorepo pattern* (`/docs/en/large-codebases`): root = repo-wide rules + a
  directory map; per-subdirectory = that area's own stack/commands only, never repeating root.
- `/init` with `CLAUDE_CODE_NEW_INIT=1` already does single-directory exploration + gap-filling
  + a reviewable propose-before-write flow, and (critically for future maintenance work)
  suggests improvements rather than overwriting an existing file. This project's phase design
  (below) mirrors `/init`'s own content heuristics for each individual file, and adds what
  `/init` doesn't do: parallel fan-out across a whole tree, and cross-directory
  non-repetition enforcement.

**Real-world 2026 sources** (dev.to, lowcode.agency, ayautomate.com — aggregator posts, lower
confidence than the above, but consistent with each other and with the official docs): confirm
the same two-tier lazy-loading pattern as the de facto standard; confirm skills nest the same
way (`.claude/skills/` discovered per-subdirectory, relevant to the future maintenance skill).
Length guidance varies 100–400 lines with no strong consensus — the official 200-line ceiling
above is treated as authoritative over these.

**ECC ("Everything Claude Code," `affaan-m/everything-claude-code`)** — real, actively forked
project bundling agents/skills/hooks/rules for Claude Code and other tools. Exact scale
(star count, agent/skill counts) varied wildly across sources in a pattern consistent with
SEO content-farm amplification — not cited as fact here. One concrete, useful detail: installed
as a plugin, its rules do **not** auto-distribute (must be copied into `.claude/rules/`
manually) — worth remembering for the `bo-mode` follow-on design.

---

## Content methodology (applies to every generated file, any depth)

- **Include**: build/test commands specific to that directory (only where they differ from an
  ancestor), architecture that requires reading multiple files in that directory to understand,
  pitfalls/conventions that differ from defaults.
- **Exclude**: anything derivable from the codebase (directory listings, dependency lists —
  the `/doctor` test), generic development advice, anything already stated in an ancestor
  `CLAUDE.md` in this file's load chain.
- **Size**: ~100 lines target, 200 hard ceiling.
- **Header**: every generated file (root included) opens with a one-line identifier —
  `# CLAUDE.md` for root (per `/init`'s own convention), `# CLAUDE.md — <path relative to
  repo root>` for every nested file.

## Workflow phase design

This is the phase structure an agent composes into a live `Workflow` script at invocation
time — described here as a brief, not as code, per the no-stored-script decision below.

### Phase A — Discovery

1. **A1 — deep-research pass.** One `/deep-research` invocation at the repo root. Brief: walk
   the tree to depth 5, excluding `target/`, `node_modules/`, `.git/`, build output, and
   anything gitignored, using the just-committed root `CLAUDE.md` as the starting map. Identify
   every directory that is a *meaningful unit* — has its own manifest (`Cargo.toml`,
   `package.json`, etc.) or is a clearly distinct operational subsystem (e.g. `desktop/src-tauri`
   vs `desktop/build` are unrelated even though both sit under `desktop/`) — and is worth its
   own `CLAUDE.md` rather than being covered by an ancestor. For each candidate: why it's
   distinct, and what's non-obvious about it. How deep-research decomposes/parallelizes that
   walk internally is its own concern, not specified here.
2. **A2 — structure the output.** One pass converts A1's narrative findings into a structured
   candidate list: `{path, depth, rationale}`.
3. **A3 — validate the candidate list.** Two independent agents re-check the structured list
   against the real repo tree before it's locked in — catching obvious omissions or
   over-inclusions in the *plan*, before any drafting effort is spent on it. Any concern either
   validator raises gets reconciled into the candidate list (add, drop, or note-for-B) before
   locking — there's no vote to win, since with two validators a disagreement has no majority;
   a flag from either one is enough to warrant a look.

### Phase B — Per-directory drafting

For each candidate from A3, in parallel within its depth level: an agent reads that directory's
contents plus the real (already-written, not draft) `CLAUDE.md` chain from root down to its
parent, then drafts content per the Content methodology above.

### Phase C — Depth-ordered execution

Phase B cannot run as one flat `parallel()` across every candidate: a depth-3 directory's
non-repetition check needs its depth-2 parent's *actual final* content — meaning verified, not
just drafted. Execution proceeds as a barrier per depth level that spans both B and D together:
all depth-1 candidates draft (B) and verify, including any redraft (D), before depth-2 starts;
depth-2 then reads depth-1's post-verification content, and so on. A depth-1 file still under
redraft blocks depth-2 candidates that would read it, not just the ones reading a sibling.

### Phase D — Independent verification

A separate agent per drafted file — never the same agent that drafted it — re-checks: no
repetition against the real ancestor chain, the line-count ceiling, the `/doctor`-style
concreteness bar, and header-convention compliance. On failure, specific findings feed back into
one bounded redraft attempt; a file failing a second time is flagged for human review rather
than shipped silently or retried indefinitely.

## Why not a stored script

A persisted `.claude/workflows/*.js` would hardcode this repo's current directory list the same
way a stale `CLAUDE.md` would go stale — new crates, removed subsystems, or restructured
folders would silently fall out of sync with a frozen script. Composing the script fresh at
invocation time — guided by this phase design as a natural-language brief, not by reading a
stored file — means every run (this one, and future maintenance runs via the companion skill)
starts from the tree as it actually is.

## Testing / acceptance

- The live run against this repo (next step, this session) is itself the acceptance test: every
  generated file must pass Phase D's verification, and a final human read-through (you) checks
  the overall set for anything the automated verify pass wouldn't catch — tone, whether the
  chosen candidate set actually feels right.
- No golden-fixture testing applies here (this isn't porting crate code); correctness is judged
  against the Content methodology section above.

## Risks

| Risk | Mitigation |
|---|---|
| Deep-research under- or over-splits the discovery walk | A2/A3 catch this before drafting effort is spent |
| A drafting agent restates ancestor content anyway | Phase D's independent verifier is a different agent than the drafter, checking against real (not self-reported) ancestor content |
| Redraft loop never converges | Bounded to one retry; second failure flags for human review instead of looping |
| Candidate set feels wrong only in aggregate (not any single file's fault) | Final human read-through after the run, not just per-file automated verification |
