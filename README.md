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
tested (81 tests passing across the workspace): wire encoding, encryption
and replay protection, key remapping, held-key tracking, the focus state
machine, framed transport, and liveness and backoff all exist and are
exercised by tests, including an end to end path that drives capture
through the wire to injection with fakes standing in for real hardware.

The platform layer and discovery do not exist yet: there is no macOS
capture, no Windows injection, no supervisor process, and no way for two
machines to find and pair with each other. As a result hop does not yet
share input between two real machines. See `ARCHITECTURE.md` for what is
built and what is not.

## Security

Input is encrypted and authenticated with XChaCha20-Poly1305 under a
pre-shared key, with replay protection. hop is designed for a local
network and should not be exposed to the internet.

That replay protection currently covers a single connection only. Frames
are bound to a session, but until the handshake exists both peers use
`SessionId::ZERO`, so traffic recorded from one connection would still be
accepted by a later one. Implementing the handshake is what makes this
real, and it is the first item in `ARCHITECTURE.md`'s handoff list.

Frame lengths and timing are not padded or masked, so an observer on the
network can learn typing rhythm and can distinguish some keystroke
classes by frame size, even though it cannot read the keys themselves.

## License

MIT
