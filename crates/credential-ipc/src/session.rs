//! The authentication and anti-replay/anti-reorder state machine for one connection (ticket
//! STUDIO-981's IPC branch). Pure logic, no I/O — [`ServerSession`] and [`ClientSession`] are
//! driven by whatever transport frames arrive (a real Unix socket in production, an in-memory
//! duplex stream in this crate's tests). Keeping this transport-free is what makes "unauthorized,
//! replayed, oversized, and out-of-order" provable without any packaging: those are properties of
//! the protocol state machine, not of the OS-level signing/ACL boundary (which is proven
//! separately, against real signed binaries, for the ownership decision itself).

/// A bootstrap token minted fresh for one desktop-launched daemon's whole lifetime, not per
/// connection: the listener accepts the same token on any number of connections for as long as it
/// runs (e.g. the daemon reconnecting after its own restart). Only the per-connection `Hello` is
/// one-shot (`ServerSession::accept_hello` below). Never `Clone`/`Debug`; construct fresh per
/// launch, never reused across a restart (design §2.5's "revision... changes on... availability
/// transitions" is the daemon-side analogue — this is the transport-level guarantee that a stale
/// launch's token cannot authenticate a new one).
pub struct Token(String);

impl Token {
    pub fn new(value: String) -> Token {
        Token(value)
    }

