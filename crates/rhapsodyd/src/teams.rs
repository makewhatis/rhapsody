//! teams — the `rhapsodyd teams` subcommand (STUDIO-642; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §4).
//!
//! Two verbs, dispatched at the very top of [`crate::run::run`] beside `mcp` so
//! the daemon's run-lock and flag parsing are untouched:
//!
//! * `rhapsodyd teams show <identity|profile>` — prints the fully-resolved
//!   prompt text plus its provenance. §4 states the bar plainly: layering means
//!   "what prompt does Alice actually get" is no longer answered by opening one
//!   file, and *any implementation that cannot answer that question in one
//!   command has got the trade wrong*. This is that command.
//! * `rhapsodyd teams fork <profile> [--force]` — materialises the resolved text
//!   into `~/.rhapsody/teams/profiles/<profile>.md` with `extends: none`.
//!
//! **`fork` is the ONE write in this slice, and only on this explicit command.**
//! Everything else here — resolving, showing, the boot-time roster report — only
//! ever READS, and never creates the profiles directory (§4).

use std::io::Write;
use std::path::{Path, PathBuf};

use rhapsody_config::profiles::{self, BodyOrigin, Origin, ResolvedProfile};
use rhapsody_config::teams::Teams;
use rhapsody_config::{Config, workflow};

use crate::bootcfg::{resolve_profiles_dir, resolve_teams_path};

/// Runs `rhapsodyd teams <verb> …`, writing the report to `stdout` and any error
/// to `stderr` behind the `symphony teams:` marker (the dispatch marker
/// convention `run_mcp` established). Returns the process exit code: 0 on
/// success, 1 on any failure — including an unknown verb, since a mistyped verb
/// that exited 0 would look like it had done something.
pub fn run_teams<O, E>(args: &[String], mut stdout: O, mut stderr: E) -> i32
where
    O: Write,
    E: Write,
{
    match teams_command(args, &|k| std::env::var(k).unwrap_or_default()) {
        Ok(out) => {
            let _ = write!(stdout, "{out}");
            0
        }
        Err(e) => {
            let _ = writeln!(stderr, "symphony teams: {e}");
            1
        }
    }
}

/// Resolves the paths the verbs work against, then dispatches. Factored out of
/// [`run_teams`] so the verbs are unit-testable without hijacking stdout.
fn teams_command(args: &[String], getenv: &dyn Fn(&str) -> String) -> Result<String, String> {
    let (teams_path, profiles_dir) = resolve_paths(getenv);
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    match verb {
        "show" => show(rest, &teams_path, &profiles_dir),
        "fork" => fork(rest, &profiles_dir),
        "" => Err("usage: rhapsodyd teams <show|fork> <name>".to_string()),
        other => Err(format!(
            "unknown verb {other:?}; usage: rhapsodyd teams <show|fork> <name>"
        )),
    }
}

/// Locates `teams.yaml` and the profiles directory the same way the daemon does
/// — anchored to the resolved store home — falling back to the DEFAULTS when no
/// workflow can be loaded, so `teams show swe` still prints the built-in from a
/// directory that has no `WORKFLOW.md`.
fn resolve_paths(getenv: &dyn Fn(&str) -> String) -> (PathBuf, PathBuf) {
    let cfg = load_config(getenv);
    let teams_path =
        resolve_teams_path(cfg.as_ref(), "", false).unwrap_or_else(|| PathBuf::from("teams.yaml"));
    let profiles_dir = resolve_profiles_dir(cfg.as_ref(), "", false)
        .unwrap_or_else(|| PathBuf::from("teams").join("profiles"));
    (teams_path, profiles_dir)
}

/// Loads + decodes + resolves the workflow the daemon would use (`SYMPHONY_WORKFLOW`,
/// else `WORKFLOW.md`), falling back to a BLANK front matter run through the same
/// `decode` → `resolve` pipeline when there is no readable workflow — so the
/// sidecar paths still land on the `~/.rhapsody` defaults, exactly as
/// `run::resolve_boot_logdir` does for the log dir. An operator inspecting a
/// profile should not need a `WORKFLOW.md` in the current directory.
fn load_config(getenv: &dyn Fn(&str) -> String) -> Option<Config> {
    let w = getenv("SYMPHONY_WORKFLOW");
    let path = Path::new(if w.is_empty() { "WORKFLOW.md" } else { &w });
    let def = workflow::load(path).unwrap_or(workflow::Definition {
        config: workflow::YamlMap::new(),
        prompt_template: String::new(),
    });
    let cfg = rhapsody_config::decode(&def).ok()?;
    rhapsody_config::resolve(cfg, &crate::bootcfg::workflow_dir(path)).ok()
}

