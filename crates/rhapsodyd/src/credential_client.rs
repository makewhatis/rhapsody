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

use rhapsody_credential_ipc::domain::{Binding, CredentialRead, CredentialState};
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

#[derive(Debug)]
pub enum ClientError {
    Frame(FrameError),
    Session(SessionError),
    Io(std::io::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Frame(e) => write!(f, "{e}"),
            ClientError::Session(e) => write!(f, "{e}"),
            ClientError::Io(e) => write!(f, "{e}"),
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
        Ok(CredentialClient { stream, session })
    }

    /// Requests `read_bound` for `account` against `expected_binding` and awaits the reply.
    pub async fn read_bound(
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
            let frame: ServerFrame = read_frame(&mut self.stream)
                .await
                .map_err(ClientError::Frame)?;
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-secret"),
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
}
