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

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rhapsody_credential_ipc::domain::CredentialRef;
use rhapsody_credential_ipc::session::{ServerSession, Token};
use rhapsody_credential_ipc::wire::{
    BootstrapMessage, ClientFrame, HelloFrame, ServerFrame, read_frame, write_frame,
};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::provider_credential::ProviderCredentialOwner;

/// How long a freshly accepted connection's own task waits for its `Hello` frame before giving up
/// on that connection specifically. Since B3 (jimmy's review of rhapsody#213), each connection's
/// pre-`Hello` phase runs in its own task independently of every other connection's, so a same-user
/// process that connects and never sends `Hello` — which the design's own threat model
/// (`provider-auth-p0-findings.md` §8) assumes can happen — bounds only its OWN task's lifetime; it
/// cannot wedge any other connection, including the real daemon's own reconnect after a restart
/// (`five_silent_connections_cannot_starve_a_later_legitimate_client` pins exactly that). A
/// legitimate handshake is a single local write immediately after `connect`, so this has ample
/// margin without making a deliberately silent connection expensive to defend against.
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

    /// Accepts connections and serves `read_bound` requests against `owner` until told to shut
    /// down via the returned [`ListenerShutdown`]. Each accepted connection's pre-`Hello` phase
    /// runs in its own task, bounded by [`HELLO_TIMEOUT`] independently of every other connection —
    /// so any number of same-user processes that connect and never send `Hello` cannot
    /// serialize-starve a later, legitimate connection's own `Hello` read behind `HELLO_TIMEOUT`
    /// multiplied by however many came before it (jimmy's review of rhapsody#213, B3: five silent
    /// connections cost the real daemon `5 × HELLO_TIMEOUT` before its own `Hello` was even read,
    /// exceeding the daemon-side `RESPONSE_TIMEOUT`). Only an AUTHENTICATED connection ever
    /// contends for `serving_slot`, the single slot that actually answers `read_bound` requests —
    /// matching "only one connection is served at a time (the daemon holds exactly one)"; a second
    /// authenticated connection (e.g. the daemon reconnecting after its own restart) waits for the
    /// first's connection to end.
    ///
    /// Every per-connection task is tracked in a [`tokio::task::JoinSet`] owned by the returned
    /// future's own stack frame. `JoinSet::drop` and `JoinSet::abort_all` only set each child's
    /// cooperative-cancellation flag, which a task already mid-poll through a synchronous
    /// `owner.read_bound` Keychain call and a ready (non-blocking) socket write does not observe
    /// until its NEXT poll — by which point it may already have sent the response (sol's first and
    /// second reviews of rhapsody#213). So revocation instead rests on a shared
    /// [`CancellationToken`] that `serve_one` checks explicitly, synchronously, after the owner
    /// read returns and before the response is written, so the check itself can never be skipped
    /// by a poll boundary. That token is flipped on EVERY way the returned future can end:
    /// [`ListenerShutdown::shutdown`], an `accept()` error, and — through the `Drop` of the
    /// `CancelOnDropConnections` that owns the connection tasks, which flips it before those tasks
    /// are torn down — being dropped or `abort()`-ed mid-poll, or panicking (jimmy's and sol's
    /// reviews of rhapsody#213, B8). Dropping or aborting the future therefore never lets an in-flight read
    /// answer, but it also does not WAIT for the connection tasks to finish: only calling
    /// `shutdown()` and then awaiting the driving future (e.g. the `tokio::spawn` `JoinHandle`)
    /// tells the caller every connection has actually finished, not merely been asked to.
    pub fn accept_and_serve(
        self,
        owner: Arc<ProviderCredentialOwner>,
    ) -> (impl Future<Output = ()> + Send + 'static, ListenerShutdown) {
        let cancel = CancellationToken::new();
        let shutdown = ListenerShutdown {
            cancel: cancel.clone(),
        };
        let future = async move {
            let serving_slot = Arc::new(tokio::sync::Semaphore::new(1));
            // Flips `cancel` if this future ends by any route other than the two loop exits below
            // (drop, `abort()`, panic, a future early `return`), none of which reach
            // `graceful_shutdown` — see `CancelOnDropConnections` and this method's doc (jimmy's
            // and sol's reviews of rhapsody#213, B8).
            let mut connections = CancelOnDropConnections {
                cancel: cancel.clone(),
                tasks: tokio::task::JoinSet::new(),
            };
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    accepted = self.listener.accept() => {
                        let (stream, _addr) = match accepted {
                            Ok(pair) => pair,
                            // An accept() failure (EMFILE, ENFILE, ECONNABORTED, ...) must revoke
                            // the channel exactly as thoroughly as an explicit `shutdown()` does —
                            // see `graceful_shutdown`'s doc (jimmy's review of rhapsody#213, B7:
                            // this exit used to `break` straight to the drain without flipping
                            // `cancel` first, leaving `JoinSet::shutdown`'s cooperative abort as
                            // the only defense against exactly the kind of in-flight synchronous
                            // Keychain read this file's whole later history is about).
                            Err(_) => break,
                        };
                        let owner = owner.clone();
                        let token = self.token.clone();
                        let serving_slot = serving_slot.clone();
                        let cancel = cancel.clone();
                        connections.tasks.spawn(async move {
                            serve_one(stream, token, owner, serving_slot, cancel).await;
                        });
                    }
                    // Reap finished connections so `connections` doesn't grow without bound; the
                    // `if` guard keeps this branch out of the poll set entirely while empty, rather
                    // than resolving to `None` every iteration and busy-looping.
                    _ = connections.tasks.join_next(), if !connections.tasks.is_empty() => {}
                }
            }
            graceful_shutdown(&cancel, &mut connections.tasks).await;
        };
        (future, shutdown)
    }
}