/// `teams show <identity|profile>`: the arg is looked up as a roster identity
/// first (printing the profile it wears), then as a profile name directly —
/// which is what makes `teams show alice` and `teams show swe` both work.
fn show(args: &[String], teams_path: &Path, profiles_dir: &Path) -> Result<String, String> {
    let name = args
        .first()
        .filter(|a| !a.is_empty())
        .ok_or("usage: rhapsodyd teams show <identity|profile>")?;
    // Best-effort: a broken teams.yaml must not stop an operator inspecting a
    // profile, so `show` falls back to treating the arg as a profile name.
    let teams = Teams::load(teams_path);
    let identity = teams.roster.iter().find(|i| &i.name == name);
    let profile_name = match identity {
        Some(i) if i.profile.is_empty() => {
            return Err(format!("identity {name:?} names no profile"));
        }
        Some(i) => i.profile.clone(),
        None => name.clone(),
    };
    let resolved =
        profiles::resolve(profiles_dir, &profile_name).map_err(|e| format!("{name}: {e}"))?;
    Ok(render_show(identity.map(|i| i.name.as_str()), &resolved))
}

/// The `teams show` report: provenance first, then the resolved prompt, so the
/// two questions §4 poses — "which base is this" and "what text does it produce"
/// — are both answered by one screen.
fn render_show(identity: Option<&str>, r: &ResolvedProfile) -> String {
    let mut out = String::new();
    if let Some(i) = identity {
        out.push_str(&format!("identity:     {i}\n"));
    }
    out.push_str(&format!("profile:      {}\n", r.name));
    match &r.provenance.base {
        Some(b) => out.push_str(&format!(
            "base:         {}@{} ({})\n",
            b.name,
            b.version,
            if b.pinned {
                "pinned"
            } else {
                "tracking latest"
            }
        )),
        None => out.push_str("base:         none (fork — this file is the whole profile)\n"),
    }
    match &r.provenance.overlay {
        Some(p) => out.push_str(&format!("overlay:      {}\n", p.display())),
        None => out.push_str("overlay:      none (the built-in, unmodified)\n"),
    }
    if let Some(d) = &r.provenance.drift {
        out.push_str(&format!(
            "drift:        pinned to {}@{}; the built-in is now {}@{} (reported, never merged)\n",
            d.name, d.pinned, d.name, d.latest
        ));
    }
    out.push_str(&format!(
        "model:        {}\n",
        field(&r.model, r.provenance.model)
    ));
    out.push_str(&format!(
        "effort:       {}\n",
        field(&r.effort, r.provenance.effort)
    ));
    out.push_str(&format!(
        "capabilities: {}\n",
        list_field(&r.capabilities, r.provenance.capabilities)
    ));
    out.push_str(&format!(
        "tools:        {} (parsed, unused in this slice)\n",
        list_field(&r.tools, r.provenance.tools)
    ));
    out.push_str(&format!(
        "body:         {}\n",
        match r.provenance.body {
            BodyOrigin::Base => "from the base (the overlay body is empty)",
            BodyOrigin::Overlay => "from the overlay (replaces the base wholesale)",
            BodyOrigin::Spliced => "the overlay, with the base spliced in at {{ base }}",
        }
    ));
    out.push_str("\n--- resolved prompt ---\n");
    out.push_str(&r.prompt);
    out.push('\n');
    out
}

fn origin_tag(o: Origin) -> &'static str {
    match o {
        Origin::Base => "[base]",
        Origin::Overlay => "[overlay]",
        Origin::Unset => "[unset — inherits the daemon's config]",
    }
}

fn field(value: &str, o: Origin) -> String {
    if value.is_empty() {
        origin_tag(o).to_string()
    } else {
        format!("{value} {}", origin_tag(o))
    }
}

fn list_field(values: &[String], o: Origin) -> String {
    if values.is_empty() {
        origin_tag(o).to_string()
    } else {
        format!("{} {}", values.join(", "), origin_tag(o))
    }
}

