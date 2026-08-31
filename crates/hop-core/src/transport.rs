use hop_proto::{open, seal, Direction, Message, ReplayWindow, SessionId, SharedKey};
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
/// direction, and requires every inbound frame to have been sealed under
/// the OPPOSITE direction from this side's writer (see [`Direction`] and
/// [`split`]).
///
/// Generic over the stream so tests can drive it through an in-memory
/// duplex pipe rather than a real socket.
pub struct TransportReader<R> {
    stream: R,
    key: SharedKey,
    session: SessionId,
    direction: Direction,
    replay: ReplayWindow,
}

/// The sending half of a split transport. Owns the outbound sequence
/// counter for its direction, and seals every frame under this side's own
/// direction (see [`Direction`] and [`split`]).
///
/// Generic over the stream so tests can drive it through an in-memory
/// duplex pipe rather than a real socket.
pub struct TransportWriter<W> {
    stream: W,
    key: SharedKey,
    session: SessionId,
    direction: Direction,
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
///
/// `direction` is the direction THIS side's writer sends in: pass
/// `Direction::ClientToServer` when splitting on the client, and
/// `Direction::ServerToClient` when splitting on the server. The returned
/// `TransportWriter` seals under `direction`; the returned
/// `TransportReader` opens expecting `direction.opposite()`. That is what
/// stops a reflected frame (this side's own outbound frame, echoed back to
/// it, whether by an attacker or a plain network loop) from authenticating
/// as inbound: it was sealed under `direction`, but this side's reader
/// requires the opposite.
///
/// Dropping only one half does NOT close the underlying connection: the
/// other half still holds its share of the stream, so no EOF is ever
/// delivered and a `recv` on the surviving half blocks forever. Tearing a
/// connection down means dropping BOTH the `TransportReader` and the
/// `TransportWriter` it was split from. A supervisor that drops only its
/// writer when it decides a peer is dead will leave its reader task
/// hanging instead of exiting, and the reconnect loop will never fire.
pub fn split<S: AsyncRead + AsyncWrite>(
    stream: S,
    key: SharedKey,
    session: SessionId,
    direction: Direction,
) -> (TransportReader<ReadHalf<S>>, TransportWriter<WriteHalf<S>>) {
    let (r, w) = io_split(stream);
    (
        TransportReader {
            stream: r,
            key: key.clone(),
            session,
            direction: direction.opposite(),
            replay: ReplayWindow::new(),
        },
        TransportWriter {
            stream: w,
            key,
            session,
            direction,
            send_seq: 0,
        },
    )
}

impl<R> TransportReader<R> {
    /// Reclaim the underlying stream half, discarding this reader's key,
    /// session, and replay window.
    ///
    /// Exists so a caller can run the handshake (see
    /// `crate::handshake`) over a transport split with `SessionId::ZERO`,
    /// then reassemble the raw stream (for example with
    /// `tokio::io::ReadHalf::unsplit`) and call `split` again with the
    /// session the handshake derived, before any input message flows.
    pub fn into_inner(self) -> R {
        self.stream
    }
}

impl<W> TransportWriter<W> {
    /// Reclaim the underlying stream half. See
    /// [`TransportReader::into_inner`].
    pub fn into_inner(self) -> W {
        self.stream
    }
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
        let frame = seal(
            &self.key,
            self.session,
            self.direction,
            self.send_seq,
            message,
        )
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

        let (seq, message) = open(&self.key, self.session, self.direction, &frame)
            .map_err(|_| TransportError::Crypto)?;
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn key() -> SharedKey {
        SharedKey::from_bytes([7u8; 32])
    }

    fn session() -> SessionId {
        SessionId([3u8; 32])
    }

    /// A regression that makes `recv` block forever (for example, a future
    /// change to `split`'s close semantics) must fail the test suite fast
    /// rather than hang CI indefinitely. Every `recv` call in this module
    /// goes through this helper for that reason.
    async fn recv_or_timeout<R: AsyncRead + Unpin>(
        reader: &mut TransportReader<R>,
    ) -> Result<Message, TransportError> {
        tokio::time::timeout(Duration::from_secs(2), reader.recv())
            .await
            .expect("recv must not hang")
    }

    /// The direction passed to `split` on the client side of a pair, used
    /// throughout these tests so the client/server roles stay explicit at
    /// every call site.
    fn client_dir() -> Direction {
        Direction::ClientToServer
    }

    /// The direction passed to `split` on the server side of a pair.
    fn server_dir() -> Direction {
        Direction::ServerToClient
    }

