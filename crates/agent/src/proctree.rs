//! proctree — harness-agnostic containment for the WHOLE process tree a coding agent creates.
//!
//! Rhapsody spawns its agent into a fresh process group and SIGKILLs that group when a run is
//! stopped, timed out or stalled (STUDIO-840, Go `syscall.Kill(-cmd.Process.Pid, SIGKILL)`). The
//! group turns out to be the wrong containment boundary, because the child can LEAVE it: STUDIO-869
//! measured all three harnesses Rhapsody can drive — Claude Code 2.1.267, opencode 1.18.30 and
//! codex-cli 0.153.4 — calling `setpgid` on the shell they run a tool command in. The leader always
//! died, the injected `rhapsodyd` MCP server (which stays in the group) always died with it, and
//! the model's own `/bin/zsh` → `/bin/bash` → `sleep` chain always survived in a group of its own:
//!
//! ```text
//! [claude] leader rc=-9
//! [claude] SURVIVORS AFTER kill(-96368, SIGKILL): 3
//! ```
//!
//! That is STUDIO-840's symptom — an operator told the work stopped while it is not — reached by a
//! second cause, and the dangerous survivor is not a stray `sleep` but a `git push` or a
//! `gh pr create` completing after the run was declared dead. It is a property of every harness, so
//! the fix lives HERE and not in [`crate::claude`]: a future opencode or codex backend gets the
//! same containment by calling [`kill_tree`] instead of killing a group.
//!
//! **The containment shape, and its bound.** Of the two shapes STUDIO-869 named, this is the
//! portable one: walk the descendant tree at kill time and signal every process group it spans. It
//! needs no spawn-side change and no platform primitive — macOS has no `PR_SET_PDEATHSIG`, so the
//! supervisor shape would need a `kqueue`/`NOTE_EXIT` babysitter process per run, which is a great
//! deal more machinery for a defect that is live today. The walk's known weakness is that it races:
//! a process forked between the snapshot and the signal is missed. That race is bounded here rather
//! than left implicit — [`kill_tree`] re-walks up to [`SWEEP_ROUNDS`] times and only stops early on
//! an empty round that FOLLOWS a kill round, so a single stale snapshot can never end the sweep.
//! Whatever is still alive after the last round is logged, never panicked on.
//!
//! **Containment is not total, and the gap is inherent to the shape.** The sweep survives parents
//! that die DURING it — membership is cumulative, so a child reparented to init mid-sweep stays in
//! the tree — but a subtree already orphaned BEFORE the first walk is unreachable. If the model ran
//! `something &` in a tool shell that then exited, the survivor's `ppid` is `1` by the time the stop
//! arrives, no `ppid` walk can attribute it to this agent, and this module will not kill it. Closing
//! that would take the spawn-side shape (a `kqueue`/`NOTE_EXIT` supervisor holding the tree open),
//! which the pluggable-harnesses design's slice 6 is where it belongs. Until then: a stop contains
//! everything the agent still has a parent chain to, which is every case STUDIO-869 measured, and
//! not literally everything it ever started.

use std::collections::BTreeSet;
use std::time::Duration;

/// How many kill rounds a single [`kill_tree`] may spend. Each round costs one `ps` (a few ms) plus
/// [`SWEEP_PAUSE`], so the whole sweep is bounded at roughly 200ms — short enough to run on the
/// stop path (including from a `Drop`, which cannot await), long enough that a shell forking one
/// more child as it dies is caught on the next pass.
const SWEEP_ROUNDS: usize = 5;

/// The pause between a kill round and the walk that verifies it, giving the OS time to make the
/// signalled processes zombies (which this module does not count as alive).
const SWEEP_PAUSE: Duration = Duration::from_millis(20);

/// One row of the process table.
#[derive(Clone, Copy)]
struct Proc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    /// `false` for a ZOMBIE — a corpse pending a reap holds an exit status, not a `git push`, so it
    /// is not something the sweep waits for. (The agent leader is a zombie for the whole window
    /// between its SIGKILL and the runtime reaping it.)
    live: bool,
}

