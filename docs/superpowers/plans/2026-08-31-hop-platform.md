# hop Platform Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make hop actually share one keyboard and mouse from the macOS machine to the Windows PC, recovering by itself from lock, sleep and network loss, configured by hand-entered address.

**Architecture:** Plan A built the platform-independent core. This plan adds the two operating system implementations of the `Capturer` and `Injector` traits, the connection supervisor that keeps them alive, the handshake that makes session binding real, and the config and CLI to drive it. A new `hop-platform` crate holds all OS-specific code and is the only place `unsafe` is permitted.

**Tech Stack:** Rust 2021, plus core-graphics 0.25 and core-foundation 0.10 (macOS capture), windows-sys 0.61 (Windows injection), sha2 0.11 (session derivation), toml 1.1 and clap 4.6 (config and CLI), on top of the existing tokio, chacha20poly1305, postcard and tracing.

**Spec:** `docs/superpowers/specs/2026-08-31-hop-design.md`

**Prior plan:** `docs/superpowers/plans/2026-08-31-hop-core.md` (complete, merged)

## What the spike established

A throwaway probe was run on the target machine (Apple Silicon, macOS 27) before this plan was written. Confirmed there, not assumed:

- A `CGEventTap` can be created from Rust and sees keyboard, mouse, scroll and modifier events system wide, once Accessibility permission is granted to the running application.
- Returning `CallbackResult::Drop` genuinely suppresses an event so the rest of the system never sees it. This is the mechanism that makes the cursor leave the Mac.
- macOS **does** disable the tap unilaterally. Stalling the callback produced `kCGEventTapDisabledByTimeout`, delivered as an event with no error return.
- Calling `CGEventTapEnable` from inside the callback **recovers** it: input continued to be captured afterwards.

That last pair is the whole reason this project exists, and it is the most likely cause of the original "it stops working until I restart it" symptom. A lock alone did **not** produce a disable event on macOS 27, so the design must not assume lock is the trigger.

## Global Constraints

- Rust edition 2021.
- `#![forbid(unsafe_code)]` stays on `hop-proto` and `hop-core`. The new `hop-platform` crate is the ONLY crate permitted `unsafe`, and every `unsafe` block there carries a comment stating why it is sound.
- No panics on any input or network path. Return `Result`. `unwrap()`/`expect()` in tests only. In the event tap callback, a panic would unwind across an FFI boundary, which is undefined behavior: the callback must never panic.
- Unknown message variants stay ignored rather than fatal.
- No em dashes in comments, doc comments, or documentation.
- Crate versions pinned to the exact minors listed in Tech Stack.
- `cargo test --workspace`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings` must pass before every commit.
- Every task ends with a commit.

## Division of verification

Tasks 1 to 6 and 10 are pure logic and are fully covered by automated tests. Tasks 7 to 9 talk to an operating system and cannot be meaningfully unit tested; they are verified by the human operator against real hardware using the checklist in Task 13. Do not fake hardware verification, and do not claim a platform task works because it compiled.

---

### Task 1: Platform crate scaffold

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/hop-platform/Cargo.toml`
- Create: `crates/hop-platform/src/lib.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `hop-core`'s `Capturer`, `Injector`, `InputEvent`, `DeviceError`
- Produces: a `hop-platform` crate that compiles on both macOS and Windows, re-exporting the traits it will implement, with platform modules behind `#[cfg(target_os = ...)]`

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/hop-platform"` to `members`, and add to `[workspace.dependencies]`:

```toml
core-graphics = "0.25.0"
core-foundation = "0.10.1"
windows-sys = "0.61.2"
sha2 = "0.11.0"
toml = "1.1.4"
clap = { version = "4.6.6", features = ["derive"] }
```

- [ ] **Step 2: Create the crate manifest**

`crates/hop-platform/Cargo.toml`:

```toml
[package]
name = "hop-platform"
version = "0.1.0"
edition.workspace = true
license.workspace = true

[dependencies]
hop-core = { path = "../hop-core" }
hop-proto = { path = "../hop-proto" }
thiserror.workspace = true
tracing.workspace = true

[target.'cfg(target_os = "macos")'.dependencies]
core-graphics.workspace = true
core-foundation.workspace = true

[target.'cfg(target_os = "windows")'.dependencies]
windows-sys = { workspace = true, features = [
    "Win32_Foundation",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_WindowsAndMessaging",
] }
```

- [ ] **Step 3: Create the crate root**

`crates/hop-platform/src/lib.rs`:

```rust
//! Operating system implementations of hop's input traits.
//!
//! This is the only crate in the workspace permitted to use `unsafe`, and
//! it exists so that everything else can stay testable without hardware.
//! Adding a platform means adding one module here and touching nothing
//! else.

pub use hop_core::{Capturer, DeviceError, InputEvent, Injector};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;
```

Create empty `crates/hop-platform/src/macos.rs` and `crates/hop-platform/src/windows.rs` containing only a doc comment, so both targets compile.

- [ ] **Step 4: Verify it builds**

