//! Receiving a file the peer copied, safely.
//!
//! Everything here treats the sender as untrusted. A peer telling us to
//! write a file to disk is the most dangerous thing in this protocol, so
//! the rules are:
//!
//! - The offered name is a bare file name, never a path. A name containing
//!   a separator, or any `..` component, or an absolute prefix, is
//!   rejected outright rather than sanitised, because sanitising path
//!   traversal is a game you lose eventually.
//! - Nothing is written outside the staging directory the caller chooses.
//! - The offered size is a cap, not a promise. A peer that keeps sending
//!   chunks past it is aborted rather than allowed to fill the disk.
//! - A transfer that never completes leaves no file on the clipboard, so
//!   a truncated file is never presented as a whole one.

use std::path::{Path, PathBuf};

/// Largest file hop will accept. Copying something larger is skipped with
/// a log line rather than saturating the network for a file the user may
/// never paste.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// How much file content goes in one frame.
///
/// The transport refuses frames above 64 KiB, and a chunk has to fit in
/// one alongside its framing, sequence number, nonce and tag. 48 KiB
/// leaves ample room without wasting round trips.
pub const CHUNK_BYTES: usize = 48 * 1024;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FileError {
    #[error("the peer offered a file name that is not a bare name: {0:?}")]
    UnsafeName(String),
    #[error("the peer offered {size} bytes, over the {max} byte limit")]
    TooLarge { size: u64, max: u64 },
    #[error("the peer sent more data than it offered")]
    Overrun,
    #[error("received a file chunk without an offer")]
    NoOffer,
}

/// Reject anything that is not a plain file name.
///
/// Deliberately a whitelist of shape rather than a blacklist of tricks:
/// the name must have exactly one component and must not be a special
/// directory entry. That rules out `../../etc/passwd`, `/etc/passwd`,
/// `C:\Windows\x`, and the backslash forms Windows accepts, without
/// needing to enumerate them.
pub fn safe_file_name(name: &str) -> Result<&str, FileError> {
    let bad = name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
        || Path::new(name).components().count() != 1;
    if bad {
        return Err(FileError::UnsafeName(name.to_string()));
    }
    Ok(name)
}

/// A file being received, one chunk at a time.
#[derive(Debug)]
pub struct FileReceive {
    staging: PathBuf,
    name: String,
    expected: u64,
    received: u64,
    buffer: Vec<u8>,
}

impl FileReceive {
    /// Begin receiving, validating what the peer claims before any of it
    /// is believed.
    pub fn begin(staging: &Path, name: &str, size: u64) -> Result<Self, FileError> {
        let name = safe_file_name(name)?;
        if size > MAX_FILE_BYTES {
            return Err(FileError::TooLarge {
                size,
                max: MAX_FILE_BYTES,
            });
        }
        Ok(Self {
            staging: staging.to_path_buf(),
            name: name.to_string(),
            expected: size,
            received: 0,
            buffer: Vec::new(),
        })
    }