/// SIGKILLs `pid`, its process group, and every descendant process it has — including the tool
/// children that put themselves in a process group of their own, which a group kill alone misses.
/// A pid of 0 is skipped, exactly as the group kill it replaces was (`kill(0, …)` would target the
/// daemon's OWN group), and so is a pid of 1.
///
/// Best-effort by construction, like the Go `_ = syscall.Kill(…)` it descends from: every signal is
/// unchecked, a process table that cannot be read degrades to the plain group kill (so STUDIO-840's
/// guarantee survives a machine with no `ps`), and a tree that outlives the sweep is warned about.
/// Nothing here returns an error, because no caller — a turn deadline, a stall, an operator Stop
/// arriving as a `Drop` — has anything to do with one.
///
/// Synchronous, and it shells out to `ps`: callable from `Drop`, at the cost of blocking the calling
/// thread for the sweep's duration.
pub fn kill_tree(pid: u32) {
    let leader = pid as i32;
    // A leader that is not itself a legal target roots no sweep at all. `0` is the disarmed guard
    // (and `kill(0, …)` would signal the daemon's OWN group); `1` is init, whose descendants are
    // every process on the machine, so even the per-pid half of a sweep rooted there is the blast
    // radius [`signalable`] exists to refuse. Unlike the same check inside [`round_targets`] this
    // one cannot be exercised red — a test that removed it would take the runner with it — so it is
    // the second line of defence, and [`round_targets`] is the one the suite pins.
    if !signalable(-leader) {
        return;
    }
    // Every pid the sweep has ever seen in this tree, and the reason the walk still works after the
    // leader dies: a SIGKILLed leader's children are reparented to init at once, so a walk rooted at
    // `leader` alone would find nothing from round 1 on. Membership is by descent from anything
    // already known, so the tree stays reachable through its own dead parents. (A pid that dies and
    // is RECYCLED inside the sweep's ~200ms window would pull a stranger in; pid space does not wrap
    // that fast, and the alternative — dropping dead parents — loses the survivors this exists for.)
    let mut known: BTreeSet<i32> = BTreeSet::from([leader]);
    let mut alive = Vec::new();
    for round in 0..=SWEEP_ROUNDS {
        alive = live_tree(&process_table(), &mut known);
        // A `ps` snapshot is milliseconds stale by the time it is parsed, so ONE empty round proves
        // nothing — a tool child forked during the snapshot would not be in it. Only an empty round
        // that follows a kill round ends the sweep.
        if alive.is_empty() && round > 0 {
            return;
        }
        if round == SWEEP_ROUNDS {
            break;
        }
        for target in round_targets(&alive, leader, &known) {
            signal(target);
        }
        std::thread::sleep(SWEEP_PAUSE);
    }
    if !alive.is_empty() {
        tracing::warn!(
            leader,
            survivors = alive.len(),
            pids = ?alive.iter().map(|p| p.pid).collect::<Vec<_>>(),
            "agent process tree outlived the kill sweep; some of the agent's work may still be running"
        );
    }
}

/// Whether a SIGKILL may be aimed at `target` (negative = a process group). The chokepoint for the
/// three arguments `kill(2)` reads as "something other than one process tree":
/// `0` is the CALLER's own process group (the daemon and everything it leads), `1` is init, and
/// `-1` is every process the user can signal at all. A leader pid of 1 is the whole distance
/// between this module and that last one, and [`kill_tree`] is a public entry point a future
/// harness adapter can hand a parsed or recycled id. Every signal the sweep delivers passes through
/// here, because [`round_targets`] is the only thing [`kill_tree`] signals.
fn signalable(target: i32) -> bool {
    target.abs() > 1
}

/// Delivers one unchecked SIGKILL. A negative `target` means the process group led by `-target`.
fn signal(target: i32) {
    // SAFETY: `kill(2)` is safe to call with any pid; SIGKILL cannot be caught, and the return is
    // deliberately ignored (an ESRCH just means the process died on its own first).
    unsafe {
        libc::kill(target, libc::SIGKILL);
    }
}

