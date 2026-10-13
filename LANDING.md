# Landing

Landing verifies a merged change against the live application before completing its ticket.
It waits until production reports the merged revision (or a descendant), watches repository-owned
checks, and posts what each check observed. A green build proves the PR built; landing adds evidence
that the change reached production and that the declared checks passed there.

Landing is **opt-in per project**. With `landing:` absent or `enabled: false`, merge completion,
auto-merge and the existing parity-checked behavior are unchanged. Rhapsody has no built-in
Kubernetes, Flux, database or metrics checks: the contract has just `http` and `command` primitives.
Your application repository owns the deployment-specific scripts and expectations.

## Availability and setup

The shipped L2 implementation provides deploy waiting, watching, evidence, Done/Held/reopen and
the project landing lock. **Auto-revert is coming in L3. The acceptance agent, operator API,
missing-contract project advisory and dashboard badge are coming in L4.** Their sections below
describe the intended behavior and are explicitly marked; they are not available in L2.

1. Use the Teams ticketless review watcher (`review.mode: ticketless` in `teams.yaml`), with a
   watched PR linked to a ticket this daemon ran. Landing takes over that watcher's merge-to-Done
   path; it is not a general observer of every merge in a repository.
2. Keep persistence enabled, normally the on-disk `storage.path` database. Landing admission
   requires a store; `storage.path: off` / `--no-store` cannot record a landing. An in-memory store
   cannot preserve it across restarts.
3. Commit `.rhapsody/landing.yaml` and any command scripts to the app repo. Provide a live signal
   that reports the deployed **application commit**, not merely the local checkout or GitOps head.
4. Add `landing: {enabled: true}` under that project's `WORKFLOW.md` front-matter entry.
   Name `review.done_state` in `teams.yaml` if successful landings should move tickets to Done.
   With it unset, landing still posts evidence but leaves tracker state alone.

In L2, avoid adding a `## Post-deploy acceptance` checklist unless you intend to park for a human:
it enters `AwaitingAcceptance`, keeps the project lock, and has no agent to finish it until L4.

## Lifecycle

The phase is stored separately from tracker state; you need not add a tracker state called Landing.
`landing.state: ""` keeps the ticket in its review state until completion or route-back.

```text
In Review --merge--> WaitingForDeploy --merge/descendant live--> Watching
                           |                                      |
                           | deploy_timeout                       | watch ends, latest checks pass
                           v                                      v
                          Held                            Done (no acceptance checklist)
                                                        AwaitingAcceptance (L4 agent -> Done)

Watching --failure_threshold--> Reopened (L2; also L3 with auto_revert: false)
                              Reverting (L3 with auto_revert: true)
                                 | revert PR merges and its landing passes
                                 v
                              Reopened + REVIEW FEEDBACK -> author fixes -> normal PR
                                 |
             revert conflict or failed revert landing -> Held + escalation (L3)
```

The contract is read from the **merged commit's Git blob**, not the current default branch or a
mutable worktree copy. Commands execute in a worktree pinned to that same commit on the daemon
host. The daemon verifies the live revision with
`git merge-base --is-ancestor <merge_sha> <reported_revision>` in its existing bare mirror.
It attempts a fetch once for an unknown revision. Malformed, unknown or unrelated revisions are
not live; a successful HTTP response or command exit alone cannot establish deployment.

Once live, the watch window starts. Checks with `every:` repeat until the window ends.
Consecutive failures are counted **per check**; a passing sample resets that check's count.
One check reaching `failure_threshold` fails the landing. A check without `every:` stops after
its first pass; a failed sample retries at the default 30-second cadence until recovery or threshold.

The watch deadline never turns a latest failed sample into a pass. If a check is failing below
threshold at the deadline, Watching continues to its next scheduled sample: recovery permits Done,
and reaching threshold fails. Choose cadences and thresholds that fit your desired observation time;
with a threshold above two, further failures below threshold may require additional samples.
Scheduling occurs on daemon ticks, so `every` and deadlines are not exact wall-clock execution times.

### Missing or broken contracts

Presence is checked at both the merge and its first parent (`merge_sha^1`); a root commit has no
parent and counts as absent before. A contract must be a regular file, not a symlink.

