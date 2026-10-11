# `harness/ci` — temp-dir leak gate and runner hygiene (STUDIO-1031)

Every test scratch directory must be removed when its test ends. Before this, the helpers that
created `rhapsody-*` directories under the OS temp dir (`rhapsody-orchestrator-*`,
`rhapsody-httpapi-golden-*`, `rhapsody-store-test-*`, `rhapsody-d2-sup-*`, `rhapsody-d4-tool-*`, …)
had no guard, so a shared dev or CI `$TMPDIR` grew to >100,000 entries — slowing every create,
lookup and exec, pinning `fseventsd`, and making short-timeout tests fail at random.

Two things keep it fixed:

## 1. The CI leak gate — `check-tmp-leaks.sh`

Fails a test job if any `rhapsody-*` entry in the temp dir is **newer than a marker** touched at the
start of the job. Only newer entries count, so another worktree's tests (or a historical backlog)
do not red an unrelated run.

```sh
touch "$RUNNER_TEMP/tmp-leak-marker"        # before the tests
cargo test --workspace
harness/ci/check-tmp-leaks.sh "$RUNNER_TEMP/tmp-leak-marker"
```

`check-tmp-leaks_test.sh` pins the scan by making it fire: it points the real script at a scratch
root and asserts an older entry and a non-`rhapsody-` entry stay green while a newer `rhapsody-*`
directory reds and is named. It runs in the `lint` CI job (bash-only, like the plugin leak scan).

The guards themselves live beside each helper: a `TempDir` guard removes its directory on `Drop`,
names it with pid + a per-process counter + a nanosecond nonce (so a recycled pid never adopts an
earlier run's name), and skips removal only when `RHAPSODY_KEEP_TEST_DIRS` is set for debugging.

## 1b. The install-action HOME gate — `check-install-action-home.py`

`taiki-e/install-action` downloads, checksums and extracts in a FIXED, user-global scratch dir
(`${HOME}/.install-action/tmp`, no locking). Every self-hosted Mac runner is a separate runner
instance on ONE host, all as the same user, so the concurrent `test` and `desktop` jobs on a push to
main shared that one directory — one job's download or its trailing `rm -rf` deleted the other's
in-flight `tmp` and the checksum verify died seconds in (`sha256sum: *tmp: No such file or
directory`). Each such step now sets a job-private `HOME: ${{ runner.temp }}/install-action-home`,
which this check pins: it reads `.github/workflows/ci.yml` and fails if any `taiki-e/install-action`
step lacks a job-private `env.HOME`, or points it at a shared path.

`check-install-action-home_test.py` pins the scan by making it fire: it drops the override from one
or both steps (the exact regression), sets a shared literal HOME, and asserts the scan reds and names
the step — while a step that is not `install-action` stays ignored. Both run in the `lint` CI job
(python3-only, like the other gates).

## 2. The Mac runner pruner — `prune-tmp-rhapsody.sh`

Even with guards, a killed test process can leave a directory behind. This deletes `rhapsody-*`
directories older than a day from the temp dir, so a backlog can never rebuild.

```sh
harness/ci/prune-tmp-rhapsody.sh      # RHAPSODY_TMP_PRUNE_DAYS=1 by default
```

Install it as a daily `launchd` job on the Mac CI runners (and any shared dev Mac). `com.rhapsody.tmp-prune.plist`
is a **template** — install it under `~/Library/LaunchAgents`, never into a shell profile or
dotfile:

```sh
sed "s#__REPO__#$PWD#g" harness/ci/com.rhapsody.tmp-prune.plist \
  > ~/Library/LaunchAgents/com.rhapsody.tmp-prune.plist
launchctl load -w ~/Library/LaunchAgents/com.rhapsody.tmp-prune.plist
```

Linux CI runners run with `PrivateTmp`, so their temp dir is already per-job and needs no pruner.
