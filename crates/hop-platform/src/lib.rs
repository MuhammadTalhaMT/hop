//! Operating system implementations of hop's input traits.
//!
//! This is the only crate in the workspace permitted to use `unsafe`, and
//! it exists so that everything else can stay testable without hardware.
//! Adding a platform means adding one module here and touching nothing
//! else.

pub use hop_core::{Capturer, DeviceError, Injector, InputEvent};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;
