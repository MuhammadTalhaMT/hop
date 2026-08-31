//! The connection handshake and per-session key derivation.
//!
//! Plan A added session binding to the AEAD (see [`hop_proto::SessionId`]),
//! but left both peers passing [`SessionId::ZERO`], so the binding did no
//! work: a frame recorded on one `ZERO` session still authenticated on the
//! next `ZERO` session. This module is what turns that protection on. Each
//! connection now derives a fresh session from both peers' random nonces,
//! so a recording made under one session cannot be replayed into another.
//!
//! ## The session/split chicken-and-egg problem
//!
//! [`crate::split`] takes a [`SessionId`] up front, but the session is only
//! known after the handshake runs, and the handshake itself needs a
//! transport to exchange messages over. [`client_handshake`] and
//! [`server_handshake`] resolve this internally, by owning the stream for
//! the whole call rather than taking already-split transport halves: each
//! one splits `stream` under [`SessionId::ZERO`] to exchange the two
//! `Handshake` messages (safe, since their freshness comes from the random
//! nonces they carry, not from session binding), then reclaims the stream
//! and re-splits it under the derived session before returning the new
//! halves. A caller can never obtain the `SessionId::ZERO`-keyed halves and
//! keep using them for input: they never leave this module, because the
//! stream is consumed by these functions rather than passed in pre-split.
//! This is why `TransportReader::into_inner` and `TransportWriter::into_inner`
//! are `pub(crate)` rather than public: nothing outside this crate has a
//! reason to touch them.

use crate::{split, TransportError, TransportReader, TransportWriter};
use hop_proto::{Direction, Message, SessionId, SharedKey, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};

/// How long either side of the handshake waits for the whole exchange to
/// complete before giving up. Five seconds is generous for a LAN.
///
/// `server_handshake` runs inline in the accept loop (see hop's
/// `run.rs`'s `run_server`), so without this a peer that completes the
/// TCP handshake and then sends nothing at all - a port scanner, a
/// monitoring probe, a machine that dropped off the network mid
/// handshake - blocks `read_exact` forever and parks the server
/// permanently: it stays alive, logs nothing, and never accepts the real
/// client again. `client_handshake` has the same hole on the other side:
/// a client dialing a wedged server would otherwise block forever with
/// no death detection, no backoff, and no reconnect, since `Liveness` is
/// only constructed after the handshake returns. This is CRITICAL 3 from
/// the whole-branch review.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Derive the session identifier both peers will authenticate every frame
/// against.
///
/// Both nonces contribute, so neither peer can choose the session alone,
/// and a recording made under one session cannot be replayed into
/// another. The order is fixed by role rather than sorted, so a reflected
/// handshake produces a different session than the genuine one.
///
/// Despite the name "per session key derivation" this module is documented
/// under, this function derives a session IDENTIFIER, not an encryption
/// key. It is used only as authenticated data (alongside the direction; see
/// `hop_proto::crypto::Direction`), so a captured frame cannot be replayed
/// into a different connection. The AEAD key itself never changes: every
/// session, past and future, is encrypted under the same static pre-shared
/// key handed to `SharedKey::from_bytes` or produced by
/// `SharedKey::generate`. There is therefore NO forward secrecy: anyone who
/// obtains that pre-shared key, now or later, can decrypt every session
/// ever recorded on the wire, not just the ones that happen after the
/// compromise. This is an acceptable tradeoff for what session binding is
/// actually for here (stopping replay across connections), not an
/// oversight, but callers must not read "per session key derivation" as a
/// claim of forward secrecy, because it is not one.
pub fn derive_session(client_nonce: &[u8; 32], server_nonce: &[u8; 32]) -> SessionId {
    let mut hasher = Sha256::new();
    hasher.update(b"hop session v1");
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    SessionId(out)
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("peer speaks protocol version {theirs}, we speak {ours}")]
    VersionMismatch { ours: u16, theirs: u16 },
    /// A non-`Handshake` frame arrived where a handshake was expected.
    ///
    /// Carries only the message's kind, never the message itself. A
    /// desynchronized or hostile peer's first frame could be a
    /// `Message::Key`, and this tool exists to forward passwords: if the
    /// message were carried here, formatting this error (which any
    /// supervisor does, straight into a log) would write the user's
    /// keystroke to disk. `SharedKey`'s manual `Debug` impl in
    /// `hop_proto::crypto` exists for the same reason.
    #[error("peer sent {0} instead of a handshake")]
    Unexpected(&'static str),
    #[error("system RNG unavailable")]
    Random,
    /// The peer connected but never completed the handshake within
    /// `HANDSHAKE_TIMEOUT`. See that constant's doc comment for why this
    /// exists: without it, a peer that never sends anything parks
    /// whichever side is waiting forever.
    #[error(
        "handshake did not complete within the timeout; the peer connected but never finished it"
    )]
    Timeout,
}

