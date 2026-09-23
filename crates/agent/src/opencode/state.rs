//! Per-run `XDG_DATA_HOME` isolation for the opencode backend (STUDIO-902). Rhapsody-only.
//!
//! ⚠️ **This module exists because sharing one opencode state directory LOSES TURNS.** It is not
//! hygiene. The STUDIO-869 spike measured it (`harness/harness-spike/opencode/`, and
//! `STUDIO-869-harness-spike-findings.md` §A.3):
//!
//! | Two concurrent turns | Turns completed |
//! |---|---|
//! | one shared state dir, database already created | **8/10** (`concurrency-trials-shared.txt`) |
//! | one shared state dir, **no database yet** | **0/10 — both turns of every pair** (`concurrency-trials-fresh-shared-xdg.txt`) |
//! | a private `XDG_DATA_HOME` per turn | **10/10** (`concurrency-trials-isolated-xdg.txt`) |
//!
//! `$XDG_DATA_HOME/opencode/opencode.db` is SQLite; the turn that loses the race is refused
//! outright — it dies in 0–1s, exits 1, writes `database is locked` to stderr and emits a
//! **completely empty event stream**. The worst case is exactly the one a daemon meets first: a
//! newly provisioned state directory, where the loss is not intermittent but total.
//!
//! ## Two things that make a naive redirect silently wrong
//!
//! 1. ⚠️ **Redirect `XDG_DATA_HOME`, never `HOME`.** Redirecting `HOME` severs the macOS keychain
//!    and the turn fails looking like a misconfigured provider (measured for goose, STUDIO-872;
//!    design §4.5 states the rule for the family).
//! 2. ⚠️ **opencode keeps its CREDENTIALS in the very directory being redirected** —
//!    `$XDG_DATA_HOME/opencode/auth.json`. A bare redirect therefore leaves the turn
//!    unauthenticated, and it fails as a 401 that is indistinguishable from a real credential
//!    problem: `harness/harness-spike/opencode/failure-401.jsonl` is literally that shape. So the
//!    state directory is SEEDED with a copy of the operator's own `auth.json`, and a missing source
//!    is refused loudly BEFORE any process is spawned ([`RunState::provision`]) rather than
//!    discovered as a 401 mid-turn. The spike's own driver refuses the same way and for the same
//!    stated reason (`sandbox/conctrials.sh` exits 3).
//!
//! Copying an existing credential is not the daemon writing a provider config: auth still defers
//! entirely to the operator's own `opencode auth login` (design §4.5). Nothing here creates,
//! refreshes or edits a credential — it is moved, unread, into the directory the CLI will look in.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::AgentError;

/// Distinguishes two state directories created in the same nanosecond by the same pid. Pid alone is
/// not enough and neither is a timestamp: pids are recycled, and a recycled pid adopting a stale
/// directory is precisely how a run inherits another run's database.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The directory name prefix, so an operator can recognize (and an admin can sweep) what these are.
const PREFIX: &str = "rhapsody-opencode";

/// The private config directory name created inside the per-session state directory for brokered
/// runs (`OPENCODE_CONFIG_DIR`, `provider-broker-design.md` §9.2). It is a separate 0700 directory
/// from the state root `XDG_DATA_HOME` names, and deliberately empty: brokered mode supplies its
/// provider config inline via `OPENCODE_CONFIG_CONTENT`, and this directory exists only so OpenCode
/// has an adapter-owned, non-project directory to consult.
const CONFIG_DIR_NAME: &str = "opencode-config";

/// A provisioned, private opencode state directory for ONE session.
///
/// Removed by [`RunState::cleanup`] (called from `Session::stop`) and again by `Drop`, because an
/// operator Stop cancels a run by DROPPING the session's future rather than returning through it —
/// the same cancellation path `crate::proctree::KillTreeOnDrop` exists for. Cleanup is idempotent.
#[derive(Debug)]
pub struct RunState {
    dir: PathBuf,
}

