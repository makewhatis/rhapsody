//! STUDIO-1003 (PB8) — the daemon end-to-end brokered chain and its recursive canary scan.
//!
//! Design §14.5 bullet 1: "fake Keychain/IPC credential -> prepared dispatch -> broker -> fake
//! upstream -> OpenCode fixture". Every piece below is the REAL daemon piece, only the two ends are
//! fakes:
//!
//! * a fake credential OWNER served over a real Unix socket, using the real `ServerSession`
//!   handshake the desktop supervisor's owner server uses (`rhapsody-credential-ipc`);
//! * the daemon's real [`CredentialResolver`] with a bootstrap frame pointing at that socket;
//! * the daemon's real [`DaemonProviderSource`], which reads the bound credential and registers a
//!   session with the real [`BrokerRuntime`]'s loopback listener;
//! * the real [`build_dispatch_runner`] / [`DispatchRunner::start`] prepared-dispatch path;
//! * the real brokered OpenCode session (`start_brokered_session`) driven by a fake CLI that issues
//!   a real HTTP request through the broker to a loopback fake upstream;
//! * a recursive canary scan over every Rhapsody-owned boundary the turn can reach: the child's argv
//!   and environment, the generated state tree, the transcript, the events, the humanized API
//!   rendering, an actual SQLite store row set, and the captured tracing log.
//!
//! The canary key is a distinctive upstream credential: the fake upstream MUST see it (it is the
//! real provider key), and it must appear NOWHERE else. The capability is the opposite: it MUST reach
//! the child (it is spendable within finite limits — the design never claims it is hidden from the
//! harness) and must appear nowhere in argv, the state tree, the transcript, events, the API
//! render, the DB, or the logs.
//!
//! MUTATION GUARD (named mutation 2): leak the canary key or the capability into any scanned
//! boundary — the broker's redactor, the opencode runner's capability redactor, or the store
//! writer — and the corresponding recursive scan reds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rhapsody_agent::opencode::Config as OpencodeConfig;
use rhapsody_agent::{
    Event, HarnessId, HarnessKnobs, LaunchContext, PreparedHarnessSpec, SessionStart,
    TURN_SUCCEEDED, Transcript, build_dispatch_runner, humanize_stream_line,
};
use rhapsody_core::Issue;
use rhapsody_credential_ipc::domain::{CredentialStateTag, Revision};
use rhapsody_credential_ipc::session::{ServerSession, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, HelloFrame, LeasePayload, ServerFrame, read_frame, write_frame,
};
use rhapsody_orchestrator::PreparedProviderSource;
use rhapsody_provider_broker::{Broker, TurnMeta};
use rhapsody_store::{EventRow, RunEnd, RunStart, Sqlite, Store, StorePath};
use rhapsodyd::broker::BrokerRuntime;
use rhapsodyd::providers::DaemonProviderSource;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

/// The upstream credential canary. It is the REAL key handed to the fake upstream and must not leak
/// anywhere else.
const CANARY_KEY: &str = "sk-CANARY-UPSTREAM-KEY-0123456789abcdef";
/// A fragment so a render that leaked only a prefix is still caught.
const CANARY_FRAGMENT: &str = "CANARY-UPSTREAM-KEY";
const MODEL: &str = "probe-model";
const PROVIDER_ID: &str = "fireworks";

// ---------------------------------------------------------------------------------------------
// Captured tracing log
// ---------------------------------------------------------------------------------------------

/// A `MakeWriter` that accumulates formatted tracing output in memory so the test can scan the
/// daemon's own broker logs for the canary.
#[derive(Clone, Default)]
struct SharedLog(Arc<Mutex<Vec<u8>>>);

impl SharedLog {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log lock")).to_string()
    }
}

