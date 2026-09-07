use crate::Usage;
use serde::{Deserialize, Serialize};

/// Bumped only for incompatible wire changes. Peers exchange this in the
/// handshake and refuse to proceed if they disagree.
pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Handshake {
        version: u16,
        capabilities: u32,
        peer_id: String,
        nonce: [u8; 32],
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
    /// The client is handing control back to the server, leaving at
    /// `along` on its return edge. Same units and meaning as `Enter`.
    Release {
        along: f32,
    },
    /// Focus has just crossed onto the peer, entering at `along` on the
    /// entry edge.
    ///
    /// `along` is in the SENDER's logical units (macOS points, Windows
    /// pixels), measured relative to that machine's anchor: the point on
    /// its edge that hop treats as the same physical place as the peer's
    /// anchor (see `hop_core::Screen::anchor`). Signed, and free to fall
    /// outside the receiver's own edge, in which case the receiver clamps
    /// to the nearest corner.
    ///
    /// Not a fraction of the edge. A fraction stretches motion by the
    /// ratio of the two edge widths, so a hand moving diagonally changes
    /// direction at the boundary, and it makes the middle of one machine
    /// map to the seam between two of the other's monitors.
    Enter {
        along: f32,
    },
    /// The sender's clipboard now holds this text. Sent when either side
    /// notices its own clipboard changed, so a copy on one machine can be
    /// pasted on the other.
    ///
    /// Text only. Images are deliberately not carried here; files have
    /// their own chunked messages below, because a file does not fit in
    /// one frame and the frame cap is what keeps a peer from making us
    /// allocate arbitrarily.
    ClipboardText(String),
    /// Start of a file the peer has copied. Followed by `FileChunk`s and
    /// then `FileEnd`.
    ///
    /// `name` is a bare file name, never a path: it is used to name a file
    /// written on this machine, so accepting a path would let a peer
    /// choose where to write. The receiver validates this rather than
    /// trusting it.
    FileOffer {
        name: String,
        size: u64,
    },
    /// One piece of the file currently being offered. Sized by the sender
    /// to fit within the transport's frame cap.
    FileChunk(Vec<u8>),
    /// The file is complete and can be put on the clipboard.
    FileEnd,
    /// The sender gave up part way through, so the receiver should discard
    /// what it has rather than leaving a truncated file behind.
    FileAbort,
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
const TAG_CLIPBOARD_TEXT: u16 = 9;
const TAG_FILE_OFFER: u16 = 10;
const TAG_FILE_CHUNK: u16 = 11;
const TAG_FILE_END: u16 = 12;
const TAG_FILE_ABORT: u16 = 13;
const TAG_ENTER: u16 = 14;

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
            nonce,
        } => encode_raw(
            TAG_HANDSHAKE,
            &(*version, *capabilities, peer_id.clone(), *nonce),
        ),
        Message::MouseMove { dx, dy } => encode_raw(TAG_MOUSE_MOVE, &(*dx, *dy)),
        Message::MouseButton { button, pressed } => {
            encode_raw(TAG_MOUSE_BUTTON, &(*button, *pressed))
        }
        Message::Scroll { dx, dy } => encode_raw(TAG_SCROLL, &(*dx, *dy)),
        Message::Key { usage, pressed } => encode_raw(TAG_KEY, &(*usage, *pressed)),
        Message::ReleaseAllKeys => encode_raw(TAG_RELEASE_ALL_KEYS, &()),
        Message::Heartbeat => encode_raw(TAG_HEARTBEAT, &()),
        Message::Release { along } => encode_raw(TAG_RELEASE, along),
        Message::ClipboardText(text) => encode_raw(TAG_CLIPBOARD_TEXT, text),
        Message::FileOffer { name, size } => encode_raw(TAG_FILE_OFFER, &(name.clone(), *size)),
        Message::FileChunk(bytes) => encode_raw(TAG_FILE_CHUNK, bytes),
        Message::FileEnd => encode_raw(TAG_FILE_END, &()),
        Message::FileAbort => encode_raw(TAG_FILE_ABORT, &()),
        Message::Enter { along } => encode_raw(TAG_ENTER, along),
        Message::Unknown => Err(CodecError::UnknownNotEncodable),
    }
}

pub fn decode(bytes: &[u8]) -> Result<Message, CodecError> {
    let frame: Frame = postcard::from_bytes(bytes)?;
    let message = match frame.tag {
        TAG_HANDSHAKE => {
            let (version, capabilities, peer_id, nonce) = postcard::from_bytes(&frame.body)?;
            Message::Handshake {
                version,
                capabilities,
                peer_id,
                nonce,
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
        TAG_RELEASE => {
            let along: f32 = postcard::from_bytes(&frame.body)?;
            Message::Release { along }
        }
        TAG_CLIPBOARD_TEXT => {
            let text: String = postcard::from_bytes(&frame.body)?;
            Message::ClipboardText(text)
        }
        TAG_FILE_OFFER => {
            let (name, size) = postcard::from_bytes(&frame.body)?;
            Message::FileOffer { name, size }
        }
        TAG_FILE_CHUNK => {
            let bytes: Vec<u8> = postcard::from_bytes(&frame.body)?;
            Message::FileChunk(bytes)
        }
        TAG_FILE_END => Message::FileEnd,
        TAG_FILE_ABORT => Message::FileAbort,
        TAG_ENTER => {
            let along: f32 = postcard::from_bytes(&frame.body)?;
            Message::Enter { along }
        }
        _ => Message::Unknown,
    };
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;

    #[test]
    fn clipboard_text_round_trips_including_awkward_content() {
        // Clipboard text is arbitrary user data: newlines, unicode, emoji
        // and an empty copy all have to survive the wire unchanged.
        for text in [
            "hello",
            "",
            "line one\nline two\r\nline three",
            "unicode: caf\u{e9} \u{4f60}\u{597d} \u{1f600}",
            "  leading and trailing whitespace  ",
        ] {
            let original = Message::ClipboardText(text.to_string());
            let decoded = decode(&encode(&original).expect("encode")).expect("decode");
            assert_eq!(original, decoded, "clipboard text did not round trip");
        }
    }

    #[test]
    fn a_peer_that_does_not_know_clipboard_ignores_it() {
        // Clipboard was added after the first release, so an older peer
        // must treat the new tag as Unknown rather than erroring out and
        // dropping the connection.
        let future = encode_raw(9999, &"some clipboard text").expect("encode");
        assert_eq!(decode(&future).expect("decode"), Message::Unknown);
    }

    #[test]
    fn round_trips_every_variant() {
        let cases = vec![
            Message::Handshake {
                version: PROTOCOL_VERSION,
                capabilities: 0,
                peer_id: "pc".into(),
                nonce: [9u8; 32],
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
            Message::Release { along: -204.5 },
            Message::Enter { along: 1337.25 },
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
