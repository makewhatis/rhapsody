//! `resolve` — parity port of Go `internal/config/resolve.go` (`Resolve`, `resolveVar`,
//! `expandTilde`) plus the Unix subset of `path/filepath` it leans on (`Clean`/`Join`/`Abs`/
//! `IsAbs`).
//!
//! # What Resolve does — and what it does NOT
//!
//! Go splits defaulting across two stages, and this port keeps the split exactly:
//!
//! * [`crate::decode::decode`] (C3) applies the `orStr`/`orInt` chain — tracker endpoint,
//!   polling interval, timeouts, logging-dir default, the mode knobs' verbatim mapping, and so
//!   on. Those defaults are NOT re-applied here.
//! * [`resolve`] (this file, Go `Resolve`) performs `$VAR` indirection on `tracker.api_key` and
//!   `storage.path`, and normalizes the three path fields — `workspace.root`, `logging.dir`,
//!   `storage.path` — expanding `~`, anchoring relatives against `workflow_dir`, and making them
//!   absolute. It carries its OWN empty-string defaults for those three paths (and the
//!   `retention_days` = 30 default), because `Resolve` runs on a `Config` that may have been built
//!   by hand (the Go resolve tests pass a bare `&Config{}`), not only on a decoded one.
//!
//! Note that the `dependency_mode`/`claim_mode`/`workspace_mode` "defaults" that the C4 ticket
//! lists live in `EffectiveFor`/`ResolveProjects` (Go `effectiveOf`, Task C5), NOT in `Resolve` —
//! `resolve.go` never touches them. See the mode test files: their default cases assert through
//! `EffectiveFor`, not `Resolve`.
//!
//! # Signature deviations from the plan sketch
//!
//! The plan's interface sketch reads `resolve(Config) -> Resolved`; the faithful port of
//! `Resolve(c *Config, workflowDir string) error` needs two adjustments the sketch elides:
//!
//! * a `workflow_dir` parameter (Go's `workflowDir`) — it is stored on the resolved config and is
//!   the anchor for relative `workspace.root` / `logging.dir` values, so it cannot be dropped; and
//! * a `Result` return — Go's `Resolve` returns the bare `filepath.Abs` → `os.Getwd` error when the
//!   working directory is undiscoverable, so this port surfaces that as a value ([`ConfigError`])
//!   rather than panicking (the crate is `unwrap`-free on non-test paths).
//!
//! [`Resolved`] is a type alias for [`Config`], not a new struct: Go's `Resolve` mutates the same
//! `*Config` in place, and every later stage (`Validate`, `ResolveProjects`, `EffectiveFor`)
//! operates on that one type. The alias names the post-resolve stage without inventing a divergent
//! shape.

use crate::decode::ConfigError;
use crate::model::Config;

/// A [`Config`] that has been through [`resolve`]: `$VAR` indirection applied and the path fields
/// normalized + defaulted. An alias, not a distinct type — see the module docs.
pub type Resolved = Config;

