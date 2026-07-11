//! obslog — orchestrator-internal port of Go `internal/obslog` (per-ticket agent transcripts).
//!
//! Go's package has no dedicated Rust crate, so it lives here. O1 ported the *path* surface the
//! effective view and the API snapshot need ([`Store::new`] + [`Store::latest_path`]); O3 adds the
//! run transcript *writer* (Go `Store.Open` / `Run`), which creates timestamped `*.jsonl` files,
//! repoints `latest.jsonl`, and is opened by the per-run worker (the only thing that writes
//! transcripts).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use rhapsody_workspace::sanitize_key;

/// Roots per-ticket transcripts under a logs directory. Mirrors Go `obslog.Store`.
pub struct Store {
    dir: String,
    /// Monotonic per-store run counter appended to the timestamped transcript file name so two runs
    /// opened within the same nanosecond still get distinct files. Mirrors Go `Store.seq`
    /// (`atomic.Uint64`); [`Ordering::Relaxed`] suffices — only per-open uniqueness is needed, not
    /// cross-thread ordering.
    seq: AtomicU64,
}

impl Store {
    /// Returns a `Store` writing under `dir`. Mirrors Go `obslog.NewStore`.
    pub fn new(dir: impl Into<String>) -> Store {
        Store {
            dir: dir.into(),
            seq: AtomicU64::new(0),
        }
    }

    /// The stable path to a ticket's most recent transcript: `<dir>/<sanitized>/latest.jsonl`.
    /// Mirrors Go `Store.LatestPath`.
    ///
    /// Go derives the dir-safe token with a private `sanitize` that, per its own doc, "mirrors
    /// `workspace.SanitizeKey`" (replace every char outside `[A-Za-z0-9._-]` with `_`, then map the
    /// traversal-unsafe results `""`, `"."`, `".."` to `"_"`). This reuses the committed
    /// [`rhapsody_workspace::sanitize_key`] rather than duplicating that logic, so the two stay in
    /// lockstep.
    pub fn latest_path(&self, ticket: &str) -> String {
        Path::new(&self.dir)
            .join(sanitize_key(ticket))
            .join("latest.jsonl")
            .to_string_lossy()
            .into_owned()
    }

    /// Creates a new timestamped run transcript for `ticket` and repoints `latest.jsonl` at it. The
    /// caller must keep (and eventually [`Run::close`]/drop) the returned [`Run`]. Mirrors Go
    /// `Store.Open`.
    ///
    /// Transcripts can contain secrets echoed by the agent (tokens, env, etc.), so the tree is owned
    /// by the daemon and not world-readable: dirs `0700`, files `0600` (Go's exact modes). The
    /// `latest.jsonl` symlink repoint is best-effort — a symlink failure never fails the open (Go
    /// ignores both the remove and the symlink error).
    pub fn open(&self, ticket: &str) -> io::Result<Run> {
        let tdir = Path::new(&self.dir).join(sanitize_key(ticket));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&tdir)?;
        // Go: fmt.Sprintf("%s-%d", time.Now().UTC().Format("20060102T150405.000000000Z"), seq.Add(1)).
        // `fetch_add` returns the PRIOR value, so `+ 1` reproduces Go's post-increment (first run ⇒ 1).
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let base = format!("{}-{seq}", Utc::now().format("%Y%m%dT%H%M%S%.9fZ"));
        let run_path = tdir.join(format!("{base}.jsonl"));
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&run_path)?;
        let stderr_path = tdir.join(format!("{base}.stderr.log"));
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&stderr_path)?;
        // Repoint latest.jsonl → this run (best-effort; ignore remove/symlink failures). The link
        // target is the bare file name so it resolves within the ticket dir.
        let latest = tdir.join("latest.jsonl");
        let _ = std::fs::remove_file(&latest);
        let _ = std::os::unix::fs::symlink(format!("{base}.jsonl"), &latest);
        Ok(Run {
            path: run_path.to_string_lossy().into_owned(),
            stdout,
            stderr,
        })
    }
}

/// One agent run's transcript: a stdout `*.jsonl` stream + a stderr `*.stderr.log` sibling. Mirrors
/// Go `obslog.Run`.
pub struct Run {
    path: String,
    stdout: File,
    stderr: File,
}

impl Run {
    /// The concrete per-run transcript file (the timestamped `*.jsonl`, NOT the `latest.jsonl`
    /// alias). Mirrors Go `Run.Path`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// An owned writer for the run's stdout `*.jsonl` stream. Go returns the shared `*os.File` from
    /// `Run.Stdout`; Rust cannot hand out a second owner of the same `File`, so this returns an
    /// independent handle to the SAME file via `try_clone` (a `dup(2)` — writes through it and
    /// through the `Run`'s own handle both land at end-of-file under `O_APPEND`). The worker boxes it
    /// into the agent [`rhapsody_agent::Transcript`]; `try_clone` failure is surfaced so the caller
    /// can degrade to running without local logging.
    pub fn stdout(&self) -> io::Result<File> {
        self.stdout.try_clone()
    }

    /// An owned writer for the run's stderr sibling. See [`Run::stdout`] for the `try_clone`
    /// rationale. Mirrors Go `Run.Stderr`.
    pub fn stderr(&self) -> io::Result<File> {
        self.stderr.try_clone()
    }

