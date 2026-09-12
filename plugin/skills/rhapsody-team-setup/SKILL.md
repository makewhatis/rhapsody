---
description: |
    Interactively set up or extend a Rhapsody team — choose the teammates a codebase needs,
    write the profile that gives a specialist its initial context, and land a valid
    teams.yaml. Use when someone wants a team for a specific repo or app, wants to add a
    teammate, wants a review-only or specialist teammate rather than a generalist, or asks
    "what context should this agent have". Triggers on setting up Rhapsody Teams, creating a
    roster, writing or forking a profile, extends: swe, ~/.rhapsody/teams/profiles/, and
    onboarding a team onto a new codebase.
metadata:
    source: https://github.com/makewhatis/rhapsody
---

# Setting up a Rhapsody team

A team is two things on disk:

- **`~/.rhapsody/teams.yaml`** — the roster: who exists, what profile each wears, what they
  route on, how much they take at once.
- **`~/.rhapsody/teams/profiles/<name>.md`** — the **role** each teammate wears. This is where
  a codebase's context lives, and it is the part worth spending time on.

> A profile is *"a document you would hand to a new hire: front matter plus a prompt body,
> with no person's name in it, no memory-bank id and no history."* Identities merely *name*
> the profile they wear — which is what stops a profile collapsing into a rename of
> `.claude/agents/`.

**Never write either file from an interview without showing it first.** See *The gate* below.

## Step 1 — what kind of teammates does this codebase need?

Ask, don't assume. The useful shapes:

| Shape | Roster | When |
|---|---|---|
| **Generalist pair** | two `profile: swe` | Default. Two teammates means reviews have somebody to go to who is not the author. |
| **Specialist + generalist** | one custom profile, one `swe` | A codebase with real domain rules — deploy semantics, a frozen vocabulary, a parity contract. |
| **Dedicated reviewer** | `profile: reviewer` | The roster is big enough that review is its own role. |
| **Ops teammate** | `profile: sre` | Infra repos. |

⚠️ **One teammate is a degenerate team.** Reviewers exclude the author, so a single-teammate
roster can never review its own work — the handoff finds nobody. Two is the practical floor.

Built-ins available today: **`swe`**, **`reviewer`**, **`sre`** (each versioned).

## Step 2 — write the specialist's profile

This is the actual deliverable. The built-in already carries generic engineering discipline —
read before you write, match the code you found, test first, errors are values, stay inside
the ticket, evidence before claims, retain what you learned. **Do not restate any of it.**

A custom profile adds only what is true *of this codebase* and would otherwise be rediscovered
(badly) on every ticket:

- **What the product actually is**, in one or two sentences a new hire would need.
- **The stack and its seams** — which language owns what, where the boundaries are.
- **Deploy and release semantics** — especially where they are surprising.
- **Vocabulary and identifier freezes** — renamed concepts where some identifiers deliberately
  did *not* move. This is the single most expensive thing for an agent to get wrong.
- **The hazards** — what looks like a bug and is deliberate; what a newcomer always breaks.
- **Where the authoritative docs are** — `CLAUDE.md`, a design record, the reference impl.

Leave out: anything in the built-in, anything the ticket will say, anything that changes
weekly (it will go stale and a stale profile is worse than a thin one).

### The file

```markdown
---
extends: swe
---

{{ base }}

## Working on <product>

<the context above, in prose>
```

- **`extends: swe`** — track the built-in. Fields you never set improve on upgrade; fields you
  set stay exactly as written. **This is usually the right answer — write it explicitly.**
- `extends: swe@2` — pin a version. Upgrades do not move it, and resolution *reports* the
  drift rather than silently merging.
- `extends: none` — a full fork; Rhapsody contributes nothing.
- ⚠️ **Omitting `extends:` is a fork, not an implicit `swe`.** An absent key parses exactly
  like `none`, so a profile that forgets it silently ships without the built-in's engineering
  discipline — and `{{ base }}` splices in nothing. "I never touched this" and "I own this"
  are meant to be distinguishable in the file itself, so Rhapsody never guesses a base.
