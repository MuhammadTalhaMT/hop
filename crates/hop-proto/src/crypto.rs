use crate::{decode, encode, Message};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

const SEQ_LEN: usize = 8;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const MIN_FRAME: usize = SEQ_LEN + NONCE_LEN + TAG_LEN;

/// The pre-shared 32 byte secret. Both machines hold the same value.
#[derive(Clone)]
pub struct SharedKey([u8; 32]);

impl SharedKey {
    pub fn generate() -> SharedKey {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("system RNG unavailable");
        SharedKey(bytes)
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

pub fn seal(key: &SharedKey, seq: u64, message: &Message) -> Result<Vec<u8>, CryptoError> {
    let plaintext = encode(message)?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|_| CryptoError::Random)?;
    let nonce: XNonce = nonce_bytes.into();

    let key_arr: Key = (*key.as_bytes()).into();
    let cipher = XChaCha20Poly1305::new(&key_arr);
    let aad = seq.to_be_bytes();
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
    frame.extend_from_slice(&aad);
    frame.extend_from_slice(&nonce_bytes);
    frame.extend_from_slice(&ciphertext);
    Ok(frame)
}

pub fn open(key: &SharedKey, frame: &[u8]) -> Result<(u64, Message), CryptoError> {
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

    let key_arr: Key = (*key.as_bytes()).into();
    let cipher = XChaCha20Poly1305::new(&key_arr);
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: &seq_arr,
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

    #[test]
    fn seals_and_opens_round_trip() {
        let key = SharedKey::generate();
        let sealed = seal(&key, 7, &sample()).expect("seal");
        let (seq, message) = open(&key, &sealed).expect("open");
        assert_eq!(seq, 7);
        assert_eq!(message, sample());
    }

    #[test]
    fn rejects_wrong_key() {
        let sealed = seal(&SharedKey::generate(), 1, &sample()).unwrap();
        assert!(open(&SharedKey::generate(), &sealed).is_err());
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        let key = SharedKey::generate();
        let mut sealed = seal(&key, 1, &sample()).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open(&key, &sealed).is_err());
    }

    #[test]
    fn rejects_tampered_sequence_number() {
        let key = SharedKey::generate();
        let mut sealed = seal(&key, 1, &sample()).unwrap();
        sealed[0] ^= 0xFF; // seq is authenticated, so this must fail the tag
        assert!(open(&key, &sealed).is_err());
    }

    #[test]
    fn rejects_short_frame() {
        let key = SharedKey::generate();
        assert!(open(&key, &[0u8; 8]).is_err());
    }

    #[test]
    fn nonces_differ_between_messages() {
        let key = SharedKey::generate();
        let a = seal(&key, 1, &sample()).unwrap();
        let b = seal(&key, 1, &sample()).unwrap();
        assert_ne!(
            a, b,
            "identical plaintexts must not produce identical frames"
        );
    }
}
