//! The desktop (server) side of the authenticated provider-credential channel (STUDIO-981/P0c). No
//! Go parity — Rhapsody-only.
//!
//! [`BootstrapListener::bind`] mints a fresh per-launch token and a Unix socket at a private,
//! per-process path, mirroring exactly what the supervisor must do immediately before spawning the
//! `rhapsodyd` sidecar: generate the [`BootstrapMessage`] here, write it as the ONE frame on the
//! child's piped stdin, then close that pipe. [`BootstrapListener::accept_and_serve`] then answers
//! `read_bound` requests from whichever process connects and proves it holds the token — by
//! construction, exactly the daemon this desktop instance just spawned, and no one else, since the
//! token never touches argv, an inheritable env var, `runtime.json`, or a log line.
//!
//! Wiring `bind`/`bootstrap_message`/`accept_and_serve` into `supervisor::Inner::build_command`'s
//! actual spawn call (piping the child's stdin and adding `--credential-bootstrap`) is the named,
//! precise next integration step this ticket leaves for the composition root — deliberately not
//! done here, to avoid touching the supervisor's own heavily-tested restart/backoff state machine
//! for a ticket whose job is to prove and specify the mechanism (P0c), not to finish wiring it into
//! every call site (P1's "production boundary").

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rhapsody_credential_ipc::domain::CredentialRef;
use rhapsody_credential_ipc::session::{ServerSession, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, HelloFrame, ServerFrame, read_frame, write_frame,
};
use tokio::net::{UnixListener, UnixStream};

use crate::provider_credential::ProviderCredentialOwner;

/// How long the listener waits for a freshly accepted connection's `Hello` frame before giving up
/// on it and returning to `accept`. Only one connection is ever served at a time (see
/// `accept_and_serve`'s doc), so without this bound a connection that never sends `Hello` — a
/// same-user process that simply connects and does nothing, which the design's own threat model
/// (`provider-auth-p0-findings.md` §8) assumes can happen — wedges every later connection,
/// including the real daemon's own reconnect after a restart, forever. A legitimate handshake is a
/// single local write immediately after `connect`, so this has ample margin without making a
/// deliberately silent connection expensive to defend against.
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);

pub struct BootstrapListener {
    token: String,
    socket_path: PathBuf,
    listener: UnixListener,
}

impl BootstrapListener {
    /// Binds a fresh Unix socket at a private per-process path under `dir` (the real supervisor
    /// uses a directory under `~/.rhapsody/run`; tests use a `TempDir`) and mints a fresh bootstrap
    /// token — each daemon launch gets its own token, never reused across a restart. The socket
    /// path itself is deterministic per desktop process id (`cred-<pid>.sock`), so a second `bind`
    /// call from the SAME process reuses that same path, unconditionally removing whatever socket
    /// file is already there (including one still served by an earlier listener in this process, if
    /// any); in production the supervisor calls this exactly once per daemon launch, so that never
    /// happens.
    pub fn bind(dir: &std::path::Path) -> std::io::Result<BootstrapListener> {
        std::fs::create_dir_all(dir)?;
        // Short name: Unix socket paths are capped at ~104 bytes total (`sun_path`), and `dir` may
        // already consume a good share of that budget.
        let socket_path = dir.join(format!("cred-{}.sock", std::process::id()));
        // A stale socket file from a crashed prior run must not make `bind` fail.
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        Ok(BootstrapListener {
            token: rhapsody_credential_ipc::token::generate(),
            socket_path,
            listener,
        })
    }

    /// The one frame to write to the freshly spawned child's stdin, then never write to that pipe
    /// again.
    pub fn bootstrap_message(&self) -> BootstrapMessage {
        BootstrapMessage {
            token: self.token.clone(),
            socket_path: self.socket_path.to_string_lossy().into_owned(),
        }
    }

