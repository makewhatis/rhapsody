//! mcp — the `rhapsodyd mcp` subcommand (parity port of `$REF/cmd/symphony/mcp.go`): serves the local
//! MCP facade over stdio, a thin client of the daemon's loopback API.
//!
//! It resolves the SAME WORKFLOW.md the daemon uses (positional arg / `SYMPHONY_WORKFLOW` / default)
//! to learn the loopback base URL (`server.port`, or the live `runtime.json` port) and which write
//! tools are enabled, then serves until the peer disconnects or `ctx` is cancelled. It never touches
//! `~/.rhapsody` or the DB — all state comes from the daemon's loopback HTTP API (INF-473). stdout is
//! the MCP transport; errors go to stderr.

use std::io::Write;
use std::path::Path;

use tracing_subscriber::fmt::MakeWriter;

use rhapsody_config::{decode, workflow};
use rhapsody_mcp::{Client, Facade, Options, resolve_daemon_port};
use rhapsody_orchestrator::CancelWait;

/// Serves the `rhapsodyd mcp` local MCP facade over stdio until the peer disconnects or `ctx` is
/// cancelled (Go's `srv.Run(ctx, &mcp.StdioTransport{})`). A config-resolution failure surfaces the
/// `symphony mcp:` marker + exit 1; a clean stop (peer EOF or ctx cancel, Go's `context.Canceled`)
/// exits 0; any other serve error exits 1. Mirrors Go `runMCP`.
pub async fn run_mcp<W>(mut ctx: CancelWait, args: &[String], stderr: W) -> i32
where
    W: for<'a> MakeWriter<'a>,
{
    let facade = match mcp_server_from_args(args, |k| std::env::var(k).unwrap_or_default()) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(stderr.make_writer(), "symphony mcp: {e}");
            return 1;
        }
    };
    // Go's `srv.Run(ctx, …)` returns when ctx is cancelled OR the peer disconnects; `run_stdio` serves
    // until the peer disconnects, so select ctx cancellation alongside it (Go treats a ctx-Canceled
    // exit as clean → 0).
    let result = tokio::select! {
        r = facade.run_stdio() => r,
        _ = ctx.cancelled() => Ok(()),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr.make_writer(), "symphony mcp: {e}");
            1
        }
    }
}

/// Resolves config and builds the facade server (factored out of [`run_mcp`] so the registration
/// surface is unit-testable without hijacking stdin/stdout). Resolves the workflow path, loads +
/// decodes the config, dials the discovered daemon port, and threads the `SYMPHONY_RUN_ID` /
/// `SYMPHONY_ISSUE` "me" defaults. `getenv` is injectable for tests. Mirrors Go `mcpServerFromArgs`.
fn mcp_server_from_args(
    args: &[String],
    getenv: impl Fn(&str) -> String,
) -> Result<Facade, String> {
    let path = resolve_mcp_workflow_path(args, &getenv);
    let def =
        workflow::load(Path::new(&path)).map_err(|e| format!("load workflow {path:?}: {e}"))?;
    let cfg = decode(&def).map_err(|e| format!("decode config from {path:?}: {e}"))?;
    // The loopback port `rhapsodyd mcp` dials: the live `runtime.json` port when a running daemon
    // published it (reflecting a dynamic/ephemeral --port), else `server.port` from WORKFLOW.md. The
    // discovery is the mcp crate's `resolve_daemon_port` — the port of mcp.go's `daemonPort` (INF-473).
    let client = Client::for_port(resolve_daemon_port(&cfg));
    let opts = Options {
        // "me" defaults: PR3's runner env injection sets these on dispatched workers so
        // symphony_run / symphony_ticket / symphony_run_status default to the worker's own run.
        // STUDIO-603: the runner emits BOTH spellings, and either is accepted here.
        default_run_id: first_set(&getenv, "RUN_ID"),
        default_issue: first_set(&getenv, "ISSUE"),
        now: None,
    };
    Ok(Facade::new(&cfg, client, opts))
}

/// Reads the "me" identity variable `suffix` under either brand prefix, preferring the `RHAPSODY_`
/// spelling and falling back to `SYMPHONY_` (STUDIO-603). The runner sets both to the same value, so
/// the precedence only matters for an environment where an operator set just one — and either wins.
fn first_set(getenv: impl Fn(&str) -> String, suffix: &str) -> String {
    let rhapsody = getenv(&format!("RHAPSODY_{suffix}"));
    if !rhapsody.is_empty() {
        return rhapsody;
    }
    getenv(&format!("SYMPHONY_{suffix}"))
}

