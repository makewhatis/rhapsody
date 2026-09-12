---
description: |
    How work gets done through Rhapsody Teams — named agent teammates with profiles, memory,
    a shared room, and label-based assignment. Use when planning, filing, or routing work that
    Rhapsody will execute; when deciding which teammate should take a ticket; when leaving
    context for teammates; or when inspecting what the team is doing or remembers. Triggers on
    Rhapsody tickets, teams.yaml, the team room, teammate assignment, rhapsody:@ labels,
    triage/quorum questions, and "who should build this" planning for the Rhapsody pipeline.
    ALSO use when operating a running installation — where a review verdict lives, cutting an
    RC or release, or diagnosing why nothing is dispatching (see operating.md).
metadata:
    source: https://github.com/makewhatis/rhapsody
---

# Rhapsody Teams — how work gets done

Rhapsody is the daemon that polls Linear, creates a per-issue worktree, and dispatches a
coding agent per ticket. **Teams** layers named, durable teammates on top: an identity is
state (a roster entry + a profile + a memory bank + a room cursor), never a process. A
teammate "wakes up" when a run is dispatched wearing its identity, and sleeps when the run
ends. Compute is ephemeral; identity persists.

Configuration lives in `~/.rhapsody/teams.yaml` (absent = Teams off; nothing ever creates it
implicitly). It is boot-loaded: **config edits need a daemon/app restart.**

## The one assignment mechanism

