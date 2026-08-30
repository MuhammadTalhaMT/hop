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
    #[error("peer closed the connection")]
    Closed,
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

    pub async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
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

    pub async fn recv(&mut self) -> Result<Message, TransportError> {
        let mut len_bytes = [0u8; 4];
        match self.stream.read_exact(&mut len_bytes).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Closed)
            }
            Err(e) => return Err(TransportError::Io(e)),
        }

        let len = u32::from_be_bytes(len_bytes) as usize;
        if len > MAX_FRAME {
            return Err(TransportError::FrameTooLarge);
        }

        let mut frame = vec![0u8; len];
        match self.stream.read_exact(&mut frame).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Closed)
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
}
