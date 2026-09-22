//! Length-prefixed JSON framing plus the message shapes exchanged over the bounded, non-HTTP
//! bootstrap/control channel (ticket STUDIO-981, design §2.5's IPC branch). Generic over
//! `AsyncRead`/`AsyncWrite` so the same code frames a real `tokio::net::UnixStream` and, in this
//! crate's own tests, an in-memory `tokio::io::duplex` pair — no packaging is needed to test
//! protocol *logic*; the separate packaged/signed evidence for the ownership decision itself lives
//! in the ticket's PR description and findings record, not in this crate.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::domain::{Binding, CredentialStateTag, Revision};

/// The largest frame this protocol accepts, checked against the length prefix BEFORE any body
/// bytes are read or allocated — an oversized declared length is rejected without ever buffering
/// the attacker-chosen amount. 64 KiB comfortably covers a credential envelope plus binding with
/// room to spare (design §2.4's envelope is a handful of short strings).
pub const MAX_FRAME_BYTES: u32 = 64 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    /// The peer declared a frame larger than [`MAX_FRAME_BYTES`]. `got` is the declared length;
    /// the body was never read.
    TooLarge {
        got: u32,
        max: u32,
    },
    /// The peer closed the connection cleanly before a length prefix arrived.
    Eof,
    Decode(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "frame io error: {e}"),
            FrameError::TooLarge { got, max } => {
                write!(f, "frame of {got} bytes exceeds the {max} byte limit")
            }
            FrameError::Eof => write!(f, "connection closed before a frame arrived"),
            FrameError::Decode(e) => write!(f, "frame decode error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Writes one length-prefixed JSON frame. Never partially writes a frame that later reads split.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg).map_err(FrameError::Decode)?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        got: u32::MAX,
        max: MAX_FRAME_BYTES,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            got: len,
            max: MAX_FRAME_BYTES,
        });
    }
    w.write_all(&len.to_be_bytes())
        .await
        .map_err(FrameError::Io)?;
    w.write_all(&body).await.map_err(FrameError::Io)?;
    w.flush().await.map_err(FrameError::Io)?;
    Ok(())
}

/// Reads one length-prefixed JSON frame, rejecting an oversized declared length before allocating
/// or reading its body.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Eof),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            got: len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await.map_err(FrameError::Io)?;
    serde_json::from_slice(&body).map_err(FrameError::Decode)
}

/// The one-shot bootstrap message the desktop app writes to the freshly spawned `rhapsodyd`
/// child's stdin, then never writes to that pipe again (the supervisor closes its write end
/// immediately after). This is the only place the bootstrap token is allowed to travel — never
/// argv, never an inheritable env var, never `runtime.json`, never a log line.
#[derive(Serialize, serde::Deserialize)]
pub struct BootstrapMessage {
    pub token: String,
    pub socket_path: String,
}

impl std::fmt::Debug for BootstrapMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapMessage")
            .field("token", &"***")
            .field("socket_path", &self.socket_path)
            .finish()
    }
}

/// The one required first frame on every connection — carries the bootstrap token to
/// `session::ServerSession::accept_hello`. Framed separately from [`ClientFrame`] (not a variant of
/// it) because it is exempt from the post-auth sequence check every other client frame is subject
/// to, and because the server must be able to read/reject it before any `ClientFrame` decoding is
/// even attempted.
#[derive(Serialize, serde::Deserialize)]
pub struct HelloFrame {
    pub token: String,
}

impl std::fmt::Debug for HelloFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelloFrame").field("token", &"***").finish()
    }
}

/// A frame the daemon (client) sends after authenticating. `Hello` itself is carried
/// out-of-band of this enum (see `session::ServerSession::accept_hello`) because it is exempt
/// from the post-auth sequence check every other client frame is subject to.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ClientFrame {
    ReadBound {
        seq: u64,
        account: String,
        expected_binding: Binding,
    },
}

