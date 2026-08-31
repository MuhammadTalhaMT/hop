use hop_core::{
    pump_client, pump_server, send_release_all, split, Control, FakeCapturer, FakeInjector,
    HeldKeys, InputEvent, RemapTable,
};
use hop_proto::{Button, Direction, Message, SessionId, SharedKey, Usage};
use std::time::Duration;
use tokio::io::duplex;
use tokio::time::timeout;

/// The client's `recv` blocks forever on a duplex that stays open with
/// nothing new written to it. A regression that causes fewer messages to
/// arrive than a test expects must fail fast, not hang the suite, so every
/// `pump_client` call in these tests is wrapped in this.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

fn key() -> SharedKey {
    SharedKey::from_bytes([42u8; 32])
}

fn session() -> SessionId {
    SessionId([9u8; 32])
}

/// Direction passed to `split` for the server side's transport half in
/// these tests: the server always writes and the client always reads.
fn server_dir() -> Direction {
    Direction::ServerToClient
}

/// Direction passed to `split` for the client side's transport half.
fn client_dir() -> Direction {
    Direction::ClientToServer
}

async fn pump_client_or_timeout<R>(
    transport: &mut hop_core::TransportReader<R>,
    injector: &mut FakeInjector,
    held: &mut HeldKeys,
) -> Result<(), hop_core::TransportError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    timeout(CLIENT_TIMEOUT, pump_client(transport, injector, held))
        .await
        .expect("client pump should not hang")
}

#[tokio::test]
async fn captured_input_arrives_injected_on_the_far_side() {
    let (a, b) = duplex(65536);
    let (_a_reader, mut server_side) = split(a, key(), session(), server_dir());
    let (mut client_side, _b_writer) = split(b, key(), session(), client_dir());

    // Every input-carrying InputEvent variant is exercised here so that
    // dropping or mis-mapping any one of them (for example Scroll arriving
    // as Mouse) is caught by the exact-variant assertion below.
    let mut capturer = FakeCapturer::new(vec![
        InputEvent::EdgeCrossed,
        InputEvent::Mouse { dx: 5, dy: -2 },
        InputEvent::Scroll { dx: 1, dy: -3 },
        InputEvent::Button {
            button: Button::Left,
            pressed: true,
        },
        InputEvent::Key {
            usage: Usage::LEFT_GUI,
            pressed: true,
        },
        InputEvent::Key {
            usage: Usage::C,
            pressed: true,
        },
    ]);
    let mut control = Control::new();
    let remap = RemapTable::mac_to_windows_defaults();
    let mut injector = FakeInjector::new();
    let mut held = HeldKeys::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");

    for _ in 0..5 {
        pump_client_or_timeout(&mut client_side, &mut injector, &mut held)
            .await
            .expect("client pump");
    }

    assert_eq!(
        injector.injected(),
        vec![
            InputEvent::Mouse { dx: 5, dy: -2 },
            InputEvent::Scroll { dx: 1, dy: -3 },
            InputEvent::Button {
                button: Button::Left,
                pressed: true
            },
            // Cmd was remapped to Ctrl before it left the Mac.
            InputEvent::Key {
                usage: Usage::LEFT_CTRL,
                pressed: true
            },
            InputEvent::Key {
                usage: Usage::C,
                pressed: true
            },
        ]
    );
}

#[tokio::test]
async fn input_before_the_edge_is_not_forwarded() {
    let (a, b) = duplex(65536);
    let (_a_reader, mut server_side) = split(a, key(), session(), server_dir());
    let (mut client_side, _b_writer) = split(b, key(), session(), client_dir());

    // No EdgeCrossed, so focus stays local and nothing should cross.
    let mut capturer = FakeCapturer::new(vec![InputEvent::Key {
        usage: Usage::A,
        pressed: true,
    }]);
    let mut control = Control::new();
    let remap = RemapTable::new();
    let mut injector = FakeInjector::new();
    let mut held = HeldKeys::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");
    server_side.send(&Message::Heartbeat).await.unwrap();

    pump_client_or_timeout(&mut client_side, &mut injector, &mut held)
        .await
        .expect("client pump");
    assert!(
        injector.injected().is_empty(),
        "local input must not reach the peer"
    );
}

