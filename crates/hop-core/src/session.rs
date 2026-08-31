use crate::{
    Action, Capturer, Control, HeldKeys, Injector, InputEvent, RemapTable, TransportError,
    TransportReader, TransportWriter,
};
use hop_proto::Message;
use tokio::io::{AsyncRead, AsyncWrite};

/// Translate a locally captured event into the message that carries it,
/// applying remapping at the source. Returns None for events that are not
/// themselves forwarded, such as the edge crossing.
pub fn event_to_message(remap: &RemapTable, event: InputEvent) -> Option<Message> {
    match event {
        InputEvent::Mouse { dx, dy } => Some(Message::MouseMove { dx, dy }),
        InputEvent::Scroll { dx, dy } => Some(Message::Scroll { dx, dy }),
        InputEvent::Button { button, pressed } => Some(Message::MouseButton { button, pressed }),
        InputEvent::Key { usage, pressed } => Some(Message::Key {
            usage: remap.apply(usage),
            pressed,
        }),
        InputEvent::EdgeCrossed => None,
    }
}

/// Translate a received message into the event to inject. Returns None for
/// messages that are not input, including unknown ones from a newer peer.
pub fn message_to_event(message: &Message) -> Option<InputEvent> {
    match *message {
        Message::MouseMove { dx, dy } => Some(InputEvent::Mouse { dx, dy }),
        Message::Scroll { dx, dy } => Some(InputEvent::Scroll { dx, dy }),
        Message::MouseButton { button, pressed } => Some(InputEvent::Button { button, pressed }),
        Message::Key { usage, pressed } => Some(InputEvent::Key { usage, pressed }),
        _ => None,
    }
}

