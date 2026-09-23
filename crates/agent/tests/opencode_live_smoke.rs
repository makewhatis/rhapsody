//! A REAL opencode turn against a REAL provider — the one check no fake can stand in for
//! (STUDIO-902).
//!
//! ⚠️ `#[ignore]`d, and it must stay that way. It needs the `opencode` binary, a working
//! `opencode auth login` credential, and network access to a paid provider, so it can never run in
//! CI. It exists because every other test in this adapter drives a bash script that this author
//! also wrote: those prove the runner handles the stream it was told to expect, and only this one
//! proves the stream is the one the CLI actually emits.
//!
//! Run it on an operator machine:
//!
//! ```sh
//! RHAPSODY_OPENCODE_SMOKE=/opt/homebrew/Cellar/opencode/1.18.30/bin/opencode \
//!   cargo test -p rhapsody-agent --test opencode_live_smoke -- --ignored --nocapture
//! ```
//!
//! `RHAPSODY_OPENCODE_SMOKE` is the ABSOLUTE path to the binary on purpose (the spike found a
//! broken npm shim ahead of the real one on `PATH`; findings §4.1). `RHAPSODY_OPENCODE_SMOKE_MODEL`
//! overrides the model, which otherwise defaults to the one the spike captured against.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rhapsody_agent::{
    EVENT_SESSION_STARTED, EVENT_TURN_COMPLETED, EVENT_TURN_FAILED, Event, Runner as _,
    TURN_SUCCEEDED, opencode,
};
use rhapsody_core::Issue;

const DEFAULT_MODEL: &str = "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash";

/// A scratch dir removed on drop (STUDIO-1031), so an assertion panic mid-smoke does not leak
/// `rhapsody-oc-smoke-*` into `$TMPDIR`.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> TempDir {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[tokio::test]
#[ignore = "needs the real opencode binary, a real credential and network; set RHAPSODY_OPENCODE_SMOKE"]
async fn a_real_turn_against_a_real_provider() {
    let Some(bin) = std::env::var("RHAPSODY_OPENCODE_SMOKE")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("skipped: set RHAPSODY_OPENCODE_SMOKE to the opencode binary's absolute path");
        return;
    };
    let model = std::env::var("RHAPSODY_OPENCODE_SMOKE_MODEL")
        .unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    // A scratch workspace under the configured root, with something real to read and edit.
    let root = TempDir::new("rhapsody-oc-smoke");
    let ws = root.join("sandbox");
    std::fs::create_dir_all(&ws).expect("create sandbox");
    std::fs::write(
        ws.join("NOTES.md"),
        "The build counter lives in counter.txt.\n",
    )
    .expect("write NOTES.md");
    std::fs::write(ws.join("counter.txt"), "7\n").expect("write counter.txt");
    let root_s = std::fs::canonicalize(&root)
        .expect("canonicalize root")
        .to_string_lossy()
        .into_owned();
    let ws_s = std::fs::canonicalize(&ws)
        .expect("canonicalize ws")
        .to_string_lossy()
        .into_owned();

    let runner = opencode::Runner::new(opencode::Config {
        command: bin,
        model,
        workspace_root: root_s,
        turn_timeout: Duration::from_secs(180),
        // Left empty so the real resolution is exercised: the credential is located from the
        // daemon's own environment and copied into the private state dir. A bug there is exactly
        // the 401 this asserts against.
        auth_source: String::new(),
        state_root: String::new(),
        // No daemon binary to point an MCP server at from a test process, and the tool-name
        // rewrite is already proven against the captures.
        inject_mcp: false,
        ..Default::default()
    });

    let sess = runner
        .start_session(
            &ws_s,
            Issue {
                id: "STUDIO-902".to_string(),
                identifier: "STUDIO-902".to_string(),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("start_session (a failure here is most likely a missing `opencode auth login`)");

    let seen: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let on_event = move |e: Event| {
        eprintln!("event {} {:?}", e.event_type, e.message);
        sink.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(e);
    };

    let (tr, err) = sess
        .run_turn(
            "Read NOTES.md, then set the value in counter.txt to 8. Reply with only the new value.",
            None,
            None,
            &on_event,
        )
        .await;

    let evs = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    eprintln!("--- {} events, usage {:?}", evs.len(), tr.usage);

    assert!(err.is_none(), "turn failed: {err:?} (events: {evs:#?})");
    assert_eq!(tr.status, TURN_SUCCEEDED);
    assert!(
        evs.iter().any(|e| e.event_type == EVENT_SESSION_STARTED),
        "no session event: {evs:#?}"
    );
    assert!(
        evs.iter().any(|e| e.event_type == EVENT_TURN_COMPLETED),
        "no completion event: {evs:#?}"
    );
    assert!(
        !evs.iter().any(|e| e.event_type == EVENT_TURN_FAILED),
        "a failure event was emitted: {evs:#?}"
    );
    assert!(
        sess.thread_id().starts_with("ses_"),
        "no session id captured: {:?}",
        sess.thread_id()
    );
    // Usage really was summed across steps, not left at zero or at one step's figures.
    assert!(
        tr.usage.total_tokens > 0,
        "no usage recorded: {:?}",
        tr.usage
    );

    // ⚠️ The agent did the WORK, not just emitted a well-formed stream.
    let counter = std::fs::read_to_string(ws.join("counter.txt")).expect("read counter.txt");
    assert_eq!(counter.trim(), "8", "the agent did not edit the file");

    sess.stop().await.expect("stop");
    // `root` (a `TempDir`) removes itself on drop.
}