#[tokio::test]
async fn local_pointer_and_click_activity_does_not_cross_the_wire() {
    // Focus stays Local the whole time (no EdgeCrossed), so mouse motion,
    // scroll and clicks must not reach the peer: the "cursor is on the Mac
    // but the PC pointer moves anyway" bug. This exercises the `other =>`
    // arm of pump_server specifically, since Mouse/Scroll/Button all fall
    // into it rather than the dedicated Key arm.
    let (a, b) = duplex(65536);
    let (_a_reader, mut server_side) = split(a, key(), session(), server_dir());
    let (mut client_side, _b_writer) = split(b, key(), session(), client_dir());

    let mut capturer = FakeCapturer::new(vec![
        InputEvent::Mouse { dx: 3, dy: 4 },
        InputEvent::Scroll { dx: 0, dy: 1 },
        InputEvent::Button {
            button: Button::Left,
            pressed: true,
        },
    ]);
    let mut control = Control::new();
    let remap = RemapTable::new();
    let mut injector = FakeInjector::new();
    let mut held = HeldKeys::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");
    // Give the client something to receive so a missing focus gate (which
    // would forward the events above) is distinguished from an empty pipe
    // that would otherwise make this pass for the wrong reason.
    server_side.send(&Message::Heartbeat).await.unwrap();

    pump_client_or_timeout(&mut client_side, &mut injector, &mut held)
        .await
        .expect("client pump");

    assert!(
        injector.injected().is_empty(),
        "local mouse activity must not reach the peer"
    );
}

#[tokio::test]
async fn explicit_release_clears_keys_held_on_the_peer() {
    // Genuinely crosses the socket, unlike a state-machine-only test: a
    // modifier is captured, forwarded, injected on the client and tracked
    // there as held. The server then takes Action::ReleaseAll from an
    // explicit release request, sends ReleaseAllKeys over the same
    // transport, and the client must inject the matching key-up and end up
    // holding nothing. This is the stuck-modifier guarantee verified end to
    // end rather than on Control in isolation.
    let (a, b) = duplex(65536);
    let (_a_reader, mut server_side) = split(a, key(), session(), server_dir());
    let (mut client_side, _b_writer) = split(b, key(), session(), client_dir());

    let mut capturer = FakeCapturer::new(vec![
        InputEvent::EdgeCrossed,
        InputEvent::Key {
            usage: Usage::LEFT_GUI,
            pressed: true,
        },
    ]);
    let mut control = Control::new();
    let remap = RemapTable::new();
    let mut injector = FakeInjector::new();
    let mut held = HeldKeys::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");

    pump_client_or_timeout(&mut client_side, &mut injector, &mut held)
        .await
        .expect("client pump");

    assert_eq!(
        injector.injected(),
        vec![InputEvent::Key {
            usage: Usage::LEFT_GUI,
            pressed: true
        }]
    );
    assert_eq!(
        held.held(),
        vec![Usage::LEFT_GUI],
        "the client must consider the key it just injected as held"
    );

    let action = control.on_release_requested();
    assert_eq!(action, hop_core::Action::ReleaseAll);
    assert_eq!(control.focus(), hop_core::Focus::Local);

    send_release_all(&mut server_side)
        .await
        .expect("send release all");

    pump_client_or_timeout(&mut client_side, &mut injector, &mut held)
        .await
        .expect("client pump");

    assert_eq!(
        injector.injected(),
        vec![
            InputEvent::Key {
                usage: Usage::LEFT_GUI,
                pressed: true
            },
            InputEvent::Key {
                usage: Usage::LEFT_GUI,
                pressed: false
            },
        ]
    );
    assert!(
        held.is_empty(),
        "the client must not consider anything held after ReleaseAllKeys"
    );
}

#[tokio::test]
async fn a_peer_with_the_wrong_key_gets_nothing() {
    // Sharing a network is not sharing trust: a machine that does not hold
    // the key must not be able to read a single keystroke.
    let (a, b) = duplex(65536);
    let (_a_reader, mut server_side) = split(
        a,
        SharedKey::from_bytes([42u8; 32]),
        session(),
        server_dir(),
    );
    let (mut eavesdropper, _b_writer) = split(
        b,
        SharedKey::from_bytes([43u8; 32]),
        session(),
        client_dir(),
    );

    let mut capturer = FakeCapturer::new(vec![
        InputEvent::EdgeCrossed,
        InputEvent::Key {
            usage: Usage::C,
            pressed: true,
        },
    ]);
    let mut control = Control::new();
    let remap = RemapTable::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");

    let mut injector = FakeInjector::new();
    let mut held = HeldKeys::new();
    assert!(
        pump_client_or_timeout(&mut eavesdropper, &mut injector, &mut held)
            .await
            .is_err()
    );
    assert!(injector.injected().is_empty());
}