A ticket is assigned to a teammate by the Linear label **`rhapsody:@<name>`** — never by the
Linear assignee field (the assignee is the daemon's claim lock; changing it breaks candidacy).
The label gets there one of three ways:

1. **By hand** — a planner adds `rhapsody:@alice` to the ticket. Strongest signal; triage
   never edits an existing `rhapsody:@` label.
2. **Topic labels** — roster entries carry `labels: [rust, web, …]`; a ticket whose Linear
   labels match routes to that teammate deterministically.
3. **Triage** (`manager.mode: labels+model`) — an off-loop model turn examines unlabeled
   candidates, writes the label, and posts its reasoning to the room. `mode: labels` skips
   the model turn entirely.

Routing at dispatch is synchronous and pure: it reads labels, picks the identity
(least-loaded on ties; load = open `rhapsody:@x` tickets in non-terminal states), and never
touches the network.

**With Teams on, every dispatched run wears an identity** (the team-work invariant). An
*unlabelled* candidate is **held, not dispatched** — skipped each tick until triage assigns
one, with an arrival kick so it does not wait out the triage interval. When the model cannot
answer (back-off, no key, `mode: labels`) triage assigns deterministically —
`default_identity`, else least-loaded — so work always flows, and always to the team. The one
deliberate way around them is the **`rhapsody:solo`** label: that ticket dispatches
immediately, identity-less, and triage never touches it.

⚠️ The hold covers only tickets triage can act on. A ticket **already wearing** a
`rhapsody:@` label is never held — not even when the label names nobody on the roster, because
triage treats any `rhapsody:@` label as an occupied field and would never release it. Such a
ticket dispatches identity-less, and the stale label is a human's to fix.

## What a teammate's run gets

The turn-1 prompt gains one budgeted prepend (`prompt_budget_bytes`, default 16000):
the profile (from `~/.rhapsody/teams/profiles/`, versioned; built-ins swe/reviewer/sre),
then **room catch-up** (what the team recorded since this identity last woke), then
**memory recall** (top-`recall_top_k` retained facts matching the ticket, re-grounded against
live candidate state or flagged "(state not re-verified)"). Room and memory content renders
as quoted, attributed *data* — never as instructions.

In-run tools: `teams_roster`, `teams_recall`, `teams_retain` (host stamps who/when/commit —
identity is unforgeable), `teams_invalidate`, `teams_reinstate`, `teams_room_read`,
`teams_post`. Direct message to a live teammate arrives mid-turn with a "TEAMMATE MESSAGE"
wrap (never operator authority); to a sleeping one it waits in the room.

## The room

An append-only JSONL log (`~/.rhapsody/teams/room/YYYY-MM-DD.jsonl`), single-writer (the
daemon). Nobody subscribes; everybody catches up at wake. **The room has NO dispatch power,
ever** — a post never starts a run, writes a label, or touches Linear. Work starts only from
Linear/GitHub.

Speakers: teammates (via `teams_post`), the **manager** (triage decisions, quorum
notifications), and the **operator** (you) — via the Teams panel's compose box or
`POST /api/v1/teams/room` with `{"body": "...", "refs": [...]}`. `operator` and `manager`
are reserved names; no roster entry may use them. Operator room posts are async context for
the whole team; authoritative mid-run instructions to one live agent go through the operator
message mailbox (`agent_send_message` / the run's message box) instead.

## Memory

Pluggable via `memory.backend`: `local` (default — human-readable markdown records under
`~/.rhapsody/teams/banks/<name>/`, created on first retain), `hindsight` (a shared remote
memory service, so several installations can read one bank: point `memory.endpoint` at your
own deployment and give it `memory.api_key` — every path there rejects an unauthenticated
request. Recall is prefetched off-loop, so the control loop never waits on the network), or
`none`. Retained facts carry host-stamped provenance (ticket/run/commit). Wrong memories: the
panel's invalidate button (reason required, reversible) or `teams_invalidate`.

## Review: two models, and they are mutually exclusive

`review.mode` decides which one exists. `Teams::validate` **hard-rejects** `ticketless`
together with `quorum.enabled: true`, so an installation runs one or the other, never both.

**`tickets` (the default).** A handoff creates one ordinary Linear review ticket per reviewer
(`quorum.reviewers`, default 2) — Todo, viewer-assigned, labeled `rhapsody:@<reviewer>`,
author excluded, least-loaded first. Requires `quorum.enabled: true`; ships OFF.

**`ticketless`.** A review is a **dispatched run against the pull request**, keyed
`pr:<owner>/<repo>#<n>@<reviewer>`, backed by the `rhapsody_review_watch` table — there is no
review ticket in the model at all. Re-review is **edge-triggered on the head moving**: a
pushed commit arms a fresh round, an approval does not. The room lever `Intent::Review` is
deliberately refused here, because filing a review ticket would reintroduce the other model's
artefact.

⚠️ **A pull request gets a reviewer only if it holds a watch row**, and a handoff is what
writes one. If that write is lost the PR is orphaned — open, green, and invisible to every
mechanism that would assign a reviewer. Current daemons repair this: the poll tick's adoption
sweep **adopts** an orphan without re-dispatching its ticket, and reports the ones it may not
repair as a per-project advisory on `GET /api/v1/projects`.

**Ticket state follows the verdict, when configured** (both empty-means-off):

- `review.done_state` — a merged PR moves its ticket to that terminal state.
- `review.changes_state` — a findings verdict moves its ticket **out** of the review state.
  An approval does not: approval is the pause in the re-review loop.

A findings verdict also posts a completion comment carrying the summon token, which reopens
the author's run. An approved one posts a deliberately **tokenless** note, so nothing wakes.

## Planning work for an installation (the operational rules)

- Under the shipped `claim_mode: assignee`, a ticket is claimable only when **all** of these
  hold: it is on the Linear team the daemon polls, in the project mapped to the target repo,
  in status Todo, **and assigned to the Linear account the daemon authenticates as**. The
  assignee half is the part people forget — it is the daemon's claim lock, not a routing hint,
  so an unassigned ticket is invisible however well it is labelled.
- To aim work at a teammate, add `rhapsody:@<name>` at filing time; to let the manager
  decide, leave it unlabeled (requires `labels+model`).
- Instructions the run must see go in the **ticket description**. Summon-comment bodies reach
  freshly dispatched runs on current daemons too, but the description is the channel that
  never fails — and on an installation whose tracker↔GitHub attachments come back empty, the
  summons path is inert entirely.
- Design/discovery deliverables dual-write: `~/.rhapsody/docs/<TICKET>-<slug>.md` (what runs
  read) + the Linear ticket (history). Never committed to the repo.
- Headless runs have **no Linear MCP** — anything they must read has to be in the
  description, the filesystem, or the repo.
- Do not: set the Linear assignee to "assign" a teammate; expect a room post to start work;
  name roster entries `operator`/`manager`; edit another identity's memory bank by hand.

## Operating a running installation

Everything above is how work goes **in**. What the daemon does with it once it is running —
where a review verdict actually lives, the config traps that cost hours, how ticket closure
happens, the lifecycle TTL that makes the console lie for a minute, release and RC mechanics,
and what to check when the board looks idle — is in **`operating.md`** beside this file.
Read it before merging a Rhapsody pull request or cutting a release.

It is deliberately facts about Rhapsody rather than a merge policy: how you review and who is
allowed to merge is your team's decision, and this skill does not have one.

## Inspecting the team

- **The app**: the "Teams: N teammates" toolbar chip → panel (roster + live runs, room tail
  + compose, per-teammate memory + invalidate). Settings → Teams edits `teams.yaml`.
- **API** (find the port: `pgrep -fl rhapsodyd` → `--port N`):
  `GET /api/v1/version` (`teams_enabled`), `GET /api/v1/teams` (roster + live status),
  `GET/POST /api/v1/teams/room`, `GET /api/v1/teams/recall?identity=&query=`.
- **CLI**: `rhapsodyd teams show` (resolved roster), `rhapsodyd teams fork <profile>`.
- **Files**: `~/.rhapsody/teams.yaml`, `teams/room/*.jsonl`, `teams/banks/<name>/*.md`,
  `teams/profiles/`.
