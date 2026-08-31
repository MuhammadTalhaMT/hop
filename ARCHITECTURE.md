# Architecture

hop separates logic that can be tested anywhere from code that must talk
to an operating system.

## Crates

- `hop-proto`: wire messages, codec, encryption, replay protection.
  Modules: `keys` (the `Usage` HID key type), `message` (the `Message`
  enum and its codec), `crypto` (`seal`/`open` under
  XChaCha20-Poly1305), `replay` (`ReplayWindow`). No sockets, no OS
  calls, no `unsafe` (`#![forbid(unsafe_code)]`).
- `hop-core`: control state machine, key remapping, held-key tracking,
  framed transport, liveness, reconnect backoff, and the glue that
  drives them from a `Capturer`/`Injector` pair. Modules: `remap`,
  `held`, `control`, `transport`, `liveness`, `backoff`, `device`,
  `session`. Talks to hardware only through the `Capturer` and
  `Injector` traits in `device`. No `unsafe`.

## The platform boundary

Two traits in `hop-core::device` are the entire surface between this tool
and an operating system:

- `Capturer` yields local input events and can swallow them. `poll`
  is non-blocking by contract: it returns immediately with `None` when
  nothing is pending rather than waiting for the next event. A real
  consumer loop (the eventual supervisor) is responsible for its own
  pacing between polls; `pump_server` itself just drains whatever is
  available in one pass and returns.
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

`Transport<S>` wraps any `AsyncRead + AsyncWrite` stream (an in-memory
duplex pipe in tests, a TCP socket in the eventual runtime) with a
length-prefixed, encrypted frame format: a big-endian `u32` length
followed by a sealed frame containing the sequence number, nonce, and
ciphertext.

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

`Transport::recv` is **not cancel safe**. It reads the length prefix and
then the frame body with two separate `read_exact` calls; if the future is
dropped between or during those reads (for example by losing a
`tokio::select!` race against a timer), any bytes already consumed are
lost and the stream desynchronizes permanently: the next `recv` reads from
the middle of a stale frame and every call after that fails. Callers that
need to wait on `recv` alongside something else must run it on its own
task rather than selecting on it directly, unless `Transport` first grows
a persistent read buffer across calls.

The sequence number is authenticated (it is AEAD associated data) before
it is ever shown to `ReplayWindow::accept`. Checking the replay window
before authentication would let an attacker forge a single frame claiming
`seq = u64::MAX`, without knowing the key, and permanently pin the window
so every genuine frame after it is rejected.

## Control and held keys

`Control` is the focus state machine (`Focus::Local` / `Focus::Remote`).
Every path that leaves `Remote` (an explicit release, a disconnect, or the
panic hotkey) returns focus to `Local` and, if anything was held, returns
`Action::ReleaseAll` so the caller sends `Message::ReleaseAllKeys`.

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
  definition unreachable at that point; see the notes below on what the
  next layer must do about that.

## What the next plan (the platform layer) must handle

These were discovered while building the core and are not solved by
anything in `hop-proto` or `hop-core`. They need to be handled correctly
by whichever plan adds macOS capture, Windows injection, and the
supervisor:

- **macOS can silently disable the event tap.** A `CGEventTap` can be
  disabled unilaterally by the OS, delivering a
  `kCGEventTapDisabledByTimeout` or `kCGEventTapDisabledByUserInput`
  event rather than crashing or returning an error. A capture
  implementation that does not watch for those two event types and
  re-enable the tap will simply stop receiving input and look like it
  needs a restart, with nothing in the logs to explain why. This is the
  specific failure that motivated this project.
- **macOS injection needs an absolute cursor position.** `CGEventPost`
  moves the pointer to an absolute location, not by a relative delta.
  Since hop's wire protocol and `InputEvent::Mouse` carry relative deltas
  (`dx`, `dy`), the macOS injector must track or query the current cursor
  position itself and add the incoming delta to it before posting.
- **The client, not the server, must release its own keys on a real
  disconnect.** `send_release_all` only works while the transport is
  still usable. On a genuine disconnect the server cannot deliver
  `ReleaseAllKeys` at all, because the peer is unreachable by definition.
  The client side must independently detect a dead connection (via
  `Liveness`) and release everything in its own `HeldKeys` without
  waiting for a message that will never arrive.
- **`Transport::recv` needs a supervision strategy, not a `select!`.**
  Because `recv` is not cancel safe (see above), whatever drives the
  client's receive loop alongside a liveness timer must give `recv` its
  own task and communicate results back over a channel, or `Transport`
  must be extended with a persistent read buffer first. Wiring `recv`
  directly into a `tokio::select!` against a timeout will eventually
  desynchronize the stream.
- **The handshake must derive a real `SessionId` before any input is
  processed.** `hop_proto::crypto::SessionId` now binds every frame to a
  session, so a frame sealed under one session cannot authenticate under
  another. But until this plan implements the handshake, both peers
  construct their `Transport` with `SessionId::ZERO`, which is the same
  value on every connection, so in a running system this binding
  currently provides no protection at all: a session recorded against
  `ZERO` still authenticates against the next connection's `ZERO`. The
  handshake must derive a fresh, per-session `SessionId` from the nonces
  both peers exchange in `Message::Handshake`, and it must do so before
  either side processes any input message, not merely before it sends
  the first one.
- **Nothing currently binds direction.** Both the client and the server
  hold the same `SharedKey`, and `seal`/`open` do not distinguish who
  sealed a frame. `Transport` today only has a working `send` path on
  one side and a working `recv` path on the other in practice, but
  nothing in the types enforces that: once the server also gains a
  receive path, a frame the server sent could be captured and reflected
  back at the server itself, and nothing stops a client from sending an
  input message the server would otherwise only expect to originate
  server-side. This plan must close that gap, for example by deriving
  separate directional keys (one for server-to-client, one for
  client-to-server) from the shared secret, or by including a role byte
  in the AAD so a frame sealed as "from the server" is rejected if it
  arrives claiming to be from the client.
