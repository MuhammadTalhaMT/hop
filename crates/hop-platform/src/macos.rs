//! macOS implementation of hop's capture and injection traits.

pub mod capture;
pub mod cursor;
pub mod keymap;

pub use capture::{CaptureError, Edge, MacCapturer};