| Before the merge | At the merge | Result |
|---|---|---|
| Either | Present and valid | Normal deploy wait and watch. |
| Either | Present but invalid | Held, with the parse error; never silently passes. |
| Present | Absent | Held: `landing contract removed by this change`. Disable `landing.enabled` before retiring contracts. |
| Absent | Absent | No live probe or checks. Done, or AwaitingAcceptance if a checklist exists. Evidence says `no landing contract: the deploy was not observed`. |

Enabling landing alone is therefore **not proof of deployment**. Add and validate the contract.
The `GET /api/v1/projects` advisory for enabled projects without a contract is coming in L4.

## WORKFLOW.md configuration reference

`landing:` belongs under `projects[]` in the YAML front matter, alongside `repo` and `slugs`.
It is not a top-level setting or a `teams.yaml` review key. A project entry may cover several
slugs, all sharing its landing configuration and lock. Use the tracker's project slug IDs.

This example spells out **every default**; change `enabled` to `true` to opt in:

<!-- landing-example:defaults -->
```yaml
projects:
  - slugs: [app]
    repo: https://github.com/example/app.git
    landing:
      enabled: false
      auto_revert: true
      deploy_timeout: 30m
      watch: 10m
      failure_threshold: 2
      revert_review: false
      state: ""
      env_file: ""
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Hand watched ticket merges to landing instead of immediate merge completion. Absent block is disabled. |
| `auto_revert` | `true` | Coming in L3: create a revert after check/acceptance failure; `false` reopens without reverting. L2 always reopens without a revert, regardless of this value. |
| `deploy_timeout` | `30m` | Maximum wait for the merge to become live; expiry is Held, never a revert. A queued landing receives its deploy window when it starts. |
| `watch` | `10m` | Observation window beginning when the live probe first sees the merge or a descendant. Latest failed samples cannot complete Done. |
| `failure_threshold` | `2` | Positive integer of consecutive failures of one check before the landing fails; success resets its counter. |
| `revert_review` | `false` | Coming in L3: whether a revert PR needs peer approval; CI and the other merge gates still apply. No effect in L2. |
| `state` | `""` | Optional tracker state name to move the ticket to on landing admission. Empty leaves it in review. |
| `env_file` | `""` (none) | Host-side environment assignments for command checks/live probes, such as kube context or a read-only DSN. Use an absolute path outside the repository. |

Durations use Go-style positive strings, such as `30s`, `1.5m` or `1h30m`; zero, negatives and
invalid units are rejected. `landing:` rejects unknown keys and names the offending spelling.

`env_file` is read on the daemon host; it is **not** sourced as a shell script. Nonblank,
noncomment lines must be `KEY=value` (optional `export ` prefix); surrounding quotes are stripped.
There is no variable substitution, command substitution or `~` path expansion. Relative paths
are relative to the daemon's working directory, not WORKFLOW.md. Commands inherit the daemon
environment plus these assignments. Keep credentials out of the repo and have scripts emit only
nonsecret observations. Evidence redacts exact env-file values, not arbitrary transformed secrets.
An unreadable env file fails probes/checks rather than silently running with missing assignments.

## Contract reference

Store the following fields in `.rhapsody/landing.yaml` in the **application repo**. Unknown keys at
every contract level are rejected with their path (for example `checks[1].htp`).

| Root field | Default / requirement | Meaning |
|---|---|---|
| `version` | Required integer `1` | Only version 1 is supported. |
| `live` | Required mapping | A single probe using the same primitive/timing fields as a check, but without `name`. Its trimmed stdout or HTTP body reports the running revision. |
| `checks` | Absent = empty list | Named checks to run once the merged revision is live. An empty list still waits through `watch`. |

| Probe / check field | Default / requirement | Meaning |
|---|---|---|
| `name` | Required for each check | Nonblank, unique within `checks`; used in evidence and failure feedback. Not allowed on `live`. |
| `http` | Exactly one of `http` or `command` | HTTP GET configuration; see the fields below. |
| `command` | Exactly one of `command` or `http` | Nonempty list of string argv elements, first a nonempty executable. No implicit shell or interpolation. Relative executables/arguments run in the merged worktree. |
| `every` | Live: `30s`; checks: absent | Positive duration. Live probes repeat while waiting. Checks repeat throughout watch if set; absent means stop after success, retry failures every `30s`. |
| `timeout` | `60s` for either primitive | Positive duration limiting each execution, including HTTP response-body reads. Timed-out commands have their process group killed. |
| `expect_stdout` | Absent | Command-only exact comparison with the **entire trimmed stdout**; exit 0 is also required. The expectation itself is not trimmed. An empty string requires empty trimmed output. |

| `http` field | Default / requirement | Meaning |
|---|---|---|
| `url` | Required | Absolute `http` or `https` URL with a host. The primitive sends GET; it has no request-header, method or JSON-extraction fields. |
| `status` | Required integer, `100..599` | Expected response status. There is no implicit `200` default. |
| `headers` | Absent = empty mapping | Response header names to string expectations. `present` requires existence; any other string requires exact value equality. Quote numeric-looking values. |
| `body_contains` | Absent | Required literal substring anywhere in the response body. Empty string imposes no content constraint. |

HTTP passes only when status, all listed headers and any body substring match. Command passes
only with exit 0 and, when specified, matching stdout. Shell syntax requires an explicit shell
argv (for example `[/bin/sh, -c, '...']`); executable scripts are easier to keep and review in repo.
Stdout evidence is bounded to 2 KiB along with its summary; comparison is not limited to that prefix.
Command stderr is discarded. HTTP evidence records status, requested header observations,
substring-match result and duration; it does not dump the response body.

For `live`, the response must be a nonempty revision string of at most 2 KiB. An HTTP `/version`
must return a **plain application commit SHA**, not `{"revision":"..."}` or a version label.
There is no JSONPath selector. Do not use `git rev-parse HEAD` in a production live script: that
reports the daemon's already-pinned checkout and falsely claims deployment without observing it.

## Worked contract: HTTP-only

Implement `/version` in the app to return its built-in application commit as plain text, and
`/healthz` to return `200` with a body containing `ok`. Replace `app.example.com` with your host.
No command scripts or external cluster tools are needed.

<!-- landing-example:http -->
```yaml
version: 1
live:
  http:
    url: https://app.example.com/version
    status: 200
  every: 30s
  timeout: 10s
