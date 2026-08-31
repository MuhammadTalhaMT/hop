use crate::{
    Action, Capturer, Control, HeldKeys, Injector, InputEvent, RemapTable, Transport,
    TransportError,
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
pub async fn pump_server<S, C>(
    transport: &mut Transport<S>,
    capturer: &mut C,
    control: &mut Control,
    remap: &RemapTable,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
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
pub async fn pump_client<S, I>(
    transport: &mut Transport<S>,
    injector: &mut I,
    held: &mut HeldKeys,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
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
                let _ = injector.inject(&InputEvent::Key {
                    usage,
                    pressed: false,
                });
            }
        }
        Message::Key { usage, pressed } => {
            held.record(usage, pressed);
            let _ = injector.inject(&InputEvent::Key { usage, pressed });
        }
        other => {
            if let Some(event) = message_to_event(&other) {
                let _ = injector.inject(&event);
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
pub async fn send_release_all<S>(transport: &mut Transport<S>) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    transport.send(&Message::ReleaseAllKeys).await
}
