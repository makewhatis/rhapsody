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

## Two GitHub repository settings that are not in any config file

- **Allow update branch.** A `BEHIND` pull request is never merged on its existing approval. With
  this setting on, the daemon updates the branch itself (GitHub does not do it on its own) — which
  moves the head and arms a fresh review round, capped at 8 rounds per pull request, before the
  merge can happen again. With it off, the daemon refuses instead, and a human must update each
  pull request by hand. Reading the setting itself needs an admin token; if that read fails, the
  daemon declines the same as if the setting were off, even when it is actually on.
- **Automatically delete head branches.** The daemon's own `gh pr merge` calls never pass
  `--delete-branch`; cleaning up a merged branch is left entirely to this setting.

Neither is visible in `WORKFLOW.md`, `teams.yaml` or any daemon endpoint — an operator whose loop
half-works will not find either by reading config.

## Restarting or upgrading without losing in-flight work

**A drain lets a run in flight finish its current turn before a restart, instead of it being
killed and redone from scratch.** `POST /api/v1/drain` arms one; `/api/v1/state` carries a `drain`
key only while one is armed (a healthy daemon has no such key at all), the console shows a banner,
and the tray gets a drain action. While armed: a run in flight finishes its current turn and is not
re-dispatched, a due retry is parked rather than fired, and no new run starts. What is lost is the
agent's conversation thread only — the worktree, branch, commits, claim and retry state all
survive, and the next dispatch resumes as a fresh turn rather than from scratch. End one early with
`POST /api/v1/drain {"active": false}` or the console banner's Cancel drain. Default is **off**: a
daemon nobody drains behaves exactly as before.

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

1. **A pull request the daemon has already flagged as stuck.** `GET /api/v1/state`'s
   `review_divergence` key (and the per-project advisory on `/api/v1/projects`) names a watched
   pull request where nobody has made the move it's waiting on for 90+ minutes. This is a report,
   never an action — it never re-dispatches, arms or merges anything — so treat it as the first
   thing to check rather than something that will resolve itself.
2. **A request-changes verdict parked in the review state.** With `review.changes_state` unset
   this is the commonest cause — the agents are done and waiting on a human.
3. **Triage failing.** A `teams triage cycle outcome="tracker_failure"` line with
   `candidates_seen=0` means no candidate set was fetched, so nothing can dispatch. Usually the
   tracker.
4. **The daemon's own quota.** A failing lookup that retries without backoff can exhaust the
   tracker's hourly limit and starve dispatch — the failure then looks like idleness.
5. **The claim lock.** Under `claim_mode: assignee` a ticket that is not assigned to the account
   the daemon authenticates as is never a candidate, however it is labelled.

## Releasing Rhapsody itself

Only relevant if you are cutting Rhapsody releases; skip it if you are just running the daemon.

**A tag push triggers nothing.** `release.yml` fires on `push: branches: [main]` and on
`workflow_dispatch` — there is no `push: tags:` trigger.

**Release candidates** are manual, two steps. The **version** comes from release-please's open
release pull request title — `chore(main): release 0.3.6` names the base `0.3.6` — and the **RC
ordinal** from the tags already cut for that base:

```
git tag -l "v0.3.6*"
```

Empty output means `-rc.1`; a list ending at `v0.3.6-rc.1` means the next is `-rc.2`. Thirty
seconds that stops you colliding with or skipping an ordinal.

```
gh release create v0.3.6-rc.2 --prerelease --target <FULL 40-char sha> --title … --notes …
gh workflow run release.yml -f tag=v0.3.6-rc.2
```

`--target` needs the **full** sha; a short one is rejected as an invalid `target_commitish`.
Verify CI is green on that commit *before* tagging.

⚠️ **A notarize failure after Apple accepts is a re-dispatch, not a signing problem.** Apple's
`notarytool 1.1.2` intermittently stack-overflows — a SIGBUS inside CoreFoundation string
formatting — in `submit --wait`, hitting roughly 2 of 8 builds on 2026-09-19. The submission itself
has already succeeded: Apple has accepted the upload by the time the wait crashes. So a red build
job does **not** mean notarization failed, and the remedy is to **re-dispatch the workflow**, not to
hunt through certificates and entitlements for a signal chain that is not broken.

**`releases/latest` is the in-app updater's channel, and it is stable-only.** The updater polls
`releases/latest/download/latest.json`, and GitHub never points `latest` at a release flagged
`prerelease` — so `gh release create --prerelease` is what keeps every installed stable app from
being offered an RC. Check it after cutting one:

```
gh release view --repo makewhatis/rhapsody --json tagName,isPrerelease
```

It must still name the last **stable** tag. On 2026-09-19, immediately after `v0.3.6-rc.1` was
published, it read `v0.3.5` — the correct outcome.

**There are two Homebrew casks.** `rhapsody` tracks stable releases and `rhapsody@rc` tracks
prereleases, so a tester installs an RC through `rhapsody@rc`. The release workflow bumps them
independently — `homebrew-bump` on a real release, `homebrew-bump-rc` on a dispatch of a prerelease
tag — so a run that skips the `@rc` bump is *not* a failure when the build was a stable release, and
a run that skips the stable bump is not a failure for an RC. Both install the same
`/Applications/Rhapsody.app`, so at most one is installed at a time.

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
