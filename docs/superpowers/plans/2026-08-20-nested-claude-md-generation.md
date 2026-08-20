# Nested CLAUDE.md Generation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a single live Claude Code Workflow, composed in-session, that discovers which
directories in the rhapsody repo deserve their own `CLAUDE.md`, drafts each one, verifies it
independently, and leaves the repo with a reviewed, committed set of nested `CLAUDE.md` files up
to 5 levels deep.

**Architecture:** One Workflow invocation with two phases. Discovery (A1–A3): a `/deep-research`
pass over the tree, structured into a candidate list, dual-validated. Draft & Verify (B–D): for
each depth level 1 through the deepest candidate found, draft every candidate at that depth in
parallel, verify each by an independent agent with one bounded redraft, and only then move to the
next depth — because a deeper file's non-repetition check needs its parent's real, final content.

**Tech Stack:** Claude Code `Workflow` tool (`agent`/`parallel`/`pipeline`/`phase`/`log`), the
`deep-research` skill, git.

## Global Constraints

- Max directory depth for generated files: 5 (from repo root). — spec Goals
- File size: ~100 lines target, 200 lines hard ceiling. — spec Content methodology / Research
- Header: root file is exactly `# CLAUDE.md`; every nested file's first line is exactly
  `# CLAUDE.md — <path relative to repo root>`. — spec Content methodology
- Discovery excludes `target/`, `node_modules/`, `.git/`, any build output directory, and anything
  gitignored. — spec Phase A1
- No content already covered by an ancestor `CLAUDE.md` in a file's load chain may be repeated.
  — spec Content methodology
- No persisted `.claude/workflows/*.js` file. The script is authored directly in the `Workflow`
  tool's inline `script` parameter at call time — never `Write`n to a committed path, never
  invoked via `scriptPath` against a checked-in file. — spec "Why not a stored script"
- Phase D: on verification failure, exactly one bounded redraft attempt; a second failure is
  flagged for human review, not retried again and not shipped silently. — spec Phase D
- Phase C: depth *N+1* candidates only start once **every** depth-*N* candidate has been both
  drafted and verified (including any redraft) — not merely drafted. — spec Phase C
- Out of scope for this plan: the `bo-mode` repo and the maintenance/delta-finding skill. Both are
  explicit Non-goals in the spec, to be planned separately later.

---

### Task 1: Author the Workflow script

**Files:**
- None committed — this task produces reviewed script text held for Task 2's tool call, not a
  repo artifact.

**Interfaces:**
- Consumes: the approved spec at `docs/superpowers/specs/2026-08-20-nested-claude-md-design.md`.
- Produces: a finalized JavaScript string (shown in full in Step 1) that Task 2 passes verbatim as
  the `script` parameter to the `Workflow` tool call. Returns a `{ written: string[],
  needsHumanReview: { path, issues }[] }` report object when executed.

- [ ] **Step 1: Write the script**

This is the exact script text Task 2 will paste into the `Workflow` tool call's `script`
parameter — nothing further to invent at execution time beyond what's already here:

```js
export const meta = {
  name: 'nested-claude-md-generation',
  description: 'Discover, draft, and verify nested CLAUDE.md files across the rhapsody repo tree',
  phases: [
    { title: 'Discovery' },
    { title: 'Draft & Verify' },
  ],
}

const CANDIDATE_SCHEMA = {
  type: 'object',
  properties: {
    candidates: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          path: { type: 'string' },
          depth: { type: 'number' },
          rationale: { type: 'string' },
          nonObvious: { type: 'string' },
        },
        required: ['path', 'depth', 'rationale'],
      },
    },
  },
  required: ['candidates'],
}

const VALIDATION_SCHEMA = {
  type: 'object',
  properties: {
    flags: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          path: { type: 'string' },
          action: { type: 'string', enum: ['add', 'drop', 'note'] },
          reason: { type: 'string' },
        },
        required: ['path', 'action', 'reason'],
      },
    },
  },
  required: ['flags'],
}

const VERIFY_SCHEMA = {
  type: 'object',
  properties: {
    passed: { type: 'boolean' },
    issues: { type: 'array', items: { type: 'string' } },
  },
  required: ['passed', 'issues'],
}

phase('Discovery')

const researchBrief = `
Walk the rhapsody repo tree (repo root) to a maximum depth of 5 directory levels below
root. Exclude target/, node_modules/, .git/, any build output directory, and anything
gitignored. Use the root CLAUDE.md (repo root, already committed) as your starting map
of the repo's architecture.

Identify every directory that is a *meaningful unit* worth its own CLAUDE.md file:
- has its own manifest (Cargo.toml, package.json, pyproject.toml, etc.), OR
- is a clearly distinct operational subsystem even without its own manifest (e.g.
  desktop/src-tauri vs desktop/build are unrelated despite sharing a desktop/ parent).

Do NOT include every directory - only ones that are independently navigable units
distinct from their parent. A directory that's just a thin pass-through, or that an
ancestor CLAUDE.md already fully covers, should NOT be included.

