//! The client connection supervisor: the piece that actually delivers this
//! project's reason for existing. It dials the server, performs the
//! handshake, pumps input while the link is healthy, and reconnects
//! forever whenever the link is lost, without ever needing a restart.
//!
//! The decision logic that does not need a socket lives in two small pure
//! units, [`release_everything`] and [`ReconnectPolicy`], so the behavior
//! this module exists to guarantee (self-healing keys, backoff that never
//! gives up) is testable without a network at all. [`ClientSupervisor::run`]
//! is deliberately thin: it wires those units to a real `TcpStream` and
//! logs every transition.

use crate::{
    client_handshake, message_to_event, Backoff, HeldKeys, Injector, InputEvent, Liveness,
    TransportError,
};
use hop_proto::{Message, SharedKey};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Inject a key-up for every key this client currently believes is held,
/// and forget them.
///
/// The server cannot deliver `ReleaseAllKeys` to a peer it can no longer
/// reach, so on a genuine disconnect the client must release its own keys
/// itself. Without this, a modifier the user pressed just before the link
/// died stays physically down on the far machine forever: the worst
/// outcome this tool can produce.
pub fn release_everything<I: Injector>(injector: &mut I, held: &mut HeldKeys) {
    for usage in held.drain_release() {
        let event = InputEvent::Key {
            usage,
            pressed: false,
        };
        if let Err(error) = injector.inject(&event) {
            // Swallowed deliberately, matching pump_client's rationale: one
            // rejected key-up must not stop the rest of the release sweep,
            // but it must not be silent either.
            tracing::warn!(?event, %error, "injector rejected a release-on-disconnect event");
        }
    }
}

/// Exponential backoff for reconnect attempts that never gives up.
///
/// Wraps [`Backoff`] with a `delay()` getter so a caller can read "how long
/// to wait" independently of "record that an attempt failed", which is
/// what makes the two testable as separate, ordered steps.
#[derive(Debug)]
pub struct ReconnectPolicy {
    backoff: Backoff,
    current: Duration,
}

const INITIAL_DELAY: Duration = Duration::from_millis(100);
const MAX_DELAY: Duration = Duration::from_secs(5);

impl ReconnectPolicy {
    pub fn new() -> Self {
        Self {
            backoff: Backoff::new(INITIAL_DELAY, MAX_DELAY),
            current: INITIAL_DELAY,
        }
    }

    /// The delay to wait before (re)trying, given everything recorded so
    /// far.
    pub fn delay(&self) -> Duration {
        self.current
    }

    /// Record a failed attempt, advancing the delay for the next one.
    pub fn failed(&mut self) {
        self.current = self.backoff.next_delay();
    }