/// Resolves the MCP workflow path, mirroring the daemon's resolution: a positional arg wins, else
/// `SYMPHONY_WORKFLOW`, else the `WORKFLOW.md` default. Mirrors Go `resolveMCPWorkflowPath`.
fn resolve_mcp_workflow_path(args: &[String], getenv: impl Fn(&str) -> String) -> String {
    if let Some(a) = args.first().filter(|a| !a.is_empty()) {
        return a.clone();
    }
    let w = getenv("SYMPHONY_WORKFLOW");
    if !w.is_empty() {
        return w;
    }
    "WORKFLOW.md".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn no_env(_: &str) -> String {
        String::new()
    }

    // Mirrors Go `TestResolveMCPWorkflowPath`: positional arg > SYMPHONY_WORKFLOW > default.
    #[test]
    fn resolve_mcp_workflow_path_precedence() {
        let args = vec!["a.md".to_string()];
        assert_eq!(
            resolve_mcp_workflow_path(&args, |_| "b.md".to_string()),
            "a.md",
            "positional arg should win"
        );
        assert_eq!(
            resolve_mcp_workflow_path(&[], |k| if k == "SYMPHONY_WORKFLOW" {
                "b.md".to_string()
            } else {
                String::new()
            }),
            "b.md",
            "SYMPHONY_WORKFLOW should be used"
        );
        assert_eq!(
            resolve_mcp_workflow_path(&[], no_env),
            "WORKFLOW.md",
            "default should be WORKFLOW.md"
        );
    }

    // The cmd/symphony wiring of Go `TestMCPServerFromArgsRegistersReadTools`: `mcp_server_from_args`
    // resolves + loads + decodes a valid workflow and builds the facade (Ok). The facade's registered
    // tool set (always-on reads + the `mcp:`-gated writes, threading SYMPHONY_RUN_ID/ISSUE) is the
    // mcp crate's `Facade::new` behavior, covered exhaustively by that crate's facade tests
    // (`read_tools_always_registered` + the `mcp:` gating tests); this asserts the F1 wiring feeds it.
    #[test]
    fn mcp_server_from_args_builds_facade_on_valid_workflow() {
        let dir = TempDir::new();
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            format!(
                "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\nserver:\n  port: 8799\nworkspace:\n  root: {}\n---\nDo {{{{ issue.identifier }}}}.\n",
                dir.path.display()
            ),
        )
        .expect("write WORKFLOW.md");
        let getenv = |k: &str| match k {
            "SYMPHONY_RUN_ID" => "7".to_string(),
            "SYMPHONY_ISSUE" => "INF-1".to_string(),
            _ => String::new(),
        };
        let args = vec![wf.to_string_lossy().into_owned()];
        assert!(
            mcp_server_from_args(&args, getenv).is_ok(),
            "mcp_server_from_args should build the facade for a valid workflow"
        );
    }

    // STUDIO-603: the "me" identity is read under EITHER brand prefix — the RHAPSODY_ spelling wins
    // when both are set (the runner sets both to the same value), and SYMPHONY_ alone still works.
    #[test]
    fn first_set_accepts_either_brand_prefix() {
        let only_symphony = |k: &str| match k {
            "SYMPHONY_RUN_ID" => "7".to_string(),
            _ => String::new(),
        };
        assert_eq!(first_set(only_symphony, "RUN_ID"), "7");

        let only_rhapsody = |k: &str| match k {
            "RHAPSODY_RUN_ID" => "9".to_string(),
            _ => String::new(),
        };
        assert_eq!(first_set(only_rhapsody, "RUN_ID"), "9");

        let both = |k: &str| match k {
            "RHAPSODY_RUN_ID" => "9".to_string(),
            "SYMPHONY_RUN_ID" => "7".to_string(),
            _ => String::new(),
        };
        assert_eq!(
            first_set(both, "RUN_ID"),
            "9",
            "RHAPSODY_ wins when both set"
        );

        // Neither set (a coordinator session) stays empty, not a literal fallback.
        assert_eq!(first_set(no_env, "ISSUE"), "");
    }

    // A bad workflow path surfaces the load error (the `symphony mcp:` dispatch marker is added by
    // `run_mcp`; the run()-level dispatch is covered by run.rs's `run_dispatches_to_mcp`).
    #[test]
    fn mcp_server_from_args_errors_on_missing_workflow() {
        let dir = TempDir::new();
        let missing = dir.child("nope.md");
        let args = vec![missing.to_string_lossy().into_owned()];
        match mcp_server_from_args(&args, no_env) {
            Ok(_) => panic!("missing workflow must error"),
            Err(err) => assert!(err.contains("load workflow"), "err = {err:?}"),
        }
    }
}
