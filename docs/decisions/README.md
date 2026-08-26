# Decision records

One record per decision or design produced **as the deliverable of a ticket**: what was
decided, when, on what evidence, and what it costs. Markdown in git — diffable, reviewed,
backed up, and it travels with the repo.

## This does not change where specs and plans live

**Specs and plans are still Linear project documents and are still never committed here.**
They are read-only inputs to a run. That rule is unchanged and this directory does not
weaken it.

What lives here is the narrow case Linear is a bad home for: a record whose *product* is the
reasoning — a design a later ticket implements against, or a discovery whose measurements a
later reader will quote. Those need to be diffable, to be reviewable in a PR, and to sit next
to the code they describe.

The test: **if a document tells a run what to build, it is a spec and it belongs in Linear. If
it records what was decided and why, so that a future reader can tell whether it still holds,
it is a record and it belongs here.**

## The convention

Borrowed unchanged from `studio-infra/docs/decisions/` (STUDIO-573), so the two repos read the
same way.

`docs/decisions/NNNN-<slug>.md`, four digits, allocated in order. The number is the id and
never changes; the slug is for humans. Every record opens with `# NNNN. Title` and a header
block:

| Field | What it carries |
| --- | --- |
| **Date** | When it was decided or measured. Not when the file was last edited. |
| **Status** | `Accepted`, or `Superseded by NNNN` — see below. |
| **Ticket** | The ticket it came from, linked. |

### Status and supersession are the point, not decoration

A record with no status reads as current forever, which is the problem the convention exists to
fix. **A README reads as current truth; a measurement is true on a date.**

- **`Accepted`** — this is what we decided, and nothing has replaced it.
- **`Superseded by NNNN`** — a later record replaced the decision. Say which, and say in one
  clause what changed. `Superseded in part by NNNN` when the measurements still hold and only
  the decision moved on.

**A superseded record is not deleted and stays readable.** It is the reason its replacement
exists; deleting it loses the reasoning that produced both.

### What stays in `README.md`

The split is **durable operating knowledge** versus **point-in-time decision**. The repo
`README.md` keeps the rules a future change must not break — the Divergences table, the parity
policy, how to build. When a record produces a durable invariant, restate the invariant in
`README.md` and link the record: the README says the rule, the record holds the reasoning.

## The records

| # | Record | Date | Status | Ticket |
| --- | --- | --- | --- | --- |
| 0001 | [Rhapsody Teams — named agents with shared profiles, under a manager](0001-rhapsody-teams.md) | 2026-08-26 | Accepted (design only; nothing implemented) | STUDIO-572 |
