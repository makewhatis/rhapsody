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

use std::collections::HashMap;
use std::time::Duration;

use rhapsody_credential_ipc::domain::{Binding, CredentialRead, CredentialState, Revision};
use rhapsody_credential_ipc::session::{ClientSession, SessionError, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, FrameError, HelloFrame, LeasePayload, ServerFrame, read_frame,
    write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::OnceCell;

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

/// The daemon's observation of owner availability across successive resolutions (design §2.5). The
/// owner's own revision is only visible when the owner answers, but the daemon-wide
/// `OwnerUnavailable`/`OwnerUnauthorized` outcomes have no owner revision to carry — and the ticket
/// makes it mandatory that EVERY daemon read carries a revision that ADVANCES on an availability/
/// authorization transition, so a refusal gate keyed on `(key, fingerprint, credential revision)`
/// re-arms the moment the owner goes away or comes back.
///
/// It folds each raw read into a per-credential state held under one lock, so a caller can never
/// observe the revision of one transition paired with the state of another. The revision it returns
/// is deliberately NOT one uniform counter:
///
/// * When the owner **answers**, its own revision passes through **untouched**. That value is the
///   expected owner CAS revision, so manufacturing a larger synthetic number here would make every
///   later Connect/Replace/Rebind/Remove fail with `StaleRevision` against the real owner (alice's
///   correction to her own round-2 suggestion, and sol's point 2, on rhapsody#221).
/// * When no owner answers (`OwnerUnavailable`/`OwnerUnauthorized`), there is no owner revision to
///   carry, so a separate daemon generation stands in and advances on each availability/
///   authorization transition. That re-arms the gate on an owner that goes away or comes back, and
///   it can never collide with a CAS because those states are never passed to a mutation.
///
/// The state is keyed by credential account, so one owner's transitions cannot mask another's, and
/// a steady-state repeat never manufactures a revision. It is NOT the stateful preparation/refusal
/// machinery PB7 owns — it is the minimal revision source that machinery needs, and it lives here
/// because only this daemon-side adapter can observe the channel's availability at all.
#[derive(Debug)]
pub struct CredentialResolver {
    state: std::sync::Mutex<HashMap<String, AvailabilityState>>,
    /// The one bootstrap frame, learned from stdin on the first resolution and reused by every later
    /// one. Caching it — rather than re-reading stdin per call, which is impossible anyway — is what
    /// makes this a boundary that spans successive reads: a fresh tracker built per call could never
    /// observe an availability transition. `None` inside the cell means a frame WAS read but is
    /// unusable/absent (EOF/malformed/timeout), a stable state that needs no re-read.
    channel: OnceCell<Option<BootstrapMessage>>,
}

impl Default for CredentialResolver {
    fn default() -> CredentialResolver {
        CredentialResolver {
            state: std::sync::Mutex::new(HashMap::new()),
            channel: OnceCell::new(),
        }
    }
}

/// How a single resolution reached (or failed to reach) the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reachability {
    /// The owner answered, so its own revision is authoritative for this read.
    Answered,
    /// No channel to an owner at all (no bootstrap frame, connect failure, or a wedged owner).
    Unavailable,
    /// A connection authenticated an owner that then rejected us.
    Unauthorized,
}

/// One credential account's availability state. `generation` is the daemon-visible revision used
/// only for reads the owner did not answer; `last` is the previous reachability class, so the next
/// class change is a transition.
#[derive(Debug)]
struct AvailabilityState {
    generation: Revision,
    last: Option<Reachability>,
}

impl Default for AvailabilityState {
    fn default() -> AvailabilityState {
        AvailabilityState {
            generation: Revision::INITIAL,
            last: None,
        }
    }
}

impl CredentialResolver {
    pub fn new() -> CredentialResolver {
        CredentialResolver::default()
    }