impl RunState {
    /// Creates a private state directory for a LEGACY native-login session, seeding it with a copy
    /// of the operator's own `auth.json`.
    ///
    /// This is the explicit `LegacyLogin` mode (`provider-broker-design.md` §9.1). `state_root`
    /// empty ⇒ the system temp dir; `auth_source` empty ⇒ [`default_auth_source`]. `workspace_root`
    /// is the daemon's own worktree root (`crate::opencode::Config::workspace_root`); empty skips
    /// the workspace-containment check (a test convenience — production always has one). Returns an
    /// error — before anything is spawned — when the credential is missing or empty, or when
    /// `state_root` resolves inside a worktree/repository, through a symlink, or under unsafe
    /// ownership (STUDIO-980).
    pub fn provision_legacy(
        state_root: &str,
        auth_source: &str,
        issue_identifier: &str,
        workspace_root: &str,
    ) -> Result<RunState, AgentError> {
        let src = if auth_source.is_empty() {
            default_auth_source()
        } else {
            PathBuf::from(auth_source)
        };
        // Checked first, so the refusal names the real problem rather than leaving a provisioned
        // directory behind on the way to reporting it.
        let meta = std::fs::metadata(&src).map_err(|e| {
            AgentError::Other(format!(
                "opencode_auth_missing: {}: {e}. opencode keeps its credentials inside the state \
                 directory this backend redirects, so a run cannot authenticate without a copy. \
                 Run `opencode auth login` as the daemon's user, or set `opencode.auth_source`",
                src.display()
            ))
        })?;
        if meta.len() == 0 {
            return Err(AgentError::Other(format!(
                "opencode_auth_missing: {} is empty; a run would be unauthenticated and fail as a \
                 401. Run `opencode auth login` as the daemon's user",
                src.display()
            )));
        }

        let state = Self::provision_dir(state_root, issue_identifier, workspace_root)?;
        let dst_dir = state.dir.join("opencode");
        let seed = || -> std::io::Result<()> {
            std::fs::create_dir_all(&dst_dir)?;
            std::fs::copy(&src, dst_dir.join("auth.json"))?;
            Ok(())
        };
        if let Err(e) = seed() {
            // The half-built directory is removed here rather than left for `Drop`, so the error
            // path leaves nothing behind either.
            state.cleanup();
            return Err(AgentError::Other(format!(
                "opencode_auth_missing: could not seed {} from {}: {e}",
                dst_dir.display(),
                src.display()
            )));
        }
        Ok(state)
    }

