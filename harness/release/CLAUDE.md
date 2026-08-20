# CLAUDE.md — harness/release

Three standalone bash scripts, no manifest (not a crate). For what's invoked where and what
`make test` does and doesn't run, see harness/CLAUDE.md's `release/` section — this file doesn't
restate that wiring, it goes one level deeper into each script's actual logic and the incident that
motivated it. This directory guards CI/release *conventions*; it is not a fixture-capture
(`harness/capture`) or daemon-boot (`harness/e2e`) concern.

## check-pr-title.sh — the actual gate

Validates a PR title as a conventional-commit subject release-please can parse. Three-way exit
contract, not the usual 0/1: **0** parses, **1** rejected (a bad title — the annotated failure),
**2** no argument at all (a mis-wired workflow, never to be confused with a rejected title). Callers
that only check "non-zero = fail" will conflate 1 and 2; `pr_title_test.sh` checks the exit code
value, not just pass/fail, for exactly this reason.

The regex is `^(type)(\(scope\))?!?: .+$` — lowercase type, optional parenthesised scope, optional
`!` breaking marker, colon, **exactly one space**, non-empty description. `TYPES` (build chore ci
deps docs feat fix perf refactor revert style test) is release-please's `DEFAULT_HEADINGS` set, not
an invented list — do not add or remove a type here without checking release-please's actual parser,
since this validator's whole job is to reject exactly what release-please would silently drop.

Why a title check exists at all: this repo squash-merges, so the PR title (+ " (#N)") *is* the only
commit subject release-please ever sees on `main`. STUDIO-406/#27 landed without a conventional type,
release-please logged "commit could not be parsed" and produced no release PR/tag/dmg/cask bump — and
the workflow stayed green. `check-pr-title.sh`'s stderr message exists to spell out that consequence
chain (release PR, tag, signed dmg, Homebrew cask, `.release-please-manifest.json`), not just the
grammar — `pr_title_test.sh` asserts on specific phrases in that message, so editing the wording needs
a matching edit to the phrase list it greps for.

## pr_title_test.sh — self-test of the validator

Plain accept/reject case table run against the real `check-pr-title.sh` (not a reimplementation).
Carries its **own** copy of `TYPES` — deliberate duplication so a drive-by edit that drops or invents
a type in the validator fails this test instead of shipping unnoticed. The `run()` helper is a plain
function (not `$(run ...)` command substitution) specifically so `out`/`status` assignments survive
into the caller's shell — a command substitution would run it in a subshell and lose them.

## version_test.sh — the same subshell trap, in a different shape

harness/CLAUDE.md's `release/` section already covers what this script drives and why it doesn't
reimplement the version-derivation logic; not repeated here. The `pr_title_test.sh` subshell-loses-
variables trap recurs in this script too, just structured differently: `new_repo` calls
`mktemp -d "$scratch/r.XXXXXX"` on **every** invocation, building a fresh throwaway repo each time
rather than reusing one and resetting it — because a counter or path mutated inside a function that
gets invoked via `$(...)` command substitution wouldn't persist across calls, there's no way to hand
a reusable scratch dir down through that pattern. The fresh-`mktemp`-per-call idiom is the workaround,
not a style choice; don't "simplify" it into a shared repo variable.

## pr-title.yml wiring (why it's a separate workflow, not a job in ci.yml)

- Triggers on `edited` (not just opened/reopened/synchronize) because a retitle doesn't move the head
  SHA — without `edited` a fixed (or newly broken) title would never re-report. That's also why it's
  its own workflow: adding `edited` to `ci.yml` would re-run the whole lint/test/web/boot-e2e/desktop
  matrix on every title tweak.
- `concurrency: cancel-in-progress` per PR number — several quick retitles queue several runs on the
  single self-hosted Mac runner; only the newest title's verdict matters.
- The PR title is passed to `check-pr-title.sh` via `env: PR_TITLE`, never interpolated into the
  `run:` block with `${{ }}` — a PR title is attacker-controlled text, so inlining it would be a shell-
  injection hole. Preserve this pattern if you touch the step.
- Job id is `pr-title`; it must be added to branch-protection required checks (needs admin) or a bad
  title still merges — this workflow reporting red is not itself enforced anywhere.

## Version-bump semantics (context for why the message says "patch", not "minor")

`release-please-config.json` is `simple`, pre-1.0, with `bump-minor-pre-major` +
`bump-patch-for-minor-pre-major`. While the repo is on 0.x, both `feat:` and `fix:` bump the PATCH;
only a breaking change (`!` or a `BREAKING CHANGE` footer) bumps the MINOR. `check-pr-title.sh`'s
help text encodes this pre-1.0 behavior directly — if the repo ever crosses 1.0, that text (and this
note) goes stale along with the config.
