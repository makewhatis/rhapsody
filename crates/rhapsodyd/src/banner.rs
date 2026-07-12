//! banner — parity port of Go `internal/banner`: renders a colorful Symphony startup banner (an
//! ASCII-art wordmark, a key-config summary, and a per-project table). [`render`] is a pure function
//! over its [`Data`] + a `color_enabled` flag, writing to any `io::Write`, so it is unit-testable
//! without a TTY. Color is plain ANSI escape-code string constants (no third-party deps); callers
//! decide `color_enabled` (TTY + `NO_COLOR`). Mirrors `$REF/internal/banner/banner.go`.

use std::io::{self, Write};

/// One resolved routing target rendered as a table row. `repo == ""` renders the "hook-clone (no
/// repo)" marker (the legacy hook-populated workspace). Mirrors Go `banner.Project`.
#[derive(Debug, Clone, Default)]
pub struct Project {
    /// Project slug.
    pub slug: String,
    /// Git remote; empty renders the hook-clone marker.
    pub repo: String,
    /// Claude model.
    pub model: String,
    /// Reasoning effort.
    pub effort: String,
    /// Permission mode.
    pub permission_mode: String,
    /// Billing-guard on/off.
    pub billing_guard: bool,
}

/// The resolved runtime config the banner summarizes: a flat, presentation-ready view (callers
/// resolve config/env first, then hand values here as strings/ints so the renderer stays pure and
/// dependency-free). Mirrors Go `banner.Data`.
#[derive(Debug, Clone, Default)]
pub struct Data {
    /// Dashboard URL; `""` => observability server disabled.
    pub dashboard_url: String,
    /// Agent backend.
    pub backend: String,
    /// Max concurrent agents.
    pub max_concurrent: i32,
    /// Max turns per run.
    pub max_turns: i32,
    /// Human-readable poll interval, e.g. `"30s"`.
    pub poll_interval: String,
    /// Active tracker states.
    pub active_states: Vec<String>,
    /// Resolved key owner the candidate filter binds to (e.g. `"David Johansen <david@…>"`); `""`
    /// => a generic key-owner fallback is shown (filtering still applies once resolved at poll time).
    pub assignee: String,
    /// On-disk path, or `"disabled"`/`"in-memory"`.
    pub storage_path: String,
    /// `0` => keep forever (only meaningful for on-disk storage).
    pub retention_days: i32,
    /// OTel endpoint; `""` => export disabled.
    pub otel_endpoint: String,
    /// Resolved routing targets.
    pub projects: Vec<Project>,
}

// ANSI escape-code string constants (SGR). Only used when `color_enabled` is true.
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";

// A tasteful block-style SYMPHONY wordmark. NOTE: several art lines carry DELIBERATE trailing
// spaces to preserve the slanted right edge of the wordmark (the trailing "Y"). Do not strip them —
// `cargo fmt` does not touch literal contents, but a "trim trailing whitespace"-on-save editor will
// silently break alignment. Mirrors Go `symphonyArt`.
const SYMPHONY_ART: [&str; 5] = [
    r" ____  __   __ __  __  ____   _   _   ___   _   _ __   __",
    r"/ ___| \ \ / /|  \/  ||  _ \ | | | | / _ \ | \ | |\ \ / /",
    r"\___ \  \ V / | |\/| || |_) || |_| || | | ||  \| | \ V / ",
    r" ___) |  | |  | |  | ||  __/ |  _  || |_| || |\  |  | |  ",
    r"|____/   |_|  |_|  |_||_|    |_| |_| \___/ |_| \_|  |_|  ",
];

/// The one-line subtitle under the wordmark. Mirrors Go `tagline`.
const TAGLINE: &str = "       ~ coding-agent orchestrator ~";

/// Applies color when enabled; otherwise returns text verbatim. Centralizing here guarantees the
/// `on == false` path emits the SAME layout with zero escapes. Mirrors Go `painter`.
struct Painter {
    on: bool,
}

impl Painter {
    fn paint(&self, codes: &str, s: &str) -> String {
        if !self.on || codes.is_empty() {
            s.to_string()
        } else {
            format!("{codes}{s}{ANSI_RESET}")
        }
    }
}