/// Name a message's kind without exposing anything it carries. Used only
/// to populate [`HandshakeError::Unexpected`]; see that variant's doc
/// comment for why the message itself must never be formatted.
fn message_kind(message: &Message) -> &'static str {
    match message {
        Message::Handshake { .. } => "Handshake",
        Message::MouseMove { .. } => "MouseMove",
        Message::MouseButton { .. } => "MouseButton",
        Message::Scroll { .. } => "Scroll",
        Message::Key { .. } => "Key",
        Message::ReleaseAllKeys => "ReleaseAllKeys",
        Message::Heartbeat => "Heartbeat",
        Message::Release => "Release",
        // Names the kind only. The text it carries is the user's
        // clipboard, so it must never reach a log.
        Message::ClipboardText(_) => "ClipboardText",
        Message::Unknown => "Unknown",
    }
}

/// Read one message and require it to be a `Handshake` at our protocol
/// version, checked before anything else about the message is trusted.
///
/// Shared by both roles so the version gate is enforced identically on
/// each side, and split out from `server_handshake` so the rejection path
/// is directly testable against a peer that never completes the rest of
/// the exchange.
async fn recv_handshake<R: AsyncRead + Unpin>(
    reader: &mut TransportReader<R>,
) -> Result<([u8; 32], String), HandshakeError> {
    match reader.recv().await? {
        Message::Handshake {
            version,
            peer_id,
            nonce,
            ..
        } => {
            if version != PROTOCOL_VERSION {
                return Err(HandshakeError::VersionMismatch {
                    ours: PROTOCOL_VERSION,
                    theirs: version,
                });
            }
            Ok((nonce, peer_id))
        }
        other => Err(HandshakeError::Unexpected(message_kind(&other))),
    }
}

/// The read-only half of the server's side of the handshake: receive and
/// validate the client's opening `Handshake` message, but do not send a
/// reply. Exists so the version-mismatch rejection is testable without a
/// peer that also completes the exchange.
async fn server_handshake_read_only<R: AsyncRead + Unpin>(
    reader: &mut TransportReader<R>,
) -> Result<([u8; 32], String), HandshakeError> {
    recv_handshake(reader).await
}

/// Perform the full client side of the handshake over `stream`: split it
/// under [`SessionId::ZERO`], send a fresh nonce, receive the server's,
/// derive the session both sides will use from here on, then reclaim the
/// stream and re-split it under that session.
///
/// `stream` is consumed rather than taken as already-split halves so that
/// the `SessionId::ZERO`-keyed transport can never escape this function;
/// see the module doc comment. The returned halves are already bound to
/// the derived session, ready for input.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
    peer_id: &str,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
    ),
    HandshakeError,
> {
    client_handshake_with_timeout(stream, key, peer_id, HANDSHAKE_TIMEOUT).await
}

/// Same as [`client_handshake`], with the timeout as a parameter so tests
/// can use a short one instead of waiting out the real
/// [`HANDSHAKE_TIMEOUT`].
async fn client_handshake_with_timeout<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
    peer_id: &str,
    timeout: Duration,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
    ),
    HandshakeError,
> {
    match tokio::time::timeout(timeout, client_handshake_inner(stream, key, peer_id)).await {
        Ok(result) => result,
        Err(_) => Err(HandshakeError::Timeout),
    }
}

async fn client_handshake_inner<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
    peer_id: &str,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
    ),
    HandshakeError,
> {
    let (mut reader, mut writer) = split(
        stream,
        key.clone(),
        SessionId::ZERO,
        Direction::ClientToServer,
    );

    let mut client_nonce = [0u8; 32];
    getrandom::fill(&mut client_nonce).map_err(|_| HandshakeError::Random)?;

    writer
        .send(&Message::Handshake {
            version: PROTOCOL_VERSION,
            capabilities: 0,
            peer_id: peer_id.to_string(),
            nonce: client_nonce,
        })
        .await?;

    let (server_nonce, _server_peer_id) = recv_handshake(&mut reader).await?;
    let session = derive_session(&client_nonce, &server_nonce);

    let stream = reader.into_inner().unsplit(writer.into_inner());
    let (reader, writer) = split(stream, key.clone(), session, Direction::ClientToServer);
    Ok((reader, writer, session))
}

