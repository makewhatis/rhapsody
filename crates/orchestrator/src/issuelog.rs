//! issuelog — parity port of Go `internal/orchestrator/issuelog.go`.
//!
//! The per-run `/log` engine: [`Orchestrator::run_transcript`] looks up a run by id, resolves its
//! recorded per-run `transcript_path`, and humanizes that concrete file into log entries (fed to the
//! shared `/runs/{id}/transcript` renderer in P6). [`humanize_transcript_file`] is the shared engine
//! behind that endpoint, live or finished. Both mirror the Go source's exact tolerance rules: a
//! missing/pruned transcript resolves to an empty (non-nil) slice with `found = true` (200, not 404),
//! and a store read error is logged and surfaced as `found = false` (the HTTP handler maps that to a
//! 404, never a 500-from-disk).
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go returns `([]agent.LogEntry, bool)`; the Rust port returns `(Vec<LogEntry>, bool)` — an
//!     empty `Vec` stands in for Go's non-nil empty slice, so "no transcript" reads as no entries.
//!   * Go reads the raw stream-json with a `bufio.Reader` (1 MiB buffer, no `Scanner` 64 KiB cap) to
//!     tolerate multi-MB lines; the Rust port uses a [`BufReader`] + [`BufRead::read_until`], which
//!     has no line-length cap either, so a huge line and a partial (unterminated) final line are both
//!     tolerated identically.
//!   * The store read error is logged via `tracing` (as the sibling crates do) instead of a threaded
//!     `slog` logger.

use std::fs::File;
use std::io::{BufRead, BufReader};

use rhapsody_agent::{self as agent, humanize_stream_line};
use rhapsody_store::{self as store};

use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;

/// Caps the `/log` response to the last N humanized entries (design §3). Mirrors Go `maxLogEntries`.
const MAX_LOG_ENTRIES: usize = 1000;

impl Orchestrator {
    /// Looks up a run by id, reads its recorded per-run `transcript_path`, and humanizes that concrete
    /// file into log entries (fed to the shared `/runs/{id}/transcript` renderer). `found == false`
    /// means no such run row (⇒ 404). A run whose transcript file was pruned or never recorded
    /// resolves to an empty (non-nil) slice with `found == true`, so the endpoint returns 200 with
    /// `entries: []` rather than a 404. The store read is best-effort: a store error is logged and
    /// surfaced as `found == false` (the HTTP handler maps that to a 404, never a 500-from-disk).
    ///
    /// Live-early fallback (RUNNING runs only): a live run's concrete per-run `transcript_path` isn't
    /// persisted until the worker opens its transcript (`ev_transcript_opened`). Until then
    /// `run.transcript_path` is empty; for a still-running run the ticket's `latest.jsonl` symlink
    /// points at THIS run's file, so fall back to it and a just-started live run still streams output.
    /// We gate on `outcome == "running"`: for a FINISHED run with no persisted path (pruned / never
    /// recorded), `latest.jsonl` would point at a LATER attempt on the same issue, so we must NOT fall
    /// back — return empty rather than another run's transcript. [`transcript_path`] returns `""` when
    /// logging is off ⇒ `humanize_transcript_file("")`. Mirrors Go `RunTranscript`.
    ///
    /// [`transcript_path`]: Orchestrator::transcript_path
    pub fn run_transcript(&self, run_id: i64) -> (Vec<agent::LogEntry>, bool) {
        let run = match self.store().get_run(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return (Vec::new(), false),
            Err(e) => {
                tracing::error!(run_id, error = %e, "run transcript lookup failed");
                return (Vec::new(), false);
            }
        };
        let mut path = run.transcript_path;
        if path.is_empty() && run.outcome == store::OUTCOME_RUNNING {
            path = self.transcript_path(&run.issue_identifier);
        }
        (humanize_transcript_file(&path), true)
    }
}

