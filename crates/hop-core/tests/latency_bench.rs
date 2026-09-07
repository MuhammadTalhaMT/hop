//! Scratch benchmark for input latency, not part of the normal suite.
//!
//! Run manually with:
//! `CARGO_TARGET_DIR=/tmp/hopcheck cargo test --workspace --test latency_bench -- --ignored --nocapture`
//!
//! Measures how long a batch of N `MouseMove` events takes to travel from
//! `pump_server`'s input to the far side of a real loopback `TcpStream`,
//! with `TCP_NODELAY` off and on, so the Nagle effect (or its absence on
//! loopback) is measured rather than assumed. Deliberately uses a real
//! `TcpStream`, not an in-memory duplex pipe: `TCP_NODELAY` has no meaning
//! on a pipe.

use hop_core::{pump_server, split, Control, FakeCapturer, InputEvent, RemapTable};
use hop_proto::{Direction, Message, SessionId, SharedKey};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

const N: usize = 200;

fn key() -> SharedKey {
    SharedKey::from_bytes([7u8; 32])
}

fn session() -> SessionId {
    SessionId([3u8; 32])
}

/// Sets up a loopback `TcpStream` pair with `TCP_NODELAY` configured as
/// requested on both ends, runs `pump_server` once over `N` queued
/// `MouseMove` events, and returns how long it took for every resulting
/// frame to be received on the far side. Works whether or not
/// `pump_server` coalesces the batch into fewer frames: the loop below
/// sums `dx` off however many frames actually arrive.
async fn measure(nodelay: bool) -> Duration {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if nodelay {
            stream.set_nodelay(true).unwrap();
        }
        stream
    });

    let client = TcpStream::connect(addr).await.unwrap();
    if nodelay {
        client.set_nodelay(true).unwrap();
    }
    let server_stream = accept.await.unwrap();

    let (_server_reader, mut server_writer) =
        split(server_stream, key(), session(), Direction::ServerToClient);
    let (mut client_reader, _client_writer) =
        split(client, key(), session(), Direction::ClientToServer);

    let events: Vec<InputEvent> = (0..N).map(|_| InputEvent::Mouse { dx: 1, dy: 1 }).collect();
    let mut capturer = FakeCapturer::new(events);
    let mut control = Control::new();
    control.on_edge_crossed(); // move focus onto the peer so motion forwards
    let remap = RemapTable::new();

    let start = Instant::now();
    pump_server(
        &mut server_writer,
        &mut capturer,
        &mut control,
        &remap,
        None,
    )
    .await
    .expect("pump_server should not fail on a live loopback socket");

    let mut total_dx = 0i32;
    let mut frames = 0u32;
    while total_dx < N as i32 {
        match client_reader.recv().await.expect("recv should not fail") {
            Message::MouseMove { dx, .. } => {
                total_dx += dx;
                frames += 1;
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    let elapsed = start.elapsed();
    println!("  nodelay={nodelay:<5} frames={frames:<4} elapsed={elapsed:?}");
    elapsed
}

#[tokio::test]
#[ignore]
async fn nagle_and_coalescing_latency() {
    println!("N = {N} MouseMove events, one pump_server batch, loopback TcpStream");
    let without_nodelay = measure(false).await;
    let with_nodelay = measure(true).await;
    println!("summary: nodelay off = {without_nodelay:?}, nodelay on = {with_nodelay:?}");
}