/// Perform the full server side of the handshake over `stream`: split it
/// under [`SessionId::ZERO`], receive the client's nonce and peer id,
/// reply with a fresh nonce of our own, derive the session both sides
/// will use from here on, then reclaim the stream and re-split it under
/// that session.
///
/// `stream` is consumed for the same reason as in [`client_handshake`]:
/// see the module doc comment. Returns the client's `peer_id` alongside
/// the session and the re-keyed halves, since the server learns the peer
/// id here and has nowhere else to get it.
pub async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
        String,
    ),
    HandshakeError,
> {
    server_handshake_with_timeout(stream, key, HANDSHAKE_TIMEOUT).await
}

/// Same as [`server_handshake`], with the timeout as a parameter so tests
/// can use a short one instead of waiting out the real
/// [`HANDSHAKE_TIMEOUT`].
async fn server_handshake_with_timeout<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
    timeout: Duration,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
        String,
    ),
    HandshakeError,
> {
    match tokio::time::timeout(timeout, server_handshake_inner(stream, key)).await {
        Ok(result) => result,
        Err(_) => Err(HandshakeError::Timeout),
    }
}

async fn server_handshake_inner<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &SharedKey,
) -> Result<
    (
        TransportReader<ReadHalf<S>>,
        TransportWriter<WriteHalf<S>>,
        SessionId,
        String,
    ),
    HandshakeError,