    /// Accepts connections and serves `read_bound` requests against `owner` until `owner`'s
    /// underlying process/task is dropped or the listener errors. Each accepted connection's
    /// pre-`Hello` phase runs in its own task, bounded by [`HELLO_TIMEOUT`] independently of every
    /// other connection — so any number of same-user processes that connect and never send `Hello`
    /// cannot serialize-starve a later, legitimate connection's own `Hello` read behind
    /// `HELLO_TIMEOUT` multiplied by however many came before it (jimmy's review of rhapsody#213,
    /// B3: five silent connections cost the real daemon `5 × HELLO_TIMEOUT` before its own `Hello`
    /// was even read, exceeding the daemon-side `RESPONSE_TIMEOUT`). Only an AUTHENTICATED
    /// connection ever contends for `serving_slot`, the single slot that actually answers
    /// `read_bound` requests — matching "only one connection is served at a time (the daemon holds
    /// exactly one)"; a second authenticated connection (e.g. the daemon reconnecting after its own
    /// restart) waits for the first's connection to end.
    ///
    /// Every per-connection task is tracked in a [`tokio::task::JoinSet`] owned by this call's own
    /// stack frame rather than fire-and-forgotten via a bare `tokio::spawn` (sol's review of
    /// rhapsody#213: an already-authenticated child task used to outlive this method being
    /// cancelled/dropped, keeping its `Arc<ProviderCredentialOwner>` and the launch token alive and
    /// still answering `read_bound` after the listener itself had shut down). Dropping a `JoinSet`
    /// aborts every task it still holds, so cancelling or dropping this future — the same thing the
    /// supervisor does to revoke a channel on stop/restart — now also tears down every connection
    /// spawned from it.
    pub async fn accept_and_serve(self, owner: Arc<ProviderCredentialOwner>) {
        let serving_slot = Arc::new(tokio::sync::Semaphore::new(1));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _addr) = match accepted {
                        Ok(pair) => pair,
                        Err(_) => return,
                    };
                    let owner = owner.clone();
                    let token = self.token.clone();
                    let serving_slot = serving_slot.clone();
                    connections.spawn(async move {
                        serve_one(stream, token, owner, serving_slot).await;
                    });
                }
                // Reap finished connections so `connections` doesn't grow without bound; the `if`
                // guard keeps this branch out of the poll set entirely while empty, rather than
                // resolving to `None` every iteration and busy-looping.
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }
}

async fn serve_one(
    mut stream: UnixStream,
    token: String,
    owner: Arc<ProviderCredentialOwner>,
    serving_slot: Arc<tokio::sync::Semaphore>,
) {
    let mut session = ServerSession::new(Token::new(token));

    let hello: HelloFrame = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream)).await
    {
        Ok(Ok(h)) => h,
        // A timed-out or errored/EOF'd Hello read are the same outcome here: give up on this
        // connection — its own task simply ends, never blocking any other connection's Hello phase
        // or the single serving slot.
        Ok(Err(_)) | Err(_) => return,
    };
    // An unauthorized connection gets no response at all — closing the stream, not answering with
    // an explicit rejection frame, so a probing caller learns nothing beyond "this didn't work".
    if session.accept_hello(&hello.token).is_err() {
        return;
    }

    // Only an authenticated connection reaches here, and only one at a time ever serves
    // `read_bound` requests. `acquire` only errors if the semaphore itself was closed, which never
    // happens here.
    let Ok(_permit) = serving_slot.acquire().await else {
        return;
    };

    loop {
        let frame: ClientFrame = match read_frame(&mut stream).await {
            Ok(f) => f,
            Err(_) => return,
        };
        let ClientFrame::ReadBound {
            seq,
            account,
            expected_binding,
        } = frame;
        if session.accept_client_seq(seq).is_err() {
            // Out-of-order/replayed: drop the connection rather than resync — a caller that lost
            // its place must reconnect (and re-authenticate) rather than being silently forgiven.
            return;
        }
        // Re-validate `account` (the wire form of a `CredentialRef`) rather than trusting whatever
        // the peer sent, even though only an already-authenticated peer reaches this line — AND
        // require it to name exactly the one credential `owner` is bound to. `owner` is
        // single-credential-scoped (see module doc), so without this check a syntactically valid
        // but WRONG account (e.g. a typo, or a future multi-provider client asking for a different
        // provider than this owner holds) would silently receive this owner's data instead of a
        // refusal.
        if CredentialRef::for_provider(strip_v1(&account)).is_err() || account != owner.account() {
            return;
        }
        let read = owner.read_bound(&expected_binding);
        let (state, lease) = split_state(read.state);
        let resp_seq = session.next_outgoing_seq();
        if write_frame(
            &mut stream,
            &ServerFrame::ReadBoundResult {
                seq: resp_seq,
                revision: read.revision,
                state,
                lease,
            },
        )
        .await
        .is_err()
        {
            return;
        }
    }
}

/// `CredentialRef::account()` is `v1:<id>`; recovers `<id>` to re-derive/validate it. Falls back to
/// the whole string (which will simply fail `for_provider`'s validation) if the prefix is absent.
fn strip_v1(account: &str) -> &str {
    account.strip_prefix("v1:").unwrap_or(account)
}