/// Grows `known` to the full descendant closure of what it already holds, and returns the rows that
/// are still running. The table is unordered (a child can be listed before its parent), so the
/// closure is taken by repeated passes until it stops growing.
fn live_tree(table: &[Proc], known: &mut BTreeSet<i32>) -> Vec<Proc> {
    loop {
        let before = known.len();
        for p in table {
            if known.contains(&p.ppid) {
                known.insert(p.pid);
            }
        }
        if known.len() == before {
            break;
        }
    }
    table
        .iter()
        .filter(|p| p.live && known.contains(&p.pid))
        .copied()
        .collect()
}

/// EVERY `kill(2)` argument one sweep round delivers — the whole decision, as a pure function, so
/// that what the sweep may and may not signal is a property a test can assert rather than something
/// spread across [`kill_tree`]'s loop body.
///
/// `-leader` is a member unconditionally, whatever the process table said: STUDIO-840's group kill
/// has to land even on a machine where `ps` could not be read and `alive` is therefore empty. Each
/// live process in the tree then contributes one more target, de-duplicated — the whole process
/// GROUP when the group's leader is itself part of the tree (the escaped tool shell's case; killing
/// the group catches the children it forked since the snapshot too), and the bare pid otherwise (a
/// process the harness parked in some pre-existing group is not a licence to signal that group's
/// other members).
///
/// The whole set, `-leader` included, then passes [`signalable`] and the two self-checks: nothing
/// that would signal init, every process on the machine, this process, or the daemon's own group
/// ever reaches [`signal`].
fn round_targets(alive: &[Proc], leader: i32, known: &BTreeSet<i32>) -> BTreeSet<i32> {
    let me = std::process::id() as i32;
    // SAFETY: `getpgrp(2)` takes no arguments, touches no memory and cannot fail.
    let my_pgid = unsafe { libc::getpgrp() };
    std::iter::once(-leader)
        .chain(
            alive
                .iter()
                .filter(|p| p.pid != leader && p.pgid != leader)
                .map(|p| {
                    if p.pgid > 1 && known.contains(&p.pgid) {
                        -p.pgid
                    } else {
                        p.pid
                    }
                }),
        )
        .filter(|t| {
            let subject = t.abs();
            signalable(*t) && subject != me && subject != my_pgid
        })
        .collect()
}

/// Snapshots the process table via `ps` — POSIX, present on both macOS (where Rhapsody ships) and
/// Linux, and the same probe the orchestrator's stop e2e asserts with.
///
/// A `ps` that cannot be run or whose format this cannot parse yields NO rows, which degrades
/// [`kill_tree`] to the plain process-group kill rather than failing it.
fn process_table() -> Vec<Proc> {
    let out = match std::process::Command::new("ps")
        .args(["-Ao", "pid=,ppid=,pgid=,stat="])
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not read the process table; falling back to a process-group kill alone"
            );
            return Vec::new();
        }
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_row)
        .collect()
}

/// Parses one `pid ppid pgid stat` row. A row that does not parse is dropped.
fn parse_row(line: &str) -> Option<Proc> {
    let mut f = line.split_whitespace();
    let pid = f.next()?.parse().ok()?;
    let ppid = f.next()?.parse().ok()?;
    let pgid = f.next()?.parse().ok()?;
    // `Z` / `Z+` / `ZN`: a corpse pending a reap.
    let live = !f.next()?.starts_with('Z');
    Some(Proc {
        pid,
        ppid,
        pgid,
        live,
    })
}

/// SIGKILLs an agent's whole process tree when a turn is abandoned by having its future DROPPED,
/// unless [`disarm`](KillTreeOnDrop::disarm)ed first. Go gets this from `exec.CommandContext(tctx,
/// …)` + `cmd.Cancel`: cancelling the run's context kills the agent. Rust's cancellation IS the
/// drop, and a dropped `tokio::process::Child` signals nothing, so before STUDIO-840 an operator
/// Stop removed the running entry, moved the ticket and answered 200 while the real `claude` kept
/// committing. Disarmed once the child has been reaped, so a kill can never reach a recycled pid.
///
/// Backend-agnostic on purpose (STUDIO-871): every harness leaks tool children the same way, so
/// every backend arms the same guard.
pub struct KillTreeOnDrop(u32);

impl KillTreeOnDrop {
    /// Arms the guard for the agent process `pid` leads.
    pub fn new(pid: u32) -> KillTreeOnDrop {
        KillTreeOnDrop(pid)
    }

