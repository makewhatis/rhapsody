//! liveness — orchestrator-internal port of Go `internal/liveness`.
//!
//! Reports whether an agent's process group is doing work, used (from O5 onward) to tell a
//! quietly-working run apart from a wedged one. Go's package has no dedicated Rust crate, so it
//! lives here — the orchestrator is its sole consumer.
//!
//! Go splits this across build tags: `sampler_linux.go` reads `/proc` (the real sampler),
//! `sampler_stub.go` is the `!linux` no-op. This Rust port keeps ONE always-compiled `/proc` reader
//! ([`group_cpu`] / [`parse_stat`]): on a host without a readable `/proc` (macOS — the CI/target
//! platform — has none) it returns `None`, byte-for-byte the same observable behavior as Go's
//! `!linux` `stubSampler` (`GroupCPU → (0, false)`), so the orchestrator still degrades to "assume
//! alive". Compiling the reader on every target (rather than build-tag-gating it like Go) lets the
//! `/proc`-parsing parity tests (`sampler_linux_test.go`) run under the macOS CI via a synthetic
//! proc root — exactly as Go's linux tests override `procRoot` — instead of being skipped.

use std::path::Path;
use std::sync::Arc;

/// The `/proc` mount point read by the platform sampler. Mirrors Go's `procRoot` package var; the
/// parity tests pass their own synthetic root to [`group_cpu`] directly (Go overrides the var).
const PROC_ROOT: &str = "/proc";

/// Reports cumulative process-group CPU usage. Mirrors Go `liveness.Sampler`.
pub trait Sampler: Send + Sync {
    /// Returns the summed user+system CPU time, in clock ticks, of every process whose
    /// process-group id equals `pgid`. `None` when the value cannot be read (e.g. no readable
    /// `/proc` on this OS, or `/proc` is unreadable), so the orchestrator degrades to "assume
    /// alive". A readable group with no live members returns `Some(0)`. Mirrors Go
    /// `GroupCPU(pgid int) (ticks uint64, ok bool)` — the `(u64, bool)` tuple collapses to
    /// `Option<u64>`.
    fn group_cpu(&self, pgid: i32) -> Option<u64>;
}

/// The `/proc`-backed sampler. Mirrors Go `linuxSampler` (and, where `/proc` is absent, Go's
/// `stubSampler`: `group_cpu` returns `None`). Reads `PROC_ROOT`.
struct ProcSampler;

impl Sampler for ProcSampler {
    fn group_cpu(&self, pgid: i32) -> Option<u64> {
        group_cpu(Path::new(PROC_ROOT), pgid)
    }
}

/// Returns the platform sampler (the `/proc` reader). Mirrors Go `liveness.NewSampler`. On a host
/// without a readable `/proc` — macOS, the CI/target platform — every `group_cpu` call returns
/// `None`, matching Go's `!linux` stub selection.
pub fn new_sampler() -> Arc<dyn Sampler> {
    Arc::new(ProcSampler)
}

/// Summed user+system CPU ticks of every process whose process-group id equals `pgid`, read from a
/// `/proc`-shaped tree rooted at `proc_root`. `None` when the root is unreadable (no `/proc`); a
/// readable root with no matching members returns `Some(0)`; a process that exits between listing
/// and reading its `stat` is skipped. Mirrors Go `groupCPU`.
fn group_cpu(proc_root: &Path, pgid: i32) -> Option<u64> {
    let entries = std::fs::read_dir(proc_root).ok()?;
    let mut total: u64 = 0;
    for e in entries.flatten() {
        // Only `<pid>` directories (Go: `IsDir` + `strconv.Atoi(Name())`).
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.parse::<i64>().is_err() {
            continue; // not a `<pid>` directory
        }
        let Ok(data) = std::fs::read_to_string(e.path().join("stat")) else {
            continue; // process exited between read_dir and read
        };
        if let Some((grp, utime, stime)) = parse_stat(&data)
            && grp == pgid
        {
            total += utime + stime;
        }
    }
    Some(total)
}