impl std::io::Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLog {
    type Writer = SharedLog;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Scratch tree
// ---------------------------------------------------------------------------------------------

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = base.join(format!("rhapsody-e2e-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod script");
}

/// Recursively collect every regular file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Assert no regular file under `dir` contains any of `needles`.
fn assert_tree_clean(label: &str, dir: &Path, needles: &[&str]) {
    for file in files_under(dir) {
        let bytes = std::fs::read(&file).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        for needle in needles.iter().filter(|needle| !needle.is_empty()) {
            assert!(
                !text.contains(needle),
                "{label}: {needle:?} was persisted in {}",
                file.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fake upstream
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct UpstreamSpy {
    authorization: Mutex<Vec<String>>,
    bodies: Mutex<Vec<String>>,
}

impl UpstreamSpy {
    fn authorizations(&self) -> Vec<String> {
        self.authorization.lock().expect("auth lock").clone()
    }
}

/// A raw loopback fake upstream: records the request's `Authorization` header and body, then answers
/// a small non-streaming JSON completion whose `echo` field carries the canary key, so the broker's
/// body redactor is exercised on the real egress path.
async fn spawn_fake_upstream() -> (u16, Arc<UpstreamSpy>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake upstream");
    let port = listener.local_addr().expect("upstream addr").port();
    let spy = Arc::new(UpstreamSpy::default());
    let task_spy = Arc::clone(&spy);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let spy = Arc::clone(&task_spy);
            tokio::spawn(async move {
                let _ = serve_one_upstream(&mut socket, &spy).await;
            });
        }
    });
    (port, spy, task)
}

async fn serve_one_upstream(
    socket: &mut tokio::net::TcpStream,
    spy: &UpstreamSpy,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if socket.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&head).to_string();
    // Header names are case-insensitive, values are not: match the name case-insensitively and keep
    // the value verbatim so the credential's exact bytes are asserted.
    if let Some(value) = head_text.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.trim()
                .eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
    }) {
        spy.authorization.lock().expect("auth lock").push(value);
    }
    let lower = head_text.to_ascii_lowercase();
    let content_length = lower
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    socket.read_exact(&mut body).await?;
    spy.bodies
        .lock()
        .expect("bodies lock")
        .push(String::from_utf8_lossy(&body).to_string());

    let payload = format!(
        "{{\"choices\":[{{\"message\":{{\"role\":\"assistant\",\"content\":\"ok\"}}}}],\
         \"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}},\
         \"echo\":\"{CANARY_KEY}\"}}"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(payload.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Fake credential owner (real ServerSession over a real Unix socket)
// ---------------------------------------------------------------------------------------------

fn spawn_fake_owner(
    socket_path: PathBuf,
    token: String,
    value: String,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(&socket_path).expect("bind owner socket");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let token = token.clone();
            let value = value.clone();
            tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(stream);
                let mut session = ServerSession::new(Token::new(token));
                let Ok(hello) = read_frame::<_, HelloFrame>(&mut reader).await else {
                    return;
                };
                if session.accept_hello(&hello.token).is_err() {
                    return;
                }
                let Ok(ClientFrame::ReadBound {
                    seq,
                    expected_binding,
                    ..
                }) = read_frame::<_, ClientFrame>(&mut reader).await
                else {
                    return;
                };
                if session.accept_client_seq(seq).is_err() {
                    return;
                }
                let response_seq = session.next_outgoing_seq();
                let _ = write_frame(
                    &mut writer,
                    &ServerFrame::ReadBoundResult {
                        seq: response_seq,
                        revision: Revision(7),
                        state: CredentialStateTag::Present,
                        lease: Some(LeasePayload {
                            binding: expected_binding,
                            value,
                        }),
                    },
                )
                .await;
            });
        }
    })
}

// ---------------------------------------------------------------------------------------------
// The fake OpenCode CLI
// ---------------------------------------------------------------------------------------------

/// The fake managed `opencode` CLI: answers the version probe, records its argv and managed env,
/// then issues a REAL HTTP request through the broker with the capability, writing the raw response
/// body it received to `resp.log` before emitting a normal successful stream-json turn.
const OPENCODE_SCRIPT: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 1.18.30; exit 0; fi
printf '%s\n' "$*" >> "__ARGV__"
env | grep -E '^(OPENCODE_|XDG_DATA_HOME=)' > "__ENV__"
cap=$(printf '%s' "$OPENCODE_AUTH_CONTENT" | sed -n 's/.*"key":"\([^"]*\)".*/\1/p')
url=$(printf '%s' "$OPENCODE_CONFIG_CONTENT" | sed -n 's/.*"baseURL":"\([^"]*\)".*/\1/p')
printf 'cap=%s url=%s\n' "$cap" "$url" > "__BOUNDARY__"
curl -sS -X POST "$url/chat/completions" \
  -H "Authorization: Bearer $cap" \
  -H "Content-Type: application/json" \
  --data '{"model":"probe-model","messages":[{"role":"user","content":"hi"}],"max_tokens":16,"stream":false}' \
  > "__RESP__"
