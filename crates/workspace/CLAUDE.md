# CLAUDE.md — crates/workspace

Parity port of Go's `internal/workspace` (Symphony v0.4.0). Read `src/lib.rs`'s top-of-file doc
comment first — it's the crate's map of what shipped in which porting task (W1/W2/W3) and why.

## File map (read together, not in isolation)

The Go package is one compilation unit split across `repo.go`/`manager.go`/`hooks.go`/etc.; this
crate mirrors that by hanging most methods off the single `Manager` struct across several files:

- `sanitize.rs` — `Workspace` struct + `sanitize_key` (identifier → safe dir name).
- `safety.rs` — lexical-only path helpers (`clean`/`join`/`dir`/`base`, ports of `path/filepath`)
  and `ensure_within_root`/`validate_launch`. Lexical means no filesystem access, no symlink
  resolution — see Pitfalls below.
- `repo.rs` — the git layer: `RepoKey` derivation, the bare-mirror cache, `ensure_from_repo`
  (worktree mode) and `ensure_clone_from_repo` (clone mode), `remove_worktree`.
- `manager.rs` — `Manager` construction, the per-repo lock registry, the legacy (empty-`repo_url`)
  create/remove path, and the `before_run`/`after_run`/`after_create`/`before_remove` hook wiring.
- `hooks.rs` — `HookRunner`: runs a hook via `bash -lc` with a timeout.
- `gc.rs` — `prune_stale_worktrees`: mtime-based GC across both workspace modes.
- `labeler.rs` — post-run best-effort PR labeling via the `gh` CLI (AIE-301).
- `gtguard.rs` + `gtguard/` — Graphite workflow guardrail injection (see below).

## Two workspace provisioning modes

`ensure_from_repo` (shared bare mirror + `git worktree add`, branch `symphony/<key>`) and
`ensure_clone_from_repo` (standalone `git clone`, no shared mirror) both live in `repo.rs` and
share path scheme, fatal-`after_create` semantics, and WIP-preserving reuse — but GC (`gc.rs`)
must tell them apart by directory shape alone (no separate on-disk marker):

- A top-level dir is a repo-namespace **parent** iff a sibling `.mirrors/<key>.git` exists
  (worktree mode) OR its name is 24-hex `RepoKey`-shaped, has no mirror, and has no `.git` of its
  own (clone mode — independent clones have no mirror).
- Otherwise it's a **leaf**: a legacy one-level hook-populated workspace, removed wholesale.

This distinction is load-bearing — `gc.rs`'s doc comments cite INF-418, a real regression where a
legacy identifier that happened to sanitize to a 24-hex string got misclassified as a namespace
parent and had its children pruned individually instead of being removed as one leaf. If you touch
the classification logic in `repo.rs` (`looks_like_repo_key`, `dir_is_git_checkout`) or `gc.rs`,
re-read that test (`hex_legacy_workspace_with_git_is_leaf`) before changing it.

## Pitfalls / invariants that aren't obvious from a single file

- **Mirror fetch uses the remote-tracking namespace, never `refs/heads/*`.** The bare mirror is
  configured with `remote.origin.fetch = +refs/heads/*:refs/remotes/origin/*` specifically so a
  pruning fetch (`fetch --prune origin`) can never delete the local `refs/heads/symphony/<key>`
  branches that live worktrees are on. Getting this backwards silently corrupts every in-progress
  worktree sharing that mirror — see `repo.rs`'s `ensure_mirror` doc comment and
  `ensure_from_repo_fetch_does_not_prune_symphony_branch`.
- **Path containment is lexical-only, symlink detection is separate.** `ensure_within_root` never
  touches the filesystem or resolves symlinks (`safety.rs`'s top comment explains why: it trusts a
  daemon-owned root with sanitized keys). Every create/remove/reuse path therefore does its own
  `symlink_metadata` (lstat, not stat) check to reject a planted symlink *before* following or
  reusing it. If you add a new path that touches an existing workspace dir, it needs this lstat
  guard too — `ensure_within_root` alone does not protect you.