    /// Reads and remembers the one bootstrap frame from `stdin`. Idempotent: only the first call
    /// consumes `stdin`; every later call is a no-op. A process that never receives a frame simply
    /// resolves `OwnerUnavailable` for its whole lifetime.
    pub async fn learn_bootstrap<R>(&self, stdin: R)
    where
        R: AsyncRead + Unpin,
    {
        let _ = self
            .channel
            .get_or_init(|| async { read_bootstrap(stdin).await })
            .await;
    }

    /// Reads one credential through the learned channel and folds the result into this resolver.
    /// Callers route EVERY read for a credential through the same resolver, which is what lets
    /// successive reads observe an availability transition. Not `resolve(stdin, ..)`: the bootstrap
    /// frame is consumed exactly once by [`learn_bootstrap`], never per read.
    pub async fn read_bound(&self, account: String, expected_binding: Binding) -> CredentialRead {
        let read = match self.channel.get() {
            Some(Some(msg)) => read_over_channel(msg, account.clone(), expected_binding).await,
            // No bootstrap frame ever arrived (or it was malformed/timed out): no owner for this
            // process's lifetime.
            Some(None) | None => unavailable_read(),
        };
        self.observe(&account, read)
    }

    /// Folds one raw read into the tracked state for `account`. The class and generation are read and
    /// written under one lock, so a caller can never observe the revision of one transition paired
    /// with the class of another.
    fn observe(&self, account: &str, read: CredentialRead) -> CredentialRead {
        let class = match read.state {
            CredentialState::OwnerUnavailable => Reachability::Unavailable,
            CredentialState::OwnerUnauthorized => Reachability::Unauthorized,
            _ => Reachability::Answered,
        };
        let revision = {
            let mut states = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(account.to_string()).or_default();
            if matches!(state.last, Some(prev) if prev != class) {
                state.generation = state.generation.next();
            }
            state.last = Some(class);
            match class {
                // The owner's revision is the expected owner CAS revision: pass it through exactly.
                Reachability::Answered => read.revision,
                // No owner revision exists; the daemon generation represents the transition.
                Reachability::Unavailable | Reachability::Unauthorized => state.generation,
            }
        };
        CredentialRead {
            revision,
            state: read.state,
        }
    }
}

/// The read outcome when no owner can be reached at all (no frame, connect failure, or a wedged
/// owner). The revision is a placeholder that [`CredentialResolver::observe`] replaces with the
/// tracked generation.
fn unavailable_read() -> CredentialRead {
    CredentialRead {
        revision: Revision::INITIAL,
        state: CredentialState::OwnerUnavailable,
    }
}

/// Connects to an already-learned channel and makes one `read_bound` call. The returned revision is
/// the OWNER's; the caller folds it through [`CredentialResolver::observe`], which passes it through
/// untouched when the owner answered. Kept private so no caller can observe an unfurled read.
async fn read_over_channel(
    msg: &BootstrapMessage,
    account: String,
    expected_binding: Binding,
) -> CredentialRead {
    let mut client = match CredentialClient::connect(msg).await {
        Ok(c) => c,
        Err(_) => return unavailable_read(),
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
        Err(_) => unavailable_read(),
    }
}

