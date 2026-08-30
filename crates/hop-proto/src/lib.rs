#![forbid(unsafe_code)]

//! Wire protocol and cryptography for hop. Knows nothing about sockets.

pub mod keys;
pub use keys::Usage;

pub mod message;
pub use message::{decode, encode, encode_raw, Button, CodecError, Message, PROTOCOL_VERSION};
