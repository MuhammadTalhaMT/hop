# hop

Keyboard and mouse sharing between macOS and Windows. Recovery from lock,
sleep, and network loss is the point of the project, not a nice to have.

## Working here

- Test first. Every behavior change starts with a failing test.
- `cargo test --workspace` must pass before any commit.
- `cargo clippy --workspace --all-targets -- -D warnings` must be silent.
- `cargo fmt --all -- --check` must be silent.
- No `unsafe` in `hop-proto` or `hop-core`; both crates forbid it with
  `#![forbid(unsafe_code)]`.
- No panics on any input or network path. Return `Result`. `unwrap` is for
  tests only.

## Design invariants

Do not break these without changing the spec first:

- Focus always returns to the local machine on disconnect, wake, or panic
  hotkey, and held keys are always released on any transition that leaves
  it. A stuck modifier on the far side is the worst outcome this tool can
  produce.
- Our own outbound heartbeats never count as evidence the peer is alive.
  Liveness tracks inbound and outbound activity separately so a half-open
  socket cannot look healthy just because we keep sending on it.
- Unknown message variants are ignored, never fatal, so a newer peer
  cannot break an older one. `Message::Unknown` carries no data and both
  `message_to_event` and `pump_client` treat it as a no-op, not an error.
- The replay window is only ever consulted with a sequence number that
  has already been authenticated by `open`. Checking it earlier would let
  a single forged frame with `seq = u64::MAX` permanently deny service
  without the attacker ever knowing the key.
- The frame size cap is enforced on the raw declared length, before any
  buffer is allocated for it. A peer's claimed frame size is never
  trusted enough to size an allocation.
- Peers are matched by id, not address (spec section on discovery), so a
  changed DHCP lease reconnects without user action. Not yet load bearing
  in code, since discovery and pairing are not built yet, but do not
  design the eventual matching logic around addresses.

## Documents

- Spec: `docs/superpowers/specs/2026-08-31-hop-design.md`
- Plans: `docs/superpowers/plans/`
- Architecture and crate layout: `ARCHITECTURE.md`