For each candidate, report: the path (relative to repo root), its depth (count of path
segments from root), why it's distinct from its parent, and anything non-obvious about
it (pitfalls, build quirks, architecture that requires reading multiple files to
understand). Use the deep-research skill to decompose and parallelize this walk however
you judge best - report back your full findings as a narrative.
`

const discoveryNarrative = await agent(researchBrief, { phase: 'Discovery', label: 'deep-research' })

const structured = await agent(
  `Convert the following research narrative into a structured candidate list of
directories that should each get their own CLAUDE.md file. Each candidate needs:
path (relative to repo root), depth (integer, path segment count from root), rationale
(why this directory is a distinct unit), and nonObvious (what's non-obvious about it -
empty string if nothing). Do not add candidates the narrative doesn't support; do not
drop any it lists.

NARRATIVE:
${discoveryNarrative}`,
  { phase: 'Discovery', label: 'structure', schema: CANDIDATE_SCHEMA }
)

const validatorPrompt = (list) => `Independently validate this candidate list of
directories proposed for their own CLAUDE.md file, by checking it against the real
rhapsody repo tree (walk the filesystem yourself - do not trust the list blindly). Flag
any directory that should be added (a meaningful unit the list missed), dropped (not
actually distinct, or just noise), or noted (borderline, worth a second look during
drafting). Be specific about why.

CANDIDATE LIST:
${JSON.stringify(list, null, 2)}`

const validations = await parallel([
  () => agent(validatorPrompt(structured.candidates), { phase: 'Discovery', label: 'validate-1', schema: VALIDATION_SCHEMA }),
  () => agent(validatorPrompt(structured.candidates), { phase: 'Discovery', label: 'validate-2', schema: VALIDATION_SCHEMA }),
])

const allFlags = validations.filter(Boolean).flatMap(v => v.flags)
log(`Discovery validators raised ${allFlags.length} flag(s) across ${structured.candidates.length} candidates`)

let candidates = structured.candidates.slice()
for (const flag of allFlags) {
  if (flag.action === 'drop') {
    candidates = candidates.filter(c => c.path !== flag.path)
  } else if (flag.action === 'add' && !candidates.some(c => c.path === flag.path)) {
    candidates.push({ path: flag.path, depth: flag.path.split('/').length, rationale: flag.reason, nonObvious: '' })
  }
  // 'note' flags stay attached to the candidate for the Phase B drafter to weigh -
  // they don't change the structural candidate set.
}
candidates = candidates.filter(c => c.depth >= 1 && c.depth <= 5)

log(`Locked candidate set: ${candidates.length} directories`)

const report = { written: [], needsHumanReview: [] }

if (!candidates.length) {
  log('No candidates survived discovery/validation - nothing to draft.')
  return report
}

phase('Draft & Verify')

const maxDepth = Math.max(...candidates.map(c => c.depth))

for (let depth = 1; depth <= maxDepth; depth++) {
  const atDepth = candidates.filter(c => c.depth === depth)
  if (!atDepth.length) continue
  log(`Depth ${depth}: drafting and verifying ${atDepth.length} file(s)`)

  const results = await pipeline(
    atDepth,
    (candidate) => agent(
      `Draft a CLAUDE.md file for the directory "${candidate.path}" in the rhapsody repo.

Why this directory got its own file: ${candidate.rationale}
Non-obvious notes from discovery: ${candidate.nonObvious || '(none noted)'}

First, read the real CLAUDE.md chain from repo root down to this directory's parent
(every ancestor CLAUDE.md that already exists on disk) - never restate anything already
covered there. Then read this directory's actual contents.

Content rules:
- Include: build/test commands specific to this directory (only where they differ from
  an ancestor), architecture that requires reading multiple files in this directory to
  understand, pitfalls/conventions that differ from tool defaults.
- Exclude: anything derivable from the codebase (directory listings, dependency lists),
  generic development advice, anything already stated in an ancestor CLAUDE.md.
- Target ~100 lines, hard ceiling 200.
- First line must be exactly: # CLAUDE.md — ${candidate.path}

Write the file directly to ${candidate.path}/CLAUDE.md using your file tools. Return the
path you wrote and its line count.`,
      { phase: 'Draft & Verify', label: `draft:${candidate.path}` }
    ),
    async (_draftResult, candidate) => {
      const verify = () => agent(
        `Independently verify the CLAUDE.md file at "${candidate.path}/CLAUDE.md" (you did
not draft it). Read it, and read its real ancestor CLAUDE.md chain from repo root down to
its parent. Check: (1) no repetition of anything in an ancestor file, (2) line count
under 200 (target ~100), (3) every line is concrete/verifiable/non-obvious - nothing a
future Claude session could derive itself from the codebase (directory listings, dep
lists, generic advice), (4) first line is exactly "# CLAUDE.md — ${candidate.path}".
Report passed: true only if all four hold; otherwise passed: false with specific issues.`,
        { phase: 'Draft & Verify', label: `verify:${candidate.path}`, schema: VERIFY_SCHEMA }
      )

      let result = await verify()
      if (result && !result.passed) {
        await agent(
          `Redraft "${candidate.path}/CLAUDE.md" to fix these specific issues found by an
