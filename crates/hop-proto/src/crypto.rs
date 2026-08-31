use crate::{decode, encode, Message};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const SEQ_LEN: usize = 8;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const MIN_FRAME: usize = SEQ_LEN + NONCE_LEN + TAG_LEN;
const DIR_LEN: usize = 1;

/// Which of the two peers sealed a frame, bound into its authenticated
/// data alongside the session and sequence number.
///
/// The session and the replay window bind a connection, but not which
/// side sent a given frame: without this, an echoed frame (a network loop,
/// or an attacker who simply reflects traffic back at its sender) is new
/// to the receiving side's replay window and authenticates as genuine
/// inbound traffic under the correct key and session. Binding the
/// direction closes that: a `TransportWriter` seals under its own
/// direction, and the paired `TransportReader` on the same side opens
/// expecting the OPPOSITE direction, so a frame this side sent itself can
/// never authenticate coming back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    /// The direction traffic flows on the other leg of the same
    /// connection.
    pub fn opposite(self) -> Direction {
        match self {
            Direction::ClientToServer => Direction::ServerToClient,
            Direction::ServerToClient => Direction::ClientToServer,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Direction::ClientToServer => 0,
            Direction::ServerToClient => 1,
        }
    }
}

/// Identifies one session between two peers, mixed into every frame's
/// authenticated data so a frame sealed under one session cannot
/// authenticate under another.
///
/// `hop_core::handshake` derives a fresh, per-session `SessionId` from both
/// peers' exchanged nonces (see `Message::Handshake`'s `nonce` field)
/// before any input message is processed, which is what makes this
/// binding real: a frame recorded on one session cannot be replayed into
/// a later one, since the later session's `SessionId` differs and the
/// AEAD tag will not authenticate under it.
///
/// [`SessionId::ZERO`] is not a real session. It is used only for the
/// brief window in which the handshake itself runs, before either peer
/// has anything to derive a real session from; see `hop_core::handshake`'s
/// module doc comment for why that is safe. It must never be used for
/// anything other than exchanging the two `Handshake` messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(pub [u8; 32]);

impl SessionId {
    /// The placeholder session used only while the handshake itself is in
    /// flight, before a real session has been derived. See the type's doc
    /// comment.
    pub const ZERO: SessionId = SessionId([0u8; 32]);
}

/// The pre-shared 32 byte secret. Both machines hold the same value.
#[derive(Clone)]
pub struct SharedKey([u8; 32]);

