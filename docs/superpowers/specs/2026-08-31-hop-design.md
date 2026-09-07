# hop: design

Date: 2026-08-31
Status: approved for planning

## Problem

Sharing one keyboard and mouse between a macOS machine and a Windows PC.
Existing tools in this space fail in the same way: after the Mac locks or
sleeps, input sharing silently stops and the user must restart the
application by hand.

Tools evaluated and rejected:

| Tool | Why not |
| --- | --- |
| Deskflow | Crashes (SIGABRT/SIGTRAP) on lock/session change; open upstream issues |
| ShareMouse | Free tier caps at 4 displays; this setup exceeds it |
| Synergy | Same sleep/wake reliability complaints, and paid |
| Barrier | Unmaintained |
| Input Leap | Archived July 2026 |
| Lan Mouse | Viable, but no clipboard, no auto-discovery, ad-hoc signed |

The goal is not feature parity with any of them. The goal is a tool that
recovers by itself, every time, without being restarted.

## Scope

In scope for v1:

- Mouse movement, buttons, and scroll from the Mac to the PC
- Keyboard from the Mac to the PC, with configurable key remapping
- Automatic recovery from lock, sleep, network loss, and peer restart
- Authenticated encryption over the LAN
- Discovery of other hop machines on the network, so peers are chosen
  from a list rather than typed in as IP addresses

Explicitly out of scope for v1, but the design must not preclude them:

- Clipboard sharing
- Drag and drop file transfer
- Linux support

## Roles and topology

One binary runs on both machines and reads its config to determine role.

- macOS is the **server**: it owns the physical keyboard and mouse.
- Windows is the **client**: it replays received input.

The user's PC monitors sit physically above the Mac, so the cursor leaves
the top edge of the Mac and returns through the bottom edge of the PC.
This mapping is config, not code.

## Architecture

The system splits into pure logic and a thin platform layer. Nearly all
complexity lives in the pure half so that it is testable without hardware.

```
crates/
  hop-proto/      wire types, codec, crypto
  hop-core/       state machine, remap, net, supervision
  hop-platform/   Capturer + Injector traits, macos.rs, windows.rs
  hop/            binary: CLI, config loading
```

### hop-proto

Wire message types, their encoding, and the cryptographic envelope.
No OS calls, no sockets. Fully unit tested.

Message types for v1:

- `Handshake { version, capabilities, nonce }`
- `MouseMove { dx, dy }` (relative deltas, not absolute coordinates)
- `MouseButton { button, pressed }`
- `Scroll { dx, dy }`
- `Key { usage, pressed }` (canonical USB HID usage code)
- `ReleaseAllKeys`
- `Heartbeat`
- `Release` (client tells server to take control back)

Unknown message types are ignored rather than treated as fatal. This is
what allows a future client that speaks clipboard messages to interoperate
with one that does not.

### hop-core

The state machine, the remap engine, the network loop, and task
supervision. Depends on the platform traits, never on a concrete platform.

### hop-platform

Two traits, each with a macOS and a Windows implementation:

- `Capturer`: yields input events and can swallow them so the local OS
  does not also process them. macOS uses `CGEventTap`.
- `Injector`: replays an event as though it came from real hardware.
  Windows uses `SendInput`.

Adding Linux means adding one file here and touching nothing else.

## Key representation

The wire protocol never carries platform-specific key codes. Capture
translates native codes to canonical USB HID usage codes; injection
translates canonical back to native. Remapping is therefore a pure
function from canonical to canonical, applied per destination peer, and is
testable with no OS involvement.

Default Mac to Windows mapping:

| Mac key | Windows key | Reason |
| --- | --- | --- |
| Left/Right Cmd | Left/Right Ctrl | So Cmd+C copies on the PC |
| Option | Alt | Positional equivalent |
| Control | Control | Unchanged |

The table is fully user-editable per peer in config.

## Reliability design

This section is the reason the project exists.

### macOS event tap invalidation

macOS unilaterally disables a `CGEventTap`, delivering
`kCGEventTapDisabledByTimeout` or `kCGEventTapDisabledByUserInput`. This
is not a crash and returns no error; the tap simply stops delivering
events. A program that does not handle these two event types explicitly
appears to hang and requires a restart. This is the most likely root cause
of the failure mode that motivated this project.

Mitigations:

1. The tap handler treats both disable events as first-class and
   re-enables the tap immediately.
2. A watchdog re-arms the tap if no events arrive for an unreasonable
   interval while in a state that should be producing them.
3. The process subscribes to `NSWorkspace` sleep, wake, lock, and unlock
   notifications, and proactively rebuilds the tap and socket on wake
   rather than discovering they are dead later.

### Connection recovery

- Every moving part (tap, socket, heartbeat) is a supervised task. A dead
  task is rebuilt in place. Nothing requires a process restart.
- The client retries connection forever with capped exponential backoff,
  so it recovers whether the server slept, rebooted, or changed network.
- Heartbeats run in both directions on a 1 second tick with a 3 second
  timeout, so a half-open socket (TCP believes it is alive, nothing
  flows) is detected and replaced.

### Stuck key prevention

If the link drops while a modifier is held, the receiving side would
otherwise believe that modifier is held forever. Both sides track the set
of currently-held keys. Any transition (disconnect, wake, mode switch,
explicit release) synthesizes key-up for every key in that set.

A configurable panic hotkey force-returns control to the Mac.

### Transport choice

