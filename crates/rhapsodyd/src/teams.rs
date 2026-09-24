//! teams — the `rhapsodyd teams` subcommand (STUDIO-642; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §4).
//!
//! Two verbs, dispatched at the very top of [`crate::run::run`] beside `mcp` so
//! the daemon's run-lock and flag parsing are untouched:
//!
//! * `rhapsodyd teams show <identity|profile> [--room N]` — prints the
//!   fully-resolved prompt text plus its provenance. §4 states the bar plainly:
//!   layering means "what prompt does Alice actually get" is no longer answered
//!   by opening one file, and *any implementation that cannot answer that
//!   question in one command has got the trade wrong*. This is that command.
//!   Since STUDIO-670 it also prints the room's recent tail, so the second
//!   question an operator in a terminal has — "what has the team been saying?" —
//!   is answered by the same command instead of by tailing JSONL by hand.
//! * `rhapsodyd teams fork <profile> [--force]` — materialises the resolved text
//!   into `~/.rhapsody/teams/profiles/<profile>.md` with `extends: none`.
//!
//! **`fork` is the ONE write in this slice, and only on this explicit command.**
//! Everything else here — resolving, showing, the boot-time roster report — only
//! ever READS, and never creates the profiles directory (§4).

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::SecondsFormat;
use rhapsody_config::profiles::{self, BodyOrigin, Origin, ResolvedProfile};
use rhapsody_config::providers::provider_turn_deadline_ms;
use rhapsody_config::room::{Cursor, LocalRoom, Message};
use rhapsody_config::teams::{Identity, Review, Teams};
use rhapsody_config::{Config, workflow};
use rhapsody_orchestrator::selection::{
    FieldSelection, SelectionOrigins, SelectionRequest, SelectionTiers, resolve_manager_selection,
    resolve_selection,
};

use crate::bootcfg::{
    resolve_manager_rules_path, resolve_profiles_dir, resolve_room_dir, resolve_teams_path,
};

/// How many room messages `teams show` prints when `--room` is not given
/// (STUDIO-670). A glance, not a catch-up: the dashboard (STUDIO-652) is where
/// an operator scrolls, and [`LocalRoom::read_since`] clamps anything wider to
/// the room's own `MAX_ROOM_WINDOW` regardless.
const DEFAULT_ROOM_TAIL: usize = 10;

/// The widest one rendered room line may be, in CHARS. A room body is capped at
/// 600 bytes on read, which is several terminal lines; the tail is only legible
/// as a glance if one message is one line.
const ROOM_LINE_WIDTH: usize = 120;

/// The usage line both `show`'s argument errors quote, so a mistyped flag and a
/// missing name teach the same syntax.
const SHOW_USAGE: &str = "usage: rhapsodyd teams show <identity|profile> [--room N]";

/// Runs `rhapsodyd teams <verb> …`, writing the report to `stdout` and any error
/// to `stderr` behind the `symphony teams:` marker (the dispatch marker
/// convention `run_mcp` established). Returns the process exit code: 0 on
/// success, 1 on any failure — including an unknown verb, since a mistyped verb
/// that exited 0 would look like it had done something.
pub fn run_teams<O, E>(args: &[String], stdout: O, stderr: E) -> i32
where
    O: Write,
    E: Write,
{
    run_teams_with(
        args,
        &|k| std::env::var(k).unwrap_or_default(),
        stdout,
        stderr,
    )
}

