//! opencode — the opencode CLI agent backend (STUDIO-902). Rhapsody-only; the frozen Go reference
//! runs exactly one backend, so nothing here is a parity port.
//!
//! Rhapsody drives `opencode run --format json` headlessly as a subprocess and maps its JSONL
//! output to the same normalized [`crate`] agent events the claude backend produces. The module
//! split mirrors [`crate::claude`]'s so the two read the same way:
//!
//! * [`args`] — [`Config`] plus argv construction ([`build_args`]).
//! * [`parse`] — one JSONL line → a normalized [`crate::Event`] ([`classify`]), plus per-step usage.
//! * [`mcpinject`] — the daemon's MCP server, and the tool-name spelling the prompts need.
//! * [`state`] — ⚠️ the per-run `XDG_DATA_HOME` isolation, without which concurrent turns are LOST.
//! * [`runner`] — the subprocess runner ([`Runner`]).
//!
//! ## Why this exists before the slices that were meant to precede it
//!
//! The pluggable-harnesses plan (`~/.rhapsody/docs/pluggable-harnesses-design.md` §9) reaches a
//! second harness at slice 8, after store/event decoupling (3), the resolution chain (4),
//! capabilities and refusal (5), the kill timer (6) and spend budgets (7). STUDIO-902 took it early
//! and deliberately, to move implementation onto a different billing pool; slices 3–7 are **not**
//! built on the way, and each place this adapter would have used one says so and names what it does
//! instead. It is meant to be cheap to subsume, not to anticipate.
//!
//! ## What the spike measured, and where each finding landed
//!
//! Everything below is `[RAN]` against opencode 1.18.30 on a real provider. The captures are in
//! `harness/harness-spike/opencode/` (with the exact command that produced each in that tree's
//! README); the written findings are `~/.rhapsody/docs/STUDIO-869-harness-spike-findings.md` §4 and
//! §A.3. Every one of these is a test in the module named beside it.
//!
//! | Finding | Where it landed |
//! |---|---|
//! | Pure JSONL on stdout, stderr 0 bytes even on failure | [`parse`] |
//! | **No terminal result event** — `step_finish{reason:"stop"}` then EOF | [`parse`], [`runner`] |
//! | Usage is per STEP and must be summed | [`parse::add_usage`] |
//! | `Both` failure signal: exit 1 AND an in-band `error` with `statusCode`/`isRetryable` | [`parse::Failure`] |
//! | Resume is same-flags, `-s <id>` | [`args::build_args`] |
//! | ⚠️ Sharing a state dir LOSES TURNS (0/10 on a fresh one); isolation is `XDG_DATA_HOME` | [`state`] |
//! | ⚠️ Credentials live INSIDE the redirected directory | [`state::RunState::provision`] |
//! | ⚠️ Tools are spelled `<server>_<tool>`, not `mcp__<server>__<tool>` | [`mcpinject::rewrite_tool_names`] |
//! | The prompt is an argv positional; stdin is closed at start | [`args`], [`runner`] |
//! | Tool children escape the process group | [`runner`], via [`crate::proctree::kill_tree`] |
//!
//! ## The session-id question (design §6.2), answered for THIS harness
//!
//! STUDIO-872 found that per-run isolation costs goose its session-id uniqueness — two isolated
//! goose runs both report `20260912_1` — which is why §6.2 asks whether an isolated session id can
//! still serve as an identity key, and leaves it to slice 9. **That question does not bind this
//! adapter, and the answer is measured rather than argued.** opencode mints a random `ses_…` id per
//! session rather than numbering per state directory, so isolation costs it nothing:
//! `harness/harness-spike/opencode/concurrency-trials-isolated-xdg.txt` records **ten isolated
//! turns with ten distinct ids**, each trial's own verdict line confirming the pair was disjoint.
//! So `session_uuid` remains a usable identity key for opencode, and [`state`] can isolate every
//! run without trading anything away. Slice 9 still owns the goose case, which is the one where the
//! trade is real.

pub mod args;
pub mod mcpinject;
pub mod parse;
pub mod runner;
pub mod state;

pub use args::{Config, auto_approve_enabled, build_args};
pub use mcpinject::{INJECTED_CONFIG_NAME, SERVER_KEY, inject_daemon_mcp, rewrite_tool_names};
pub use parse::{Classified, Failure, add_usage, classify};
pub use runner::Runner;
pub use state::RunState;

/// A RAII scratch directory for this module's tests.
///
/// Hand-rolled rather than `tempfile`, matching the crate's existing convention (`claude::runner`'s
/// tests carry their own). ⚠️ It differs from that one in two ways on purpose, because the bug this
/// adapter exists to avoid is two runs sharing a directory: the name carries a nanosecond stamp in
/// addition to pid+counter, so a RECYCLED pid cannot reproduce an earlier name, and it is created
/// with `create_dir` rather than `create_dir_all`, so a collision fails loudly instead of silently
/// adopting a directory that is already in use.
#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new() -> TempDir {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "rhapsody-opencode-test-{}-{nanos}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir(&dir).expect("create scratch dir");
            TempDir { dir }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
