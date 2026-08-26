# 0001. Rhapsody Teams — named agents with shared profiles, under a manager

- **Date:** 2026-08-26
- **Status:** Accepted — design only. **Nothing here is implemented.** No config field
  exists, no profile ships, no bank is created, no dispatch path changed.
- **Ticket:** [STUDIO-572](https://linear.app/studio49/issue/STUDIO-572/design-rhapsody-teams-an-optional-feature-where-named-agents-with)
- **Builds on:** [STUDIO-569](https://linear.app/studio49/issue/STUDIO-569/discovery-persistent-named-agents-an-identity-and-memory-layer-over)
  (studio-infra PR #71, `docs/decisions/0001-persistent-named-agents.md`). Its measurements,
  its retention policy and its cost model are consumed here, not re-derived. Where this
  record departs from it, the departure is named and justified.

---

## The answer, first

**Ship T1–T3 and stop.** That is: the off-by-default toggle, profiles as editable
artifacts, and *deterministic* routing that prepends a teammate identity to the run prompt.
Named agents, real role templates, assignment you can read off a Linear label, visible in the
run timeline — **zero extra model turns per ticket, zero cloud dependency, zero bytes of
change to any existing golden.**

Everything the vision calls for beyond that — memory, inter-agent messaging, a model that
decides who takes a ticket — is either blocked on infrastructure that does not exist yet, or
is unjustified until T3 has been used for a week and produced a number. The slice plan says
which is which.

Two findings from the code shape the whole design and are worth stating before anything else:

- **Rhapsody's SQLite schema cannot change.** `crates/store/src/sqlite.rs` asserts that opening
  a fresh DB and applying the migrations produces a dump *byte-identical* to
  `harness/fixtures/schema.sql`, and `crates/harness-fixtures/src/lib.rs` separately asserts
  that file holds *exactly* six tables. Teams therefore adds no column to `runs`, no seventh
  table, and no row shape to the parity store. Its state is a sidecar.
- **Everything Teams needs for messaging already exists and is already durable.**
  `symphony_send_message` → `POST /api/v1/runs/{id}/message` → the `run_messages` table, with a
  bounded 16-deep mailbox and a `not_running` error for a run that is not live. That *is* the
  state-aware handshake the ticket asks us to steal from herdr, minus the resident PTY pane,
  and it leaves a durable trace, which is exactly what herdr does not.

---

## 1. The three concepts, and how they are kept apart

| Concept | Lives in | Cardinality | Owned by |
| --- | --- | --- | --- |
| **Profile** | `~/.rhapsody/teams/profiles/<name>.md` | shared | ships as a default, then the user |
| **Identity** | a roster entry in `~/.rhapsody/teams.yaml` + a memory bank + `assignments` rows | one per teammate | the user |
| **Manager** | a pure function in `crates/orchestrator`, called once inside `dispatch_issue` | one per team, and it is not a component | Rhapsody |

The split is enforced by putting the three in **three different kinds of storage**, so
collapsing them is a refactor rather than a typo:

- A profile is a **file with a prompt body**. It has no name of a person in it, no bank id, no
  history. It is a document you would hand to a new hire.
- An identity is a **short structured record** — `name`, `profile`, `labels`, `bank` — with no
  prompt text at all. Alice and Bob are four lines each and both say `profile: swe`.
- The manager **stores nothing**. See §3; that is the whole of its design.

The failure the ticket names — "collapsing profile into identity is the mistake that makes the
whole thing a rename of `.claude/agents/`" — is prevented structurally: an identity record has
nowhere to put a prompt, because the roster schema has no prompt field. If you want Alice to
behave differently from Bob, you either give her a different profile or you give her an
overlay, and the overlay is a *profile* file that other identities can also wear.

Per the ticket, `booch/.claude/agents/` is not the model and is not cited as precedent. The
idiom this design does follow is Rhapsody's own: **front matter plus a prompt body**, which is
what `WORKFLOW.md` already is and what `crates/config/src/workflow.rs` already parses.

---

## 2. Config, and the off switch

### 2.1 Where it lives, and why not in `WORKFLOW.md`

The whole feature lives behind one toggle owned by `crates/config` — but **not as a
front-matter field in `WORKFLOW.md`**, and the reason is a real hazard rather than taste:

`crates/config/src/encode.rs` rebuilds the front matter *from the typed `Raw` mirror* and
prunes it. Any key not modelled in `Raw` is silently dropped the first time the dashboard
config editor saves. A `teams:` block hand-written into `WORKFLOW.md` would therefore survive
until the user touched the Settings screen and then vanish without an error. Modelling it in
`Raw` instead is possible — `capabilities` did it — but it costs a `prune_empty`-correct field,
a decode/encode round-trip test, and a permanent risk of a stray key reaching
`effective_json` and breaking the `config/*.json` goldens.

So Teams config is **a file of its own**, following the `capabilities.yaml` precedent
(`crates/config/src/capabilities.rs`) that BO-11 already established for user-editable,
non-parity data:

```
~/.rhapsody/teams.yaml              the toggle, the roster, the manager, the memory backend
~/.rhapsody/teams/profiles/<name>.md   one profile per file: front matter + prompt body
~/.rhapsody/teams/teams.db          the assignments sidecar (NOT rhapsody.db)
```

One deliberate divergence from `capabilities.rs`: **`teams.yaml` is not seeded on first read.**
`load_or_seed` writes `capabilities.yaml` the first time the daemon reads it, which is
harmless there and would be a behaviour change here — a disabled feature must not create a
file. `teams.yaml` is created only by an explicit enable (Settings toggle, or
`rhapsody teams init`). **An absent file is the off state, and it is the shipped state.**

### 2.2 The schema

```yaml
# ~/.rhapsody/teams.yaml   — absent by default; absent ≡ enabled: false
enabled: false

manager:
  mode: labels             # off | labels | labels+model      (default: labels)
  default_identity: ""     # who takes a ticket nothing matches; empty ⇒ run without an identity
  model: ""                # consulted ONLY in labels+model, and only on a Tier-1 miss
  max_tokens: 4000         # hard cap on the arbitration turn
  timeout_ms: 5000         # exceeded ⇒ fall back to the deterministic answer

memory:
  backend: none            # none | hindsight                 (default: none — see §5.4)
  endpoint: ""             # Hindsight MCP base, e.g. https://hindsight.<tailnet>.ts.net/mcp/
  bank_prefix: "agent-"    # STUDIO-569: bank id `agent-<name>`, `default` tenant, same instance
  recall_top_k: 8

roster:
  - name: alice
    profile: swe
    labels: [rust, config, parity]   # what the deterministic router matches
    bank: ""                         # empty ⇒ `<bank_prefix><name>`
    max_concurrent: 0                # 0 ⇒ unlimited (see §3.4)
  - name: bob
    profile: swe
    labels: [web, ui]
  - name: jimmy
    profile: reviewer
    labels: [review]
```

A profile file:

```markdown
---
extends: swe            # `swe` (track latest) | `swe@3` (pin) | `none` (fork)
model: ""               # empty ⇒ inherit the built-in, which inherits Rhapsody's config
effort: ""
capabilities: [code-review, test-coverage]   # names from the BO-11 registry — reused, not reinvented
tools: []               # allowlist; empty ⇒ inherit
---

{{ base }}

Extra house rules for this team: never touch `harness/fixtures/` without saying why in the PR body.
```

`{{ base }}` interpolates the built-in prompt body, so the common case — "the shipped SWE prompt
plus two sentences" — is a two-line file. An empty body means "no change to the prompt."

### 2.3 The `team_id` collision — read this before writing any code

`runs.team_id` already exists and is **the Linear team id** (INF-223), used by
`crates/orchestrator/src/claim.rs` to call `move_issue_state`. It has nothing to do with this
feature. Rhapsody Teams uses **`identity`** for a teammate and **`roster`** for the set. The
word `team` appears in this feature only in the file name. Reusing `team_id` for a roster would
break state promotion in a way that looks like a Linear outage.

### 2.4 Proving it is inert

"Off costs nothing" is a claim with nine touchpoints, each with a mechanism and a test:

| # | Touchpoint | Off-behaviour | What proves it |
| --- | --- | --- | --- |
| 1 | `~/.rhapsody/teams.yaml` | absent ⇒ `Teams::disabled()`; never seeded | unit test: absent path ⇒ disabled, no file created |
| 2 | `WORKFLOW.md` front matter | no new `Raw` field at all | existing `encode`/`decode` round-trip tests unchanged |
| 3 | `GET /api/v1/config`, `/projects` | no new key in `effective_json` | `api/config.json`, `api/projects.json` goldens unchanged |
| 4 | `rhapsody.db` | no column, no table; sidecar only | `schema_matches_committed_golden` + `canary_schema_has_all_tables` unchanged |
| 5 | Turn-1 prompt | `teammate_section` empty ⇒ the `if !x.is_empty()` guard in `build_turn_prompt` skips it | prompt byte-identical; the exact mechanism BO-12 proved for `capabilities_section` |
| 6 | Dispatch | `route()` not called; the new `WorkerDeps` field is `String::new()` | same shape BO-12 used; existing dispatch tests unchanged |
| 7 | MCP | `teams_*` routes **removed from the router** when off | `list_tools` byte-identical; the `allow_handoff` mechanism at `crates/mcp/src/server.rs:69–80` |
| 8 | Network | `memory.backend` never read; no client constructed; no DNS | nothing to test — there is no code path |
| 9 | Selection, concurrency, claim | **no Teams code above the dispatch boundary, ever** | `select.rs`, `eligible`, `concurrency.rs`, `claim.rs` appear in zero Teams diffs |

Row 9 is the invariant, not a detail. Teams is strictly *downstream* of the decision to
dispatch. It never sees an issue Rhapsody was not already going to run.

**The acceptance criterion for the inertness slice is stronger than any assertion it could
add: the T1 PR edits zero existing goldens and zero existing tests.** If Teams is genuinely
inert, nothing that exists today needs to change to stay green. If a golden has to move, the
design is wrong, not the golden.

---

## 3. The manager routes; it does not queue

### 3.1 It is a function, not a component

```rust
/// Called once, inside dispatch_issue, AFTER the issue has been selected and a slot taken.
fn route(roster: &Roster, iss: &Issue, load: &LoadSnapshot) -> Routed
```

Every property that turns a router into a second EM is absent by construction:

- **It has no store.** It cannot persist an intention. The `assignments` row is written *after*
  dispatch and is past tense — "run 412 was Alice" — never "Alice should get STUDIO-x". A row
  exists only if a run exists.
- **It cannot enlarge or reorder the work.** It runs after `select_dispatch` / `eligible` /
  `global_slots`. Delete the router and the identical set of issues dispatches in the identical
  order; only `identity` is unset.
- **It cannot say "not yet."** `Routed` is `{ identity: Option<String>, reason: RouteReason }`.
  There is **no `Defer`, no `Queue`, no `Retry` variant, and there never may be.** That missing
  variant is the entire defence, and it is checkable in code review by reading one enum.
- **It holds no idea of what is in flight.** `LoadSnapshot` is *derived at call time* from the
  same `running` map `global_slots` reads. It is a read of Rhapsody's state, never a copy.

STUDIO-297 produced duplicate PRs because a second source of *assignment* competed with Linear.
Here Linear still assigns: the issue is in an active state, assigned or claimed exactly as
today, and two daemons with Teams on and the same roster still contend through the existing
`claim_mode: pool` protocol. Teams adds no second claim path, no second poller, and no second
notion of "mine". It only labels *who runs* something Linear already handed over.

This also settles the vision's "a manager who knows the team and hands each ticket to the right
member": the manager knows the team, and hands over the ticket *Linear already chose*. It does
not choose tickets.

### 3.2 How it decides — three tiers, cheapest first

**Tier 0 — explicit (0 turns).** A Linear label `rhapsody:@alice` names the identity outright.
This reuses the `rhapsody:*` label convention BO-12 already established for capabilities, so it
is one more prefix in a namespace that exists. Deterministic, and auditable *in Linear*, which
matters: the assignment is visible where the work lives.

**Tier 1 — labels → identity (0 turns).** Each roster entry declares `labels:`. Score each
identity by `|ticket.labels ∩ identity.labels|`; highest wins. Ties break by (a) fewest live
runs from `LoadSnapshot`, then (b) roster order — so the tiebreak is total and the function is
deterministic given the same inputs. Pure, unit-testable, no I/O.

**Tier 2 — model arbitration (1 turn), only on a Tier-1 score of zero or an unbroken tie.**
Off by default. One cheap turn, given: the roster (name, profile one-liner, labels, live-run
count), the ticket title + labels + the head of its description, and — when memory is on — a
short recall digest per candidate (`types: ["experience"]`, ≤5 facts). Returns
`{identity, reason, confidence}`.

### 3.3 What routing costs

| Mode | Extra model turns per ticket | Extra latency before work starts | New failure mode on the dispatch path |
| --- | --- | --- | --- |
| `off` | 0 | 0 | none |
| `labels` **(default)** | 0 | one `HashSet` intersection | none |
| `labels+model` | 0 on a Tier-0/1 hit, 1 on a miss | ~1–3 s on a miss | one, and it is bounded |

The Tier-2 prompt is roughly 2.5k input (10 identities × ~40 tokens, a ~500-token ticket head,
10 × 5 × ~30 tokens of digest) and ~100 output. At Haiku-class rates that is a fraction of a
cent, and **the token cost is not the argument** — the arguments are the seconds of latency in
front of every unmatched ticket, and the fact that it introduces a model call into a code path
that today cannot fail for a model reason.

**So: yes, a cheap deterministic path handles the obvious cases, and it is the default.** The
model is opt-in, it is only ever consulted on a miss, it is capped by `max_tokens` and
`timeout_ms`, and on any error, timeout, or unparseable answer it falls back to the Tier-1
result — never to an exception, and never to a retry.

### 3.4 When it is wrong, when the teammate is busy, when nobody fits

**When it is wrong: the ticket is the correction channel, and the manager does not learn.** A
misroute produces a normal Rhapsody run: the work happens, the PR is reviewed, and a human
either accepts it or relabels and re-summons with `@symphony`. Rhapsody already has that loop.
The manager keeps no accuracy table, no feedback store, and never re-assigns a run in flight —
each of those is a queue wearing a different hat. The decision is written to `events` as
`teams.route` with its reason, so misroutes are *countable* after the fact; that count is the
input to whether Tier 2 is ever worth building (§7, T7).

**"Busy" is not a state a teammate has, and that is the point of the execution model.** An
identity is durable serializable state, not a resident process, so there is no Alice to be
occupied. A second ticket for Alice hydrates a second session from the same state and runs it.
The only real constraint is Rhapsody's *existing* global and per-state concurrency, enforced
before routing is ever called. `max_concurrent` per identity exists as an escape hatch for a
user who wants a teammate serialised; when it would be exceeded the router **picks the
next-best candidate and never queues** — because the alternative is holding work, which is the
prohibited behaviour in its purest form.

**When nobody fits: fall back, never refuse.** `default_identity` takes it. If that is unset,
the ticket dispatches **exactly as it does today** — no identity, no bank, no prepended
section, byte-identical to Teams-off — and an event `teams.unrouted` is recorded. Refusal is
not an option under consideration: a Teams feature that can withhold work is a second queue,
and it fails closed against the user's actual intent, which is that the ticket gets done.

---

## 4. Defaults are seeds — the upgrade problem

**Named strategy: layered defaults with explicit fork.** Overlay by default; pin on request;
fork when the user wants full ownership. Rhapsody **only ever reads** a user's profile file.

- Built-in profiles ship compiled into the binary and are **versioned**: `swe@1`, `reviewer@1`,
  `sre@1`.
- A user profile file is an **overlay**. `extends: swe` (unpinned) tracks the newest built-in:
  fields the user never set improve for free on upgrade; fields the user set stay exactly as
  written, forever. The body composes via `{{ base }}` rather than replacing wholesale.
- `extends: swe@3` **pins** the base. Upgrades do not move it. Rhapsody *reports* drift —
  "`alice`'s profile overlays `swe@3`; the built-in is now `swe@5`" — in `GET /api/v1/teams`
  and a single startup warning. **Reports; never mutates.**
- `extends: none` is a **fork**: the file is the whole profile and Rhapsody contributes nothing
  to it. `rhapsody teams fork swe` materialises the fully-resolved current text into the file
  and sets `extends: none`, so choosing seed-once semantics is one command and is explicit.

**Why not a three-way merge.** The merge target is prose prompts, there is no interactive
resolver inside a daemon, and a conflicted profile is a *broken agent discovered at dispatch
time*. A background upgrade that can leave a user unable to run is worse than either extreme.

**Why not seed-once-then-hands-off as the default.** It is the ticket's stated failure in
reverse: every improvement Rhapsody ships would reach only new users, and the shipped defaults
would rot into decoration within two releases. Note honestly that `capabilities.rs` today *is*
seed-once-plus-append — a new bundled capability reaches existing users, but an edited one is
never updated. That is the right trade for a one-sentence `instruction` and the wrong one for a
page-long role prompt. Unifying the two is named as a follow-up in §7, not smuggled into this
design.

**Why layering wins.** It makes the two cases *distinguishable in the file itself*. "I never
touched this" and "I own this" are different states, so the upgrade has an unambiguous answer
for each — and the ambiguity is precisely what makes clobbering possible. The user still owns
their team: they edit these files with Claude, Codex, or by hand, and the files are theirs.

**The cost, stated plainly.** Resolution is now a function, so "what prompt does Alice actually
get" is no longer answered by opening one file. That is a real ergonomic loss and it is paid
for with `rhapsody teams show alice` and
`GET /api/v1/teams/profiles/<name>/resolved`, which print the fully-resolved text. Any
implementation that cannot answer that question in one command has got the trade wrong.

---

## 5. Memory — what an identity retains, and what invalidates it

[STUDIO-569](https://linear.app/studio49/issue/STUDIO-569/discovery-persistent-named-agents-an-identity-and-memory-layer-over)
owns the mechanism. This design **consumes its recommendation unchanged**: one bank per
identity, `agent-<name>`, `default` tenant, on the Hindsight instance already running, at $0.00
marginal; `enable_observations: false`; the MCP endpoint directly, not the Go
`internal/studiomemory` client. Its five retention rules are adopted verbatim and are not
restated here. What follows is only what *Rhapsody* contributes.

### 5.1 Retain — Rhapsody supplies the evidence, the agent supplies the prose

569's rule 1 is that the payload is **constructed, never a transcript**, because the retention
policy cannot be enforced at extraction time — a `retain_mission` was measured to *launder*
conclusions rather than refuse them, stripping the hedge and leaving a confident false
assertion. Rhapsody's contribution is that it already knows every field rule 3 requires:

```
document_id: run-<run_id>
metadata:    { ticket, commit_sha, pr, run_id, identity }
content:     authored by the agent at end of run — observations and outcomes only
```

`ticket`, `run_id` and `identity` come from the run; `commit_sha` and `pr` from the workspace
and `gh`. The agent never has to remember to attach provenance, which is the part of a
discipline that erodes first. The write is **best-effort and never fatal** — a failed retain
logs and the run completes, exactly as booch treats a failed memory call.

### 5.2 Recall — and what notices a fact that was never true

Recall runs **at turn 1 only**, `types: ["experience"]`, `include: {source_facts: {}}`, bounded
by `recall_top_k`, rendered into the same prepend slot `capabilities_section` uses. Both
Hindsight defaults are wrong for this use (`world` is where laundered conclusions live; source
facts are where the proof is), which is why both are overridden.

The ticket's hardest question is the trigger, not the mechanism: consolidation catches "B
contradicts A" and nothing catches "A was never true and nothing contradicts it" — the real
case being a ticket remembered as open that had been Done for five days. Cheapest answers
first:

1. **Re-ground at recall; do not trust.** Every fact carries `ticket` in metadata. Before a
   recalled fact reaches the prompt, any ticket it names is re-grounded against the tracker and
   rendered **with its current state attached** — *"STUDIO-408 — the poller skipped null
   attachments (ticket now: Done, 2026-08-19)"* — rather than dropped. This costs **zero model
   turns**: it is a map lookup against issues the poller already fetched this cycle, with
   `symphony_ticket` / `GET /api/v1/issues/{id}/history` as the fallback for anything stale.
   **This is the answer to the exact case the ticket names**, and it is free.
2. **Commit SHAs re-ground the same way** — `git cat-file -e <sha>` in the workspace. A fact
   naming a SHA the repo does not have is a fact about a force-push or a closed PR; render it
   flagged, do not hide it.
3. **A fact with no ticket and no SHA, which was never true: nothing automatic catches it.**
   That is 569's conclusion and this design does not pretend otherwise. The trigger is a human
   at review time. Rhapsody's contribution is making the correction *reachable at the moment
   someone notices* — a per-fact invalidate-with-reason exposed as an MCP tool and a dashboard
   button, instead of a `kubectl port-forward` and a hand-written `curl`.
4. **Rule 1 remains the load-bearing defence**: the conclusion was never authored, so it was
   never stored.

### 5.3 Invalidation — reconciling the ticket's ⚠️ with 569's measurement

The ticket warns that the per-fact path is unverified end to end because S0's `Invalidate`
returned **400**. 569 §*The correction path* reports the opposite, measured live on
2026-08-24: `PATCH /v1/default/banks/{bank}/memories/{id}` with
`{"state": "invalidated", "reason": ...}` removed the fact from recall, **pruned its derived
observation** (the bank went 5 facts → 4), stored the reason, and was reversible.

These are not in conflict — they are two different call sites. The 400 came from the **Go
client**, `internal/studiomemory.Invalidate`, which sends `{"state": "invalidated"}` and no
`reason`. 569 separately concludes that agent banks do not use that client at all. So: **the
raw MCP/HTTP path is confirmed; the unconfirmed path is the one this design does not use.**

That said, "confirmed from a probe script" is not "confirmed from Rhapsody's client", so
re-checking it once against the live deployment is an **acceptance criterion of the memory
slice** (§7, T4) rather than an open design question. Nothing else is allowed to depend on it
before that check passes.

Two adjacent facts, carried forward for the operator's sake: `readableByModel` refuses **any**
non-`valid` state, so an invalidated fact is invisible to the model rather than merely
deprioritised; and `DELETE /banks/{bank}` wipes an identity entirely, which is both the "fire
Jimmy" story and the reset when a bank's memory has gone bad past repair. The second is
destructive and irreversible and belongs behind a confirm dialog, not an MCP tool.

### 5.4 Why `memory.backend` defaults to `none`

**Rhapsody cannot reach the bank today.** 569 measured the `hindsight-memory` NetworkPolicy as
admitting exactly one source, `booch-production` — not a laptop, not a runner, not a cloud
routine. Rhapsody runs from `/Applications/Rhapsody.app` on a laptop. This is 569's own
headline finding: *the gap is not identity or wake, it is reachability*, and a woken agent with
no route to memory is just a cron job.

So Teams v1 is designed to be **useful with no memory at all**: named routing, real profiles,
and per-identity work history that comes free from `assignments ⋈ runs` (§6). `backend:
hindsight` becomes selectable when studio-infra's **S1** (expose the service on the tailnet)
lands. That is a dependency, stated, and it is why the slice plan puts memory fourth.

**No local memory backend is designed here.** 569 owns memory infrastructure and its whole
point is that the bank is already paid for; reimplementing one locally would be re-deriving
what the ticket says not to re-derive. Noted as an open question, not a slice: because rule 1
means records are *constructed* rather than extracted, a future local backend could be an
append-only file plus a tag index — nothing in this design depends on extraction, embedding, or
consolidation.

---

## 6. How teammates communicate

### 6.1 Four paths, priced, memory-first

| Path | Cost | Where it is durably recorded | Use when |
| --- | --- | --- | --- |
| **Recall from a teammate's bank** | 0 turns (one recall) | the bank — already | *"what did we learn about X"* — **the default** |
| **Direct message to a live teammate** | 0 extra turns (rides the target's existing loop) | `run_messages` + an `events` row | A needs B *now* and B is running |
| **Manager-routed ask** | 1 turn (hydrate B) | B's bank + the ticket | judgement only B can give, B is idle |
| **Broadcast to the roster** | N turns | N banks | **refused in v1** |

**Memory-first is the rule, not a preference.** "What did we learn about X" is a *recall*, and
answering it with a live turn buys a model call to read a database. The router prefers a bank
query in every case where the question is about the past.

### 6.2 Direct addressing already exists, and its constraint is the design

`symphony_send_message` is ON by default (`mcp.allow_send_message`), takes any `run_id`, posts
to `POST /api/v1/runs/{id}/message`, persists the operator's original text to `run_messages`,
and delivers through a bounded 16-deep mailbox. **A teammate can already message another
teammate today, durably, with no new code.**

Crucially it returns `not_running` for a run that is not live. So **the only teammate you can
direct-message is one that is currently running** — which is precisely what durable-state /
ephemeral-compute wants, enforced by an error code that already exists rather than by a rule
someone has to remember. There is no idle Alice sitting in a pane waiting for a keystroke,
because there is no pane.

This answers the ticket's "if identities are long-running, summonable agent processes that can
sit idle waiting for a prompt, direct addressing becomes an option too": **they are not, and it
does not — and that is deliberate.** N idle identities cost nothing precisely because none of
them is a process. Making them addressable-while-idle would mean N resident sessions, which is
herdr's model and the one thing the ticket says not to copy.

### 6.3 What we steal from herdr, and the one thing we do not

**Steal the state-aware handshake.** herdr's `idle / working / blocked / done` maps onto state
Rhapsody already derives: `symphony_run_status` returns `alive | stalled | completed | failed |
interrupted | not-dispatched`, and identity status is `working` if the identity has any live
run, else `idle`. **It is derived at read time and never stored** — a stored status is state
that can go stale, and it is one refactor from being a queue.

**Steal the MCP surface.** These ops belong inside the agent's own loop, and Rhapsody already
injects an MCP server per workspace (`crates/agent/src/claude/mcpinject.rs`), so "message
teammate X" is a tool call, not a shell-out.

**Do not build `herdr_agent_wait` as a blocking call.** A worker blocking on a peer's status
burns wall-clock inside a turn and creates a deadlock — A waits on B, B waits on A — with no
supervisor to break it. Rhapsody's equivalent is already non-blocking and already correct: A
sends, A keeps working, and B's answer arrives in A's mailbox on a later turn. Where a
synchronous read is genuinely required, the right shape is bounded polling of
`symphony_run_status` **in the agent's own loop** — visible, interruptible, and capped by the
turn budget — never a daemon-side block. This is the one place the design deliberately departs
from the prior art the ticket points at, and the reason is that the prior art has a
multiplexer to unstick a hung pane and Rhapsody does not.

### 6.4 What we take from Buzz, and what we refuse

Take **the property**: the exchange must be durable and addressed to a teammate, not ephemeral
pane I/O. We do not need Nostr to have it — `run_messages` is durable, addressed by run, and
already in the parity schema.

Refuse **coordination in chat**. Buzz puts task assignment in channels; that is the second-EM
pattern §3 exists to prevent. Linear stays the ledger; messages and summons are *triggers*,
never the record.

Note, and do not build: Buzz's identity is a **keypair** (a passport — who you are, what you
may do, portable between hosts); ours is a **memory bank** (a brain — what you know). They are
complementary and a keypair does not solve memory staleness. A portable key per identity is a
plausible later layer and is explicitly out of scope for v1.

### 6.5 Where the exchange is recorded — three places, none of them new

1. **`run_messages`** — the message text verbatim, with run id and timestamp. Already in the
   parity schema, already durable.
2. **`events`** — a `teams.message` row so the exchange appears in the run timeline and in
   `GET /api/v1/events`. An existing table and an existing column; no schema change.
3. **The ticket or the PR** — if the exchange changed a decision, the agent's *existing*
   obligation to say so on the PR covers it.

**Nothing in Teams creates a channel Linear cannot see.** That is the whole requirement.

### 6.6 Broadcast is refused, and `teams_ask` is deferred

**Broadcast** buys N model turns to answer a question that is almost always a memory question,
and N replies fanning into one 16-deep mailbox is a fan-in this design has no bounded answer
for. If it is ever genuinely needed, the shape is *recall across all banks* — N recalls, not N
turns — and that is a different, much cheaper feature.

**`teams_ask` (the manager-routed hydrate-and-ask) is deferred out of v1**, and the reason is
worth being explicit about: it would dispatch a run **not tied to a Linear issue**, which
Rhapsody has never done. Such a run has no ticket, therefore no state machine, therefore
nothing for `classifyCleanExit` to classify and nothing for the poller to reconcile. That is a
new run *kind*, not a new tool, and it deserves its own ticket. v1 ships memory-first plus
direct-to-live, both of which are entirely existing machinery.

### 6.7 The MCP surface Teams adds

All gated by the toggle and **removed from the router when off**, following `allow_handoff`
(`crates/mcp/src/server.rs:69–80`), so `list_tools` is byte-identical and a disabled feature is
not merely inert but invisible.

| Tool | Reads/Writes | Notes |
| --- | --- | --- |
| `teams_roster` | read | who exists, profile, derived status |
| `teams_recall {identity, query}` | read | the memory-first path; no live turn |
| `teams_invalidate {identity, fact_id, reason}` | write | §5.3; requires `memory.backend: hindsight` |
| ~~`teams_ask`~~ | — | deferred, §6.6 |

Direct messaging needs **no new tool** — `symphony_send_message` is the path, and `not_running`
is the intended constraint rather than a limitation to work around.

---

## 7. Slices

Each stands alone and each is useful if the next never happens.

- **T1 — The toggle and the roster, inert.** `crates/config/src/teams.rs`: the types, the
  `~/.rhapsody/teams.yaml` loader, absent ⇒ disabled, never seeded. No dispatch change, no
  prompt change, no MCP change.
  *Acceptance:* `cargo test --workspace` green with **zero edits to any existing test or
  golden**. That is the whole point of the slice — it proves the inertness claim in §2.4
  *before* anything is built on top of it.

- **T2 — Profiles as artifacts.** `~/.rhapsody/teams/profiles/*.md` parsed with the existing
  `workflow::Definition` (front matter + body). Built-in `swe@1`, `reviewer@1`, `sre@1`.
  `extends` resolution, `{{ base }}` interpolation, drift reporting. `rhapsody teams show
  <identity>` prints the resolved text.
  *Useful alone:* a user can author, fork and inspect profiles before anything routes.

- **T3 — Deterministic routing and the prompt prepend.** `route()` as a pure function, Tier 0 +
  Tier 1 only. `teammate_section` prepended in `build_turn_prompt`. The `assignments` row in the
  sidecar plus a `teams.route` event.
  **This is the smallest version of the whole feature that works, and the recommendation is to
  ship T1–T3 and stop until it has been used for a week** — mirroring 569's S2 verdict that one
  agent, by hand, for a week is what decides whether the rest is worth building.

- **T4 — Memory, read side.** `memory.backend: hindsight`, recall at turn 1 with re-grounding
  (§5.2). **Blocked on studio-infra S1** (a route to the bank).
  *Acceptance includes:* one live `PATCH … {"state":"invalidated","reason":…}` from Rhapsody's
  own client, per §5.3.

- **T5 — Memory, write side.** The constructed end-of-run record with full `metadata` and
  `document_id`; `enable_observations: false` set at bank creation; best-effort, never fatal.
  Depends on T4 only for the client. **If T4 ships, T5 must not be skipped** — recall without
  the authored-record discipline is how the bank fills with laundered conclusions.

- **T6 — Talking, minimal.** `teams_roster` and `teams_recall`, gated off with the feature.
  Direct messaging is *documentation*, not code: `symphony_send_message` is already the path.
  *Useful alone.*

- **T7 — Model arbitration.** `manager.mode: labels+model`, bounded and opt-in, falling back to
  T3's answer on any error. **Do not build this until T3's misroute rate is a measured number**
  from `teams.route` events. It is the only slice whose justification is empirical rather than
  structural.

- **T8 — The invalidation surface.** `teams_invalidate` plus a dashboard button. After T5,
  because before it there is nothing to invalidate.

**Deferred, named, and not slices:** `teams_ask` (needs an issue-less run path, §6.6);
broadcast (§6.6); a local memory backend (§5.4); portable keypair identity (§6.4); per-project
rosters (needs a `Raw` field, a `prune_empty`-correct encode, and an `effective_json`
decision — the `capabilities` precedent shows exactly how, and the note at
`crates/config/src/effective_json.rs:182` shows exactly what not to surface); unifying
`capabilities.yaml` onto the profile layering model of §4.

---

## 8. Where the ticket's open questions are answered

| Question | Answer |
| --- | --- |
| How does the manager decide? | §3.2 — explicit label, then label∩label, then (opt-in) one model turn |
| What does routing cost? | §3.3 — **zero extra turns by default**; the deterministic path is the default, not a fallback |
| Right teammate busy? | §3.4 — "busy" is not a state an identity has; concurrency is Rhapsody's existing limit |
| Nobody suitable? | §3.4 — `default_identity`, else dispatch exactly as today. **Never refuse** |
| Can teammates talk, and how? | §6.1–6.3 — memory-first; direct-to-live via the existing `symphony_send_message`; broadcast refused |
| Where is the exchange recorded? | §6.5 — `run_messages`, `events`, and the ticket/PR. Nothing new, nothing Linear cannot see |
| Defaults upgrade without clobbering? | §4 — layered defaults with explicit fork; three-way merge rejected with reasons |
| What invalidates a fact? | §5.2–5.3 — free re-grounding at recall for anything naming a ticket or SHA; a human plus `PATCH … invalidated` for the rest, and we say plainly that nothing automatic catches it |
| Is it inert when off? | §2.4 — nine touchpoints, each with a mechanism and a test, and a T1 acceptance criterion of *zero edits to existing goldens* |
| Should we ship something smaller? | **Yes** — T1–T3, then stop and use it for a week |

---

## 9. What this design assumes

Everything above is grounded in code read in this repo or in measurements from
[STUDIO-569](https://linear.app/studio49/issue/STUDIO-569/discovery-persistent-named-agents-an-identity-and-memory-layer-over)
except these:

- **The Tier-2 token estimate in §3.3 is arithmetic, not a measurement.** No arbitration prompt
  has been run. It is the right order of magnitude and the wrong thing to quote as a price.
- **Tier 1's hit rate is unknown.** Whether label∩label routes correctly often enough to make
  Tier 2 unnecessary is exactly what T3's week is for, and T7 is gated on the number.
- **569's reachability finding is inherited, not re-verified here.** The NetworkPolicy was
  measured on 2026-08-24; if studio-infra S1 has since landed, `memory.backend: hindsight`
  becomes viable earlier than the slice order assumes. Check before planning T4.
- **`{{ base }}` interpolation is a new templating affordance** in `crates/config/src/prompt.rs`
  territory. It is a small addition, but it is an addition, and it has not been prototyped.
- **The per-fact invalidate has never been exercised from Rhapsody's own client** — only from
  569's probe. §5.3 makes this a T4 acceptance criterion rather than an assumption, but until
  T4 runs it remains one.
