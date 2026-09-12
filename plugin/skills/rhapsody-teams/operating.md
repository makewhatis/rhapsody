# Operating a Rhapsody installation

`SKILL.md` covers filing and routing work *into* Rhapsody. This file covers what the daemon
does with it once it is running: where a review verdict lives, who closes a ticket, the config
traps that cost real hours, how a release is actually cut, and what to check when nothing is
happening.

These are facts about Rhapsody, not a merge policy. How you review, how many gates you want and
who is allowed to merge are your team's decisions, and this skill deliberately does not have
one.

---

## Where a verdict actually lives

- `gh pr view --json comments` returns **issue comments only** and can mis-render. Use
  `gh api repos/{owner}/{repo}/issues/{n}/comments`.
- `gh pr review --approve` / `--request-changes` **errors** for a Rhapsody agent — `gh`
  authenticates as the pull request's own author, and GitHub refuses a self-review. Agent
  verdicts therefore always land as **comments**, never as GitHub review states.
- Cross-check `GET /api/v1/teams/room` and, under `review.mode: ticketless`, the
  `rhapsody_review_watch` row's `status`.

## Closing tickets

**`review.done_state` owns closure.** Do not hand-close a merged ticket; the daemon moves it,
and it has been observed doing so in 16–108 seconds. Hand-move only when you have *seen* the
`auto-done` line fail — it logs a `WARN` naming the error.

## The 60-second lie

The lifecycle memo has a **60s TTL**. For up to a minute after any tracker state change, the
console and `GET /api/v1/history/issues` serve the **previous** value. Before diagnosing "stuck
in review", check `auto-done` in the log and the ticket in the tracker — and check the clock.

Note also that a row's **UPDATED** column is its *run's* last activity, not its ticket's. A row
can legitimately read "4h ago" while its state changed seconds ago.

## Stopping a run

`POST /api/v1/runs/{id}/stop` moves the ticket to Backlog and kills the agent's whole process
group. On builds before the fix for that path it moved the ticket and killed nothing while
reporting success, so on an older daemon verify the agent actually stopped rather than trusting
the 200.

## Config traps

- **`teams.yaml` is boot-only.** `WORKFLOW.md` hot-reloads; `teams.yaml` does not. Every
  roster, `max_concurrent` or `review.mode` edit costs a daemon restart.
- ⚠️ **A rejected `teams.yaml` degrades to Teams-disabled** and reports Teams **off** — with
  **exit code 0 either way**. A config typo is indistinguishable from a deliberate Teams-off
  install. Verify with `rhapsodyd teams show <name>`: a resolved roster description means it
  parsed, `profile_unknown` means it did not.
- **Deleting `teams.yaml` destroys no history.** Memory and the room live in the sibling
  `~/.rhapsody/teams/` (`banks/`, `room/`), run history in `rhapsody.db`. But with no
  `teams.yaml` the room **renders empty while the files are intact** — rename rather than
  delete when testing.
- **`quorum.enabled: true` and `review.mode: ticketless` are mutually exclusive**, and
  validation hard-rejects the pair rather than guessing which one you meant.

## When the board looks idle

Check, in order:

1. **A request-changes verdict parked in the review state.** With `review.changes_state` unset
   this is the commonest cause — the agents are done and waiting on a human.
2. **Triage failing.** A `teams triage cycle outcome="tracker_failure"` line with
   `candidates_seen=0` means no candidate set was fetched, so nothing can dispatch. Usually the
   tracker.
3. **The daemon's own quota.** A failing lookup that retries without backoff can exhaust the
   tracker's hourly limit and starve dispatch — the failure then looks like idleness.
4. **The claim lock.** Under `claim_mode: assignee` a ticket that is not assigned to the account
   the daemon authenticates as is never a candidate, however it is labelled.

## Releasing Rhapsody itself

Only relevant if you are cutting Rhapsody releases; skip it if you are just running the daemon.

**A tag push triggers nothing.** `release.yml` fires on `push: branches: [main]` and on
`workflow_dispatch` — there is no `push: tags:` trigger.

**Release candidates** are manual, two steps:

```
gh release create v0.3.5-rc.1 --prerelease --target <FULL 40-char sha> --title … --notes …
gh workflow run release.yml -f tag=v0.3.5-rc.1
```

`--target` needs the **full** sha; a short one is rejected as an invalid `target_commitish`.
Verify CI is green on that commit *before* tagging.

**Promotion** is merging release-please's pull request: one run then cuts the tag, the GitHub
Release, the CHANGELOG, the signed + notarized dmg and the Homebrew cask bump.

⚠️ **Every release pull request arrives blocked.** Its workflow runs sit at `action_required`
because a bot opened it, so the required checks never start and the pull request reads
`BLOCKED` with *no checks listed at all*. Approve the runs first:

```
gh api -X POST repos/{owner}/{repo}/actions/runs/{run_id}/approve
```

## A design record is not current fact

A design document's body describes what was true when it was written — and so, for that matter,
does a skill. Before quoting one as live behaviour, check whether its slices shipped; a ticket's
own body will often name them. Re-fixing something already fixed is the failure this rule exists
to prevent, and it is easy. Read the code, or check the slice ticket's state.
