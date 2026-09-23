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

### ⛔ `rhapsody:human` — the label that refuses dispatch (STUDIO-949)

Not a routing hint: a **refusal**. A ticket carrying `rhapsody:human` is never dispatched, with
or without Teams, and auto-promote never moves it to Todo. It is for work an agent cannot do —
console work in a web dashboard, a purchase on a physical device, a legal form — which was
previously communicated only in the title and enforced nowhere.

Enforced at `dispatch::eligible()`, the single chokepoint every dispatch path flows through,
plus `promote.rs` (so dag never promotes one into a Todo that will never run) and `triage.rs`
(so no identity and no manager turn is spent on it). The refusal is logged once per ticket, not
per tick, and the reconciliation sweep reports it as *held for a human* rather than as an
unexplained stall.

⚠️ **This is the fourth member of the label family and the only one that stops work.** When
planning, `rhapsody:@<name>`, `rhapsody:solo` and topic labels all decide WHO runs a ticket;
this one decides that NOBODY does. A ticket without it behaves byte-identically to before the
label existed.

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

## Which CLI a teammate runs on

A profile's **`harness:`** front-matter field picks the coding-agent backend that teammate's runs
use — `claude` or `opencode` today (`codex` is a recognized name with no runner yet and falls back
to the configured backend, with a warning). Empty — the shipped default for every built-in profile
— inherits the daemon's configured `agent.backend`, so an installation that never writes `harness:`
anywhere is unaffected. `rhapsodyd teams show <name>` renders the resolved value with its origin,
and marks it when this build can't actually run it. (If you point a profile at `opencode`, set
`opencode.command` to an **absolute path**: a bare `opencode` can resolve to a broken npm-global
install ahead of a working one on `PATH`, exiting 1 without running anything.)

⚠️ **A model name is meaningless without its harness.** `claude-opus-5` is a Claude model name;
handed to an opencode teammate it reaches that provider as an unrecognized model, and the run fails
outright rather than degrading gracefully — every review assigned to that teammate fails, and with
few reviewers configured a required verdict can become unobtainable. This is exactly why
`review.model`/`review.effort` below are scoped per harness rather than a single string.

**`provider:` names the credential-backed inference provider a run uses.** It is the same shape on a
profile, a roster identity, and the `manager:` block: an operator-chosen canonical provider id
(`fireworks`, `openrouter`, …) declared in `WORKFLOW.md`'s `providers:`. It is **never a
credential** — the value is a plain id, and anything that is not a canonical id is refused, so a
secret has no field to travel in. Empty inherits (profile → identity → the daemon's configured
selection). A provider reference the config does not define, or an explicit provider on the
`claude` harness, is a dispatch **refusal** — never a silent fallback to another provider or to
`agent.backend`.

The **manager** carries its own tuple (`manager.harness` / `manager.provider` / `manager.model`)
and never borrows a teammate's. An absent `manager.harness` means `claude` (independent of
`agent.backend`), and an explicit non-Claude manager harness must name both its provider and its
model.

## The room

An append-only JSONL log (`~/.rhapsody/teams/room/YYYY-MM-DD.jsonl`), single-writer (the
daemon). Nobody subscribes; everybody catches up at wake. **The room has NO dispatch power,
ever** — a post never starts a run, writes a label, or touches Linear. Work starts only from
Linear/GitHub.

Speakers: teammates (via `teams_post`), the **manager** (triage decisions, quorum
notifications), and the **operator** (you) — via the Teams panel's compose box or
`POST /api/v1/teams/room` with `{"body": "...", "refs": [...]}` (a write, so it needs the
`X-Rhapsody-Operator: 1` header — see operating.md, "Calling a write endpoint yourself").
`operator` and `manager`
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

**Who reviews can be pinned, not only arranged.** `review.required` (a list of roster names,
default empty) names identities selected on **every** pull request regardless of load or roster
order — the case a review-only or specialist teammate otherwise loses to a busy roster. Required
reviewers are chosen first and the remaining slots are filled by the usual least-loaded ranking,
so on both paths `reviewers` stays the **total**: `reviewers: 2` with one required means one
pinned plus one ranked. A pinned teammate is still never handed its own authored pull request. A
pin the daemon cannot use **degrades rather than blocking** — the round always proceeds with the
reviewers that can run: an off-roster name (a typo, or one written in the wrong case) is reported
at boot by name; a profile naming a harness this build cannot run drops the pin and keeps the
teammate a ranked candidate, since it reviews on `agent.backend`; and a ticketless `review.model`
refusal removes it from the round entirely with one warning. More pins than `reviewers` clamps to
the first `reviewers` in `required:` order, with one boot warning naming both numbers.