/// Applies `$VAR` indirection and path normalization to `config` in place (Go `Resolve`).
///
/// `workflow_dir` is the directory containing the selected WORKFLOW.md; it is stored on the result
/// and used to anchor relative `workspace.root` / `logging.dir` values (upstream §6.1). Returns an
/// error only when the current working directory cannot be determined while absolutizing a relative
/// path (Go's `filepath.Abs` error path) — see [`ConfigError::WorkingDir`].
pub fn resolve(mut config: Config, workflow_dir: &str) -> Result<Resolved, ConfigError> {
    config.workflow_dir = workflow_dir.to_string();

    // tracker.api_key: $VAR only.
    config.tracker.api_key = resolve_var(&config.tracker.api_key);

    // workspace.root: $VAR + ~ + relative-to-workflow-dir + absolute. Default
    // ~/.rhapsody/workspaces (a DURABLE location alongside the DB + logs, NOT $TMPDIR). Rhapsody's
    // runtime home is ~/.rhapsody — an INTENTIONAL divergence from Go v0.4.0's ~/.symphony
    // (TRA-238; the port's first deliberate behavioral divergence — see the DIVERGENCES section in
    // the root README). The logs/db defaults below diverge the same way.
    let mut root = resolve_var(&config.workspace.root);
    if root.is_empty() {
        root = "~/.rhapsody/workspaces".to_string();
    }
    root = expand_tilde(&root);
    if !is_abs(&root) {
        root = join(workflow_dir, &root);
    }
    config.workspace.root = abs(&root)?;

    // logging.dir: $VAR + ~ + relative-to-workflow-dir + absolute (same normalization as root).
    let mut log_dir = resolve_var(&config.logging.dir);
    if log_dir.is_empty() {
        log_dir = "~/.rhapsody/logs".to_string();
    }
    log_dir = expand_tilde(&log_dir);
    if !is_abs(&log_dir) {
        log_dir = join(workflow_dir, &log_dir);
    }
    config.logging.dir = clean(&abs(&log_dir)?);

    // storage.path: $VAR + ~ + absolute. Default ~/.rhapsody/rhapsody.db (TRA-238 divergence; see the
    // workspace-root note above). "off" (case-insensitive) and ":memory:" (exact) are sentinels
    // honored verbatim — NOT path-resolved. Evaluate them against the resolved+trimmed value. Unlike
    // root/logging, a relative storage path anchors to the CWD (filepath.Abs), NOT workflow_dir, and
    // its Abs error is swallowed (path stays as-is).
    let mut sp = resolve_var(&config.storage.path);
    sp = sp.trim().to_string();
    if sp.is_empty() {
        sp = "~/.rhapsody/rhapsody.db".to_string();
    }
    if sp.eq_ignore_ascii_case("off") || sp == ":memory:" {
        config.storage.path = sp; // pass sentinel through verbatim
    } else {
        sp = expand_tilde(&sp);
        if !is_abs(&sp)
            && let Ok(absolute) = abs(&sp)
        {
            sp = absolute;
        }
        config.storage.path = clean(&sp);
    }
    if config.storage.retention_days.is_none() {
        config.storage.retention_days = Some(30);
    }

    Ok(config)
}

/// Go `resolveVar`: returns `$NAME`'s env value (empty when unset) iff `s` is exactly `$NAME` for a
/// valid identifier; otherwise `s` verbatim. Full-string match, mirroring Go's regexp
/// `^\$([A-Za-z_][A-Za-z0-9_]*)$` (hand-rolled to avoid a `regex` dependency).
fn resolve_var(s: &str) -> String {
    if let Some(name) = s.strip_prefix('$')
        && is_var_name(name)
    {
        // os.Getenv returns "" for an unset var; unwrap_or_default matches (and also maps a
        // non-UTF-8 value to "", irrelevant for the ASCII names configs use).
        return std::env::var(name).unwrap_or_default();
    }
    s.to_string()
}

/// Reports whether `name` is a valid shell identifier: a leading letter/underscore followed by
/// letters, digits, or underscores (the capture group in Go's `varPattern`).
fn is_var_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(&b) if b == b'_' || b.is_ascii_alphabetic() => {}
        _ => return false,
    }
    bytes[1..]
        .iter()
        .all(|&b| b == b'_' || b.is_ascii_alphanumeric())
}

/// Go `os.UserHomeDir` (Unix branch): `$HOME` when set and non-empty, else `None`. The daemon
/// targets macOS/Linux, so only the Unix path is ported (no passwd fallback); an undiscoverable
/// home leaves `~` unexpanded, mirroring Go's error-swallow in `expandTilde`.
fn home_dir() -> Option<String> {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => Some(h),
        _ => None,
    }
}

/// Go `expandTilde`: `~` → home, `~/x` → `home/x` (via `filepath.Join`). Any other form — or an
/// undiscoverable home — is returned unchanged (no `~user` expansion, matching Go).
fn expand_tilde(p: &str) -> String {
    if p == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return join(&home, rest);
    }
    p.to_string()
}

/// Go `filepath.IsAbs` (Unix): the path begins with `/`.
fn is_abs(p: &str) -> bool {
    p.starts_with('/')
}

/// Go `filepath.Abs` (Unix): an absolute path is cleaned; a relative one is joined onto the current
/// working directory and cleaned. Errors only when the CWD is undiscoverable (`os.Getwd`).
fn abs(p: &str) -> Result<String, ConfigError> {
    if is_abs(p) {
        return Ok(clean(p));
    }
    let wd = std::env::current_dir().map_err(|e| ConfigError::WorkingDir(e.to_string()))?;
    Ok(join(&wd.to_string_lossy(), p))
}

/// Go `filepath.Join` (Unix, the two-arg form this port needs): join the parts with `/` and clean;
/// an empty first part defers to the second, two empty parts yield "".
fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        if b.is_empty() {
            String::new()
        } else {
            clean(b)
        }
    } else {
        clean(&format!("{a}/{b}"))
    }
}