    /// Stands the kill down (the child is reaped).
    pub fn disarm(&mut self) {
        self.0 = 0; // `kill_tree` skips pid 0
    }
}

impl Drop for KillTreeOnDrop {
    fn drop(&mut self) {
        kill_tree(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// SIGKILLs the process group led by `pgid` — the fixtures' own cleanup. Not a production path:
    /// [`kill_tree`] signals only what [`round_targets`] returns.
    fn kill_group(pgid: i32) {
        signal(-pgid);
    }

    /// The `ps` rows for processes in the group led by `pgid` that are still ALIVE — zombies
    /// excluded, for the reason [`Proc::live`] gives. Each row is kept whole so a failure names
    /// exactly what survived.
    fn live_rows_in_group(pgid: i32) -> Vec<String> {
        let out = std::process::Command::new("ps")
            .args(["-Ao", "pid=,pgid=,stat=,comm="])
            .output()
            .expect("ps -Ao pid=,pgid=,stat=,comm=");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?;
                let grp: i32 = f.next()?.parse().ok()?;
                let stat = f.next()?;
                let comm = f.next().unwrap_or("");
                (grp == pgid && !stat.starts_with('Z')).then(|| format!("{pid} {stat} {comm}"))
            })
            .collect()
    }

    /// Polls until nothing live is left in `pgid`, or `timeout` elapses.
    fn wait_group_quiet(pgid: i32, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if live_rows_in_group(pgid).is_empty() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A stand-in harness: a leader in its OWN process group that starts one background job under
    /// bash job control (`set -m` gives every job a new process group — precisely what claude,
    /// opencode and codex do to the shell they run a tool command in) and then sleeps forever.
    /// Nothing but a kill ends it, so a passing assertion can never be the fixture having finished.
    ///
    /// Reaps both on drop, INCLUDING when the test that owns it panicked: a fixture that leaks its
    /// escaped group when the assertion reds would leave sleepers on the CI runner forever, and the
    /// mutation-check in this ticket runs the red case on purpose.
    struct Harness {
        leader: std::process::Child,
        dir: std::path::PathBuf,
        escaped: i32,
    }

    impl Harness {
        fn start() -> Harness {
            // Unique per fixture: the tests run in parallel threads of one binary. No shell
            // metacharacters, because the path is interpolated into the script below.
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir()
                .join(format!("rhapsody-proctree-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create the fixture's temp dir");
            let pgid_file = dir.join("escaped.pgid");
            // `bash -c`'s own `$$` is the escaped child's pid, and with job control on it is also
            // its pgid — macOS ships bash 3.2, which has no `BASHPID`, so the child reports for
            // itself. The report is written and renamed so a reader never sees a half-written file.
            let script = format!(
                "set -m\n\
                 bash -c 'ps -o pgid= -p $$ | tr -d \" \" > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; \
                 while true; do sleep 3600; done' _ \"{pgid_file}\" &\n\
                 set +m\n\
                 while true; do sleep 3600; done\n",
                pgid_file = pgid_file.display()
            );
            // Null stdio: the fixture must not hold a pipe this test's own parent is reading, or a
            // RED run hangs the runner instead of failing it.
            let leader = std::process::Command::new("bash")
                .args(["-c", &script])
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the stand-in harness leader");

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let escaped = loop {
                if let Some(v) = std::fs::read_to_string(&pgid_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                {
                    break v;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the stand-in harness never reported an escaped process group"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            Harness {
                leader,
                dir,
                escaped,
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            kill_group(self.escaped);
            kill_group(self.leader.id() as i32);
            let _ = self.leader.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// STUDIO-871. The headline finding of the STUDIO-869 spike: every harness escapes the process
    /// group Rhapsody spawns it into, so STUDIO-840's `kill(-leader, SIGKILL)` kills the leader,
    /// reports success, and leaves the `cargo build` / `git push` / `gh pr create` the model
    /// launched running to completion.
    ///
    /// The assertion is on OS process state, never on a return value — reverting [`kill_tree`] to a
    /// bare `kill(-leader, SIGKILL)` reds it with the survivors named.
    #[test]
    fn kill_tree_kills_a_tool_child_that_escaped_the_agents_process_group() {
        let h = Harness::start();
        assert_ne!(
            h.escaped,
            h.leader.id() as i32,
            "the fixture did not escape the leader's group, so it cannot prove containment"
        );
        assert!(
            !live_rows_in_group(h.escaped).is_empty(),
            "the escaped group ({}) should be alive before the kill",
            h.escaped
        );

        kill_tree(h.leader.id());

        assert!(
            wait_group_quiet(h.escaped, Duration::from_secs(10)),
            "a tool child of the killed agent survived: group {} still runs {:?}",
            h.escaped,
            live_rows_in_group(h.escaped)
        );
    }

    /// STUDIO-840's half of the guarantee, still intact: the agent leader and everything sharing its
    /// group (the injected `rhapsodyd` MCP server, fake-claude's background stdin drain) die too.
    #[test]
    fn kill_tree_still_kills_the_agents_own_process_group() {
        let h = Harness::start();
        let leader = h.leader.id() as i32;
        assert!(
            !live_rows_in_group(leader).is_empty(),
            "the leader's group ({leader}) should be alive before the kill"
        );

        kill_tree(h.leader.id());

        assert!(
            wait_group_quiet(leader, Duration::from_secs(10)),
            "the agent leader survived its own kill: group {leader} still runs {:?}",
            live_rows_in_group(leader)
        );
    }

    /// The one signal this module must never deliver. `kill(-1, SIGKILL)` hits EVERY process the
    /// user can signal — the daemon, the operator's shell, this test binary — and the whole distance
    /// between the sweep and that call is a leader pid of 1, because every round signals the leader's
    /// group. A pid of 1 is not reachable from `Child::id()`, but [`kill_tree`] is a public entry
    /// point a future harness adapter can hand a parsed or recycled id, and the blast radius of being
    /// wrong once is the whole machine.
    ///
    /// `kill_tree(1)` cannot be called here — the red half of that experiment takes the test runner,
    /// the daemon and the operator's session with it — which is exactly why the round's ENTIRE
    /// target set is a pure function: the requirement ("a sweep rooted at pid 1 delivers no signal
    /// at all") is assertable without delivering one. Asserting `!signalable(-1)` instead would only
    /// restate the implementation, and would stay green if the guard were deleted from the one place
    /// that consults it.
    #[test]
    fn a_sweep_rooted_at_pid_one_signals_nothing_at_all() {
        assert!(
            round_targets(&[], 1, &BTreeSet::from([1])).is_empty(),
            "a leader of 1 makes the round's unconditional group kill kill(-1, …)"
        );
    }

    /// STUDIO-840's half of the guarantee, pinned where it now lives: a process table that could not
    /// be read yields no rows at all, and the round must still deliver the leader's group kill.
    #[test]
    fn round_targets_kill_the_leaders_group_even_with_an_unreadable_process_table() {
        assert_eq!(
            round_targets(&[], 424242, &BTreeSet::from([424242])),
            BTreeSet::from([-424242]),
            "the group kill must not depend on `ps` having been readable"
        );
    }

    /// A zero pid is the disarmed guard, and `kill(0, …)` / `kill(-0, …)` would signal the DAEMON's
    /// own process group — every process the daemon leads, including itself.
    #[test]
    fn kill_tree_of_pid_zero_is_a_no_op() {
        kill_tree(0);
        // Reaching this line is the assertion: the test process is in the daemon's position here,
        // and a `kill(0, SIGKILL)` would have taken the whole test binary down with it.
    }

    /// The sweep must never aim at the process doing the sweeping or at its group, however the
    /// process table reads — a self-signal would kill the daemon on any operator Stop.
    #[test]
    fn round_targets_never_include_this_process_or_its_group() {
        let me = std::process::id() as i32;
        // SAFETY: `getpgrp(2)` takes no arguments and cannot fail.
        let my_pgid = unsafe { libc::getpgrp() };
        let leader = 424242;
        let alive = vec![
            Proc {
                pid: me,
                ppid: leader,
                pgid: my_pgid,
                live: true,
            },
            Proc {
                pid: 1,
                ppid: leader,
                pgid: 1,
                live: true,
            },
        ];
        let known = BTreeSet::from([leader, me, my_pgid, 1]);
        assert_eq!(
            round_targets(&alive, leader, &known),
            BTreeSet::from([-leader]),
            "the sweep aimed at its own process or group"
        );
    }

    /// A tool child that leads a group of its own is signalled as a GROUP (so the children it forks
    /// between the snapshot and the signal die with it), while one parked in a group nobody in the
    /// tree leads is signalled as a bare pid (its group's other members are not ours to kill).
    #[test]
    fn round_targets_prefer_the_group_only_when_the_tree_leads_it() {
        let leader = 424242;
        let alive = vec![
            // The escaped tool shell: leads its own group.
            Proc {
                pid: 500,
                ppid: leader,
                pgid: 500,
                live: true,
            },
            // Its child, in that same escaped group.
            Proc {
                pid: 501,
                ppid: 500,
                pgid: 500,
                live: true,
            },
            // A descendant parked in a group led by a stranger.
            Proc {
                pid: 600,
                ppid: leader,
                pgid: 777,
                live: true,
            },
            // In the leader's own group: covered by the unconditional group kill.
            Proc {
                pid: 700,
                ppid: leader,
                pgid: leader,
                live: true,
            },
        ];
        let known = BTreeSet::from([leader, 500, 501, 600, 700]);
        assert_eq!(
            round_targets(&alive, leader, &known),
            BTreeSet::from([-leader, -500, 600]),
            "want the leader's group, the escaped group signalled once, and the parked process alone"
        );
    }

    /// The tree stays reachable through its own dead parents — the property the sweep depends on
    /// once the leader is SIGKILLed and the OS reparents its children to init.
    #[test]
    fn live_tree_keeps_a_descendant_whose_parent_already_died() {
        let leader = 424242;
        let mut known = BTreeSet::from([leader]);
        // Round 0: the whole chain is visible through the live leader.
        let round0 = vec![
            Proc {
                pid: leader,
                ppid: 1,
                pgid: leader,
                live: true,
            },
            Proc {
                pid: 500,
                ppid: leader,
                pgid: 500,
                live: true,
            },
            Proc {
                pid: 501,
                ppid: 500,
                pgid: 500,
                live: true,
            },
        ];
        assert_eq!(live_tree(&round0, &mut known).len(), 3);
        // Round 1: the leader is gone and 500 has been reparented to init. A walk rooted at the
        // leader alone would now find nothing.
        let round1 = vec![
            Proc {
                pid: 500,
                ppid: 1,
                pgid: 500,
                live: true,
            },
            Proc {
                pid: 501,
                ppid: 500,
                pgid: 500,
                live: true,
            },
        ];
        let alive: Vec<i32> = live_tree(&round1, &mut known)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(
            alive,
            vec![500, 501],
            "the orphaned tool children fell out of the tree when their parent died"
        );
    }

    /// A zombie is an exit status waiting to be collected: it cannot run a command, so the sweep
    /// must not keep burning rounds waiting for one to be reaped.
    #[test]
    fn live_tree_ignores_zombies() {
        let leader = 424242;
        let mut known = BTreeSet::from([leader]);
        let table = vec![Proc {
            pid: 500,
            ppid: leader,
            pgid: 500,
            live: false,
        }];
        assert!(
            live_tree(&table, &mut known).is_empty(),
            "a zombie was counted as a surviving process"
        );
        assert!(
            known.contains(&500),
            "a zombie's pid must still join the tree, or its own children become unreachable"
        );
    }

    /// The `ps` format the sweep reads, pinned: a row it cannot parse is dropped rather than
    /// panicked on, and the stat column decides liveness.
    #[test]
    fn parse_row_reads_the_ps_columns_and_drops_junk() {
        let p = parse_row(" 96368   96350   96368 Ss  ").expect("a well-formed row");
        assert_eq!((p.pid, p.ppid, p.pgid, p.live), (96368, 96350, 96368, true));
        let z = parse_row("96639 96638 96635 Z+").expect("a zombie row");
        assert!(!z.live, "a Z stat is not a live process");
        assert!(parse_row("PID PPID PGID STAT").is_none());
        assert!(parse_row("").is_none());
        assert!(parse_row("96368 96350 96368").is_none(), "no stat column");
    }
}
