//! Operating system implementations of hop's input traits.
//!
//! This is the only crate in the workspace permitted to use `unsafe`, and
//! it exists so that everything else can stay testable without hardware.
//! Adding a platform means adding one module here and touching nothing
//! else.

pub use hop_core::{Capturer, DeviceError, Injector, InputEvent};

#[cfg(target_os = "macos")]
pub mod macos;

// Not gated on `cfg(target_os = "windows")`: `windows::keymap` is pure
// lookup-table data with no `windows-sys` calls, so it is built and tested
// on every host. Anything under this module that actually touches the
// Windows API (e.g. `SendInput`) must gate itself internally rather than
// gating the whole module, so the table stays testable off Windows.
pub mod windows;