**Ticket state follows the verdict, when configured** (both empty-means-off):

- `review.done_state` — a merged PR moves its ticket to that terminal state.
- `review.changes_state` — a findings verdict moves its ticket **out** of the review state.
  An approval does not: approval is the pause in the re-review loop.

A findings verdict also posts a completion comment carrying the summon token, which reopens
the author's run. An approved one posts a deliberately **tokenless** note, so nothing wakes.

### The loop is bounded, and the bound is durable (STUDIO-956)

The edge trigger above answers "when does a round arm". It does **not** answer "how many". Two
mechanisms do:

- `REVIEW_ROUNDS_PER_PR_CAP` × reviewers — the long-standing hard cap. On reaching it the loop
  stops and logs at DEBUG; the sweep does not read that line, so a capped pull request used to
  page a human as *"nothing has reported it blocked"*.
- `review.adjudicate_after_rounds` — **the opt-in that makes the stop a decision.** At the
  configured round threshold the loop stops arming rounds and hands the pull request to the
  **manager** for one adjudication: **ship it** (the open findings do not block) or **escalate**
  (naming the specific findings, the round count and the head). `0` — the default — leaves the
  loop exactly as it was.

⚠️ **The round count and the manager's decision are DURABLE**, in `rhapsody_review_bound`
(one row per pull request, rehydrated at boot). They were in memory until 2026-09-21, which is
why five daemon restarts in one day turned a nominal 16-round cap into **46 review runs on one
pull request** — every restart refunded every budget. Do not assume a restart clears a spent
budget; it does not, by design.

⚠️ **A settled decision governs only the head it was made at** (STUDIO-971). When the author
pushes a content-changing commit the decision stops governing, the loop resumes and arms
**one** round at the new head, and the durable count keeps climbing rather than resetting. A
no-op rebase does not resume it (STUDIO-960's property). An `escalate` never resumes on the
author's own push — an escalation names a human as the next actor.

⚠️ **A `ship` verdict does not bypass the merge gate.** Approval-at-head, CI and the draft and
conflict gates all still apply, so a *ship* on a pull request with open findings stops the loop
and hands it to a human rather than merging it. In practice ship and escalate both end with a
person, differing in what they say about the findings.

**The operator's escape hatch is `POST /api/v1/reviews/clear`** — it forgets both halves (the
count and the decision) for one pull request, and the loop resumes from zero. It is the only
way to release a pull request the manager has settled, and the sweep's WARN names it.

### What a round costs, and how many can run

- **Rounds 2+ are delta reviews** (STUDIO-959): a reviewer is given the diff since *its own*
  `last_reviewed_sha` plus its own prior findings, not a cold read of the whole pull request. A
  reviewer new to a pull request, or one whose prior sha is not an ancestor of the head, still
  gets a full review and says which mode it took.
- **A verdict survives a head move that changed nothing** (STUDIO-960), so a rebase with no
  content change no longer arms a fresh round for every reviewer.
- `agent.max_concurrent_reviews` (STUDIO-950) — reviews draw from **their own** global pool
  instead of competing with implementations for `max_concurrent_agents`. Unset ⇒ the shared
  draw, byte-identical to before. Lives in `WORKFLOW.md`, so it hot-reloads.

### Two more things the review path now does on its own

- **A conflicted pull request routes back to its author** (STUDIO-961) — once per conflicted
  head, only on a settled `mergeStateStatus`, and it respects `rhapsody:human`. Requires
  `review.changes_state` to be set; without it the feature forms no plan at all.
- **A finished run's still-draft pull request pokes its author** (STUDIO-962) — once per head,
  never while the author's run is live, escalating to a human on two independent axes. The
  daemon never marks a pull request ready itself.

### Spend has a meter and a ceiling (STUDIO-957)

`GET /api/v1/metrics/providers` answers "tokens by provider by day" without a hand-written SQL
join. `budgets.<provider>.daily_tokens` in `WORKFLOW.md` refuses **new dispatch** on that
provider once the day's spend crosses it, leaving other providers running; `<= 0` or an absent
entry is unlimited, matching `max_concurrent`'s idiom. It hot-reloads.

