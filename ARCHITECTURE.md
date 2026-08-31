# Architecture

hop separates logic that can be tested anywhere from code that must talk
to an operating system.

## Crates

- `hop-proto`: wire messages, codec, encryption, replay protection.
  Modules: `keys` (the `Usage` HID key type), `message` (the `Message`
  enum and its codec), `crypto` (`seal`/`open` under
  XChaCha20-Poly1305, `Direction`, `SessionId`), `replay`
  (`ReplayWindow`). No sockets, no OS calls, no `unsafe`
  (`#![forbid(unsafe_code)]`).
- `hop-core`: control state machine, key remapping, held-key tracking,
  the split transport, handshake, liveness, reconnect backoff, the
  client supervisor, and the glue that drives them from a
  `Capturer`/`Injector` pair. Modules: `remap`, `held`, `control`,
  `transport`, `handshake`, `liveness`, `backoff`, `supervisor`,
  `device`, `session`. Talks to hardware only through the `Capturer`
  and `Injector` traits in `device`. No `unsafe`
  (`#![forbid(unsafe_code)]`).
- `hop-platform`: operating system implementations of `hop-core`'s
  `Capturer` and `Injector` traits. macOS capture through a
  `CGEventTap` (module `macos`, including edge detection and cursor
  parking in `macos::cursor`), Windows input injection through
  `SendInput` (module `windows::inject`), and the keycode tables that
  translate each platform's native codes to and from `hop_proto::Usage`
  (`macos::keymap`, `windows::keymap`). This is the only crate in the
  workspace permitted `unsafe`; every `unsafe` block here must carry a
  comment justifying why it is sound.
- `hop`: the binary crate. `config` loads and validates the TOML config
  file, `keymap` resolves user-facing key names, and `main`/`run` wire
  up the `keygen` and `run` subcommands on top of `hop-core` and
  `hop-platform`. No `unsafe` (`#![forbid(unsafe_code)]`). Holds no
  protocol or state-machine logic of its own.

## The platform boundary

Two traits in `hop-core::device` are the entire surface between this tool
and an operating system:

- `Capturer` yields local input events and can swallow them. `poll`
  is non-blocking by contract: it returns immediately with `None` when
  nothing is pending rather than waiting for the next event. A real
  consumer loop is responsible for its own pacing between polls;
  `pump_server` itself just drains whatever is available in one pass
  and returns.
- `Injector` replays events as though they came from real hardware.

`FakeCapturer` and `FakeInjector` implement them for tests, which is why
the full input path can be verified without a mouse or a second machine.

Adding a platform means adding one implementation of each trait. No shared
logic changes. This is the intended shape of a Linux contribution.

## Key representation

Keys travel as USB HID usage codes (`hop_proto::Usage`, Keyboard page
0x07), never as platform key codes. Each platform translates at its edge,
and remapping (`RemapTable`) is a pure function from one canonical code
to another, applied once at the sending side. A rule's output is never
fed back through the table, so chained rules cannot rewrite each other.

## Wire format

`Message` is encoded with an explicit `u16` tag ahead of a postcard-encoded
body, and `decode` dispatches on that tag by hand rather than relying on
serde's built-in enum representation. This is deliberate: serde cannot
express "ignore variants you have not heard of" for an externally tagged
enum, and that behavior is required so that a newer peer can add message
types (for example clipboard support) without breaking an older build. An
unrecognized tag decodes to `Message::Unknown`, which is treated as inert
data by everything downstream: `message_to_event` produces no event for
it, and `pump_client` neither injects anything for it nor treats it as an
error.

## Transport

There is no single `Transport` type. `split()` takes any `AsyncRead +
AsyncWrite` stream (an in-memory duplex pipe in tests, a TCP socket at
runtime) and returns a `TransportReader` and a `TransportWriter`, each
owning only the half of the stream and the state its direction needs.
Both halves speak the same length-prefixed, encrypted frame format: a
big-endian `u32` length followed by a sealed frame containing the
sequence number, nonce, and ciphertext.

The split exists because a single type with both `send` and `recv`
taking `&mut self` cannot do bidirectional I/O: `recv` is not cancel
safe (see below), so it must run to completion once started, which
means neither "run `recv` on its own task" (it strands `send`, and the
`send_seq` counter, on whichever side does not hold the task) nor
`Arc<Mutex<Transport>>` (a parked `recv` holds the lock for as long as
it takes the peer to send the next frame, so `send` on the same `Arc`
deadlocks behind it) can compose. Splitting into independent halves is
what let `Liveness` gain a real consumer: a `TransportReader` can be
raced against a heartbeat timer in one task while a `TransportWriter`
sends from another.

