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
