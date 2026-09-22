//! The daemon-side half of the authenticated desktop-to-daemon provider-credential channel P0c
//! selected (STUDIO-981; see `rhapsody_credential_ipc`'s crate doc for why direct daemon Keychain
//! access was rejected). No Go parity — Rhapsody-only.
//!
//! This module is, by construction, the daemon's ONLY possible path to a provider credential: it
//! never touches the OS Keychain itself, only a Unix socket named by a one-shot bootstrap frame the
//! desktop supervisor writes to this process's stdin immediately after spawning it. A `rhapsodyd`
//! invocation that never receives that frame — a bare CLI run, or a confused-deputy process that
//! launches the same signed binary directly, bypassing the real supervisor — has no way to reach
//! this module's socket at all and simply runs with no credential owner. There is nothing to fall
//! back to and nothing else to try; that absence IS the confused-deputy defense.

use std::time::Duration;

use rhapsody_credential_ipc::domain::{Binding, CredentialRead, CredentialState, Revision};
use rhapsody_credential_ipc::session::{ClientSession, SessionError, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, FrameError, HelloFrame, LeasePayload, ServerFrame, read_frame,
    write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

/// How long the daemon waits for the desktop's one bootstrap frame on stdin before concluding this
/// launch has no credential owner. Bounds how long a confused-deputy invocation (which never sends
/// this frame) blocks before the rest of boot proceeds; a legitimate supervisor-spawned daemon
/// receives it within milliseconds of process creation.
pub const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `read_bound` waits for the owner's response before giving up. Without this bound, an
/// owner that accepted `Hello` but is itself wedged (e.g. blocked inside its own credential-owner
/// mutation lock) hangs the calling daemon task forever instead of producing `OwnerUnavailable` —
/// design §2.5 requires "blocking reads are concurrency/timeout bounded".
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum ClientError {
    Frame(FrameError),
    Session(SessionError),
    Io(std::io::Error),
    /// The owner accepted `Hello` but never answered a request within [`RESPONSE_TIMEOUT`].
    Timeout,
    /// A prior `read_bound` call on this client already failed; see the `poisoned` field's doc on
    /// [`CredentialClient`]. The caller must open a new connection instead of retrying this one.
    Poisoned,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Frame(e) => write!(f, "{e}"),
            ClientError::Session(e) => write!(f, "{e}"),
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Timeout => write!(f, "timed out waiting for the owner's response"),
            ClientError::Poisoned => {
                write!(
                    f,
                    "client is poisoned by a prior failure; open a new connection"
                )
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// Reads the one bootstrap frame from `stdin`, bounded by [`BOOTSTRAP_TIMEOUT`]. `None` — an EOF, a
/// malformed frame, or a timed-out read — all mean the same thing: no credential owner for this
/// process's lifetime. That is an entirely ordinary, fully supported way to run `rhapsodyd` (a bare
/// CLI invocation, a workflow with no provider feature configured), not an error.
pub async fn read_bootstrap<R>(mut stdin: R) -> Option<BootstrapMessage>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(BOOTSTRAP_TIMEOUT, read_frame(&mut stdin))
        .await
        .ok()?
        .ok()
}

/// One authenticated connection to the desktop's credential owner. The only constructors are
/// [`CredentialClient::connect`] (real `UnixStream`) and, for tests, [`CredentialClient::handshake`]
/// over any duplex stream — both require a bootstrap token to have already been obtained via
/// [`read_bootstrap`], so a process that never received one can never build one of these.
pub struct CredentialClient<S> {
    stream: S,
    session: ClientSession,
    /// Set on any `read_bound` failure, including [`ClientError::Timeout`]. `ReadBoundResult`
    /// carries no request-identifying data beyond the server's own independent sequence, so a
    /// client that gave up waiting and was then reused for a new request could have a late reply to
    /// the abandoned request arrive and be accepted as the answer to the new one — `accept_server_
    /// seq` only checks strict ordering, not which logical request a reply belongs to. `resolve_
    /// credential` never reuses a client across calls today, so this cannot happen yet, but
    /// `CredentialClient` is `pub` and PB7 is expected to hold one open across many requests.
    poisoned: bool,
}

impl CredentialClient<UnixStream> {
    /// Connects to the bootstrap message's socket and completes the Hello handshake.
    pub async fn connect(
        msg: &BootstrapMessage,
    ) -> Result<CredentialClient<UnixStream>, ClientError> {
        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .map_err(ClientError::Io)?;
        CredentialClient::handshake(stream, msg.token.clone()).await
    }
}

impl<S> CredentialClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub async fn handshake(
        mut stream: S,
        token: String,
    ) -> Result<CredentialClient<S>, ClientError> {
        let session = ClientSession::new(Token::new(token));
        write_frame(
            &mut stream,
            &HelloFrame {
                token: session.hello_token().to_string(),
            },
        )
        .await
        .map_err(ClientError::Frame)?;
        Ok(CredentialClient {
            stream,
            session,
            poisoned: false,
        })
    }

    /// Requests `read_bound` for `account` against `expected_binding` and awaits the reply. Once
    /// any call to this method fails, this client is permanently poisoned (see the `poisoned`
    /// field's doc) and every subsequent call fails fast with [`ClientError::Poisoned`] without
    /// touching the stream — the caller must open a new connection rather than retry this one.
    pub async fn read_bound(
        &mut self,
        account: String,
        expected_binding: Binding,
    ) -> Result<CredentialRead, ClientError> {
        if self.poisoned {
            return Err(ClientError::Poisoned);
        }
        match self.read_bound_inner(account, expected_binding).await {
            Ok(read) => Ok(read),
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    async fn read_bound_inner(
        &mut self,
        account: String,
        expected_binding: Binding,
    ) -> Result<CredentialRead, ClientError> {
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
        .map_err(ClientError::Frame)?;

        loop {
            let frame: ServerFrame =
                match tokio::time::timeout(RESPONSE_TIMEOUT, read_frame(&mut self.stream)).await {
                    Ok(r) => r.map_err(ClientError::Frame)?,
                    Err(_) => return Err(ClientError::Timeout),
                };
            match frame {
                ServerFrame::ReadBoundResult {
                    seq: resp_seq,
                    revision,
                    state,
                    lease,
                } => {
                    self.session
                        .accept_server_seq(resp_seq)
                        .map_err(ClientError::Session)?;
                    return Ok(CredentialRead {
                        revision,
                        state: to_domain_state(state, lease),
                    });
                }
                // An unsolicited revision push that arrives ahead of our response is legitimate
                // (the owner mutated between our request and its reply) — accept its sequence and
                // keep waiting for the actual response rather than treating it as an error.
                ServerFrame::RevisionChanged { seq: resp_seq, .. } => {
                    self.session
                        .accept_server_seq(resp_seq)
                        .map_err(ClientError::Session)?;
                }
            }
        }
    }
}

/// Composes bootstrap + connect + one `read_bound` call into a single `CredentialRead`, filling in
/// the two states that only make sense at this daemon-wide level (design §2.5): `OwnerUnavailable`
/// when there is no way to even reach an owner (no bootstrap frame ever arrived, or the socket
/// connect itself failed) and `OwnerUnauthorized` when a connection WAS established and `Hello` was
/// sent, but the owner closed it without ever answering — the server's deliberate no-oracle
/// response to a wrong token (see `credential_bootstrap::serve_one` in the desktop crate).
///
/// The `revision` on both of those synthesized states is a fixed placeholder, not a real tracked
/// generation: making it a genuine, transition-observing counter needs the stateful
/// preparation/refusal-gate machinery PB7 owns, which is explicitly out of P0c's scope. A caller
/// must not treat two `OwnerUnavailable` reads from this function as comparable revisions.
pub async fn resolve_credential<R>(
    stdin: R,
    account: String,
    expected_binding: Binding,
) -> CredentialRead
where
    R: AsyncRead + Unpin,
{
    let unavailable = || CredentialRead {
        revision: Revision::INITIAL,
        state: CredentialState::OwnerUnavailable,
    };

    let Some(msg) = read_bootstrap(stdin).await else {
        return unavailable();
    };
    let mut client = match CredentialClient::connect(&msg).await {
        Ok(c) => c,
        Err(_) => return unavailable(),
    };
    match client.read_bound(account, expected_binding).await {
        Ok(read) => read,
        // A clean EOF here is the ONLY signal a rejected `Hello` ever produces (the server closes
        // silently rather than answering — see `credential_bootstrap::serve_one`), so this is the
        // closest available approximation of `OwnerUnauthorized`. It is not perfectly precise: an
        // owner that authenticated us and then crashed/closed before answering this exact request
        // produces the identical EOF and would also be classified `OwnerUnauthorized` rather than
        // `OwnerUnavailable`. Disambiguating the two needs the server to distinguish them on the
        // wire, which the deliberate no-oracle rejection design does not do; accepted here as a
        // P0c-scope approximation rather than population of a state PB7 depends on for anything
        // safety-critical (both states already forbid any credential use).
        Err(ClientError::Frame(FrameError::Eof)) => CredentialRead {
            revision: Revision::INITIAL,
            state: CredentialState::OwnerUnauthorized,
        },
        // A `Hello`-accepted owner that never answers within `RESPONSE_TIMEOUT` (wedged, or simply
        // too slow) is unavailable, not unauthorized — the connection itself was never rejected.
        // Every other connect/frame failure is unavailable too.
        Err(_) => unavailable(),
    }
}

fn to_domain_state(
    tag: rhapsody_credential_ipc::domain::CredentialStateTag,
    lease: Option<LeasePayload>,
) -> CredentialState {
    use rhapsody_credential_ipc::domain::CredentialStateTag as Tag;
    match (tag, lease) {
        (Tag::Present, Some(l)) => CredentialState::Present(
            rhapsody_credential_ipc::domain::BoundCredentialLease::new(l.binding, l.value),
        ),
        // A `Present` tag with no lease payload is a protocol violation from a well-behaved server
        // (it should never happen), not a value a caller could act on safely — fail closed to
        // `Malformed` rather than fabricating an empty credential.
        (Tag::Present, None) => CredentialState::Malformed,
        (Tag::Absent, _) => CredentialState::Absent,
        (Tag::DeniedOrLocked, _) => CredentialState::DeniedOrLocked,
        (Tag::Malformed, _) => CredentialState::Malformed,
        (Tag::BindingMismatch, _) => CredentialState::BindingMismatch,
        (Tag::OwnerUnavailable, _) => CredentialState::OwnerUnavailable,
        (Tag::OwnerUnauthorized, _) => CredentialState::OwnerUnauthorized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_credential_ipc::domain::{CredentialStateTag, Revision};
    use rhapsody_credential_ipc::session::ServerSession;
    use tokio::io::duplex;

    fn a_binding() -> Binding {
        Binding {
            provider_id: "p".into(),
            adapter: "a".into(),
            base_url: "https://example".into(),
        }
    }

    #[tokio::test]
    async fn read_bootstrap_returns_none_on_immediate_eof() {
        // An empty, already-closed "stdin" is exactly what a confused-deputy direct launch (or a
        // bare CLI invocation with /dev/null on stdin) presents.
        let empty: &[u8] = &[];
        let got = read_bootstrap(empty).await;
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn read_bootstrap_returns_none_on_garbage_input() {
        let garbage: &[u8] = b"not a valid frame at all, no length prefix logic applies";
        // Garbage bytes will likely decode as SOME length prefix and then fail to parse as JSON, or
        // hit EOF reading the declared body — either way, `None`.
        let got = read_bootstrap(garbage).await;
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn read_bootstrap_parses_a_real_frame() {
        let (mut tx, rx) = duplex(4096);
        write_frame(
            &mut tx,
            &BootstrapMessage {
                token: "tok".into(),
                socket_path: "/tmp/rhapsody-test.sock".into(),
            },
        )
        .await
        .unwrap();
        drop(tx);
        let got = read_bootstrap(rx).await.expect("frame parsed");
        assert_eq!(got.token, "tok");
        assert_eq!(got.socket_path, "/tmp/rhapsody-test.sock");
    }

    // End-to-end over an in-memory duplex, driving BOTH the real client (`CredentialClient`) and a
    // minimal hand-rolled server loop using the real `ServerSession` — proves the client's framing,
    // handshake, and sequence handling interoperate with the exact session state machine the real
    // desktop-side server uses (a real `UnixStream` adds nothing to this logic; it is exercised
    // separately in the desktop crate's socket-level tests).
    #[tokio::test]
    async fn read_bound_round_trips_through_a_real_handshake_and_present_state() {
        let (client_io, server_io) = duplex(8192);
        let expected_binding = a_binding();
        let server_binding = expected_binding.clone();

        let server = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(server_io);
            let mut session = ServerSession::new(Token::new("secret-token".into()));
            let hello: HelloFrame = read_frame(&mut r).await.unwrap();
            session.accept_hello(&hello.token).expect("hello accepted");

            let ClientFrame::ReadBound {
                seq,
                account,
                expected_binding: got_binding,
            } = read_frame(&mut r).await.unwrap();
            session.accept_client_seq(seq).expect("seq accepted");
            assert_eq!(account, "v1:spike-test-provider");
            assert_eq!(got_binding, server_binding);

            let resp_seq = session.next_outgoing_seq();
            write_frame(
                &mut w,
                &ServerFrame::ReadBoundResult {
                    seq: resp_seq,
                    revision: Revision(3),
                    state: CredentialStateTag::Present,
                    lease: Some(LeasePayload {
                        binding: server_binding,
                        value: "sk-secret".into(),
                    }),
                },
            )
            .await
            .unwrap();
        });

        let mut client = CredentialClient::handshake(client_io, "secret-token".into())
            .await
            .expect("handshake");
        let read = client
            .read_bound("v1:spike-test-provider".into(), expected_binding)
            .await
            .expect("read_bound");
        assert_eq!(read.revision, Revision(3));
        match read.state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-secret")
            }
            other => panic!("expected Present, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_bound_reports_unauthorized_as_a_session_error_when_the_server_rejects_hello() {
        let (client_io, server_io) = duplex(8192);
        let server = tokio::spawn(async move {
            let (mut r, _w) = tokio::io::split(server_io);
            let mut session = ServerSession::new(Token::new("expected-token".into()));
            let hello: HelloFrame = read_frame(&mut r).await.unwrap();
            // The server's real behavior on a bad token: reject and say nothing further, dropping
            // the connection. We just assert the rejection is observed server-side here; the
            // client-visible effect (an EOF/IO error on its next read) is exercised implicitly by
            // never sending a response.
            assert!(session.accept_hello(&hello.token).is_err());
        });

        // The client presents the WRONG token; a real server would silently close instead of
        // answering — from the client's point of view that surfaces as an IO/frame error on the
        // next read, not as a distinguishable "Unauthorized" wire message (the design deliberately
        // never echoes anything about a failed auth attempt back to the caller).
        let mut client = CredentialClient::handshake(client_io, "wrong-token".into())
            .await
            .expect("handshake always sends Hello; rejection is a server-side decision");
        let err = client
            .read_bound("v1:x".into(), a_binding())
            .await
            .expect_err("no response ever arrives for an unauthorized connection");
        assert!(matches!(err, ClientError::Frame(_)));
        server.await.unwrap();
    }

    // N3 (jimmy's review of rhapsody#213): a `CredentialClient` that gave up waiting for a response
    // must not be reusable — a late reply to the abandoned request could otherwise be accepted as
    // the answer to a NEW request on the same client, since `ReadBoundResult` carries only the
    // server's own independent sequence, not any per-request identifier `accept_server_seq` could
    // use to reject a stale match. Once `read_bound` fails for any reason, every subsequent call
    // must fail fast with `Poisoned` instead of touching the stream.
    #[tokio::test]
    async fn a_client_is_poisoned_after_any_read_bound_failure_and_refuses_reuse() {
        let (client_io, server_io) = duplex(8192);
        // The server accepts Hello and then never answers anything, ever — simulating exactly the
        // "wedged after auth" scenario RESPONSE_TIMEOUT exists to bound.
        let server = tokio::spawn(async move {
            let (mut r, _w) = tokio::io::split(server_io);
            let mut session = ServerSession::new(Token::new("secret".into()));
            let hello: HelloFrame = read_frame(&mut r).await.unwrap();
            session.accept_hello(&hello.token).expect("hello accepted");
            // Never read or answer the follow-up ReadBound; hold the connection open.
            tokio::time::sleep(RESPONSE_TIMEOUT * 3).await;
        });

        let mut client = CredentialClient::handshake(client_io, "secret".into())
            .await
            .expect("handshake");

        let first = client.read_bound("v1:x".into(), a_binding()).await;
        assert!(matches!(first, Err(ClientError::Timeout)));

        // A second call on the SAME client must refuse outright, not attempt another write/read
        // that could race the first request's still-possibly-arriving late reply.
        let second = client.read_bound("v1:y".into(), a_binding()).await;
        assert!(
            matches!(second, Err(ClientError::Poisoned)),
            "a client must refuse reuse after any read_bound failure, got {second:?}"
        );

        server.abort();
    }

    // --- resolve_credential: the daemon-wide OwnerUnavailable/OwnerUnauthorized states ------------

    fn unix_socket_path(name: &str) -> std::path::PathBuf {
        // Unix socket paths are capped at ~104 bytes (`sun_path`); `/tmp` directly with a short
        // name keeps well under that regardless of the ambient `$TMPDIR`.
        std::path::PathBuf::from("/tmp").join(format!("rd-cc-{name}-{}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn resolve_credential_reports_owner_unavailable_with_no_bootstrap_frame() {
        let empty: &[u8] = &[];
        let read = resolve_credential(empty, "v1:x".into(), a_binding()).await;
        assert_eq!(read.state.tag(), CredentialStateTag::OwnerUnavailable);
    }

    #[tokio::test]
    async fn resolve_credential_reports_owner_unavailable_when_the_socket_connect_fails() {
        let (mut tx, rx) = duplex(4096);
        write_frame(
            &mut tx,
            &BootstrapMessage {
                token: "tok".into(),
                socket_path: "/tmp/rd-cc-definitely-does-not-exist.sock".into(),
            },
        )
        .await
        .unwrap();
        drop(tx);
        let read = resolve_credential(rx, "v1:x".into(), a_binding()).await;
        assert_eq!(read.state.tag(), CredentialStateTag::OwnerUnavailable);
    }

    #[tokio::test]
    async fn resolve_credential_reports_owner_unauthorized_when_the_real_owner_rejects_the_token() {
        let path = unix_socket_path("unauth");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind real socket");

        let accept = tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            let (mut r, _w) = tokio::io::split(stream);
            let mut session = ServerSession::new(Token::new("expected-token".into()));
            let hello: HelloFrame = read_frame(&mut r).await.expect("read hello");
            // Wrong token: reject and close, exactly as the real desktop server does.
            assert!(session.accept_hello(&hello.token).is_err());
        });

        let (mut tx, rx) = duplex(4096);
        write_frame(
            &mut tx,
            &BootstrapMessage {
                token: "wrong-token".into(),
                socket_path: path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        drop(tx);

        let read = resolve_credential(rx, "v1:x".into(), a_binding()).await;
        assert_eq!(read.state.tag(), CredentialStateTag::OwnerUnauthorized);

        accept.await.unwrap();
        std::fs::remove_file(&path).ok();
    }

    // A wedged owner (accepted `Hello`, then never answers) must not hang the calling daemon task
    // forever — it must time out and report `OwnerUnavailable` within a bounded wait. Without
    // `RESPONSE_TIMEOUT` bounding `read_bound`'s response read, this test never completes.
    #[tokio::test]
    async fn resolve_credential_reports_owner_unavailable_when_the_owner_never_answers() {
        let path = unix_socket_path("wedged");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind real socket");

        let accept = tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            let (mut r, _w) = tokio::io::split(stream);
            let mut session = ServerSession::new(Token::new("secret".into()));
            let hello: HelloFrame = read_frame(&mut r).await.unwrap();
            session.accept_hello(&hello.token).expect("hello accepted");
            // Authenticated, then deliberately never reads or answers the follow-up ReadBound —
            // simulating an owner wedged after a successful handshake. Hold `_w` for the test's
            // whole run so the connection stays open rather than EOFing.
            tokio::time::sleep(RESPONSE_TIMEOUT * 3).await;
        });

        let (mut tx, rx) = duplex(4096);
        write_frame(
            &mut tx,
            &BootstrapMessage {
                token: "secret".into(),
                socket_path: path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        drop(tx);

        let started = std::time::Instant::now();
        let read = resolve_credential(rx, "v1:x".into(), a_binding()).await;
        assert_eq!(read.state.tag(), CredentialStateTag::OwnerUnavailable);
        assert!(
            started.elapsed() < RESPONSE_TIMEOUT * 2,
            "resolve_credential must return once RESPONSE_TIMEOUT elapses, not wait for the owner"
        );

        accept.abort();
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn resolve_credential_passes_through_a_real_present_read() {
        let path = unix_socket_path("present");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind real socket");
        let binding = a_binding();
        let server_binding = binding.clone();

        let accept = tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            let (mut r, mut w) = tokio::io::split(stream);
            let mut session = ServerSession::new(Token::new("secret".into()));
            let hello: HelloFrame = read_frame(&mut r).await.unwrap();
            session.accept_hello(&hello.token).expect("hello accepted");
            let ClientFrame::ReadBound { seq, .. } = read_frame(&mut r).await.unwrap();
            session.accept_client_seq(seq).unwrap();
            let resp_seq = session.next_outgoing_seq();
            write_frame(
                &mut w,
                &ServerFrame::ReadBoundResult {
                    seq: resp_seq,
                    revision: Revision(9),
                    state: CredentialStateTag::Present,
                    lease: Some(LeasePayload {
                        binding: server_binding,
                        value: "sk-resolved".into(),
                    }),
                },
            )
            .await
            .unwrap();
        });

        let (mut tx, rx) = duplex(4096);
        write_frame(
            &mut tx,
            &BootstrapMessage {
                token: "secret".into(),
                socket_path: path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        drop(tx);

        let read = resolve_credential(rx, "v1:spike-test-provider".into(), binding).await;
        assert_eq!(read.revision, Revision(9));
        match read.state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-resolved")
            }
            other => panic!("expected Present, got {other:?}"),
        }

        accept.await.unwrap();
        std::fs::remove_file(&path).ok();
    }
}
