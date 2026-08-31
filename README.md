# hop

Keyboard and mouse sharing between a Mac and a Windows PC, with the Mac
driving the PC. It is meant to keep working after the Mac locks or
sleeps, rather than requiring a restart.

## Status

Untested on real hardware. No human has yet confirmed that the cursor
crosses to the PC, that keys arrive correctly on the far side, or that
the link recovers from a real sleep. Everything described below is
verified by the automated test suite, a manual spike, and CI only; see
`ARCHITECTURE.md` for what each crate actually does.

What exists:

- A wire protocol with encryption, replay protection, and a handshake
  that runs on every connection (`hop-proto`, `hop-core`).
- A control state machine, key remapping, held-key tracking, a split
  transport, liveness, reconnect backoff, and a supervisor that ties
  them together (`hop-core`).
- macOS input capture through a `CGEventTap`, edge detection, cursor
  parking, and Windows input injection through `SendInput`
  (`hop-platform`, the only crate permitted `unsafe`).
- A command line: `hop keygen` generates a pre-shared key, and `hop run
  --config <path>` runs the machine's configured role.

What is missing:

- Peer discovery. Addresses are configured by hand in the config file.
- PC to Mac input. Only macOS capture and Windows injection exist, so
  the Mac's keyboard and mouse can drive the PC, but not the other way
  around.
- Clipboard sharing.
- File transfer.

Known rough edges a first user will hit:

- Edge detection reads only the main display's bounds
  (`CGDisplay::main()`), so it is likely to misbehave with more than one
  monitor connected to the Mac.
- Trackpad and Magic Mouse scrolling is likely to be over-sensitive:
  continuous scroll events report pixel-scale point deltas, and the
  Windows side multiplies whatever delta it receives by `WHEEL_DELTA`
  without any separate scaling for that case.
- `~` and `%APPDATA%` are not expanded in the config file's `key_file`
  path. Key paths must be written out in full.

## What it does

- Share one keyboard and mouse between a Mac and a Windows PC
- Command acts as Control on the PC, so Cmd+C copies there
- Clipboard sync: copy text on one machine, paste on the other
- A menu bar item on the Mac for starting and stopping it
- Recovers by itself from lock, sleep and network loss

Not yet: peer discovery (addresses are configured by hand), driving the
Mac from the PC's keyboard, and clipboard images or files.

## Setup

Both machines need the SAME key file. Generate it once on the Mac with
`hop keygen` and copy it across.

Server, on the Mac, at `~/.config/hop/config.toml`:

```toml
role = "server"
bind = "0.0.0.0:24810"

[[peers]]
id = "pc"

[peers.remap]
LeftGui = "LeftCtrl"        # Command acts as Control on the PC
RightGui = "RightCtrl"

[layout]
top = "pc"                  # the edge the cursor crosses to reach the PC

[security]
key_file = "~/.config/hop/key"

[input]
panic_hotkey = "LeftCtrl+LeftAlt+Escape"
```

Client, on the PC, at `%APPDATA%\hop\config.toml`:

```toml
role = "client"
id = "pc"
server = "192.168.18.90:24810"     # the Mac's address and port

[security]
key_file = "%APPDATA%\\hop\\key"

[input]
return_edge = "bottom"             # mirrors the server's top edge
```

Two settings deserve attention:

`panic_hotkey` is required on the server and is the emergency escape. If
input ever gets stuck on the peer, pressing it returns control to the Mac
immediately. It is handled inside the event tap itself, so it works even
when the network connection is wedged.

`return_edge` on the client must be the mirror of the server's `[layout]`
edge: `top` pairs with `bottom`, `left` with `right`. The two files live
on different machines so the pairing cannot be validated at load time.
Get it backwards and the cursor crosses to the PC with no automatic way
back, leaving only the panic hotkey.

On macOS, whatever runs `hop` needs Accessibility permission (System
Settings, Privacy and Security, Accessibility). Without it the event tap
cannot be created at all and `hop run` will say so.

## Running it

    hop run --config ~/.config/hop/config.toml

Or, on the Mac, put it in the menu bar instead:

    hop menubar --config ~/.config/hop/config.toml

The menu bar item shows `hop ●` while running and `hop ○` while stopped,
with Start, Stop and Quit. It launches `hop run` as a separate process,
so a problem in hop cannot take the menu bar item down with it, and Quit
always stops the engine rather than leaving it orphaned holding your
input.

## Clipboard

Copying text on either machine makes it available to paste on the other.
Text only, up to 56 KB; a larger copy is skipped rather than truncated,
so you never get text that looks complete but is not. Images and files do
not cross.

Worth knowing: everything you copy is sent to the other machine,
including passwords copied from a password manager. It travels encrypted
and only your paired machine can read it, and clipboard contents are
never written to logs, but it does leave the machine you copied on.

## Security

Input is encrypted and authenticated with XChaCha20-Poly1305 under a
pre-shared key. A handshake runs on every connection: both peers
contribute a random nonce, and the resulting session id authenticates
every frame along with its direction, so a frame recorded on one
connection cannot be replayed into a later one, and a frame sent by one
peer cannot be reflected back at it.

There is no forward secrecy. The session id is used only as
authenticated data, not as an encryption key; encryption always uses the
same static pre-shared key. Anyone who obtains that key can decrypt
every session ever recorded on the wire, past or future.

Frame lengths and timing are not padded or masked, so an observer on the
network can learn typing rhythm and can distinguish some keystroke
classes by frame size, even though it cannot read the keys themselves.

hop is designed for a local network and should not be exposed to the
internet.

## License

MIT