/// The listener's per-connection tasks, owned together with the token that revokes them. Its
/// `Drop` flips `cancel` on every way the listener future can end without reaching
/// `graceful_shutdown` — being dropped or `abort()`-ed mid-poll, a panic, an early `return` —
/// because `JoinSet`'s own drop only sets each child's cooperative flag, which a child parked in
/// the synchronous `owner.read_bound` Keychain call cannot observe before it writes its reply
/// (jimmy's review of rhapsody#213, B8). Holding the `JoinSet` as a field rather than as a sibling
/// local next to a separate drop guard makes the order a language guarantee instead of a
/// declaration-order convention (sol's review of rhapsody#213): a value's `Drop::drop` always runs
/// before any of its fields are dropped, so the token is flipped before the `JoinSet` starts
/// tearing its children down, however this frame is later rearranged.
struct CancelOnDropConnections {
    cancel: CancellationToken,
    tasks: tokio::task::JoinSet<()>,
}

impl Drop for CancelOnDropConnections {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The single shared teardown every `accept_and_serve` loop exit funnels through, whether the
/// caller explicitly called [`ListenerShutdown::shutdown`] or the accept loop gave up on its own
/// (an `accept()` error). Flips `cancel` UNCONDITIONALLY before draining — jimmy's review of
/// rhapsody#213 (B7) found that the accept-error exit used to skip straight to
/// `connections.shutdown()` without cancelling first, so a connection task parked inside the
/// synchronous `owner.read_bound` Keychain call at that moment would still see `cancel.is_
/// cancelled() == false` when it resumed and would answer anyway. There being exactly one call
/// site for this function is what makes "every exit cancels before it drains" true by
/// construction, not something a future new exit has to remember to repeat.
///
/// This does NOT itself guarantee no in-flight read answers after this returns — that guarantee
/// comes from `serve_one`'s own explicit `cancel.is_cancelled()` check taken synchronously right
/// after `owner.read_bound` returns (see that check's doc). What this function's ordering DOES
/// guarantee: `cancel` is flipped before any live connection task is waited on, so a task that is
/// merely idle (awaiting its next frame, its `Hello`, or the serving slot) is guaranteed to observe
/// cancellation rather than being torn down by `JoinSet::shutdown`'s cooperative abort without
/// ever having had the chance to check; `graceful_shutdown_cancels_before_it_drains` below pins
/// this ordering directly.
async fn graceful_shutdown(cancel: &CancellationToken, connections: &mut tokio::task::JoinSet<()>) {
    cancel.cancel();
    connections.shutdown().await;
}

/// A handle to request a graceful, waited-for shutdown of a listener returned by
/// [`BootstrapListener::accept_and_serve`]. Aborting or dropping the listener's driving future
/// also revokes the channel — no in-flight read answers afterward — but does not wait for the
/// connection tasks to finish; calling [`shutdown`](ListenerShutdown::shutdown) and then awaiting
/// the driving future is the only combination that tells the caller the channel is fully revoked
/// AND every connection has ended. See that method's doc.
#[derive(Clone)]
pub struct ListenerShutdown {
    cancel: CancellationToken,
}

impl ListenerShutdown {
    /// Signals the listener to stop accepting new connections and every existing connection to
    /// stop answering further `read_bound` requests. Does not itself wait for anything — await the
    /// listener's driving future (returned alongside this handle) afterward to know shutdown has
    /// actually completed.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

async fn serve_one(
    mut stream: UnixStream,
    token: String,
    owner: Arc<ProviderCredentialOwner>,
    serving_slot: Arc<tokio::sync::Semaphore>,
    cancel: CancellationToken,
) {
    let mut session = ServerSession::new(Token::new(token));

    let hello: HelloFrame = tokio::select! {
        () = cancel.cancelled() => return,
        result = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream)) => match result {
            Ok(Ok(h)) => h,
            // A timed-out or errored/EOF'd Hello read are the same outcome here: give up on this
            // connection — its own task simply ends, never blocking any other connection's Hello
            // phase or the single serving slot.
            Ok(Err(_)) | Err(_) => return,
        },
    };
    // An unauthorized connection gets no response at all — closing the stream, not answering with
    // an explicit rejection frame, so a probing caller learns nothing beyond "this didn't work".
    if session.accept_hello(&hello.token).is_err() {
        return;
    }