printf '{"type":"step_start","sessionID":"ses_e2e"}\n'
printf '{"type":"text","sessionID":"ses_e2e","part":{"type":"text","text":"ok"}}\n'
printf '{"type":"step_finish","sessionID":"ses_e2e","part":{"reason":"stop"}}\n'
"#;

fn issue(identifier: &str) -> Issue {
    Issue {
        id: identifier.to_string(),
        identifier: identifier.to_string(),
        ..Default::default()
    }
}

/// Read the capability the child was actually handed, from its boundary record.
fn capability_from_boundary(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix("cap="))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn the_brokered_daemon_chain_is_canary_clean_at_every_boundary() {
    // A thread-local subscriber: the current-thread runtime polls every task (broker listener,
    // credential read, opencode runner) on this thread, so the broker's own logs are captured.
    let log = SharedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    let _trace_guard = tracing::subscriber::set_default(subscriber);

    let scratch = Scratch::new("brokered-e2e");
    let workspace = scratch.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = workspace.canonicalize().expect("canonicalize workspace");
    let state_root = scratch.path().join("state");
    std::fs::create_dir_all(&state_root).expect("state root");
    let state_root = state_root.canonicalize().expect("canonicalize state root");

    let script = scratch.path().join("opencode");
    let argv_log = scratch.path().join("argv.log");
    let env_log = scratch.path().join("env.log");
    let boundary_log = scratch.path().join("boundary.log");
    let resp_log = scratch.path().join("resp.log");
    let transcript_out = scratch.path().join("transcript.out");
    let transcript_err = scratch.path().join("transcript.err");
    let store_path = scratch.path().join("history.db");
    write_executable(
        &script,
        &OPENCODE_SCRIPT
            .replace("__ARGV__", &argv_log.display().to_string())
            .replace("__ENV__", &env_log.display().to_string())
            .replace("__BOUNDARY__", &boundary_log.display().to_string())
            .replace("__RESP__", &resp_log.display().to_string()),
    );

    // --- the fake upstream + the real broker listener ---------------
    let (upstream_port, upstream_spy, upstream_task) = spawn_fake_upstream().await;
    let mut runtime = BrokerRuntime::bind().expect("bind the broker");
    let broker: Broker = runtime.broker_handle();
    let listener = runtime.take_listener().expect("the bound listener");
    let broker_addr = listener.local_addr();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let serve = tokio::spawn(async move {
        let _ = listener
            .run_with_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    // --- the fake IPC owner + the daemon's real resolver -------------
    let owner_socket = scratch.path().join("owner.sock");
    let owner = spawn_fake_owner(
        owner_socket.clone(),
        "bootstrap-secret".to_string(),
        CANARY_KEY.to_string(),
    );
    let resolver = rhapsodyd::providers::unavailable_owner();
    resolver.adopt_bootstrap(Some(BootstrapMessage {
        token: "bootstrap-secret".to_string(),
        socket_path: owner_socket.to_string_lossy().into_owned(),
    }));

    // --- the prepared dispatch: real source -> real broker registration ---
    let normalized_endpoint = format!("http://127.0.0.1:{upstream_port}/v1");
    let plan = rhapsody_agent::ResolvedProviderPlan {
        stable_id: PROVIDER_ID.to_string(),
        protocol: rhapsody_agent::ProviderProtocol::OpenAiCompatible,
        normalized_endpoint: normalized_endpoint.clone(),
        allow_insecure_http: true,
        credential_binding: String::new(),
        credential_ref: "keychain".to_string(),
        limits: rhapsody_agent::ProviderLimits::default(),
        model: MODEL.to_string(),
        origins: rhapsody_agent::ProviderOrigins::default(),
    };
    let day_authority: Arc<dyn rhapsody_provider_broker::CumulativeBudgetAuthority> = Arc::new(
        rhapsodyd::providerbudget::StoreDayAuthority::new(Arc::new(rhapsody_store::Noop), false),
    );
    let source = DaemonProviderSource::new(resolver, runtime.registrar(), day_authority);
    let opened = source
        .open_provider(&plan)
        .await
        .expect("the fake owner must present the bound credential");
    assert_eq!(
        broker.live_session_count(),
        1,
        "the prepared source must register exactly one live broker session"
    );

    let config = OpencodeConfig {
        command: script.to_string_lossy().into_owned(),
        model: MODEL.to_string(),
        workspace_root: workspace.to_string_lossy().into_owned(),
        turn_timeout: Duration::from_secs(30),
        state_root: state_root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let spec = PreparedHarnessSpec {
        harness: HarnessId::Opencode,
        model: Some(MODEL.to_string()),
        provider: Some(opened.provider),
        knobs: HarnessKnobs::Opencode(config),
    };
    let runner = build_dispatch_runner(spec).expect("build the dispatch runner");

    let transcript = Transcript {
        stdout: Some(Box::new(
            std::fs::File::create(&transcript_out).expect("transcript stdout"),
        )),
        stderr: Some(Box::new(
            std::fs::File::create(&transcript_err).expect("transcript stderr"),
        )),
    };
    let mut started = runner
        .start(SessionStart {
            workspace_path: workspace.to_string_lossy().into_owned(),
            issue: issue("STUDIO-1003"),
            transcript: Some(transcript),
            launch: LaunchContext::default(),
        })
        .await
        .expect("start the brokered session");
    assert!(
        started.broker_turns.is_some(),
        "a provider dispatch must carry the worker-owned ledger receiver"
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let on_event = move |event: Event| {
        sink.lock().expect("events lock").push(event);
    };

    let (attempt, receipt) = started
        .broker_turns
        .as_mut()
        .expect("broker turns")
        .arm_turn(TurnMeta::without_deadline())
        .expect("arm a turn");
    let (turn, error) = started
        .session
        .run_turn_brokered("do the work", None, None, &on_event, Some(attempt))
        .await;
    let ledger = receipt.take().expect("the finalized turn ledger");
    assert_eq!(turn.status, TURN_SUCCEEDED, "{error:?}");
    assert_eq!(
        ledger.forwarded_requests(),
        1,
        "exactly one forwarded request"
    );

    let events: Vec<Event> = events.lock().expect("events lock").clone();
    let capability = capability_from_boundary(&boundary_log);
    assert!(
        capability.len() == 43,
        "the child must have been handed a real 256-bit capability, got {capability:?}"
    );

    // 1. The upstream MUST see the real canary key, never the capability (the whole point of the
    //    broker: the real key stops here).
    let authorizations = upstream_spy.authorizations();
    assert_eq!(authorizations.len(), 1, "exactly one upstream request");
    assert_eq!(
        authorizations[0],
        format!("Bearer {CANARY_KEY}"),
        "the fake upstream must receive the real provider key"
    );
    assert!(
        !authorizations[0].contains(&capability),
        "the capability must never reach the upstream"
    );
    let upstream_bodies = upstream_spy.bodies.lock().expect("bodies lock").clone();
    assert_eq!(upstream_bodies.len(), 1);
    assert!(
        !upstream_bodies[0].contains(&capability),
        "the capability must never appear in the upstream request body"
    );
    assert!(
        upstream_bodies[0].contains(MODEL),
        "the upstream request body must be the validated chat request"
    );

    // 2. The child MUST see the capability (it is spendable within limits) and never the real key.
    let env_text = std::fs::read_to_string(&env_log).unwrap_or_default();
    assert!(
        env_text.contains(&capability),
        "the child receives the capability in its managed env"
    );
    assert!(
        !env_text.contains(CANARY_FRAGMENT),
        "the real provider key must never reach the child"
    );
    let argv_text = std::fs::read_to_string(&argv_log).unwrap_or_default();
    assert!(
        !argv_text.contains(CANARY_FRAGMENT) && !argv_text.contains(&capability),
        "neither the key nor the capability may appear in the child's argv: {argv_text}"
    );

    // 3. The broker redacts the upstream body before the child receives it: the response the child
    //    got must not carry the canary (the fake upstream echoed it).
    let received = std::fs::read_to_string(&resp_log).unwrap_or_default();
    assert!(
        !received.is_empty(),
        "the child must have received a broker response body"
    );
    assert!(
        !received.contains(CANARY_FRAGMENT),
        "the broker must redact the upstream key from the body the child receives: {received}"
    );

    // 4. Recursive scans over every Rhapsody-owned persistence boundary.
    let needles = [CANARY_FRAGMENT, capability.as_str()];

    // 4a. The generated state tree (pre-cleanup).
    assert_tree_clean("state tree", &state_root, &needles);

    // 4b. The transcript (redaction must precede the transcript boundary).
    assert_tree_clean("transcript", scratch.path(), &[CANARY_FRAGMENT]);
    let transcript_text = std::fs::read_to_string(&transcript_out).unwrap_or_default();
    assert!(
        !transcript_text.contains(&capability),
        "the capability must be redacted before the transcript boundary"
    );

    // 4c. Events.
    let events_debug = format!("{events:?}");
    assert!(
        !events_debug.contains(CANARY_FRAGMENT) && !events_debug.contains(&capability),
        "a broker event must carry neither the key nor the capability"
    );
    for event in &events {
        assert!(
            !event.message.contains(CANARY_FRAGMENT) && !event.message.contains(&capability),
            "event {:?} leaked a secret",
            event.event_type
        );
    }

    // 4d. The API rendering: the httpapi transcript humanizes the redacted stream lines.
    for line in transcript_text.lines() {
        for entry in humanize_stream_line(line.as_bytes()) {
            assert!(
                !entry.text.contains(CANARY_FRAGMENT) && !entry.text.contains(&capability),
                "the humanized API render leaked a secret: {:?}",
                entry.text
            );
        }
    }

    // 4e. A real SQLite store: persist the run + its events through the store the daemon uses, then
    //     scan both the read-back rows and the raw database files.
    let store = Sqlite::open(StorePath::Disk(store_path.clone())).expect("open the store");
    let run_id = store
        .start_run(RunStart {
            issue_id: "issue-1".to_string(),
            issue_identifier: "STUDIO-1003".to_string(),
            title: "brokered e2e".to_string(),
            attempt: 1,
            session_uuid: "sess".to_string(),
            branch: "symphony/STUDIO-1003".to_string(),
            started_at: String::new(),
            transcript_path: String::new(),
            project_slug: String::new(),
            repo: String::new(),
            team_id: String::new(),
        })
        .expect("start_run");
    let rows: Vec<EventRow> = events
        .iter()
        .enumerate()
        .map(|(index, event)| EventRow {
            seq: index as i64 + 1,
            at: String::new(),
            kind: event.event_type.clone(),
            tool: String::new(),
            text: event.message.clone(),
        })
        .collect();
    store.append_events(run_id, &rows).expect("append_events");
    store
        .end_run(
            run_id,
            RunEnd {
                outcome: rhapsody_store::OUTCOME_COMPLETED.to_string(),
                turns: 1,
                total_tokens: ledger.provider_reported_tokens().unwrap_or(0) as i64,
                ..Default::default()
            },
        )
        .expect("end_run");
    for row in store.run_events(run_id).expect("run_events") {
        assert!(
            !row.text.contains(CANARY_FRAGMENT) && !row.text.contains(&capability),
            "a persisted event row leaked a secret: {:?}",
            row.text
        );
    }
    let run = store.get_run(run_id).expect("get_run").expect("run row");
    let run_debug = format!("{run:?}");
    assert!(
        !run_debug.contains(CANARY_FRAGMENT) && !run_debug.contains(&capability),
        "the persisted run row leaked a secret"
    );
    for file in files_under(scratch.path()).into_iter().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("history.db"))
    }) {
        let bytes = std::fs::read(&file).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(CANARY_FRAGMENT) && !text.contains(&capability),
            "the raw DB file {} leaked a secret",
            file.display()
        );
    }

    // 4f. Captured daemon logs.
    let logs = log.contents();
    assert!(
        !logs.contains(CANARY_FRAGMENT),
        "the daemon's captured logs leaked the upstream key"
    );
    assert!(
        !logs.contains(&capability),
        "the daemon's captured logs leaked the capability"
    );

    // 5. Drop the session: revocation + the private state directory must be gone.
    drop(started.session);
    assert_eq!(
        broker.live_session_count(),
        0,
        "dropping the session must revoke its registry entry"
    );
    assert!(
        !state_root.exists() || files_under(&state_root).is_empty(),
        "the per-run state directory must be removed on drop: {:?}",
        files_under(&state_root)
    );

    // 6. Shutdown leaves no listener and no fake credential artifact.
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;
    assert!(
        tokio::net::TcpStream::connect(broker_addr).await.is_err(),
        "the broker listener must be gone after shutdown"
    );
    owner.abort();
    upstream_task.abort();
}
