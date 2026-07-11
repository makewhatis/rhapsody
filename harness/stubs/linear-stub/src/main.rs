//! linear-stub — scripted Linear GraphQL double for capture + tracker tests.
//!
//! Usage: `linear-stub --scenario <path.json> [--port N]` (port defaults to 0 → ephemeral).
//! Prints `LISTENING <port>` on stdout once bound (the capture script / e2e greps for it to read
//! the actual port), then serves `POST /graphql`. Routes by operation name; mutations mutate the
//! in-memory scenario state (issue-state updates, comments, assignee) so multi-step daemon runs
//! behave. See `lib.rs` for the enumerated operation set.

use std::io::Write;

use anyhow::{Context, Result, bail};
use linear_stub::{router, scenario::Scenario};

#[tokio::main]
async fn main() -> Result<()> {
    let (scenario_path, port) = parse_args(std::env::args().skip(1))?;
    let scenario = Scenario::from_path(&scenario_path)?;
    let app = router(scenario);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind 127.0.0.1:{port}"))?;
    let bound = listener.local_addr().context("resolve bound address")?;

    // Announce readiness so a supervising script can grep the actual (possibly ephemeral) port.
    // Flush so the reader never blocks on stdio buffering.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "LISTENING {}", bound.port()).context("write LISTENING")?;
    stdout.flush().context("flush stdout")?;

    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

/// Parse `--scenario <path>` (required) and `--port <N>` (optional, default 0 → ephemeral).
fn parse_args(args: impl Iterator<Item = String>) -> Result<(String, u16)> {
    let mut scenario = None;
    let mut port: u16 = 0;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" => scenario = Some(args.next().context("--scenario needs a value")?),
            "--port" => {
                let value = args.next().context("--port needs a value")?;
                port = value
                    .parse()
                    .with_context(|| format!("invalid --port {value:?}"))?;
            }
            other => bail!(
                "unexpected argument {other:?} (usage: linear-stub --scenario <path.json> [--port N])"
            ),
        }
    }
    let scenario = scenario.context("missing required --scenario <path.json>")?;
    Ok((scenario, port))
}