/// Faithful port of Go `filepath.Clean` (Unix semantics: separator `/`, no volume name).
///
/// Lexically simplifies a path — collapsing repeated `/`, dropping `.` elements, resolving inner
/// `..` against the preceding element, and reducing a rooted leading `/..` to `/` — without ever
/// touching the filesystem. `""` cleans to `.`. This is the byte-for-byte algorithm from Go's
/// `path/filepath`, ported so resolved paths match the Go daemon exactly (the C6 golden depends on
/// it, and real configs may carry `..`/`//` that the resolve tests do not).
fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let bytes = path.as_bytes();
    let n = bytes.len();
    let rooted = bytes[0] == b'/';

    // `out` is the cleaned path under construction; `dotdot` is the earliest index we may backtrack
    // to (Go's `out.w`/`dotdot`). Building bytes and `truncate`-ing is index-safe; the only bytes we
    // append are copied verbatim from `path` or ASCII `/`/`.`, so the result stays valid UTF-8.
    let mut out: Vec<u8> = Vec::with_capacity(n + 1);
    let mut r: usize = 0;
    let mut dotdot: usize = 0;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }

    while r < n {
        if bytes[r] == b'/' {
            // empty path element
            r += 1;
        } else if bytes[r] == b'.' && (r + 1 == n || bytes[r + 1] == b'/') {
            // . element
            r += 1;
        } else if bytes[r] == b'.'
            && r + 1 < n
            && bytes[r + 1] == b'.'
            && (r + 2 == n || bytes[r + 2] == b'/')
        {
            // .. element: remove the last element written since `dotdot`
            r += 2;
            if out.len() > dotdot {
                // backtrack to the separator before the last element
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, and not rooted, so keep the .. element
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            // real path element: add a separator if one is needed, then copy the element
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && bytes[r] != b'/' {
                out.push(bytes[r]);
                r += 1;
            }
        }
    }

    if out.is_empty() {
        return ".".to_string();
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::workflow::{Definition, YamlMap};
    use std::path::Path;

    /// A `Config` decoded from empty front matter — the stand-in for the Go resolve tests' bare
    /// `&Config{}`. Each test then overrides only the field it exercises. The decode-defaults on
    /// the other fields never affect the asserted outcome, because `resolve` transforms each of
    /// its fields independently (and the storage tests below decode+resolve for real, exactly like
    /// Go's `decodeResolveStorage`).
    fn decode_blank() -> Config {
        let def = Definition {
            config: YamlMap::new(),
            prompt_template: String::new(),
        };
        decode(&def).expect("decode of empty front matter")
    }

    // Build a config from a base tracker/workspace front matter (+ optional storage block) and run
    // Decode + Resolve against `workflow_dir`. Mirrors Go `decodeResolveStorage`.
    fn decode_resolve_storage(storage: Option<&str>, workflow_dir: &str) -> Config {
        let mut front = String::from(
            "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nworkspace:\n  root: /ws/root\n",
        );
        if let Some(block) = storage {
            front.push_str(block);
        }
        let config: YamlMap = serde_yaml_ng::from_str(&front).expect("front matter parses");
        let def = Definition {
            config,
            prompt_template: "Body.".to_string(),
        };
        let decoded = decode(&def).expect("decode");
        resolve(decoded, workflow_dir).expect("resolve")
    }

    // ---- resolve_test.go mirrors ----

    // Mirrors Go `TestResolveWorkspaceRootRelativeWorkflowDirBecomesAbsolute`.
    #[test]
    fn workspace_root_relative_workflow_dir_becomes_absolute() {
        let mut c = decode_blank();
        c.workspace.root = "ws".to_string();
        let r = resolve(c, ".").expect("resolve");
        assert!(
            Path::new(&r.workspace.root).is_absolute(),
            "workspace root must be absolute, got {:?}",
            r.workspace.root
        );
    }

    // Mirrors Go `TestResolveAPIKeyFromEnv`. Uses `$HOME` (a reliably-set var) instead of a
    // test-mutated var: it exercises the identical `resolveVar` → env-lookup path while keeping the
    // test sound under Rust 2024's parallel-test env model (no `unsafe { set_var }`, no data race).
    #[test]
    fn api_key_from_env() {
        let mut c = decode_blank();
        c.tracker.api_key = "$HOME".to_string();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        assert_eq!(r.tracker.api_key, std::env::var("HOME").unwrap_or_default());
    }

    // Mirrors Go `TestResolveAPIKeyEmptyEnvBecomesEmpty`.
    #[test]
    fn api_key_empty_env_becomes_empty() {
        let mut c = decode_blank();
        c.tracker.api_key = "$DEFINITELY_UNSET_VAR_XYZ".to_string();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        assert_eq!(r.tracker.api_key, "", "unset $VAR should resolve to empty");
    }

    // Mirrors Go `TestResolveLiteralAPIKeyUnchanged`.
    #[test]
    fn literal_api_key_unchanged() {
        let mut c = decode_blank();
        c.tracker.api_key = "lin_api_literal".to_string();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        assert_eq!(r.tracker.api_key, "lin_api_literal");
    }

    // Mirrors Go `TestResolveWorkspaceRootTildeAndAbsolute`.
    #[test]
    fn workspace_root_tilde_and_absolute() {
        let mut c = decode_blank();
        c.workspace.root = "~/symphony_ws".to_string();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(r.workspace.root, join(&home, "symphony_ws"));
    }

    // Mirrors Go `TestResolveWorkspaceRootRelativeToWorkflowDir`.
    #[test]
    fn workspace_root_relative_to_workflow_dir() {
        let mut c = decode_blank();
        c.workspace.root = "ws".to_string();
        let r = resolve(c, "/home/user/project").expect("resolve");
        assert_eq!(r.workspace.root, "/home/user/project/ws");
    }

    // Mirrors Go `TestResolveWorkspaceRootDefault`, but asserts Rhapsody's ~/.rhapsody/workspaces
    // default — the TRA-238 divergence from Go v0.4.0's ~/.symphony/symphony_workspaces.
    #[test]
    fn workspace_root_default() {
        let mut c = decode_blank();
        c.workspace.root = String::new();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        let want = clean(&expand_tilde("~/.rhapsody/workspaces"));
        assert_eq!(r.workspace.root, want);
        assert!(Path::new(&r.workspace.root).is_absolute());
        assert!(!r.workspace.root.contains('~'), "root not normalized");
    }

    // Mirrors Go `TestResolveWorkspaceRootRelativeWithDotWorkflowDir`.
    #[test]
    fn workspace_root_relative_with_dot_workflow_dir() {
        let mut c = decode_blank();
        c.workspace.root = "ws".to_string();
        let r = resolve(c, ".").expect("resolve");
        assert!(Path::new(&r.workspace.root).is_absolute());
    }

    // Mirrors Go `TestResolveSetsWorkflowDir`.
    #[test]
    fn sets_workflow_dir() {
        let r = resolve(decode_blank(), "/some/dir").expect("resolve");
        assert_eq!(r.workflow_dir, "/some/dir");
    }

    // Mirrors Go `TestResolveLoggingDirRelativeToWorkflowDir`.
    #[test]
    fn logging_dir_relative_to_workflow_dir() {
        let mut c = decode_blank();
        c.logging.dir = "logs".to_string();
        let r = resolve(c, "/home/user/project").expect("resolve");
        assert_eq!(r.logging.dir, "/home/user/project/logs");
        assert!(Path::new(&r.logging.dir).is_absolute());
    }

    // Mirrors Go `TestResolveLoggingDirTildeAbsolute`.
    #[test]
    fn logging_dir_tilde_absolute() {
        let mut c = decode_blank();
        c.logging.dir = "~/.symphony/logs".to_string();
        let r = resolve(c, "/tmp/wf").expect("resolve");
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(r.logging.dir, join(&home, ".symphony/logs"));
        assert!(Path::new(&r.logging.dir).is_absolute());
    }

    // ---- storage_test.go mirrors (the Resolve/default half; the ValidateDispatch cases are C5) ----

    // Mirrors Go `TestStorageDefaultPath`, but asserts Rhapsody's ~/.rhapsody/rhapsody.db default —
    // the TRA-238 divergence from Go v0.4.0's ~/.symphony/symphony.db.
    #[test]
    fn storage_default_path() {
        let cfg = decode_resolve_storage(None, "/wf");
        let want = clean(&expand_tilde("~/.rhapsody/rhapsody.db"));
        assert_eq!(cfg.storage.path, want, "default storage path");
        assert_eq!(cfg.storage.retention_days, Some(30), "retention default");
        // Neither off nor in-memory (Go: !Storage.Off() && !Storage.InMemory()).
        assert!(!cfg.storage.path.eq_ignore_ascii_case("off"));
        assert_ne!(cfg.storage.path, ":memory:");
    }

    // Mirrors Go `TestStorageOff`.
    #[test]
    fn storage_off() {
        let cfg = decode_resolve_storage(Some("storage:\n  path: \"off\"\n"), "/wf");
        assert_eq!(cfg.storage.path, "off", "off path should pass through");
    }

    // Mirrors Go `TestStorageInMemory`.
    #[test]
    fn storage_in_memory() {
        let cfg = decode_resolve_storage(Some("storage:\n  path: \":memory:\"\n"), "/wf");
        assert_eq!(
            cfg.storage.path, ":memory:",
            ":memory: path should pass through"
        );
    }

    // Mirrors Go `TestStorageVarExpansion`. Uses `$HOME` (see `api_key_from_env` for why); like the
    // Go test's `/custom/db/...`, it is an absolute value that resolve passes through unchanged.
    #[test]
    fn storage_var_expansion() {
        let cfg = decode_resolve_storage(Some("storage:\n  path: $HOME\n"), "/wf");
        let want = clean(&std::env::var("HOME").unwrap_or_default());
        assert_eq!(cfg.storage.path, want);
    }

    // Mirrors Go `TestStorageRelativePathAbsolute`. A relative storage path anchors to the CWD.
    #[test]
    fn storage_relative_path_absolute() {
        let cfg = decode_resolve_storage(Some("storage:\n  path: rel/db.sqlite\n"), "/wf");
        assert!(
            Path::new(&cfg.storage.path).is_absolute(),
            "relative path must resolve absolute, got {:?}",
            cfg.storage.path
        );
        assert!(
            cfg.storage.path.ends_with("rel/db.sqlite"),
            "relative path suffix wrong: {:?}",
            cfg.storage.path
        );
    }

    // ---- filepath helper coverage (locks the ported path lexicon to Go's `path/filepath`) ----

    // Cases straight from Go's `filepath.Clean` doc/example: collapse `//`, drop `.`, resolve `..`,
    // reduce leading `/..`, and turn "" into ".".
    #[test]
    fn clean_matches_go_filepath() {
        assert_eq!(clean("a/c"), "a/c");
        assert_eq!(clean("a//c"), "a/c");
        assert_eq!(clean("a/c/."), "a/c");
        assert_eq!(clean("a/c/b/.."), "a/c");
        assert_eq!(clean("/../a/c"), "/a/c");
        assert_eq!(clean("/../a/b/../././/c"), "/a/c");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("."), ".");
        assert_eq!(clean(".."), "..");
        assert_eq!(clean("a/../.."), "..");
        assert_eq!(clean("/"), "/");
        assert_eq!(clean("/a/"), "/a");
    }

    // Go `filepath.Join` two-arg behavior (empty parts, trailing-slash cleanup).
    #[test]
    fn join_matches_go_filepath() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/home/user/project", "ws"), "/home/user/project/ws");
        assert_eq!(join("a", ""), "a");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("", ""), "");
        assert_eq!(join("/a/", "/b"), "/a/b");
    }

    // Go `filepath.IsAbs` (Unix).
    #[test]
    fn is_abs_matches_go_filepath() {
        assert!(is_abs("/a/b"));
        assert!(is_abs("/"));
        assert!(!is_abs("a/b"));
        assert!(!is_abs("./a"));
        assert!(!is_abs(""));
    }

    // Go `resolveVar`: exact `$NAME` match only; everything else is literal.
    #[test]
    fn resolve_var_full_match_only() {
        assert_eq!(resolve_var("plain"), "plain");
        assert_eq!(resolve_var("$UNSET_VAR_ABC_XYZ"), "");
        assert_eq!(resolve_var("$"), "$"); // no identifier => literal
        assert_eq!(resolve_var("${HOME}"), "${HOME}"); // braces are not the pattern
        assert_eq!(resolve_var("$HOME/x"), "$HOME/x"); // must match the whole string
        assert_eq!(resolve_var("pre$HOME"), "pre$HOME");
        // A set var resolves to its value (HOME is reliably present in the test env).
        assert_eq!(
            resolve_var("$HOME"),
            std::env::var("HOME").unwrap_or_default()
        );
    }

    // Go `expandTilde`: `~` and `~/x` expand against $HOME; other forms pass through untouched.
    #[test]
    fn expand_tilde_forms() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/sub/dir"), join(&home, "sub/dir"));
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
        assert_eq!(expand_tilde("~user"), "~user"); // no ~user expansion
    }
}
