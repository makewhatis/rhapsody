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
    /// underlying process/task is dropped or the listener errors. Only one connection is served at a
    /// time (the daemon holds exactly one). A connection that never completes its `Hello` handshake
    /// within [`HELLO_TIMEOUT`] is dropped so it cannot hold this slot against a later connection —
    /// including the real daemon reconnecting after its own restart — forever.
    pub async fn accept_and_serve(self, owner: Arc<ProviderCredentialOwner>) {
        loop {
            let (stream, _addr) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let owner = owner.clone();
            let token = self.token.clone();
            // Deliberately awaited in this same loop (not `tokio::spawn`ed per-connection): exactly
            // one credential-bearing connection is ever meant to be active, so a second accept only
            // proceeds once the previous connection has ended (e.g. the daemon reconnecting after
            // its own restart), never two live connections racing the same owner concurrently.
            serve_one(stream, token, owner).await;
        }
    }
}

async fn serve_one(mut stream: UnixStream, token: String, owner: Arc<ProviderCredentialOwner>) {
    let mut session = ServerSession::new(Token::new(token));

    let hello: HelloFrame = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream)).await
    {
        Ok(Ok(h)) => h,
        // A timed-out or errored/EOF'd Hello read are the same outcome here: give up on this
        // connection and let `accept_and_serve` move on to the next one rather than blocking it.
        Ok(Err(_)) | Err(_) => return,
    };
    // An unauthorized connection gets no response at all — closing the stream, not answering with
    // an explicit rejection frame, so a probing caller learns nothing beyond "this didn't work".
    if session.accept_hello(&hello.token).is_err() {
        return;
    }

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
            // Comfortably above `HELLO_TIMEOUT` so a test that first parks a silent connection (to
            // prove it cannot wedge a later legitimate one) never races its own client-side wait
            // against the server-side timeout that frees the slot this client needs.
            let frame: ServerFrame = tokio::time::timeout(
                std::time::Duration::from_secs(3),
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