/// [`run_teams`] with the environment injected, so the tests exercise the real
/// stream/exit-code contract against a hermetic temp home instead of mutating
/// the process environment out from under a parallel test.
fn run_teams_with<O, E>(
    args: &[String],
    getenv: &dyn Fn(&str) -> String,
    mut stdout: O,
    mut stderr: E,
) -> i32
where
    O: Write,
    E: Write,
{
    match teams_command(args, getenv) {
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
    // Loaded once and shared: the paths and, for `show`, the configured backend an inheriting
    // profile falls back to (STUDIO-903). `resolve_paths` errors when there is no runtime home,
    // which is also the only case where the backend is unavailable — so a successful resolve
    // always carries one.
    let cfg = load_config(getenv);
    let (teams_path, profiles_dir, room_dir, rules_path) = resolve_paths(cfg.as_ref())?;
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    match verb {
        "show" => show(
            rest,
            &teams_path,
            &profiles_dir,
            &room_dir,
            &rules_path,
            cfg.as_ref(),
        ),
        "fork" => fork(rest, &profiles_dir),
        "" => Err("usage: rhapsodyd teams <show|fork> <name>".to_string()),
        other => Err(format!(
            "unknown verb {other:?}; usage: rhapsodyd teams <show|fork> <name>"
        )),
    }
}

/// Locates `teams.yaml`, the profiles directory and the room the same way the daemon does
/// — anchored to the resolved store home. A directory with no `WORKFLOW.md` is
/// fine (see [`load_config`]: the defaults still land on `~/.rhapsody`).
///
/// When there is no on-disk store home to anchor to — `storage.path` is `off` or
/// `:memory:`, or the workflow will not decode — this is an ERROR rather than a
/// guess. The obvious fallback, a relative `./teams/profiles/`, would mean
/// `teams fork` quietly creating directories in whatever directory the operator
/// happened to be standing in, which is exactly the kind of surprise write §4's
/// read-only posture exists to avoid.
fn resolve_paths(cfg: Option<&Config>) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    // All four anchor to the same runtime home, so they resolve or fail together.
    match (
        resolve_teams_path(cfg, "", false),
        resolve_profiles_dir(cfg, "", false),
        resolve_room_dir(cfg, "", false),
        resolve_manager_rules_path(cfg, "", false),
    ) {
        (Some(teams), Some(profiles), Some(room), Some(rules)) => {
            Ok((teams, profiles, room, rules))
        }
        _ => Err(
            "no Rhapsody runtime home to read profiles from: the workflow does not decode, or \
             storage.path is `off`/`:memory:`. Point SYMPHONY_WORKFLOW at a workflow with an \
             on-disk storage.path."
                .to_string(),
        ),
    }
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

/// `teams show <identity|profile> [--room N]`: the arg is looked up as a roster
/// identity first (printing the profile it wears), then as a profile name
/// directly — which is what makes `teams show alice` and `teams show swe` both
/// work.
fn show(
    args: &[String],
    teams_path: &Path,
    profiles_dir: &Path,
    room_dir: &Path,
    rules_path: &Path,
    cfg: Option<&Config>,
) -> Result<String, String> {
    let (name, room_tail) = parse_show_args(args)?;
    let backend = cfg.map_or("", |c| c.agent.backend.as_str());
    // Best-effort: a broken teams.yaml must not stop an operator inspecting a
    // profile, so `show` falls back to treating the arg as a profile name.
    //
    // `try_load` rather than `load` since STUDIO-891, for the REASON and not for
    // the value: a rejected config degrades to the off state and the daemon
    // still exits 0, so the only evidence an operator gets is an absence — a
    // roster that silently does not resolve. This command needs no daemon and no
    // log access, which makes it the right place to say what was refused. The
    // `Err` arm still yields the off state, so the fallback above is unchanged.
    let (teams, rejected) = match Teams::try_load(teams_path) {
        Ok(t) => (t, String::new()),
        Err(e) => (Teams::disabled(), e.to_string()),
    };
    let identity = teams.roster.iter().find(|i| i.name == name);
    let profile_name = match identity {
        Some(i) if i.profile.is_empty() => {
            return Err(format!("identity {name:?} names no profile"));
        }
        Some(i) => i.profile.clone(),
        None => name.clone(),
    };
    // The manager is the daemon's own built-in identity (STUDIO-1013): its prompt is the built-in
    // `manager` profile with the maintainer's standing rules rendered into it as policy, and
    // `show` is the one command that answers "what prompt does it actually get". Every other
    // profile resolves unchanged.
    let resolved = if profile_name == rhapsody_config::manager::MANAGER_PROFILE {
        rhapsody_config::manager::resolve_prompt(profiles_dir, rules_path)
    } else {
        profiles::resolve(profiles_dir, &profile_name)
    }
    .map_err(|e| format!("{name}: {e}"))?;
    // Teams off has no room to speak of, so its report is byte-identical to the
    // one this command printed before the section existed (STUDIO-670).
    //
    // `room_tail > 0` is what makes `--room 0` SUPPRESS the section, and it has
    // to be decided here: the room reads a `limit` of 0 as "no particular
    // limit" and answers with its DEFAULT window (`effective_limit`), so
    // passing the count straight through would turn the documented way to
    // silence the tail into the widest one this command can print.
    let room = if teams.enabled && room_tail > 0 {
        render_room(room_dir, room_tail)
    } else {
        String::new()
    };
    // `review.model`/`review.effort` are only ever consulted on the ticketless path
    // (STUDIO-901; `Teams::review_ticketless`) — on any other install (including the default,
    // `mode: off`) `dispatch_issue` never reaches the block that reads them, so a set value is
    // dead config. `None` here is what makes `render_show` suppress the two lines entirely rather
    // than asserting an override that install cannot honour, so a Teams-off `show` (no
    // `teams.yaml` at all) prints no line this addition did not exist to add (the alignment fix
    // widened every label's gutter by one, so it is not byte-identical to pre-STUDIO-901 output).
    let review = teams.review_ticketless().then_some(&teams.review);
    // The effective Teams/manager/review tuple and its per-tier origins (STUDIO-993, P12). Built
    // from the SAME pure resolver the dispatch path consumes (`rhapsody_orchestrator::selection`),
    // so `show` reports what a run would actually select rather than a second resolution algorithm
    // that could disagree with it. Gated on `teams.enabled`: an installation with no Teams has no
    // manager and no routing fields, and this section would be a claim about a feature that is off.
    let effective = match cfg {
        Some(cfg) if teams.enabled => render_effective(cfg, &teams, identity, &resolved),
        _ => String::new(),
    };
    Ok(render_show(
        identity.map(|i| i.name.as_str()),
        &resolved,
        review,
        &room,
        backend,
        &render_rejection_for(teams_path, &rejected),
        &effective,
    ))
}

/// The banner a rejected `teams.yaml` gets, above everything else `show` prints
/// so it cannot scroll off the top of a long prompt.
///
/// It states three things in the order an operator needs them: that the file was
/// refused, what the daemon is therefore doing (Teams OFF — the consequence, and
/// the half that explains an idle board), and the daemon's own reason quoted
/// VERBATIM. Verbatim matters: a second wording here would be a second place for
/// the rule to be explained, free to drift from the one that actually decides
/// whether the file loads.
fn render_rejection_for(path: &Path, reason: &str) -> String {
    if reason.is_empty() {
        return String::new();
    }
    format!(
        "!!! {} was REJECTED; Teams is OFF for this daemon !!!\n    {reason}\n\n",
        path.display()
    )
}

/// `show`'s arguments: one positional name plus the optional `--room N`.
/// Written as a loop, like [`fork`]'s, so flag order never matters.
fn parse_show_args(args: &[String]) -> Result<(String, usize), String> {
    let mut name: Option<String> = None;
    let mut tail = DEFAULT_ROOM_TAIL;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--room" => {
                let v = rest.next().ok_or(SHOW_USAGE)?;
                tail = v
                    .parse()
                    .map_err(|_| format!("--room takes a message count, got {v:?}"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            other if name.is_none() && !other.is_empty() => name = Some(other.to_string()),
            "" => return Err(SHOW_USAGE.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok((name.ok_or(SHOW_USAGE)?, tail))
}

/// The **Room** section: the newest `limit` room-wide posts, oldest first, one
/// line each (STUDIO-670).
///
/// This is the same peek `teams_room_read` performs and nothing new: an empty
/// reader, [`Cursor::default`] and the room's own clamp. Two properties follow
/// from that empty reader, and both are load-bearing rather than incidental:
///
/// * **No cursor is advanced.** A glance from a terminal must never eat a
///   teammate's catch-up, so this reads from the beginning of the window every
///   time and never touches `Cursors`.
/// * **Direct messages are not shown.** `Audience::visible_to("")` is false for
///   every `to:` a named teammate, so a `to: alice` hand-off never renders here.
///   That is deliberate: the CLI is the operator's glance at the room, not a way
///   to read somebody else's mail.
///
/// A room that was never written renders nothing at all — and creating it to
/// find that out is exactly what [`LocalRoom`] refuses to do.
fn render_room(room_dir: &Path, limit: usize) -> String {
    let got = match LocalRoom::new(room_dir).read_since("", &Cursor::default(), limit) {
        Ok(got) => got,
        // A room the CLI cannot read must not cost the operator the profile
        // report they actually asked for: name the reason in one line, print
        // the rest.
        Err(e) => return format!("\n--- room ---\n({e})\n"),
    };
    if got.messages.is_empty() && got.skipped.is_empty() {
        return String::new();
    }
    // "(last 0)" would read as a claim about the room rather than about what
    // could be parsed out of it, so a section that carries only skips is bare.
    let mut out = if got.messages.is_empty() {
        "\n--- room ---\n".to_string()
    } else {
        format!("\n--- room (last {}) ---\n", got.messages.len())
    };
    for m in &got.messages {
        out.push_str(&room_line(m));
    }
    // "Skipped loudly, never fatal" (§0.11.4): a corrupt line costs its own line
    // and nothing else, but the operator is told it happened.
    if !got.skipped.is_empty() {
        let n = got.skipped.len();
        out.push_str(&format!(
            "({n} unreadable line{} skipped)\n",
            if n == 1 { "" } else { "s" }
        ));
    }
    out
}

/// One message as `<at>  <from>  <body-first-line>`, bounded by
/// [`ROOM_LINE_WIDTH`] chars. Only the first line of the body is printed: the
/// tail is a glance, and a message's own first line is what its author wrote as
/// its headline.
fn room_line(m: &Message) -> String {
    let head = format!(
        "{}  {}  ",
        m.at.to_rfc3339_opts(SecondsFormat::Secs, true),
        m.from
    );
    let body = m.body.lines().next().unwrap_or_default().trim();
    let budget = ROOM_LINE_WIDTH.saturating_sub(head.chars().count());
    // `trim_end`: an empty body would otherwise leave the separator's two spaces
    // dangling at the end of the line.
    format!(
        "{}\n",
        format!("{head}{}", truncate_chars(body, budget)).trim_end()
    )
}

/// `s` cut to at most `max` CHARS, the cut marked with a trailing `…` that
/// itself counts against the budget — so the caller's width bound holds exactly.
/// Char-indexed rather than byte-sliced, because a room body is arbitrary UTF-8.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let end = s
        .char_indices()
        .nth(max - 1)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…", &s[..end])
}

/// The `teams show` report: provenance first, then the room, then the resolved
/// prompt, so the two questions §4 poses — "which base is this" and "what text
/// does it produce" — are both answered by one screen.
///
/// `room` sits BETWEEN them rather than at the very end (STUDIO-670): the
/// resolved prompt is unbounded prose, and a glance an operator has to scroll a
/// screenful of it to reach is not a glance. It is empty whenever Teams is off
/// or `--room 0` was passed, and then this renders exactly what it always did.
fn render_show(
    identity: Option<&str>,
    r: &ResolvedProfile,
    review: Option<&Review>,
    room: &str,
    backend: &str,
    rejection: &str,
    effective: &str,
) -> String {
    let mut out = String::new();
    // First, so it is the line an operator reads before anything else. Empty on
    // every accepted config, which keeps an ordinary `show` byte-identical to
    // what it printed before this existed (STUDIO-670's property).
    out.push_str(rejection);
    if let Some(i) = identity {
        out.push_str(&format!("identity:      {i}\n"));
    }
    out.push_str(&format!("profile:       {}\n", r.name));
    match &r.provenance.base {
        Some(b) => out.push_str(&format!(
            "base:          {}@{} ({})\n",
            b.name,
            b.version,
            if b.pinned {
                "pinned"
            } else {
                "tracking latest"
            }
        )),
        None => out.push_str("base:          none (fork — this file is the whole profile)\n"),
    }
    match &r.provenance.overlay {
        Some(p) => out.push_str(&format!("overlay:       {}\n", p.display())),
        None => out.push_str("overlay:       none (the built-in, unmodified)\n"),
    }
    if let Some(d) = &r.provenance.drift {
        out.push_str(&format!(
            "drift:         pinned to {}@{}; the built-in is now {}@{} (reported, never merged)\n",
            d.name, d.pinned, d.name, d.latest
        ));
    }
    // ABOVE `model`, decided by the ticket: this is the line that selects the binary, so it is the
    // most load-bearing of the resolved fields — an operator reads it before the model question,
    // which is scoped inside whichever CLI wins (STUDIO-903).
    out.push_str(&format!(
        "harness:       {}\n",
        harness_field(&r.harness, r.provenance.harness, backend)
    ));
    // The provider this profile names, with the same base/overlay origin every sibling field
    // carries (STUDIO-993, P12). It sits between `harness` and `model` because it is the middle
    // field of the tuple: empty means "inherit the daemon's config" exactly as they do. What a run
    // actually resolves to across ALL tiers is the `--- effective selection ---` block below.
    out.push_str(&format!(
        "provider:      {}\n",
        field(&r.provider, r.provenance.provider)
    ));
    out.push_str(&format!(
        "model:         {}\n",
        field(&r.model, r.provenance.model)
    ));
    out.push_str(&format!(
        "effort:        {}\n",
        field(&r.effort, r.provenance.effort)
    ));
    // What an operator actually needs answered (STUDIO-901, ticket §4): "what model REVIEWS this
    // identity's pull requests" is a different question from "what model does this identity run
    // with", because `review.model`/`review.effort` (when set) win over the profile above for a
    // REVIEW run specifically — and are otherwise invisible, since they are not part of this
    // identity's own profile at all.
    //
    // `None` (Teams off, or `review.mode` anything but `ticketless`) suppresses BOTH lines rather
    // than printing them with a claim that cannot come true (jimmy/alice round 1 on PR #168):
    // `dispatch_issue` only ever applies `review.model`/`review.effort` to a run
    // `dispatch_review` staged, and only the ticketless path stages one. `show` on any other
    // install prints exactly the lines it printed before this ticket — not a column-for-column
    // match, since the round-1 alignment fix widened every label's gutter by one (jimmy round 2).
    if let Some(review) = review {
        // The harness a review of THIS identity actually runs on (STUDIO-908): `review.model` is
        // scoped by harness, so the useful answer is this identity's own entry, not the raw map.
        // `backend` is also the fallback the legacy bare-scalar spelling resolves against.
        let review_harness = resolved_harness(&r.harness, backend);
        out.push_str(&format!(
            "review provider: {}\n",
            review_field(
                "provider",
                &review.provider,
                &review_harness,
                backend,
                &r.provider,
                r.provenance.provider
            )
        ));
        out.push_str(&format!(
            "review model:  {}\n",
            review_field(
                "model",
                &review.model,
                &review_harness,
                backend,
                &r.model,
                r.provenance.model
            )
        ));
        out.push_str(&format!(
            "review effort: {}\n",
            review_field(
                "effort",
                &review.effort,
                &review_harness,
                backend,
                &r.effort,
                r.provenance.effort
            )
        ));
    }
    out.push_str(&format!(
        "capabilities:  {}\n",
        list_field(&r.capabilities, r.provenance.capabilities)
    ));
    out.push_str(&format!(
        "tools:         {} (parsed, unused in this slice)\n",
        list_field(&r.tools, r.provenance.tools)
    ));
    // A fork has no base, so a `{{ base }}` token in one splices nothing. Say
    // that, rather than claiming a splice the `base: none` line contradicts.
    let has_base = r.provenance.base.is_some();
    out.push_str(&format!(
        "body:          {}\n",
        match r.provenance.body {
            BodyOrigin::Base => "from the base (the overlay body is empty)",
            BodyOrigin::Overlay => "from the overlay (replaces the base wholesale)",
            BodyOrigin::Spliced if has_base =>
                "the overlay, with the base spliced in at {{ base }}",
            BodyOrigin::Spliced =>
                "from the overlay; its {{ base }} spliced nothing, because a fork has no base",
        }
    ));
    out.push_str(effective);
    out.push_str(room);
    out.push_str("\n--- resolved prompt ---\n");
    out.push_str(&r.prompt);
    out.push('\n');
    out
}

/// The effective Teams/manager tuples an operator cannot otherwise see (STUDIO-993, P12): what each
/// of [`harness`](ResolvedProfile::harness), `provider` and `model` resolves to across the
/// field-wise precedence chain `ticket > review > profile > identity > project > global`, with the
/// ORIGIN of each, plus the manager's own independent tuple.
///
/// This consumes the SAME pure resolver ([`resolve_selection`]/[`resolve_manager_selection`]) the
/// dispatch path uses, so `show` cannot disagree with what a run would select — the ticket's
/// explicit rule ("do not recompute a second selection algorithm in UI/CLI"). Every input it needs
/// is non-secret config already on this command's read-only path; the resolver performs no I/O and
/// returns no credential.
///
/// ⚠️ The tiers fed here are the tiers the DISPATCH path actually builds
/// (`Orchestrator::selection_inputs`): the CLI has no ticket and no target project, and dispatch
/// does NOT yet feed the identity or review tier — a roster entry's own `harness:`/`provider:`/
/// `model:` and `review.provider` are parsed and displayed but do not reach a run (PR #273 round 1).
/// Feeding them here would print a tuple no dispatch would ever produce and state an override as
/// fact; they are reported separately, labeled configured-only, below.
///
/// A typed refusal is rendered IN PLACE of the resolved fields and the profile report is still
/// printed above it: an invalid provider refuses the RUN, never degrades the whole Teams feature to
/// disabled — which is exactly the distinction P12 exists to make visible.
fn render_effective(
    cfg: &Config,
    teams: &Teams,
    identity: Option<&Identity>,
    profile: &ResolvedProfile,
) -> String {
    let providers = &cfg.providers;
    let deadline_ms = provider_turn_deadline_ms(cfg.opencode.turn_timeout_ms);
    let tiers = SelectionTiers {
        // The CLI has no ticket and no target project, and — like `selection_inputs` — it feeds
        // neither the identity nor the review tier, because dispatch does not. See the doc comment.
        ticket: FieldSelection::default(),
        review: None,
        profile: FieldSelection {
            harness: profile.harness.clone(),
            provider: profile.provider.clone(),
            model: profile.model.clone(),
        },
        identity: FieldSelection::default(),
        project: FieldSelection::default(),
        global: FieldSelection::from_agent(&cfg.agent),
    };
    let mut out = String::new();
    out.push_str(
        "\n--- effective selection (the tuple a dispatched run resolves; field-wise: ticket > review > profile > identity > project > global, but this command has no ticket or project in scope and dispatch feeds neither the review nor the identity tier) ---\n",
    );
    match resolve_selection(&SelectionRequest {
        tiers,
        providers,
        turn_deadline_ms: deadline_ms,
    }) {
        Ok(sel) => {
            out.push_str(&format!(
                "harness:   {} [{}]\n",
                sel.harness_name,
                sel.origins.harness.as_str()
            ));
            out.push_str(&format!(
                "provider:  {}\n",
                resolved_provider_line(sel.provider_id.as_str(), &sel.origins)
            ));
            out.push_str(&format!(
                "model:     {}\n",
                resolved_model_line(sel.model.as_deref(), &sel.origins)
            ));
        }
        Err(e) => out.push_str(&format!("REFUSED:   {e}\n")),
    }
    // A roster entry's own routing fields are parsed and shown, but dispatch does not feed the
    // identity tier yet, so they must not be folded into the tuple above. Reported separately and
    // labeled, rather than silently dropped or falsely claimed as applied (PR #273 round 1).
    if let Some(i) = identity {
        let configured = [
            ("harness:", i.harness.as_str()),
            ("provider:", i.provider.as_str()),
            ("model:", i.model.as_str()),
            ("effort:", i.effort.as_str()),
        ];
        if configured.iter().any(|(_, value)| !value.is_empty()) {
            out.push_str(
                "\n--- identity routing fields (configured on the roster entry, NOT yet applied at dispatch — a run still resolves from the profile tier and below) ---\n",
            );
            for (name, value) in configured {
                if !value.is_empty() {
                    out.push_str(&format!("{name:<10}{value}\n"));
                }
            }
        }
    }
    // The manager's own tuple, which never borrows a teammate's (design §5 / parent D6) — the
    // independent half an operator has no other command to ask about.
    out.push_str("\n--- manager (independent of every teammate) ---\n");
    let manager = FieldSelection {
        harness: teams.manager.harness.clone(),
        provider: teams.manager.provider.clone(),
        model: teams.manager.model.clone(),
    };
    match resolve_manager_selection(&manager, providers, deadline_ms) {
        Ok(sel) => {
            out.push_str(&format!(
                "harness:   {} [{}]\n",
                sel.harness_name,
                sel.origins.harness.as_str()
            ));
            out.push_str(&format!(
                "provider:  {}\n",
                resolved_provider_line(sel.provider_id.as_str(), &sel.origins)
            ));
            out.push_str(&format!(
                "model:     {}\n",
                resolved_model_line(sel.model.as_deref(), &sel.origins)
            ));
        }
        Err(e) => out.push_str(&format!("REFUSED:   {e}\n")),
    }
    out
}

/// The `provider:` line of an effective/manager tuple: the stable canonical id with its origin, or
/// the explicit "no Rhapsody provider" branch — the legacy native-login path, which is a real
/// answer rather than a blank.
fn resolved_provider_line(provider_id: &str, origins: &SelectionOrigins) -> String {
    if provider_id.is_empty() {
        return "(none — native login; no Rhapsody provider selected)".to_string();
    }
    match origins.provider {
        Some(o) => format!("{provider_id} [{}]", o.as_str()),
        None => provider_id.to_string(),
    }
}

/// The `model:` line of an effective/manager tuple. An absent model is NOT unset for the manager:
/// it preserves the harness CLI's own default, and saying so is the difference between a reported
/// resolution and a blank.
fn resolved_model_line(model: Option<&str>, origins: &SelectionOrigins) -> String {
    match (model, origins.model) {
        (Some(m), Some(o)) => format!("{m} [{}]", o.as_str()),
        (Some(m), None) => m.to_string(),
        (None, _) => "(the harness CLI's own default)".to_string(),
    }
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

/// The `harness:` line (STUDIO-903): the CLI this teammate's runs actually use, rendered like
/// every sibling field with its origin.
///
/// ⚠️ The RESOLVED value, never the raw front-matter field. An empty `harness` is the common case
/// and means "inherit `agent.backend`", so the useful fact is that backend's own value — `backend`
/// is the resolved `agent.backend` from the same workflow the daemon would boot, and naming it is
/// what turns the line from a restatement into an answer.
///
/// A harness this build cannot run is MARKED, because the dispatcher REFUSES a profile that names
/// one (`spawn_worker`, STUDIO-978; it used to silently fall back to `agent.backend`): plain
/// `harness: codex [overlay]` would claim a CLI that never runs, which is the misleading report
/// decision 2 of this ticket exists to avoid. The mark is only ever appended — `<value> [origin]`
/// stays byte-identical for the implemented harnesses that are the overwhelmingly common case, so a
/// mark means something.
///
/// The empty-`harness` branch is deliberately UNMARKED, including when `agent.backend` itself
/// names a harness this build cannot run: `runner_for_backend` rejecting the backend makes
/// `build_effective` fail and the daemon refuses to boot, and `validate` rejects an unknown name
/// outright, so both are loud and a second report would only be noisier.
fn harness_field(profile_value: &str, origin: Origin, backend: &str) -> String {
    if profile_value.is_empty() {
        return format!("{backend} [unset — inherits agent.backend]");
    }
    match harness_note(profile_value) {
        Some(note) => format!("{profile_value} {} ({note})", origin_tag(origin)),
        None => format!("{profile_value} {}", origin_tag(origin)),
    }
}

/// The parenthetical [`harness_field`] appends when the resolved harness is not one this build can
/// run, or `None` when it is.
///
/// Recognized-but-unimplemented (`codex`) and a name no registry knows are told apart: the first
/// is a build limitation, the second a typo, and the operator's next move differs. Neither says it
/// "runs on" anything any more (STUDIO-978): `spawn_worker` REFUSES such a dispatch rather than
/// falling back to `agent.backend`, so the note names the refusal.
fn harness_note(harness: &str) -> Option<String> {
    if rhapsody_orchestrator::effective::harness_is_implemented(harness) {
        return None;
    }
    if rhapsody_config::HARNESS_NAMES.contains(&harness) {
        Some(
            "recognized harness, but this build has no runner for it; a dispatch is refused"
                .to_string(),
        )
    } else {
        Some("not a recognized harness; a dispatch is refused".to_string())
    }
}

/// The harness an identity's runs actually use, for [`harness_field`]'s reason and by the same
/// rule: the profile's resolved `harness` when it names one, else the configured `agent.backend`
/// (STUDIO-978 — a named harness this build cannot run is kept verbatim because the dispatch is
/// REFUSED, not run on the backend; see `Orchestrator::effective_harness`). Shared with the
/// review-scoped lines so `show` explains the review model against the harness the run resolves to
/// (STUDIO-908).
fn resolved_harness(profile_value: &str, backend: &str) -> String {
    if profile_value.is_empty() {
        backend.to_string()
    } else {
        profile_value.to_string()
    }
}

/// The `review model:`/`review effort:` line (STUDIO-901, scoped by harness in STUDIO-908):
/// `teams.review.<name>.<harness>` when the operator set an entry for the harness this identity's
/// runs actually use — which WINS over this identity's own profile for a review run and says so —
/// else the profile's own value (rendered with its normal [`field`] provenance), since an unset
/// entry means a review run inherits exactly what this identity's profile already gives an
/// ordinary dispatch.
///
/// `fallback` is the configured `agent.backend`: the harness the legacy bare-scalar spelling
/// belongs to (STUDIO-908), so a bare `review.model` reads as applying here exactly when this
/// identity's harness IS the backend. The origin label keeps the spelling the operator wrote — a
/// bare scalar renders as `review.model`, not `review.model.<harness>`, because naming a harness
/// they never wrote is the misattribution this ticket removes.
///
/// A value set for a DIFFERENT harness is called out rather than hidden: for `model` it is a
/// refusal (the review will not run at all), and for `effort` it simply means this harness
/// inherits. Either way the operator sees which harnesses are named, since the raw map is the one
/// thing this line exists to make legible.
///
/// Only called when `render_show`'s `review` argument is `Some` — the caller (`show`) gates that
/// on [`Teams::review_ticketless`](rhapsody_config::teams::Teams::review_ticketless), so this
/// function itself never has to ask "can this override even fire": by the time it runs, it can.
fn review_field(
    name: &str,
    scoped: &rhapsody_config::teams::HarnessScoped,
    harness: &str,
    fallback: &str,
    profile_value: &str,
    profile_origin: Origin,
) -> String {
    if scoped.is_empty() {
        return format!(
            "(unset — a review run uses this profile's {name}, {})",
            field(profile_value, profile_origin)
        );
    }
    if let Some(value) = scoped.for_harness(harness, fallback) {
        let key = if scoped.legacy().is_some() {
            format!("review.{name}")
        } else {
            format!("review.{name}.{harness}")
        };
        if name == "provider" {
            // `review.provider` is parsed, validated and harness-scoped, but dispatch does not yet
            // feed the review tier (`selection_inputs` passes `review: None`), so a review run
            // still takes its provider from the profile and global tiers. Report the configured
            // value and say so, rather than claiming an override that never happens (PR #273
            // round 1).
            return format!(
                "{value} [{key} — configured, but not yet applied at dispatch: a review run still \
                 uses this profile's provider]"
            );
        }
        return format!("{value} [{key} — overrides this profile's {name} for a review run]");
    }
    let listed = scoped
        .resolved(fallback)
        .iter()
        .map(|(h, v)| format!("{h}: {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    if name == "provider" {
        // Nothing applies `review.provider` yet, so a value scoped to another harness is not the
        // wrong-run refusal a `review.model` mismatch is: say exactly that instead of describing a
        // refusal dispatch never performs.
        format!(
            "(unset for harness {harness} — review.provider names {listed}; it is not applied at \
             dispatch yet, so no review is refused and a review on {harness} uses this profile's \
             provider)"
        )
    } else if name == "model" {
        // A model configured for another harness IS a refusal: handing a review a model its harness
        // cannot honour would run it on the wrong model, which is the trap this closes.
        format!(
            "(unset for harness {harness} — review.model names {listed}, so a review on {harness} \
             is refused rather than run on the wrong model)"
        )
    } else {
        format!(
            "(unset for harness {harness} — review.effort names {listed}; a review on {harness} \
             inherits this profile's effort)"
        )
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
    use chrono::DateTime;

    /// The newest shipped version of a built-in profile. Used instead of a
    /// hardcoded `@1` so a designed built-in bump (T4 shipped v2) moves these
    /// assertions with the registry rather than breaking them — the version
    /// these tests care about is "the latest", not a particular number.
    fn newest_builtin(name: &str) -> u32 {
        rhapsody_config::profiles::builtin_profiles()
            .iter()
            .filter(|b| b.name == name)
            .map(|b| b.version)
            .max()
            .unwrap_or_else(|| panic!("no built-in profile named {name:?}"))
    }

    /// Points the verbs at a hermetic store home by writing a WORKFLOW.md whose
    /// `storage.path` sits under `dir`, and returns the resolved profiles dir.
    fn hermetic(dir: &TempDir) -> (Vec<String>, PathBuf) {
        hermetic_backend(dir, "")
    }

    /// [`hermetic`] with an explicit `agent.backend`, so the harness line's inherit branch can be
    /// exercised against a resolved backend that is not the shipped default (STUDIO-903).
    fn hermetic_backend(dir: &TempDir, backend: &str) -> (Vec<String>, PathBuf) {
        let agent = if backend.is_empty() {
            String::new()
        } else {
            format!("agent:\n  backend: {backend}\n")
        };
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            format!(
                "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\n{agent}storage:\n  path: {}/rhapsody.db\n---\nDo {{{{ issue.identifier }}}}.\n",
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
        assert!(out.contains("profile:       swe"), "out = {out}");
        assert!(
            out.contains(&format!(
                "base:          swe@{} (tracking latest)",
                newest_builtin("swe")
            )),
            "out = {out}"
        );
        assert!(out.contains("overlay:       none"), "out = {out}");
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
        assert!(out.contains("identity:      alice"), "out = {out}");
        assert!(out.contains("profile:       reviewer"), "out = {out}");
        assert!(
            out.contains("You are a code reviewer on this codebase."),
            "out = {out}"
        );
        assert!(!profiles_dir.exists(), "show must not create the dir");
    }

    /// STUDIO-1013: `teams show manager` resolves the built-in manager identity and renders the
    /// maintainer's `manager-rules.md` into its prompt as policy. No rules file means the built-in
    /// profile alone — the optional feature's off state.
    #[test]
    fn show_manager_renders_the_maintainer_rules_as_policy() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        let teams_dir = dir.child("teams");
        std::fs::create_dir_all(&teams_dir).expect("teams dir");
        std::fs::write(
            teams_dir.join("manager-rules.md"),
            "Escalate to the maintainer after three rounds.\n",
        )
        .expect("write rules");

        let out = run(&["show", "manager"], &env[0]).expect("show manager");
        assert!(
            out.contains(rhapsody_config::manager::POLICY_HEADING),
            "the policy heading must be rendered: {out}"
        );
        assert!(
            out.contains("Escalate to the maintainer after three rounds."),
            "the rule text must reach the resolved prompt: {out}"
        );
        assert!(
            out.contains("You are the manager."),
            "the built-in manager profile must be the base: {out}"
        );
    }

    // ── review model/effort visibility (STUDIO-901, ticket §4) ──────────────

    /// The whole §4 bar for this ticket: an operator asking "what model will actually review
    /// this?" gets a direct answer, in the same one command that already answers "what model does
    /// this identity run with?" — and when `review.model`/`review.effort` are set, the line says
    /// they WIN over the profile above, so the two lines are never mistaken for each other.
    ///
    /// This file's spelling is the legacy bare scalar, so the label is `review.model`, not
    /// `review.model.claude`: the operator never wrote a harness, and STUDIO-908 resolves the bare
    /// value against `agent.backend` (claude here).
    #[test]
    fn show_reports_the_review_scoped_model_when_set() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  model: claude-opus-5\n  effort: high\nroster:\n  - name: alice\n    profile: reviewer\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review model:  claude-opus-5 [review.model — overrides this profile's model for a review run]"),
            "out = {out}"
        );
        assert!(
            out.contains("review effort: high [review.effort — overrides this profile's effort for a review run]"),
            "out = {out}"
        );
    }

    /// **alice's blocking finding on PR #172, seen from `show`.** A legacy bare `review.model` is
    /// not pinned to `claude`: on an installation whose `agent.backend` is `opencode` it applies
    /// to that harness, and the line says so rather than claiming a refusal that will not happen.
    #[test]
    fn show_resolves_a_legacy_bare_review_model_against_the_configured_backend() {
        let dir = TempDir::new();
        let (env, _) = hermetic_backend(&dir, "opencode");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  model: some-opencode-model\nroster:\n  - name: alice\n    profile: reviewer\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review model:  some-opencode-model [review.model — overrides this profile's model for a review run]"),
            "out = {out}"
        );
    }

    /// **STUDIO-908, the diagnostic the live breakage lacked.** A `review.model` scoped to a
    /// harness the identity does NOT run on is reported as such — named back to the operator,
    /// rather than printed as though it applied (which is how the original bug read) or hidden
    /// (which would leave them wondering). The line names the harness, the configured model and
    /// its harness, and the consequence: a review is refused, not run on the wrong model.
    #[test]
    fn show_reports_a_review_model_scoped_to_another_harness_as_refused() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        // A per-harness map with an entry for `opencode` only; the reviewer below runs claude.
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    opencode: fireworks-ai/x\nroster:\n  - name: alice\n    profile: reviewer\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review model:  (unset for harness claude — review.model names opencode: fireworks-ai/x, so a review on claude is refused rather than run on the wrong model)"),
            "out = {out}"
        );
    }

    /// Absent `review.model`/`review.effort` on the ticketless path — the default that path
    /// itself would ship with — says plainly that a review run inherits this identity's own
    /// profile, rather than printing nothing and leaving the question unanswered.
    #[test]
    fn show_reports_review_scoped_model_as_inherited_when_unset() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review model:  (unset — a review run uses this profile's model, [unset — inherits the daemon's config])"),
            "out = {out}"
        );
        assert!(
            out.contains("review effort: (unset — a review run uses this profile's effort, [unset — inherits the daemon's config])"),
            "out = {out}"
        );
    }

    /// **jimmy/alice round-1 finding 2 on PR #168, mutation-checked.** `review.model`/
    /// `review.effort` are dead config on any installation whose `review.mode` is not
    /// `ticketless` — including the SHIPPED default, `mode: off`, and Teams disabled entirely (no
    /// `teams.yaml` at all). `show` must not claim an override that install can never honour, so a
    /// Teams-off install's report suppresses both review-scoped lines and otherwise prints exactly
    /// the lines it printed before this ticket. Gating `render_show`'s `review` argument on
    /// anything other than `teams.review_ticketless()` turns this red.
    #[test]
    fn show_suppresses_the_review_scoped_lines_off_the_ticketless_path() {
        // Teams enabled, but on `mode: tickets` — the review override is set and inert.
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: tickets\n  model: claude-opus-5\n  effort: high\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(!out.contains("review model:"), "out = {out}");
        assert!(!out.contains("review effort:"), "out = {out}");

        // No teams.yaml at all: the review-scoped lines stay suppressed, exactly as before this
        // ticket added them.
        let dir2 = TempDir::new();
        let (env2, _) = hermetic(&dir2);
        let out = run(&["show", "swe"], &env2[0]).expect("show swe");
        assert!(!out.contains("review model:"), "out = {out}");
        assert!(!out.contains("review effort:"), "out = {out}");
    }

    // ── the resolved harness (STUDIO-903) ───────────────────────────────────

    /// The ticket's headline: a profile that sets `harness:` reports the CLI this teammate's runs
    /// actually use, with its origin — rendered exactly like every sibling field. Before this the
    /// one field that selects the binary was the only one absent, so the operator had to read the
    /// nested `config.opencode` block to guess (which proves the block parsed, never that this
    /// teammate resolved to it).
    #[test]
    fn show_reports_the_resolved_harness_with_its_origin() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join("swe.md"),
            "---\nextends: swe\nharness: opencode\n---\n{{ base }}\n",
        )
        .expect("write overlay");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            out.contains("harness:       opencode [overlay]"),
            "out = {out}"
        );
    }

    /// ⚠️ The trap the ticket names: printing the raw front matter would leave an inheriting
    /// teammate with `[unset]` and no answer — the empty string IS the common case. The resolved
    /// value is `agent.backend`'s, and the marker says so.
    #[test]
    fn show_reports_the_configured_backend_for_an_inheriting_teammate() {
        let dir = TempDir::new();
        let (env, _) = hermetic_backend(&dir, "opencode");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            out.contains("harness:       opencode [unset — inherits agent.backend]"),
            "out = {out}"
        );
    }

    /// The shipped default, with no `agent.backend` written anywhere: the inherit marker names
    /// `claude`, the value the daemon actually resolves.
    #[test]
    fn show_reports_the_default_backend_for_an_inheriting_teammate() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            out.contains("harness:       claude [unset — inherits agent.backend]"),
            "out = {out}"
        );
    }

    /// **Decision 2, mutation-checked by the assertion below.** The registry recognizes names this
    /// build cannot run (`codex`), and `spawn_worker` REFUSES a profile naming one (STUDIO-978) —
    /// so plain `harness: codex [overlay]` would claim a CLI that never runs. It is marked, and the
    /// mark names the refusal.
    #[test]
    fn show_marks_a_recognized_harness_this_build_cannot_run() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join("swe.md"),
            "---\nextends: swe\nharness: codex\n---\n{{ base }}\n",
        )
        .expect("write overlay");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        let line = out
            .lines()
            .find(|l| l.starts_with("harness:"))
            .unwrap_or_else(|| panic!("no harness line in {out}"));
        assert_eq!(
            line,
            "harness:       codex [overlay] (recognized harness, but this build has no runner for it; a dispatch is refused)",
            "out = {out}"
        );
    }

    /// A name no registry knows — the mistyped `harness:` the ticket opens with — is marked
    /// differently from a recognized-but-unimplemented one, because the operator's fix differs,
    /// and it too names the refusal.
    #[test]
    fn show_marks_a_harness_no_registry_knows() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join("swe.md"),
            "---\nextends: swe\nharness: openai\n---\n{{ base }}\n",
        )
        .expect("write overlay");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        let line = out
            .lines()
            .find(|l| l.starts_with("harness:"))
            .unwrap_or_else(|| panic!("no harness line in {out}"));
        assert_eq!(
            line,
            "harness:       openai [overlay] (not a recognized harness; a dispatch is refused)",
            "out = {out}"
        );
    }

    /// A harness the build CAN run carries no mark — the common case stays a clean
    /// `<value> [origin]` line, so the mark means something when it appears.
    #[test]
    fn show_prints_an_implemented_harness_with_no_mark() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic(&dir);
        std::fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join("swe.md"),
            "---\nextends: swe\nharness: opencode\n---\n{{ base }}\n",
        )
        .expect("write overlay");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        let line = out
            .lines()
            .find(|l| l.starts_with("harness:"))
            .unwrap_or_else(|| panic!("no harness line in {out}"));
        assert_eq!(line, "harness:       opencode [overlay]", "out = {out}");
    }

    // ── the review-scoped PROVIDER (STUDIO-993, P12) ──

    /// `review.provider` is the review-tier sibling of `review.model`, and it is displayed the same
    /// way: scoped to the harness the reviewer actually runs on, and naming that scope back to the
    /// operator.
    #[test]
    fn show_reports_the_review_scoped_provider_when_set() {
        let dir = TempDir::new();
        let (env, _) = hermetic_backend(&dir, "opencode");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: fireworks\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review provider: fireworks [review.provider.opencode — configured, but not yet applied at dispatch: a review run still uses this profile's provider]"),
            "out = {out}"
        );
    }

    /// **The harness-scope mutation guard.** A `review.provider` configured for a harness the
    /// reviewer does not run on is reported with the OTHER harness named — never silently applied
    /// to the wrong run and never silently dropped. A display that read `review.provider` unscoped
    /// would print `fireworks [review.provider — …]` here instead, turning this red.
    #[test]
    fn show_reports_a_review_provider_scoped_to_another_harness() {
        let dir = TempDir::new();
        let (env, _) = hermetic_backend(&dir, "claude");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: fireworks\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("review provider: (unset for harness claude — review.provider names opencode: fireworks; it is not applied at dispatch yet, so no review is refused and a review on claude uses this profile's provider)"),
            "out = {out}"
        );
    }

    // ── the effective Teams/manager tuple and its origins (STUDIO-993, P12) ──
    /// The two providers every effective-selection test below resolves against. A canonical id, an
    /// `openai-compatible` protocol and a Keychain credential source — never a value.
    const PROVIDERS: &str = "  fireworks:\n    protocol: openai-compatible\n    base_url: https://api.fireworks.ai/inference/v1\n    credential:\n      source: keychain\n  openrouter:\n    protocol: openai-compatible\n    base_url: https://openrouter.ai/api/v1\n    credential:\n      source: keychain\n";

    /// [`hermetic`] with an `agent.backend` and a `providers:` block, so the effective-selection
    /// resolver has a real registry to select against.
    fn hermetic_providers(dir: &TempDir, backend: &str) -> (Vec<String>, PathBuf) {
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            format!(
                "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\nagent:\n  backend: {backend}\nproviders:\n{PROVIDERS}storage:\n  path: {}/rhapsody.db\n---\nDo {{{{ issue.identifier }}}}.\n",
                dir.path.display()
            ),
        )
        .expect("write WORKFLOW.md");
        let env = vec![wf.to_string_lossy().into_owned()];
        (env, dir.path.join("teams").join("profiles"))
    }

    /// Write a profile overlay with the given routing front matter, so the profile tier can carry
    /// each field independently.
    fn write_profile(profiles_dir: &Path, name: &str, front_matter: &str) {
        std::fs::create_dir_all(profiles_dir).expect("create profiles dir");
        std::fs::write(
            profiles_dir.join(format!("{name}.md")),
            format!("---\nextends: {name}\n{front_matter}---\n{{{{ base }}}}\n"),
        )
        .expect("write overlay");
    }

    /// The `provider:` line P12 exists to add, and the fresh `--- effective selection ---` block
    /// that reports the SAME resolved values the dispatch path computes — from the profile tier
    /// here, with that tier's origin. Before this ticket the provider was the one resolved field
    /// `show` never printed at all.
    #[test]
    fn show_reports_the_profile_provider_and_the_effective_origin() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic_providers(&dir, "opencode");
        write_profile(
            &profiles_dir,
            "swe",
            "harness: opencode\nprovider: fireworks\nmodel: accounts/fireworks/models/deepseek-v4p1-flash\n",
        );
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("provider:      fireworks [overlay]"),
            "out = {out}"
        );
        assert!(
            out.contains(
                "--- effective selection (the tuple a dispatched run resolves; field-wise: ticket > review > profile > identity > project > global, but this command has no ticket or project in scope and dispatch feeds neither the review nor the identity tier) ---"
            ),
            "out = {out}"
        );
        assert!(
            out.contains("\nharness:   opencode [profile]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nprovider:  fireworks [profile]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nmodel:     accounts/fireworks/models/deepseek-v4p1-flash [profile]\n"),
            "out = {out}"
        );
    }

    /// **The identity-tier disclosure guard** (PR #273 round 1). A roster entry's own routing fields
    /// are NOT fed to dispatch — `Orchestrator::selection_inputs` passes `identity:
    /// FieldSelection::default()` — so `show` must not report them as the effective tuple. They are
    /// disclosed in their own labeled block instead. A CLI that folded the identity tier into the
    /// effective tuple (the pre-round-1 behaviour) would print `openrouter [identity]` above and red
    /// this.
    #[test]
    fn show_reports_identity_fields_as_configured_but_not_applied() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic_providers(&dir, "opencode");
        write_profile(&profiles_dir, "swe", "harness: opencode\n");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n    provider: openrouter\n    model: anthropic/claude-sonnet-4-6\n    effort: xhigh\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        // The effective tuple is what dispatch would resolve: `swe` names no provider, so the global
        // tier answers, and the identity's fields are nowhere in it.
        assert!(
            out.contains("\nprovider:  (none — native login; no Rhapsody provider selected)\n"),
            "out = {out}"
        );
        assert!(
            !out.contains("openrouter [identity]"),
            "the identity tier must not be reported as applied: {out}"
        );
        // ...and the configured fields are disclosed separately, labeled as not applied.
        assert!(
            out.contains("--- identity routing fields (configured on the roster entry, NOT yet applied at dispatch"),
            "out = {out}"
        );
        assert!(out.contains("\nprovider: openrouter\n"), "out = {out}");
        assert!(
            out.contains("\nmodel:    anthropic/claude-sonnet-4-6\n"),
            "out = {out}"
        );
        assert!(out.contains("\neffort:   xhigh\n"), "out = {out}");
        // The profile itself names no provider, so the top-level line stays the inherit marker.
        assert!(
            out.contains("provider:      [unset — inherits the daemon's config]"),
            "out = {out}"
        );
    }

    /// The lowest tier is still an origin: a profile that names neither harness nor provider
    /// inherits `agent.backend` and reports it as the GLOBAL tier, never a blank.
    #[test]
    fn show_reports_the_global_tier_for_an_inheriting_teammate() {
        let dir = TempDir::new();
        let (env, _) = hermetic_providers(&dir, "opencode");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains("\nharness:   opencode [global]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nprovider:  (none — native login; no Rhapsody provider selected)\n"),
            "out = {out}"
        );
    }

    /// **An invalid provider refuses the RUN, never disables Teams** (P12's headline distinction).
    /// The report still prints the profile and its resolved prompt; only the effective tuple is
    /// replaced by the typed refusal, so an operator sees exactly which provider is unconfigured
    /// without the whole command or the whole feature going dark.
    #[test]
    fn show_refuses_an_unconfigured_provider_without_disabling_teams() {
        let dir = TempDir::new();
        let (env, profiles_dir) = hermetic_providers(&dir, "opencode");
        write_profile(
            &profiles_dir,
            "swe",
            "harness: opencode\nprovider: bogus\nmodel: some-model\n",
        );
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "alice"], &env[0]).expect("show must still succeed");
        assert!(out.contains("REFUSED:   selection_refusal:"), "out = {out}");
        assert!(
            out.contains("provider \"bogus\" is not configured"),
            "the refusal names the provider: {out}"
        );
        assert!(
            out.contains("profile:       swe"),
            "the profile is still shown: {out}"
        );
        assert!(
            out.contains("--- resolved prompt ---"),
            "the resolved prompt is still shown: {out}"
        );
        assert!(
            !out.contains("Teams is OFF"),
            "one invalid provider must not disable Teams: {out}"
        );
    }

    /// The manager has its own tuple and its own origin, independent of every teammate (design §5,
    /// parent D6). An absent tuple is the Claude + CLI-default-model manager, reported as
    /// `[default]` rather than as an inherited teammate value.
    #[test]
    fn show_reports_the_default_manager_tuple_and_its_origin() {
        let dir = TempDir::new();
        let (env, _) = hermetic_providers(&dir, "opencode");
        std::fs::write(dir.child("teams.yaml"), "enabled: true\n").expect("write teams.yaml");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            out.contains("--- manager (independent of every teammate) ---"),
            "out = {out}"
        );
        assert!(
            out.contains("\nharness:   claude [default]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nprovider:  (none — native login; no Rhapsody provider selected)\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nmodel:     (the harness CLI's own default)\n"),
            "out = {out}"
        );
    }

    /// An explicit manager tuple reports `[manager]` for each field — the origin that distinguishes
    /// the manager's own choice from anything a teammate selected.
    #[test]
    fn show_reports_an_explicit_manager_tuple() {
        let dir = TempDir::new();
        let (env, _) = hermetic_providers(&dir, "opencode");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nmanager:\n  harness: opencode\n  provider: fireworks\n  model: accounts/fireworks/models/deepseek-v4p1-flash\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            out.contains("\nharness:   opencode [manager]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nprovider:  fireworks [manager]\n"),
            "out = {out}"
        );
        assert!(
            out.contains("\nmodel:     accounts/fireworks/models/deepseek-v4p1-flash [manager]\n"),
            "out = {out}"
        );
    }

    /// Teams off has no manager and no routing fields, so the two new sections are suppressed
    /// entirely — a Teams-off `show` stays the report it always was (aside from the `provider:`
    /// line, which is an unconditional profile field like `harness:`/`model:`).
    #[test]
    fn show_prints_no_effective_sections_when_teams_is_off() {
        let dir = TempDir::new();
        let (env, _) = hermetic_providers(&dir, "opencode");
        let out = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(!out.contains("effective selection"), "out = {out}");
        assert!(!out.contains("--- manager"), "out = {out}");
    }

    /// STUDIO-891: a REJECTED `teams.yaml` is reported by the command, not only
    /// by a log line the operator has to go looking for.
    ///
    /// The rejection path degrades to `Teams::disabled()` and still exits 0, so
    /// without this the evidence that a config was refused is an absence — the
    /// roster silently not resolving, and a board that quietly stops being
    /// reviewed. `teams show` is the surface that needs no daemon and no log
    /// access, so it is where the reason belongs.
    ///
    /// The command still SUCCEEDS: §4's "best-effort" contract is that a broken
    /// `teams.yaml` must not stop an operator inspecting a profile.
    #[test]
    fn show_reports_a_rejected_teams_config_without_refusing_to_run() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        // Unsatisfiable: two teammates cannot supply two non-author reviewers.
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nreview:\n  mode: ticketless\n  reviewers: 2\nroster:\n  - name: alice\n  - name: jimmy\n",
        )
        .expect("write teams.yaml");
        let out = run(&["show", "swe"], &env[0]).expect("a rejected config must not fail `show`");
        assert!(
            out.contains("teams.yaml was REJECTED"),
            "the rejection must be stated, not implied: {out}"
        );
        assert!(
            out.contains("review.reviewers is 2") && out.contains("at most 1"),
            "the daemon's own reason must be quoted verbatim: {out}"
        );
        assert!(
            out.contains("Teams is OFF"),
            "the consequence is the half an operator acts on: {out}"
        );
        assert!(
            out.contains("--- resolved prompt ---"),
            "the profile is still shown: {out}"
        );

        // A config the daemon accepts prints no such banner — the report is the
        // exception, so an ordinary `show` is byte-identical to what it was.
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n",
        )
        .expect("rewrite teams.yaml");
        let ok = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(
            !ok.contains("REJECTED"),
            "no banner on a valid config: {ok}"
        );
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
        assert!(out.contains("model:         opus [overlay]"), "out = {out}");
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
        assert!(
            out.contains(&format!("forked sre from sre@{}", newest_builtin("sre"))),
            "out = {out}"
        );

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
            shown.contains("base:          none (fork"),
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
        assert!(
            out.contains(&format!("forked swe from swe@{}", newest_builtin("swe"))),
            "out = {out}"
        );
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

    /// With no on-disk store home to anchor to, the verbs REFUSE rather than
    /// guessing at a relative `./teams/profiles/` — a `fork` that created
    /// directories in whatever directory the operator was standing in would be
    /// exactly the surprise write §4's read-only posture exists to avoid.
    #[test]
    fn no_runtime_home_is_an_error_not_a_cwd_relative_guess() {
        let dir = TempDir::new();
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\nstorage:\n  path: \":memory:\"\n---\nDo it.\n",
        )
        .expect("write WORKFLOW.md");
        let env = wf.to_string_lossy().into_owned();
        for args in [vec!["show", "swe"], vec!["fork", "swe"]] {
            let err = run(&args, &env).expect_err("must refuse without a runtime home");
            assert!(err.contains("no Rhapsody runtime home"), "err = {err}");
        }
        assert!(
            !dir.path.join("teams").exists() && !Path::new("teams").exists(),
            "nothing may be created when there is no runtime home"
        );
    }

    // ── the room tail (STUDIO-670) ────────────────────────────────────────────

    /// The room root the hermetic workflow anchors to, and the banks root whose
    /// absence is what proves no cursor was written.
    fn room_and_banks(dir: &TempDir) -> (PathBuf, PathBuf) {
        (
            dir.path.join("teams").join("room"),
            dir.path.join("teams").join("banks"),
        )
    }

    /// Turns Teams on with `alice` on the roster, so the room section is reached
    /// at all (it is Teams-on only).
    fn teams_on(dir: &TempDir) {
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write teams.yaml");
    }

    /// Appends room-wide posts to the hermetic room, one minute apart from a
    /// fixed clock so the rendered `at` column is deterministic.
    fn post_room(dir: &TempDir, bodies: &[(&str, &str)]) {
        let room = LocalRoom::new(room_and_banks(dir).0);
        for (i, (from, body)) in bodies.iter().enumerate() {
            let at = DateTime::from_timestamp(1_756_000_000 + 60 * i as i64, 0)
                .expect("a valid fixed timestamp");
            room.append(&Message::room(*from, at, *body))
                .expect("append room post");
        }
    }

    /// The section body: everything between the room header and the resolved
    /// prompt, which is where the glance belongs.
    fn room_lines(out: &str) -> Vec<String> {
        let (_, after) = out
            .split_once("--- room (")
            .unwrap_or_else(|| panic!("no room section in {out}"));
        let (_, body) = after
            .split_once(") ---\n")
            .unwrap_or_else(|| panic!("malformed room header in {out}"));
        body.split("\n--- resolved prompt ---")
            .next()
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// The headline: `teams show` prints the roster report, then the bounded
    /// room tail oldest-first, and reading it advances NO cursor.
    #[test]
    fn show_prints_the_room_tail_and_advances_no_cursor() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        post_room(
            &dir,
            &[
                ("operator", "first post"),
                ("@manager", "assigned STUDIO-670 to alice"),
                ("alice", "took it"),
            ],
        );
        let (_, banks) = room_and_banks(&dir);

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(out.contains("--- room (last 3) ---"), "out = {out}");
        assert_eq!(
            room_lines(&out),
            vec![
                "2025-08-24T01:46:40Z  operator  first post".to_string(),
                "2025-08-24T01:47:40Z  @manager  assigned STUDIO-670 to alice".to_string(),
                "2025-08-24T01:48:40Z  alice  took it".to_string(),
            ],
            "oldest first, `<at>  <from>  <body>`: {out}"
        );
        // The section is a glance, so it sits ABOVE the resolved prompt rather
        // than behind a screenful of prose.
        let room_at = out.find("--- room (").expect("room section");
        let prompt_at = out.find("--- resolved prompt ---").expect("prompt section");
        assert!(room_at < prompt_at, "room must precede the prompt: {out}");
        // The peek must never eat a teammate's catch-up: no bank, and so no
        // cursor file, may be created by a read.
        assert!(
            !banks.exists(),
            "a peek must write no cursor: {} exists",
            banks.display()
        );
    }

    /// `--room N` narrows the tail; the default is [`DEFAULT_ROOM_TAIL`].
    #[test]
    fn room_flag_bounds_the_tail() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        let bodies: Vec<(&str, String)> = (0..14).map(|i| ("alice", format!("post {i}"))).collect();
        let bodies: Vec<(&str, &str)> = bodies.iter().map(|(f, b)| (*f, b.as_str())).collect();
        post_room(&dir, &bodies);

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(
            out.contains(&format!("--- room (last {DEFAULT_ROOM_TAIL}) ---")),
            "the default tail is {DEFAULT_ROOM_TAIL}: {out}"
        );
        assert_eq!(room_lines(&out).len(), DEFAULT_ROOM_TAIL);
        assert!(room_lines(&out)[0].ends_with("post 4"), "out = {out}");

        let out = run(&["show", "alice", "--room", "2"], &env[0]).expect("show --room 2");
        assert_eq!(room_lines(&out).len(), 2);
        assert!(room_lines(&out)[1].ends_with("post 13"), "out = {out}");
    }

    /// `--room 0` suppresses the section entirely, and Teams-off output is
    /// byte-identical to what it was before the section existed.
    #[test]
    fn room_zero_and_teams_off_print_no_section() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        post_room(&dir, &[("alice", "hello")]);

        let zero = run(&["show", "alice", "--room", "0"], &env[0]).expect("show --room 0");
        assert!(!zero.contains("--- room"), "zero = {zero}");

        // Teams off: the same profile, and the same bytes, room or no room.
        std::fs::write(dir.child("teams.yaml"), "enabled: false\n").expect("write teams.yaml");
        let off = run(&["show", "swe"], &env[0]).expect("show swe");
        assert!(!off.contains("--- room"), "off = {off}");
        std::fs::remove_dir_all(room_and_banks(&dir).0).expect("remove the room");
        assert_eq!(
            off,
            run(&["show", "swe"], &env[0]).expect("show swe"),
            "Teams off must print the same bytes with and without a room"
        );
    }

    /// A room that was never written is simply no section — and no `mkdir`.
    #[test]
    fn no_room_dir_is_no_section_and_creates_nothing() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        let (room, _) = room_and_banks(&dir);

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(!out.contains("--- room"), "out = {out}");
        assert!(!room.exists(), "a read must not create {}", room.display());
    }

    /// Direct messages are NOT shown: the CLI is the operator's glance at the
    /// room, not a way to read another teammate's mail.
    #[test]
    fn room_hides_direct_messages() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        let room = LocalRoom::new(room_and_banks(&dir).0);
        let at = DateTime::from_timestamp(1_756_000_000, 0).expect("fixed timestamp");
        room.append(&Message::room("@manager", at, "room-wide notice"))
            .expect("append room post");
        room.append(&Message::addressed(
            "@manager",
            "alice",
            at,
            "private hand-off",
        ))
        .expect("append direct post");

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(out.contains("room-wide notice"), "out = {out}");
        assert!(
            !out.contains("private hand-off"),
            "a direct message must never be printed: {out}"
        );
        assert_eq!(room_lines(&out).len(), 1);
    }

    /// A long or multi-line body is flattened to its first line and truncated,
    /// so one message is always one line under the width bound.
    #[test]
    fn room_lines_are_one_line_and_bounded() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        post_room(
            &dir,
            &[
                ("alice", "the headline\nthe body nobody asked for\nand more"),
                ("alice", &"x".repeat(400)),
            ],
        );

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        let lines = room_lines(&out);
        assert_eq!(lines.len(), 2, "out = {out}");
        assert!(lines[0].ends_with("the headline"), "lines = {lines:?}");
        assert!(
            !out.contains("the body nobody asked for"),
            "only the first line of a body is printed: {out}"
        );
        assert!(lines[1].ends_with('…'), "a cut body is marked: {lines:?}");
        for l in &lines {
            assert!(
                l.chars().count() <= ROOM_LINE_WIDTH,
                "{l:?} is {} chars, over the {ROOM_LINE_WIDTH} bound",
                l.chars().count()
            );
        }
    }

    /// A corrupt line is skipped and COUNTED — never fatal, and never silent.
    #[test]
    fn room_counts_unreadable_lines() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        teams_on(&dir);
        post_room(&dir, &[("alice", "good one")]);
        let (room, _) = room_and_banks(&dir);
        let log = std::fs::read_dir(&room)
            .expect("read room")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .next()
            .expect("one log file");
        let mut text = std::fs::read_to_string(&log).expect("read log");
        text.push_str("{not json\n{\"from\":\"alice\"}\n");
        std::fs::write(&log, text).expect("write log");

        let out = run(&["show", "alice"], &env[0]).expect("show alice");
        assert!(out.contains("good one"), "out = {out}");
        assert!(
            out.contains("(2 unreadable lines skipped)"),
            "the skip must be counted: {out}"
        );
    }

    /// `--room` wants a number, and says so rather than guessing one.
    #[test]
    fn room_flag_rejects_a_missing_or_bad_count() {
        let dir = TempDir::new();
        let (env, _) = hermetic(&dir);
        assert!(
            run(&["show", "swe", "--room"], &env[0])
                .expect_err("no count")
                .contains("usage:")
        );
        assert!(
            run(&["show", "swe", "--room", "lots"], &env[0])
                .expect_err("bad count")
                .contains("--room takes a message count")
        );
        assert!(
            run(&["show", "swe", "--wat"], &env[0])
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
        let getenv = getenv_for(&env[0]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_teams_with(
            &["show".to_string(), "swe".to_string()],
            &getenv,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&out).contains("--- resolved prompt ---"));
        assert!(err.is_empty());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_teams_with(
            &["show".to_string(), "nobody".to_string()],
            &getenv,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(String::from_utf8_lossy(&err).starts_with("symphony teams: "));
    }
}
