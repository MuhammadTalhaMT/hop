#![forbid(unsafe_code)]

//! Wire protocol and cryptography for hop. Knows nothing about sockets.

pub mod keys;
pub use keys::Usage;
