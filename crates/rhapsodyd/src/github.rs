//! Operator check for owner-scoped GitHub credentials (STUDIO-1804).

use std::{io::Write, path::Path, sync::Arc};

pub async fn run(args: &[String], mut out: impl Write, mut err: impl Write) -> i32 {
    if args.first().map(String::as_str) != Some("check")
        || args.len() > 2
        || args.get(1).is_some_and(|s| s.starts_with('-'))
    {
        let _ = writeln!(err, "usage: rhapsodyd github check [WORKFLOW.md]");
        return 1;
    }
    let path = args.get(1).cloned().unwrap_or_else(|| {
        std::env::var("SYMPHONY_WORKFLOW").unwrap_or_else(|_| "WORKFLOW.md".into())
    });
    let config = match rhapsody_config::workflow::load(Path::new(&path))
        .map_err(|e| e.to_string())
        .and_then(|d| rhapsody_config::decode(&d).map_err(|e| e.to_string()))
    {
        Ok(config) => config,
        Err(reason) => {
            let _ = writeln!(err, "symphony github: {reason}");
            return 1;
        }
    };
    let (report, ok) = rhapsody_orchestrator::github::check(
        &config,
        Arc::new(rhapsody_workspace::github::SystemKeychain),
    )
    .await;
    if write!(out, "{report}").is_err() {
        return 1;
    }
    i32::from(!ok)
}