/// Writes the full banner (wordmark + summary + projects table) to `w`. When `color_enabled` is
/// false the output contains zero ANSI escape sequences but is byte-identical in layout to the
/// colored output with escapes stripped. Mirrors Go `banner.Render`.
pub fn render<W: Write>(w: &mut W, d: &Data, color_enabled: bool) -> io::Result<()> {
    let p = Painter { on: color_enabled };
    let mut b = String::new();

    b.push('\n');
    // Wordmark: bold magenta art, then a dim/cyan tagline line.
    for line in SYMPHONY_ART {
        b.push_str(&p.paint(&format!("{ANSI_BOLD}{ANSI_MAGENTA}"), line));
        b.push('\n');
    }
    b.push_str(&p.paint(&format!("{ANSI_DIM}{ANSI_CYAN}"), TAGLINE));
    b.push('\n');
    b.push('\n');

    // --- key-config summary ---
    let dash_color = if d.dashboard_url.is_empty() {
        ANSI_YELLOW
    } else {
        ANSI_GREEN
    };
    let dash = if d.dashboard_url.is_empty() {
        "disabled"
    } else {
        d.dashboard_url.as_str()
    };
    push_row(&mut b, &p, "Dashboard", dash, dash_color);
    push_row(
        &mut b,
        &p,
        "Agent",
        &format!(
            "backend={}  max_concurrent_agents={}  max_turns={}",
            or_dash(&d.backend),
            d.max_concurrent,
            d.max_turns
        ),
        ANSI_GREEN,
    );
    push_row(&mut b, &p, "Storage", &storage_line(d), storage_color(d));
    push_row(
        &mut b,
        &p,
        "Poll",
        &format!(
            "interval={}  active_states=[{}]",
            or_dash(&d.poll_interval),
            d.active_states.join(", ")
        ),
        ANSI_GREEN,
    );
    // Assignee: candidates are always filtered to the API key owner's assigned issues.
    let (assignee, assignee_color) = if d.assignee.trim().is_empty() {
        ("LINEAR_API_KEY owner (resolved at first poll)", ANSI_YELLOW)
    } else {
        (d.assignee.as_str(), ANSI_GREEN)
    };
    push_row(&mut b, &p, "Assignee", assignee, assignee_color);
    if d.otel_endpoint.is_empty() {
        push_row(&mut b, &p, "OTel", "disabled", ANSI_YELLOW);
    } else {
        push_row(
            &mut b,
            &p,
            "OTel",
            &format!("endpoint={}", d.otel_endpoint),
            ANSI_GREEN,
        );
    }

    b.push('\n');

    // --- projects table ---
    b.push_str("  ");
    b.push_str(&p.paint(
        &format!("{ANSI_BOLD}{ANSI_CYAN}"),
        &format!("PROJECTS ({})", d.projects.len()),
    ));
    b.push('\n');
    write_projects(&mut b, &p, &d.projects);
    b.push('\n');

    w.write_all(b.as_bytes())
}

/// One key-config summary row: a dim, 12-wide label then a colored value. Mirrors the `row` closure
/// in Go `Render`.
fn push_row(b: &mut String, p: &Painter, label: &str, value: &str, val_color: &str) {
    b.push_str("  ");
    b.push_str(&p.paint(ANSI_DIM, &format!("{label:<12}")));
    b.push_str("  ");
    b.push_str(&p.paint(val_color, value));
    b.push('\n');
}

/// The presentation-ready form of one project (repo marker + dashes applied). Mirrors Go `rowData`.
struct RowData {
    slug: String,
    repo: String,
    model: String,
    effort: String,
    perm: String,
    guard: String,
}

/// Renders the per-project table: a dim header row then one row per project. Columns are
/// width-aligned from the data so values line up in both color modes. Mirrors Go `writeProjects`.
fn write_projects(b: &mut String, p: &Painter, projects: &[Project]) {
    let rows: Vec<RowData> = projects
        .iter()
        .map(|pr| RowData {
            slug: or_dash(&pr.slug),
            repo: if pr.repo.trim().is_empty() {
                "hook-clone (no repo)".to_string()
            } else {
                pr.repo.clone()
            },
            model: or_dash(&pr.model),
            effort: or_dash(&pr.effort),
            perm: or_dash(&pr.permission_mode),
            guard: bool_on_off(pr.billing_guard),
        })
        .collect();

    let hdr = RowData {
        slug: "slug".to_string(),
        repo: "repo".to_string(),
        model: "model".to_string(),
        effort: "effort".to_string(),
        perm: "permission".to_string(),
        guard: "billing".to_string(),
    };
    let w_slug = max_len(&hdr.slug, &rows, |r| &r.slug);
    let w_repo = max_len(&hdr.repo, &rows, |r| &r.repo);
    let w_model = max_len(&hdr.model, &rows, |r| &r.model);
    let w_effort = max_len(&hdr.effort, &rows, |r| &r.effort);
    let w_perm = max_len(&hdr.perm, &rows, |r| &r.perm);

    let line = |r: &RowData| {
        format!(
            "  {:<w_slug$}  {:<w_repo$}  {:<w_model$}  {:<w_effort$}  {:<w_perm$}  {}",
            r.slug, r.repo, r.model, r.effort, r.perm, r.guard
        )
    };

    // Header (dim) then green-slug rows.
    b.push_str(&p.paint(ANSI_DIM, &line(&hdr)));
    b.push('\n');

    for r in &rows {
        b.push_str("  ");
        b.push_str(&p.paint(ANSI_GREEN, &format!("{:<w_slug$}", r.slug)));
        b.push_str("  ");
        b.push_str(&p.paint(ANSI_CYAN, &format!("{:<w_repo$}", r.repo)));
        b.push_str("  ");
        b.push_str(&p.paint(ANSI_GREEN, &format!("{:<w_model$}", r.model)));
        b.push_str("  ");
        b.push_str(&p.paint(ANSI_GREEN, &format!("{:<w_effort$}", r.effort)));
        b.push_str("  ");
        b.push_str(&p.paint(ANSI_DIM, &format!("{:<w_perm$}", r.perm)));
        b.push_str("  ");
        b.push_str(&p.paint(guard_color(&r.guard), &r.guard));
        b.push('\n');
    }
}