impl SharedKey {
    pub fn generate() -> Result<SharedKey, CryptoError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| CryptoError::Random)?;
        Ok(SharedKey(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> SharedKey {
        SharedKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for SharedKey {
    /// Never print key material, including into logs or panic messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedKey(redacted)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("frame too short")]
    TooShort,
    #[error("authentication failed")]
    Authentication,
    #[error("codec: {0}")]
    Codec(#[from] crate::CodecError),
    #[error("system RNG unavailable")]
    Random,
}

/// Build the associated data a frame is authenticated under: the session
/// bytes, the direction byte, then the 8 big-endian sequence bytes. Only
/// the sequence bytes go on the wire in the clear; the session and
/// direction are never transmitted, since both peers already know them.
fn associated_data(
    session: SessionId,
    direction: Direction,
    seq_bytes: [u8; SEQ_LEN],
) -> [u8; 32 + DIR_LEN + SEQ_LEN] {
    let mut aad = [0u8; 32 + DIR_LEN + SEQ_LEN];
    aad[..32].copy_from_slice(&session.0);
    aad[32] = direction.tag();
    aad[33..].copy_from_slice(&seq_bytes);
    aad
}

/// Seal a frame as having been sent in `direction`. The paired reader on
/// the far side must open it expecting the same `direction`; a reader on
/// the SAME side as this writer must open expecting `direction.opposite()`,
/// which is what stops a reflected frame from authenticating. See
/// [`Direction`]'s doc comment.
pub fn seal(
    key: &SharedKey,
    session: SessionId,
    direction: Direction,
    seq: u64,
    message: &Message,
) -> Result<Vec<u8>, CryptoError> {
    let plaintext = encode(message)?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|_| CryptoError::Random)?;
    let nonce: XNonce = nonce_bytes.into();

    let cipher = XChaCha20Poly1305::new(key.as_bytes().into());
    let seq_bytes = seq.to_be_bytes();
    let aad = associated_data(session, direction, seq_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Authentication)?;

    let mut frame = Vec::with_capacity(SEQ_LEN + NONCE_LEN + ciphertext.len());
    frame.extend_from_slice(&seq_bytes);
    frame.extend_from_slice(&nonce_bytes);
    frame.extend_from_slice(&ciphertext);
    Ok(frame)
}

/// Open a frame, requiring it to have been sealed under `direction`. See
/// [`seal`] for which direction a caller should pass.
pub fn open(
    key: &SharedKey,
    session: SessionId,
    direction: Direction,
    frame: &[u8],
) -> Result<(u64, Message), CryptoError> {
    if frame.len() < MIN_FRAME {
        return Err(CryptoError::TooShort);
    }
    let (seq_bytes, rest) = frame.split_at(SEQ_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);

    let mut seq_arr = [0u8; SEQ_LEN];
    seq_arr.copy_from_slice(seq_bytes);
    let seq = u64::from_be_bytes(seq_arr);

    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(nonce_bytes);
    let nonce: XNonce = nonce_arr.into();

    let cipher = XChaCha20Poly1305::new(key.as_bytes().into());
    let aad = associated_data(session, direction, seq_arr);
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Authentication)?;

    Ok((seq, decode(&plaintext)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Usage};

    fn sample() -> Message {
        Message::Key {
            usage: Usage::C,
            pressed: true,
        }
    }

    fn session() -> SessionId {
        SessionId([5u8; 32])
    }

    /// The direction used by every test that does not care which one, so
    /// changing this in one place changes it everywhere.
    fn direction() -> Direction {
        Direction::ClientToServer
    }

    #[test]
    fn seals_and_opens_round_trip() {
        let key = SharedKey::generate().unwrap();
        let sealed = seal(&key, session(), direction(), 7, &sample()).expect("seal");
        let (seq, message) = open(&key, session(), direction(), &sealed).expect("open");
        assert_eq!(seq, 7);
        assert_eq!(message, sample());
    }

    #[test]
    fn rejects_wrong_key() {
        let sealed = seal(
            &SharedKey::generate().unwrap(),
            session(),
            direction(),
            1,
            &sample(),
        )
        .unwrap();
        assert!(open(
            &SharedKey::generate().unwrap(),
            session(),
            direction(),
            &sealed
        )
        .is_err());
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        let key = SharedKey::generate().unwrap();
        let mut sealed = seal(&key, session(), direction(), 1, &sample()).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open(&key, session(), direction(), &sealed).is_err());
    }

    #[test]
    fn rejects_tampered_sequence_number() {
        let key = SharedKey::generate().unwrap();
        let mut sealed = seal(&key, session(), direction(), 1, &sample()).unwrap();
        sealed[0] ^= 0xFF; // seq is authenticated, so this must fail the tag
        assert!(open(&key, session(), direction(), &sealed).is_err());
    }

    #[test]
    fn rejects_short_frame() {
        let key = SharedKey::generate().unwrap();
        assert!(open(&key, session(), direction(), &[]).is_err());
        assert!(open(&key, session(), direction(), &[0u8; 8]).is_err());
        assert!(open(&key, session(), direction(), &[0u8; MIN_FRAME - 1]).is_err());
    }

    #[test]
    fn nonces_differ_between_messages() {
        let key = SharedKey::generate().unwrap();
        let a = seal(&key, session(), direction(), 1, &sample()).unwrap();
        let b = seal(&key, session(), direction(), 1, &sample()).unwrap();
        // Compare the nonce field itself (bytes 8..32), not the whole
        // frame: comparing whole frames would still pass on a fixed-nonce
        // implementation that varied some other byte.
        assert_ne!(
            a[SEQ_LEN..SEQ_LEN + NONCE_LEN],
            b[SEQ_LEN..SEQ_LEN + NONCE_LEN],
            "identical plaintexts must not produce identical nonces"
        );
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let key = SharedKey::generate().unwrap();
        let plaintext = encode(&sample()).expect("encode");
        let sealed = seal(&key, session(), direction(), 1, &sample()).unwrap();
        assert!(
            !sealed
                .windows(plaintext.len())
                .any(|window| window == plaintext.as_slice()),
            "the encoded plaintext must not appear verbatim in the sealed frame"
        );
    }

    #[test]
    fn rejects_a_frame_sealed_under_a_different_session() {
        // A frame recorded on one session (for example a captured typing
        // session replayed by an attacker who has become the client's
        // server, via rogue mDNS or ARP spoofing) must not authenticate
        // under a different session, even with the correct shared key and
        // a fresh replay window.
        let key = SharedKey::generate().unwrap();
        let sealed = seal(&key, SessionId([1u8; 32]), direction(), 1, &sample()).unwrap();
        assert!(matches!(
            open(&key, SessionId([2u8; 32]), direction(), &sealed),
            Err(CryptoError::Authentication)
        ));
    }

    #[test]
    fn rejects_a_frame_sealed_under_the_other_direction() {
        // This is FINDING 2: the session and the replay window bind a
        // connection, but not which side sent a frame. Without direction
        // in the AAD, a frame sealed as ClientToServer authenticates just
        // as well when opened expecting ServerToClient, which is exactly
        // what lets a reflected frame (our own outbound heartbeat, echoed
        // back to us) pass as genuine inbound traffic.
        let key = SharedKey::generate().unwrap();
        let sealed = seal(&key, session(), Direction::ClientToServer, 1, &sample()).unwrap();
        assert!(matches!(
            open(&key, session(), Direction::ServerToClient, &sealed),
            Err(CryptoError::Authentication)
        ));
    }
}