/// Drain the capturer, forwarding whatever the control state machine says
/// belongs to the peer.
pub async fn pump_server<W, C>(
    transport: &mut TransportWriter<W>,
    capturer: &mut C,
    control: &mut Control,
    remap: &RemapTable,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
    C: Capturer,
{
    while let Some(event) = capturer.poll() {
        match event {
            InputEvent::EdgeCrossed => {
                control.on_edge_crossed();
            }
            InputEvent::Key { usage, pressed } => {
                if let Action::Forward(usage, pressed) = control.on_key(usage, pressed) {
                    if let Some(message) =
                        event_to_message(remap, InputEvent::Key { usage, pressed })
                    {
                        transport.send(&message).await?;
                    }
                }
            }
            other => {
                if control.focus() == crate::Focus::Remote {
                    if let Some(message) = event_to_message(remap, other) {
                        transport.send(&message).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Receive one message and inject it if it is input.
///
/// `held` is the client's own record of what it has physically pressed on
/// this machine. It is authoritative in a way the server's held-key set is
/// not: the server records usages before remapping, purely as a "was
/// anything held" signal for deciding whether to send `ReleaseAllKeys` at
/// all, while this set records the post-remap usages actually injected
/// here. A `ReleaseAllKeys` message is answered from this set, never from
/// anything the peer claims, so the release always matches what is really
/// down on this keyboard.
pub async fn pump_client<R, I>(
    transport: &mut TransportReader<R>,
    injector: &mut I,
    held: &mut HeldKeys,
) -> Result<(), TransportError>
where
    R: AsyncRead + Unpin,
    I: Injector,
{
    let message = transport.recv().await?;
    match message {
        Message::ReleaseAllKeys => {
            // Deliberately does not go through message_to_event: this is
            // the stuck-modifier guarantee, so it must inject a key-up for
            // every key this client actually holds, not merely decode a
            // message into an event.
            for usage in held.drain_release() {
                let event = InputEvent::Key {
                    usage,
                    pressed: false,
                };
                if let Err(error) = injector.inject(&event) {
                    // Swallowed deliberately: aborting the receive loop
                    // over one rejected key-up would be worse than the
                    // stray key it leaves un-released, but it must not be
                    // silent, or a platform injector that starts rejecting
                    // everything looks identical to a healthy connection.
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
    Ok(())
}

/// Tell the peer to release every key it believes is held.
///
/// Call this whenever `Control` returns `Action::ReleaseAll` and the link is
/// still usable, for example on an explicit release or the panic hotkey. On a
/// genuine disconnect the peer is unreachable by definition, so the client
/// must also release its own keys when it detects a dead connection.
pub async fn send_release_all<W>(transport: &mut TransportWriter<W>) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    transport.send(&Message::ReleaseAllKeys).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_proto::{Button, Usage};

    /// `message_to_event` is public API that the platform layer calls, so it
    /// is tested directly rather than only through `pump_client`, which
    /// handles some variants itself and would leave the rest uncovered.
    #[test]
    fn every_input_variant_maps_to_its_own_event() {
        assert_eq!(
            message_to_event(&Message::MouseMove { dx: 3, dy: -4 }),
            Some(InputEvent::Mouse { dx: 3, dy: -4 })
        );
        assert_eq!(
            message_to_event(&Message::Scroll { dx: 1, dy: -2 }),
            Some(InputEvent::Scroll { dx: 1, dy: -2 })
        );
        assert_eq!(
            message_to_event(&Message::MouseButton {
                button: Button::Right,
                pressed: true
            }),
            Some(InputEvent::Button {
                button: Button::Right,
                pressed: true
            })
        );
        assert_eq!(
            message_to_event(&Message::Key {
                usage: Usage::C,
                pressed: false
            }),
            Some(InputEvent::Key {
                usage: Usage::C,
                pressed: false
            })
        );
    }

    #[test]
    fn control_messages_produce_no_event() {
        for message in [
            Message::Heartbeat,
            Message::Release,
            Message::ReleaseAllKeys,
            Message::Unknown,
        ] {
            assert_eq!(message_to_event(&message), None, "{message:?}");
        }
    }

    #[test]
    fn edge_crossed_is_local_only_and_never_sent() {
        // The edge crossing tells the local machine to hand over control.
        // It is not something the peer needs, so it must not become a
        // message.
        let remap = RemapTable::new();
        assert!(event_to_message(&remap, InputEvent::EdgeCrossed).is_none());
    }

    #[tokio::test]
    async fn pump_client_keeps_processing_after_injection_is_rejected() {
        // A Windows injector that starts rejecting events must not stall
        // the client: the socket must stay up, and the next message must
        // still be received and acted on. Silently wedging here, with
        // heartbeats still flowing and liveness still reporting healthy,
        // is exactly the failure mode this project exists to prevent.
        use crate::transport::split;
        use crate::FailingInjector;
        use hop_proto::{Direction, SessionId, SharedKey};
        use tokio::io::duplex;

        let (a, b) = duplex(4096);
        let (_ar, mut server) = split(
            a,
            SharedKey::from_bytes([1u8; 32]),
            SessionId::ZERO,
            Direction::ServerToClient,
        );
        let (mut client, _bw) = split(
            b,
            SharedKey::from_bytes([1u8; 32]),
            SessionId::ZERO,
            Direction::ClientToServer,
        );
        let mut injector = FailingInjector;
        let mut held = HeldKeys::new();

        server
            .send(&Message::Key {
                usage: Usage::C,
                pressed: true,
            })
            .await
            .expect("send first message");
        server
            .send(&Message::Key {
                usage: Usage::A,
                pressed: true,
            })
            .await
            .expect("send second message");

        // The first pump call injects a rejected event and must still
        // return Ok, not surface the injector's error to the caller.
        assert!(pump_client(&mut client, &mut injector, &mut held)
            .await
            .is_ok());
        // A second message must still be received and processed, proving
        // the earlier rejection did not stall the receive loop.
        assert!(pump_client(&mut client, &mut injector, &mut held)
            .await
            .is_ok());

        // held still records both keys as pressed: pump_client tracks
        // what it attempted to inject regardless of whether the platform
        // accepted it, since the alternative (only recording successful
        // injections) would make ReleaseAllKeys release the wrong set the
        // moment the platform starts failing.
        assert_eq!(held.held(), vec![Usage::A, Usage::C]);
    }
}