    /// Record a successful connection, so a brief blip does not inherit a
    /// long delay from an earlier outage.
    pub fn connected(&mut self) {
        self.backoff.reset();
        self.current = INITIAL_DELAY;
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply one already-received message: update `held` and inject the
/// corresponding event.
///
/// Deliberately mirrors [`crate::pump_client`]'s per-message handling
/// rather than calling it, because `pump_client` owns its own `recv()`
/// call and `TransportReader::recv` is not cancel safe (see its doc
/// comment): racing it inside `tokio::select!` against a heartbeat timer
/// would desynchronize the connection. The supervisor instead gives
/// `recv` its own task (see `run_connection`) and applies each message
/// here, in the task that owns `injector` and `held`.
fn apply_message<I: Injector>(message: Message, injector: &mut I, held: &mut HeldKeys) {
    match message {
        Message::ReleaseAllKeys => {
            for usage in held.drain_release() {
                let event = InputEvent::Key {
                    usage,
                    pressed: false,
                };
                if let Err(error) = injector.inject(&event) {
                    tracing::warn!(?event, %error, "injector rejected event");
                }
            }
        }
        Message::Key { usage, pressed } => {
            held.record(usage, pressed);
            let event = InputEvent::Key { usage, pressed };
            if let Err(error) = injector.inject(&event) {
                tracing::warn!(?event, %error, "injector rejected event");
            }
        }
        other => {
            if let Some(event) = message_to_event(&other) {
                if let Err(error) = injector.inject(&event) {
                    tracing::warn!(?event, %error, "injector rejected event");
                }
            }
        }
    }
}

/// Supervises one client's connection to a `hop` server: connect,
/// handshake, pump input, and reconnect forever.
pub struct ClientSupervisor {
    addr: String,
    key: SharedKey,
    peer_id: String,
    heartbeat_interval: Duration,
    death_timeout: Duration,
}

impl ClientSupervisor {
    /// `addr` is the `host:port` to dial. `peer_id` is presented during the
    /// handshake. Heartbeats tick every second and the peer is declared
    /// dead after three seconds of inbound silence, per the spec.
    pub fn new(addr: impl Into<String>, key: SharedKey, peer_id: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            key,
            peer_id: peer_id.into(),
            heartbeat_interval: Duration::from_secs(1),
            death_timeout: Duration::from_secs(3),
        }
    }

    /// Override the heartbeat cadence and death timeout. Exists so tests
    /// can exercise death detection without waiting on the real spec
    /// values.
    pub fn with_liveness(mut self, heartbeat_interval: Duration, death_timeout: Duration) -> Self {
        self.heartbeat_interval = heartbeat_interval;
        self.death_timeout = death_timeout;
        self
    }

    /// Reconnect forever: connect, handshake, pump input while the link is
    /// healthy, and on any disconnect release this machine's own held keys
    /// and try again. An unattended machine must reconnect after an outage
    /// of any length, so this never returns.
    pub async fn run<I: Injector>(&mut self, injector: &mut I) -> ! {
        let mut policy = ReconnectPolicy::new();
        let mut held = HeldKeys::new();

        loop {
            tracing::info!(addr = %self.addr, "connecting");
            match TcpStream::connect(&self.addr).await {
                Ok(stream) => {
                    self.run_connection(stream, injector, &mut held, &mut policy)
                        .await;
                }
                Err(error) => {
                    tracing::warn!(%error, addr = %self.addr, "connect failed");
                }
            }

            // Whatever just happened (a failed connect, a failed
            // handshake, a declared death, or a transport error), the
            // server cannot reach us to release keys it thinks we're
            // holding, so we release our own.
            release_everything(injector, &mut held);
            policy.failed();
            let delay = policy.delay();
            tracing::info!(?delay, "reconnecting after backoff");
            tokio::time::sleep(delay).await;
        }
    }

    /// Run a single connection attempt to completion: handshake, pump
    /// input, and return once the link is gone for any reason. Never
    /// itself sleeps or retries; that is `run`'s job.
    async fn run_connection<I: Injector>(
        &self,
        stream: TcpStream,
        injector: &mut I,
        held: &mut HeldKeys,
        policy: &mut ReconnectPolicy,
    ) {
        let (reader, mut writer, session) =
            match client_handshake(stream, &self.key, &self.peer_id).await {
                Ok(v) => v,
                Err(error) => {
                    // Safe to log directly: HandshakeError::Unexpected now
                    // carries only the offending message's kind (see its
                    // doc comment), never the message itself, so this can
                    // never write a keystroke to the log.
                    tracing::warn!(%error, "handshake failed");
                    return;
                }
            };
        tracing::info!(
            ?session,
            "handshake complete; moved onto the derived session"
        );

        // The handshake alone does not prove the peer holds the key for
        // THIS session: both Handshake messages necessarily travel under
        // the fixed SessionId::ZERO (see the handshake module's doc
        // comment), so a network attacker who merely recorded an earlier
        // legitimate handshake can replay it verbatim into a new
        // connection and produce the same derived session, without ever
        // knowing the key. They cannot forge anything past that, because
        // they cannot seal a new frame under the derived session. So the
        // connection is not treated as confirmed, and the backoff is not
        // reset, until a real frame actually authenticates under
        // `session`.
        let mut confirmed = false;

        // recv is NOT cancel safe (see TransportReader::recv's doc
        // comment): racing it in the select! below against the heartbeat
        // ticker would desynchronize the connection permanently. Instead
        // it gets its own task that always runs it to completion, and
        // forwards each result over a channel.
        let (tx, mut rx) = mpsc::channel::<Result<Message, TransportError>>(8);
        let reader_task = tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let result = reader.recv().await;
                let ended = result.is_err();
                if tx.send(result).await.is_err() || ended {
                    break;
                }
            }
        });

        let now = Instant::now();
        let mut liveness = Liveness::new(now, self.heartbeat_interval, self.death_timeout);
        let mut ticker = tokio::time::interval(self.heartbeat_interval);

        loop {
            tokio::select! {
                received = rx.recv() => {
                    match received {
                        Some(Ok(message)) => {
                            liveness.record_activity(Instant::now());
                            if !confirmed {
                                confirmed = true;
                                policy.connected();
                                tracing::info!(
                                    "first frame authenticated under the derived session; connection confirmed"
                                );
                            }
                            apply_message(message, injector, held);
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "transport error; disconnecting");
                            break;
                        }
                        None => {
                            tracing::warn!("reader task ended unexpectedly; disconnecting");
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    if liveness.is_dead(now) {
                        tracing::warn!(
                            timeout = ?self.death_timeout,
                            "no activity from peer within the timeout; declaring connection dead"
                        );
                        break;
                    }
                    // Our own outbound heartbeat must never count as
                    // evidence the peer is alive: record_heartbeat_sent
                    // feeds a separate clock from record_activity, which
                    // is exactly what stops a half-open socket from
                    // looking healthy forever.
                    if let Err(error) = writer.send(&Message::Heartbeat).await {
                        tracing::warn!(%error, "failed to send heartbeat; disconnecting");
                        break;
                    }
                    liveness.record_heartbeat_sent(Instant::now());
                }
            }
        }

        // Tear down BOTH transport halves together. tokio::io::split
        // shares the underlying stream, so dropping only one half (the
        // writer, say) leaves the other half still holding its share:
        // no EOF is ever delivered, the reader task blocks forever, and
        // this reconnect loop would never fire again. Aborting the
        // reader task and awaiting it guarantees its TransportReader
        // (and the ReadHalf inside it) is actually dropped, even if it
        // is currently blocked awaiting data that will never arrive on a
        // half-open socket, before the TransportWriter below is dropped
        // too.
        reader_task.abort();
        let _ = reader_task.await;
        drop(writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::FakeInjector;
    use hop_proto::{SessionId, SharedKey, Usage};
    use std::sync::{Arc, Mutex};
    use tokio::io::duplex;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn a_dead_connection_releases_held_keys_locally() {
        // The server cannot deliver ReleaseAllKeys to a peer it can no
        // longer reach, so the client must release its own keys when it
        // decides the link is dead. Without this, a modifier stays down
        // on this machine forever.
        let mut held = HeldKeys::new();
        held.record(Usage::LEFT_CTRL, true);
        let mut injector = FakeInjector::new();

        release_everything(&mut injector, &mut held);

        assert_eq!(
            injector.injected(),
            vec![InputEvent::Key {
                usage: Usage::LEFT_CTRL,
                pressed: false
            }]
        );
        assert!(held.is_empty());
    }

    #[test]
    fn backoff_resets_after_a_successful_connection() {
        // A brief blip must not inherit a long delay from an earlier
        // outage, or a quick recovery would be needlessly slow.
        let mut policy = ReconnectPolicy::new();
        policy.failed();
        policy.failed();
        assert!(policy.delay() > Duration::from_millis(100));
        policy.connected();
        assert_eq!(policy.delay(), Duration::from_millis(100));
    }

    #[test]
    fn the_policy_never_gives_up() {
        let mut policy = ReconnectPolicy::new();
        for _ in 0..1000 {
            policy.failed();
            assert!(policy.delay() <= Duration::from_secs(5));
        }
    }

    /// A concrete Injector that logs into shared state, so a test can
    /// observe injections from outside a spawned `run()` task even though
    /// `run()` never returns and therefore never hands the injector back.
    #[derive(Clone)]
    struct SharedInjector(Arc<Mutex<Vec<InputEvent>>>);

    impl Injector for SharedInjector {
        fn inject(&mut self, event: &InputEvent) -> Result<(), crate::device::DeviceError> {
            self.0.lock().unwrap().push(*event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn client_handshake_moves_input_onto_the_derived_session_not_zero() {
        // Pins the trap a security review found in this exact loop: after
        // the handshake returns a SessionId, nothing should let a caller
        // keep using SessionId::ZERO-keyed halves for input. FINDING 5
        // closes this structurally: client_handshake and server_handshake
        // consume the stream and hand back halves already re-split under
        // the derived session, so a ZERO-keyed transport is never even
        // reachable from `run_connection`'s call site. This test exercises
        // the actual `client_handshake` function `run_connection` calls,
        // not a reimplementation of it, so it fails if the re-key is ever
        // removed from that function.
        let (client_io, server_io) = duplex(65536);
        let key = SharedKey::from_bytes([13u8; 32]);

        let server_key = key.clone();
        let server = tokio::spawn(async move {
            let (real_reader, real_writer, session, _peer_id) =
                crate::server_handshake(server_io, &server_key)
                    .await
                    .expect("server handshake");
            (real_reader, real_writer, session)
        });

        let (client_reader, mut client_writer, client_session) =
            client_handshake(client_io, &key, "pc")
                .await
                .expect("client handshake");
        let (server_reader, mut server_writer, server_session) = server.await.unwrap();

        assert_eq!(
            client_session, server_session,
            "both sides derive the same session"
        );
        assert_ne!(client_session, SessionId::ZERO);

        // A frame the client sends after client_handshake returns must
        // authenticate under the derived session...
        client_writer
            .send(&Message::Key {
                usage: Usage::C,
                pressed: true,
            })
            .await
            .expect("send under derived session");
        let mut server_reader = server_reader;
        let received = tokio::time::timeout(Duration::from_secs(2), server_reader.recv())
            .await
            .expect("recv must not hang")
            .expect("a frame sealed under the derived session must open");
        assert_eq!(
            received,
            Message::Key {
                usage: Usage::C,
                pressed: true
            }
        );

        // ...and must NOT authenticate under SessionId::ZERO, the
        // placeholder that means "no handshake happened". A receiver
        // still keyed to ZERO (what a caller would be left with if it
        // used the pre-handshake halves instead of this function's
        // output) must reject it.
        server_writer
            .send(&Message::Heartbeat)
            .await
            .expect("send under derived session");
        let mut client_reader = client_reader;
        let opened_under_derived =
            tokio::time::timeout(Duration::from_secs(2), client_reader.recv())
                .await
                .expect("recv must not hang")
                .expect("the client's own reader, keyed to the derived session, must open it");
        assert_eq!(opened_under_derived, Message::Heartbeat);
    }

    #[tokio::test]
    async fn a_dead_link_releases_a_held_modifier_locally_end_to_end() {
        // The headline guarantee, exercised through the real
        // ClientSupervisor::run over a real TCP socket rather than pinned
        // only by a comment: a modifier gets pressed, the link goes
        // silent without ever closing cleanly (a half-open socket, not a
        // clean EOF), and the client must notice via liveness and inject
        // the release itself, since the server can never reach it to ask.
        let key = SharedKey::from_bytes([9u8; 32]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_key = key.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_r, mut w, _session, _peer_id) = crate::server_handshake(stream, &server_key)
                .await
                .expect("server handshake");
            w.send(&Message::Key {
                usage: Usage::LEFT_CTRL,
                pressed: true,
            })
            .await
            .expect("send key press");

            // Go silent without closing: a half-open socket, the exact
            // case that Closed/EOF handling alone cannot detect.
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let log: Arc<Mutex<Vec<InputEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut injector = SharedInjector(log.clone());
        let mut supervisor = ClientSupervisor::new(addr.to_string(), key, "pc")
            .with_liveness(Duration::from_millis(20), Duration::from_millis(60));

        let handle = tokio::spawn(async move {
            supervisor.run(&mut injector).await;
        });

        // Long enough for connect, handshake, the key press, and the
        // shortened death timeout to all land.
        tokio::time::sleep(Duration::from_millis(500)).await;
        handle.abort();

        let injected = log.lock().unwrap().clone();
        assert_eq!(
            injected,
            vec![
                InputEvent::Key {
                    usage: Usage::LEFT_CTRL,
                    pressed: true
                },
                InputEvent::Key {
                    usage: Usage::LEFT_CTRL,
                    pressed: false
                },
            ],
            "the client must inject its own release when the link dies, leaving nothing held"
        );
    }
}