/// Extracts pgrp (field 5), utime (field 14) and stime (field 15) from a `/proc/<pid>/stat` line.
/// Field 2 (comm) is parenthesized and may itself contain spaces and `)`, so the fields that follow
/// the FINAL `)` are parsed. `None` on any malformed line. Mirrors Go `parseStat`.
fn parse_stat(s: &str) -> Option<(i32, u64, u64)> {
    // Everything after the last `)`: an absent `)`, or a `)` as the final byte, both yield no
    // parseable fields (Go guards `rparen < 0 || rparen+1 >= len(s)`).
    let rest_str = &s[s.rfind(')')? + 1..];
    // rest[0]=state(3) rest[1]=ppid(4) rest[2]=pgrp(5) ... rest[11]=utime(14) rest[12]=stime(15)
    let rest: Vec<&str> = rest_str.split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    let pgrp = rest[2].parse::<i32>().ok()?;
    let utime = rest[11].parse::<u64>().ok()?;
    let stime = rest[12].parse::<u64>().ok()?;
    Some((pgrp, utime, stime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    fn write_stat(root: &str, pid: i32, line: &str) {
        let dir = std::path::Path::new(root).join(pid.to_string());
        std::fs::create_dir_all(&dir).expect("mkdir pid dir");
        std::fs::write(dir.join("stat"), line).expect("write stat");
    }

    // Mirrors Go `TestGroupCPUSumsMatchingGroup`: sums utime+stime across every member of the
    // process group, parsing fields after the LAST `)` (comm may contain spaces and `)`); a
    // non-matching group is ignored; a readable root with no members returns `Some(0)`.
    #[test]
    fn group_cpu_sums_matching_group() {
        let dir = TempDir::new();
        let root = dir.path.clone();
        // fields: pid (comm) state ppid pgrp session tty tpgid flags minflt … utime stime …
        write_stat(
            &root,
            100,
            "100 (claude) S 1 100 100 0 -1 4194304 0 0 0 0 10 5 0 0\n",
        );
        // comm contains spaces and a `)`: must parse after the LAST `)`.
        write_stat(
            &root,
            101,
            "101 (weird ) name) S 1 100 100 0 -1 0 0 0 0 0 20 7 0 0\n",
        );
        // different group: must be ignored.
        write_stat(
            &root,
            200,
            "200 (other) S 1 200 200 0 -1 0 0 0 0 0 99 99 0 0\n",
        );

        let p = Path::new(&root);
        assert_eq!(group_cpu(p, 100), Some(42), "(10+5)+(20+7)");
        assert_eq!(group_cpu(p, 200), Some(198));
        assert_eq!(group_cpu(p, 999), Some(0), "readable proc, no members");
    }

    // Mirrors Go `TestGroupCPUUnreadableRoot`: a missing/unreadable `/proc` yields `None`
    // (`ok=false`) so callers degrade to "assume alive".
    #[test]
    fn group_cpu_unreadable_root() {
        let dir = TempDir::new();
        let missing = std::path::Path::new(&dir.path).join("does-not-exist");
        assert_eq!(group_cpu(&missing, 1), None);
    }

    // Mirrors Go `TestParseStatFailures`: every malformed line returns `None`.
    #[test]
    fn parse_stat_failures() {
        let cases = [
            ("short line", "100 (claude) S 1 100"), // < 13 fields after `)`
            (
                "no closing paren",
                "100 (claude S 1 100 100 0 -1 0 0 0 0 0 10 5",
            ), // never terminated
            (
                "non-numeric pgrp",
                "100 (x) S 1 abc 100 0 -1 0 0 0 0 0 10 5",
            ),
            (
                "non-numeric utime",
                "100 (x) S 1 100 100 0 -1 0 0 0 0 0 xx 5",
            ),
            ("empty", ""),
        ];
        for (name, line) in cases {
            assert!(parse_stat(line).is_none(), "{name}: want None");
        }
    }

    // The default platform sampler on a host without `/proc` (macOS — the CI/target) reports
    // unreadable, so stall detection degrades to "assume alive" — the parity mirror of Go's `!linux`
    // `stubSampler` selection. Gated off Linux, where `/proc` exists and the sampler reads it.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn new_sampler_assumes_alive_without_proc() {
        let s = new_sampler();
        assert_eq!(s.group_cpu(std::process::id() as i32), None);
        assert_eq!(s.group_cpu(1), None);
    }
}