/// A non-secret lease payload as it travels on the wire. `Debug` redacts `value`; the receiving
/// side converts this into a real [`crate::domain::BoundCredentialLease`] immediately, which
/// zeroizes on drop.
#[derive(Serialize, serde::Deserialize)]
pub struct LeasePayload {
    pub binding: Binding,
    pub value: String,
}

impl std::fmt::Debug for LeasePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeasePayload")
            .field("binding", &self.binding)
            .field("value", &"***")
            .finish()
    }
}

/// A frame the desktop (server) sends. `ReadBoundResult.seq` is stamped from the server's own
/// independent, strictly increasing sequence — it does NOT echo the request's sequence; the client
/// tracks the two sequences separately (see `session::ClientSession`). `RevisionChanged` is
/// reserved for PB7, which will push it unsolicited the moment a local Connect/Replace/Rebind/
/// Remove commits; nothing in this ticket's scope constructs one yet.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ServerFrame {
    ReadBoundResult {
        seq: u64,
        revision: Revision,
        state: CredentialStateTag,
        lease: Option<LeasePayload>,
    },
    RevisionChanged {
        seq: u64,
        revision: Revision,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[derive(Debug, Serialize, serde::Deserialize, PartialEq, Eq)]
    struct Ping {
        n: u32,
    }

    #[tokio::test]
    async fn round_trips_a_frame() {
        let (mut a, mut b) = duplex(4096);
        write_frame(&mut a, &Ping { n: 7 }).await.expect("write");
        let got: Ping = read_frame(&mut b).await.expect("read");
        assert_eq!(got, Ping { n: 7 });
    }

    // The core oversized-message defense: a declared length beyond the cap must be rejected
    // without the reader ever allocating/consuming that many bytes.
    #[tokio::test]
    async fn read_frame_rejects_an_oversized_declared_length() {
        let (mut a, mut b) = duplex(8);
        let huge = MAX_FRAME_BYTES + 1;
        a.write_all(&huge.to_be_bytes())
            .await
            .expect("write oversized prefix");
        drop(a);
        let err = read_frame::<_, Ping>(&mut b).await.unwrap_err();
        match err {
            FrameError::TooLarge { got, max } => {
                assert_eq!(got, huge);
                assert_eq!(max, MAX_FRAME_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_frame_reports_eof_on_a_clean_close_before_any_bytes() {
        let (a, mut b) = duplex(8);
        drop(a);
        let err = read_frame::<_, Ping>(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::Eof));
    }

    #[test]
    fn bootstrap_message_debug_redacts_the_token() {
        let msg = BootstrapMessage {
            token: "super-secret-token".to_string(),
            socket_path: "/tmp/rhapsody.sock".to_string(),
        };
        let rendered = format!("{msg:?}");
        assert!(
            !rendered.contains("super-secret-token"),
            "leaked: {rendered}"
        );
        assert!(rendered.contains("/tmp/rhapsody.sock"));
    }

    #[test]
    fn hello_frame_debug_redacts_the_token() {
        let hello = HelloFrame {
            token: "super-secret-token".to_string(),
        };
        assert!(!format!("{hello:?}").contains("super-secret-token"));
    }

    #[tokio::test]
    async fn hello_frame_round_trips_over_a_real_frame() {
        let (mut a, mut b) = duplex(4096);
        write_frame(
            &mut a,
            &HelloFrame {
                token: "t".to_string(),
            },
        )
        .await
        .expect("write hello");
        let got: HelloFrame = read_frame(&mut b).await.expect("read hello");
        assert_eq!(got.token, "t");
    }

    #[test]
    fn lease_payload_debug_redacts_the_value() {
        let payload = LeasePayload {
            binding: Binding {
                provider_id: "p".into(),
                adapter: "a".into(),
                base_url: "https://example".into(),
            },
            value: "sk-secret".into(),
        };
        assert!(!format!("{payload:?}").contains("sk-secret"));
    }
}