Run: `cargo test --workspace`
Expected: compiles, all existing tests still pass, `hop-platform` reports 0 tests.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/hop-platform
git commit -m "feat: add hop-platform crate for os specific code"
```

---

### Task 2: macOS keycode translation

**Files:**
- Create: `crates/hop-platform/src/macos/keymap.rs`
- Modify: `crates/hop-platform/src/macos.rs`

**Interfaces:**
- Consumes: `hop_proto::Usage`
- Produces: `pub fn virtual_key_to_usage(code: i64) -> Option<Usage>` and `pub fn usage_to_virtual_key(usage: Usage) -> Option<i64>`

macOS reports its own virtual key codes, which are not HID usage codes and are not even in the same order. This table is pure data and is a classic source of silent wrong-key bugs, so it is tested rather than trusted.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_correctly() {
        // macOS virtual keycode 0 is 'a', which is HID usage 0x04.
        assert_eq!(virtual_key_to_usage(0), Some(Usage::A));
        assert_eq!(virtual_key_to_usage(8), Some(Usage::C));
    }

    #[test]
    fn maps_modifiers_correctly() {
        // 55 is Command, which must become the GUI usage so that the
        // remap layer can turn it into Control for Windows.
        assert_eq!(virtual_key_to_usage(55), Some(Usage::LEFT_GUI));
        assert_eq!(virtual_key_to_usage(59), Some(Usage::LEFT_CTRL));
        assert_eq!(virtual_key_to_usage(58), Some(Usage::LEFT_ALT));
        assert_eq!(virtual_key_to_usage(56), Some(Usage::LEFT_SHIFT));
    }

    #[test]
    fn round_trips_every_known_key() {
        for code in 0..=0x7F {
            if let Some(usage) = virtual_key_to_usage(code) {
                assert_eq!(
                    usage_to_virtual_key(usage),
                    Some(code),
                    "keycode {code} did not round trip"
                );
            }
        }
    }

    #[test]
    fn unknown_keycodes_are_none_rather_than_wrong() {
        // Guessing at an unmapped key would type the wrong character on
        // the peer, which is worse than dropping it.
        assert_eq!(virtual_key_to_usage(9999), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hop-platform keymap`
Expected: FAIL, `cannot find function virtual_key_to_usage`.

- [ ] **Step 3: Write the implementation**

```rust
use hop_proto::Usage;

/// macOS virtual keycode to USB HID usage. Sourced from Carbon's
/// `Events.h` keycodes paired with the HID Keyboard usage page.
///
/// Entries are `(macos_virtual_keycode, hid_usage)`. Keep this sorted by
/// keycode and keep it injective: two macOS keys must never map to one
/// usage, or the reverse lookup would be ambiguous.
const TABLE: &[(i64, u16)] = &[
    (0, 0x04),  // a
    (1, 0x16),  // s
    (2, 0x07),  // d
    (3, 0x09),  // f
    (4, 0x0B),  // h
    (5, 0x0A),  // g
    (6, 0x1D),  // z
    (7, 0x1B),  // x
    (8, 0x06),  // c
    (9, 0x19),  // v
    (11, 0x05), // b
    (12, 0x14), // q
    (13, 0x1A), // w
    (14, 0x08), // e
    (15, 0x15), // r
    (16, 0x1C), // y
    (17, 0x17), // t
    (18, 0x1E), // 1
    (19, 0x1F), // 2
    (20, 0x20), // 3
    (21, 0x21), // 4
    (22, 0x23), // 6
    (23, 0x22), // 5
    (24, 0x2E), // =
    (25, 0x26), // 9
    (26, 0x24), // 7
    (27, 0x2D), // -
    (28, 0x25), // 8
    (29, 0x27), // 0
    (30, 0x30), // ]
    (31, 0x12), // o
    (32, 0x18), // u
    (33, 0x2F), // [
    (34, 0x0C), // i
    (35, 0x13), // p
    (36, 0x28), // return
    (37, 0x0F), // l
    (38, 0x0D), // j
    (39, 0x34), // '
    (40, 0x0E), // k
    (41, 0x33), // ;
    (42, 0x31), // backslash
    (43, 0x36), // ,
    (44, 0x38), // /
    (45, 0x11), // n
    (46, 0x10), // m
    (47, 0x37), // .
    (48, 0x2B), // tab
    (49, 0x2C), // space
    (50, 0x35), // `
    (51, 0x2A), // delete
    (53, 0x29), // escape
    (55, 0xE3), // command
    (56, 0xE1), // shift
    (57, 0x39), // caps lock
    (58, 0xE2), // option
    (59, 0xE0), // control
    (60, 0xE5), // right shift
    (61, 0xE6), // right option
    (62, 0xE4), // right control
    (96, 0x3E),  // f5
    (97, 0x3F),  // f6
    (98, 0x40),  // f7
    (99, 0x3C),  // f3
    (100, 0x41), // f8
    (101, 0x42), // f9
    (109, 0x43), // f10
    (103, 0x44), // f11
    (111, 0x45), // f12
    (118, 0x3D), // f4
    (120, 0x3B), // f2
    (122, 0x3A), // f1
    (123, 0x50), // left arrow
    (124, 0x4F), // right arrow
    (125, 0x51), // down arrow
    (126, 0x52), // up arrow
];

pub fn virtual_key_to_usage(code: i64) -> Option<Usage> {
    TABLE
        .iter()
        .find(|(vk, _)| *vk == code)
        .map(|(_, usage)| Usage(*usage))
}

pub fn usage_to_virtual_key(usage: Usage) -> Option<i64> {
    TABLE
        .iter()
        .find(|(_, u)| *u == usage.0)
        .map(|(vk, _)| *vk)
}
```

Add `pub mod keymap;` to `crates/hop-platform/src/macos.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p hop-platform keymap`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/hop-platform/src/macos
git commit -m "feat: add macos virtual keycode to hid usage table"
```

---

### Task 3: Windows scancode translation