fn split_state(
    state: rhapsody_credential_ipc::domain::CredentialState,
) -> (
    rhapsody_credential_ipc::domain::CredentialStateTag,
    Option<rhapsody_credential_ipc::wire::LeasePayload>,
) {
    use rhapsody_credential_ipc::domain::CredentialState as S;
    let tag = state.tag();
    let lease = match state {
        S::Present(lease) => Some(rhapsody_credential_ipc::wire::LeasePayload {
            binding: lease.binding.clone(),
            value: lease.expose_secret().to_string(),
        }),
        _ => None,
    };
    (tag, lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::mock::MockKeyring;
    use rhapsody_credential_ipc::domain::{Binding, CredentialStateTag, Revision};
    use rhapsody_credential_ipc::session::ClientSession;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        // Unix socket paths are capped at ~104 bytes (`sun_path`). `std::env::temp_dir()` on macOS
        // resolves to a long per-session `$TMPDIR` (often 60-80 bytes on its own), so this uses
        // `/tmp` directly with a short name — the same reason `sign.sh`-style scripts elsewhere in
        // this crate avoid nesting deeply under it for socket/pipe paths.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from("/tmp").join(format!("rd-cb-{}-{n}", std::process::id() % 100_000));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn owner_with_secret() -> Arc<ProviderCredentialOwner> {
        let owner = ProviderCredentialOwner::for_test(MockKeyring::empty());
        owner
            .connect(
                Revision::INITIAL,
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
                "sk-real-socket-secret".into(),
            )
            .expect("connect");
        Arc::new(owner)
    }

    // The real end-to-end proof: an actual `UnixListener`/`UnixStream` pair, an actual
    // `ProviderCredentialOwner`, and the real client-side handshake/read_bound implementation
    // rhapsodyd boots with — not an in-memory duplex stand-in.
    #[tokio::test]
    async fn a_real_unix_socket_round_trips_an_authenticated_read_bound() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();

        let serve = tokio::spawn(listener.accept_and_serve(owner));

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;

        let read = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await
            .expect("read_bound over a real unix socket");

        assert_eq!(read.state.tag(), CredentialStateTag::Present);

        drop(client);
        serve.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_connection_presenting_the_wrong_token_gets_no_response() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let socket_path = listener.bootstrap_message().socket_path;
        let owner = owner_with_secret();
        let serve = tokio::spawn(listener.accept_and_serve(owner));

        let stream = UnixStream::connect(&socket_path).await.expect("connect");
        let mut client = rhapsodyd_test_client(stream, "totally-wrong-token".into()).await;

        let err = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await;
        assert!(
            err.is_err(),
            "an unauthorized connection must never get a real response"
        );

        serve.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    // `owner` is bound to exactly one credential; a syntactically valid but WRONG account must be
    // refused, not silently answered with this owner's data.
    #[tokio::test]
    async fn a_request_for_a_different_account_gets_no_response() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();
        let serve = tokio::spawn(listener.accept_and_serve(owner));

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;

        let err = client
            .read_bound(
                "v1:some-other-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await;
        assert!(
            err.is_err(),
            "a request for a different account must never get a real response"
        );

        serve.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    // A same-user process that connects and sends nothing (never a `Hello`) must not be able to
    // wedge the shared accept loop against a later, legitimate connection — including the real
    // daemon reconnecting after its own restart. Without `HELLO_TIMEOUT` bounding the first read in
    // `serve_one`, this test hangs forever instead of completing.
    #[tokio::test]
    async fn a_silent_connection_cannot_wedge_a_later_legitimate_client() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();
        let serve = tokio::spawn(listener.accept_and_serve(owner));

        // Connect but never write anything, and hold the stream open for the whole test — a
        // dropped stream would EOF immediately and prove nothing about the timeout.
        let silent = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect silent");

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect legit");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;
        let read = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await
            .expect("a legitimate client must still be served after the silent one times out");
        assert_eq!(read.state.tag(), CredentialStateTag::Present);

        drop(silent);
        drop(client);
        serve.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    // jimmy's review of rhapsody#213 (B3): a single silent connection being bounded by
    // `HELLO_TIMEOUT` is not enough if connections are still handled one at a time — N silent
    // connections queued ahead of the real daemon's own connection would then cost it
    // `N * HELLO_TIMEOUT` before its `Hello` is even read, which can exceed the daemon-side
    // `RESPONSE_TIMEOUT` for a large enough N. Five silent connections (jimmy's own reproduction
    // count), all opened and held open BEFORE the legitimate one connects, must not delay the
    // legitimate client's `read_bound` past the real client's own `RESPONSE_TIMEOUT`-equivalent
    // wait — proving each connection's pre-`Hello` phase is handled independently, not serially.
    #[tokio::test]
    async fn five_silent_connections_cannot_starve_a_later_legitimate_client() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();
        let serve = tokio::spawn(listener.accept_and_serve(owner));

        let mut silent = Vec::new();
        for _ in 0..5 {
            silent.push(
                UnixStream::connect(&msg.socket_path)
                    .await
                    .expect("connect silent"),
            );
        }

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect legit");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;
        let read = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await
            .expect(
                "a legitimate client must be served promptly regardless of how many silent \
                 connections were opened ahead of it",
            );
        assert_eq!(read.state.tag(), CredentialStateTag::Present);

        drop(silent);
        drop(client);
        serve.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    // sol's review of rhapsody#213: dropping/aborting the outer `accept_and_serve` task used to
    // leave an already-authenticated connection's own spawned task running, still holding the
    // owner and still answering `read_bound`. A second read over the SAME already-authenticated
    // client, issued only after the outer serve task has been awaited to completion following
    // `abort()`, must now fail instead of succeeding.
    #[tokio::test]
    async fn aborting_the_listener_task_drops_authenticated_child_connections() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();

        let serve = tokio::spawn(listener.accept_and_serve(owner));

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;

        let read = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await
            .expect("first read succeeds while the listener task is alive");
        assert_eq!(read.state.tag(), CredentialStateTag::Present);

        serve.abort();
        let _ = serve.await;

        let second = client
            .read_bound(
                "v1:spike-test-provider".into(),
                Binding {
                    provider_id: "spike-test-provider".into(),
                    adapter: "openai-chat-completions-bearer-v1".into(),
                    base_url: "https://api.example/v1".into(),
                },
            )
            .await;
        assert!(
            second.is_err(),
            "an authenticated child task must not retain the credential owner after the listener task shuts down"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // A minimal client double built directly on the wire/session primitives (rather than importing
    // the daemon crate, which this crate does not and should not depend on) — exercises the exact
    // same `ClientSession`/framing the real `rhapsodyd` client uses.
    struct TestClient {
        stream: UnixStream,
        session: ClientSession,
    }

    async fn rhapsodyd_test_client(stream: UnixStream, token: String) -> TestClient {
        let session = ClientSession::new(Token::new(token));
        let mut stream = stream;
        write_frame(
            &mut stream,
            &HelloFrame {
                token: session.hello_token().to_string(),
            },
        )
        .await
        .expect("write hello");
        TestClient { stream, session }
    }

    impl TestClient {
        async fn read_bound(
            &mut self,
            account: String,
            expected_binding: Binding,
        ) -> Result<rhapsody_credential_ipc::domain::CredentialRead, ()> {
            let seq = self.session.next_outgoing_seq();
            write_frame(
                &mut self.stream,
                &ClientFrame::ReadBound {
                    seq,
                    account,
                    expected_binding,
                },
            )
            .await
            .map_err(|_| ())?;
            // Mirrors the real daemon client's own `RESPONSE_TIMEOUT` (`crates/rhapsodyd/src/
            // credential_client.rs`) rather than using a more generous value — a test double whose
            // wait is longer than the real client's would let a fix pass here while the real client
            // still gave up too soon against the exact same server.
            let frame: ServerFrame = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                read_frame(&mut self.stream),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            match frame {
                ServerFrame::ReadBoundResult {
                    revision,
                    state,
                    lease,
                    ..
                } => Ok(rhapsody_credential_ipc::domain::CredentialRead {
                    revision,
                    state: match (state, lease) {
                        (CredentialStateTag::Present, Some(l)) => {
                            rhapsody_credential_ipc::domain::CredentialState::Present(
                                rhapsody_credential_ipc::domain::BoundCredentialLease::new(
                                    l.binding, l.value,
                                ),
                            )
                        }
                        (CredentialStateTag::Absent, _) => {
                            rhapsody_credential_ipc::domain::CredentialState::Absent
                        }
                        _ => rhapsody_credential_ipc::domain::CredentialState::Malformed,
                    },
                }),
                ServerFrame::RevisionChanged { .. } => Err(()),
            }
        }
    }
}
