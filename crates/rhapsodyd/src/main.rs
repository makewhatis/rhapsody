//! rhapsodyd — the Rhapsody daemon binary (parity port of Go `cmd/symphony/main.go`).
//!
//! Sets up SIGINT/SIGTERM-triggered cancellation, then delegates to [`rhapsodyd::run::run`] and exits
//! with its return code. Mirrors Go `main` (`signal.NotifyContext(ctx, os.Interrupt, SIGTERM)` →
//! `os.Exit(run(ctx, os.Args[1:], os.Stderr))`).

use std::io::IsTerminal;

use rhapsody_orchestrator::CancelSignal;

#[tokio::main]
async fn main() {
    // A cancellation signal fired on SIGINT/SIGTERM, mirroring Go's `signal.NotifyContext`.
    let signal = CancelSignal::new();
    let ctx = signal.wait();
    let sig = signal.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        sig.cancel();
    });

    // Whether stderr is a TTY governs the banner's ANSI color (Go type-asserts the writer to
    // `*os.File`; the Rust boot receives the TTY-ness explicitly).
    let is_terminal = std::io::stderr().is_terminal();
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `std::io::stderr` (the fn) is a `MakeWriter` — the daemon's log/banner/error sink.
    let code = rhapsodyd::run::run(ctx, &args, std::io::stderr, is_terminal).await;
    std::process::exit(code);
}

/// Resolves once the process receives SIGINT (Ctrl-C) or SIGTERM — the two signals Go's daemon
/// treats as a graceful-shutdown request. A failure to register the SIGTERM handler (rare) falls back
/// to Ctrl-C only, so shutdown wiring never aborts startup.
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sigterm) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
