use crate::Usage;
use serde::{Deserialize, Serialize};

/// Bumped only for incompatible wire changes. Peers exchange this in the
/// handshake and refuse to proceed if they disagree.
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Handshake {
        version: u16,
        capabilities: u32,
        peer_id: String,
    },
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: Button,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        usage: Usage,
        pressed: bool,
    },
    /// Sent on every control transition so the receiver cannot be left
    /// holding a modifier down forever.
    ReleaseAllKeys,
    Heartbeat,
    /// The client is handing control back to the server.
    Release,
    /// A variant this build does not understand. Decoding produces this
    /// instead of failing, so a newer peer can add message types without
    /// breaking an older one.
    Unknown,
}

// Explicit wire tags. Never renumber an existing one; only append.
//
// serde's own enum encoding cannot express "ignore variants you have not
// heard of", so the tag is carried explicitly and dispatched by hand.
// That is the whole reason this indirection exists.
const TAG_HANDSHAKE: u16 = 1;
const TAG_MOUSE_MOVE: u16 = 2;
const TAG_MOUSE_BUTTON: u16 = 3;
const TAG_SCROLL: u16 = 4;
const TAG_KEY: u16 = 5;
const TAG_RELEASE_ALL_KEYS: u16 = 6;
const TAG_HEARTBEAT: u16 = 7;
const TAG_RELEASE: u16 = 8;

#[derive(Serialize, Deserialize)]
struct Frame {
    tag: u16,
    body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("a message of unknown type cannot be encoded")]
    UnknownNotEncodable,
}

/// Encode an arbitrary tag and body. Exposed for tests that need to
/// simulate a peer speaking a future version of the protocol.
pub fn encode_raw<T: Serialize>(tag: u16, body: &T) -> Result<Vec<u8>, CodecError> {
    let frame = Frame {
        tag,
        body: postcard::to_allocvec(body)?,
    };
    Ok(postcard::to_allocvec(&frame)?)
}

pub fn encode(message: &Message) -> Result<Vec<u8>, CodecError> {
    match message {
        Message::Handshake {
            version,
            capabilities,
            peer_id,
        } => encode_raw(TAG_HANDSHAKE, &(*version, *capabilities, peer_id.clone())),
        Message::MouseMove { dx, dy } => encode_raw(TAG_MOUSE_MOVE, &(*dx, *dy)),
        Message::MouseButton { button, pressed } => {
            encode_raw(TAG_MOUSE_BUTTON, &(*button, *pressed))
        }
        Message::Scroll { dx, dy } => encode_raw(TAG_SCROLL, &(*dx, *dy)),
        Message::Key { usage, pressed } => encode_raw(TAG_KEY, &(*usage, *pressed)),
        Message::ReleaseAllKeys => encode_raw(TAG_RELEASE_ALL_KEYS, &()),
        Message::Heartbeat => encode_raw(TAG_HEARTBEAT, &()),
        Message::Release => encode_raw(TAG_RELEASE, &()),
        Message::Unknown => Err(CodecError::UnknownNotEncodable),
    }
}

pub fn decode(bytes: &[u8]) -> Result<Message, CodecError> {
    let frame: Frame = postcard::from_bytes(bytes)?;
    let message = match frame.tag {
        TAG_HANDSHAKE => {
            let (version, capabilities, peer_id) = postcard::from_bytes(&frame.body)?;
            Message::Handshake {
                version,
                capabilities,
                peer_id,
            }
        }
        TAG_MOUSE_MOVE => {
            let (dx, dy) = postcard::from_bytes(&frame.body)?;
            Message::MouseMove { dx, dy }
        }
        TAG_MOUSE_BUTTON => {
            let (button, pressed) = postcard::from_bytes(&frame.body)?;
            Message::MouseButton { button, pressed }
        }
        TAG_SCROLL => {
            let (dx, dy) = postcard::from_bytes(&frame.body)?;
            Message::Scroll { dx, dy }
        }
        TAG_KEY => {
            let (usage, pressed) = postcard::from_bytes(&frame.body)?;
            Message::Key { usage, pressed }
        }
        TAG_RELEASE_ALL_KEYS => Message::ReleaseAllKeys,
        TAG_HEARTBEAT => Message::Heartbeat,
        TAG_RELEASE => Message::Release,
        _ => Message::Unknown,
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;

    #[test]
    fn round_trips_every_variant() {
        let cases = vec![
            Message::Handshake {
                version: PROTOCOL_VERSION,
                capabilities: 0,
                peer_id: "pc".into(),
            },
            Message::MouseMove { dx: -3, dy: 7 },
            Message::MouseButton {
                button: Button::Left,
                pressed: true,
            },
            Message::Scroll { dx: 0, dy: -1 },
            Message::Key {
                usage: Usage::C,
                pressed: true,
            },
            Message::ReleaseAllKeys,
            Message::Heartbeat,
            Message::Release,
        ];
        for original in cases {
            let bytes = encode(&original).expect("encode");
            let decoded = decode(&bytes).expect("decode");
            assert_eq!(original, decoded, "round trip failed");
        }
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = encode(&Message::MouseMove { dx: 1, dy: 1 }).unwrap();
        let truncated = &bytes[..bytes.len() - 1];
        assert!(decode(truncated).is_err());
    }

    #[test]
    fn unknown_message_types_decode_to_unknown() {
        // A newer peer sending a message this build has never heard of
        // must not break the connection. This is what lets clipboard
        // support ship later without breaking older installs.
        let future = encode_raw(9999, &()).expect("encode");
        assert_eq!(decode(&future).expect("decode"), Message::Unknown);
    }
}