    #[tokio::test]
    async fn sends_and_receives_a_message() {
        let (a, b) = duplex(4096);
        let (_ar, mut client) = split(a, key(), session(), client_dir());
        let (mut server, _bw) = split(b, key(), session(), server_dir());

        let sent = Message::Key {
            usage: Usage::C,
            pressed: true,
        };
        client.send(&sent).await.expect("send");
        assert_eq!(recv_or_timeout(&mut server).await.expect("recv"), sent);
    }

    #[tokio::test]
    async fn preserves_order_across_many_messages() {
        let (a, b) = duplex(65536);
        let (_ar, mut client) = split(a, key(), session(), client_dir());
        let (mut server, _bw) = split(b, key(), session(), server_dir());

        for i in 0..50 {
            client
                .send(&Message::MouseMove { dx: i, dy: -i })
                .await
                .unwrap();
        }
        for i in 0..50 {
            assert_eq!(
                recv_or_timeout(&mut server).await.unwrap(),
                Message::MouseMove { dx: i, dy: -i }
            );
        }
    }

    #[tokio::test]
    async fn rejects_a_peer_with_the_wrong_key() {
        let (a, b) = duplex(4096);
        let (_ar, mut client) = split(a, SharedKey::from_bytes([1u8; 32]), session(), client_dir());
        let (mut server, _bw) = split(b, SharedKey::from_bytes([2u8; 32]), session(), server_dir());

        client.send(&Message::Heartbeat).await.unwrap();
        assert!(matches!(
            recv_or_timeout(&mut server).await,
            Err(TransportError::Crypto)
        ));
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
        let (_ar, mut client) = split(a, key(), SessionId([1u8; 32]), client_dir());
        let (mut server, _bw) = split(b, key(), SessionId([2u8; 32]), server_dir());

        client.send(&Message::Heartbeat).await.unwrap();
        assert!(matches!(
            recv_or_timeout(&mut server).await,
            Err(TransportError::Crypto)
        ));
    }

    #[tokio::test]
    async fn a_reflected_frame_is_rejected() {
        // This is FINDING 2: the session and the replay window bind a
        // connection, but not which side sent a frame. Before binding the
        // direction, a frame this side sealed and sent out could be
        // echoed straight back to this side's own reader (a network loop,
        // or an attacker who just reflects what it sees) and would
        // authenticate as genuine inbound traffic, since it was new to
        // that reader's replay window. Binding the direction closes this:
        // the reflected frame was sealed under this side's OWN direction,
        // but this side's reader requires the opposite.
        let (a, mut peer) = duplex(4096);
        let (mut reader, mut writer) = split(a, key(), session(), client_dir());

        writer.send(&Message::Heartbeat).await.expect("send");

        // Read the exact bytes `writer` just put on the wire, then feed
        // them straight back into this side's own `reader` (`peer` is the
        // far end of the same duplex `a` was split from, so writing into
        // it delivers to `reader`), exactly as an echo would.
        let mut len_bytes = [0u8; 4];
        peer.read_exact(&mut len_bytes).await.expect("read len");
        let len = u32::from_be_bytes(len_bytes) as usize;
        let mut frame = vec![0u8; len];
        peer.read_exact(&mut frame).await.expect("read frame");

        peer.write_all(&len_bytes).await.expect("write len");
        peer.write_all(&frame).await.expect("write frame");
        peer.flush().await.expect("flush");

        let result = recv_or_timeout(&mut reader).await;
        assert!(
            matches!(result, Err(TransportError::Crypto)),
            "a reflected frame must fail authentication, got {result:?}"
        );
    }