    /// Creates a private state directory for a BROKERED session: a `Brokered` `RunState` holds **no
    /// reusable credential file at all** (`provider-broker-design.md` §9.1). The child receives only
    /// the per-turn capability in its environment; there is no `auth.json`, and none is created or
    /// copied. The private `OPENCODE_CONFIG_DIR` is created 0700 alongside it.
    ///
    /// It performs no credential read — the refusal ordering that a brokered dispatch needs (probe
    /// first, then prepare) is the caller's to enforce — but it does enforce every state-root
    /// containment/ownership rule the legacy path does.
    pub fn provision_brokered(
        state_root: &str,
        issue_identifier: &str,
        workspace_root: &str,
    ) -> Result<RunState, AgentError> {
        let state = Self::provision_dir(state_root, issue_identifier, workspace_root)?;
        let config_dir = state.dir.join(CONFIG_DIR_NAME);
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&config_dir) {
            state.cleanup();
            return Err(AgentError::Other(format!(
                "opencode state config dir {}: {e}",
                config_dir.display()
            )));
        }
        Ok(state)
    }

    /// Creates the private 0700 per-session directory under a validated `state_root`, shared by both
    /// provisioning modes. Creates no credential file of any kind; the caller decides what (if
    /// anything) is seeded into it.
    fn provision_dir(
        state_root: &str,
        issue_identifier: &str,
        workspace_root: &str,
    ) -> Result<RunState, AgentError> {
        let root = if state_root.is_empty() {
            // Canonicalized up front, unlike the operator-configured case below: macOS's own
            // `/tmp`/`/var/folders` are themselves reached through a `/var` -> `/private/var`
            // symlink, and `validate_root_is_safe` refuses any OTHER symlink in a configured path
            // outright. Resolving our own default here means it never gets flagged as if an
            // operator had configured a suspicious symlink (STUDIO-980 review).
            let tmp = std::env::temp_dir();
            tmp.canonicalize().map_err(|e| {
                AgentError::Other(format!(
                    "opencode_state_unsafe: could not resolve the system temp dir {}: {e}",
                    tmp.display()
                ))
            })?
        } else {
            PathBuf::from(state_root)
        };
        // Checked before anything is created: refusing here means a bad `state_root` never gets a
        // `create_dir_all` chance to plant so much as an empty directory (STUDIO-980).
        validate_root_is_safe(&root, workspace_root)?;
        std::fs::create_dir_all(&root).map_err(|e| {
            AgentError::Other(format!("opencode state root {}: {e}", root.display()))
        })?;

        let dir = root.join(unique_name(issue_identifier));
        // `DirBuilder::create` (recursive `false`, matching the old `create_dir`): if this name
        // somehow already exists, that is a collision with another run's live database and must
        // fail rather than be adopted. Mode `0o700` is set ATOMICALLY in the `mkdir` call itself —
        // never permissive-then-chmod, which would leave a race window where the directory is
        // world-readable before the fix-up lands (STUDIO-980).
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| AgentError::Other(format!("opencode state dir {}: {e}", dir.display())))?;
        Ok(RunState { dir })
    }

    /// The value to set `XDG_DATA_HOME` to for this session's children.
    pub fn xdg_data_home(&self) -> &Path {
        &self.dir
    }

    /// The value to set `OPENCODE_CONFIG_DIR` to for a brokered session's children: a private 0700
    /// directory inside the state root, created empty by [`RunState::provision_brokered`].
    pub fn config_dir(&self) -> PathBuf {
        self.dir.join(CONFIG_DIR_NAME)
    }

    /// Removes the state directory. Idempotent, best-effort, and never fails a run: by the time
    /// this is reached the turn's outcome is already decided, and a leaked directory is a disk
    /// problem rather than a correctness one.
    pub fn cleanup(&self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                dir = %self.dir.display(), err = %e,
                "opencode: could not remove the per-run state directory"
            );
        }
    }
}

impl Drop for RunState {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Where the operator's own opencode credential lives, from the DAEMON's environment:
/// `$XDG_DATA_HOME/opencode/auth.json` when that variable is set, else the XDG default
/// `$HOME/.local/share/opencode/auth.json`.
///
/// Read from the daemon's environment on purpose — it is the operator's login that is being
/// located, and the child's `XDG_DATA_HOME` is about to be overwritten with the private directory.
pub fn default_auth_source() -> PathBuf {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        }
    };
    base.join("opencode").join("auth.json")
}

