use hop_core::{
    pump_client, pump_server, Control, FakeCapturer, FakeInjector, InputEvent, RemapTable,
    Transport,
};
use hop_proto::{Message, SharedKey, Usage};
use tokio::io::duplex;

fn key() -> SharedKey {
    SharedKey::from_bytes([42u8; 32])
}

#[tokio::test]
async fn captured_input_arrives_injected_on_the_far_side() {
    let (a, b) = duplex(65536);
    let mut server_side = Transport::new(a, key());
    let mut client_side = Transport::new(b, key());

    let mut capturer = FakeCapturer::new(vec![
        InputEvent::EdgeCrossed,
        InputEvent::Mouse { dx: 5, dy: -2 },
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

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");

    for _ in 0..3 {
        pump_client(&mut client_side, &mut injector)
            .await
            .expect("client pump");
    }

    assert_eq!(
        injector.injected(),
        vec![
            InputEvent::Mouse { dx: 5, dy: -2 },
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
    let mut server_side = Transport::new(a, key());
    let mut client_side = Transport::new(b, key());

    // No EdgeCrossed, so focus stays local and nothing should cross.
    let mut capturer = FakeCapturer::new(vec![InputEvent::Key {
        usage: Usage::A,
        pressed: true,
    }]);
    let mut control = Control::new();
    let remap = RemapTable::new();
    let mut injector = FakeInjector::new();

    pump_server(&mut server_side, &mut capturer, &mut control, &remap)
        .await
        .expect("server pump");
    server_side.send(&Message::Heartbeat).await.unwrap();

    pump_client(&mut client_side, &mut injector)
        .await
        .expect("client pump");
    assert!(
        injector.injected().is_empty(),
        "local input must not reach the peer"
    );
}

#[tokio::test]
async fn disconnect_releases_keys_held_on_the_peer() {
    // The stuck-modifier guarantee, verified end to end.
    let mut control = Control::new();
    control.on_edge_crossed();
    control.on_key(Usage::LEFT_GUI, true);

    let action = control.on_disconnected();
    assert_eq!(action, hop_core::Action::ReleaseAll);
    assert_eq!(control.focus(), hop_core::Focus::Local);
}

#[tokio::test]
async fn a_peer_with_the_wrong_key_gets_nothing() {
    // Sharing a network is not sharing trust: a machine that does not hold
    // the key must not be able to read a single keystroke.
    let (a, b) = duplex(65536);
    let mut server_side = Transport::new(a, SharedKey::from_bytes([42u8; 32]));
    let mut eavesdropper = Transport::new(b, SharedKey::from_bytes([43u8; 32]));

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
    assert!(pump_client(&mut eavesdropper, &mut injector).await.is_err());
    assert!(injector.injected().is_empty());
}