    #[tokio::test]
    async fn reports_closure_when_the_peer_goes_away() {
        // Dropping only the writer half leaves the reader half still
        // holding its share of the duplex, so the stream never sees EOF.
        // Tearing down a connection means dropping BOTH halves, which is
        // exactly the semantic `split`'s doc comment now calls out.
        let (a, b) = duplex(4096);
        let (a_reader, a_writer) = split(a, key(), session(), client_dir());
        let (mut server, _bw) = split(b, key(), session(), server_dir());
        drop(a_reader);
        drop(a_writer);
        assert!(matches!(
            recv_or_timeout(&mut server).await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn rejects_a_replayed_frame() {
        // Capturing a frame and sending it twice must not deliver it twice,
        // or an attacker could re-inject a captured keystroke.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session(), server_dir());

        // Build one frame by hand so it can be sent twice verbatim. The
        // receiver was split as the server side, so it expects inbound
        // frames sealed as ClientToServer.
        let frame = hop_proto::seal(
            &key(),
            session(),
            Direction::ClientToServer,
            1,
            &Message::Heartbeat,
        )
        .unwrap();
        let len = u32::try_from(frame.len()).unwrap();
        for _ in 0..2 {
            a.write_all(&len.to_be_bytes()).await.unwrap();
            a.write_all(&frame).await.unwrap();
        }
        a.flush().await.unwrap();

        assert_eq!(
            recv_or_timeout(&mut receiver).await.unwrap(),
            Message::Heartbeat
        );
        assert!(matches!(
            recv_or_timeout(&mut receiver).await,
            Err(TransportError::Replay)
        ));
    }

    #[tokio::test]
    async fn refuses_an_oversized_declared_length() {
        // A peer claiming a huge frame must be refused before we allocate.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session(), server_dir());
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        assert!(matches!(
            recv_or_timeout(&mut receiver).await,
            Err(TransportError::FrameTooLarge)
        ));
    }

    #[tokio::test]
    async fn a_forged_frame_cannot_poison_the_replay_window() {
        // The sequence number must only reach the replay window after the
        // frame authenticates. Otherwise one forged frame claiming a huge
        // seq pins the window and permanently rejects genuine traffic.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session(), server_dir());

        let mut forged = Vec::new();
        forged.extend_from_slice(&u64::MAX.to_be_bytes());
        forged.extend_from_slice(&[0u8; 64]);
        let len = u32::try_from(forged.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&forged).await.unwrap();
        a.flush().await.unwrap();
        assert!(matches!(
            recv_or_timeout(&mut receiver).await,
            Err(TransportError::Crypto)
        ));

        // A genuine frame must still be accepted afterwards.
        let real = hop_proto::seal(
            &key(),
            session(),
            Direction::ClientToServer,
            1,
            &Message::Heartbeat,
        )
        .unwrap();
        let len = u32::try_from(real.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&real).await.unwrap();
        a.flush().await.unwrap();
        assert_eq!(
            recv_or_timeout(&mut receiver).await.unwrap(),
            Message::Heartbeat
        );
    }

    #[tokio::test]
    async fn truncated_body_is_reported_distinctly_from_a_clean_close() {
        // A peer that vanishes mid-frame is a fault worth logging or rate
        // limiting, not a graceful shutdown, so it must not be conflated
        // with `Closed`.
        let (mut a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session(), server_dir());

        let frame = hop_proto::seal(
            &key(),
            session(),
            Direction::ClientToServer,
            1,
            &Message::Heartbeat,
        )
        .unwrap();
        let len = u32::try_from(frame.len()).unwrap();
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&frame[..frame.len() - 1]).await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        assert!(matches!(
            recv_or_timeout(&mut receiver).await,
            Err(TransportError::Truncated)
        ));
    }

    #[tokio::test]
    async fn closing_between_frames_is_still_reported_as_closed() {
        // The two failure modes must stay distinguishable: nothing at all
        // arriving (a clean close between frames) is different from a
        // partial frame arriving (a truncation).
        let (a, b) = duplex(4096);
        let (mut receiver, _bw) = split(b, key(), session(), server_dir());
        drop(a);
        assert!(matches!(
            recv_or_timeout(&mut receiver).await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn reader_and_writer_work_concurrently() {
        // The supervisor must be able to wait on incoming frames while a
        // heartbeat timer fires on the same connection. That is impossible
        // with a single &mut self type, which is why this split exists.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO, client_dir());
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO, server_dir());

        let reader = tokio::spawn(async move {
            let first = recv_or_timeout(&mut ar).await.expect("recv");
            let second = recv_or_timeout(&mut ar).await.expect("recv");
            (first, second)
        });

        bw.send(&Message::Heartbeat).await.expect("send");
        bw.send(&Message::Release).await.expect("send");

        let (first, second) = reader.await.expect("join");
        assert_eq!(first, Message::Heartbeat);
        assert_eq!(second, Message::Release);

        // And the other direction on the same pair still works.
        aw.send(&Message::Heartbeat).await.expect("send");
        assert_eq!(
            recv_or_timeout(&mut br).await.expect("recv"),
            Message::Heartbeat
        );
    }

    #[tokio::test]
    async fn each_direction_has_its_own_sequence_space() {
        // Both sides start at seq 1. If they shared a replay window, the
        // second direction's first frame would look like a replay.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO, client_dir());
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO, server_dir());

        aw.send(&Message::Heartbeat).await.unwrap();
        bw.send(&Message::Heartbeat).await.unwrap();
        assert_eq!(recv_or_timeout(&mut br).await.unwrap(), Message::Heartbeat);
        assert_eq!(recv_or_timeout(&mut ar).await.unwrap(), Message::Heartbeat);
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
            let frame =
                hop_proto::seal(&key(), session(), Direction::ClientToServer, 1, &message).unwrap();
            assert!(
                frame.len() < MAX_FRAME,
                "{message:?} sealed to {} bytes, which is not under MAX_FRAME",
                frame.len()
            );
        }
    }
}
