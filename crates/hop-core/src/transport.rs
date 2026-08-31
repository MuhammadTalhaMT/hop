use hop_proto::{open, seal, Message, ReplayWindow, SessionId, SharedKey};
use tokio::io::{
    split as io_split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf,
};

/// Refuse absurd frames rather than allocating whatever a peer claims.
const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decryption or authentication failed")]
    Crypto,
    #[error("replayed frame refused")]
    Replay,
    #[error("frame too large")]
    FrameTooLarge,
    /// The peer closed cleanly, between frames.
    #[error("peer closed the connection")]
    Closed,
    /// The peer vanished part way through a frame. Distinct from `Closed`
    /// because a supervisor should treat a truncation as a fault worth
    /// logging or rate limiting, not as a graceful shutdown.
    #[error("connection ended mid-frame")]
    Truncated,
}

/// Convert a raw length prefix into a length we are willing to allocate.
/// The cap MUST be enforced here, before any buffer is created, so that
/// a peer cannot make us allocate whatever size it claims.
fn validated_len(prefix: [u8; 4]) -> Result<usize, TransportError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME {
        return Err(TransportError::FrameTooLarge);
    }
    Ok(len)
}

/// The receiving half of a split transport. Owns the replay window for its
/// direction.
///
/// Generic over the stream so tests can drive it through an in-memory
/// duplex pipe rather than a real socket.
pub struct TransportReader<R> {
    stream: R,
    key: SharedKey,
    session: SessionId,
    replay: ReplayWindow,
}

/// The sending half of a split transport. Owns the outbound sequence
/// counter for its direction.
///
/// Generic over the stream so tests can drive it through an in-memory
/// duplex pipe rather than a real socket.
pub struct TransportWriter<W> {
    stream: W,
    key: SharedKey,
    session: SessionId,
    send_seq: u64,
}

/// Split a stream into a reader and a writer half that can be driven from
/// different tasks.
///
/// Each direction keeps its own sequence counter and its own replay
/// window, so the two directions cannot be mistaken for replays of each
/// other, and a caller can await `recv` on one task while `send` runs on
/// another (or on the same task interleaved with a heartbeat timer),
/// which a single `&mut self` type could never allow.
pub fn split<S: AsyncRead + AsyncWrite>(
    stream: S,
    key: SharedKey,
    session: SessionId,
) -> (TransportReader<ReadHalf<S>>, TransportWriter<WriteHalf<S>>) {
    let (r, w) = io_split(stream);
    (
        TransportReader {
            stream: r,
            key: key.clone(),
            session,
            replay: ReplayWindow::new(),
        },
        TransportWriter {
            stream: w,
            key,
            session,
            send_seq: 0,
        },
    )
}