/// Refuses `root` (the resolved `state_root`) before anything is written under it (STUDIO-980):
/// inside the daemon's own worktree root, inside any git worktree/repository, reached through a
/// symlink at all, or sitting under unsafe ownership anywhere in its ancestor chain.
///
/// It is enough to validate `root` alone, never the per-session directory `provision` creates under
/// it: containment is downward-closed (nothing else writes into `root` between this check and the
/// `mkdir`), so a safe `root` makes every fresh child of it safe too.
fn validate_root_is_safe(root: &Path, workspace_root: &str) -> Result<(), AgentError> {
    // Lexical, not real: `std::path::absolute` prepends the cwd for a relative path WITHOUT
    // touching the filesystem or resolving symlinks — crucially, it does NOT collapse `..`
    // components either (that would require knowing what a symlink points to). A `..` therefore
    // survives into `absolute_root` verbatim, which is exactly what makes it dangerous: a
    // nonexistent leading component followed by `..` (e.g. `<root>/nonexistent/../workspaces/x`)
    // would find no existing ancestor short of a real one far above the intended target, compare
    // as outside every containment check below, and then have the `..` actually resolved by
    // `create_dir_all` once `nonexistent` is created — landing the real directory right back
    // inside what containment was refusing (STUDIO-980 review, finding B3). So `..` is refused
    // outright, the same as a symlink component.
    let absolute_root = std::path::absolute(root).map_err(|e| {
        AgentError::Other(format!("opencode_state_unsafe: {}: {e}", root.display()))
    })?;
    if absolute_root
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(AgentError::Other(format!(
            "opencode_state_unsafe: {} contains a '..' component; opencode state_root must be a \
             direct path with no parent-directory traversal",
            absolute_root.display()
        )));
    }

    // `ancestors()` on an absolute path always reaches "/", which always exists, so this is only
    // `None` for a path whose absolutized form still has no existing prefix — treated as unsafe
    // rather than guessed at.
    let ancestor = absolute_root
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| {
            AgentError::Other(format!(
                "opencode_state_unsafe: no existing ancestor of {} to validate",
                absolute_root.display()
            ))
        })?;

    // Resolve every symlink up to (and including) `ancestor`, then re-attach whatever suffix of
    // `root` doesn't exist yet lexically — it cannot itself be a symlink, since nothing has created
    // it. A configured `state_root` reached through ANY symlink component is refused outright
    // (design §2.7: "symlink components refused"), not merely followed-and-checked-for-containment:
    // if the canonical form disagrees with the lexical absolute form, some component resolved
    // somewhere other than where it lexically appears. `provision` exempts its own empty-`state_root`
    // default by canonicalizing `std::env::temp_dir()` itself before calling here.
    let canonical_ancestor = ancestor.canonicalize().map_err(|e| {
        AgentError::Other(format!(
            "opencode_state_unsafe: {}: {e}",
            ancestor.display()
        ))
    })?;
    let suffix = absolute_root
        .strip_prefix(ancestor)
        .unwrap_or_else(|_| Path::new(""));
    let canonical = canonical_ancestor.join(suffix);

    if canonical != absolute_root {
        return Err(AgentError::Other(format!(
            "opencode_state_unsafe: {} is reached through a symlink (resolves to {}); opencode \
             state must be configured with a path free of symlink components",
            absolute_root.display(),
            canonical.display()
        )));
    }

    // SAFETY: geteuid() takes no arguments and has no preconditions; unsafe only as an FFI import.
    let our_uid = unsafe { libc::geteuid() };
    // Walk EVERY existing ancestor, not just the nearest one: an attacker who controls a directory
    // higher up the chain can rename a safe leaf away and plant their own in its place, the same
    // hazard a symlink swap would create. This is the same shape sshd's `StrictModes` checks.
    for a in ancestor.ancestors() {
        let meta = std::fs::metadata(a).map_err(|e| {
            AgentError::Other(format!("opencode_state_unsafe: {}: {e}", a.display()))
        })?;
        if !ownership_is_safe(meta.uid(), meta.mode(), our_uid) {
            return Err(AgentError::Other(format!(
                "opencode_state_unsafe: {} is owned by uid {} and is writable by other users \
                 without the sticky bit set; refusing to provision opencode state under it",
                a.display(),
                meta.uid()
            )));
        }
    }

    if !workspace_root.is_empty() {
        let canonical_ws = Path::new(workspace_root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(workspace_root));
        if canonical.starts_with(&canonical_ws) {
            return Err(AgentError::Other(format!(
                "opencode_state_unsafe: {} is inside the workspace root {}; opencode state must \
                 live outside every worktree",
                canonical.display(),
                canonical_ws.display()
            )));
        }
    }

    if let Some(repo_root) = canonical.ancestors().find(|p| p.join(".git").exists()) {
        return Err(AgentError::Other(format!(
            "opencode_state_unsafe: {} is inside a git worktree/repository ({}); opencode state \
             must never live inside one",
            canonical.display(),
            repo_root.display()
        )));
    }

    Ok(())
}

