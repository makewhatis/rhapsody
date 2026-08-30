# CLAUDE.md — harness/fixtures

No manifest — this directory *is* the data: committed goldens captured by `harness/capture/capture.sh`
(see root CLAUDE.md for the `make fixtures` invocation and the recapture-not-hand-edit rule, and
`harness/CLAUDE.md`'s capture/ section for how it works and which Go tree it reads). If a golden test
goes red, that's your code changing the daemon's real output (a porting bug) or `normalize.sh`/
`normalize()` drifting (fix the rule) — not that the `.json` is wrong.

Consumers: every file here is loaded through `crates/harness-fixtures` (`fixtures_dir()` / `load()`
/ `load_json()`), never read directly by porting-crate tests. See that crate's CLAUDE.md for the
normalize/canary contract — this file only documents what each fixture *is*.

## What each fixture represents

Every file is a `grab()`'d snapshot of one live HTTP response (or a raw `sqlite3` dump) from the
reference daemon during a `make fixtures` run — trace exact provenance in `capture/capture.sh`
if you need to know precisely which scenario produced a given byte.

| Path | Source |
|---|---|
| `schema.sql` | `sqlite3 .schema` on the capture DB, sqlite_sequence stripped. 6 tables: `runs`, `events`, `retry_queue`, `claims`, `totals`, `run_messages`. `crates/harness-fixtures`'s `canary_schema_has_all_tables` hardcodes that count — bump it in the same change if a table is added, don't just let it go red. |
| `config/{minimal,full,graphite}.json` | `GET /api/v1/config` after booting each of `capture/workflows/{minimal,full,graphite}.md` — the three WORKFLOW.md shapes the config loader must parity-match. |
| `api/config.json` | Same endpoint, from the `minimal` scenario's run — byte-identical to `config/minimal.json` by construction (both are `grab()`s of the same live response in the same boot). If you ever see them diverge, that's a capture bug, not a hand-fixable typo — investigate `capture.sh`, don't patch one file to match the other. |
| `api/state.json`, `api/projects.json`, `api/history.json`, `api/metrics.json`, `api/events.json`, `api/logs.json` | `GET /api/v1/{state,projects,history,metrics,events,logs}` after the minimal scenario's one run completes. |
| `api/run_detail.json` | `GET /api/v1/runs/1` for the successful run. `run_detail_error.json` / `run_detail_stalled.json` are the same endpoint captured after forcing the run into `outcome:"failed"` via `fake-claude-error` / a hung/killed turn — see `harness/CLAUDE.md`'s stubs/ section for those env-var knobs. |
| `runs/success.jsonl`, `runs/error.jsonl`, `runs/stalled.jsonl` | `GET /api/v1/runs/1/events` for each of the three outcomes above. |
| `runs/success_transcript.jsonl` | `GET /api/v1/runs/1/transcript` for the successful run. |
| `db/go-daemon.db` | `VACUUM INTO` of the capture DB — the one binary golden that isn't `diff -r`-stable across runs; see `harness/CLAUDE.md`'s capture/ section for the determinism contract and why. |
| `db/go-daemon-rows.json` | `SELECT * ... ORDER BY 1` from every table in `go-daemon.db`, normalized. |

**Naming gotcha**: the four `runs/*.jsonl` files are *not* newline-delimited JSON despite the
extension — each is one pretty-printed JSON object (`{"events": [...], "run_id": ...}`, or
`{"entries": [...], "run_id": ..., "generated_at": ...}` for the transcript). The extension mirrors
the live endpoint's URL shape (`.../events`, `.../transcript`), not the file's actual format; don't
write a line-oriented parser against these expecting one JSON value per line.

See `harness/CLAUDE.md`'s capture/ section for which files the TRA-238 rewrite touches and why
every other golden here stays byte-exact Go output — not repeated here.