impl<W: AsyncWrite + Unpin> TransportWriter<W> {
    /// Note for callers: after any `Err(TransportError::Io(_))` here, a
    /// partial frame may already be sitting on the wire (the length prefix
    /// or part of the frame body may have been written before the write
    /// failed). The stream is desynchronized at that point, so this
    /// `TransportWriter` must be discarded and the connection
    /// re-established, not reused. Also note the asymmetry with `recv` on
    /// `TransportReader`: a peer that has gone away surfaces from `recv` as
    /// `Closed`, but surfaces from `send` as an `Io` error, since writing to
    /// a dead peer fails at the OS level rather than reading a clean EOF.
    pub async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        // wrapping_add avoids a debug-build panic on overflow. After
        // wraparound the receiver would reject the reused seq 0 as too old,
        // but that is unreachable in practice: at 1000 messages per second,
        // wrapping u64 takes roughly 5.8e8 years. The counter is 64 bit
        // specifically so this never matters.
        self.send_seq = self.send_seq.wrapping_add(1);
        let frame = seal(&self.key, self.session, self.send_seq, message)
            .map_err(|_| TransportError::Crypto)?;
        if frame.len() > MAX_FRAME {
            return Err(TransportError::FrameTooLarge);
        }
        let len = u32::try_from(frame.len()).map_err(|_| TransportError::FrameTooLarge)?;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> TransportReader<R> {
    /// Not cancel safe. `read_exact` discards any bytes it already consumed
    /// when its future is dropped, so cancelling this method mid-frame (for
    /// example by racing it in `tokio::select!` against a heartbeat timer)
    /// permanently desynchronizes the connection: the next call reads from
    /// the middle of a stale frame and every subsequent `recv` fails. A
    /// caller that needs to wait on `recv` alongside a timer must either
    /// run `recv` on its own task (so it is polled to completion regardless
    /// of what else is selected on) or this type must first grow a
    /// persistent read buffer across calls. Do not `select!` on this method
    /// directly.
    pub async fn recv(&mut self) -> Result<Message, TransportError> {
        let mut len_bytes = [0u8; 4];
        match self.stream.read_exact(&mut len_bytes).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Closed)
            }
            Err(e) => return Err(TransportError::Io(e)),
        }

        let len = validated_len(len_bytes)?;

        let mut frame = vec![0u8; len];
        match self.stream.read_exact(&mut frame).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Truncated)
            }
            Err(e) => return Err(TransportError::Io(e)),
        }

        let (seq, message) =
            open(&self.key, self.session, &frame).map_err(|_| TransportError::Crypto)?;
        if !self.replay.accept(seq) {
            return Err(TransportError::Replay);
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_proto::{Message, SessionId, SharedKey, Usage};
    use std::time::Duration;
    use tokio::io::duplex;
    use tokio::io::AsyncWriteExt;

    fn key() -> SharedKey {
        SharedKey::from_bytes([7u8; 32])
    }

    fn session() -> SessionId {
        SessionId([3u8; 32])
    }

    #[tokio::test]
    async fn sends_and_receives_a_message() {
        let (a, b) = duplex(4096);
        let (_ar, mut client) = split(a, key(), session());
        let (mut server, _bw) = split(b, key(), session());

        let sent = Message::Key {
            usage: Usage::C,
            pressed: true,
        };
        client.send(&sent).await.expect("send");
        assert_eq!(server.recv().await.expect("recv"), sent);
    }

    #[tokio::test]
    async fn preserves_order_across_many_messages() {
        let (a, b) = duplex(65536);
        let (_ar, mut client) = split(a, key(), session());
        let (mut server, _bw) = split(b, key(), session());

        for i in 0..50 {
            client
                .send(&Message::MouseMove { dx: i, dy: -i })
                .await
                .unwrap();
        }
        for i in 0..50 {
            assert_eq!(
                server.recv().await.unwrap(),
                Message::MouseMove { dx: i, dy: -i }
            );
        }
    }

    #[tokio::test]
    async fn rejects_a_peer_with_the_wrong_key() {
        let (a, b) = duplex(4096);
        let (_ar, mut client) = split(a, SharedKey::from_bytes([1u8; 32]), session());
        let (mut server, _bw) = split(b, SharedKey::from_bytes([2u8; 32]), session());

        client.send(&Message::Heartbeat).await.unwrap();
        assert!(matches!(server.recv().await, Err(TransportError::Crypto)));
    }

    #[tokio::test]
    async fn a_frame_sealed_under_one_session_is_refused_on_another() {
        // This is the attack CRITICAL 1 fixes: an attacker who records a
        // typing session and later becomes the client's server (rogue mDNS
        // response, ARP spoofing) can no longer replay the recording into a
        // fresh session, because a fresh transport for a different session
        // rejects frames sealed under the earlier one even though the
        // shared key and every AEAD tag would otherwise be valid.
        let (a, b) = duplex(4096);
        let (_ar, mut client) = split(a, key(), SessionId([1u8; 32]));
        let (mut server, _bw) = split(b, key(), SessionId([2u8; 32]));

        client.send(&Message::Heartbeat).await.unwrap();
        assert!(matches!(server.recv().await, Err(TransportError::Crypto)));
    }

    #[tokio::test]
    async fn reports_closure_when_the_peer_goes_away() {
        let (a, b) = duplex(4096);
        let (ar, aw) = split(a, key(), session());
        let (mut server, _bw) = split(b, key(), session());

        // BOTH halves must go. Dropping only the writer leaves the reader
        // holding its share of the stream, so no EOF is ever delivered and
        // the peer waits forever.
        drop(aw);
        drop(ar);

        // Bounded so a regression fails fast instead of hanging CI with no
        // message, which is what this test did when it dropped one half.
        let outcome = tokio::time::timeout(Duration::from_secs(2), server.recv())
            .await
            .expect("recv should report closure promptly, not block");
        assert!(matches!(outcome, Err(TransportError::Closed)));
    }

    #[tokio::test]
    async fn rejects_a_replayed_frame() {
        // Capturing a frame and sending it twice must not deliver it twice,
        // or an attacker could re-inject a captured keystroke.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session());

        // Build one frame by hand so it can be sent twice verbatim.
        let frame = hop_proto::seal(&key(), session(), 1, &Message::Heartbeat).unwrap();
        let len = u32::try_from(frame.len()).unwrap();
        for _ in 0..2 {
            a.write_all(&len.to_be_bytes()).await.unwrap();
            a.write_all(&frame).await.unwrap();
        }
        a.flush().await.unwrap();

        assert_eq!(receiver.recv().await.unwrap(), Message::Heartbeat);
        assert!(matches!(receiver.recv().await, Err(TransportError::Replay)));
    }

    #[tokio::test]
    async fn refuses_an_oversized_declared_length() {
        // A peer claiming a huge frame must be refused before we allocate.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session());
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        assert!(matches!(
            receiver.recv().await,
            Err(TransportError::FrameTooLarge)
        ));
    }

    #[tokio::test]
    async fn a_forged_frame_cannot_poison_the_replay_window() {
        // The sequence number must only reach the replay window after the
        // frame authenticates. Otherwise one forged frame claiming a huge
        // seq pins the window and permanently rejects genuine traffic.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session());

        let mut forged = Vec::new();
        forged.extend_from_slice(&u64::MAX.to_be_bytes());
        forged.extend_from_slice(&[0u8; 64]);
        let len = u32::try_from(forged.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&forged).await.unwrap();
        a.flush().await.unwrap();
        assert!(matches!(receiver.recv().await, Err(TransportError::Crypto)));

        // A genuine frame must still be accepted afterwards.
        let real = hop_proto::seal(&key(), session(), 1, &Message::Heartbeat).unwrap();
        let len = u32::try_from(real.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&real).await.unwrap();
        a.flush().await.unwrap();
        assert_eq!(receiver.recv().await.unwrap(), Message::Heartbeat);
    }

    #[tokio::test]
    async fn truncated_body_is_reported_distinctly_from_a_clean_close() {
        // A peer that vanishes mid-frame is a fault worth logging or rate
        // limiting, not a graceful shutdown, so it must not be conflated
        // with `Closed`.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session());

        let frame = hop_proto::seal(&key(), session(), 1, &Message::Heartbeat).unwrap();
        let len = u32::try_from(frame.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&frame[..frame.len() - 1]).await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        assert!(matches!(
            receiver.recv().await,
            Err(TransportError::Truncated)
        ));
    }

    #[tokio::test]
    async fn closing_between_frames_is_still_reported_as_closed() {
        // The two failure modes must stay distinguishable: nothing at all
        // arriving (a clean close between frames) is different from a
        // partial frame arriving (a truncation).
        let (a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session());
        drop(a);
        assert!(matches!(receiver.recv().await, Err(TransportError::Closed)));
    }

    #[tokio::test]
    async fn reader_and_writer_work_concurrently() {
        // The supervisor must be able to wait on incoming frames while a
        // heartbeat timer fires on the same connection. That is impossible
        // with a single &mut self type, which is why this split exists.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO);
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO);

        let reader = tokio::spawn(async move {
            let first = ar.recv().await.expect("recv");
            let second = ar.recv().await.expect("recv");
            (first, second)
        });

        bw.send(&Message::Heartbeat).await.expect("send");
        bw.send(&Message::Release).await.expect("send");

        let (first, second) = reader.await.expect("join");
        assert_eq!(first, Message::Heartbeat);
        assert_eq!(second, Message::Release);

        // And the other direction on the same pair still works.
        aw.send(&Message::Heartbeat).await.expect("send");
        assert_eq!(br.recv().await.expect("recv"), Message::Heartbeat);
    }

    #[tokio::test]
    async fn each_direction_has_its_own_sequence_space() {
        // Both sides start at seq 1. If they shared a replay window, the
        // second direction's first frame would look like a replay.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO);
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO);

        aw.send(&Message::Heartbeat).await.unwrap();
        bw.send(&Message::Heartbeat).await.unwrap();
        assert_eq!(br.recv().await.unwrap(), Message::Heartbeat);
        assert_eq!(ar.recv().await.unwrap(), Message::Heartbeat);
    }

    #[tokio::test]
    async fn every_message_variant_seals_under_the_frame_cap() {
        // Pins that no existing variant is close to MAX_FRAME, so a future
        // protocol addition cannot silently make send() start failing.
        let cases = vec![
            Message::Handshake {
                version: 1,
                capabilities: 0,
                peer_id: "a-fairly-realistic-machine-identifier-1234".into(),
                nonce: [0u8; 32],
            },
            Message::MouseMove { dx: -3, dy: 7 },
            Message::MouseButton {
                button: hop_proto::Button::Left,
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
        for message in cases {
            let frame = hop_proto::seal(&key(), session(), 1, &message).unwrap();
            assert!(
                frame.len() < MAX_FRAME,
                "{message:?} sealed to {} bytes, which is not under MAX_FRAME",
                frame.len()
            );
        }
    }
}