> {
    let (mut reader, mut writer) = split(
        stream,
        key.clone(),
        SessionId::ZERO,
        Direction::ServerToClient,
    );

    let (client_nonce, peer_id) = server_handshake_read_only(&mut reader).await?;

    let mut server_nonce = [0u8; 32];
    getrandom::fill(&mut server_nonce).map_err(|_| HandshakeError::Random)?;

    // The server's own identifier is not yet part of this exchange: only
    // the client's peer_id is consumed by either side today. Sent as
    // empty rather than omitted, since the wire message still needs a
    // value in that field.
    writer
        .send(&Message::Handshake {
            version: PROTOCOL_VERSION,
            capabilities: 0,
            peer_id: String::new(),
            nonce: server_nonce,
        })
        .await?;

    let session = derive_session(&client_nonce, &server_nonce);
    let stream = reader.into_inner().unsplit(writer.into_inner());
    let (reader, writer) = split(stream, key.clone(), session, Direction::ServerToClient);
    Ok((reader, writer, session, peer_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split;
    use hop_proto::{Direction, SharedKey};
    use tokio::io::duplex;

    #[test]
    fn session_depends_on_both_nonces() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let base = derive_session(&a, &b);
        assert_ne!(base, derive_session(&[9u8; 32], &b), "client nonce ignored");
        assert_ne!(base, derive_session(&a, &[9u8; 32]), "server nonce ignored");
    }

    #[test]
    fn session_is_order_sensitive() {
        // Roles are asymmetric, so client-then-server must not equal
        // server-then-client. Otherwise a reflected handshake would
        // produce the same session.
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_ne!(derive_session(&a, &b), derive_session(&b, &a));
    }

    #[test]
    fn session_is_deterministic() {
        let a = [7u8; 32];
        let b = [8u8; 32];
        assert_eq!(derive_session(&a, &b), derive_session(&a, &b));
    }

    #[test]
    fn session_is_never_zero_for_real_nonces() {
        // ZERO is the documented "no handshake yet" placeholder. A real
        // handshake must never coincidentally produce it.
        assert_ne!(derive_session(&[0u8; 32], &[0u8; 32]), SessionId::ZERO);
    }

    #[tokio::test]
    async fn client_and_server_agree_on_a_session() {
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);

        let server_key = key.clone();
        let server = tokio::spawn(async move { server_handshake(a, &server_key).await });
        let (_client_reader, _client_writer, client_session) =
            client_handshake(b, &key, "pc").await.expect("client");
        let (_server_reader, _server_writer, server_session, peer_id) =
            server.await.unwrap().expect("server");

        assert_eq!(
            client_session, server_session,
            "peers must derive the same session"
        );
        assert_eq!(peer_id, "pc");
        assert_ne!(client_session, SessionId::ZERO);
    }

    #[tokio::test]
    async fn a_version_mismatch_is_refused() {
        // A peer speaking a different protocol version must be rejected
        // before any input is processed, not silently tolerated.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);
        let (mut sr, _sw) = split(a, key.clone(), SessionId::ZERO, Direction::ServerToClient);
        let (_cr, mut cw) = split(b, key, SessionId::ZERO, Direction::ClientToServer);

        cw.send(&Message::Handshake {
            version: PROTOCOL_VERSION + 1,
            capabilities: 0,
            peer_id: "pc".into(),
            nonce: [1u8; 32],
        })
        .await
        .unwrap();

        assert!(matches!(
            server_handshake_read_only(&mut sr).await,
            Err(HandshakeError::VersionMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn an_unexpected_keystroke_never_reaches_the_formatted_error() {
        // A desynchronized or hostile peer's first frame could be a
        // Message::Key, and any supervisor logs a handshake failure by
        // formatting this error. The keystroke's usage code must never
        // appear in that formatted string, only the message's kind.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);
        let (mut sr, _sw) = split(a, key.clone(), SessionId::ZERO, Direction::ServerToClient);
        let (_cr, mut cw) = split(b, key, SessionId::ZERO, Direction::ClientToServer);

        cw.send(&Message::Key {
            usage: hop_proto::Usage::C,
            pressed: true,
        })
        .await
        .unwrap();

        let error = server_handshake_read_only(&mut sr)
            .await
            .expect_err("a Key message is not a handshake");
        let rendered = error.to_string();

        assert!(
            !rendered.contains("Usage"),
            "rendered error must not name the usage field: {rendered}"
        );
        assert!(
            !rendered.contains('6'),
            "rendered error must not contain Usage::C's value: {rendered}"
        );
        assert!(
            !rendered.contains("pressed"),
            "rendered error must not contain the message's fields: {rendered}"
        );
        assert!(
            rendered.contains("Key"),
            "rendered error should still name the message kind: {rendered}"
        );
    }

    #[tokio::test]
    async fn input_after_the_handshake_is_bound_to_the_derived_session_not_zero() {
        // This is FINDING 5: client_handshake and server_handshake consume
        // the stream and hand back halves already re-split under the
        // derived session, so there is no SessionId::ZERO-keyed transport
        // for a caller to obtain and keep using for input. No input
        // message is ever sealed or opened under ZERO; only the two
        // Handshake messages are, entirely inside these two functions.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([4u8; 32]);

        let server_key = key.clone();
        let server = tokio::spawn(async move {
            let (mut r, mut w, _session, _peer_id) = server_handshake(a, &server_key)
                .await
                .expect("server handshake");
            let received = r.recv().await.expect("recv input");
            w.send(&Message::Heartbeat).await.expect("send input");
            received
        });

        let (mut r, mut w, _session) = client_handshake(b, &key, "pc")
            .await
            .expect("client handshake");
        let sent = Message::Key {
            usage: hop_proto::Usage::C,
            pressed: true,
        };
        w.send(&sent).await.expect("send input");
        let reply = r.recv().await.expect("recv input");

        assert_eq!(server.await.unwrap(), sent);
        assert_eq!(reply, Message::Heartbeat);
    }

    // CRITICAL 3 from the whole-branch review: a peer that completes the
    // TCP handshake and then sends nothing must be abandoned, not allowed
    // to block a handshake forever. Both tests use a short, test-only
    // timeout (via the `_with_timeout` helpers) rather than the real
    // five-second `HANDSHAKE_TIMEOUT`, so a passing run is fast; both are
    // also wrapped in an outer real-time bound so that if the timeout
    // wrapper were ever removed, the test fails on its own within a
    // couple of seconds instead of hanging CI, which has already
    // happened once in this project.
    #[tokio::test]
    async fn server_handshake_gives_up_on_a_silent_peer_instead_of_blocking_forever() {
        let (server_io, _client_io) = duplex(4096);
        let key = SharedKey::from_bytes([5u8; 32]);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            server_handshake_with_timeout(server_io, &key, Duration::from_millis(50)),
        )
        .await
        .expect(
            "server_handshake must give up on its own within its timeout, not hang until this \
             test's outer bound fires",
        );

        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }

    #[tokio::test]
    async fn client_handshake_gives_up_on_a_silent_peer_instead_of_blocking_forever() {
        // The client sends its own opening Handshake message first (see
        // client_handshake_inner), so the peer here has to at least
        // accept that write; it just never replies, exactly like a
        // server that accepted the TCP connection and then wedged.
        let (client_io, _server_io) = duplex(4096);
        let key = SharedKey::from_bytes([6u8; 32]);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client_handshake_with_timeout(client_io, &key, "pc", Duration::from_millis(50)),
        )
        .await
        .expect(
            "client_handshake must give up on its own within its timeout, not hang until this \
             test's outer bound fires",
        );

        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }
}