impl ControlHandle {
    /// The daemon's off-loop `GET /api/v1/runs/{id}/transcript` surface — the [`ControlHandle`]
    /// mirror of [`Orchestrator::run_transcript`], reading the run row + humanizing its recorded
    /// transcript through the shared store OFF the control loop (so a multi-MB read never blocks
    /// dispatch, unlike routing it through the loop). It serves the persisted per-run
    /// `transcript_path`; unlike the loop-owned method it omits the RUNNING-run `latest.jsonl`
    /// live-early fallback (which needs the loop-owned `eff.log_dir`), so a just-started live run whose
    /// worker has not yet opened its transcript reads as `entries: []` until the path persists — an
    /// accepted narrow gap for the P6 assembly. Mirrors Go `RunTranscript` (persisted-path branch).
    pub fn run_transcript(&self, run_id: i64) -> (Vec<agent::LogEntry>, bool) {
        let run = match self.store().get_run(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return (Vec::new(), false),
            Err(e) => {
                tracing::error!(run_id, error = %e, "run transcript lookup failed");
                return (Vec::new(), false);
            }
        };
        (humanize_transcript_file(&run.transcript_path), true)
    }
}

/// Reads the raw stream-json transcript at `path`, humanizes each line via the shared
/// [`humanize_stream_line`], and returns the resulting entries (oldest → newest, capped to the last
/// [`MAX_LOG_ENTRIES`]). It is the shared engine behind the per-run `/runs/{id}/transcript` endpoint
/// (a concrete per-run `*.jsonl` file, whether the run is live or finished). An empty path, a
/// missing/unreadable file, or a partially-written final line are all tolerated: it always returns a
/// non-nil, possibly empty slice (so callers never panic on nil and "no transcript" reads as no
/// entries). Mirrors Go `humanizeTranscriptFile`.
pub(crate) fn humanize_transcript_file(path: &str) -> Vec<agent::LogEntry> {
    if path.is_empty() {
        return Vec::new();
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    // claude can emit multi-MB lines, so read through a 1 MiB `BufReader` + `read_until` (no
    // `Scanner`-style 64 KiB cap; Rust's `read_until` grows the buffer to the full line).
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut out: Vec<agent::LogEntry> = Vec::with_capacity(256);
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break, // EOF (a final unterminated line was already processed the prior pass).
            Ok(_) => out.extend(humanize_stream_line(&line)),
            Err(_) => break,
        }
    }
    // Cap to the last MAX_LOG_ENTRIES (Seq assignment happens in the HTTP layer).
    if out.len() > MAX_LOG_ENTRIES {
        out.drain(..out.len() - MAX_LOG_ENTRIES);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunStart, Sqlite, Store, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::obslog::Store as TranscriptStore;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{TempDir, empty_effective};

    /// Writes `lines` (each already a JSON object) to a fresh temp `*.jsonl`, returning the path, so
    /// the humanize-engine tests don't need the obslog store. Mirrors Go `writeJSONL`.
    fn write_jsonl(dir: &TempDir, lines: &[&str]) -> String {
        let path = dir.child("20260101T000000.000000000Z-1.jsonl");
        let mut f = File::create(&path).expect("create jsonl");
        f.write_all((lines.join("\n") + "\n").as_bytes())
            .expect("write jsonl");
        path
    }

    // Mirrors Go `TestHumanizeTranscriptFileKinds`: drives the shared humanize engine over a small
    // fixture and asserts the per-kind classification + tool attribution.
    #[test]
    fn humanize_transcript_file_kinds() {
        let dir = TempDir::new();
        let path = write_jsonl(
            &dir,
            &[
                r#"{"type":"system","subtype":"init"}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"output"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"type":"result"}"#,
            ],
        );
        let entries = humanize_transcript_file(&path);
        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "event",
                "thinking",
                "tool_use",
                "tool_result",
                "text",
                "event"
            ]
        );
        assert_eq!(entries[2].tool, "Bash");
        assert_eq!(entries[2].text, "command=ls");
    }

    // Mirrors Go `TestHumanizeTranscriptFileMissing`: an empty path and a missing file both yield a
    // non-nil, empty slice, so "no transcript" reads as no entries (never a nil panic).
    #[test]
    fn humanize_transcript_file_missing() {
        assert!(humanize_transcript_file("").is_empty());
        let dir = TempDir::new();
        let missing = dir.child("nope.jsonl");
        assert!(humanize_transcript_file(&missing).is_empty());
    }

    // Mirrors Go `TestHumanizeTranscriptFileHugeAndPartial`: proves the buffered-reader path (no
    // 64 KiB cap) handles a multi-MB line and a partially-written (unterminated, invalid) final line.
    #[test]
    fn humanize_transcript_file_huge_and_partial() {
        let dir = TempDir::new();
        let huge = "x".repeat(2 << 20); // ~2 MiB single line
        let big = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{huge}"}}]}}}}"#
        );
        let path = dir.child("huge.jsonl");
        // One valid ~2 MiB line + a partial (no newline, invalid JSON) final line.
        let content = format!("{big}\n{}", r#"{"type":"assist"#);
        File::create(&path)
            .and_then(|mut f| f.write_all(content.as_bytes()))
            .expect("write huge");
        let entries = humanize_transcript_file(&path);
        assert_eq!(entries.len(), 1, "want 1 text entry, got {entries:?}");
        assert_eq!(entries[0].kind, "text");
        // The huge line is read via the buffered reader (no 64 KiB cap) and the partial final line is
        // tolerated. Prose is generously capped, so a multi-MB dump never reaches the client.
        assert!(
            entries[0].text.chars().count() <= 4096,
            "text len must be capped well under the 2 MiB input"
        );
    }

    /// An orchestrator with `eff.log_dir` + `eff.transcripts` set to `dir`/`obs`, on `store`. Mirrors
    /// the `New(...)` + `o.eff = &effective{logDir, transcripts}` + `o.SetStore(mem)` setup the Go
    /// `TestRunTranscript*` cases hand-build.
    fn transcript_orch(
        dir: &str,
        obs: Arc<TranscriptStore>,
        store: Arc<dyn Store + Send + Sync>,
    ) -> Orchestrator {
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.log_dir = dir.to_string();
        eff.transcripts = obs;
        let mut o = Orchestrator::new("");
        o.eff = Some(eff);
        o.set_store(store);
        o
    }

    // Mirrors Go `TestRunTranscriptFallsBackToLatestWhenRowPathEmpty`: a live run whose store row has
    // no transcript_path yet (the worker hasn't reported it) falls back to the ticket's latest.jsonl,
    // so a just-started live run streams output instead of showing empty.
    #[test]
    fn run_transcript_falls_back_to_latest_when_row_path_empty() {
        let dir = TempDir::new();
        let obs = Arc::new(TranscriptStore::new(&dir.path));
        let run = obs.open("MT-1").expect("obslog open");
        writeln!(
            &run.stdout().expect("stdout"),
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"live line"}}]}}}}"#
        )
        .expect("write stdout");
        run.close().expect("obslog close");

        let mem: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"));
        let id = mem
            .start_run(RunStart {
                issue_identifier: "MT-1".to_string(), // NO transcript_path persisted yet
                ..Default::default()
            })
            .expect("start_run");

        let o = transcript_orch(&dir.path, obs, mem);
        let (entries, found) = o.run_transcript(id);
        assert!(found, "run_transcript found=false for an existing run");
        assert_eq!(
            entries.len(),
            1,
            "expected the latest.jsonl line via fallback"
        );
        assert_eq!(entries[0].kind, "text");
        assert_eq!(entries[0].text, "live line");
    }

    // Mirrors Go `TestRunTranscriptNoFallbackForFinishedRun`: a FINISHED run with no persisted
    // transcript_path must NOT fall back to the ticket's latest.jsonl (which points at a LATER attempt
    // on the same issue) — it returns empty rather than another run's output. Guards the
    // outcome=="running" gate on the live-early fallback.
    #[test]
    fn run_transcript_no_fallback_for_finished_run() {
        let dir = TempDir::new();
        let obs = Arc::new(TranscriptStore::new(&dir.path));
        let run = obs.open("MT-1").expect("obslog open");
        writeln!(
            &run.stdout().expect("stdout"),
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"LATER attempt output"}}]}}}}"#
        )
        .expect("write stdout");
        run.close().expect("obslog close");

        let mem: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"));
        let id = mem
            .start_run(RunStart {
                issue_identifier: "MT-1".to_string(),
                ..Default::default()
            })
            .expect("start_run");
        // Finish the run WITHOUT a transcript_path (pruned / never recorded → column stays empty).
        mem.end_run(
            id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.to_string(),
                ended_at: "2026-06-03T11:00:00Z".to_string(),
                ..Default::default()
            },
        )
        .expect("end_run");

        let o = transcript_orch(&dir.path, obs, mem);
        let (entries, found) = o.run_transcript(id);
        assert!(
            found,
            "run_transcript found=false for an existing finished run"
        );
        assert!(
            entries.is_empty(),
            "a finished run with no persisted path must NOT fall back to latest.jsonl, got {entries:?}"
        );
    }
}
