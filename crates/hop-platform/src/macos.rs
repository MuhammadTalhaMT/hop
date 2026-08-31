//! macOS implementation of hop's capture and injection traits.

pub mod capture;
pub mod keymap;

pub use capture::{CaptureError, MacCapturer};