/// Whether a directory owned by `owner_uid` is safe to create (or walk through to) opencode state
/// under, from the current process's effective uid `our_uid`.
///
/// The owner must be trusted first: ourselves, or root (uid 0, the owner of a system-managed shared
/// dir like `/tmp`). A directory owned by any OTHER user is under that user's sole control — they
/// can `chmod` it at any moment regardless of its mode right now — so it is never safe, whatever its
/// current permissions read.
///
/// A trusted owner is not automatically enough, though: owning a directory ourselves at a permissive
/// mode is exactly as exploitable as a foreign owner, because directory write permission (not
/// ownership of the entries inside it) is what controls renaming or unlinking a sibling. So even a
/// self-owned or root-owned directory must be either not writable by group/other at all, or writable
/// with the sticky bit set (so only the owner or root can rename/unlink an entry another user created
/// there — the same property that makes `/tmp` mode `1777` safe to share).
fn ownership_is_safe(owner_uid: u32, mode: u32, our_uid: u32) -> bool {
    let owner_trusted = owner_uid == our_uid || owner_uid == 0;
    if !owner_trusted {
        return false;
    }
    let sticky = mode & 0o1000 != 0;
    let writable_by_others = mode & 0o022 != 0;
    sticky || !writable_by_others
}

/// A directory name no other run can produce: prefix, a sanitized issue identifier for legibility,
/// the pid, a nanosecond stamp, and a process-wide counter.
fn unique_name(issue_identifier: &str) -> String {
    let slug: String = issue_identifier
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{PREFIX}-{slug}-{}-{nanos}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;

    fn seeded_auth(dir: &Path) -> String {
        let p = dir.join("auth.json");
        std::fs::write(&p, b"{\"fireworks-ai\":{\"type\":\"api\"}}").expect("write auth");
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn provision_creates_a_private_dir_and_copies_the_credential() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let root = tmp.path().join("root");

        let st = RunState::provision_legacy(&root.to_string_lossy(), &auth, "STUDIO-902", "")
            .expect("provision");
        let seeded = st.xdg_data_home().join("opencode").join("auth.json");
        assert!(seeded.is_file(), "auth.json seeded at {}", seeded.display());
        assert_eq!(
            std::fs::read(&seeded).expect("read"),
            std::fs::read(&auth).expect("read src"),
            "the credential is copied byte-for-byte, never rewritten"
        );
        assert!(
            st.xdg_data_home().to_string_lossy().contains("STUDIO-902"),
            "the directory names its issue so an operator can tell what leaked"
        );
    }

    // ⚠️ The headline property. Two sessions must never be handed the same state directory: that
    // is the `database is locked` failure, and it drops turns silently with an EMPTY event stream.
    #[test]
    fn two_provisions_never_share_a_directory() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let root = tmp.path().join("root").to_string_lossy().into_owned();

        let states: Vec<RunState> = (0..16)
            .map(|_| RunState::provision_legacy(&root, &auth, "STUDIO-902", "").expect("provision"))
            .collect();
        let dirs: std::collections::BTreeSet<PathBuf> = states
            .iter()
            .map(|s| s.xdg_data_home().to_path_buf())
            .collect();
        assert_eq!(dirs.len(), states.len(), "every state dir is distinct");
        // And each really is its own tree on disk, not a shared one reached by different names.
        for s in &states {
            assert!(
                s.xdg_data_home()
                    .join("opencode")
                    .join("auth.json")
                    .is_file()
            );
        }
    }

    // The refusal is the whole point of the credential check: a missing auth.json must stop the run
    // BEFORE a process is spawned, because the alternative is a 401 that reads like a provider
    // misconfiguration and costs a real dispatch to diagnose.
    #[test]
    fn a_missing_credential_is_refused_before_anything_is_created() {
        let tmp = TempDir::new();
        let root = tmp.path().join("root");
        let missing = tmp.path().join("nope").join("auth.json");

        let err = RunState::provision_legacy(
            &root.to_string_lossy(),
            &missing.to_string_lossy(),
            "STUDIO-902",
            "",
        )
        .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.starts_with("opencode_auth_missing:"), "{msg}");
        assert!(msg.contains("opencode auth login"), "names the fix: {msg}");
        assert!(
            !root.exists() || std::fs::read_dir(&root).into_iter().flatten().count() == 0,
            "a refused provision leaves no state directory behind"
        );
    }

    #[test]
    fn an_empty_credential_is_refused_too() {
        let tmp = TempDir::new();
        let auth = tmp.path().join("auth.json");
        std::fs::write(&auth, b"").expect("write");
        let err = RunState::provision_legacy(
            &tmp.path().join("root").to_string_lossy(),
            &auth.to_string_lossy(),
            "X-1",
            "",
        )
        .expect_err("must refuse an empty credential");
        assert!(err.to_string().contains("is empty"), "{err}");
    }

    // Cleanup runs on `stop()` AND on drop, because an operator Stop cancels a run by dropping the
    // future. Both paths, and cleanup twice, must be safe.
    #[test]
    fn cleanup_is_idempotent_and_drop_also_removes() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let root = tmp.path().join("root").to_string_lossy().into_owned();

        let path = {
            let st = RunState::provision_legacy(&root, &auth, "X-1", "").expect("provision");
            let p = st.xdg_data_home().to_path_buf();
            assert!(p.is_dir());
            st.cleanup();
            assert!(!p.exists(), "cleanup removed it");
            st.cleanup(); // idempotent — must not panic or warn-fail
            p
        };
        assert!(!path.exists());

        let st = RunState::provision_legacy(&root, &auth, "X-2", "").expect("provision");
        let p = st.xdg_data_home().to_path_buf();
        drop(st);
        assert!(!p.exists(), "Drop removed the directory of a cancelled run");
    }

    // The default source follows XDG, and must not be `$HOME` itself — redirecting HOME is the
    // documented way to break the macOS keychain (design §4.5).
    #[test]
    fn default_auth_source_follows_xdg() {
        let p = default_auth_source();
        assert!(p.ends_with("opencode/auth.json"), "{}", p.display());
    }

    // ⚠️ Mutation target: mkdir the outer dir with `create_dir_all` and no explicit mode, and this
    // fails with 0o755 under the machine's 022 umask — the exact live defect STUDIO-980 closes.
    #[test]
    fn provision_creates_the_outer_directory_atomically_at_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let root = tmp.path().join("root");

        let st = RunState::provision_legacy(&root.to_string_lossy(), &auth, "STUDIO-980", "")
            .expect("provision");
        let mode = std::fs::metadata(st.xdg_data_home())
            .expect("stat outer dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "the outer per-session directory must be created 0700, not created permissively \
             and chmodded after the fact"
        );
    }

    // A `state_root` resolving inside the daemon's own worktree root must be refused before
    // anything is written — opencode's database would otherwise land in `git status`.
    #[test]
    fn a_state_root_inside_the_workspace_root_is_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let workspace_root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace root");
        let bad_state_root = workspace_root.join("opencode-state");

        let err = RunState::provision_legacy(
            &bad_state_root.to_string_lossy(),
            &auth,
            "STUDIO-980",
            &workspace_root.to_string_lossy(),
        )
        .expect_err("a state root inside the workspace root must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert!(
            !bad_state_root.exists(),
            "a refused provision must write nothing"
        );
    }

    // ⚠️ Mutation target for B3: a NONEXISTENT leading component followed by `..` must not let
    // `create_dir_all` land the real directory inside the workspace root behind the containment
    // check's back. `<tmp>/nonexistent/../workspaces/opencode-state` has no existing ancestor short
    // of `<tmp>` itself, so a check that only compares the unresolved lexical suffix against
    // `workspace_root` would see `nonexistent/../workspaces/opencode-state` and never match — while
    // `create_dir_all` resolves the `..` for real once `nonexistent` exists, writing straight into
    // the workspace it was told to avoid.
    #[test]
    fn a_state_root_through_a_nonexistent_component_and_parent_dir_is_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let workspace_root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace root");
        let bad_state_root = tmp
            .path()
            .join("nonexistent")
            .join("..")
            .join("workspaces")
            .join("opencode-state");

        let err = RunState::provision_legacy(
            &bad_state_root.to_string_lossy(),
            &auth,
            "STUDIO-980",
            &workspace_root.to_string_lossy(),
        )
        .expect_err("a '..' traversal that lands inside the workspace root must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert!(
            !tmp.path().join("nonexistent").exists(),
            "the traversal component must never even be created"
        );
        assert_eq!(
            std::fs::read_dir(&workspace_root)
                .expect("read workspace root")
                .count(),
            0,
            "nothing must be written inside the workspace root via the traversal"
        );
    }

    // The same shape, but bypassing the git-repository guard rather than the workspace-root one —
    // the two containment checks are independent and both must be closed against this trick.
    #[test]
    fn a_state_root_through_a_nonexistent_component_and_parent_dir_into_a_repo_is_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir .git");
        let bad_state_root = tmp
            .path()
            .join("nonexistent")
            .join("..")
            .join("repo")
            .join("opencode-state");

        let err =
            RunState::provision_legacy(&bad_state_root.to_string_lossy(), &auth, "STUDIO-980", "")
                .expect_err("a '..' traversal that lands inside a git repository must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert!(!repo.join("opencode-state").exists());
    }

    // The same destination, reached through a symlink rather than a literal path — refused because
    // ANY symlink component in a configured `state_root` is refused outright (not merely followed
    // and then checked for containment).
    #[test]
    fn a_state_root_reached_through_a_symlink_into_the_workspace_root_is_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let workspace_root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        let link = outside.join("state-link");
        std::os::unix::fs::symlink(&workspace_root, &link).expect("symlink");

        let err = RunState::provision_legacy(
            &link.to_string_lossy(),
            &auth,
            "STUDIO-980",
            &workspace_root.to_string_lossy(),
        )
        .expect_err("a symlink resolving into the workspace root must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&workspace_root)
                .expect("read workspace root")
                .count(),
            0,
            "nothing must be written through the symlink"
        );
    }

    // "repository" is broader than "this run's worktree": a state root planted inside ANY git
    // checkout must be refused too, independent of the workspace-root check.
    #[test]
    fn a_state_root_inside_a_git_repository_is_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir .git");
        let bad_state_root = repo.join("opencode-state");

        let err =
            RunState::provision_legacy(&bad_state_root.to_string_lossy(), &auth, "STUDIO-980", "")
                .expect_err("a state root inside a git repository must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert!(!bad_state_root.exists());
    }

    // A symlink that lands OUTSIDE every workspace root and repository must still be refused: the
    // rule is "no symlink components", not "no symlink components that happen to be dangerous".
    #[test]
    fn a_state_root_reached_through_a_symlink_outside_every_repo_is_still_refused() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).expect("mkdir real");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let err = RunState::provision_legacy(&link.to_string_lossy(), &auth, "STUDIO-980", "")
            .expect_err("any symlink component in a configured state_root must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&real).expect("read real").count(),
            0,
            "nothing must be written through the symlink"
        );
    }

    // The empty-`state_root` default must NOT trip the symlink refusal above: `provision`
    // canonicalizes `std::env::temp_dir()` itself first, which is exactly what exempts macOS's own
    // `/var` -> `/private/var` symlink from a rule aimed at operator-configured paths.
    #[test]
    fn the_default_state_root_is_not_refused_for_its_own_symlinked_temp_dir() {
        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());

        let st = RunState::provision_legacy("", &auth, "STUDIO-980", "")
            .expect("the default root must work");
        assert!(
            st.xdg_data_home()
                .join("opencode")
                .join("auth.json")
                .is_file()
        );
    }

    // ⚠️ Mutation target for B2: a directory WE OWN but at a permissive mode is exactly as
    // exploitable as a foreign-owned one, because directory write permission — not ownership of the
    // entries inside it — is what lets another local user rename our fresh 0700 session directory
    // away and plant their own before `auth.json` is copied in. Constructible without root, unlike
    // the foreign-owner cases below.
    #[test]
    fn a_state_root_we_own_but_world_writable_without_the_sticky_bit_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new();
        let auth = seeded_auth(tmp.path());
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).expect("chmod");

        let err = RunState::provision_legacy(&root.to_string_lossy(), &auth, "STUDIO-980", "")
            .expect_err("a self-owned, world-writable, non-sticky root must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&root).expect("read root").count(),
            0,
            "a refused provision must write nothing under an unsafe root"
        );
    }

    // Pure-logic coverage for the ownership guard: constructing an actual foreign-owned directory
    // needs root (chown to another uid is not permitted otherwise), so this is the deterministic
    // substitute — it exercises exactly the branch `validate_root_is_safe` calls, and turns red if
    // that call is ever removed or the safe/unsafe cases are swapped.
    #[test]
    fn ownership_is_safe_matches_the_shared_tmp_dir_convention() {
        assert!(
            ownership_is_safe(501, 0o755, 501),
            "owning it yourself at a non-writable-by-others mode is safe"
        );
        assert!(
            !ownership_is_safe(501, 0o777, 501),
            "owning it yourself does NOT excuse a world-writable, non-sticky mode: directory \
             write permission, not entry ownership, is what lets another user rename our entry away"
        );
        assert!(
            ownership_is_safe(0, 0o1777, 501),
            "root-owned + sticky + world-writable is the standard safe /tmp shape"
        );
        assert!(
            !ownership_is_safe(0, 0o777, 501),
            "root-owned, world-writable, NOT sticky is the classic unsafe shared-dir shape"
        );
        assert!(
            ownership_is_safe(0, 0o755, 501),
            "root is a trusted owner even when the caller is a different uid"
        );
        assert!(
            !ownership_is_safe(502, 0o755, 501),
            "a directory owned by a DIFFERENT non-root user is never safe, however tight its mode \
             reads right now — that owner can chmod it at any time"
        );
    }

    // ⚠️ Mutation target: make `Brokered` seed an auth.json (or fall back to the legacy path when
    // the credential is absent) and this fails. A brokered state directory must contain NO reusable
    // credential file at all (`provider-broker-design.md` §9.1) — the child receives only a per-turn
    // capability in its environment.
    #[test]
    fn brokered_provision_creates_no_auth_json_and_a_private_config_dir() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new();
        let root = tmp.path().join("root");

        let st = RunState::provision_brokered(&root.to_string_lossy(), "STUDIO-1001", "")
            .expect("brokered provision");
        let auth = st.xdg_data_home().join("opencode").join("auth.json");
        assert!(
            !auth.exists(),
            "a brokered session must never create or copy auth.json: {}",
            auth.display()
        );
        // And nothing else under the state root carries a credential file either.
        assert!(
            !st.xdg_data_home().join("opencode").exists(),
            "brokered mode must create no opencode credential directory"
        );

        let cfg = st.config_dir();
        assert!(
            cfg.is_dir(),
            "OPENCODE_CONFIG_DIR is created: {}",
            cfg.display()
        );
        let mode = std::fs::metadata(&cfg)
            .expect("stat config dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the config dir must be created 0700");

        // The config dir is empty: brokered config is supplied inline, never as a project file.
        assert_eq!(
            std::fs::read_dir(&cfg).expect("read config dir").count(),
            0,
            "the brokered config dir must start empty"
        );
    }

    // The brokered mode still enforces every containment/ownership rule the legacy mode does: the
    // two modes differ ONLY in whether a credential file is seeded.
    #[test]
    fn brokered_provision_refuses_a_state_root_inside_the_workspace() {
        let tmp = TempDir::new();
        let workspace_root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspace_root).expect("mkdir workspace root");
        let bad = workspace_root.join("opencode-state");

        let err = RunState::provision_brokered(
            &bad.to_string_lossy(),
            "STUDIO-1001",
            &workspace_root.to_string_lossy(),
        )
        .expect_err("a brokered state root inside the workspace must be refused");
        assert!(
            err.to_string().starts_with("opencode_state_unsafe:"),
            "{err}"
        );
        assert!(!bad.exists(), "a refused brokered provision writes nothing");
    }
}