TCP, not UDP. Key events must be ordered and reliable: a dropped key-up
means a stuck modifier. LAN latency for TCP is around 1ms, which is not
perceptible for cursor movement. TCP also makes disconnection explicit and
detectable, which serves the primary goal.

## Discovery

Typing IP addresses by hand is the setup papercut in comparable tools, and
addresses change. hop advertises itself over mDNS/DNS-SD (Bonjour) as
`_hop._tcp.local`, which is native on macOS and supported on Windows, and
works on both without any daemon or central server.

Each instance advertises its `id`, its role, its port, and its protocol
version. Discovery is a plain list of candidates:

```
$ hop discover
  #  NAME              ADDRESS              ROLE     VERSION
  1  talhas-mac        192.168.1.42:24810  server   1
  2  desktop-pc        192.168.1.43:24810   client   1

$ hop pair 2
Paired with desktop-pc. Wrote peer to ~/.config/hop/config.toml
```

`hop pair` writes the chosen peer into config, so the normal path never
involves typing an address. Manual address entry stays supported as a
fallback for networks that block multicast, which is common on corporate
and guest WiFi.

Two properties this must preserve:

- **Discovery is not authorization.** Anything on the network can
  advertise itself as a hop instance. Being discovered only makes a
  machine appear in a list; the shared key still gates every connection,
  and an unpaired peer cannot inject a single keystroke. Discovery
  changes convenience, never trust.
- **Addresses may change; identity does not.** Peers are matched by `id`
  from the handshake, not by address. If the PC gets a new DHCP lease,
  the running system re-resolves it through discovery and reconnects
  without the user editing anything. This is part of the recovery story,
  not just first-run setup.

Advertising is on by default on the LAN and can be disabled in config for
users who would rather not broadcast a machine name.

## Security

- A 32 byte key generated by `hop keygen`, stored in its own file with
  mode `600`, referenced from config by path. Keeping it out of the config
  file means the config can be shared or pasted into a bug report safely.
- Traffic is sealed with XChaCha20-Poly1305: encrypted and authenticated,
  so a party on the same network can neither read keystrokes nor inject
  forged input.
- Each message carries a sequence number validated against a sliding
  window, rejecting replayed packets.
- The handshake proves both sides possess the key before any input is
  processed, and carries version and capability negotiation.
- Binds to the LAN interface by default. This tool is not intended to be
  exposed to the internet, and the README will say so plainly.

Future consideration, not v1: storing the key in the macOS Keychain and
Windows Credential Manager instead of a file.

## Configuration

TOML. `~/.config/hop/config.toml` on macOS, `%APPDATA%\hop\config.toml`
on Windows.

The server listens; clients dial it and identify themselves by `id`
during the handshake. This direction matters: it is what lets the PC
recover on its own after the Mac sleeps, reboots, or changes address,
without the Mac having to discover the PC.

Server (the Mac):

```toml
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"                  # matched against the id the client presents

[peers.remap]
LeftGui = "LeftCtrl"
RightGui = "RightCtrl"
LeftAlt = "LeftAlt"

[layout]
top = "pc"                 # PC monitors sit above the Mac

[security]
key_file = "~/.config/hop/key"

[input]
panic_hotkey = "LeftCtrl+LeftAlt+Escape"
```

Client (the PC):

```toml
role = "client"
id = "pc"
server = "talhas-mac"      # resolved by discovery; may also be host:port

[discovery]
enabled = true             # advertise and resolve over mDNS

[security]
key_file = "%APPDATA%\\hop\\key"
```

`server` accepts either a discovered name or a literal `host:port`, so a
network that blocks multicast degrades to manual entry rather than
breaking.

Remapping is defined on the server, next to the peer it applies to, so
the translation happens once at the source and clients stay simple.

Peers are a list and layout is an edge-to-peer mapping, so supporting a
third machine later is a config change rather than a redesign, even though
only two are used today.

## Testing strategy

Automated, run in CI on every push:

- `hop-proto`: codec round-trip, crypto seal/open, tamper rejection,
  replay rejection, unknown-message tolerance
- `hop-core`: every state machine transition including edge cases,
  remap table application, held-key release on each transition type,
  reconnect backoff behavior, and peer re-resolution when a discovered
  address changes
- End-to-end: fake `Capturer` and `Injector` driving the full loop over a
  loopback socket, verifying that what is captured on one side is what is
  injected on the other

Manual, performed by the user on real hardware (the author does not drive
the user's input devices):

1. `hop discover` lists the other machine, and `hop pair` connects it
   without any address being typed
2. Cursor crosses the top edge onto the PC and returns via the bottom
3. Typing lands on the PC, and Cmd+C on the Mac keyboard copies on the PC
4. Lock the Mac for ten minutes, unlock, confirm sharing resumes with no
   manual restart
5. Sleep the Mac fully, wake it, confirm the same
6. Reboot the PC, confirm the client reconnects on its own
7. Panic hotkey returns control to the Mac

## CI and distribution

GitHub Actions builds and tests on macOS and Windows runners. The Windows
runner produces the `.exe` as a downloadable artifact, so no Rust
toolchain is ever needed on the Windows machine.

## Open source readiness

The repo ships `README.md`, `ARCHITECTURE.md`, `CONTRIBUTING.md`, and
`CLAUDE.md`. The crate split exists so a contributor can work inside one
box without understanding the others, and so the most likely outside
contribution (Linux support) is additive rather than invasive.

## Explicit non-goals

- Video. This is not a remote desktop; it shares input only.
- Internet operation. LAN only, by design.
- Feature parity with Deskflow or Synergy.
