# CLAUDE.md — crates/config

Parity port of Go `internal/config` (+ `internal/workflow`, `internal/prompt` as modules) — see
each file's top-of-file doc comment for the exact Go source it ports. No crate-specific build/test
commands beyond the root (`cargo test -p rhapsody-config`); this file is architecture + pitfalls.

## Pipeline order (enforced by call order, not the type system)

WORKFLOW.md text flows through five independent stages, called in this exact sequence by every
real caller (the orchestrator, and `tests/golden.rs`):

```
workflow::load → decode::decode → resolve::resolve → validate::validate(&mut _) → projects::resolve_projects
```

1. **`workflow.rs`** — splits WORKFLOW.md into YAML front matter + Markdown prompt body. `load`/
   `save`/`marshal`. Also owns the atomic-write helpers (`create_temp`, `write_temp_and_rename`)
   that `capabilities.rs`'s registry writer reuses — the shared `~/.rhapsody` file-write convention
   (temp file in the same dir, 0600, rename over).
2. **`decode.rs`** — front-matter map → typed `Config`, applying the `orStr`/`orInt`/`orBool`/
   `orSlice` field-default chain. Does **not** do `$VAR` expansion or path resolution — `api_key`
   and the three path fields (`workspace.root`, `logging.dir`, `storage.path`) are stored verbatim.
   Also owns the hand-rolled `parse_go_duration` — a faithful port of Go `time.ParseDuration`
   (`"2m0s"`, `"1.5h"`, unit table, overflow behavior). Don't replace it with a `humantime`/chrono
   parser; the golden tests pin its exact edge-case behavior (fractional units, `"0"` special case).
3. **`resolve.rs`** — `$VAR` env indirection + path normalization ONLY: `~` expansion, anchoring
   relatives to `workflow_dir`, absolutizing. Carries its own defaults for the three path fields
   (`~/.rhapsody/{workspaces,logs}`, `~/.rhapsody/rhapsody.db`) plus `retention_days: 30`, because it
   must also work on a hand-built `Config` that never went through `decode`. Ports a private
   `path/filepath`-equivalent lexicon (`clean`/`join`/`abs`/`is_abs`) — Unix-only, no volume names.
4. **`validate.rs`** — `ValidateDispatch` scheduler preflight. Takes `&mut Resolved`, not `&_`: it
   trims `projects[].slugs[]` **in place** so `resolve_projects` afterward sees the same normalized
   slugs Linear would report. Callers must run `validate` before `resolve_projects` for this reason
   — never reorder them.
5. **`projects.rs`** — the per-project override overlay. `effective_for`/`effective_of` layer a
   project's *set* fields over the top-level defaults (presence-based: non-empty wins, empty
   inherits), then materialize the `dependency_mode`/`workspace_mode`/`claim_mode`/
   `dep_mode_prompt_file` defaults **last**, so both the per-project and legacy/top-level (`project:
   None`) paths get them. `resolve_projects` fans each project's `slugs` out into one
   `ResolvedProject` per slug, sharing one `group` key (the project's first slug) so a per-project
   concurrency cap applies to the whole fanned group, not per slug.

`encode.rs` and `effective_json.rs` are separate downstream consumers, not part of the above chain:

- **`encode.rs`** — inverse of `decode`: typed `Config` → `Definition`. Round-trips to an
  *equivalent* config at the `resolve_projects` level, not always field-for-field — a single
  trivial project (one slug, no name/overrides, enabled) canonicalizes back to the legacy
  `tracker.project_slug` form. Serializes the full `Raw` tree (no `serde` `skip_serializing_if`,
  matching Go's untagged `raw` struct) then prunes empty fields afterward in one pass
  (`prune_empty`) — don't add per-field `omitempty` logic instead of extending that pass.
- **`effective_json.rs`** — the `GET /api/v1/config` view. Deliberately re-`decode`s the
  `Definition` internally rather than taking a `Resolved` — the response's `global`/`projects` must
  show pre-resolution values (unexpanded `~`, `null` retention) to match the captured Go fixtures.
  Don't "fix" this to take a `Resolved`; it would break `tests/golden.rs`.

## Conventions specific to this crate

- **Raw/typed split** (`model.rs`): every YAML-facing `Raw*` struct field that has a config default
  is `Option<T>` (or relies on `Vec::is_empty()`), so `decode` can tell "absent" from "explicit
  zero/false/empty" — mirroring Go's pointer fields. When adding a new knob, decide up front which
  of the three stages (decode/resolve/projects) owns its default; putting it in the wrong stage is
  the most common way to silently diverge from Go, since the Go split is exactly the same three-way
  cut across `config.go`/`resolve.go`/`projects.go`.
- **Error strings are the observable contract.** Every `ConfigError`/`ValidationError`/
  `WorkflowError`/`RenderError` variant's `Display` reproduces a Go sentinel token byte-for-byte
  (`workflow_parse_error`, `unsupported_tracker_kind: "jira"`, `template_render_error: …`). These
  surface verbatim in daemon logs and the config HTTP API's error bodies — treat a wording change
  here as a breaking API change, not a cosmetic one.
- **`allow_handoff`** (`Mcp`) is a Rhapsody-only addition (TRA-242, no Go v0.4.0 counterpart) that
  is deliberately *not* surfaced in `effective_json`'s response or round-tripped through `encode`'s
  pruning, so the config goldens stay byte-identical to Go. If you add another Rhapsody-only field,
  follow the same pattern (decode it, but keep it out of the parity-checked surfaces) unless you
  intend to add a new Divergences entry.
- **`~/.rhapsody/*` path defaults** (`workspace.root`, `logging.dir`, `storage.path`) are this
  crate's implementation of the TRA-238 divergence from Go's `~/.symphony/*` — see root README
  Divergences. They're set in `resolve.rs`, not `decode.rs`.

## Testing

- `tests/golden.rs` is the crate's parity gate: it drives three committed capture workflows
  (`harness/capture/workflows/{minimal,full,graphite}.md`, referenced via
  `CARGO_MANIFEST_DIR/../../harness/...`) through the full `decode → resolve → validate →
  effective_json::render` pipeline and diffs against `harness/fixtures/config/*.json`, normalized
  via `harness_fixtures::normalize_with_home`. A failure here means real behavior drifted from the
  Go reference — fix the port, don't edit the fixture (fixtures are recaptured with `make fixtures`
  from an operator machine, never hand-edited).
- `prompt.rs`'s own test module reads the same `harness/capture/workflows/*.md` files via a
  relative path (not `CARGO_MANIFEST_DIR`-anchored) — it depends on `cargo test` being invoked with
  the crate directory as the working directory, which `cargo test -p rhapsody-config` already
  guarantees.
- Most modules mirror one Go `_test.go` file almost 1:1, down to individual test names in comments
  (`// Mirrors Go TestDecodeAppliesDefaults`). When porting a new Go test, follow that convention —
  it's what lets someone diff this crate's test list against the Go source directly.