    // Only an authenticated connection reaches here, and only one at a time ever serves
    // `read_bound` requests. `acquire` only errors if the semaphore itself was closed, which never
    // happens here.
    let Ok(_permit) = (tokio::select! {
        () = cancel.cancelled() => return,
        permit = serving_slot.acquire() => permit,
    }) else {
        return;
    };

    loop {
        let frame: ClientFrame = tokio::select! {
            () = cancel.cancelled() => return,
            result = read_frame(&mut stream) => match result {
                Ok(f) => f,
                Err(_) => return,
            },
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
        // `owner.read_bound` is fully synchronous (a Keychain call, possibly blocking on macOS
        // Keychain locking), so it runs to completion within this task's current poll no matter
        // what `cancel` does concurrently on another thread — a task abort or a `JoinSet` drop
        // cannot interrupt it. This check, taken synchronously right after that call returns and
        // before the response is built or written, is what actually closes the window: if shutdown
        // was requested at any point up to and including while the Keychain call was in flight,
        // this connection now discards the result instead of sending it (sol's review of
        // rhapsody#213, second round).
        if cancel.is_cancelled() {
            return;
        }
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
            value: lease.expose_for_broker(str::to_owned),
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

        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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
        shutdown.shutdown();
        let _ = serve.await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_connection_presenting_the_wrong_token_gets_no_response() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let socket_path = listener.bootstrap_message().socket_path;
        let owner = owner_with_secret();
        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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

        shutdown.shutdown();
        let _ = serve.await;
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
        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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

        shutdown.shutdown();
        let _ = serve.await;
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
        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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
        shutdown.shutdown();
        let _ = serve.await;
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
        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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
        shutdown.shutdown();
        let _ = serve.await;
        std::fs::remove_dir_all(&dir).ok();
    }

    // sol's first review of rhapsody#213: dropping/aborting the outer `accept_and_serve` task used
    // to leave an already-authenticated connection's own spawned task running, still holding the
    // owner and still answering `read_bound`. A second read over the SAME already-authenticated
    // client, issued only after `shutdown()` and awaiting the listener task to completion, must now
    // fail instead of succeeding.
    #[tokio::test]
    async fn shutdown_drops_authenticated_child_connections() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();
        let owner = owner_with_secret();

        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

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

        shutdown.shutdown();
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

    // sol's second review of rhapsody#213: `JoinSet::shutdown`/`abort` only sets each child's
    // cooperative-cancellation flag, which a connection task already mid-poll through a
    // synchronous, blocking `owner.read_bound` Keychain call cannot observe until its NEXT poll —
    // by which point it may already have written its response, even though `shutdown()` was called
    // (and, in this test, `serve.await` had already completed) before the Keychain call returned.
    // Proves the fix: a read genuinely blocked inside the Keychain call when `shutdown()` fires
    // must never deliver its response, no matter when the blocking call happens to unblock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_during_a_blocked_owner_read_discards_the_in_flight_response() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();

        let backing = MockKeyring::empty();
        let (blocking_kr, entered_rx, release_tx) = BlockingKeyring::new(backing);
        let arm = blocking_kr.clone();
        let owner = ProviderCredentialOwner::for_test(blocking_kr);
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
            .expect("connect (unblocked: not yet armed)");
        // Arm the pause only now, so it catches `read_bound`'s `get_password` call and not the one
        // `connect` above already made to check the owner was `Absent`.
        arm.arm();
        let owner = Arc::new(owner);

        let (fut, shutdown) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;

        let read_task = tokio::spawn(async move {
            let outcome = client
                .read_bound(
                    "v1:spike-test-provider".into(),
                    Binding {
                        provider_id: "spike-test-provider".into(),
                        adapter: "openai-chat-completions-bearer-v1".into(),
                        base_url: "https://api.example/v1".into(),
                    },
                )
                .await;
            // Keep `client` (and so its socket) alive until the read settles, so the server
            // observes shutdown rather than an early client-side EOF.
            drop(client);
            outcome
        });

        // Block on a real OS thread (via `spawn_blocking`) rather than in this async task, so the
        // wait itself doesn't tie up the only worker thread the server's genuinely-blocking
        // `owner.read_bound` call needs in order to make progress concurrently.
        tokio::task::spawn_blocking(move || entered_rx.recv())
            .await
            .expect("join")
            .expect("read must signal it entered the blocking Keychain call");

        // Shut the listener down WHILE the read is still parked inside the Keychain call — the
        // exact window a bare task abort/`JoinSet` drop cannot close.
        shutdown.shutdown();

        // Only now release the blocked call, letting `serve_one` resume past it.
        release_tx
            .send(())
            .expect("release the paused read so the server task can finish");

        serve
            .await
            .expect("the listener task must complete cleanly, waiting for the freed connection");

        let outcome = tokio::time::timeout(Duration::from_secs(1), read_task)
            .await
            .expect("the client task must not hang waiting for a response that will never come")
            .expect("read task must not panic");
        assert!(
            outcome.is_err(),
            "a read already in flight when shutdown was requested must never deliver its response"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // jimmy's review of rhapsody#213 (B8): aborting (or dropping) the listener's driving future
    // WITHOUT ever calling `ListenerShutdown::shutdown` bypasses both of the accept loop's
    // `break`s, so `graceful_shutdown` never runs and nothing flips `cancel` — while `JoinSet`'s
    // drop only sets each child's cooperative flag, which a task parked inside the synchronous
    // Keychain call cannot observe before it writes its reply. The `ListenerShutdown` handle is
    // kept alive and never called, exactly as a supervisor that `select!`s the future against the
    // child process (or `abort()`s it on restart) would leave it. A read in flight at that moment
    // must still never deliver its response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn aborting_the_listener_during_a_blocked_owner_read_discards_the_in_flight_response() {
        let dir = temp_dir();
        let listener = BootstrapListener::bind(&dir).expect("bind");
        let msg = listener.bootstrap_message();

        let backing = MockKeyring::empty();
        let (blocking_kr, entered_rx, release_tx) = BlockingKeyring::new(backing);
        let arm = blocking_kr.clone();
        let owner = ProviderCredentialOwner::for_test(blocking_kr);
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
            .expect("connect (unblocked: not yet armed)");
        arm.arm();
        let owner = Arc::new(owner);

        let (fut, _shutdown_never_called) = listener.accept_and_serve(owner);
        let serve = tokio::spawn(fut);

        let stream = UnixStream::connect(&msg.socket_path)
            .await
            .expect("connect");
        let mut client = rhapsodyd_test_client(stream, msg.token.clone()).await;

        let read_task = tokio::spawn(async move {
            let outcome = client
                .read_bound(
                    "v1:spike-test-provider".into(),
                    Binding {
                        provider_id: "spike-test-provider".into(),
                        adapter: "openai-chat-completions-bearer-v1".into(),
                        base_url: "https://api.example/v1".into(),
                    },
                )
                .await;
            drop(client);
            outcome
        });

        tokio::task::spawn_blocking(move || entered_rx.recv())
            .await
            .expect("join")
            .expect("read must signal it entered the blocking Keychain call");

        // Abort the listener task WHILE the read is parked inside the Keychain call, and wait for
        // the abort to land, before the blocked call is released.
        serve.abort();
        let _ = serve.await;

        release_tx
            .send(())
            .expect("release the paused read so the connection task can finish");

        let outcome = tokio::time::timeout(Duration::from_secs(1), read_task)
            .await
            .expect("the client task must not hang waiting for a response that will never come")
            .expect("read task must not panic");
        assert!(
            outcome.is_err(),
            "a read in flight when the listener task was aborted must never deliver its response"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Pins `graceful_shutdown`'s own ordering contract directly, independent of which
    // `accept_and_serve` loop exit happens to call it (jimmy's review of rhapsody#213, B7: the
    // accept-error exit used to reach `connections.shutdown()` without cancelling `cancel` first,
    // so a task doing synchronous work concurrently with the drain could still observe stale
    // (not-yet-cancelled) state once it finished). The spawned task mimics `serve_one`'s real
    // shape: synchronous, non-yielding work (a real `std::thread::sleep`, which `JoinSet::
    // shutdown`'s cooperative abort cannot interrupt) followed by a check of `cancel.is_cancelled()`
    // taken only once that work completes. If `graceful_shutdown` cancelled AFTER draining instead
    // of before, the drain would still wait out the same sleep (abort can't shorten it), but the
    // task's check would read `false` — this test would catch that as a wrong result rather than a
    // hang, because the timeout around the call fires only if the drain itself never returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_cancels_before_it_drains() {
        let cancel = CancellationToken::new();
        let mut connections = tokio::task::JoinSet::new();
        let result = Arc::new(std::sync::Mutex::new(None));
        let task_result = result.clone();
        let task_cancel = cancel.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        connections.spawn(async move {
            let _ = entered_tx.send(());
            std::thread::sleep(Duration::from_millis(50));
            *task_result.lock().unwrap() = Some(task_cancel.is_cancelled());
        });

        tokio::task::spawn_blocking(move || entered_rx.recv())
            .await
            .expect("join")
            .expect("task must signal it entered its synchronous phase");

        tokio::time::timeout(
            Duration::from_secs(2),
            graceful_shutdown(&cancel, &mut connections),
        )
        .await
        .expect("graceful_shutdown must not hang");

        assert_eq!(
            *result.lock().unwrap(),
            Some(true),
            "the task's post-sleep cancellation check must observe true — cancel must be flipped \
             BEFORE graceful_shutdown starts draining, not after"
        );
    }

    /// A `Keyring` double whose `get_password()` blocks — once armed via [`BlockingKeyring::arm`] —
    /// until the test explicitly releases it, signaling the test the instant it is entered. Armed
    /// lazily (rather than pausing the very first call unconditionally, as `provider_credential`'s
    /// own double does) because seeding this owner's state goes through the real `connect`, which
    /// makes its own `get_password` call first to check the owner is `Absent`; arming only after
    /// that succeeds lets the pause catch exactly the later `read_bound` call under test.
    struct BlockingKeyring {
        inner: Arc<MockKeyring>,
        armed: std::sync::atomic::AtomicBool,
        entered_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release_rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl BlockingKeyring {
        fn new(
            inner: Arc<MockKeyring>,
        ) -> (
            Arc<BlockingKeyring>,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let kr = Arc::new(BlockingKeyring {
                inner,
                armed: std::sync::atomic::AtomicBool::new(false),
                entered_tx: std::sync::Mutex::new(Some(entered_tx)),
                release_rx: std::sync::Mutex::new(Some(release_rx)),
            });
            (kr, entered_rx, release_tx)
        }

        fn arm(&self) {
            self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::credential::Keyring for BlockingKeyring {
        fn get_password(&self) -> Result<String, crate::credential::KeyringError> {
            if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if let Some(tx) = self.entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = self.release_rx.lock().unwrap().take() {
                    let _ = rx.recv();
                }
            }
            self.inner.get_password()
        }
        fn set_password(&self, token: &str) -> Result<(), crate::credential::KeyringError> {
            self.inner.set_password(token)
        }
        fn delete_credential(&self) -> Result<(), crate::credential::KeyringError> {
            self.inner.delete_credential()
        }
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