    /// Flushes and closes both files. Mirrors Go `Run.Close` (which returns the first close error);
    /// the underlying `File`s also close on drop, so a caller that only needs best-effort teardown
    /// can simply drop the `Run` (as the worker does).
    pub fn close(mut self) -> io::Result<()> {
        let r1 = self.stdout.flush();
        let r2 = self.stderr.flush();
        r1?;
        r2
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testsupport::TempDir;

    // Mirrors Go `TestOpenUsesRestrictivePermissions`: the ticket dir is 0700 and both transcript
    // files are 0600 (they may hold secrets echoed by the agent).
    #[test]
    fn open_uses_restrictive_permissions() {
        let dir = TempDir::new();
        let s = Store::new(dir.path.clone());
        let run = s.open("MT-PERM").expect("open");

        let tdir = dir.child("MT-PERM");
        let dmode = std::fs::metadata(&tdir)
            .expect("stat tdir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "ticket dir mode");

        for p in [
            run.path().to_string(),
            run.path().strip_suffix(".jsonl").unwrap().to_string() + ".stderr.log",
        ] {
            let fmode = std::fs::metadata(&p)
                .expect("stat file")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(fmode, 0o600, "file {p} mode");
        }
        run.close().expect("close");
    }

    // Mirrors Go `TestOpenWritesAndLatestSymlink`: the run file holds the raw stdout line, the
    // latest.jsonl alias resolves to it, and the stderr sibling exists.
    #[test]
    fn open_writes_and_latest_symlink() {
        let dir = TempDir::new();
        let s = Store::new(dir.path.clone());
        let run = s.open("MT-1").expect("open");
        run.stdout()
            .expect("stdout")
            .write_all(b"{\"type\":\"system\"}\n")
            .expect("write stdout");
        run.stderr()
            .expect("stderr")
            .write_all(b"diag line\n")
            .expect("write stderr");
        let run_path = run.path().to_string();
        run.close().expect("close");

        let mut data = String::new();
        File::open(&run_path)
            .expect("open run file")
            .read_to_string(&mut data)
            .expect("read");
        assert!(
            data.contains("\"type\":\"system\""),
            "run file content = {data:?}"
        );

        let mut latest = String::new();
        File::open(s.latest_path("MT-1"))
            .expect("open latest")
            .read_to_string(&mut latest)
            .expect("read latest");
        assert_eq!(latest, data, "latest.jsonl must resolve to the run file");

        let stderr_path = run_path.strip_suffix(".jsonl").unwrap().to_string() + ".stderr.log";
        assert!(
            std::fs::metadata(&stderr_path).is_ok(),
            "stderr file missing"
        );
    }

    // Mirrors Go `TestOpenRepointsLatestOnSecondRun`: two runs get distinct files and latest.jsonl
    // points at the second.
    #[test]
    fn open_repoints_latest_on_second_run() {
        let dir = TempDir::new();
        let s = Store::new(dir.path.clone());
        let r1 = s.open("MT-1").expect("open r1");
        r1.stdout()
            .expect("stdout")
            .write_all(b"first\n")
            .expect("write");
        let p1 = r1.path().to_string();
        r1.close().expect("close r1");

        let r2 = s.open("MT-1").expect("open r2");
        r2.stdout()
            .expect("stdout")
            .write_all(b"second\n")
            .expect("write");
        let p2 = r2.path().to_string();
        r2.close().expect("close r2");

        assert_ne!(p1, p2, "two runs must have distinct files");
        let mut latest = String::new();
        File::open(s.latest_path("MT-1"))
            .expect("open latest")
            .read_to_string(&mut latest)
            .expect("read");
        assert_eq!(
            latest, "second\n",
            "latest.jsonl should point at the 2nd run"
        );
    }

    // Mirrors Go `TestOpenSanitizesTicketDir`: separators/spaces in the ticket collapse in the dir.
    #[test]
    fn open_sanitizes_ticket_dir() {
        let dir = TempDir::new();
        let s = Store::new(dir.path.clone());
        let run = s.open("team/MT 2").expect("open");
        assert!(
            std::fs::metadata(dir.child("team_MT_2")).is_ok(),
            "sanitized ticket dir missing"
        );
        run.close().expect("close");
    }

    // Mirrors Go `obslog` `TestLatestPath` + `TestSanitizeTicket`: the stable path is
    // `<dir>/<sanitized>/latest.jsonl`, and separators / spaces in the ticket collapse to `_`.
    #[test]
    fn latest_path_shape_and_sanitizes_ticket() {
        let s = Store::new("/logs");
        assert_eq!(s.latest_path("MT-1"), "/logs/MT-1/latest.jsonl");
        // Separator AND space become `_` (Go `sanitize("team/MT 9") == "team_MT_9"`).
        assert_eq!(s.latest_path("team/MT 9"), "/logs/team_MT_9/latest.jsonl");
    }

    // Mirrors Go `TestSanitizeRejectsTraversalSegments`: dot segments and the empty string collapse
    // to a single safe component, and a traversal attempt can never escape the ticket tree.
    #[test]
    fn latest_path_rejects_traversal_segments() {
        let s = Store::new("/logs");
        for bad in ["", ".", ".."] {
            assert_eq!(s.latest_path(bad), "/logs/_/latest.jsonl", "ticket {bad:?}");
        }
        // A traversal attempt is reduced to a single safe path component: no separators survive, so
        // the joined path stays within the ticket tree.
        let joined = s.latest_path("../../etc");
        assert!(joined.starts_with("/logs/"), "escaped tree: {joined}");
        let ticket = joined
            .strip_prefix("/logs/")
            .and_then(|r| r.strip_suffix("/latest.jsonl"))
            .expect("shape <dir>/<ticket>/latest.jsonl");
        assert!(
            !ticket.contains('/'),
            "ticket must be one component: {ticket:?}"
        );
        assert!(
            ticket != "." && ticket != "..",
            "ticket must be safe: {ticket:?}"
        );
    }
}