**Files:**
- Create: `crates/hop-platform/src/windows/keymap.rs`
- Modify: `crates/hop-platform/src/windows.rs`

**Interfaces:**
- Consumes: `hop_proto::Usage`
- Produces: `pub fn usage_to_scancode(usage: Usage) -> Option<(u16, bool)>` returning the PS/2 set 1 scancode and whether it is an extended key

`SendInput` with `KEYEVENTF_SCANCODE` is used rather than virtual key codes, because scancodes are layout independent: the letter the peer types then depends on the peer's own keyboard layout, which is what a user expects.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_to_set_one_scancodes() {
        assert_eq!(usage_to_scancode(Usage::A), Some((0x1E, false)));
        assert_eq!(usage_to_scancode(Usage::C), Some((0x2E, false)));
    }

    #[test]
    fn maps_modifiers_including_extended_flag() {
        assert_eq!(usage_to_scancode(Usage::LEFT_CTRL), Some((0x1D, false)));
        // Right control shares the base code and is distinguished only by
        // the extended flag. Getting this wrong sticks the wrong modifier.
        assert_eq!(usage_to_scancode(Usage::RIGHT_CTRL), Some((0x1D, true)));
        assert_eq!(usage_to_scancode(Usage::LEFT_ALT), Some((0x38, false)));
        assert_eq!(usage_to_scancode(Usage::RIGHT_ALT), Some((0x38, true)));
    }

    #[test]
    fn unmapped_usages_are_none() {
        assert_eq!(usage_to_scancode(Usage(0xFFFF)), None);
    }

    #[test]
    fn every_entry_is_reachable_and_distinct() {
        // A duplicated (code, extended) pair would silently type the wrong
        // key for one of the two usages.
        let mut seen = std::collections::HashSet::new();
        for (_, code, extended) in TABLE {
            assert!(seen.insert((*code, *extended)), "duplicate scancode {code:#x}");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hop-platform keymap`
Expected: FAIL, `cannot find function usage_to_scancode`.

- [ ] **Step 3: Write the implementation**

```rust
use hop_proto::Usage;

/// USB HID usage to PS/2 set 1 scancode, with the extended-key flag.
///
/// Entries are `(hid_usage, scancode, extended)`. Extended keys share a
/// base scancode with a non-extended key and are distinguished by a 0xE0
/// prefix, which `SendInput` expresses with `KEYEVENTF_EXTENDEDKEY`.
pub(crate) const TABLE: &[(u16, u16, bool)] = &[
    (0x04, 0x1E, false), // a
    (0x05, 0x30, false), // b
    (0x06, 0x2E, false), // c
    (0x07, 0x20, false), // d
    (0x08, 0x12, false), // e
    (0x09, 0x21, false), // f
    (0x0A, 0x22, false), // g
    (0x0B, 0x23, false), // h
    (0x0C, 0x17, false), // i
    (0x0D, 0x24, false), // j
    (0x0E, 0x25, false), // k
    (0x0F, 0x26, false), // l
    (0x10, 0x32, false), // m
    (0x11, 0x31, false), // n
    (0x12, 0x18, false), // o
    (0x13, 0x19, false), // p
    (0x14, 0x10, false), // q
    (0x15, 0x13, false), // r
    (0x16, 0x1F, false), // s
    (0x17, 0x14, false), // t
    (0x18, 0x16, false), // u
    (0x19, 0x2F, false), // v
    (0x1A, 0x11, false), // w
    (0x1B, 0x2D, false), // x
    (0x1C, 0x15, false), // y
    (0x1D, 0x2C, false), // z
    (0x1E, 0x02, false), // 1
    (0x1F, 0x03, false), // 2
    (0x20, 0x04, false), // 3
    (0x21, 0x05, false), // 4
    (0x22, 0x06, false), // 5
    (0x23, 0x07, false), // 6
    (0x24, 0x08, false), // 7
    (0x25, 0x09, false), // 8
    (0x26, 0x0A, false), // 9
    (0x27, 0x0B, false), // 0
    (0x28, 0x1C, false), // return
    (0x29, 0x01, false), // escape
    (0x2A, 0x0E, false), // backspace
    (0x2B, 0x0F, false), // tab
    (0x2C, 0x39, false), // space
    (0x2D, 0x0C, false), // -
    (0x2E, 0x0D, false), // =
    (0x2F, 0x1A, false), // [
    (0x30, 0x1B, false), // ]
    (0x31, 0x2B, false), // backslash
    (0x33, 0x27, false), // ;
    (0x34, 0x28, false), // '
    (0x35, 0x29, false), // `
    (0x36, 0x33, false), // ,
    (0x37, 0x34, false), // .
    (0x38, 0x35, false), // /
    (0x39, 0x3A, false), // caps lock
    (0x3A, 0x3B, false), // f1
    (0x3B, 0x3C, false), // f2
    (0x3C, 0x3D, false), // f3
    (0x3D, 0x3E, false), // f4
    (0x3E, 0x3F, false), // f5
    (0x3F, 0x40, false), // f6
    (0x40, 0x41, false), // f7
    (0x41, 0x42, false), // f8
    (0x42, 0x43, false), // f9
    (0x43, 0x44, false), // f10
    (0x44, 0x57, false), // f11
    (0x45, 0x58, false), // f12
    (0x4F, 0x4D, true),  // right arrow
    (0x50, 0x4B, true),  // left arrow
    (0x51, 0x50, true),  // down arrow
    (0x52, 0x48, true),  // up arrow
    (0xE0, 0x1D, false), // left control
    (0xE1, 0x2A, false), // left shift
    (0xE2, 0x38, false), // left alt
    (0xE3, 0x5B, true),  // left gui (windows key)
    (0xE4, 0x1D, true),  // right control
    (0xE5, 0x36, false), // right shift
    (0xE6, 0x38, true),  // right alt
    (0xE7, 0x5C, true),  // right gui
];

pub fn usage_to_scancode(usage: Usage) -> Option<(u16, bool)> {
    TABLE
        .iter()
        .find(|(u, _, _)| *u == usage.0)
        .map(|(_, code, extended)| (*code, *extended))
}
```

Add `pub mod keymap;` to `crates/hop-platform/src/windows.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p hop-platform keymap`
Expected: PASS, 4 tests.

Note on `every_entry_is_reachable_and_distinct`: it asserts distinctness of
the `(scancode, extended)` PAIR, not of the scancode alone. Right control
and right alt deliberately share a base scancode with their left
counterparts and differ only by the extended flag, which is correct
hardware behavior. If this test fails, a genuine duplicate exists and the
table is wrong; do not weaken the assertion to make it pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hop-platform/src/windows
git commit -m "feat: add hid usage to windows scancode table"
```

---

### Task 4: Split the transport

**Files:**
- Modify: `crates/hop-core/src/transport.rs`
- Modify: `crates/hop-core/src/session.rs`
- Modify: `crates/hop-core/tests/end_to_end.rs`

**Interfaces:**
- Consumes: existing `Transport`
- Produces: `pub struct TransportReader<R>` with `recv`, `pub struct TransportWriter<W>` with `send`, and `pub fn split<S>(stream: S, key: SharedKey, session: SessionId) -> (TransportReader<ReadHalf<S>>, TransportWriter<WriteHalf<S>>)`

The whole-branch review of Plan A found that `Transport` cannot do bidirectional I/O: `send` and `recv` both take `&mut self`, so the spec's requirement for heartbeats in both directions is impossible and `Liveness` has no possible consumer. This task fixes that, and must be done before the supervisor.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn reader_and_writer_work_concurrently() {
        // The supervisor must be able to wait on incoming frames while a
        // heartbeat timer fires on the same connection. That is impossible
        // with a single &mut self type, which is why this split exists.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO);
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO);

        let reader = tokio::spawn(async move {
            let first = ar.recv().await.expect("recv");
            let second = ar.recv().await.expect("recv");
            (first, second)
        });

        bw.send(&Message::Heartbeat).await.expect("send");
        bw.send(&Message::Release).await.expect("send");

        let (first, second) = reader.await.expect("join");
        assert_eq!(first, Message::Heartbeat);
        assert_eq!(second, Message::Release);

        // And the other direction on the same pair still works.
        aw.send(&Message::Heartbeat).await.expect("send");
        assert_eq!(br.recv().await.expect("recv"), Message::Heartbeat);
    }

    #[tokio::test]
    async fn each_direction_has_its_own_sequence_space() {
        // Both sides start at seq 1. If they shared a replay window, the
        // second direction's first frame would look like a replay.
        let (a, b) = duplex(65536);
        let (mut ar, mut aw) = split(a, key(), SessionId::ZERO);
        let (mut br, mut bw) = split(b, key(), SessionId::ZERO);

        aw.send(&Message::Heartbeat).await.unwrap();
        bw.send(&Message::Heartbeat).await.unwrap();
        assert_eq!(br.recv().await.unwrap(), Message::Heartbeat);
        assert_eq!(ar.recv().await.unwrap(), Message::Heartbeat);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hop-core transport`
Expected: FAIL, `cannot find function split`.

- [ ] **Step 3: Write the implementation**

Restructure `transport.rs` so the sending state (`key`, `session`, `send_seq`) and the receiving state (`key`, `session`, `replay`) live in separate types, and `split` uses `tokio::io::split`:

```rust
use tokio::io::{split as io_split, ReadHalf, WriteHalf};

/// The receiving half. Owns the replay window.
pub struct TransportReader<R> {
    stream: R,
    key: SharedKey,
    session: SessionId,
    replay: ReplayWindow,
}

/// The sending half. Owns the outbound sequence counter.
pub struct TransportWriter<W> {
    stream: W,
    key: SharedKey,
    session: SessionId,
    send_seq: u64,
}

/// Split a stream into halves that can be used from different tasks.
///
/// Each direction keeps its own sequence counter and its own replay
/// window, so the two directions cannot be mistaken for replays of each
/// other.
pub fn split<S: AsyncRead + AsyncWrite>(
    stream: S,
    key: SharedKey,
    session: SessionId,
) -> (TransportReader<ReadHalf<S>>, TransportWriter<WriteHalf<S>>) {
    let (r, w) = io_split(stream);
    (
        TransportReader { stream: r, key: key.clone(), session, replay: ReplayWindow::new() },
        TransportWriter { stream: w, key, session, send_seq: 0 },
    )
}
```

Move the existing `send` body onto `TransportWriter` (bounded by `W: AsyncWrite + Unpin`) and the existing `recv` body, including `validated_len`, onto `TransportReader` (bounded by `R: AsyncRead + Unpin`). Keep every existing behavior: the frame cap checked before allocation, `Closed` versus `Truncated`, replay rejection after authentication, and the cancel-safety doc comment on `recv`.

Remove the original `Transport` type rather than keeping a wrapper, and update every call site. Keeping both would leave two ways to do the same thing, and the wrapper would still be unusable for the bidirectional case that motivated the split.

Change the pump signatures to take the half they actually need:

```rust
pub async fn pump_server<W, C>(
    transport: &mut TransportWriter<W>,
    capturer: &mut C,
    control: &mut Control,
    remap: &RemapTable,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
    C: Capturer,

pub async fn pump_client<R, I>(
    transport: &mut TransportReader<R>,
    injector: &mut I,
    held: &mut HeldKeys,
) -> Result<(), TransportError>
where
    R: AsyncRead + Unpin,
    I: Injector,

pub async fn send_release_all<W>(
    transport: &mut TransportWriter<W>,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
```

Update `crates/hop-core/tests/end_to_end.rs` accordingly: each test now calls `split` twice, once per side, and passes the appropriate half. Every existing transport and end-to-end test must still pass unchanged in meaning, including the cross-session refusal, the replay refusal, the truncation and close distinction, and the frame cap.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --workspace`
Expected: all existing tests plus the 2 new ones pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hop-core
git commit -m "feat: split transport into reader and writer halves"
```

---

### Task 5: Handshake and session derivation

**Files:**
- Create: `crates/hop-core/src/handshake.rs`
- Modify: `crates/hop-core/src/lib.rs`
- Modify: `crates/hop-proto/src/crypto.rs`
- Modify: `crates/hop-proto/Cargo.toml`

**Interfaces:**
- Consumes: `Message::Handshake`, `SessionId`, `TransportReader`, `TransportWriter`
- Produces: `pub fn derive_session(client_nonce: &[u8; 32], server_nonce: &[u8; 32]) -> SessionId`, `pub async fn client_handshake(...) -> Result<SessionId, HandshakeError>`, `pub async fn server_handshake(...) -> Result<(SessionId, String), HandshakeError>`

Plan A added session binding to the crypto but left both peers using `SessionId::ZERO`, so the protection is currently inert. This task makes it real.

- [ ] **Step 1: Add the hash dependency**

Add `sha2 = "0.11.0"` to `[workspace.dependencies]` and to `hop-proto`'s dependencies.

- [ ] **Step 2: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_depends_on_both_nonces() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let base = derive_session(&a, &b);
        assert_ne!(base, derive_session(&[9u8; 32], &b), "client nonce ignored");
        assert_ne!(base, derive_session(&a, &[9u8; 32]), "server nonce ignored");
    }

    #[test]
    fn session_is_order_sensitive() {
        // Roles are asymmetric, so client-then-server must not equal
        // server-then-client. Otherwise a reflected handshake would
        // produce the same session.
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_ne!(derive_session(&a, &b), derive_session(&b, &a));
    }

    #[test]
    fn session_is_deterministic() {
        let a = [7u8; 32];
        let b = [8u8; 32];
        assert_eq!(derive_session(&a, &b), derive_session(&a, &b));
    }

    #[test]
    fn session_is_never_zero_for_real_nonces() {
        // ZERO is the documented "no handshake yet" placeholder. A real
        // handshake must never coincidentally produce it.
        assert_ne!(derive_session(&[0u8; 32], &[0u8; 32]), SessionId::ZERO);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p hop-core handshake`
Expected: FAIL, `cannot find function derive_session`.

- [ ] **Step 4: Write the derivation**

```rust
use hop_proto::SessionId;
use sha2::{Digest, Sha256};

/// Derive the session identifier both peers will authenticate every frame
/// against.
///
/// Both nonces contribute, so neither peer can choose the session alone,
/// and a recording made under one session cannot be replayed into
/// another. The order is fixed by role rather than sorted, so a reflected
/// handshake produces a different session than the genuine one.
pub fn derive_session(client_nonce: &[u8; 32], server_nonce: &[u8; 32]) -> SessionId {
    let mut hasher = Sha256::new();
    hasher.update(b"hop session v1");
    hasher.update(client_nonce);
    hasher.update(server_nonce);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    SessionId(out)
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p hop-core handshake`
Expected: PASS, 4 tests.

- [ ] **Step 6: Write the failing test for the exchange**

```rust
    #[tokio::test]
    async fn client_and_server_agree_on_a_session() {
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);
        let (mut sr, mut sw) = split(a, key.clone(), SessionId::ZERO);
        let (mut cr, mut cw) = split(b, key, SessionId::ZERO);

        let server = tokio::spawn(async move { server_handshake(&mut sr, &mut sw).await });
        let client = client_handshake(&mut cr, &mut cw, "pc").await.expect("client");
        let (server_session, peer_id) = server.await.unwrap().expect("server");

        assert_eq!(client, server_session, "peers must derive the same session");
        assert_eq!(peer_id, "pc");
        assert_ne!(client, SessionId::ZERO);
    }

    #[tokio::test]
    async fn a_version_mismatch_is_refused() {
        // A peer speaking a different protocol version must be rejected
        // before any input is processed, not silently tolerated.
        let (a, b) = duplex(65536);
        let key = SharedKey::from_bytes([3u8; 32]);
        let (mut sr, _sw) = split(a, key.clone(), SessionId::ZERO);
        let (_cr, mut cw) = split(b, key, SessionId::ZERO);

        cw.send(&Message::Handshake {
            version: PROTOCOL_VERSION + 1,
            capabilities: 0,
            peer_id: "pc".into(),
            nonce: [1u8; 32],
        })
        .await
        .unwrap();

        assert!(matches!(
            server_handshake_read_only(&mut sr).await,
            Err(HandshakeError::VersionMismatch { .. })
        ));
    }
```

- [ ] **Step 7: Implement the exchange**

The client sends its `Handshake` first with a fresh random nonce, the server replies with its own, and both derive the session. Define:

```rust
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("transport: {0}")]
    Transport(#[from] crate::TransportError),
    #[error("peer speaks protocol version {theirs}, we speak {ours}")]
    VersionMismatch { ours: u16, theirs: u16 },
    #[error("peer sent {0:?} instead of a handshake")]
    Unexpected(Message),
    #[error("system RNG unavailable")]
    Random,
}
```

Both functions must reject any message that is not a `Handshake` as their first frame, and must check `version` against `PROTOCOL_VERSION` before deriving anything. Split out whatever small helper the version-mismatch test needs so that path is directly testable.

- [ ] **Step 8: Run the tests**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/hop-core crates/hop-proto Cargo.toml Cargo.lock
git commit -m "feat: add handshake and per session key derivation"
```

---

### Task 6: Connection supervisor

**Files:**
- Create: `crates/hop-core/src/supervisor.rs`
- Modify: `crates/hop-core/src/lib.rs`

**Interfaces:**
- Consumes: `split`, handshake functions, `Liveness`, `Backoff`, `Control`, `HeldKeys`
- Produces: `pub struct ClientSupervisor` with `pub async fn run<I: Injector>(&mut self, injector: &mut I) -> !`, and the reconnect policy as a separately testable unit

This is where the project's promise is actually delivered: reconnect forever, detect a half-open socket, and never require a restart.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_dead_connection_releases_held_keys_locally() {
        // The server cannot deliver ReleaseAllKeys to a peer it can no
        // longer reach, so the client must release its own keys when it
        // decides the link is dead. Without this, a modifier stays down
        // on this machine forever.
        let mut held = HeldKeys::new();
        held.record(Usage::LEFT_CTRL, true);
        let mut injector = FakeInjector::new();

        release_everything(&mut injector, &mut held);

        assert_eq!(
            injector.injected(),
            vec![InputEvent::Key { usage: Usage::LEFT_CTRL, pressed: false }]
        );
        assert!(held.is_empty());
    }

    #[test]
    fn backoff_resets_after_a_successful_connection() {
        // A brief blip must not inherit a long delay from an earlier
        // outage, or a quick recovery would be needlessly slow.
        let mut policy = ReconnectPolicy::new();
        policy.failed();
        policy.failed();
        assert!(policy.delay() > Duration::from_millis(100));
        policy.connected();
        assert_eq!(policy.delay(), Duration::from_millis(100));
    }

    #[test]
    fn the_policy_never_gives_up() {
        let mut policy = ReconnectPolicy::new();
        for _ in 0..1000 {
            policy.failed();
            assert!(policy.delay() <= Duration::from_secs(5));
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hop-core supervisor`
Expected: FAIL, `cannot find function release_everything`.

- [ ] **Step 3: Write the implementation**

Provide `release_everything` and `ReconnectPolicy` as small pure units (tested above), then build `ClientSupervisor::run` on top of them. The loop must:

1. Connect with `TcpStream::connect`, on failure log, wait `policy.delay()`, `policy.failed()`, and retry forever.
2. Perform the client handshake and derive the session. On mismatch, log and treat as a failed connection.
3. `split` the stream, then run two tasks: a reader driving `pump_client`, and a heartbeat ticker sending `Message::Heartbeat` on the interval.
4. Feed every received frame to `Liveness::record_activity`, and check `is_dead` on each heartbeat tick.
5. On death, disconnect, or any transport error: call `release_everything`, `policy.failed()`, and loop.
6. On a successful handshake: `policy.connected()`.

Every transition logs via `tracing`. The function never returns.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hop-core
git commit -m "feat: add client supervisor with reconnect and self healing"
```

---

### Task 7: macOS capture

**Files:**
- Create: `crates/hop-platform/src/macos/capture.rs`
- Modify: `crates/hop-platform/src/macos.rs`

**Interfaces:**
- Consumes: `Capturer`, `InputEvent`, the keymap from Task 2
- Produces: `pub struct MacCapturer` implementing `Capturer`, plus `MacCapturer::start() -> Result<Self, CaptureError>`

This is the task the spike was written for. The proven approach: a background thread owns a `CFRunLoop` and the tap, the callback pushes events into a channel, and `poll` drains that channel without blocking.

- [ ] **Step 1: Write what can be tested without hardware**

Only the event translation is testable here. Write a test for a pure `fn translate(event_type, keycode, dx, dy) -> Option<InputEvent>` helper covering a key down, a key up, a mouse move, a scroll, and an unmapped keycode returning `None`. Run it and confirm it fails, then passes.

- [ ] **Step 2: Implement the tap**

Adapt this, which is the code proven working on the target machine during the spike:

```rust
use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};

unsafe extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}
```

The tap is created with `CGEventTapLocation::HID`, `CGEventTapPlacement::HeadInsertEventTap`, and `CGEventTapOptions::Default` (NOT `ListenOnly`, which cannot suppress). Events of interest must include `KeyDown`, `KeyUp`, `FlagsChanged`, `MouseMoved`, `LeftMouseDragged`, `RightMouseDragged`, `LeftMouseDown`, `LeftMouseUp`, `RightMouseDown`, `RightMouseUp`, `OtherMouseDown`, `OtherMouseUp`, and `ScrollWheel`.

The dragged variants are not optional: macOS emits `MouseMoved` only while no
button is held, and switches to `LeftMouseDragged` or `RightMouseDragged` once
one is. Omitting them means the peer sees a button press, a frozen cursor, and
a release, so drag-select, drag-and-drop and window dragging all fail.

**Do NOT put `TapDisabledByTimeout` or `TapDisabledByUserInput` in
`events_of_interest`.** Their discriminants are `0xFFFFFFFE` and `0xFFFFFFFF`,
and the mask is built as `1 << (etype as u64)`, so including them shifts by
about four billion. With overflow checks on, which is the default dev profile,
that panics inside `CGEventTap::new` before the tap is ever created, killing
capture entirely in every debug build while working by accident in release.
macOS delivers both events to the callback regardless of the mask, which is
precisely why no mask bit is defined for them. Handle them in the callback,
never in the mask.

Three requirements the spike proved necessary:

- On `TapDisabledByTimeout` or `TapDisabledByUserInput`, call `CGEventTapEnable(port, true)` immediately and log at warn level. macOS delivers these as events with no error return, and a program that ignores them goes deaf while appearing healthy. This is the single most important behavior in the file.
- Store the mach port where the callback can reach it (a `OnceLock<usize>` holding the `CFMachPortRef` works and is what the spike used).
- The callback must never panic: a panic across the FFI boundary is undefined behavior. Wrap the body so any failure logs and returns `CallbackResult::Keep`.

Return `CallbackResult::Drop` when the shared "we are currently remote" flag is set, and `Keep` otherwise. That flag is an `Arc<AtomicBool>` the owner flips when focus changes.

- [ ] **Step 3: Add the watchdog**

Spawn a thread that, if no event has been seen for 5 seconds while the tap should be active, calls `CGEventTapEnable` again and logs. The spike showed a lock alone did not disable the tap on macOS 27, so this is the belt-and-braces for disable causes we have not observed.

- [ ] **Step 4: Verify it compiles and the unit test passes**

Run: `cargo test -p hop-platform`
Expected: PASS.

Hardware verification happens in Task 13. Do not claim this works because it compiled.

- [ ] **Step 5: Commit**

```bash
git add crates/hop-platform/src/macos
git commit -m "feat: add macos event tap capture with automatic re-arming"
```

---

### Task 8: macOS edge detection and cursor parking

**Files:**
- Create: `crates/hop-platform/src/macos/cursor.rs`
- Modify: `crates/hop-platform/src/macos/capture.rs`

**Interfaces:**
- Produces: `pub fn cursor_position() -> (f64, f64)`, `pub fn warp_cursor(x: f64, y: f64)`, `pub fn hide_cursor()`, `pub fn show_cursor()`, and emission of `InputEvent::EdgeCrossed` from the capturer

The user's PC monitors sit above the Mac, so the cursor leaves through the TOP edge. That mapping comes from config, not from this file.

- [ ] **Step 1: Write the pure test**

The decision of whether a position counts as an edge crossing is pure. Test `fn crossed(edge: Edge, x: f64, y: f64, screen: (f64, f64)) -> bool` for: top edge with y at 0 returns true, top edge with y at 5 returns false, and each of the other three edges at their own boundary.

- [ ] **Step 2: Implement**

While focus is Local, the capturer watches the cursor position. When it reaches the configured edge, emit `InputEvent::EdgeCrossed` and set the remote flag so subsequent events are suppressed.

While Remote, park the real cursor: warp it back to the same point on every mouse move so it cannot drift off screen or interact with the Mac, and hide it so the user sees only the PC's cursor moving. Restore it on return.

Relative deltas come from `EventField::MOUSE_EVENT_DELTA_X` and `MOUSE_EVENT_DELTA_Y`, which stay meaningful even while the cursor is parked. Do not compute deltas from absolute positions, because parking makes those constant.

- [ ] **Step 3: Verify and commit**

Run: `cargo test -p hop-platform`

```bash
git add crates/hop-platform/src/macos
git commit -m "feat: add edge detection and cursor parking on macos"
```

---

### Task 9: Windows injection

**Files:**
- Create: `crates/hop-platform/src/windows/inject.rs`
- Modify: `crates/hop-platform/src/windows.rs`

**Interfaces:**
- Consumes: `Injector`, `InputEvent`, the scancode table from Task 3
- Produces: `pub struct WindowsInjector` implementing `Injector`

- [ ] **Step 1: Write the pure test**

The construction of the `INPUT` structure is testable without calling into Windows. Extract `fn key_input(usage: Usage, pressed: bool) -> Option<INPUT>` and test that a key down sets no `KEYEVENTF_KEYUP`, a key up sets it, a right-hand modifier sets `KEYEVENTF_EXTENDEDKEY`, and an unmapped usage yields `None`. Gate the test with `#[cfg(target_os = "windows")]`; it runs on the Windows CI runner.

- [ ] **Step 2: Implement**

Use `SendInput` with `KEYEVENTF_SCANCODE` for keys (layout independent, so the PC's own layout decides the character), `MOUSEEVENTF_MOVE` with relative deltas for motion, `MOUSEEVENTF_WHEEL` for scroll, and the button pairs for clicks.

Every `unsafe` block carries a comment stating why it is sound. `SendInput` returns the number of events inserted: if it returns fewer than requested, return `DeviceError::Rejected` including `GetLastError`. That error is now logged rather than swallowed silently, which is what makes a failing injector visible instead of looking like a hang.

- [ ] **Step 3: Verify and commit**

Run: `cargo test -p hop-platform` (the Windows-gated test runs in CI)

```bash
git add crates/hop-platform/src/windows
git commit -m "feat: add windows sendinput injection"
```

---

### Task 10: Configuration

**Files:**
- Create: `crates/hop/Cargo.toml`
- Create: `crates/hop/src/config.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `pub struct Config` with `Config::load(path: &Path) -> Result<Config, ConfigError>`, covering both roles

- [ ] **Step 1: Write the failing test**

Test that a valid server config parses with its remap table and layout; that a valid client config parses; that a missing key file path is an error naming the field; that an unknown key name in the remap table is an error naming the key rather than being silently ignored; and that a config specifying neither role is an error.

The unknown-key case matters: silently dropping a remap line the user wrote would leave them with a key that mysteriously does not work.

- [ ] **Step 2: Implement**

Follow the schema in the spec's Configuration section exactly, including `role`, `bind`, `[[peers]]` with `id` and `[peers.remap]`, `[layout]`, `[security] key_file`, and `[input] panic_hotkey`. Parse remap key names into `Usage` via a name table, and reject unknown names.

- [ ] **Step 3: Verify and commit**

Run: `cargo test -p hop`

```bash
git add crates/hop Cargo.toml Cargo.lock
git commit -m "feat: add configuration loading and validation"
```

---

### Task 11: CLI and wiring

**Files:**
- Create: `crates/hop/src/main.rs`
- Create: `crates/hop/src/run.rs`

**Interfaces:**
- Produces: the `hop` binary with `hop keygen`, `hop run`, and `hop --help`

- [ ] **Step 1: Implement `hop keygen`**

Generate a key with `SharedKey::generate()`, write it base64 encoded to the configured path with mode `0o600` on Unix, and refuse to overwrite an existing key file unless `--force` is passed. Print the path and a reminder to copy the same key to the other machine.

- [ ] **Step 2: Implement `hop run`**

Load the config, load the key, then dispatch on role. On macOS as server: start `MacCapturer`, listen on the bind address, accept a client, perform the server handshake, and drive `pump_server`. On Windows as client: construct `WindowsInjector` and hand it to `ClientSupervisor::run`, which never returns.

Initialise `tracing_subscriber` so the logs the supervisor and capturer emit are actually visible, with `RUST_LOG` respected and a sensible default of `info`.

- [ ] **Step 3: Verify**

Run: `cargo build --workspace && cargo run -p hop -- --help`
Expected: help text lists `keygen` and `run`.

- [ ] **Step 4: Commit**

```bash
git add crates/hop
git commit -m "feat: add hop cli with keygen and run"
```

---

### Task 12: Release artifacts

**Files:**
- Modify: `.github/workflows/ci.yml`
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Build the Windows binary in CI**

Add a job that builds `--release` on `windows-latest` and uploads `hop.exe` with `actions/upload-artifact`, so the Windows machine never needs a Rust toolchain. Do the same for macOS.

- [ ] **Step 2: Verify**

Push the branch and confirm the workflow goes green and the artifact is downloadable. This is the first time CI has actually run, since the repository had no remote during Plan A.

- [ ] **Step 3: Commit**

```bash
git add .github
git commit -m "ci: publish platform binaries as artifacts"
```

---

### Task 13: Hardware verification

**Files:**
- Modify: `README.md`
- Create: `docs/manual-test.md`

This is the task that decides whether the project works. It is performed by the human operator on real hardware, because no automated test can tell you whether your cursor actually crossed onto your PC.

- [ ] **Step 1: Write the checklist**

Create `docs/manual-test.md` containing the steps below, so it can be re-run after any future change.

- [ ] **Step 2: Hand the operator the setup steps**

1. On the Mac: `cargo run -p hop -- keygen`, then copy the key file to the PC.
2. Grant Accessibility permission to whatever runs `hop` (System Settings, Privacy and Security, Accessibility). Without it the tap cannot be created at all.
3. Write both config files, with the PC's LAN address on the client and `top = "pc"` on the server.
4. Download `hop.exe` from the CI artifact onto the PC and run it.
5. Start `hop run` on the Mac.

- [ ] **Step 3: The checks**

1. Cursor leaves the TOP edge of the Mac and appears on the PC.
2. Cursor returns through the bottom edge of the PC.
3. Typing lands on the PC, and the letters are correct (this is what validates both keymap tables).
4. Cmd+C on the Mac keyboard copies on the PC, proving the remap.
5. Scroll and both mouse buttons work on the PC.
6. Lock the Mac, wait ten minutes, unlock: sharing resumes with no restart.
7. Sleep the Mac fully, wake it: the same.
8. Reboot the PC: the client reconnects on its own.
9. Pull the network cable or disable WiFi briefly: it recovers on its own.
10. Hold Cmd, then kill the link: the PC does not end up with a stuck modifier.
11. The panic hotkey returns control to the Mac.

- [ ] **Step 4: Update the README status**

Replace the "does not yet share input between two real machines" wording with what is actually true once the checks pass, including which of them were verified and on what OS versions.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/manual-test.md
git commit -m "docs: add hardware verification checklist"
```

---

## Deliberately not in this plan

- **Peer discovery.** Addresses are typed by hand for now. `hop discover` and `hop pair` are Plan C.
- **Windows capture.** Only the Mac drives. Controlling the Mac from the PC's keyboard needs a Windows capture hook, which is a separate implementation from injection.
- **Clipboard and file transfer.** Both fit as new message types plus a capability flag, which is why the capability handshake exists.
- **Code signing and notarisation.** The Windows binary will be unsigned, and macOS will need Accessibility permission granted manually.