    /// Take one chunk. Refuses to accept more than was offered, so a peer
    /// cannot use a small offer to smuggle a large file.
    pub fn chunk(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        let would_be = self.received.saturating_add(bytes.len() as u64);
        if would_be > self.expected {
            return Err(FileError::Overrun);
        }
        self.received = would_be;
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Where the completed file will be written.
    pub fn destination(&self) -> PathBuf {
        self.staging.join(&self.name)
    }

    pub fn is_complete(&self) -> bool {
        self.received == self.expected
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn expected(&self) -> u64 {
        self.expected
    }

    /// The bytes, for the caller to write out once complete.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }
}

/// Send whatever the local clipboard change was: text in one frame, or a
/// file as an offer, a run of chunks, and an end marker.
///
/// Reading the file here rather than in `ClipboardSync` keeps that module
/// free of I/O and therefore testable without a filesystem. A file that
/// cannot be read, or is too large, is skipped with a log line: the peer's
/// clipboard is left alone rather than being handed something partial.
pub async fn send_local_change<W>(
    writer: &mut crate::TransportWriter<W>,
    change: crate::clipboard::LocalChange,
) -> Result<(), crate::TransportError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use crate::clipboard::LocalChange;
    use hop_proto::Message;

    match change {
        LocalChange::Text(text) => {
            tracing::debug!(bytes = text.len(), "forwarding copied text to the peer");
            writer.send(&Message::ClipboardText(text)).await
        }
        LocalChange::File(path) => {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    // A directory, a permissions problem, or a file that
                    // vanished between the copy and now.
                    tracing::debug!(%error, "could not read the copied file; not sending it");
                    return Ok(());
                }
            };
            if bytes.len() as u64 > MAX_FILE_BYTES {
                tracing::info!(
                    bytes = bytes.len(),
                    limit = MAX_FILE_BYTES,
                    "copied file is over the size limit; not sending it"
                );
                return Ok(());
            }
            let name = match std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
            {
                Some(name) => name.to_string(),
                None => {
                    tracing::debug!("copied path has no file name; not sending it");
                    return Ok(());
                }
            };

            tracing::info!(bytes = bytes.len(), "sending a copied file to the peer");
            writer
                .send(&Message::FileOffer {
                    name,
                    size: bytes.len() as u64,
                })
                .await?;
            for chunk in bytes.chunks(CHUNK_BYTES) {
                writer.send(&Message::FileChunk(chunk.to_vec())).await?;
            }
            writer.send(&Message::FileEnd).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_accepted() {
        assert!(safe_file_name("notes.txt").is_ok());
        assert!(safe_file_name("a file with spaces.pdf").is_ok());
        assert!(safe_file_name("caf\u{e9}.txt").is_ok());
    }

    #[test]
    fn path_traversal_is_refused_not_sanitised() {
        // The peer chooses this string. Every one of these would write
        // outside the staging directory if it were merely cleaned up.
        for name in [
            "../secret",
            "../../etc/passwd",
            "/etc/passwd",
            "sub/dir.txt",
            "..\\windows\\system32",
            "C:\\Windows\\evil.exe",
            "..",
            ".",
            "",
        ] {
            assert!(
                safe_file_name(name).is_err(),
                "{name:?} should have been refused"
            );
        }
    }

    #[test]
    fn a_name_with_a_null_byte_is_refused() {
        // Would truncate the path at the OS boundary.
        assert!(safe_file_name("evil\0.txt").is_err());
    }

    #[test]
    fn an_oversized_offer_is_refused_before_any_data_arrives() {
        let err = FileReceive::begin(Path::new("/tmp"), "big.bin", MAX_FILE_BYTES + 1).unwrap_err();
        assert!(matches!(err, FileError::TooLarge { .. }));
    }

    #[test]
    fn chunks_accumulate_to_completion() {
        let mut rx = FileReceive::begin(Path::new("/tmp"), "notes.txt", 5).expect("begin");
        assert!(!rx.is_complete());
        rx.chunk(b"hel").expect("chunk");
        rx.chunk(b"lo").expect("chunk");
        assert!(rx.is_complete());
        assert_eq!(rx.into_bytes(), b"hello");
    }

    #[test]
    fn a_peer_cannot_send_more_than_it_offered() {
        // Otherwise a one byte offer could be used to fill the disk.
        let mut rx = FileReceive::begin(Path::new("/tmp"), "small.bin", 2).expect("begin");
        assert_eq!(rx.chunk(b"toolong"), Err(FileError::Overrun));
    }

    #[test]
    fn the_destination_stays_inside_the_staging_directory() {
        let rx = FileReceive::begin(Path::new("/tmp/stage"), "notes.txt", 1).expect("begin");
        assert_eq!(rx.destination(), Path::new("/tmp/stage/notes.txt"));
        assert!(rx.destination().starts_with("/tmp/stage"));
    }

    #[test]
    fn an_incomplete_transfer_is_never_complete() {
        // The caller only publishes a file to the clipboard once this is
        // true, so a truncated file must never report otherwise.
        let mut rx = FileReceive::begin(Path::new("/tmp"), "notes.txt", 10).expect("begin");
        rx.chunk(b"partial").expect("chunk");
        assert!(!rx.is_complete());
    }
}