/// The storage summary line. Mirrors Go `storageLine`.
fn storage_line(d: &Data) -> String {
    match d.storage_path.trim().to_lowercase().as_str() {
        "" | "disabled" | "off" => "disabled".to_string(),
        "in-memory" | ":memory:" => "in-memory (ephemeral)".to_string(),
        _ => {
            if d.retention_days == 0 {
                format!("{}  retention_days=forever", d.storage_path)
            } else {
                format!("{}  retention_days={}", d.storage_path, d.retention_days)
            }
        }
    }
}

/// The storage summary color (yellow when disabled). Mirrors Go `storageColor`.
fn storage_color(d: &Data) -> &'static str {
    match d.storage_path.trim().to_lowercase().as_str() {
        "" | "disabled" | "off" => ANSI_YELLOW,
        _ => ANSI_GREEN,
    }
}

/// The billing-guard cell color (green when on). Mirrors Go `guardColor`.
fn guard_color(s: &str) -> &'static str {
    if s == "on" { ANSI_GREEN } else { ANSI_YELLOW }
}

/// `on`/`off` for a bool. Mirrors Go `boolOnOff`.
fn bool_on_off(b: bool) -> String {
    if b {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// The value verbatim, or `-` when blank-after-trim. Mirrors Go `orDash`.
fn or_dash(s: &str) -> String {
    if s.trim().is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

/// The max byte-length of `header` and every row's projected cell — the column width. Mirrors Go
/// `maxLen`.
fn max_len(header: &str, rows: &[RowData], get: impl Fn(&RowData) -> &str) -> usize {
    let mut w = header.len();
    for r in rows {
        let l = get(r).len();
        if l > w {
            w = l;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strips SGR color escapes (`\x1b[…m`) so layouts can be compared independent of color — the
    /// Rust equivalent of the Go test's `ansiPattern`/`stripANSI` (kept out of the production code).
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    fn render_to_string(d: &Data, color_enabled: bool) -> String {
        let mut buf = Vec::new();
        render(&mut buf, d, color_enabled).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    /// A representative multi-field `Data`. Mirrors Go `sampleData`.
    fn sample_data() -> Data {
        Data {
            dashboard_url: "http://127.0.0.1:8080".to_string(),
            backend: "claude".to_string(),
            max_concurrent: 10,
            max_turns: 20,
            poll_interval: "30s".to_string(),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            assignee: "David Johansen <david@example.com>".to_string(),
            storage_path: "/tmp/ws/symphony.db".to_string(),
            retention_days: 30,
            otel_endpoint: "127.0.0.1:4317".to_string(),
            projects: vec![
                Project {
                    slug: "alpha".to_string(),
                    repo: "git@github.com:org/alpha.git".to_string(),
                    model: "opus".to_string(),
                    effort: "high".to_string(),
                    permission_mode: "bypassPermissions".to_string(),
                    billing_guard: true,
                },
                Project {
                    slug: "beta".to_string(),
                    repo: String::new(),
                    model: "sonnet".to_string(),
                    effort: "medium".to_string(),
                    permission_mode: "default".to_string(),
                    billing_guard: false,
                },
            ],
        }
    }

    // Mirrors Go `TestRenderContainsProjectRows`: each project's slug + its own model + effort + repo.
    #[test]
    fn render_contains_project_rows() {
        let out = render_to_string(&sample_data(), false);
        for want in ["alpha", "opus", "high", "git@github.com:org/alpha.git"] {
            assert!(out.contains(want), "expected {want:?} (alpha)\n{out}");
        }
        for want in ["beta", "sonnet", "medium"] {
            assert!(out.contains(want), "expected {want:?} (beta)\n{out}");
        }
    }

    // Mirrors Go `TestRenderEmptyRepoShowsHookCloneMarker`.
    #[test]
    fn render_empty_repo_shows_hook_clone_marker() {
        let out = render_to_string(&sample_data(), false);
        assert!(
            out.contains("hook-clone (no repo)"),
            "expected hook-clone marker\n{out}"
        );
    }

    // Mirrors Go `TestRenderSummaryContainsKeyConfig`.
    #[test]
    fn render_summary_contains_key_config() {
        let out = render_to_string(&sample_data(), false);
        for want in [
            "http://127.0.0.1:8080",
            "/tmp/ws/symphony.db",
            "30s",
            "claude",
            "127.0.0.1:4317",
        ] {
            assert!(
                out.contains(want),
                "expected summary to contain {want:?}\n{out}"
            );
        }
    }

    // Mirrors Go `TestRenderShowsAssignee`.
    #[test]
    fn render_shows_assignee() {
        let out = render_to_string(&sample_data(), false);
        assert!(
            out.contains("David Johansen <david@example.com>"),
            "expected resolved assignee in banner\n{out}"
        );
    }

    // Mirrors Go `TestRenderAssigneeFallbackWhenUnresolved`.
    #[test]
    fn render_assignee_fallback_when_unresolved() {
        let mut d = sample_data();
        d.assignee = String::new(); // viewer not resolved at startup
        let out = render_to_string(&d, false);
        assert!(
            out.contains("LINEAR_API_KEY owner"),
            "expected key-owner fallback label\n{out}"
        );
    }

    // Mirrors Go `TestRenderNoColorHasNoEscapes`.
    #[test]
    fn render_no_color_has_no_escapes() {
        let out = render_to_string(&sample_data(), false);
        assert!(
            !out.contains('\x1b'),
            "color_enabled=false must emit zero ANSI escapes"
        );
    }

    // Mirrors Go `TestRenderColorHasEscapes`.
    #[test]
    fn render_color_has_escapes() {
        let out = render_to_string(&sample_data(), true);
        assert!(
            out.contains('\x1b'),
            "color_enabled=true must emit some ANSI escapes"
        );
    }

    // Mirrors Go `TestRenderColorAndNoColorSameLayout`: stripping escapes from the colored output
    // must yield the plain layout.
    #[test]
    fn render_color_and_no_color_same_layout() {
        let color = render_to_string(&sample_data(), true);
        let plain = render_to_string(&sample_data(), false);
        assert_eq!(
            strip_ansi(&color),
            plain,
            "color output (escapes stripped) must equal plain layout"
        );
    }

    // Mirrors Go `TestRenderMultiProjectTwoRows`: two projects with DIFFERENT models render two rows
    // with the right per-project model/effort, each appearing exactly once.
    #[test]
    fn render_multi_project_two_rows() {
        let d = Data {
            dashboard_url: "http://127.0.0.1:9090".to_string(),
            backend: "claude".to_string(),
            poll_interval: "10s".to_string(),
            storage_path: "disabled".to_string(),
            projects: vec![
                Project {
                    slug: "proj-one".to_string(),
                    repo: "https://github.com/o/one".to_string(),
                    model: "opus-4".to_string(),
                    effort: "max".to_string(),
                    ..Default::default()
                },
                Project {
                    slug: "proj-two".to_string(),
                    repo: "https://github.com/o/two".to_string(),
                    model: "sonnet-4".to_string(),
                    effort: "low".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = render_to_string(&d, false);
        assert!(
            out.contains("proj-one") && out.contains("opus-4") && out.contains("max"),
            "expected proj-one/opus-4/max row\n{out}"
        );
        assert!(
            out.contains("proj-two") && out.contains("sonnet-4") && out.contains("low"),
            "expected proj-two/sonnet-4/low row\n{out}"
        );
        assert_eq!(out.matches("opus-4").count(), 1, "opus-4 appears once");
        assert_eq!(out.matches("sonnet-4").count(), 1, "sonnet-4 appears once");
    }

    // Mirrors Go `TestRenderDashboardDisabledNote`.
    #[test]
    fn render_dashboard_disabled_note() {
        let mut d = sample_data();
        d.dashboard_url = String::new(); // disabled
        let out = render_to_string(&d, false);
        assert!(
            out.contains("disabled"),
            "empty dashboard_url should render a disabled note\n{out}"
        );
    }
}
