# hop

Share one keyboard and mouse between a Mac and a Windows PC, and keep
working after the Mac locks or sleeps.

## Why

Comparable tools stop working after a lock or sleep and need to be
restarted by hand. hop treats recovery as the primary feature: every
moving part is supervised and rebuilt in place, and the client reconnects
by itself for as long as it takes.

## Status

Under construction. The protocol and control core are implemented and
tested (144 tests passing across the workspace): wire encoding, encryption
and replay protection, key remapping, held-key tracking, the focus state
machine, framed transport, and liveness and backoff all exist and are
exercised by tests, including an end to end path that drives capture
through the wire to injection with fakes standing in for real hardware.

The platform layer now exists too: macOS capture through a `CGEventTap`
that re-arms itself when macOS disables it, Windows injection through
`SendInput`, a handshake, and a client supervisor that reconnects on its
own. What is still missing is the command line wiring that starts them,
and peer discovery, so addresses must be configured by hand. Until the
wiring lands, hop does not yet share input between two real machines.
See `ARCHITECTURE.md` for what is built and what is not.

## Security

Input is encrypted and authenticated with XChaCha20-Poly1305 under a
pre-shared key, with replay protection. hop is designed for a local
network and should not be exposed to the internet.

Each connection derives a session identifier used only as authenticated
data, not an encryption key, so encryption always uses the same static
pre-shared key and there is no forward secrecy: anyone who obtains that
key can decrypt every session ever recorded on the wire, past or future.

Replay protection spans sessions, not just single connections. Each
connection performs a handshake in which both peers contribute a random
nonce, and every frame is authenticated against the session derived from
both. Traffic recorded from one connection therefore cannot be replayed
into a later one: it fails authentication rather than being accepted.
Frames are also bound to their direction, so a peer will not accept its
own traffic reflected back at it.

Frame lengths and timing are not padded or masked, so an observer on the
network can learn typing rhythm and can distinguish some keystroke
classes by frame size, even though it cannot read the keys themselves.

## License

MIT