checks:
  - name: health
    http:
      url: https://app.example.com/healthz
      status: 200
      headers:
        strict-transport-security: present
      body_contains: ok
    every: 30s
    timeout: 10s
```

This waits for the merge, then samples health for the configured watch window. Two consecutive
bad health responses fail at the default threshold; an isolated failure followed by success does not.

## Worked contract: Kubernetes + GitOps

**The following are example scripts that live in the app repo, not part of Rhapsody.** Commit them
as executable files under `scripts/landing/`. Adapt the selectors, migration table and Flux resource
names to your app. The daemon host needs `kubectl`, `jq` and `psql`, cluster read access, and a
database read-only role. Configure `KUBE_CONTEXT`, `APP_NAMESPACE`, `LANDING_READONLY_DSN` and
`FLUX_NAMESPACE` (often `flux-system`) through the host-side `env_file`.

This example assumes immutable images tagged `:<40-character application commit SHA>`.
The live probe reads the **running containers' images**, not the image requested by a deployment
manifest or the latest GitOps commit. Ready pods must agree on one revision; rollout readiness
is a separate check.

<!-- landing-example:gitops -->
```yaml
version: 1
live:
  command: [scripts/landing/live-rev.sh]
  every: 30s
  timeout: 10s
checks:
  - name: rollout
    command: [scripts/landing/flux-ready.sh, app]
    every: 30s
    timeout: 60s
  - name: migration
    command: [scripts/landing/schema.sh]
    expect_stdout: "8|f"
    timeout: 10s
  - name: health
    http:
      url: https://app.example.com/healthz
      status: 200
      body_contains: ok
    every: 30s
    timeout: 10s