independent verifier: ${JSON.stringify(result.issues)}. Keep everything that was already
correct. Read the real ancestor CLAUDE.md chain again before rewriting. Overwrite the
file at ${candidate.path}/CLAUDE.md directly.`,
          { phase: 'Draft & Verify', label: `redraft:${candidate.path}` }
        )
        result = await verify()
      }
      return { path: candidate.path, passed: !!result?.passed, issues: result?.issues || [] }
    }
  )

  for (const r of results.filter(Boolean)) {
    if (r.passed) report.written.push(r.path)
    else report.needsHumanReview.push(r)
  }
}

log(`Done. ${report.written.length} file(s) written and verified, ${report.needsHumanReview.length} flagged for human review.`)
return report
```

- [ ] **Step 2: Review the script against a concrete checklist**

Confirm, line by line, before moving to Task 2:

- `meta` is a pure literal (no variables/spreads/calls) — matches the tool's hard requirement.
- `phase()` calls (`'Discovery'`, `'Draft & Verify'`) match `meta.phases` titles exactly.
- No `Date.now()`, `Math.random()`, or argless `new Date()` anywhere (none present above).
- Depth cap of 5 is enforced (`candidates.filter(c => c.depth >= 1 && c.depth <= 5)`).
- Discovery brief explicitly excludes `target/`, `node_modules/`, `.git/`, build output, and
  gitignored paths.
- Phase C's depth barrier is a real `for` loop with `await` per depth — not a single flat
  `parallel`/`pipeline` across all candidates — so depth *N+1* cannot start before depth *N*'s
  drafts and verifications (including redraft) finish.
- Phase D's redraft is bounded to exactly one attempt (`verify()` called at most twice per
  candidate); a second failure lands in `report.needsHumanReview`, never a silent third try.
- The verifier prompt is a separate `agent()` call from the drafter — never the same call.
- No step here writes this script to any path under `.claude/workflows/`.

- [ ] **Step 3: Mark this task done**

No test to run and nothing to commit — the deliverable is the reviewed script text above, held
for Task 2's tool call. Proceed to Task 2 once every checklist item in Step 2 is confirmed.

---

### Task 2: Execute the Workflow and integrate results

**Files:**
- Create: `<candidate_path>/CLAUDE.md` for every path in the run's `report.written` (exact set
  determined at runtime by Task 1's script — not enumerable in advance).
- Modify: none outside the newly created `CLAUDE.md` files.

**Interfaces:**
- Consumes: the exact script text from Task 1, Step 1, passed verbatim as the `Workflow` tool's
  `script` parameter (not `scriptPath` — no file to point at).
- Produces: the run's returned report shape, `{ written: string[], needsHumanReview: { path:
  string, issues: string[] }[] }`, used by this task's own Steps 3–5.

- [ ] **Step 1: Invoke the Workflow tool**

Call the `Workflow` tool with the `script` parameter set to the full text from Task 1, Step 1,
pasted directly into the call. Do not `Write` it to a file first and do not reference a
`scriptPath`. This call runs in the background and returns a task ID.

- [ ] **Step 2: Wait for completion and read the report**

Claude Code notifies on completion (or use `/workflows` to watch live progress; do not poll with
`ScheduleWakeup`). When the run finishes, read its returned value — the `report` object logged by
the script's final `log(...)` line and returned from the script. Note the counts of
`report.written` and `report.needsHumanReview`.

If the run errors out on a script bug rather than completing, fix the specific issue in the
script text and re-invoke `Workflow` with `resumeFromRunId` set to the prior run's ID — completed
`agent()` calls with unchanged prompts return cached results instantly, so only the broken step
and everything after it re-runs.

- [ ] **Step 3: Resolve every flagged file**

For each entry in `report.needsHumanReview`, read the file at `<path>/CLAUDE.md` and the
verifier's `issues` list. Either fix the file directly (following the same Content methodology
rules as the script's draft/verify prompts) or, if the right fix isn't obvious, leave it as-is and
list it explicitly in this task's final report to the user — never silently drop it from that
list.

- [ ] **Step 4: Spot-check the written set**

For at least one file per depth level present in `report.written`, open it and confirm: the first
line matches `# CLAUDE.md — <path>` exactly, the file is under the 200-line ceiling, and it does
not restate anything already present in its real ancestor `CLAUDE.md` chain (open the ancestors
alongside it to check).

- [ ] **Step 5: Commit**

```bash
git status
git add <each new CLAUDE.md path individually, not -A>
git commit -m "docs: generate nested CLAUDE.md files via discovery/draft/verify workflow"
```

- [ ] **Step 6: Report to the user**

Summarize: how many files were written and verified, their paths grouped by depth, which (if any)
remain flagged after Step 3 and why, and that branch `docs/claude-md-structure` now holds this
commit alongside the earlier root `CLAUDE.md` and design-spec commits, ready for their review or
push.