/// `teams fork <profile> [--force]`: materialise the fully-resolved text into
/// the user's own file with `extends: none`, so choosing seed-once semantics is
/// one explicit command (§4).
///
/// It refuses to overwrite an existing file unless `--force` is passed. §4's
/// invariant is that Rhapsody only ever READS a user's profile file; a `fork`
/// that silently clobbered a file the user had authored would be that invariant
/// broken by the one command allowed to write.
fn fork(args: &[String], profiles_dir: &Path) -> Result<String, String> {
    let mut name = None;
    let mut force = false;
    for a in args {
        match a.as_str() {
            "--force" | "-f" => force = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag {other:?}"));
            }
            other if name.is_none() && !other.is_empty() => name = Some(other.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let name = name.ok_or("usage: rhapsodyd teams fork <profile> [--force]")?;
    let resolved = profiles::resolve(profiles_dir, &name).map_err(|e| e.to_string())?;
    let path = profiles::profile_path(profiles_dir, &name);
    if path.exists() && !force {
        return Err(format!(
            "{} already exists; pass --force to overwrite it with its own fully-resolved text",
            path.display()
        ));
    }
    // The one directory this slice may create, and only here.
    std::fs::create_dir_all(profiles_dir)
        .map_err(|e| format!("create {}: {e}", profiles_dir.display()))?;
    let def = profiles::fork_definition(&resolved);
    workflow::save(&path, &def).map_err(|e| format!("write {}: {e}", path.display()))?;
    let base = match &resolved.provenance.base {
        Some(b) => format!("{}@{}", b.name, b.version),
        None => "none".to_string(),
    };
    Ok(format!(
        "forked {name} from {base} into {}\nit is now yours: `extends: none`, and Rhapsody will not update it again\n",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Points the verbs at a hermetic store home by writing a WORKFLOW.md whose
    /// `storage.path` sits under `dir`, and returns the resolved profiles dir.
    fn hermetic(dir: &TempDir) -> (Vec<String>, PathBuf) {
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            format!(
                "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\nstorage:\n  path: {}/rhapsody.db\n---\nDo {{{{ issue.identifier }}}}.\n",
                dir.path.display()
            ),
        )
        .expect("write WORKFLOW.md");
        let env = vec![wf.to_string_lossy().into_owned()];
        (env, dir.path.join("teams").join("profiles"))
    }

    fn getenv_for(wf: &str) -> impl Fn(&str) -> String + '_ {
        move |k: &str| {
            if k == "SYMPHONY_WORKFLOW" {
                wf.to_string()
            } else {
                String::new()
            }
        }
    }

    fn run(args: &[&str], wf: &str) -> Result<String, String> {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        teams_command(&owned, &getenv_for(wf))
    }

    /// §4's one-command bar: `teams show <profile>` prints the resolved prompt
    /// AND its provenance, with no profile file anywhere on disk.
    #[test]
    fn show_prints_the_builtin_prompt_and_its_provenance() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(out.contains("profile:      swe"), "out = {out}");
        assert!(
            out.contains("base:         swe@1 (tracking latest)"),
            "out = {out}"
        );
        assert!(out.contains("overlay:      none"), "out = {out}");
        assert!(out.contains("--- resolved prompt ---"), "out = {out}");
        assert!(
            out.contains("You are a software engineer on this codebase."),
            "the resolved prompt text must be printed: {out}"
        );
        assert!(
            !profiles_dir.exists(),
            "show must not create {}",
            profiles_dir.display()
        );
    }

    /// `teams show <identity>` resolves through the roster, which is the
    /// question §4 actually poses ("what prompt does Alice actually get").
    #[test]
    fn show_resolves_an_identity_through_the_roster() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: reviewer\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(out.contains("identity:     alice"), "out = {out}");
        assert!(out.contains("profile:      reviewer"), "out = {out}");
        assert!(
            out.contains("You are a code reviewer on this codebase."),
            "out = {out}"
        );
        assert!(!profiles_dir.exists(), "show must not create the dir");
    }

    /// An overlay's provenance — including the pin's drift line — is what the
    /// operator sees, and the prompt is the composed one.
    #[test]
    fn show_reports_an_overlay_and_its_splice() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join("swe.md"),
            "---\nextends: swe\nmodel: opus\n---\n{{ base }}\n\nHouse rule: cite the ticket.\n",
        )
        .expect("write overlay");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(out.contains("model:        opus [overlay]"), "out = {out}");
        assert!(
            out.contains("the overlay, with the base spliced in at {{ base }}"),
            "out = {out}"
        );
        assert!(out.contains("House rule: cite the ticket."), "out = {out}");
        assert!(
            out.contains("You are a software engineer on this codebase."),
            "the base must be spliced in: {out}"
        );
    }

    /// An unknown name is a loud failure, not an empty report.
    #[test]
    fn show_rejects_an_unknown_name() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        let err = run(&["show", "nobody"], &env[0]).expect_err("must fail");
        assert!(err.contains("profile_unknown:"), "err = {err}");
    }

    /// `fork` writes the ONE named file (creating the directory it needs), and
    /// the result is self-contained: `extends: none` and the resolved prose.
    #[test]
    fn fork_materialises_a_self_contained_file() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        assert!(!profiles_dir.exists());

        let out = run(&["fork", "sre"], &env[0]).expect("fork sre");
        assert!(out.contains("forked sre from sre@1"), "out = {out}");

        let path = profiles_dir.join("sre.md");
        let text = std::fs::read_to_string(&path).expect("read forked file");
        assert!(text.starts_with("---\nextends: none\n"), "text = {text}");
        assert!(
            text.contains("You are a site reliability engineer on this system."),
            "the resolved prose must be materialised: {text}"
        );
        // And only that file: forking `sre` does not write `swe` or `reviewer`.
        let mut names: Vec<String> = std::fs::read_dir(&profiles_dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        assert_eq!(names, vec!["sre.md".to_string()]);

        // The fork now resolves with no base at all.
        let shown = run(&["show", "sre"], &env[0]).expect("show sre");
        assert!(
            shown.contains("base:         none (fork"),
            "shown = {shown}"
        );
    }

    /// `fork` refuses to clobber a user's file — §4's read-only invariant holds
    /// even inside the one command allowed to write — unless `--force` is given.
    #[test]
    fn fork_refuses_to_overwrite_without_force() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        let path = profiles_dir.join("swe.md");
        std::fs::write(&path, "---\nextends: swe\n---\nMine.\n").expect("write overlay");

        let err = run(&["fork", "swe"], &env[0]).expect_err("must refuse");
        assert!(err.contains("already exists"), "err = {err}");
        assert!(err.contains("--force"), "err = {err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "---\nextends: swe\n---\nMine.\n",
            "the refusal must leave the file untouched"
        );

        let out = run(&["fork", "swe", "--force"], &env[0]).expect("--force overwrites");
        assert!(out.contains("forked swe from swe@1"), "out = {out}");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("---\nextends: none\n"), "text = {text}");
        assert!(
            text.contains("Mine."),
            "the overlay's own body survives: {text}"
        );
    }

    /// An unknown profile is not forked, and nothing is created on the way out.
    #[test]
    fn fork_rejects_an_unknown_profile_and_creates_nothing() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        let err = run(&["fork", "nobody"], &env[0]).expect_err("must fail");
        assert!(err.contains("profile_unknown:"), "err = {err}");
        assert!(
            !profiles_dir.exists(),
            "a failed fork must not create {}",
            profiles_dir.display()
        );
    }

    /// Usage errors are loud and non-zero rather than a silent success.
    #[test]
    fn missing_and_unknown_verbs_are_errors() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        assert!(run(&[], &env[0]).expect_err("no verb").contains("usage:"));
        assert!(
            run(&["wat"], &env[0])
                .expect_err("bad verb")
                .contains("unknown verb")
        );
        assert!(
            run(&["show"], &env[0])
                .expect_err("no name")
                .contains("usage:")
        );
        assert!(
            run(&["fork"], &env[0])
                .expect_err("no name")
                .contains("usage:")
        );
        assert!(
            run(&["fork", "swe", "--wat"], &env[0])
                .expect_err("bad flag")
                .contains("unknown flag")
        );
    }

    /// `run_teams` prints the report to stdout on success and the marked error
    /// to stderr on failure, with the exit codes the dispatch contract needs.
    #[test]
    fn run_teams_writes_the_right_stream_and_code() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        // SAFETY-adjacent: this test sets the process env, so it reads it back
        // through the same public entry point the binary uses.
        unsafe { std::env::set_var("SYMPHONY_WORKFLOW", &env[0]) };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_teams(&["show".to_string(), "swe".to_string()], &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("--- resolved prompt ---"));
        assert!(err.is_empty());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_teams(
            &["show".to_string(), "nobody".to_string()],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(String::from_utf8_lossy(&err).starts_with("symphony teams: "));
        unsafe { std::env::remove_var("SYMPHONY_WORKFLOW") };
    }
}