⚠️ **It bounds new dispatch, never a live run** — a single runaway turn passes any daily
ceiling untouched. And the window is *local day*, so a trip clears at local midnight.

**A review run can use its own model, scoped per harness.** `review.model` / `review.effort`
override the routed reviewer's own profile for a review run specifically — unset, the default,
means the review inherits whatever model that reviewer's profile would have used anyway. Both are
maps from harness name to value (`review.model: { claude: ..., opencode: ... }`); a legacy bare
scalar (`review.model: claude-opus-5`) still parses and resolves against the installation's own
configured `agent.backend`. ⚠️ If a model is configured for some harness but not the one the routed
reviewer's runs actually use, the review is **refused** rather than sent to the wrong provider or
silently downgraded to a cheaper model — this is the trap above, closed by config. `review.effort`
in the same situation just inherits, since an effort value can't make a provider reject a model.

**A review run can also use its own provider, scoped per harness the same way.** `review.provider`
(`review.provider: { opencode: fireworks }`, or a legacy bare scalar) selects the provider a review
run uses, overriding the routed reviewer's own selection — unset means inherit. Like `review.model`,
a provider configured for a harness the routed reviewer does **not** use is **refused** rather than
applied to the wrong run, and every value must be a canonical provider id.

**A cleared review can merge itself.** `review.auto_merge` (default `false`) lets the daemon merge
a pull request once every reviewer has recorded a non-blocking verdict at its current head and CI
is green. It never arms GitHub's own auto-merge — it checks green once and merges immediately or
not at all — and it refuses a draft pull request outright. A branch that has fallen behind is never
merged on its existing approval either: if the repository allows it, the daemon updates the branch
itself, which moves the head and arms a fresh review round rather than merging (see `operating.md`
for the two GitHub repository settings this depends on).

**`auto_merge` can be scoped per project.** The top-level `teams.review.auto_merge` is the
installation-wide default; a top-level `projects:` entry overrides it for the Linear project
slugs it names, so one repo can be held for a human while a sibling still merges itself:

```yaml
review:
  auto_merge: true
projects:
  - slugs: [4f4a2350682f]   # the Linear project slugId, NOT its display name
    review:
      auto_merge: false      # this repo is merged by a human
```

`slugs:` takes the same values as `WORKFLOW.md`'s own `projects:` list — Linear's opaque
**`slugId` hex** — never the project's display name; the daemon warns at boot about any slug that
matches nothing. An unset override inherits the global value, both directions. When a project fans
out to several slugs that share one repo, their answers are ANDed: to **hold a merge back**, name
any one slug; to **opt in under a global `false`**, name **every** slug, or an unnamed sibling
inherits `false` and keeps the repo human-merged.

## Planning work for an installation (the operational rules)

- Under the shipped `claim_mode: assignee`, a ticket is claimable only when **all** of these
  hold: it is on the Linear team the daemon polls, in the project mapped to the target repo,
  in status Todo, **and assigned to the Linear account the daemon authenticates as**. The
  assignee half is the part people forget — it is the daemon's claim lock, not a routing hint,
  so an unassigned ticket is invisible however well it is labelled.
- To aim work at a teammate, add `rhapsody:@<name>` at filing time; to let the manager
  decide, leave it unlabeled (requires `labels+model`).
- ⚠️ **Labelled and Todo is not enough if the ticket is unassigned.** The assignee is the claim
  lock, so a ticket filed by a tool that does not set it sits in Todo looking ready and is never
  picked up. Set the assignee to the account the daemon authenticates as (`GET
  /api/v1/linear/identity`) whenever you file or move a ticket into a dispatchable state.
- To stop a ticket being dispatched at all, label it `rhapsody:human` — see the refusal above.
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
  `GET/POST /api/v1/teams/room`, `GET /api/v1/teams/recall?identity=&query=`,
  `GET /api/v1/runs/{id}/provenance` (the harness, model and provider a run actually dispatched
  with, and each value's origin — recorded once at dispatch, never re-derived from live config).
- **CLI**: `rhapsodyd teams show` (resolved roster), `rhapsodyd teams fork <profile>`.
- **Files**: `~/.rhapsody/teams.yaml`, `teams/room/*.jsonl`, `teams/banks/<name>/*.md`,
  `teams/profiles/`.