```

`scripts/landing/live-rev.sh` (example; label `app=app`, container `app`):

```sh
#!/bin/sh
set -eu
pods=$(kubectl --context "$KUBE_CONTEXT" -n "$APP_NAMESPACE" get pods \
  -l app=app -o json | jq -r '.items[] | select(.metadata.deletionTimestamp == null) |
  select(any(.status.conditions[]?; .type == "Ready" and .status == "True")) | .metadata.name')
test -n "$pods"
revisions=$(
  for pod in $pods; do
    kubectl --context "$KUBE_CONTEXT" -n "$APP_NAMESPACE" get pod "$pod" -o json |
      jq -er '.status.containerStatuses[] | select(.name == "app" and .ready) |
        .image | capture(":(?<sha>[0-9a-f]{40})$").sha' || exit 1
  done
)
revision=$(printf '%s\n' "$revisions" | sort -u)
test "$(printf '%s\n' "$revision" | wc -l | tr -d ' ')" = 1
printf '%s\n' "$revision"
```

`scripts/landing/flux-ready.sh` (example; matching Flux Kustomization and Deployment names):

```sh
#!/bin/sh
set -eu
name=${1:?supply the Flux Kustomization and Deployment name}
kubectl --context "$KUBE_CONTEXT" -n "$FLUX_NAMESPACE" get kustomization "$name" -o json |
  jq -e '.status.observedGeneration >= .metadata.generation and
    any(.status.conditions[]?; .type == "Ready" and .status == "True")' >/dev/null
kubectl --context "$KUBE_CONTEXT" -n "$APP_NAMESPACE" rollout status \
  "deployment/$name" --timeout=45s >/dev/null
printf 'flux and rollout ready\n'
```

`scripts/landing/schema.sh` (example; read-only `psql` against a `schema_migrations(version, dirty)` table):

```sh
#!/bin/sh
set -eu
PGOPTIONS="${PGOPTIONS:-} -c default_transaction_read_only=on" \
  psql "$LANDING_READONLY_DSN" -X -A -t -v ON_ERROR_STOP=1 \
  -c 'SELECT version, dirty FROM schema_migrations;'
```

This query emits `8|f` for version 8, not dirty. A PR expecting migration 9 changes the contract to
`expect_stdout: "9|f"` in that same PR. It does not run migrations or repair the database.
Check scripts should return nonzero on unreadable state and print a concise, nonsecret observation
on stdout. A command that cannot reach production is a failed check, not evidence of health.

## Evidence, failure and revert

The terminal/acceptance handoff comment on the ticket contains a table of
`name | observed | pass/fail | at`, including live samples and each check's observations.
Timestamps are epoch seconds; command outcomes include exit code/stdout, and HTTP outcomes include
status/header values and duration. A Held comment also names its reason. The stable landing ID
marker lets restart recovery recognize already-posted evidence instead of duplicating the comment.

In L2, threshold failure prepends a `## REVIEW FEEDBACK` block listing failed checks and values,
moves the ticket to `review.changes_state` (default fallback `In Progress`), and arms its author
for redispatch under the ordinary eligibility gates. **The deployed code remains in place.**
A deploy timeout instead records Held and notifies the operator: the new revision was never
observed, so it does not justify reverting anything. Broken/removed contracts also hold.

### Auto-revert (coming in L3)

With `auto_revert: true`, failed checks or acceptance lead to Reverting. The daemon creates a
`git revert <merge_sha>` PR titled `revert: <original title> (landing failed: <check>)`, labelled
`rhapsody:revert`, with the failed evidence in its body. It must pass CI and normal merge gates;
`revert_review: true` additionally requires peer approval.

The revert has its own landing at the revert commit, using that commit's contract. Only after it
lands successfully does the original ticket reopen with REVIEW FEEDBACK. A conflicting revert or
a failed revert landing goes Held and escalates to a lead/human. **At most one automatic revert
per landing**: there is no revert-of-revert loop. `auto_revert: false` reopens directly.

### Post-deploy acceptance (agent coming in L4)

A ticket may include a `## Post-deploy acceptance` section with Markdown checklist items.
After deterministic checks pass, it enters `AwaitingAcceptance`. L4 will dispatch a read-only
`lander` run keyed `land:<owner>/<repo>#<pr>`, given the checklist and existing evidence. It cannot
push or open PRs; it verifies each item and posts one comment with evidence and a pass/fail verdict.
No checklist means no agent or token cost. Unmet items fail the landing. Drain pauses new landing
agent dispatches; deterministic checks continue while draining.

## Per-project landing lock and restarts

Auto-merge pauses for the WORKFLOW project group while a nonterminal landing exists. The lock is
reserved durably **before** auto-merge invokes GitHub, closing the merge-to-watcher gap; one sweep
admits at most one merge for that group. It spans all slugs of the same project entry, not every
project on the daemon. Human merges are not blocked; their watched landings queue behind the first.
Avoid overlapping human merges if you need checks to attribute failure to exactly one change.

The store's open phases are `WaitingForDeploy`, `Watching`, `AwaitingAcceptance` and `Reverting`.
Done, Held and Reopened are terminal and release the lock; **Held is not a deployment freeze**.
In particular, L2's parked AwaitingAcceptance keeps the lock until it can be resolved.

Migration 38 adds `rhapsody_landing` and `rhapsody_landing_project`. Rows retain merge/revert
coordinates, phase, absolute deadlines, results and pending effects. A restart resumes the
existing window rather than granting a fresh one. Deterministic checks run off the control loop
and continue during a drain. Do not edit the database to force a phase transition.

## Operator API (coming in L4)

These endpoints and the ticket-card badge are **not implemented in L2**. Until L4, inspect the
ticket evidence and the database read below; a 404 from a landing route is not an empty listing.

| Endpoint | Intended operation |
|---|---|
| `GET /api/v1/landings` | List landings, phases and evidence. |
| `POST /api/v1/landings/{id}/retry` | Retry a held/failed landing after its underlying issue is fixed. |
| `POST /api/v1/landings/{id}/skip` | Mark Done with an explicit `skipped by operator` evidence record. Skipping is an override, not a passed check. |
| `POST /api/v1/landings/{id}/stop` | Cancel a pending revert. |

L4 operator writes use the shared loopback operator-write guard: target
`http://127.0.0.1:<daemon-port>`, send `X-Rhapsody-Operator: 1` and JSON, and omit cookies.
The dashboard/desktop client will provide this posture. Bad IDs or phase-incompatible operations
return 404/409; consult the shipped API for permitted phase transitions when L4 lands.
The badge will show phase and the first failing check; `/api/v1/projects` will advise on enabled
projects missing a contract.

## Read real state

Prefer the **application API, database and metrics backend** over container stdout for checks.
Query a persisted migration version/dirty flag, an API's visible result, or a metrics error-rate
window with explicit start/end times. Missing metrics or query failure should be reported as
unknown/failed, not converted into zero errors. Container logs may go only to an OTel collector,
and a healthy-looking old line says nothing about current production.

When investigating a Kubernetes result, enumerate pods and target **pods by name** (and containers
with `-c`), as the live example does. `kubectl logs deployment/app` can silently select a different
pod from the one you meant to inspect. Use logs to explain observed state, not replace it.

For Rhapsody itself, prefer the landing API once L4 is available. The durable L2 state can be read
without changing it (substitute your actual `storage.path`):

```sh
sqlite3 -readonly "$HOME/.rhapsody/rhapsody.db" \
  'SELECT id, project_slug, ticket, merge_sha, phase, deploy_deadline_s,
          watch_until_s, hold_reason, results_json
     FROM rhapsody_landing ORDER BY id DESC LIMIT 20;'
```

Inspect the declared revision, current phase and recorded samples before drawing a conclusion
from daemon/container stdout. A ticket being merged, a daemon `/healthz` returning 200, or an empty
logs panel is not proof that the app passed landing.

## Migrations and auto-revert

Recommend **expand/contract** for projects enabling `auto_revert`. First expand the schema in a
backward-compatible way; deploy code compatible with old and new shapes; backfill as needed; only
later ship the contract step as a separate change once old code is no longer needed.

Auto-revert rolls back application code **only**. It does not undo data writes, run down-migrations,
restore a database backup or guarantee that old code works against a destructive schema change.
Keep the schema check read-only, and design each migration so the previous application revision
can still run when its successor is reverted. Rhapsody recommends this discipline but does not
enforce it or supply a database-specific migration primitive.
