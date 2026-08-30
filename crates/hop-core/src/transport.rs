use hop_proto::{open, seal, Message, ReplayWindow, SharedKey};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

/// A length-prefixed, encrypted message stream over any byte stream.
///
/// Generic over the stream so tests can drive it through an in-memory
/// duplex pipe rather than a real socket.
pub struct Transport<S> {
    stream: S,
    key: SharedKey,
    send_seq: u64,
    replay: ReplayWindow,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub fn new(stream: S, key: SharedKey) -> Self {
        Self {
            stream,
            key,
            send_seq: 0,
            replay: ReplayWindow::new(),
        }
    }

    /// Note for callers: after any `Err(TransportError::Io(_))` here, a
    /// partial frame may already be sitting on the wire (the length prefix
    /// or part of the frame body may have been written before the write
    /// failed). The stream is desynchronized at that point, so this
    /// `Transport` must be discarded and the connection re-established, not
    /// reused. Also note the asymmetry with `recv`: a peer that has gone
    /// away surfaces from `recv` as `Closed`, but surfaces from `send` as an
    /// `Io` error, since writing to a dead peer fails at the OS level rather
    /// than reading a clean EOF.
    pub async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        // wrapping_add avoids a debug-build panic on overflow. After
        // wraparound the receiver would reject the reused seq 0 as too old,
        // but that is unreachable in practice: at 1000 messages per second,
        // wrapping u64 takes roughly 5.8e8 years. The counter is 64 bit
        // specifically so this never matters.
        self.send_seq = self.send_seq.wrapping_add(1);
        let frame = seal(&self.key, self.send_seq, message).map_err(|_| TransportError::Crypto)?;
        if frame.len() > MAX_FRAME {
            return Err(TransportError::FrameTooLarge);
        }
        let len = u32::try_from(frame.len()).map_err(|_| TransportError::FrameTooLarge)?;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
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

        let len = Self::validated_len(len_bytes)?;

        let mut frame = vec![0u8; len];
        match self.stream.read_exact(&mut frame).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Truncated)
            }
            Err(e) => return Err(TransportError::Io(e)),
        }

        let (seq, message) = open(&self.key, &frame).map_err(|_| TransportError::Crypto)?;
        if !self.replay.accept(seq) {
            return Err(TransportError::Replay);
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_proto::{Message, SharedKey, Usage};
    use tokio::io::duplex;
    use tokio::io::AsyncWriteExt;

    fn key() -> SharedKey {
        SharedKey::from_bytes([7u8; 32])
    }

    #[tokio::test]
    async fn sends_and_receives_a_message() {
        let (a, b) = duplex(4096);
        let mut client = Transport::new(a, key());
        let mut server = Transport::new(b, key());

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
        let mut client = Transport::new(a, key());
        let mut server = Transport::new(b, key());

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
        let mut client = Transport::new(a, SharedKey::from_bytes([1u8; 32]));
        let mut server = Transport::new(b, SharedKey::from_bytes([2u8; 32]));

        client.send(&Message::Heartbeat).await.unwrap();
        assert!(matches!(server.recv().await, Err(TransportError::Crypto)));
    }

    #[tokio::test]
    async fn reports_closure_when_the_peer_goes_away() {
        let (a, b) = duplex(4096);
        let client = Transport::new(a, key());
        let mut server = Transport::new(b, key());
        drop(client);
        assert!(matches!(server.recv().await, Err(TransportError::Closed)));
    }

    #[tokio::test]
    async fn rejects_a_replayed_frame() {
        // Capturing a frame and sending it twice must not deliver it twice,
        // or an attacker could re-inject a captured keystroke.
        let (mut a, b) = duplex(4096);
        let mut receiver = Transport::new(b, key());

        // Build one frame by hand so it can be sent twice verbatim.
        let frame = hop_proto::seal(&key(), 1, &Message::Heartbeat).unwrap();
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
        let mut receiver = Transport::new(b, key());
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
        let mut receiver = Transport::new(b, key());

        let mut forged = Vec::new();
        forged.extend_from_slice(&u64::MAX.to_be_bytes());
        forged.extend_from_slice(&[0u8; 64]);
        let len = u32::try_from(forged.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&forged).await.unwrap();
        a.flush().await.unwrap();
        assert!(matches!(receiver.recv().await, Err(TransportError::Crypto)));

        // A genuine frame must still be accepted afterwards.
        let real = hop_proto::seal(&key(), 1, &Message::Heartbeat).unwrap();
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
        let mut receiver = Transport::new(b, key());

        let frame = hop_proto::seal(&key(), 1, &Message::Heartbeat).unwrap();
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
        let mut receiver = Transport::new(b, key());
        drop(a);
        assert!(matches!(receiver.recv().await, Err(TransportError::Closed)));
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
            let frame = hop_proto::seal(&key(), 1, &message).unwrap();
            assert!(
                frame.len() < MAX_FRAME,
                "{message:?} sealed to {} bytes, which is not under MAX_FRAME",
                frame.len()
            );
        }
    }
}
