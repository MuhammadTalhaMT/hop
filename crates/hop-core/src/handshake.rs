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
//! transport to exchange messages over. This module resolves that by
//! running the handshake over a transport that was split with
//! [`SessionId::ZERO`]. That is safe because the handshake exchanges
//! nothing but the two `Handshake` messages themselves: their freshness
//! comes from the random nonces they carry, not from session binding, and
//! neither `client_handshake` nor `server_handshake` ever sends or receives
//! anything else. Once both peers derive the real session, the caller must
//! move the transport onto it before any input message is sent or
//! received; `TransportReader::into_inner` and `TransportWriter::into_inner`
//! exist for exactly this, so the stream can be reassembled (for example
//! with `tokio::io::ReadHalf::unsplit`) and re-split under the derived
//! session. `client_handshake` and `server_handshake` hand back the derived
//! `SessionId` rather than mutating the transport themselves, so a caller
//! cannot start pumping input without first doing something with that
//! value.

use crate::{TransportError, TransportReader, TransportWriter};
use hop_proto::{Message, SessionId, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

/// Derive the session identifier both peers will authenticate every frame
/// against.
///
/// Both nonces contribute, so neither peer can choose the session alone,
/// and a recording made under one session cannot be replayed into
/// another. The order is fixed by role rather than sorted, so a reflected
/// handshake produces a different session than the genuine one.
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
    #[error("peer sent {0:?} instead of a handshake")]
    Unexpected(Message),
    #[error("system RNG unavailable")]
    Random,
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
        other => Err(HandshakeError::Unexpected(other)),
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

/// Perform the client side of the handshake: send a fresh nonce, receive
/// the server's, and derive the session both sides will use from here on.
///
/// `reader` and `writer` must be a transport split with
/// [`SessionId::ZERO`]; see the module doc comment for why that is safe
/// here and why the caller must move onto the returned session before any
/// further message is sent or received.
pub async fn client_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut TransportReader<R>,
    writer: &mut TransportWriter<W>,
    peer_id: &str,
) -> Result<SessionId, HandshakeError> {
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

    let (server_nonce, _server_peer_id) = recv_handshake(reader).await?;
    Ok(derive_session(&client_nonce, &server_nonce))
}

/// Perform the server side of the handshake: receive the client's nonce
/// and peer id, reply with a fresh nonce of our own, and derive the
/// session both sides will use from here on.
///
/// `reader` and `writer` must be a transport split with
/// [`SessionId::ZERO`]; see the module doc comment for why that is safe
/// here and why the caller must move onto the returned session before any
/// further message is sent or received.
///
/// Returns the client's `peer_id` alongside the session, since the server
/// learns it here and has nowhere else to get it.
pub async fn server_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut TransportReader<R>,
    writer: &mut TransportWriter<W>,
) -> Result<(SessionId, String), HandshakeError> {
    let (client_nonce, peer_id) = server_handshake_read_only(reader).await?;

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

    Ok((derive_session(&client_nonce, &server_nonce), peer_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split;
    use hop_proto::SharedKey;
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
        let (mut sr, mut sw) = split(a, key.clone(), SessionId::ZERO);
        let (mut cr, mut cw) = split(b, key, SessionId::ZERO);

        let server = tokio::spawn(async move { server_handshake(&mut sr, &mut sw).await });
        let client = client_handshake(&mut cr, &mut cw, "pc")
            .await
            .expect("client");
        let (server_session, peer_id) = server.await.unwrap().expect("server");

        assert_eq!(client, server_session, "peers must derive the same session");
        assert_eq!(peer_id, "pc");
        assert_ne!(client, SessionId::ZERO);
    }

    #[tokio::test]
    async fn a_version_mismatch_is_refused() {
        // A peer speaking a different protocol version must be rejected
        // before any input is processed, not silently tolerated.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);
        let (mut sr, _sw) = split(a, key.clone(), SessionId::ZERO);
        let (_cr, mut cw) = split(b, key, SessionId::ZERO);

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
    async fn input_after_the_handshake_is_bound_to_the_derived_session_not_zero() {
        // Exercises the full resolution to the split/handshake
        // chicken-and-egg problem described in the module doc comment: the
        // handshake runs entirely under SessionId::ZERO, then both peers
        // reclaim the raw stream, unsplit it, and re-split under the
        // derived session. No input message is ever sealed or opened
        // under ZERO; only the two Handshake messages are.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([4u8; 32]);
        let (mut sr, mut sw) = split(a, key.clone(), SessionId::ZERO);
        let (mut cr, mut cw) = split(b, key.clone(), SessionId::ZERO);

        let server_key = key.clone();
        let server = tokio::spawn(async move {
            let (session, _peer_id) = server_handshake(&mut sr, &mut sw)
                .await
                .expect("server handshake");
            let stream = sr.into_inner().unsplit(sw.into_inner());
            let (mut r, mut w) = split(stream, server_key, session);
            let received = r.recv().await.expect("recv input");
            w.send(&Message::Heartbeat).await.expect("send input");
            received
        });

        let session = client_handshake(&mut cr, &mut cw, "pc")
            .await
            .expect("client handshake");
        let stream = cr.into_inner().unsplit(cw.into_inner());
        let (mut r, mut w) = split(stream, key, session);
        let sent = Message::Key {
            usage: hop_proto::Usage::C,
            pressed: true,
        };
        w.send(&sent).await.expect("send input");
        let reply = r.recv().await.expect("recv input");

        assert_eq!(server.await.unwrap(), sent);
        assert_eq!(reply, Message::Heartbeat);
    }
}