- **`{{ base }}`** splices the built-in's body at that point. It is a literal token splice,
  **not Liquid** — profile bodies are never template-rendered, so a stray `{{` elsewhere is
  just text, and there is no interpolation surface.

Optional front matter, all inherited from the built-in when omitted: `model`, `effort`,
`capabilities`, `tools`. (`tools` is parsed and reported by `teams show`, but does not yet
gate anything — do not rely on it to restrict a teammate.)

⚠️ **Rhapsody only ever *reads* profile files.** The one exception in the whole feature is
`rhapsodyd teams fork <profile>`, which materialises a resolved copy on an explicit command.
So writing a profile is your job, not the daemon's — and nothing will overwrite it.

## Step 3 — the roster

```yaml
enabled: true
roster:
  - name: alice          # label-safe: it becomes rhapsody:@alice
    profile: myapp-swe   # a built-in name, or your profile's filename stem
    labels: [server, go] # deterministic routing: matches the ticket's Linear labels
    bank: ''             # empty ⇒ <memory.bank_prefix><name>
    max_concurrent: 0    # 0 ⇒ unlimited
```

- **`labels`** is how a ticket reaches a teammate without the manager thinking about it. Leave
  empty to route by `rhapsody:@` label or triage only.
- **`max_concurrent: 0`** is unlimited and is the shipped default. Set `1` if you want a
  teammate to finish one thing before starting another.
- `operator` and `manager` are **reserved** — no roster entry may use either.

## The gate — how to land it safely

⚠️ Four properties of this file make "just write it" the wrong move:

1. **`teams.yaml` is boot-only.** `WORKFLOW.md` hot-reloads; this does not. Every edit costs a
   daemon restart.
2. **A rejected file degrades to Teams-off** — and reports Teams off, **with exit code 0
   either way**. A typo is indistinguishable from a deliberate Teams-off install.
3. **Enabling Teams changes dispatch.** With Teams on, an unmatched candidate is *held* until
   triage assigns it (the team-work invariant), rather than dispatching identity-less.
4. **`quorum.enabled: true` and `review.mode: ticketless` are mutually exclusive** and
   validation hard-rejects the pair.

So:

1. **Show the proposed `teams.yaml` and each profile body in full, and get a yes.**
2. Back up anything you are replacing (`cp teams.yaml teams.yaml.bak-<why>`).
3. Write the files.
4. **Restart the daemon** — the app's tray Quit drains cleanly, then relaunch.
5. **Verify it parsed**, which is the step people skip:

```
rhapsodyd teams show <name>     # a resolved roster ⇒ parsed
                                # profile_unknown   ⇒ it did NOT parse
curl -s localhost:$PORT/api/v1/version   # "teams_enabled": true
```

Absence of a `teams triage task started` line in the log after a restart means Teams did not
come up, whatever the file looks like.

## There is also a UI

**Settings → Teams** has a working editor — "Create teams.yaml…", add-teammate, live
validation that disables Save rather than inviting a doomed round-trip. Prefer it for small
edits, and say so rather than hand-editing YAML on someone's behalf. This skill earns its keep
on the part the UI cannot do: **writing the profile body.**

## Worked shape — a specialist for an app

Two teammates, one carrying the codebase's context:

```yaml
enabled: true
manager: { mode: labels+model }
review: { mode: ticketless, reviewers: 1, done_state: Done }
roster:
  - { name: alice, profile: myapp-swe, labels: [], bank: '', max_concurrent: 0 }
  - { name: jimmy, profile: myapp-swe, labels: [], bank: '', max_concurrent: 0 }
```

Both wear the specialist profile — the context is about the *codebase*, not the person, so
there is rarely a reason for one teammate to know less. Differentiate with `labels` when the
repo has genuinely separate areas, not by giving one a thinner profile.

## What not to do

- Don't invent front-matter fields. The set is `extends`, `model`, `effort`, `capabilities`,
  `tools` — everything else belongs in the body.
- Don't put a person's name, a bank id, or history in a profile. It is a role.
- Don't restate the built-in's engineering discipline; `{{ base }}` already includes it.
- Don't set the Linear **assignee** to route work — that field is the daemon's claim lock.
- Don't create a one-teammate roster and expect reviews to fire.