Each direction keeps its own sequence counter and its own replay
window, and `TransportWriter` seals frames under the direction it was
given while the paired `TransportReader` only accepts frames sealed
under the opposite direction. That is what stops a reflected frame
(this side's own outbound frame, echoed back to it) from authenticating
as inbound.

Dropping only one half does not close the underlying connection: the
other half still holds its share of the stream, so no EOF is ever
delivered and a `recv` on the surviving half blocks forever. Tearing a
connection down means dropping both halves.

`TransportError` has six variants, and the distinctions between them are
load bearing for a caller deciding how to react:

- `Io`: the underlying stream failed. After an `Io` error from `send`, a
  partial frame may already be on the wire, so the `Transport` must be
  discarded and the connection re-established rather than reused.
- `Crypto`: decryption or authentication failed, for example a peer using
  the wrong key.
- `Replay`: the frame's sequence number was already seen or is older than
  the replay window.
- `FrameTooLarge`: the declared length exceeds `MAX_FRAME` (64 KiB). This
  is checked before any buffer is allocated, so a peer cannot make the
  receiver allocate whatever size it claims.
- `Closed`: the peer closed the connection cleanly, between frames.
- `Truncated`: the peer vanished in the middle of a frame. This is kept
  distinct from `Closed` because a mid-frame death is a fault worth
  logging or rate limiting, not a graceful shutdown.

`TransportReader::recv` is **not cancel safe**. It reads the length
prefix and then the frame body with two separate `read_exact` calls; if
the future is dropped between or during those reads (for example by
losing a `tokio::select!` race against a timer), any bytes already
consumed are lost and the stream desynchronizes permanently: the next
`recv` reads from the middle of a stale frame and every call after that
fails. `ClientSupervisor` never races `recv` directly against the
heartbeat ticker for exactly this reason. It gives the `TransportReader`
its own task that always runs `recv` to completion and forwards each
result over an `mpsc` channel; the supervisor's own `select!` loop then
races `rx.recv()` (cancel safe, since a dropped channel receive loses
nothing already read off the socket) against the heartbeat interval.

The sequence number is authenticated (it is AEAD associated data) before
it is ever shown to `ReplayWindow::accept`. Checking the replay window
before authentication would let an attacker forge a single frame claiming
`seq = u64::MAX`, without knowing the key, and permanently pin the window
so every genuine frame after it is rejected.

## Control and held keys

`Control` is the focus state machine (`Focus::Local` / `Focus::Remote`).
Every path that leaves `Remote` (an explicit release, a disconnect, the
panic hotkey, or waking from sleep) returns focus to `Local` and, if
anything was held, returns `Action::ReleaseAll` so the caller sends
`Message::ReleaseAllKeys`.

There are two distinct `HeldKeys` in play, recording different things for
different reasons:

- The server side's `Control` holds a `HeldKeys` that records *pre-remap*
  usages as captured locally. It exists purely as a "was anything held at
  all" signal, deciding whether a transition needs to send
  `ReleaseAllKeys` in the first place. It is not a reliable per-key
  record on the wire, because the usages that actually cross the wire
  have been through `RemapTable::apply` and may differ from what this set
  recorded.
- The client passed into `pump_client` keeps its own `HeldKeys`, recording
  the *post-remap* usages it has actually injected on this machine. This
  set is authoritative: when `Message::ReleaseAllKeys` arrives,
  `pump_client` releases exactly the keys in this set, never anything the
  peer's message claims, so the release always matches what is physically
  down on this keyboard.

## `session` module

`session.rs` is the glue between the state machine, the transport, and
the device traits. It exports:

- `event_to_message` / `message_to_event`: pure translation between
  `InputEvent` and `Message`, with remapping applied on the sending side.
- `pump_server`: drains a `Capturer`, runs each event through `Control`,
  and forwards what should go to the peer.
- `pump_client`: receives one message and injects it, maintaining the
  client's own `HeldKeys` and answering `ReleaseAllKeys` from it.
- `send_release_all`: sends `Message::ReleaseAllKeys` to the peer. Used
  when `Control` returns `Action::ReleaseAll` and the link is still
  usable. It cannot help on a genuine disconnect, since the peer is by
  definition unreachable at that point; see `handshake` and `supervisor`
  below for how the client covers that case independently.

## `handshake` module

A handshake runs on every connection before any input message is
processed. `client_handshake` and `server_handshake` each split the raw
stream under the fixed `SessionId::ZERO` just long enough to exchange
`Message::Handshake` (each side's random nonce and peer id), then
reclaim the stream and call `split` again with the real session,
`derive_session(client_nonce, server_nonce)`. The `SessionId::ZERO`-keyed
halves are never returned to the caller, so nothing outside
`hop-core::handshake` can send or receive input under `ZERO`; a caller
only ever gets back a `TransportReader`/`TransportWriter` pair keyed by
the derived session.

Because both `Message::Handshake` messages necessarily travel under the
fixed `SessionId::ZERO`, an attacker who recorded an earlier legitimate
handshake can replay it into a new connection and produce the same
derived session, without ever knowing the key. `ClientSupervisor`
accounts for this: it does not reset the reconnect backoff on handshake
completion, only once a real frame has actually authenticated under the
derived session. Both sides of the handshake are also time bounded, so
a peer that stops responding mid-handshake does not block the caller
forever.

`split` is called with `Direction::ClientToServer` on the client and
`Direction::ServerToClient` on the server, so the two sides' writers
seal under opposite directions and their readers each require the
opposite of what they seal; see the Transport section above for what
that buys.

## `supervisor` module

`ClientSupervisor` is the client-side loop that owns reconnection,
liveness, and self-healing:

- `ReconnectPolicy` wraps `Backoff` with capped exponential delay and
  never gives up; a failed connection attempt increases the delay, a
  confirmed one resets it.
- On every reconnect it runs `client_handshake`, then drives the
  connection with the reader on its own task (see the Transport section
  above) racing a heartbeat ticker in a `select!` loop.
- `Liveness` tracks inbound and outbound activity on separate clocks:
  the client's own outbound heartbeats never count as evidence the peer
  is alive, so a half-open socket (one that still accepts writes but
  has stopped delivering reads) is detected instead of looking healthy
  forever.
- When the liveness timer decides the connection is dead, or the
  transport errors out, or the reader task ends, `release_everything`
  releases every key in the client's own `HeldKeys` through the
  `Injector` before the loop reconnects. This is what makes the client
  self-healing on a disconnect the server has no way to signal: it does
  not wait for `Message::ReleaseAllKeys`, which by definition cannot
  arrive once the link is actually down.

## What is still outstanding

Everything below is genuinely unbuilt or unverified as of this writing,
not a stale list of solved problems:

- **Nothing has run on real hardware yet.** The handshake, the
  supervisor, macOS capture, and Windows injection are each covered by
  automated tests and, for the platform layer, a manual spike, but no
  human has confirmed the cursor actually crosses to a real PC, that
  keys arrive correctly, or that a real sleep/wake cycle recovers
  cleanly.
- **There is no macOS injector and no Windows capturer.** `hop-platform`
  implements `Capturer` for macOS and `Injector` for Windows only, and
  `hop::run` enforces that pairing: the macOS build refuses `role =
  "client"` and the Windows build refuses `role = "server"`. Only the
  Mac's keyboard and mouse can drive the PC; there is no path for the
  PC's input to drive the Mac. Building that direction would require a
  macOS injector, which is where `CGEventPost`'s absolute-position
  requirement (posting a location, not a relative delta, while the wire
  protocol carries relative `dx`/`dy`) would become relevant again.
- **Peer discovery does not exist.** `hop::config::DiscoveryConfig` has
  an `enabled` flag, but nothing in `hop::run` reads it or performs any
  discovery; addresses are configured by hand in the TOML file.
- **No clipboard sharing and no file transfer.**
- **Edge detection assumes a single display.**
  `hop_platform::macos::cursor::screen_size` reads only
  `CGDisplay::main().bounds()`, so a Mac with more than one monitor
  attached is likely to see edge crossings misbehave on displays other
  than the main one.
- **Scroll sensitivity is unverified and likely wrong for trackpads.**
  `hop_platform::windows::inject::wheel_delta` multiplies whatever
  delta it receives by `WHEEL_DELTA` with no separate scaling for
  continuous input. On macOS, continuous scroll events (trackpads and
  Magic Mouse, the primary pointing devices on Apple Silicon laptops)
  report pixel-scale point deltas rather than line deltas, so this path
  is expected to be over-sensitive; it has not been checked against real
  trackpad hardware.
- **Config paths are not expanded.** `~` and `%APPDATA%` in
  `key_file` are taken literally, not resolved to a home or app-data
  directory, so key paths must be written out in full.