    fn constant_time_eq(&self, other: &str) -> bool {
        let a = self.0.as_bytes();
        let b = other.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// The connection tried to send a non-Hello frame (or a second Hello) before/without
    /// authenticating.
    NotAuthenticated,
    /// `Hello.token` did not match this session's expected token.
    Unauthorized,
    /// A frame's sequence number was not exactly the next one expected — covers both a replayed
    /// (repeated/old) sequence and an out-of-order (skipped-ahead) one with a single check, since
    /// both are "not equal to the one and only acceptable next value".
    OutOfOrder { expected: u64, got: u64 },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotAuthenticated => write!(f, "frame received before authentication"),
            SessionError::Unauthorized => write!(f, "bootstrap token did not match"),
            SessionError::OutOfOrder { expected, got } => {
                write!(f, "expected sequence {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

/// The server (desktop) side of one connection: authenticates exactly once, then enforces a
/// strictly increasing sequence on every subsequent client frame.
pub struct ServerSession {
    expected_token: Token,
    authenticated: bool,
    expected_client_seq: u64,
    next_server_seq: u64,
}

impl ServerSession {
    pub fn new(expected_token: Token) -> ServerSession {
        ServerSession {
            expected_token,
            authenticated: false,
            expected_client_seq: 1,
            next_server_seq: 1,
        }
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Consumes the connection's one required `Hello`. A repeat call (a second Hello on an
    /// already-authenticated connection) is rejected the same as a bad token — the handshake is
    /// one-shot, not a re-authentication channel.
    pub fn accept_hello(&mut self, presented_token: &str) -> Result<(), SessionError> {
        if self.authenticated {
            return Err(SessionError::Unauthorized);
        }
        if !self.expected_token.constant_time_eq(presented_token) {
            return Err(SessionError::Unauthorized);
        }
        self.authenticated = true;
        Ok(())
    }

    /// Validates and advances the expected sequence for one incoming client frame. Must be called
    /// exactly once per accepted frame, in wire order.
    pub fn accept_client_seq(&mut self, seq: u64) -> Result<(), SessionError> {
        if !self.authenticated {
            return Err(SessionError::NotAuthenticated);
        }
        if seq != self.expected_client_seq {
            return Err(SessionError::OutOfOrder {
                expected: self.expected_client_seq,
                got: seq,
            });
        }
        self.expected_client_seq += 1;
        Ok(())
    }

    /// The sequence number to stamp on the next server-originated frame (a `ReadBoundResult` or an
    /// unsolicited `RevisionChanged`).
    pub fn next_outgoing_seq(&mut self) -> u64 {
        let s = self.next_server_seq;
        self.next_server_seq += 1;
        s
    }
}

/// The client (daemon) side of one connection: presents the token once, then stamps its own
/// strictly increasing sequence on every request, and validates the server's independent sequence
/// on every frame it receives back.
pub struct ClientSession {
    token: Token,
    next_client_seq: u64,
    expected_server_seq: u64,
}

impl ClientSession {
    pub fn new(token: Token) -> ClientSession {
        ClientSession {
            token,
            next_client_seq: 1,
            expected_server_seq: 1,
        }
    }

    /// The token to present in this connection's one `Hello` frame.
    pub fn hello_token(&self) -> &str {
        &self.token.0
    }

    pub fn next_outgoing_seq(&mut self) -> u64 {
        let s = self.next_client_seq;
        self.next_client_seq += 1;
        s
    }

    pub fn accept_server_seq(&mut self, seq: u64) -> Result<(), SessionError> {
        if seq != self.expected_server_seq {
            return Err(SessionError::OutOfOrder {
                expected: self.expected_server_seq,
                got: seq,
            });
        }
        self.expected_server_seq += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authed_session() -> ServerSession {
        let mut s = ServerSession::new(Token::new("correct-token".into()));
        s.accept_hello("correct-token").expect("hello accepted");
        s
    }

    #[test]
    fn wrong_token_is_rejected_and_authenticates_nothing() {
        let mut s = ServerSession::new(Token::new("correct-token".into()));
        assert_eq!(
            s.accept_hello("wrong-token"),
            Err(SessionError::Unauthorized)
        );
        assert!(!s.is_authenticated());
        // A frame sent without ever authenticating is rejected too, not merely accepted-but-flagged.
        assert_eq!(s.accept_client_seq(1), Err(SessionError::NotAuthenticated));
    }

    #[test]
    fn missing_hello_rejects_every_subsequent_frame() {
        let mut s = ServerSession::new(Token::new("t".into()));
        assert_eq!(s.accept_client_seq(1), Err(SessionError::NotAuthenticated));
    }

    #[test]
    fn correct_token_authenticates_exactly_once() {
        let mut s = authed_session();
        assert!(s.is_authenticated());
        // Replaying the correct token on an already-authenticated connection is still rejected —
        // Hello is one-shot, not a re-auth channel a captured token could replay mid-session.
        assert_eq!(
            s.accept_hello("correct-token"),
            Err(SessionError::Unauthorized)
        );
    }

    #[test]
    fn sequential_frames_are_accepted_in_order() {
        let mut s = authed_session();
        s.accept_client_seq(1).expect("seq 1");
        s.accept_client_seq(2).expect("seq 2");
        s.accept_client_seq(3).expect("seq 3");
    }

    #[test]
    fn a_replayed_sequence_number_is_rejected() {
        let mut s = authed_session();
        s.accept_client_seq(1).expect("seq 1");
        s.accept_client_seq(2).expect("seq 2");
        // Replay: repeats seq 1 instead of continuing from 3.
        assert_eq!(
            s.accept_client_seq(1),
            Err(SessionError::OutOfOrder {
                expected: 3,
                got: 1
            })
        );
    }

    #[test]
    fn an_out_of_order_sequence_number_is_rejected() {
        let mut s = authed_session();
        s.accept_client_seq(1).expect("seq 1");
        // Skips ahead to 5 instead of 2.
        assert_eq!(
            s.accept_client_seq(5),
            Err(SessionError::OutOfOrder {
                expected: 2,
                got: 5
            })
        );
    }

    #[test]
    fn out_of_order_rejection_does_not_advance_expected_seq() {
        let mut s = authed_session();
        s.accept_client_seq(1).expect("seq 1");
        assert!(s.accept_client_seq(9).is_err());
        // The very next legitimate frame (seq 2) must still be accepted — one bad frame doesn't
        // permanently wedge the connection's expectation.
        s.accept_client_seq(2)
            .expect("seq 2 still accepted after a rejected out-of-order frame");
    }

    #[test]
    fn client_and_server_sequences_are_independent_and_symmetric() {
        let mut server = authed_session();
        let mut client = ClientSession::new(Token::new("correct-token".into()));

        let req_seq = client.next_outgoing_seq();
        server
            .accept_client_seq(req_seq)
            .expect("server accepts client's first seq");

        let resp_seq = server.next_outgoing_seq();
        client
            .accept_server_seq(resp_seq)
            .expect("client accepts server's first seq");

        // A second round trip continues both counters independently from 1.
        let req_seq2 = client.next_outgoing_seq();
        server
            .accept_client_seq(req_seq2)
            .expect("server accepts client's second seq");
        let resp_seq2 = server.next_outgoing_seq();
        client
            .accept_server_seq(resp_seq2)
            .expect("client accepts server's second seq");
    }
}