- **Per-repo lock is released before hooks run, not held across them.** `repo_locks` (a
  `Mutex<HashMap<String, Arc<AsyncMutex<()>>>>`) serializes mirror mutations, but the lock is
  dropped before `after_create`/`before_remove` so concurrent same-repo workers aren't serialized
  across an arbitrarily long hook — it's only reacquired if a hook failure requires rolling back
  shared mirror admin state (`worktree remove --force` + `prune`).
- **GC's `live` callback is invoked *before* the per-repo lock, never under it** — `gc.rs`'s
  `LiveCheck` doc comment spells out the deadlock: in production `live` round-trips through the
  orchestrator control loop, which itself takes the same per-repo lock. Locking around `live()`
  would deadlock; the ordering is deliberate and covered by `live_called_outside_repo_lock`.
- **Hook timeout kills the whole process group, not just `bash`.** `hooks.rs` spawns each hook in
  its own process group (`process_group(0)`) so a timeout's `SIGKILL` reaches a backgrounded
  grandchild too — without it, a leaked background process holds the output pipe open and the reap
  hangs. This crate deliberately does NOT mirror Go's `WaitDelay` 10s "leaked pipe on a *successful*
  hook eventually reads as success" backstop; here that case reads as a timeout instead (see
  `hooks.rs`'s module doc for the PR note).
- **`gtguard/` is not a crate** — `gt-guard.sh` + `settings.local.json` are runtime assets copied
  **verbatim** from the Go reference and embedded into `gtguard.rs` via `include_bytes!` (Go's
  `//go:embed`). Edit the files under `gtguard/` directly (not generated, not a build script); a
  canary test (`embedded_assets_match_on_disk`) fails if the embedded bytes drift from disk. It
  uses `settings.local.json` rather than `settings.json` deliberately — Claude Code merges Local
  scope on top of Project scope, so a repo's own committed hooks are never clobbered, and Symphony
  (not Claude Code) writes the file so it stays out of commits even in a repo that doesn't gitignore
  it.
- **`git_flow` branch-prefix values are cross-process contracts, not this crate's to rename** — see
  root CLAUDE.md's Divergences section. `repo.rs` is where that contract is baked in (branch names,
  worktree admin paths).
- **`RepoKey` must stay byte-identical to Go's `crypto/sha256`.** `repo_key` in `repo.rs` is locked
  against a known digest (`repo_key_matches_go_sha256`) independent of having the Go binary
  available — don't "fix" the hex-truncation-to-24-chars scheme without checking that test.

## Testing this crate

- Tests shell out to real `git`, `bash`, and (in `labeler.rs`/`gtguard.rs`) a faked `gh`; they need
  those on `PATH`. `gtguard.rs`'s no-jq fallback test skips itself if `jq`/`cat`/`grep` aren't
  resolvable on the host — it's not exercising anything if it doesn't run.
  `remove_failure_wraps_err_workspace_remove` skips under `geteuid()==0` since permission bits
  can't block root.
  gt-guard.sh itself renders those tokens with regexes, not a shell parser — it is deliberately
  over-broad (blocks a keyword inside a string/comment) and does not follow aliases; that's a known
  scope boundary, not a bug to fix.
- `src/lib.rs`'s `testutil` module (not a separate file) is the shared scaffolding every other
  module's `#[cfg(test)]` block pulls from: a hand-rolled `TempDir` (Go's `t.TempDir()`), real-git
  origin builders, and the fake-`gh` harness. Rust 2024 forbids the `unsafe` `std::env::set_var`
  Go's `t.Setenv` relies on, so env injection for tests goes through explicit overlays instead
  (`Manager::gh_env_overlay`, `HookRunner::run_env`'s `extra` param) — never add a test that mutates
  the process environment directly.