fn to_domain_state(
    tag: rhapsody_credential_ipc::domain::CredentialStateTag,
    lease: Option<LeasePayload>,
) -> CredentialState {
    use rhapsody_credential_ipc::domain::CredentialStateTag as Tag;
    match (tag, lease) {
        (Tag::Present, Some(mut l)) => {
            CredentialState::Present(rhapsody_credential_ipc::domain::BoundCredentialLease::new(
                l.binding.clone(),
                std::mem::take(&mut l.value),
            ))
        }
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
                assert_eq!(lease.into_lease_payload().value, "sk-secret")
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

    // --- CredentialResolver: the daemon-wide OwnerUnavailable/OwnerUnauthorized states -------------

    fn unix_socket_path(name: &str) -> std::path::PathBuf {
        // Unix socket paths are capped at ~104 bytes (`sun_path`); `/tmp` directly with a short
        // name keeps well under that regardless of the ambient `$TMPDIR`.
        std::path::PathBuf::from("/tmp").join(format!("rd-cc-{name}-{}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn resolve_credential_reports_owner_unavailable_with_no_bootstrap_frame() {
        let empty: &[u8] = &[];
        let resolver = CredentialResolver::new();
        resolver.learn_bootstrap(empty).await;
        let read = resolver.read_bound("v1:x".into(), a_binding()).await;
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
        let resolver = CredentialResolver::new();
        resolver.learn_bootstrap(rx).await;
        let read = resolver.read_bound("v1:x".into(), a_binding()).await;
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

        let resolver = CredentialResolver::new();
        resolver.learn_bootstrap(rx).await;
        let read = resolver.read_bound("v1:x".into(), a_binding()).await;
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
        let resolver = CredentialResolver::new();
        resolver.learn_bootstrap(rx).await;
        let read = resolver.read_bound("v1:x".into(), a_binding()).await;
        assert_eq!(read.state.tag(), CredentialStateTag::OwnerUnavailable);
        assert!(
            started.elapsed() < RESPONSE_TIMEOUT * 2,
            "resolve must return once RESPONSE_TIMEOUT elapses, not wait for the owner"
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

        let resolver = CredentialResolver::new();
        resolver.learn_bootstrap(rx).await;
        let read = resolver
            .read_bound("v1:spike-test-provider".into(), binding)
            .await;
        assert_eq!(read.revision, Revision(9));
        match read.state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.into_lease_payload().value, "sk-resolved")
            }
            other => panic!("expected Present, got {other:?}"),
        }

        accept.await.unwrap();
        std::fs::remove_file(&path).ok();
    }

    // §2.5 revision observation, on the real daemon path (the channel is learned once and reused by
    // every later read). The daemon-wide OwnerUnavailable/OwnerUnauthorized outcomes carry the
    // tracked generation, and it ADVANCES on an availability transition — which is what re-arms a
    // refusal gate keyed on it. This drives ONE resolver through two reads; if the tracker were
    // rebuilt per read (the defect sol found at rhapsody#221), the second read's fresh generation
    // would start at zero and the assertion below would fail.
    #[tokio::test]
    async fn availability_transitions_advance_the_resolver_revision() {
        let path = unix_socket_path("avail");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind real socket");
        let binding = a_binding();
        let server_binding = binding.clone();
        let resolver = CredentialResolver::new();

        // The owner serves exactly one Present@9 read; the task then returns and drops the listener,
        // so the next connection cannot be accepted.
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
                        value: "sk-avail".into(),
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

        // Read 1: the owner answers at its own revision 9, and it passes through untouched. This is
        // also where the resolver learns the channel (once) — every later read reuses it.
        resolver.learn_bootstrap(rx).await;
        let answered = resolver
            .read_bound("v1:spike-test-provider".into(), binding)
            .await;
        assert_eq!(
            answered.revision,
            Revision(9),
            "an answered read carries the owner's revision unchanged"
        );
        accept.await.unwrap();

        // Read 2: the owner is gone. The SAME resolver reuses the learned channel; the
        // Answered -> Unavailable transition advances the daemon generation, so the refusal gate
        // re-arms. There is no stdin argument here at all — the frame was consumed once above.
        let gone = resolver
            .read_bound("v1:spike-test-provider".into(), a_binding())
            .await;
        assert_eq!(gone.state.tag(), CredentialStateTag::OwnerUnavailable);
        // The daemon generation is a SEPARATE counter from the owner's revision (that is what lets
        // an answered read pass the owner's revision through untouched), so it need not exceed 9 —
        // but it must CHANGE from the answered read and advance past its own initial value, which is
        // what re-arms a gate keyed on equality. Removing the transition bump leaves it at
        // `Revision::INITIAL`, failing the second assertion.
        assert_ne!(
            gone.revision, answered.revision,
            "an availability transition must change the revision the gate sees"
        );
        assert!(
            gone.revision > Revision::INITIAL,
            "the Answered->Unavailable transition must advance the daemon generation"
        );

        std::fs::remove_file(&path).ok();
    }

    // --- CredentialResolver revision algebra (the folding rules, without transport) ---------------

    fn answered(rev: u64) -> CredentialRead {
        CredentialRead {
            revision: Revision(rev),
            state: CredentialState::Absent,
        }
    }

    fn unavailable() -> CredentialRead {
        CredentialRead {
            revision: Revision::INITIAL,
            state: CredentialState::OwnerUnavailable,
        }
    }

    // The property sol's point 2 demands: on an answered read the revision is EXACTLY the owner's,
    // never a synthetic increment. `Unavailable -> Answered@0` must return 0 so a caller can pass it
    // to Connect; returning 1 would make the real owner refuse with StaleRevision(0).
    #[test]
    fn an_answered_read_carries_the_owners_revision_untouched() {
        let resolver = CredentialResolver::new();
        let _ = resolver.observe("v1:x", unavailable());
        let read = resolver.observe("v1:x", answered(0));
        assert_eq!(read.revision, Revision(0));
    }

    // A real owner mutation after an availability transition must be visible: the transition does
    // not mask the owner's next revision (alice's round-2 finding on rhapsody#221).
    #[test]
    fn a_real_owner_mutation_after_a_transition_is_visible() {
        let resolver = CredentialResolver::new();
        assert_eq!(resolver.observe("v1:x", answered(9)).revision, Revision(9));
        let gone = resolver.observe("v1:x", unavailable());
        assert_ne!(
            gone.revision,
            Revision(9),
            "the transition changes the revision"
        );
        // The owner comes back at the revision it had: still the owner's own value, unchanged.
        assert_eq!(resolver.observe("v1:x", answered(9)).revision, Revision(9));
        // ...and a following owner Replace (9 -> 10) is visible again.
        let mutated = resolver.observe("v1:x", answered(10));
        assert_ne!(mutated.revision, Revision(9));
        assert_eq!(mutated.revision, Revision(10));
    }

    // A desktop restart resets the owner's counter to zero; the daemon must not mask the low
    // revisions that follow by insisting on monotonicity (alice's restart case).
    #[test]
    fn an_owner_restart_that_resets_its_revision_is_visible() {
        let resolver = CredentialResolver::new();
        assert_eq!(resolver.observe("v1:x", answered(5)).revision, Revision(5));
        assert_eq!(resolver.observe("v1:x", answered(0)).revision, Revision(0));
        assert_eq!(resolver.observe("v1:x", answered(1)).revision, Revision(1));
    }

    #[test]
    fn a_steady_unavailable_repeat_does_not_manufacture_a_revision() {
        let resolver = CredentialResolver::new();
        let first = resolver.observe("v1:x", unavailable());
        let repeat = resolver.observe("v1:x", unavailable());
        assert_eq!(first.revision, repeat.revision);
    }

    // Availability state is keyed by account: one credential's transition must not move another's
    // generation, or a shared resolver across providers would mask one owner's transitions.
    #[test]
    fn availability_state_is_tracked_per_account() {
        let resolver = CredentialResolver::new();
        // Drive account A through two availability transitions.
        let _ = resolver.observe("v1:a", answered(0));
        let _ = resolver.observe("v1:a", unavailable());
        let _ = resolver.observe("v1:a", answered(1));
        let a_gone = resolver.observe("v1:a", unavailable());
        // Account B has never transitioned, so its generation is still its first one.
        let b_gone = resolver.observe("v1:b", unavailable());
        assert_ne!(
            a_gone.revision, b_gone.revision,
            "each account tracks its own generation"
        );
    }
}
